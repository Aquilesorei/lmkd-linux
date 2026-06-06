//! MaintenanceManager — slow housekeeping that must run off the 5s evictor loop.
//!
//! Some watchers block (CPU-idle sampling, and later swap reclaim's swapoff/
//! swapon). Running them inline in the evictor would stall pressure response, so
//! they live here on a longer, separate poll. Maintenance acts only when the
//! system is calm (Normal pressure) — under pressure the evictor owns all
//! process actions, and the two must never act on the same process concurrently.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::logger::{LogEntry, Logger};
use crate::monitor;
use crate::monitor::process::Process;
use crate::monitor::psi::PressureLevel;
use crate::output::locked_print;

/// Maintenance poll interval. Much longer than the evictor's 5s — these are
/// housekeeping actions, not pressure response.
const POLL_SECS: u64 = 60;

/// Unix-seconds of the last plasma-discover reap (0 = never). Cooldown floor.
static LAST_PD_REAP: AtomicU64 = AtomicU64::new(0);

pub fn run(log: Arc<Logger>) {
    loop {
        if crate::should_shutdown() {
            return;
        }

        // Only act when calm. Treat a PSI read error as "not calm" → skip.
        let calm = monitor::psi::read_pressure()
            .map(|p| monitor::psi::pressure_level(&p) == PressureLevel::Normal)
            .unwrap_or(false);

        let cycle_start = Instant::now();
        if calm {
            let procs = monitor::process::list_processes();
            check_plasma_discover(&procs, &log);
        }

        // Subtract work already spent this cycle (notably the blocking idle
        // sample) so the loop period stays ~POLL_SECS instead of doubling.
        let spent = cycle_start.elapsed().as_secs();
        interruptible_sleep(POLL_SECS.saturating_sub(spent));
    }
}

/// Sleep in 1s slices so shutdown is observed promptly even mid-interval.
fn interruptible_sleep(secs: u64) {
    for _ in 0..secs {
        if crate::should_shutdown() {
            return;
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reap an idle, oversized plasma-discover. No-op unless
/// `[plasma_discover] watch_memory = true`. The idle CPU sample blocks for
/// `pd_idle_check_secs`, which is why this runs on the maintenance thread.
fn check_plasma_discover(procs: &[Process], log: &Logger) {
    let (threshold_mb, idle_secs, cooldown_secs) = {
        let cfg = crate::config::get();
        if !cfg.watch_plasma_discover {
            return;
        }
        (cfg.pd_rss_threshold_mb, cfg.pd_idle_check_secs, cfg.pd_cooldown_secs)
    };

    // The kernel truncates comm to 15 chars; "plasma-discover" is exactly 15.
    let Some(pd) = procs.iter().find(|p| p.name == "plasma-discover") else { return };
    let rss_mb = pd.rss_kb / 1024;
    if rss_mb < threshold_mb {
        return;
    }

    // Cooldown gate before the blocking idle sample — don't sleep if we couldn't
    // act anyway.
    let now = now_secs();
    let last = LAST_PD_REAP.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < cooldown_secs {
        return;
    }

    // Fingerprint the PID before the long sample so we can detect recycling.
    let Some(start_time) = crate::executor::read_start_time(pd.pid) else { return };

    // Idle detection: a non-zero CPU-jiffy delta over the window ⇒ it's working.
    let Some(before) = monitor::process::cpu_jiffies(pd.pid) else { return };
    interruptible_sleep(idle_secs);
    if crate::should_shutdown() {
        return;
    }
    let Some(after) = monitor::process::cpu_jiffies(pd.pid) else { return };
    if after.saturating_sub(before) > 0 {
        return; // active — leave it alone
    }

    // Recycle guard: if start_time changed, the original plasma-discover exited
    // and the PID was reused — never terminate the impostor.
    if crate::executor::read_start_time(pd.pid) != Some(start_time) {
        return;
    }

    locked_print(&format!(
        "[plasma-discover] idle, RSS {rss_mb}MB ≥ {threshold_mb}MB — reaping (relaunches on demand)"
    ));
    let r = crate::executor::killer::terminate(pd.pid);
    if r.success {
        LAST_PD_REAP.store(now, Ordering::Relaxed);
        log.log(&LogEntry::new("REAP", pd.pid, "plasma-discover", rss_mb as f64, "idle_reaped"));
    } else {
        // Don't arm the cooldown on failure — matches the other watchers.
        log.log(&LogEntry::new(
            "REAP", pd.pid, "plasma-discover", rss_mb as f64,
            &format!("skipped: {}", r.error.unwrap_or_default()),
        ));
    }
}
