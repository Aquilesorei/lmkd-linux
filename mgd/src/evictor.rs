
use std::collections::{HashSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use mgd_common::util::unix_timestamp_secs;


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
    throttle_snapshot: Arc<Mutex<HashMap<String, crate::throttle::ThrottledState>>>,
    event_log: crate::events::EventLog,
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
    let mut throttle = crate::throttle::ThrottleManager::new();
    let mut idle_reclaim_pid_tracker: HashMap<u32, std::time::Instant> = HashMap::new();
    let mut idle_freeze_pid_tracker: HashMap<u32, std::time::Instant> = HashMap::new();
    let mut last_idle_reclaim_check = std::time::Instant::now();
    let mut last_active_pid = None;
    let mut recently_killed_cgroups: HashMap<String, std::time::Instant> = HashMap::new();
    let mut sustained_critical_swap_start: Option<std::time::Instant> = None;
    let (mut last_pswpin, mut last_pswpout) = crate::monitor::meminfo::read_vmstat_swap_counters();
    let mut memcap = crate::throttle::MemCapManager::new();
    let mut sustained_emergency_start: Option<std::time::Instant> = None;
    let mut hibernate_triggered = false;

    loop {
        if crate::should_shutdown() {
            throttle.restore_all();
            memcap.restore_all();
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
                    // Timeout: no PSI event, system is calm.
                    // Still run idle reclaim if the cooldown has elapsed so
                    // background cgroups get reclaimed proactively during quiet
                    // periods rather than waiting for the next pressure spike.
                    {
                        let config = crate::config::get();
                        if config.idle_reclaim_enabled {
                            let now_inst = std::time::Instant::now();
                            if now_inst.duration_since(last_idle_reclaim_check).as_secs()
                                >= config.idle_reclaim_global_cooldown_sec
                            {
                                last_idle_reclaim_check = now_inst;
                                let procs = monitor::process::list_processes();
                                let frozen_set: HashSet<u32> =
                                    frozen.lock().unwrap().frozen_pids().into_iter().collect();
                                let plan_procs: Vec<&Process> =
                                    procs.iter().filter(|p| !frozen_set.contains(&p.pid)).collect();
                                let active_pid = crate::plugin_server::get_active_foreground_pid();
                                check_idle_process_reclaim(
                                    &plan_procs, active_pid, &mut idle_reclaim_pid_tracker, &mut idle_freeze_pid_tracker, &frozen, &log,
                                );
                            }
                        }
                    }
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

        let now = std::time::Instant::now();
        let dt = now.duration_since(last_time).as_secs_f64();

        // Swap I/O rate: pswpin + pswpout delta over last cycle, pages → KB/s.
        // Normalized at 50 MB/s (51200 KB/s) total I/O — above that = thrash.
        let (cur_pswpin, cur_pswpout) = crate::monitor::meminfo::read_vmstat_swap_counters();
        let swap_io_pages = cur_pswpin.saturating_sub(last_pswpin)
            .saturating_add(cur_pswpout.saturating_sub(last_pswpout));
        last_pswpin = cur_pswpin;
        last_pswpout = cur_pswpout;
        let swap_io_kbs = if dt > 0.1 { (swap_io_pages * 4) as f64 / dt } else { 0.0 };
        let swap_io_val = (swap_io_kbs / 51200.0).clamp(0.0, 1.0);

        // Continuous pressure score: 55% PSI + 20% swap used + 15% GPU UMA + 10% swap I/O rate
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
        let p_score = 0.55 * psi_val + 0.20 * swap_val + 0.15 * gpu_val + 0.10 * swap_io_val;

        // Calculate trend (dP/dt)
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

        let swap_used_pct = meminfo.swap_used_pct();
        let swap_exhausted = meminfo.swap_total_kb > 0 && swap_used_pct >= 95.0;

        let prev_effective = effective_level.clone();
        let prev_sustained = sustained_critical_swap_start;
        (effective_level, sustained_critical_swap_start) = apply_swap_overrides(
            effective_level,
            swap_used_pct,
            meminfo.swap_total_kb,
            sustained_critical_swap_start,
            now,
        );
        if swap_exhausted && effective_level >= PressureLevel::Critical && prev_effective < PressureLevel::Critical {
            mgd_common::sync_print!(
                "[controller] Swap exhausted ({:.1}% used) — forcing effective pressure to CRITICAL to trigger eviction",
                swap_used_pct
            );
        }
        if effective_level >= PressureLevel::Emergency && prev_effective < PressureLevel::Emergency {
            if let Some(start) = sustained_critical_swap_start.or(prev_sustained) {
                let elapsed = now.duration_since(start).as_secs();
                mgd_common::sync_print!(
                    "[controller] Sustained Critical pressure with exhausted swap (>=98% used for {}s) — escalating to EMERGENCY to evict HIGH-tier candidates",
                    elapsed
                );
            } else {
                mgd_common::sync_print!(
                    "[controller] Escalating to EMERGENCY (composite pressure score threshold exceeded: score={:.2})",
                    p_score
                );
            }
        }

        last_level = effective_level.clone();

        // Hibernate last-resort: if Emergency sustained beyond threshold (disabled by default)
        if effective_level >= PressureLevel::Emergency {
            let start = sustained_emergency_start.get_or_insert(now);
            let threshold = crate::config::get().emergency_hibernate_after_sec;
            if !hibernate_triggered && threshold > 0 && now.duration_since(*start).as_secs() >= threshold {
                hibernate_triggered = true;
                mgd_common::sync_print!(
                    "[responder] CRITICAL: Emergency pressure sustained {}s — triggering systemctl hibernate",
                    threshold
                );
                let _ = std::process::Command::new("systemctl").arg("hibernate").spawn();
            }
        } else {
            sustained_emergency_start = None;
        }

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

            // Unfreeze idle-frozen process when it becomes the active window
            if active_pid_changed {
                if let Some(apid) = active_pid {
                    let reg = frozen.lock().unwrap();
                    if reg.is_frozen(apid) {
                        let st = reg.start_time(apid);
                        let name = reg.name(apid).to_string();
                        drop(reg);
                        let r = crate::executor::freezer::unfreeze_checked(apid, st);
                        if r.success {
                            frozen.lock().unwrap().remove(apid);
                            mgd_common::sync_print!(
                                "[idle-freeze] Unfroze {} (PID {}) on focus", name, apid
                            );
                        } else {
                            mgd_common::sync_print!(
                                "[idle-freeze] Unfreeze on focus failed for PID {} ({}): {:?}",
                                apid, name, r.error
                            );
                        }
                    }
                }
            }

            let procs = monitor::process::list_processes();
            let frozen_set: HashSet<u32> = frozen.lock().unwrap().frozen_pids().into_iter().collect();
            let plan_procs: Vec<&Process> = procs.iter()
                .filter(|p| !frozen_set.contains(&p.pid))
                .collect();

            // Update CPU throttling (tiered, debounced)
            throttle.update(&plan_procs, active_pid);
            *throttle_snapshot.lock().unwrap() = throttle.snapshot();

            // Cap memory.max on expendable background cgroups at High+ pressure
            memcap.update(&plan_procs, active_pid, &effective_level);

            // Idle cgroup reclaim: only runs at Normal/Calm pressure
            if effective_level == PressureLevel::Normal {
                if config.idle_reclaim_enabled {
                    check_idle_process_reclaim(&plan_procs, active_pid, &mut idle_reclaim_pid_tracker, &mut idle_freeze_pid_tracker, &frozen, &log);
                }
            }
        }

        // Restore memory caps when pressure drops below High
        if effective_level < PressureLevel::High {
            memcap.restore_all();
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

        // Cleanup expired recently killed cgroups (cooldown = 45s)
        let now_inst = std::time::Instant::now();
        recently_killed_cgroups.retain(|_, time| now_inst.duration_since(*time).as_secs() < 45);

        // Exclude frozen PIDs and recently killed cgroups so their RSS isn't double-counted toward the deficit.
        let frozen_set: HashSet<u32> = frozen.lock().unwrap().frozen_pids().into_iter().collect();
        let plan_procs: Vec<&Process> = procs.iter()
            .filter(|p| !frozen_set.contains(&p.pid))
            .filter(|p| {
                if let Some(ref cgroup_path) = p.cgroup_path {
                    if recently_killed_cgroups.contains_key(cgroup_path) {
                        return false; // Skip recently targeted cgroup
                    }
                }
                true
            })
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

                let result_str = execute_decision(d, &frozen, &checkpointed, &log, &event_log);
                mgd_common::sync_print!("  {:<10} {:<8} {:<22} {:>6.1}MB  {}", d.action, d.pid, d.name, d.rss_mb, result_str);

                // Track recently terminated/killed cgroups
                if d.action == Action::Kill || d.action == Action::Terminate {
                    let cgroup_path = plan_procs.iter()
                        .find(|p| p.pid == d.pid)
                        .and_then(|p| p.cgroup_path.clone())
                        .or_else(|| mgd_common::util::read_process_cgroup_path(d.pid));
                    if let Some(cgroup_path) = cgroup_path {
                        recently_killed_cgroups.insert(cgroup_path, std::time::Instant::now());
                    }
                }
            }
            // Ring the doorbell — recovery thread may have new work.
            let (lock, cvar) = &*recovery_wake;
            *lock.lock().unwrap() = true;
            cvar.notify_one();
        }
    }
}

