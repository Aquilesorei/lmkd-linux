
//! MaintenanceManager — slow/blocking housekeeping kept off the 5s evictor loop.
//!
//! The CPU-idle sample (plasma-discover) and swapoff/swapon (reclaim) block, so
//! they run here on a 60s poll. Acts only at Normal pressure; under pressure the
//! evictor owns all process actions and the two must not act on one concurrently.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mgd_common::logger::{LogEntry, Logger};
use crate::monitor;
use crate::monitor::psi::PressureLevel;
use mgd_common::output::locked_print;

const POLL_SECS: u64 = 60;

/// Reclaim-helper locations, probed in order. Absolute, root-writable only — no
/// PATH search, no attacker-controllable input (PRIVILEGE_DESIGN §3). Covers
/// manual/install.sh (/usr/local/bin) and distro packaging (/usr/bin).
const RECLAIM_HELPER_CANDIDATES: &[&str] = &[
    "/usr/local/bin/mgd-zram-reclaim",
    "/usr/bin/mgd-zram-reclaim",
];



/// Unix-seconds of the last proactive swap reclaim (0 = never).
static LAST_RECLAIM: AtomicU64 = AtomicU64::new(0);

/// Set once when the reclaim helper is absent/uncapped, to log only once.
static RECLAIM_DISABLED: AtomicBool = AtomicBool::new(false);

pub fn run(log: Arc<Logger>) {
    loop {
        if crate::should_shutdown() {
            return;
        }

        let pressure = monitor::psi::read_pressure().ok();
        // A PSI read error counts as not-calm.
        let calm = pressure
            .as_ref()
            .map(|p| monitor::psi::pressure_level(p) == PressureLevel::Normal)
            .unwrap_or(false);

        let cycle_start = Instant::now();
        if calm {
            if let Some(p) = pressure.as_ref() {
                check_proactive_reclaim(p, &log);
            }
        }

        // Subtract time already spent (the idle sample) to hold the period at ~POLL_SECS.
        let spent = cycle_start.elapsed().as_secs();
        interruptible_sleep(POLL_SECS.saturating_sub(spent));
    }
}

/// Sleep in 1s slices so shutdown is observed mid-interval.
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


/// Pure gate inputs for proactive reclaim (I/O-free for unit tests).
struct ReclaimGates {
    swap_used_pct: f64,
    zram_used_mb: u64,
    /// Decompressed footprint, MB — the figure the OOM guard uses.
    zram_orig_mb: u64,
    mem_available_mb: u64,
    threshold_pct: f64,
    min_zram_used_mb: u64,
    headroom_mult: f64,
}

/// Err(reason) if any reclaim gate fails. The headroom check is the OOM guard:
/// pages expand 2-3x decompressing, so compare against the decompressed footprint.
fn reclaim_gates_pass(g: &ReclaimGates) -> Result<(), String> {
    if g.swap_used_pct < g.threshold_pct {
        return Err(format!(
            "swap {:.0}% < threshold {:.0}%",
            g.swap_used_pct, g.threshold_pct
        ));
    }
    if g.zram_used_mb < g.min_zram_used_mb {
        return Err(format!(
            "zram used {}MB < min {}MB",
            g.zram_used_mb, g.min_zram_used_mb
        ));
    }
    let required_mb = (g.zram_orig_mb as f64 * g.headroom_mult) as u64;
    if g.mem_available_mb <= required_mb {
        return Err(format!(
            "insufficient headroom: avail {}MB <= decompressed {}MB × {:.1} = {}MB",
            g.mem_available_mb, g.zram_orig_mb, g.headroom_mult, required_mb
        ));
    }
    Ok(())
}

