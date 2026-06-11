
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};


use crate::engine::decision::{Action, Decision, plan, get_priority};
use crate::executor::registry::{FrozenRegistry, CheckpointRegistry};
use mgd_common::logger::{LogEntry, Logger};
use crate::monitor;
use crate::monitor::meminfo::MemInfo;
use crate::monitor::process::Process;
use crate::monitor::psi::PressureLevel;



/// Set once on zram-compact EACCES (grant absent) to log only once per session.
static ZRAM_COMPACT_DISABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Unix-seconds of the last page-cache drop (0 = never).
static LAST_CACHE_DROP: AtomicU64 = AtomicU64::new(0);

pub fn run(
    frozen: Arc<Mutex<FrozenRegistry>>,
    checkpointed: Arc<Mutex<CheckpointRegistry>>,
    log: Arc<Logger>,
    recovery_wake: Arc<(Mutex<bool>, Condvar)>,
    calibrator: Arc<Mutex<crate::engine::calibrate::Calibrator>>,
) {
    mgd_common::sync_print!("[responder] PSI source: {}", monitor::psi::pressure_source());
    let psi_trigger = monitor::psi::PsiTrigger::new().ok();
    if let Some(t) = &psi_trigger {
        mgd_common::sync_print!("[responder] PSI epoll trigger registered on {} (zero-CPU idle).", t.source);
    } else {
        mgd_common::sync_print!("[responder] PSI epoll failed — falling back to 5s polling.");
    }

    let mut last_level = PressureLevel::Normal;

    loop {
        if crate::should_shutdown() { return; }

        if crate::should_reload() {
            crate::config::reload();
        }

        // Zero-CPU idle: if pressure was Normal last cycle, block on the kernel
        // PSI trigger instead of doing expensive /proc/pid walks.
        // Timeout 5s just to re-check shutdown flags and maintain the loop pulse.
        if last_level == PressureLevel::Normal {
            if let Some(trigger) = &psi_trigger {
                if !trigger.wait(5000) {
                    continue; // Timeout, no pressure -> skip the whole cycle
                }
            } else {
                thread::sleep(Duration::from_secs(5));
            }
        } else {
            // When Elevated or higher, poll actively so we can monitor recovery
            // or escalate if pressure rises further.
            thread::sleep(Duration::from_secs(5));
        }

        let pressure = match monitor::psi::read_pressure() {
            Ok(p) => p,
            Err(e) => {
                mgd_common::sync_print!("[responder] PSI error: {e}");
                thread::sleep(Duration::from_secs(5));
                continue;
            }
        };

        let level = monitor::psi::pressure_level(&pressure);
        let meminfo = crate::monitor::meminfo::read_meminfo();
        let effective_level = escalate_for_swap(&level, &meminfo);
        last_level = effective_level.clone();

        // Passive calibration (Phase D): feed the raw PSI sample before any
        // action this cycle. Samples taken while interventions are in flight
        // are excluded inside observe() — the daemon must not calibrate off
        // pressure it is already treating.
        {
            let intervention = frozen.lock().unwrap().count() > 0
                || checkpointed.lock().unwrap().count() > 0;
            calibrator.lock().unwrap().observe(
                pressure.some_avg10,
                pressure.full_avg10,
                intervention,
                5,
            );
        }

        if effective_level == PressureLevel::Normal {
            // We just recovered to Normal, or the 1s trigger fired but the 10s EMA 
            // hasn't crossed the 5.0% threshold yet. Do nothing.
            continue;
        }

        let mut procs = monitor::process::list_processes();
        procs.sort_by_key(|p| std::cmp::Reverse(p.rss_kb));

        print_status(&pressure, &effective_level, &procs, &meminfo, &frozen);

        crate::plugin_server::broadcast_pressure(&effective_level.to_string());

        // System pre-actions run before plan() so their freed RAM shrinks the
        // deficit. zram compact (cheaper) first, then cache drop.
        compact_zram(&effective_level, &log);
        check_cache_drop(&effective_level, &log);

        // Exclude frozen PIDs so their RSS isn't double-counted toward the deficit.
        let frozen_set: HashSet<u32> = frozen.lock().unwrap().frozen_pids().into_iter().collect();
        let plan_procs: Vec<&Process> = procs.iter()
            .filter(|p| !frozen_set.contains(&p.pid))
            .collect();

        let decisions = plan(&effective_level, &plan_procs, meminfo.available_kb, meminfo.total_kb);
        if decisions.is_empty() {
            mgd_common::sync_print!("✓ No action needed.");
        } else {
            mgd_common::sync_print!("⚡ EXECUTING:");
            for d in &decisions {
                if frozen.lock().unwrap().is_frozen(d.pid) { continue; }

                let result_str = execute_decision(d, &frozen, &checkpointed, &log);
                mgd_common::sync_print!("  {:<10} {:<8} {:<22} {:>6.1}MB  {}", d.action, d.pid, d.name, d.rss_mb, result_str);
            }
            // Ring the doorbell — recovery thread may have new work.
            let (lock, cvar) = &*recovery_wake;
            *lock.lock().unwrap() = true;
            cvar.notify_one();
        }
    }
}

