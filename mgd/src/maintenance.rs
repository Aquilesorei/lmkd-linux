
//! MaintenanceManager — slow/blocking housekeeping kept off the 5s evictor loop.
//!
//! The CPU-idle sample (plasma-discover) and swapoff/swapon (reclaim) block, so
//! they run here on a 60s poll. Acts only at Normal pressure; under pressure the
//! evictor owns all process actions and the two must not act on one concurrently.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mgd_common::logger::{LogEntry, Logger};
use crate::engine::calibrate::{render_suggestion, Calibrator};
use crate::executor::registry::{CheckpointRegistry, FrozenRegistry};
use crate::monitor;
use crate::monitor::psi::PressureLevel;
use mgd_common::output::locked_print;
use libc;

const POLL_SECS: u64 = 60;

/// Persist calibration aggregates at most this often (plus shutdown flush).
const CALIBRATION_FLUSH_SECS: u64 = 600;

const RECLAIM_HELPER_CANDIDATES: &[&str] = &[
    "/usr/local/bin/mgd-zram-reclaim",
    "/usr/bin/mgd-zram-reclaim",
];



/// Unix-seconds of the last proactive swap reclaim (0 = never).
static LAST_RECLAIM: AtomicU64 = AtomicU64::new(0);

/// Set once when the reclaim helper is absent/uncapped, to log only once.
static RECLAIM_DISABLED: AtomicBool = AtomicBool::new(false);

pub fn run(
    log: Arc<Logger>,
    frozen: Arc<Mutex<FrozenRegistry>>,
    checkpointed: Arc<Mutex<CheckpointRegistry>>,
    calibrator: Arc<Mutex<Calibrator>>,
    reclaim_wake: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
) {
    let mut last_calibration_flush = Instant::now();
    // (pid, start_time) → last instant we saw cpu_pct > 0
    let mut auto_kill_last_active: HashMap<(u32, u64), Instant> = HashMap::new();

    loop {
        if crate::should_shutdown() {
            return;
        }

        crate::plugin_server::check_and_restart_plugins();

        let pressure = monitor::psi::read_pressure().ok();
        // A PSI read error counts as not-calm.
        let calm = pressure
            .as_ref()
            .map(|p| monitor::psi::pressure_level(p) == PressureLevel::Normal)
            .unwrap_or(false);

        // Check if evictor signalled that kills freed headroom.
        let kill_triggered = {
            let (lock, _) = &*reclaim_wake;
            let mut flag = lock.lock().unwrap();
            let was_set = *flag;
            *flag = false;
            was_set
        };

        let cycle_start = Instant::now();
        if calm {
            check_auto_kill_idle(&mut auto_kill_last_active, &frozen, &log);

            if let Some(p) = pressure.as_ref() {
                check_proactive_reclaim(p, &log, false);

                
                let intervention = frozen.lock().unwrap().count() > 0
                    || checkpointed.lock().unwrap().count() > 0;
                calibrator.lock().unwrap().observe(
                    p.some_avg10,
                    p.full_avg10,
                    intervention,
                    POLL_SECS,
                );
            }
        }

        if last_calibration_flush.elapsed().as_secs() >= CALIBRATION_FLUSH_SECS {
            last_calibration_flush = Instant::now();
            flush_calibration(&calibrator, &log);
        }

        if kill_triggered && !calm {
            if let Some(p) = pressure.as_ref() {
                mgd_common::sync_print!("[maintenance] Kill-triggered reclaim attempt (post-eviction headroom)");
                check_proactive_reclaim(p, &log, true);
            }
        }

        // Subtract time already spent (the idle sample) to hold the period at ~POLL_SECS.
        let spent = cycle_start.elapsed().as_secs();

        // Block on condvar instead of fixed sleep so the evictor can wake us early
        // when kills create headroom. Timeout = POLL_SECS (normal maintenance cadence).
        let (lock, cvar) = &*reclaim_wake;
        let _ = cvar.wait_timeout(
            lock.lock().unwrap(),
            Duration::from_secs(POLL_SECS.saturating_sub(spent)),
        );
    }
}

// ── Passive calibration persistence (Phase D) ─────────────────────────────────
// The Calibrator itself is pure (engine/calibrate.rs); all file I/O lives here.

pub fn calibration_state_path() -> PathBuf {
    mgd_common::util::home_dir().join(".local/share/mgd/calibration_state.toml")
}

pub fn calibration_suggestion_path() -> PathBuf {
    mgd_common::util::home_dir().join(".local/share/mgd/calibration_suggestion.toml")
}

/// Load persisted aggregates, or start fresh (first run / unparseable file).
pub fn load_calibrator() -> Calibrator {
    fs::read_to_string(calibration_state_path())
        .ok()
        .and_then(|s| Calibrator::from_toml(&s))
        .unwrap_or_else(Calibrator::new)
}