/// Proactive swap reclaim via the capped helper (PRIVILEGED, PRIVILEGE_DESIGN
/// §2). No-op unless `[reclaim] proactive_swap_reclaim = true`. All gates live
/// here; the helper is dumb. Requires `some_avg60 < 5%` so a just-subsided spike
/// doesn't trigger a reclaim.
fn check_proactive_reclaim(pressure: &monitor::psi::MemoryPressure, log: &Logger) {
    let (enabled, threshold_pct, cooldown_secs, min_used_mb, headroom_mult) = {
        let cfg = crate::config::get();
        if !cfg.proactive_swap_reclaim {
            return;
        }
        (
            cfg.proactive_swap_reclaim,
            cfg.reclaim_threshold_pct,
            cfg.reclaim_cooldown_secs,
            cfg.reclaim_min_zram_used_mb,
            cfg.reclaim_headroom_mult,
        )
    };
    if !enabled || RECLAIM_DISABLED.load(Ordering::Relaxed) {
        return;
    }

    if pressure.some_avg60 >= 5.0 {
        return;
    }

    let now = now_secs();
    let last = LAST_RECLAIM.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < cooldown_secs {
        return;
    }

    let Some(helper) = resolve_reclaim_helper() else {
        RECLAIM_DISABLED.store(true, Ordering::Relaxed);
        locked_print(
            "[reclaim] swap reclaim unavailable: mgd-zram-reclaim not found in \
             /usr/local/bin or /usr/bin — disabling for session. See docs/PRIVILEGE_DESIGN.md §2."
        );
        log.log(&LogEntry::new("RECLAIM", 0, "zram", 0.0, "unavailable: helper absent"));
        return;
    };

    let meminfo = monitor::meminfo::read_meminfo();
    let gates = ReclaimGates {
        swap_used_pct: meminfo.swap_used_pct(),
        zram_used_mb: monitor::zram::zram_used_mb_total(),
        zram_orig_mb: monitor::zram::zram_orig_mb_total(),
        mem_available_mb: meminfo.available_kb / 1024,
        threshold_pct,
        min_zram_used_mb: min_used_mb,
        headroom_mult,
    };

    if let Err(reason) = reclaim_gates_pass(&gates) {
        log.log(&LogEntry::new("RECLAIM", 0, "zram", 0.0, &format!("skipped: {reason}")));
        return;
    }

    locked_print(&format!(
        "[reclaim] calm, swap {:.0}% full, zram {}MB compressed ({}MB decompressed), \
         avail {}MB — reclaiming",
        gates.swap_used_pct, gates.zram_used_mb, gates.zram_orig_mb, gates.mem_available_mb
    ));

    match run_reclaim_helper(helper) {
        Ok(()) => {
            LAST_RECLAIM.store(now, Ordering::Relaxed);
            let after = monitor::meminfo::read_meminfo();
            log.log(&LogEntry::new(
                "RECLAIM", 0, "zram", gates.zram_orig_mb as f64,
                &format!(
                    "reclaimed: swap {:.0}%→{:.0}%, avail {}MB→{}MB",
                    gates.swap_used_pct, after.swap_used_pct(),
                    gates.mem_available_mb, after.available_kb / 1024
                ),
            ));
        }
        Err((code, e)) => {
            // Don't arm the cooldown on failure.
            locked_print(&format!("[reclaim] helper failed (exit {code:?}): {e}"));
            log.log(&LogEntry::new("RECLAIM", 0, "zram", 0.0, &format!("failed: {e}")));
            // Exit 2 = uncapped binary (persistent); other codes are transient.
            if code == Some(2) {
                RECLAIM_DISABLED.store(true, Ordering::Relaxed);
                locked_print(
                    "[reclaim] disabling for session: helper present but not capped \
                     (setcap cap_sys_admin+ep). See docs/PRIVILEGE_DESIGN.md §2."
                );
            }
        }
    }
}

/// First existing+executable candidate, or None. No PATH search, no env/config.
fn resolve_reclaim_helper() -> Option<&'static str> {
    RECLAIM_HELPER_CANDIDATES
        .iter()
        .copied()
        .find(|p| helper_available(p))
}

fn helper_available(path: &str) -> bool {
    let Ok(c) = std::ffi::CString::new(path) else { return false };
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

/// Exec the helper with a cleared environment and no args. Err carries the exit
/// code so the caller can distinguish uncapped (2) from transient failures.
fn run_reclaim_helper(path: &str) -> Result<(), (Option<i32>, String)> {
    let status = std::process::Command::new(path)
        .env_clear()
        .status()
        .map_err(|e| (None, format!("spawn failed: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err((status.code(), format!("{status}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_gates() -> ReclaimGates {
        ReclaimGates {
            swap_used_pct: 50.0,
            zram_used_mb: 3000,
            zram_orig_mb: 6000,
            mem_available_mb: 10_000,
            threshold_pct: 30.0,
            min_zram_used_mb: 2048,
            headroom_mult: 1.5,
        }
    }

    #[test]
    fn all_gates_pass() {
        assert!(reclaim_gates_pass(&base_gates()).is_ok()); // 10000 > 6000*1.5
    }

    #[test]
    fn fails_below_swap_threshold() {
        let mut g = base_gates();
        g.swap_used_pct = 20.0;
        assert!(reclaim_gates_pass(&g).is_err());
    }

    #[test]
    fn fails_below_min_zram_used() {
        let mut g = base_gates();
        g.zram_used_mb = 1000;
        assert!(reclaim_gates_pass(&g).is_err());
    }

    #[test]
    fn fails_insufficient_headroom() {
        let mut g = base_gates();
        g.mem_available_mb = 8000; // < 6000*1.5
        assert!(reclaim_gates_pass(&g).is_err());
    }

    #[test]
    fn headroom_boundary_is_strict() {
        let mut g = base_gates();
        g.mem_available_mb = 9000; // == 6000*1.5 must fail (<=)
        assert!(reclaim_gates_pass(&g).is_err());
        g.mem_available_mb = 9001;
        assert!(reclaim_gates_pass(&g).is_ok());
    }

    #[test]
    fn missing_helper_is_unavailable() {
        assert!(!helper_available("/nonexistent/mgd-zram-reclaim-xyz"));
    }

    #[test]
    fn resolver_probes_only_root_controlled_absolute_paths() {
        for p in RECLAIM_HELPER_CANDIDATES {
            assert!(p.starts_with('/'), "candidate not absolute: {p}");
            assert!(
                p.starts_with("/usr/local/bin/") || p.starts_with("/usr/bin/"),
                "candidate not in a root-controlled dir: {p}"
            );
        }
        assert!(resolve_reclaim_helper().is_none() || resolve_reclaim_helper().unwrap().starts_with('/'));
    }
}