/// Escalate one tier when swap is nearly full — PSI misses a slow swap fill.
fn escalate_for_swap(level: &PressureLevel, m: &MemInfo) -> PressureLevel {
    if m.swap_total_kb < 256 * 1024 || m.swap_used_pct() < 85.0 {
        return level.clone();
    }
    match level {
        PressureLevel::Normal    => PressureLevel::Elevated,
        PressureLevel::Elevated  => PressureLevel::High,
        PressureLevel::High      => PressureLevel::Critical,
        PressureLevel::Critical  => PressureLevel::Emergency,
        PressureLevel::Emergency => PressureLevel::Emergency,
    }
}


/// Compact zram at Elevated+ to free fragmented pages before touching a process.
/// No-op unless `[zram] compact_on_elevated = true`; skips pools < `min_used_mb`.
/// EACCES (grant absent) disables the feature for the session.
fn compact_zram(level: &PressureLevel, log: &Logger) {
    if *level < PressureLevel::Elevated {
        return;
    }
    if ZRAM_COMPACT_DISABLED.load(Ordering::Relaxed) {
        return;
    }

    let (enabled, min_used_mb) = {
        let cfg = crate::config::get();
        (cfg.compact_zram_on_elevated, cfg.zram_min_used_mb)
    };
    if !enabled {
        return;
    }

    for device in crate::monitor::zram::zram_devices() {
        // Gate before compacting; skip a device whose used-RAM is unreadable.
        let Some(before_mb) = crate::monitor::zram::zram_used_mb(&device) else { continue };
        if before_mb < min_used_mb {
            continue;
        }

        match crate::monitor::zram::compact(&device) {
            Ok(()) => {
                let after_mb = crate::monitor::zram::zram_used_mb(&device).unwrap_or(before_mb);
                let reclaimed = before_mb.saturating_sub(after_mb);
                mgd_common::sync_print!(
                    "[zram] compacted {device} — {before_mb}MB→{after_mb}MB used ({reclaimed}MB reclaimed)"
                );
                log.log(&LogEntry::new(
                    "ZRAM", 0, &device, reclaimed as f64,
                    &format!("compacted {before_mb}MB->{after_mb}MB"),
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                ZRAM_COMPACT_DISABLED.store(true, Ordering::Relaxed);
                mgd_common::sync_print!(
                    "[zram] compact unavailable ({device}): sysfs grant absent — disabling for \
                     session. See docs/PRIVILEGE_DESIGN.md §1."
                );
                log.log(&LogEntry::new("ZRAM", 0, &device, 0.0, "unavailable: EACCES (grant absent)"));
                return;
            }
            Err(e) => {
                mgd_common::sync_print!("[zram] compact failed on {device}: {e}");
            }
        }
    }
}

/// Drop page cache for configured trees at the trigger level+, before freezing.
/// No-op unless `[cache_drop] enabled` with non-empty `paths`. Cooldown-gated.
fn check_cache_drop(level: &PressureLevel, log: &Logger) {
    let (trigger, cooldown_secs, paths) = {
        let cfg = crate::config::get();
        if !cfg.cache_drop_enabled || cfg.cache_drop_paths.is_empty() {
            return;
        }
        (cfg.cache_drop_trigger.clone(), cfg.cache_drop_cooldown_secs, cfg.cache_drop_paths.clone())
    };

    if *level < trigger {
        return;
    }

    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let last = LAST_CACHE_DROP.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < cooldown_secs {
        return;
    }
    // Arm up-front: the walk is the cost being rate-limited.
    LAST_CACHE_DROP.store(now, Ordering::Relaxed);

    let mut total_files = 0usize;
    let mut total_bytes = 0u64;
    for r in crate::monitor::cache::drop_caches(&paths) {
        if r.files_advised > 0 {
            log.log(&LogEntry::new(
                "CACHE", 0, &r.pattern,
                (r.bytes_advised / (1024 * 1024)) as f64,
                &format!("advised {} files", r.files_advised),
            ));
        }
        total_files += r.files_advised;
        total_bytes += r.bytes_advised;
    }

    if total_files > 0 {
        let mb = total_bytes / (1024 * 1024);
        mgd_common::sync_print!("[cache] dropped cache for {total_files} files (~{mb}MB advised) before freeze");
    }
}

fn print_status(
    pressure: &monitor::psi::MemoryPressure,
    effective_level: &PressureLevel,
    procs: &[monitor::process::Process],
    meminfo: &MemInfo,
    frozen: &Arc<Mutex<FrozenRegistry>>,
) {
    let (total_rss, _) = procs.iter()
        .fold((0u64, 0u64), |(r, s), p| (r.saturating_add(p.rss_kb), s.saturating_add(p.swap_kb)));
    let frozen_count = frozen.lock().unwrap().count();

    mgd_common::sync_print!(
        "\n[responder] [{effective_level}] some avg10={:.2}% | RAM {:.0}/{:.0}MB | Swap {:.0}% | Avail {:.0}MB | Procs {} | Frozen {}",
        pressure.some_avg10,
        total_rss as f64 / 1024.0,
        meminfo.total_kb as f64 / 1024.0,
        meminfo.swap_used_pct(),
        meminfo.available_kb as f64 / 1024.0,
        procs.len(),
        frozen_count,
    );

    mgd_common::sync_print!("{:<8} {:<22} {:>8} {:>8} {:>5}  PRI", "PID", "NAME", "RSS(MB)", "SWP(MB)", "OOM");
    mgd_common::sync_print!("{}", "-".repeat(65));
    let reg = frozen.lock().unwrap();
    for p in procs.iter().take(10) {
        let marker = if reg.is_frozen(p.pid) { " ❄" } else { "" };
        mgd_common::sync_print!("{:<8} {:<22} {:>8.1} {:>8.1} {:>5}  {}{}",
            p.pid, p.name,
            p.rss_kb as f64 / 1024.0,
            p.swap_kb as f64 / 1024.0,
            p.oom_score,
            get_priority(&p.name, p.exe_basename.as_deref()),
            marker,
        );
    }
}

fn execute_decision(
    d: &Decision,
    frozen: &Arc<Mutex<FrozenRegistry>>,
    checkpointed: &Arc<Mutex<CheckpointRegistry>>,
    log: &Logger,
) -> String {
    let (action_name, s) = match d.action {
        Action::Freeze => ("FREEZE", execute_freeze(d, frozen)),
        Action::Terminate => ("TERMINATE", execute_terminate(d)),
        Action::Kill => ("KILL", execute_kill(d)),
        Action::Checkpoint => ("CHECKPOINT", execute_checkpoint(d, checkpointed)),
        Action::None => return String::new(),
    };
    log.log(&LogEntry::new(action_name, d.pid, &d.name, d.rss_mb, &s));
    s
}

fn execute_freeze(d: &Decision, frozen: &Arc<Mutex<FrozenRegistry>>) -> String {
    // Abort if start_time is gone rather than freeze a recycled PID.
    let st = match crate::executor::read_start_time(d.pid) {
        Some(t) => t,
        None => return "aborted: process vanished before freeze".into(),
    };
    let r = crate::executor::freezer::freeze_checked(d.pid, st);
    if r.success {
        if frozen.lock().unwrap().add(d.pid, &d.name) {
            "frozen".into()
        } else {
            crate::executor::freezer::unfreeze(d.pid);
            "aborted: process vanished before fingerprint".into()
        }
    } else {
        format!("fail: {}", r.error.unwrap_or_default())
    }
}

fn execute_terminate(d: &Decision) -> String {
    // SIGTERM→wait→SIGKILL blocks up to 5s; run it off-thread so the responder
    // isn't stalled per process at Critical.
    let pid = d.pid;
    std::thread::spawn(move || { crate::executor::killer::terminate(pid); });
    "terminating (async SIGTERM→SIGKILL)".into()
}

fn execute_kill(d: &Decision) -> String {
    let r = crate::executor::killer::kill(d.pid);
    if r.success { "killed".into() }
    else { format!("fail: {}", r.error.unwrap_or_default()) }
}

fn execute_checkpoint(d: &Decision, checkpointed: &Arc<Mutex<CheckpointRegistry>>) -> String {
    let r = crate::executor::checkpoint::checkpoint(d.pid, &d.name);
    if r.success {
        let dir = r.snapshot_dir.unwrap();
        checkpointed.lock().unwrap()
            .add(d.pid, &d.name, dir.clone(), d.rss_mb as u64 * 1024);
        format!("checkpointed → {dir:?}")
    } else {
        let kr = crate::executor::killer::kill(d.pid);
        if kr.success { "killed (CRIU failed)".into() }
        else { format!("kill_fail: {}", kr.error.unwrap_or_default()) }
    }
}
