//! MaintenanceManager — slow housekeeping that must run off the 5s evictor loop.
//!
//! Some watchers block (CPU-idle sampling, and later swap reclaim's swapoff/
//! swapon). Running them inline in the evictor would stall pressure response, so
//! they live here on a longer, separate poll. Maintenance acts only when the
//! system is calm (Normal pressure) — under pressure the evictor owns all
//! process actions, and the two must never act on the same process concurrently.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// Standard, root-controlled install locations for the capped swap-reclaim
/// helper, probed in order. All are absolute (no PATH search) and writable only
/// by root, so resolving among them introduces no attacker-controllable input —
/// matches PRIVILEGE_DESIGN §3 ("absolute path, no PATH search"). Covers manual
/// installs / install.sh (/usr/local/bin) and distro packaging (/usr/bin).
const RECLAIM_HELPER_CANDIDATES: &[&str] = &[
    "/usr/local/bin/mgd-zram-reclaim", // manual install / install.sh
    "/usr/bin/mgd-zram-reclaim",       // distro packaging (RPM/DEB)
];

/// Unix-seconds of the last plasma-discover reap (0 = never). Cooldown floor.
static LAST_PD_REAP: AtomicU64 = AtomicU64::new(0);

/// Unix-seconds of the last proactive swap reclaim (0 = never). Cooldown floor.
static LAST_RECLAIM: AtomicU64 = AtomicU64::new(0);

/// Set once if the reclaim helper is absent/non-executable, so a missing opt-in
/// install logs exactly once instead of every maintenance cycle.
static RECLAIM_DISABLED: AtomicBool = AtomicBool::new(false);

pub fn run(log: Arc<Logger>) {
    loop {
        if crate::should_shutdown() {
            return;
        }

        // Read pressure once; treat a read error as "not calm" → skip everything.
        let pressure = monitor::psi::read_pressure().ok();
        let calm = pressure
            .as_ref()
            .map(|p| monitor::psi::pressure_level(p) == PressureLevel::Normal)
            .unwrap_or(false);

        let cycle_start = Instant::now();
        if calm {
            let procs = monitor::process::list_processes();
            check_plasma_discover(&procs, &log);
            // `pressure` is Some here (calm implies a successful read).
            if let Some(p) = pressure.as_ref() {
                check_proactive_reclaim(p, &log);
            }
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

/// Gate inputs for proactive swap reclaim, separated from I/O so the decision
/// is pure and unit-testable.
struct ReclaimGates {
    /// Current swap fill, percent (0-100).
    swap_used_pct: f64,
    /// Compressed RAM the zram pool holds, MB (min-used gate).
    zram_used_mb: u64,
    /// Decompressed footprint of the zram pool, MB (OOM-headroom gate).
    zram_orig_mb: u64,
    /// MemAvailable, MB.
    mem_available_mb: u64,
    /// Config: minimum swap fill to bother reclaiming.
    threshold_pct: f64,
    /// Config: minimum compressed pool size.
    min_zram_used_mb: u64,
    /// Config: required MemAvailable / decompressed-footprint ratio (OOM guard).
    headroom_mult: f64,
}

/// Pure reclaim decision: returns Err(reason) if any gate fails, Ok(()) if all
/// pass. Kept free of I/O so the (critical) OOM-headroom math is unit-testable.
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
    // OOM guard (critical): pages expand 2-3× decompressing back into RAM, so
    // require real headroom against the DECOMPRESSED footprint, not compressed.
    let required_mb = (g.zram_orig_mb as f64 * g.headroom_mult) as u64;
    if g.mem_available_mb <= required_mb {
        return Err(format!(
            "insufficient headroom: avail {}MB <= decompressed {}MB × {:.1} = {}MB",
            g.mem_available_mb, g.zram_orig_mb, g.headroom_mult, required_mb
        ));
    }
    Ok(())
}

/// Proactive swap reclaim (PRIVILEGED, PRIVILEGE_DESIGN §2). When the system is
/// calm, pull compressed pages back to RAM by cycling the zram swap device via
/// the capped `mgd-zram-reclaim` helper. All safety gates live HERE in the
/// unprivileged daemon; the helper is dumb. No-op unless
/// `[reclaim] proactive_swap_reclaim = true`.
///
/// `pressure` is the current PSI read; this is only ever called when the level
/// is already Normal, but we additionally require `some_avg60` to be calm so a
/// just-subsided spike doesn't trigger an immediate reclaim.
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

    // Extra calm gate: avg60 must be quiet, not just the instantaneous level.
    if pressure.some_avg60 >= 5.0 {
        return;
    }

    // Cooldown before any of the (cheap) sysfs reads.
    let now = now_secs();
    let last = LAST_RECLAIM.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < cooldown_secs {
        return;
    }

    // Resolve the helper from the standard locations. Probe once; disable for
    // the session if none is present so a missing opt-in install isn't logged
    // every minute. The resolved absolute path is what we exec.
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
        // Silent skip is fine for the common case; log at debug-ish level so the
        // reason is visible without spamming on every cycle is acceptable here
        // because we're already gated by cooldown.
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
        Err(e) => {
            // Don't arm the cooldown on failure (matches the other watchers).
            locked_print(&format!("[reclaim] helper failed: {e}"));
            log.log(&LogEntry::new("RECLAIM", 0, "zram", 0.0, &format!("failed: {e}")));
            // A privilege failure (helper present but uncapped) is persistent;
            // disable for the session so it isn't retried every cooldown.
            if e.contains("exit status: 1") || e.contains("EPERM") {
                RECLAIM_DISABLED.store(true, Ordering::Relaxed);
                locked_print("[reclaim] disabling for session (helper present but reclaim failed)");
            }
        }
    }
}