/// Pure: given current effective pressure + swap stats + sustained-start timer,
/// returns the updated (effective_level, sustained_critical_swap_start).
/// No I/O, no logging — caller owns those responsibilities.
pub(crate) fn apply_swap_overrides(
    mut effective: PressureLevel,
    swap_used_pct: f64,
    swap_total_kb: u64,
    sustained_start: Option<std::time::Instant>,
    now: std::time::Instant,
) -> (PressureLevel, Option<std::time::Instant>) {
    let swap_exhausted = swap_total_kb > 0 && swap_used_pct >= 95.0;
    if swap_exhausted && effective < PressureLevel::Critical {
        effective = PressureLevel::Critical;
    }

    let swap_near_full = swap_total_kb > 0 && swap_used_pct >= 98.0;
    let new_sustained = if swap_near_full && effective >= PressureLevel::Critical {
        sustained_start.or(Some(now))
    } else if effective >= PressureLevel::Emergency {
        // Already at Emergency (score-driven or prior escalation) — preserve the timer so
        // a single cycle dip below 98% doesn't restart the 45 s window.
        sustained_start
    } else {
        None
    };

    if let Some(start) = new_sustained {
        let elapsed = now.duration_since(start).as_secs();
        if elapsed >= 45 && effective < PressureLevel::Emergency {
            effective = PressureLevel::Emergency;
        }
    }

    (effective, new_sustained)
}

