
use std::collections::{HashSet, HashMap};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ControlState {
    Calm,
    Warning,
    Evicting,
    Critical,
    Emergency,
}

impl ControlState {
    fn to_pressure_level(self) -> PressureLevel {
        match self {
            ControlState::Calm => PressureLevel::Normal,
            ControlState::Warning => PressureLevel::Elevated,
            ControlState::Evicting => PressureLevel::High,
            ControlState::Critical => PressureLevel::Critical,
            ControlState::Emergency => PressureLevel::Emergency,
        }
    }
}

/// Set once on zram-compact EACCES (grant absent) to log only once per session.
static ZRAM_COMPACT_DISABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Unix-seconds of the last page-cache drop (0 = never).
static LAST_CACHE_DROP: AtomicU64 = AtomicU64::new(0);

/// Unix-seconds of the last early background process reclaim.
static LAST_EARLY_RECLAIM: AtomicU64 = AtomicU64::new(0);

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
    let mut last_time = std::time::Instant::now();
    let mut last_score = 0.0;
    let mut current_state = ControlState::Calm;
    let mut pending_state = ControlState::Calm;
    let mut pending_ticks = 0usize;
    let mut background_tracker: HashMap<String, std::time::Instant> = HashMap::new();
    let mut throttled_states: HashMap<String, ThrottledState> = HashMap::new();
    let mut last_idle_reclaim_check = std::time::Instant::now();
    let mut last_active_pid = None;

    loop {
        if crate::should_shutdown() {
            // Cleanup CPU throttling on shutdown
            for path in throttled_states.keys() {
                let _ = write_cgroup_cpu_weight(path, 100);
                let _ = write_cgroup_cpu_max(path, "max 100000");
            }
            return;
        }

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

        let meminfo = crate::monitor::meminfo::read_meminfo();

        // Calculate continuous pressure score: 60% PSI, 25% Swap used, 15% GPU UMA overhead
        let psi_val = (pressure.some_avg10 / 100.0).clamp(0.0, 1.0);
        let swap_val = if meminfo.swap_total_kb > 0 {
            (meminfo.swap_used_pct() / 100.0).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let total_gpu = crate::plugin_server::get_total_gpu_kb();
        let gpu_val = if meminfo.total_kb > 0 {
            (total_gpu as f64 / meminfo.total_kb as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let p_score = 0.60 * psi_val + 0.25 * swap_val + 0.15 * gpu_val;

        // Calculate trend (dP/dt)
        let now = std::time::Instant::now();
        let dt = now.duration_since(last_time).as_secs_f64();
        let trend = if dt > 0.1 {
            (p_score - last_score) / dt
        } else {
            0.0
        };
        last_score = p_score;
        last_time = now;

        // Determine target state based on score & trend
        let target_state = if p_score >= 0.70 || (p_score >= 0.55 && trend > 0.05) {
            ControlState::Emergency
        } else if p_score >= 0.50 || (p_score >= 0.35 && trend > 0.03) {
            ControlState::Critical
        } else if p_score >= 0.30 || (p_score >= 0.20 && trend > 0.02) {
            ControlState::Evicting
        } else if p_score >= 0.15 {
            ControlState::Warning
        } else {
            ControlState::Calm
        };

        // State transitions with hysteresis
        if target_state > current_state {
            // Escalation: Needs 2 ticks of persistence, unless it's a massive spike (instant)
            let instant_escalate = (target_state == ControlState::Emergency || target_state == ControlState::Critical) && trend > 0.08;
            if target_state == pending_state && !instant_escalate {
                pending_ticks += 1;
                if pending_ticks >= 2 {
                    current_state = target_state;
                    pending_ticks = 0;
                }
            } else if instant_escalate {
                mgd_common::sync_print!("[controller] Instant escalation triggered due to rapid pressure spike (trend: {:.3})", trend);
                current_state = target_state;
                pending_state = target_state;
                pending_ticks = 0;
            } else {
                pending_state = target_state;
                pending_ticks = 1;
            }
        } else if target_state < current_state {
            // Recovery: Needs longer persistence
            let required_ticks = match target_state {
                ControlState::Calm => 12,    // 1 minute of Calm at 5s polling
                ControlState::Warning => 6, // 30s
                _ => 4,                     // 20s
            };
            if target_state == pending_state {
                pending_ticks += 1;
                if pending_ticks >= required_ticks {
                    current_state = target_state;
                    pending_ticks = 0;
                }
            } else {
                pending_state = target_state;
                pending_ticks = 1;
            }
        } else {
            // Target matches current state: reset pending state
            pending_state = current_state;
            pending_ticks = 0;
        }

        let mut effective_level = current_state.to_pressure_level();

        let swap_exhausted = meminfo.swap_total_kb > 0 && meminfo.swap_used_pct() >= 95.0;
        if swap_exhausted && effective_level < PressureLevel::Critical {
            effective_level = PressureLevel::Critical;
            mgd_common::sync_print!(
                "[controller] Swap exhausted ({:.1}% used) — forcing effective pressure to CRITICAL to trigger eviction",
                meminfo.swap_used_pct()
            );
        }

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

        // Background CPU Throttling and Idle cgroup reclaim manager
        let config = crate::config::get();
        let active_pid = crate::plugin_server::get_active_foreground_pid();
        let now_inst = std::time::Instant::now();
        let idle_reclaim_interval_elapsed = now_inst.duration_since(last_idle_reclaim_check).as_secs() >= config.idle_reclaim_global_cooldown_sec;
        let active_pid_changed = active_pid != last_active_pid;

        if active_pid_changed || idle_reclaim_interval_elapsed {
            last_active_pid = active_pid;
            if idle_reclaim_interval_elapsed {
                last_idle_reclaim_check = now_inst;
            }

            let procs = monitor::process::list_processes();
            let frozen_set: HashSet<u32> = frozen.lock().unwrap().frozen_pids().into_iter().collect();
            let plan_procs: Vec<&Process> = procs.iter()
                .filter(|p| !frozen_set.contains(&p.pid))
                .collect();

            // Update CPU throttling (tiered, debounced)
            update_cpu_throttling(&plan_procs, active_pid, &mut background_tracker, &mut throttled_states);

            // Idle cgroup reclaim: only runs at Normal/Calm pressure
            if effective_level == PressureLevel::Normal {
                if config.idle_reclaim_enabled {
                    check_idle_process_reclaim(&plan_procs, active_pid, &mut background_tracker, &log);
                }
            }
        }

        if effective_level == PressureLevel::Normal {
            continue;
        }

        let mut procs = monitor::process::list_processes();
        procs.sort_by_key(|p| std::cmp::Reverse(p.rss_kb));

        print_status(&pressure, &effective_level, &procs, &meminfo, &frozen);

        crate::plugin_server::broadcast_pressure(&effective_level.to_string());

        // System pre-actions run before plan() so their freed RAM shrinks the
        // deficit. zram compact (cheaper) first, then cache drop.
        compact_zram(&effective_level, &log);

        // Exclude frozen PIDs so their RSS isn't double-counted toward the deficit.
        let frozen_set: HashSet<u32> = frozen.lock().unwrap().frozen_pids().into_iter().collect();
        let plan_procs: Vec<&Process> = procs.iter()
            .filter(|p| !frozen_set.contains(&p.pid))
            .collect();

        let active_pid = crate::plugin_server::get_active_foreground_pid();
        check_early_process_reclaim(&effective_level, &plan_procs, active_pid, &log);
        check_cache_drop(&effective_level, &log);

        let decisions = plan(&effective_level, &plan_procs, meminfo.available_kb, meminfo.total_kb, swap_exhausted);
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

fn reclaim_process_cgroup(pid: u32, bytes: u64) -> Result<(), std::io::Error> {
    let cgroup_file = format!("/proc/{}/cgroup", pid);
    let content = std::fs::read_to_string(&cgroup_file)?;
    for line in content.lines() {
        if let Some(path) = line.strip_prefix("0::") {
            let path = path.trim();
            if path == "/" {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "cannot reclaim root cgroup",
                ));
            }
            let reclaim_path = std::path::Path::new("/sys/fs/cgroup")
                .join(path.trim_start_matches('/'))
                .join("memory.reclaim");
            if reclaim_path.exists() {
                std::fs::write(&reclaim_path, format!("{}", bytes))?;
                return Ok(());
            }
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, "cgroup memory.reclaim not found"))
}

fn check_early_process_reclaim(
    level: &PressureLevel,
    plan_procs: &[&Process],
    active_pid: Option<u32>,
    log: &Logger,
) {
    if *level != PressureLevel::Elevated {
        return;
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let last = LAST_EARLY_RECLAIM.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < 30 {
        return; // 30s cooldown
    }
    LAST_EARLY_RECLAIM.store(now, Ordering::Relaxed);

    // Filter candidate background processes:
    // - Priority >= 50 (expendable/user apps)
    // - RSS > 20MB
    // - Not the active foreground process
    let mut targets: Vec<&Process> = plan_procs
        .iter()
        .filter(|p| {
            p.rss_kb > 20_000
                && Some(p.pid) != active_pid
                && get_priority(&p.name, p.exe_basename.as_deref()) >= 50
        })
        .copied()
        .collect();

    // Sort by RSS descending to target the largest background processes first
    targets.sort_by_key(|p| std::cmp::Reverse(p.rss_kb));

    for p in targets.iter().take(3) {
        let reclaim_bytes = (p.rss_kb / 2) * 1024; // reclaim 50% of RSS
        match reclaim_process_cgroup(p.pid, reclaim_bytes) {
            Ok(()) => {
                mgd_common::sync_print!(
                    "[reclaim] Proactively pushed ~{}MB of background PID {} ({}) to Zram",
                    reclaim_bytes / (1024 * 1024),
                    p.pid,
                    p.name
                );
                log.log(&LogEntry::new(
                    "EARLY_RECLAIM",
                    p.pid,
                    &p.name,
                    (reclaim_bytes / (1024 * 1024)) as f64,
                    "pushed to zram via cgroup reclaim",
                ));
            }
            Err(e) => {
                // Ignore transient errors, log at debug
                mgd_common::sync_print!(
                    "[reclaim] Early reclaim failed for PID {} ({}): {}",
                    p.pid,
                    p.name,
                    e
                );
            }
        }
    }
}

fn check_idle_process_reclaim(
    plan_procs: &[&Process],
    active_pid: Option<u32>,
    background_tracker: &mut HashMap<String, std::time::Instant>,
    log: &Logger,
) {
    let config = crate::config::get();
    
    // Safety gates: swap/zram occupancy
    let meminfo = crate::monitor::meminfo::read_meminfo();
    if meminfo.swap_total_kb > 0 {
        let swap_used_pct = meminfo.swap_used_pct();
        if swap_used_pct > config.idle_reclaim_max_swap_occupancy_pct {
            return;
        }
        let swap_free_mb = meminfo.swap_free_kb / 1024;
        if swap_free_mb < 1500 {
            return; // Less than 1.5 GB swap free
        }
    }

    // 1. Group processes by cgroup path
    let mut cgroup_groups: HashMap<String, Vec<&Process>> = HashMap::new();
    for p in plan_procs {
        if let Some(cgroup_path) = get_process_cgroup_path(p.pid) {
            cgroup_groups.entry(cgroup_path).or_default().push(p);
        }
    }

    // 2. Identify the active foreground cgroup path (if any)
    let foreground_cgroup_path = active_pid.and_then(|pid| get_process_cgroup_path(pid));

    // 3. Prune dead cgroups from background tracker
    let active_cgroups: HashSet<&String> = cgroup_groups.keys().collect();
    background_tracker.retain(|path, _| active_cgroups.contains(path));

    let mut reclaimed_count = 0;
    for (cgroup_path, processes) in &cgroup_groups {
        if Some(cgroup_path) == foreground_cgroup_path.as_ref() {
            background_tracker.remove(cgroup_path);
            continue;
        }

        // Sum the total RSS of processes in this cgroup
        let total_rss_kb: u64 = processes.iter().map(|p| p.rss_kb).sum();
        
        // Find minimum priority in this cgroup
        let mut min_priority = 100;
        let mut debug_name = String::new();
        for p in processes {
            let prio = get_priority(&p.name, p.exe_basename.as_deref());
            if prio < min_priority {
                min_priority = prio;
                debug_name = p.name.clone();
            }
        }

        if min_priority < 50 || total_rss_kb < config.idle_reclaim_rss_min_mb * 1024 {
            background_tracker.remove(cgroup_path);
            continue;
        }

        // Track duration in background
        let background_duration = background_tracker
            .entry(cgroup_path.clone())
            .or_insert_with(std::time::Instant::now)
            .elapsed()
            .as_secs();

        if background_duration >= config.idle_reclaim_sec {
            // Qualifies for reclaim!
            let reclaim_bytes = (total_rss_kb * config.idle_reclaim_pct / 100) * 1024;
            if reclaim_bytes > 0 {
                // Write to the cgroup's memory.reclaim
                let reclaim_path = std::path::Path::new("/sys/fs/cgroup")
                    .join(cgroup_path.trim_start_matches('/'))
                    .join("memory.reclaim");
                
                if reclaim_path.exists() {
                    match std::fs::write(&reclaim_path, format!("{}", reclaim_bytes)) {
                        Ok(()) => {
                            mgd_common::sync_print!(
                                "[reclaim] Proactively reclaimed ~{}MB from idle background cgroup {} (e.g. {})",
                                reclaim_bytes / (1024 * 1024),
                                cgroup_path,
                                debug_name
                            );
                            // Log using the PID of the largest process in the cgroup for compatibility with log tools
                            let max_proc = processes.iter().max_by_key(|p| p.rss_kb);
                            if let Some(p) = max_proc {
                                log.log(&LogEntry::new(
                                    "EARLY_RECLAIM",
                                    p.pid,
                                    &p.name,
                                    (reclaim_bytes / (1024 * 1024)) as f64,
                                    "proactively pushed idle cgroup to zram",
                                ));
                            }

                            // Reset the timer in tracker to serve as per-process cooldown
                            background_tracker.insert(cgroup_path.clone(), std::time::Instant::now());

                            reclaimed_count += 1;
                            if reclaimed_count >= 3 {
                                break;
                            }
                        }
                        Err(e) => {
                            mgd_common::sync_print!(
                                "[reclaim] Proactive idle reclaim failed for cgroup {} ({}): {}",
                                cgroup_path,
                                debug_name,
                                e
                            );
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThrottledState {
    None,
    WeightOnly,
    Full,
}

fn get_process_cgroup_path(pid: u32) -> Option<String> {
    let cgroup_file = format!("/proc/{}/cgroup", pid);
    let content = std::fs::read_to_string(&cgroup_file).ok()?;
    for line in content.lines() {
        if let Some(path) = line.strip_prefix("0::") {
            let path = path.trim();
            if path != "/" {
                return Some(path.to_string());
            }
        }
    }
    None
}

fn write_cgroup_cpu_weight(cgroup_path: &str, weight: u32) -> Result<(), std::io::Error> {
    let path = std::path::Path::new("/sys/fs/cgroup")
        .join(cgroup_path.trim_start_matches('/'))
        .join("cpu.weight");
    if path.exists() {
        std::fs::write(&path, format!("{}", weight))?;
        return Ok(());
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, "cpu.weight not found"))
}

fn write_cgroup_cpu_max(cgroup_path: &str, max_limit: &str) -> Result<(), std::io::Error> {
    let path = std::path::Path::new("/sys/fs/cgroup")
        .join(cgroup_path.trim_start_matches('/'))
        .join("cpu.max");
    if path.exists() {
        std::fs::write(&path, max_limit)?;
        return Ok(());
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, "cpu.max not found"))
}

fn update_cpu_throttling(
    plan_procs: &[&Process],
    active_pid: Option<u32>,
    background_tracker: &mut HashMap<String, std::time::Instant>,
    throttled_states: &mut HashMap<String, ThrottledState>,
) {
    // 1. Group processes by cgroup path
    let mut cgroup_groups: HashMap<String, Vec<&Process>> = HashMap::new();
    for p in plan_procs {
        if let Some(cgroup_path) = get_process_cgroup_path(p.pid) {
            cgroup_groups.entry(cgroup_path).or_default().push(p);
        }
    }

    // 2. Identify the active foreground cgroup path (if any)
    let foreground_cgroup_path = active_pid.and_then(|pid| get_process_cgroup_path(pid));

    // 3. Prune dead cgroups from trackers
    let active_cgroups: HashSet<&String> = cgroup_groups.keys().collect();
    background_tracker.retain(|path, _| active_cgroups.contains(path));
    throttled_states.retain(|path, _| active_cgroups.contains(path));

    // 4. Update cgroup throttling states
    for (cgroup_path, processes) in &cgroup_groups {
        let current_throttled = throttled_states.get(cgroup_path).copied().unwrap_or(ThrottledState::None);

        if Some(cgroup_path) == foreground_cgroup_path.as_ref() {
            // Foreground cgroup must be unthrottled instantly
            if current_throttled != ThrottledState::None {
                let _ = write_cgroup_cpu_weight(cgroup_path, 100);
                let _ = write_cgroup_cpu_max(cgroup_path, "max 100000");
                throttled_states.insert(cgroup_path.clone(), ThrottledState::None);
                mgd_common::sync_print!("[throttle] Restored foreground cgroup {} to normal CPU shares", cgroup_path);
            }
            background_tracker.remove(cgroup_path);
            continue;
        }

        // Background cgroup: find minimum priority of processes inside it
        let mut min_priority = 100;
        let mut debug_name = String::new();
        for p in processes {
            let prio = get_priority(&p.name, p.exe_basename.as_deref());
            if prio < min_priority {
                min_priority = prio;
                debug_name = p.name.clone();
            }
        }

        if min_priority < 60 {
            // Exclude priorities < 60 (system, high, normal apps like IDEs/browsers) from CPU throttling
            if current_throttled != ThrottledState::None {
                let _ = write_cgroup_cpu_weight(cgroup_path, 100);
                let _ = write_cgroup_cpu_max(cgroup_path, "max 100000");
                throttled_states.insert(cgroup_path.clone(), ThrottledState::None);
                mgd_common::sync_print!("[throttle] Restored background cgroup {} to normal CPU shares (priority < 60)", cgroup_path);
            }
            background_tracker.remove(cgroup_path);
            continue;
        }

        // Track duration in background
        let background_duration = background_tracker
            .entry(cgroup_path.clone())
            .or_insert_with(std::time::Instant::now)
            .elapsed()
            .as_secs();

        // Target throttled state
        let target_throttled = if background_duration >= 10 { // 10s debounce
            if min_priority >= 80 {
                ThrottledState::Full
            } else {
                ThrottledState::WeightOnly
            }
        } else {
            ThrottledState::None
        };

        if target_throttled != current_throttled {
            match target_throttled {
                ThrottledState::None => {
                    let _ = write_cgroup_cpu_weight(cgroup_path, 100);
                    let _ = write_cgroup_cpu_max(cgroup_path, "max 100000");
                    mgd_common::sync_print!("[throttle] Unthrottled cgroup {}", cgroup_path);
                }
                ThrottledState::WeightOnly => {
                    if write_cgroup_cpu_weight(cgroup_path, 1).is_ok() {
                        let _ = write_cgroup_cpu_max(cgroup_path, "max 100000");
                        mgd_common::sync_print!(
                            "[throttle] Set weight=1 for background cgroup {} (e.g. {})",
                            cgroup_path,
                            debug_name
                        );
                    }
                }
                ThrottledState::Full => {
                    if write_cgroup_cpu_weight(cgroup_path, 1).is_ok() && write_cgroup_cpu_max(cgroup_path, "50000 100000").is_ok() {
                        mgd_common::sync_print!(
                            "[throttle] Capped CPU & weight=1 for low-priority cgroup {} (e.g. {})",
                            cgroup_path,
                            debug_name
                        );
                    }
                }
            }
            throttled_states.insert(cgroup_path.clone(), target_throttled);
        }
    }
}