/// First candidate path that exists and is executable, or None. Probes the
/// fixed root-controlled list in order — never a PATH search, never env/config.
fn resolve_reclaim_helper() -> Option<&'static str> {
    RECLAIM_HELPER_CANDIDATES
        .iter()
        .copied()
        .find(|p| helper_available(p))
}

/// True if `path` exists and is executable (access(X_OK)). No PATH search.
fn helper_available(path: &str) -> bool {
    let Ok(c) = std::ffi::CString::new(path) else { return false };
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

/// Exec the capped helper with a CLEARED environment and no arguments — matches
/// the helper's no-argv/no-env discipline and removes any inherited-env vector.
/// Returns Err with a message on non-zero exit or spawn failure.
fn run_reclaim_helper(path: &str) -> Result<(), String> {
    let status = std::process::Command::new(path)
        .env_clear()
        .status()
        .map_err(|e| format!("spawn failed: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{status}"))
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
        // avail 10000 > 6000 × 1.5 = 9000 → passes.
        assert!(reclaim_gates_pass(&base_gates()).is_ok());
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
        // The OOM guard: decompressed 6000 × 1.5 = 9000; avail only 8000.
        let mut g = base_gates();
        g.mem_available_mb = 8000;
        assert!(reclaim_gates_pass(&g).is_err());
    }

    #[test]
    fn headroom_boundary_is_strict() {
        // avail exactly == required must FAIL (<=), no zero-margin reclaim.
        let mut g = base_gates();
        g.mem_available_mb = 9000; // == 6000 × 1.5
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
        // Every candidate must be an absolute path under a root-owned dir — no
        // relative paths, no PATH search, nothing user-writable.
        for p in RECLAIM_HELPER_CANDIDATES {
            assert!(p.starts_with('/'), "candidate not absolute: {p}");
            assert!(
                p.starts_with("/usr/local/bin/") || p.starts_with("/usr/bin/"),
                "candidate not in a standard root-controlled dir: {p}"
            );
        }
        // On the test host the helper isn't installed system-wide, so resolution
        // yields None rather than a bogus path.
        assert!(resolve_reclaim_helper().is_none() || resolve_reclaim_helper().unwrap().starts_with('/'));
    }
}