pub(crate) struct IdleReclaimConfig {
    pub max_swap_occupancy_pct: f64,
    pub idle_sec: u64,
    pub rss_min_mb: u64,
    pub reclaim_pct: u64,
}

/// Pure: returns (pid, reclaim_bytes) pairs for processes eligible for idle cgroup reclaim.
/// Keyed by PID in background_tracker. No cgroup writes — caller handles I/O.
pub(crate) fn select_idle_candidates(
    procs: &[&crate::monitor::process::Process],
    active_pid: Option<u32>,
    background_tracker: &std::collections::HashMap<u32, std::time::Instant>,
    swap_used_pct: f64,
    cfg: &IdleReclaimConfig,
) -> Vec<(u32, u64)> {
    if swap_used_pct > cfg.max_swap_occupancy_pct {
        return vec![];
    }
    let mut candidates = vec![];
    for p in procs {
        if Some(p.pid) == active_pid {
            continue;
        }
        let prio = crate::engine::decision::get_priority(&p.name, p.exe_basename.as_deref());
        if prio < 50 {
            continue;
        }
        if p.rss_kb < cfg.rss_min_mb * 1024 {
            continue;
        }
        let duration = background_tracker
            .get(&p.pid)
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        if duration < cfg.idle_sec {
            continue;
        }
        let reclaim_bytes = (p.rss_kb * cfg.reclaim_pct / 100) * 1024;
        candidates.push((p.pid, reclaim_bytes));
    }
    candidates
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

    let now = unix_timestamp_secs();
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
    event_log: &crate::events::EventLog,
) -> String {
    let (action_name, s) = match d.action {
        Action::Freeze => ("FREEZE", freeze_process(d, frozen)),
        Action::Terminate => ("TERMINATE", terminate_process(d)),
        Action::Kill => ("KILL", kill_process(d)),
        Action::Checkpoint => ("CHECKPOINT", execute_checkpoint(d, checkpointed)),
        Action::None => return String::new(),
    };
    log.log(&LogEntry::new(action_name, d.pid, &d.name, d.rss_mb, &s));
    crate::events::push(event_log, action_name, d.pid, &d.name, &s);
    s
}

