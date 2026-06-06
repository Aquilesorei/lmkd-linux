use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::sync_print;
use crate::engine::decision::{Action, Decision, plan, get_priority};
use crate::executor::registry::{FrozenRegistry, CheckpointRegistry};
use crate::logger::{LogEntry, Logger};
use crate::monitor;
use crate::monitor::meminfo::MemInfo;
use crate::monitor::process::Process;
use crate::monitor::psi::PressureLevel;

/// Unix-seconds of the last plasmashell restart (0 = never). Cooldown floor state
/// for the GPU-leak watcher; mirrors the AtomicBool pattern used in main.rs.
static LAST_PLASMA_RESTART: AtomicU64 = AtomicU64::new(0);

/// Unix-seconds of the last Firefox GC trigger (0 = never). Cooldown floor for the
/// preventive Firefox watcher.
static LAST_FIREFOX_GC: AtomicU64 = AtomicU64::new(0);

pub fn run(
    frozen: Arc<Mutex<FrozenRegistry>>,
    checkpointed: Arc<Mutex<CheckpointRegistry>>,
    log: Arc<Logger>,
) {
    loop {
        if crate::should_shutdown() { return; }

        // SIGHUP: reload config before the next decision cycle
        if crate::should_reload() {
            crate::config::reload();
        }

        let pressure = match monitor::psi::read_pressure() {
            Ok(p) => p,
            Err(e) => {
                sync_print!("[responder] PSI error: {e}");
                thread::sleep(Duration::from_secs(5));
                continue;
            }
        };

        let level = monitor::psi::pressure_level(&pressure);
        let meminfo = crate::monitor::meminfo::read_meminfo();
        let effective_level = escalate_for_swap(&level, &meminfo);

        let mut procs = monitor::process::list_processes();
        procs.sort_by_key(|p| std::cmp::Reverse(p.rss_kb));

        print_status(&pressure, &effective_level, &procs, &meminfo, &frozen);

        // Optional KDE Plasma GPU-leak watcher (gated behind [plasma] config).
        check_plasma_gpu(&procs, &log);

        // Optional Firefox preventive-memory watcher (gated behind [firefox] config).
        // Pass effective_level so swap-escalated pressure also suppresses it.
        check_firefox_memory(&procs, &effective_level, &log);

        // Fix 1: exclude already-frozen PIDs from plan so their RSS isn't
        // double-counted toward deficit, which would cause underkill.
        let frozen_set: HashSet<u32> = frozen.lock().unwrap().frozen_pids().into_iter().collect();
        let plan_procs: Vec<&Process> = procs.iter()
            .filter(|p| !frozen_set.contains(&p.pid))
            .collect();

        let decisions = plan(&effective_level, &plan_procs, meminfo.available_kb, meminfo.total_kb);
        if decisions.is_empty() {
            sync_print!("✓ No action needed.");
        } else {
            sync_print!("⚡ EXECUTING:");
            for d in &decisions {
                if frozen.lock().unwrap().is_frozen(d.pid) { continue; }

                let result_str = execute_decision(d, &frozen, &checkpointed, &log);
                sync_print!("  {:<10} {:<8} {:<22} {:>6.1}MB  {}", d.action, d.pid, d.name, d.rss_mb, result_str);
            }
        }

        thread::sleep(Duration::from_secs(5));
    }
}

/// Fix 2: escalate pressure level when swap is nearly exhausted.
/// PSI alone misses pre-OOM state when swap fills slowly — this catches it.
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