/// Persist aggregates if dirty, and (re)write the suggestion file once the
/// data gates pass. Called periodically from the loop and at shutdown.
pub fn flush_calibration(calibrator: &Arc<Mutex<Calibrator>>, log: &Logger) {
    let (state_toml, suggestion) = {
        let mut cal = calibrator.lock().unwrap();
        if !cal.dirty() {
            return;
        }
        (cal.to_toml(), cal.suggest())
    };

    let state_path = calibration_state_path();
    if let Some(parent) = state_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Err(e) = fs::write(&state_path, &state_toml) {
        mgd_common::sync_print!("[calibrate] cannot persist state to {}: {e}", state_path.display());
        return;
    }

    let Some(s) = suggestion else { return };
    let rendered = render_suggestion(&s, &crate::config::get().psi, now_secs());
    let sug_path = calibration_suggestion_path();
    // Rewrite only on change so the mtime stays meaningful and we log once
    // per actual revision, not every flush.
    if fs::read_to_string(&sug_path).ok().as_deref() == Some(rendered.as_str()) {
        return;
    }
    match fs::write(&sug_path, &rendered) {
        Ok(()) => {
            mgd_common::sync_print!(
                "[calibrate] [psi] suggestion ready ({:.0}h observed, {} stalls) → {}",
                s.observed_hours, s.stall_events, sug_path.display()
            );
            log.log(&LogEntry::new(
                "CALIBRATE", 0, "psi", s.elevated_pct,
                &format!("suggested elevated_pct={:.1} full_critical_pct={:.1}",
                    s.elevated_pct, s.full_critical_pct),
            ));
        }
        Err(e) => mgd_common::sync_print!("[calibrate] cannot write suggestion to {}: {e}", sug_path.display()),
    }
}

fn now_secs() -> u64 {
    mgd_common::util::unix_timestamp_secs()
}


fn check_auto_kill_idle(
    last_active: &mut HashMap<(u32, u64), Instant>,
    frozen: &Arc<Mutex<FrozenRegistry>>,
    log: &Logger,
) {
    let cfg = crate::config::get();
    if cfg.auto_kill_rules.is_empty() {
        return;
    }
    let procs = monitor::process::list_processes();
    let now = Instant::now();

    let live_keys: HashMap<(u32, u64), &crate::monitor::process::Process> = procs.iter()
        .filter_map(|p| {
            crate::executor::read_start_time(p.pid).map(|st| ((p.pid, st), p))
        })
        .collect();

    for (&key, &p) in &live_keys {
        let Some(idle_secs) = cfg.auto_kill_idle_after_for(&p.name) else { continue };
        if frozen.lock().unwrap().is_frozen(p.pid) {
            continue;
        }
        if p.cpu_pct > 0.1 {
            last_active.insert(key, now);
        } else {
            last_active.entry(key).or_insert(now);
        }
        let elapsed = now.duration_since(last_active[&key]).as_secs();
        if elapsed >= idle_secs {
            // Safety: normal SIGTERM, process may be ours or in our slice.
            let _ = unsafe { libc::kill(p.pid as libc::pid_t, libc::SIGTERM) };
            mgd_common::sync_print!(
                "[auto-kill] {} (pid {}) idle {}s >= {}s threshold — SIGTERM",
                p.name, p.pid, elapsed, idle_secs
            );
            log.log(&LogEntry::new(
                "KILL", p.pid, &p.name,
                p.rss_kb as f64 / 1024.0,
                "auto-kill-idle",
            ));
            last_active.remove(&key);
        }
    }

    last_active.retain(|k, _| live_keys.contains_key(k));
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

/// Minimum cooldown for the kill-triggered path (2 min). Shorter than the calm
/// path default (10 min) so post-eviction reclaim is more responsive.
const KILL_RECLAIM_COOLDOWN_SECS: u64 = 120;

/// Proactive swap reclaim via the capped helper (PRIVILEGED, PRIVILEGE_DESIGN
/// §2). No-op unless `[reclaim] proactive_swap_reclaim = true`. All gates live
/// here; the helper is dumb.
///
/// `skip_calm_gate`: when true (kill-triggered path), bypasses `some_avg60 < 5%`
/// and uses a shorter cooldown — kills just freed headroom, calm check is
/// irrelevant.
fn check_proactive_reclaim(
    pressure: &monitor::psi::MemoryPressure,
    log: &Logger,
    skip_calm_gate: bool,
) {
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

    if !skip_calm_gate && pressure.some_avg60 >= 5.0 {
        return;
    }

    let effective_cooldown = if skip_calm_gate {
        cooldown_secs.min(KILL_RECLAIM_COOLDOWN_SECS)
    } else {
        cooldown_secs
    };
    let now = now_secs();
    let last = LAST_RECLAIM.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < effective_cooldown {
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

    mgd_common::sync_print!(
        "[reclaim] {} swap {:.0}% full, zram {}MB compressed ({}MB decompressed), \
         avail {}MB — reclaiming",
        if skip_calm_gate { "post-eviction:" } else { "calm," },
        gates.swap_used_pct, gates.zram_used_mb, gates.zram_orig_mb, gates.mem_available_mb
    );

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
            mgd_common::sync_print!("[reclaim] helper failed (exit {code:?}): {e}");
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