fn freeze_process(d: &Decision, frozen: &Arc<Mutex<FrozenRegistry>>) -> String {
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

fn terminate_process(d: &Decision) -> String {
    // SIGTERM→wait→SIGKILL blocks up to 5s; run it off-thread so the responder
    // isn't stalled per process at Critical.
    let pid = d.pid;
    std::thread::spawn(move || { crate::executor::killer::sigterm(pid); });
    "terminating (async SIGTERM→SIGKILL)".into()
}

fn kill_process(d: &Decision) -> String {
    let r = crate::executor::killer::sigkill(d.pid);

    match r.error {
        None => "killed".to_string(),
        Some(err) => format!("fail: {}", err),
    }
}

fn execute_checkpoint(d: &Decision, checkpointed: &Arc<Mutex<CheckpointRegistry>>) -> String {
    let r = crate::executor::checkpoint::checkpoint(d.pid, &d.name);
    if r.success {
        let dir = r.snapshot_dir.unwrap();
        checkpointed.lock().unwrap()
            .add(d.pid, &d.name, dir.clone(), d.rss_mb as u64 * 1024);
        format!("checkpointed → {dir:?}")
    } else {
        let kr = crate::executor::killer::sigkill(d.pid);
        if kr.success { "killed (CRIU failed)".into() }
        else { format!("kill_fail: {}", kr.error.unwrap_or_default()) }
    }
}

fn reclaim_cgroup(cgroup_path: &str, bytes_size: u64) -> Result<(), std::io::Error> {

    if bytes_size == 0 {
        return Ok(());
    }

    if cgroup_path == "/" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cannot reclaim root cgroup",
        ));
    }
    let reclaim_path = crate::throttle::cgroup_sysfs_path(cgroup_path, "memory.reclaim");
    if reclaim_path.exists() {
        std::fs::write(&reclaim_path, format!("{}", bytes_size))?;
        return Ok(());
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

    let now = unix_timestamp_secs();
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
        let reclaim_bytes_size = (p.rss_kb / 2) * 1024; // reclaim 50% of RSS
        let Some(cgroup) = p.cgroup_path.as_deref() else { continue };
        match reclaim_cgroup(cgroup, reclaim_bytes_size) {
            Ok(()) => {
                mgd_common::sync_print!(
                    "[reclaim] Proactively pushed ~{}MB of background PID {} ({}) to Zram",
                    reclaim_bytes_size / (1024 * 1024),
                    p.pid,
                    p.name
                );
                log.log(&LogEntry::new(
                    "EARLY_RECLAIM",
                    p.pid,
                    &p.name,
                    (reclaim_bytes_size / (1024 * 1024)) as f64,
                    "pushed to zram via cgroup reclaim",
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // EAGAIN: nothing reclaimable right now, skip silently.
            }
            Err(e) => {
                mgd_common::sync_print!(
                    "[reclaim] Early reclaim failed for PID {} ({}): {}",
                    p.pid, p.name, e
                );
            }
        }
    }
}

fn check_idle_process_reclaim(
    plan_procs: &[&Process],
    active_pid: Option<u32>,
    pid_tracker: &mut HashMap<u32, std::time::Instant>,
    freeze_pid_tracker: &mut HashMap<u32, std::time::Instant>,
    frozen: &Arc<Mutex<FrozenRegistry>>,
    log: &Logger,
) {
    let config = crate::config::get();
    let meminfo = crate::monitor::meminfo::read_meminfo();

    let swap_used_pct = if meminfo.swap_total_kb > 0 {
        let pct = meminfo.swap_used_pct();
        // Hard gate: less than 1.5 GB swap free is too risky to push more
        if meminfo.swap_free_kb / 1024 < 1500 {
            return;
        }
        pct
    } else {
        0.0
    };

    // Prune entries for processes no longer alive (shared by both reclaim and freeze trackers)
    let live_pids: HashSet<u32> = plan_procs.iter().map(|p| p.pid).collect();
    pid_tracker.retain(|pid, _| live_pids.contains(pid));

    // Delegate candidate selection to the pure helper
    let idle_cfg = IdleReclaimConfig {
        max_swap_occupancy_pct: config.idle_reclaim_max_swap_occupancy_pct,
        idle_sec: config.idle_reclaim_sec,
        rss_min_mb: config.idle_reclaim_rss_min_mb,
        reclaim_pct: config.idle_reclaim_pct,
    };
    let candidates = select_idle_candidates(plan_procs, active_pid, pid_tracker, swap_used_pct, &idle_cfg);

    // Start the background clock for all eligible processes not yet tracked
    for p in plan_procs {
        if Some(p.pid) != active_pid {
            pid_tracker.entry(p.pid).or_insert_with(std::time::Instant::now);
        }
    }

    // Execute: write to each candidate's cgroup memory.reclaim (cap at 3)
    for (i, (pid, bytes_to_reclaim_size)) in candidates.iter().enumerate() {
        if i >= 3 { break; }
        if *bytes_to_reclaim_size == 0 { continue; }

        let proc_entry = plan_procs.iter().find(|p| p.pid == *pid).copied();
        let name = proc_entry.map(|p| p.name.as_str()).unwrap_or("unknown");
        let cgroup = match proc_entry.and_then(|p| p.cgroup_path.as_deref()) {
            Some(c) => c,
            None => continue,
        };

        match reclaim_cgroup(cgroup, *bytes_to_reclaim_size) {
            Ok(()) => {
                mgd_common::sync_print!(
                    "[reclaim] Proactively reclaimed ~{}MB from idle background process {} (PID {})",
                    bytes_to_reclaim_size / (1024 * 1024),
                    name,
                    pid
                );
                if let Some(p) = proc_entry {
                    log.log(&LogEntry::new(
                        "EARLY_RECLAIM",
                        p.pid,
                        &p.name,
                        (*bytes_to_reclaim_size / (1024 * 1024)) as f64,
                        "proactively pushed idle process to zram",
                    ));
                }
                // Reset timer → serves as per-process cooldown
                pid_tracker.insert(*pid, std::time::Instant::now());
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // EAGAIN: kernel has nothing reclaimable right now — not an error.
                // Reset timer so we back off for a full idle_sec before retrying.
                pid_tracker.insert(*pid, std::time::Instant::now());
            }
            Err(e) => {
                mgd_common::sync_print!(
                    "[reclaim] Proactive idle reclaim failed for PID {} ({}): {}",
                    pid, name, e
                );
            }
        }
    }

    // Proactive idle freeze: SIGSTOP processes idle >= freeze_after_sec.
    // Uses a PID-keyed tracker so duplicate process names don't share timers.
    freeze_pid_tracker.retain(|pid, _| live_pids.contains(pid));

    if let Some(freeze_secs) = config.idle_reclaim_freeze_after_sec {
        let mut freeze_count = 0;
        for p in plan_procs {
            if freeze_count >= 2 { break; }
            if Some(p.pid) == active_pid { continue; }
            if p.rss_kb < config.idle_reclaim_rss_min_mb * 1024 { continue; }

            let elapsed = freeze_pid_tracker
                .entry(p.pid)
                .or_insert_with(std::time::Instant::now)
                .elapsed()
                .as_secs();
            if elapsed < freeze_secs { continue; }

            let st = match crate::executor::read_start_time(p.pid) {
                Some(t) => t,
                None => continue,
            };
            let r = crate::executor::freezer::freeze_checked(p.pid, st);
            if r.success {
                if frozen.lock().unwrap().add(p.pid, &p.name) {
                    mgd_common::sync_print!(
                        "[idle-freeze] Froze idle background process {} (PID {}, idle {}s)",
                        p.name, p.pid, elapsed
                    );
                    log.log(&LogEntry::new(
                        "IDLE_FREEZE",
                        p.pid,
                        &p.name,
                        p.rss_kb as f64 / 1024.0,
                        "proactively froze idle background process",
                    ));
                    freeze_pid_tracker.remove(&p.pid);
                    freeze_count += 1;
                } else {
                    crate::executor::freezer::unfreeze(p.pid);
                }
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::psi::PressureLevel;
    use crate::monitor::process::Process;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    fn make_process(pid: u32, name: &str, rss_kb: u64) -> Process {
        Process {
            pid,
            name: name.to_string(),
            exe_basename: None,
            rss_kb,
            swap_kb: 0,
            oom_score: 0,
            cgroup_path: None,
            cpu_pct: 0.0,
        }
    }

    fn default_idle_cfg() -> IdleReclaimConfig {
        IdleReclaimConfig { max_swap_occupancy_pct: 60.0, idle_sec: 180, rss_min_mb: 50, reclaim_pct: 20 }
    }

    fn make_pid_tracker(pid: u32, secs_ago: u64) -> HashMap<u32, Instant> {
        let mut m = HashMap::new();
        m.insert(pid, Instant::now() - Duration::from_secs(secs_ago));
        m
    }

    // ── apply_swap_overrides ─────────────────────────────────────────────────

    #[test]
    fn swap_below_95_no_override() {
        let now = Instant::now();
        let (level, sustained) = apply_swap_overrides(PressureLevel::Elevated, 94.9, 10_000_000, None, now);
        assert_eq!(level, PressureLevel::Elevated);
        assert!(sustained.is_none());
    }

    #[test]
    fn swap_95_forces_critical() {
        let now = Instant::now();
        let (level, _) = apply_swap_overrides(PressureLevel::Elevated, 95.0, 10_000_000, None, now);
        assert_eq!(level, PressureLevel::Critical);
    }

    #[test]
    fn swap_95_no_override_when_already_critical() {
        let now = Instant::now();
        let (level, _) = apply_swap_overrides(PressureLevel::Critical, 95.0, 10_000_000, None, now);
        assert_eq!(level, PressureLevel::Critical);
    }

    #[test]
    fn swap_no_device_no_override() {
        let now = Instant::now();
        let (level, _) = apply_swap_overrides(PressureLevel::Elevated, 99.0, 0, None, now);
        assert_eq!(level, PressureLevel::Elevated);
    }

    #[test]
    fn sustained_critical_swap_escalates_emergency() {
        let start = Instant::now() - Duration::from_secs(46);
        let now = Instant::now();
        let (level, _) = apply_swap_overrides(PressureLevel::Critical, 98.5, 10_000_000, Some(start), now);
        assert_eq!(level, PressureLevel::Emergency);
    }

    #[test]
    fn sustained_critical_swap_not_yet_45s() {
        let start = Instant::now() - Duration::from_secs(44);
        let now = Instant::now();
        let (level, _) = apply_swap_overrides(PressureLevel::Critical, 98.5, 10_000_000, Some(start), now);
        assert_eq!(level, PressureLevel::Critical);
    }

    #[test]
    fn sustained_critical_swap_resets_when_swap_drops() {
        let start = Instant::now() - Duration::from_secs(50);
        let now = Instant::now();
        let (_, sustained) = apply_swap_overrides(PressureLevel::Critical, 97.9, 10_000_000, Some(start), now);
        assert!(sustained.is_none(), "timer must reset when swap < 98%");
    }

    #[test]
    fn swap_98_starts_sustained_timer() {
        let now = Instant::now();
        let (_, sustained) = apply_swap_overrides(PressureLevel::Critical, 98.0, 10_000_000, None, now);
        assert!(sustained.is_some(), "timer must start at >=98% swap + Critical");
    }

    #[test]
    fn swap_98_preserves_existing_timer() {
        let start = Instant::now() - Duration::from_secs(10);
        let now = Instant::now();
        let (_, sustained) = apply_swap_overrides(PressureLevel::Critical, 98.0, 10_000_000, Some(start), now);
        assert!(sustained.unwrap().elapsed().as_secs() >= 10, "existing timer must not be reset");
    }

    // ── select_idle_candidates ───────────────────────────────────────────────

    #[test]
    fn idle_reclaim_skips_foreground_pid() {
        let p = make_process(1234, "firefox", 200_000);
        let r = select_idle_candidates(&[&p], Some(1234), &make_pid_tracker(1234, 300), 10.0, &default_idle_cfg());
        assert!(r.is_empty());
    }

    #[test]
    fn idle_reclaim_skips_rss_below_minimum() {
        let p = make_process(5678, "app", 40 * 1024); // 40 MB < 50 MB min
        let r = select_idle_candidates(&[&p], None, &make_pid_tracker(5678, 300), 10.0, &default_idle_cfg());
        assert!(r.is_empty());
    }

    #[test]
    fn idle_reclaim_skips_swap_saturated() {
        let p = make_process(5678, "app", 200_000);
        let r = select_idle_candidates(&[&p], None, &make_pid_tracker(5678, 300), 61.0, &default_idle_cfg());
        assert!(r.is_empty());
    }

    #[test]
    fn idle_reclaim_skips_not_yet_idle() {
        let p = make_process(5678, "app", 200_000);
        let r = select_idle_candidates(&[&p], None, &make_pid_tracker(5678, 100), 10.0, &default_idle_cfg());
        assert!(r.is_empty());
    }

    #[test]
    fn idle_reclaim_skips_not_in_tracker() {
        let p = make_process(5678, "app", 200_000);
        let r = select_idle_candidates(&[&p], None, &HashMap::<u32, Instant>::new(), 10.0, &default_idle_cfg());
        assert!(r.is_empty());
    }

    #[test]
    fn idle_reclaim_selects_eligible() {
        let p = make_process(5678, "app", 200_000);
        let r = select_idle_candidates(&[&p], None, &make_pid_tracker(5678, 300), 10.0, &default_idle_cfg());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, 5678);
        assert_eq!(r[0].1, 40_000 * 1024); // 20% of 200_000 KB * 1024
    }

    #[test]
    fn idle_reclaim_selects_multiple() {
        let p1 = make_process(100, "app_a", 100_000);
        let p2 = make_process(200, "app_b", 150_000);
        let mut tracker = make_pid_tracker(100, 300);
        tracker.insert(200, Instant::now() - Duration::from_secs(250));
        let r = select_idle_candidates(&[&p1, &p2], None, &tracker, 10.0, &IdleReclaimConfig { max_swap_occupancy_pct: 60.0, idle_sec: 180, rss_min_mb: 50, reclaim_pct: 10 });
        assert_eq!(r.len(), 2);
        let pids: Vec<u32> = r.iter().map(|(pid, _)| *pid).collect();
        assert!(pids.contains(&100));
        assert!(pids.contains(&200));
    }

    // ── ThrottledState ───────────────────────────────────────────────────────

    #[test]
    fn throttle_state_eq() {
        use crate::throttle::ThrottledState;
        assert_eq!(ThrottledState::None, ThrottledState::None);
        assert_ne!(ThrottledState::None, ThrottledState::WeightOnly);
        assert_ne!(ThrottledState::WeightOnly, ThrottledState::Full);
    }
}