/// KDE Plasma + Intel UMA workaround: plasmashell leaks GPU memory (allocated from
/// system RAM) over long uptimes. When it crosses the configured threshold and the
/// cooldown has elapsed, restart it and log the reclaimed memory.
///
/// No-op unless `[plasma] watch_gpu_leak = true`. Reads GPU residency from fdinfo
/// with no elevated privileges.
fn check_plasma_gpu(procs: &[Process], log: &Logger) {
    let (threshold_mb, cooldown_secs) = {
        let cfg = crate::config::get();
        if !cfg.watch_gpu_leak {
            return;
        }
        (cfg.gpu_leak_threshold_mb, cfg.min_restart_interval_secs)
    };

    let Some(plasma) = procs.iter().find(|p| p.name == "plasmashell") else { return };

    let Some(gpu_kb) = crate::monitor::gpu::process_gpu_kb(plasma.pid) else { return };
    let before_mb = gpu_kb / 1024;
    if before_mb <= threshold_mb {
        return;
    }

    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let last = LAST_PLASMA_RESTART.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < cooldown_secs {
        return; // cooldown floor not elapsed
    }

    sync_print!("[plasma] plasmashell GPU mem {before_mb}MB > {threshold_mb}MB threshold — restarting");
    if let Err(e) = crate::monitor::gpu::restart_plasmashell() {
        // Don't arm the cooldown on failure, so a transient/missing-binary case
        // isn't silently muted for the whole interval.
        sync_print!("[plasma] restart skipped: {e}");
        log.log(&LogEntry::new("RESTART", plasma.pid, "plasmashell", 0.0, &format!("skipped: {e}")));
        return;
    }

    LAST_PLASMA_RESTART.store(now, Ordering::Relaxed);

    // Re-read against the *new* plasmashell instance to report what was reclaimed.
    let new_procs = crate::monitor::process::list_processes();
    let new_plasma = new_procs.iter().find(|p| p.name == "plasmashell");
    let pid = new_plasma.map(|p| p.pid).unwrap_or(plasma.pid);
    let after_mb = new_plasma
        .and_then(|p| crate::monitor::gpu::process_gpu_kb(p.pid))
        .map(|kb| kb / 1024)
        .unwrap_or(0);

    log.log(&LogEntry::new(
        "RESTART", pid, "plasmashell", 0.0,
        &format!("gpu_leak_reclaimed {before_mb}MB→{after_mb}MB"),
    ));
}

/// Firefox preventive-memory watcher. Runs ONLY at PressureLevel::Normal — under any
/// pressure the evictor already manages Firefox content processes via the priority
/// system, and the two must never act on Firefox concurrently. When healthy and RSS
/// is high, nudges Firefox's internal GC via SIGUSR1 (non-disruptive, no restart).
///
/// No-op unless `[firefox] watch_memory = true`.
fn check_firefox_memory(procs: &[Process], level: &PressureLevel, log: &Logger) {
    let (rss_threshold_mb, warn_threshold_mb, cooldown_secs) = {
        let cfg = crate::config::get();
        if !cfg.watch_firefox {
            return;
        }
        (cfg.firefox_rss_threshold_mb, cfg.firefox_warn_threshold_mb, cfg.firefox_gc_cooldown_secs)
    };

    // Preventive maintenance only: bail on any pressure. The evictor handles
    // Firefox under pressure — never run both on Firefox at once.
    if *level != PressureLevel::Normal {
        return;
    }

    let (total_kb, _pids) = crate::monitor::firefox::firefox_total_rss_kb(procs);
    let total_mb = total_kb / 1024;

    // Warning is informational and always logged (no cooldown), independent of GC.
    if total_mb >= warn_threshold_mb {
        log.log(&LogEntry::new("WARN", 0, "firefox", total_mb as f64, "rss_above_warn_threshold"));
    }

    if total_mb < rss_threshold_mb {
        return;
    }

    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let last = LAST_FIREFOX_GC.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < cooldown_secs {
        return; // GC cooldown not elapsed
    }

    match crate::monitor::firefox::trigger_firefox_gc(procs) {
        Ok(pid) => {
            LAST_FIREFOX_GC.store(now, Ordering::Relaxed);
            log.log(&LogEntry::new("GC", pid, "firefox", total_mb as f64, "gc_triggered"));
        }
        Err(e) => {
            // Don't arm the cooldown on failure.
            sync_print!("[firefox] GC skipped: {e}");
            log.log(&LogEntry::new("GC", 0, "firefox", total_mb as f64, &format!("skipped: {e}")));
        }
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

    sync_print!(
        "\n[responder] [{effective_level}] some avg10={:.2}% | RAM {:.0}/{:.0}MB | Swap {:.0}% | Avail {:.0}MB | Procs {} | Frozen {}",
        pressure.some_avg10,
        total_rss as f64 / 1024.0,
        meminfo.total_kb as f64 / 1024.0,
        meminfo.swap_used_pct(),
        meminfo.available_kb as f64 / 1024.0,
        procs.len(),
        frozen_count,
    );

    sync_print!("{:<8} {:<22} {:>8} {:>8} {:>5}  PRI", "PID", "NAME", "RSS(MB)", "SWP(MB)", "OOM");
    sync_print!("{}", "-".repeat(65));
    let reg = frozen.lock().unwrap();
    for p in procs.iter().take(10) {
        let marker = if reg.is_frozen(p.pid) { " ❄" } else { "" };
        sync_print!("{:<8} {:<22} {:>8.1} {:>8.1} {:>5}  {}{}",
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
    // Read start_time first — if it's gone, abort rather than freeze a recycled PID.
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
    // Spawn the blocking SIGTERM→wait→SIGKILL sequence on a background thread so
    // the responder isn't stalled for up to 5s per process at Critical pressure.
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
