mod scoring;
mod spike;
mod leak;
mod reclaim_primitive;
mod idle_reclaim;
mod early_reclaim;
mod compact_zram;
mod cache_drop;
mod execute;
mod gates;

pub(crate) use gates::feature_gates;
pub(crate) use reclaim_primitive::reclaim_cgroup;

use std::collections::{HashSet, HashMap};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use mgd_common::types::{Kb, Pid};

use crate::config::CompiledConfig;
use crate::engine::decision::{plan, get_priority};
use crate::executor::registry::{FrozenRegistry, CheckpointRegistry};
use mgd_common::logger::{LogAction, Logger};
use crate::monitor;
use crate::monitor::meminfo::MemInfo;
use crate::monitor::process::Process;
use crate::monitor::psi::PressureLevel;

use scoring::{ScoreTracker, StateMachine};
use idle_reclaim::idle_timeout_reclaim;

/// Frozen + spike-victim PIDs (optionally + the spikes themselves) to exclude from
/// `plan()`/candidate lists — their RSS is already accounted for or off-limits.
fn excluded_pids(
    frozen: &Arc<Mutex<FrozenRegistry>>,
    spike_tracker: &crate::spike_mode::SpikeTracker,
    include_spike_pids: bool,
) -> HashSet<Pid> {
    let base = frozen.lock().unwrap().frozen_pids().into_iter()
        .chain(spike_tracker.victim_pids());
    if include_spike_pids {
        base.chain(spike_tracker.spike_pids()).collect()
    } else {
        base.collect()
    }
}

#[allow(clippy::too_many_arguments)]
fn try_elevate_scheduler_priority() {
    use mgd_common::output::locked_print;
    unsafe {
        let param = libc::sched_param { sched_priority: 20 };
        // Set policy to SCHED_RR (Real-Time Round Robin) with priority 20
        if libc::sched_setscheduler(0, libc::SCHED_RR, &param) == 0 {
            locked_print("[responder] Evictor thread set to SCHED_RR (priority 20)");
        } else {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EPERM) {
                // If unprivileged and CAP_SYS_NICE is missing, fall back to setting highest normal priority (nice -20)
                if libc::setpriority(libc::PRIO_PROCESS, 0, -20) == 0 {
                    locked_print("[responder] Set scheduler priority to nice -20 (highest normal priority)");
                } else {
                    locked_print("[responder] Running with standard priority (CAP_SYS_NICE missing for RT/Nice elevation)");
                }
            } else {
                mgd_common::sync_print!("[responder] Warning: failed to set scheduler policy: {}", err);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    frozen: Arc<Mutex<FrozenRegistry>>,
    checkpointed: Arc<Mutex<CheckpointRegistry>>,
    log: Arc<Logger>,
    recovery_wake: Arc<(Mutex<bool>, Condvar)>,
    reclaim_wake: Arc<(Mutex<bool>, Condvar)>,
    calibrator: Arc<Mutex<crate::engine::calibrate::Calibrator>>,
    throttle_snapshot: Arc<Mutex<HashMap<String, crate::throttle::ThrottledState>>>,
    event_log: crate::events::EventLog,
    spike_snapshot: Arc<Mutex<crate::spike_mode::SpikeSnapshot>>,
    leak_snapshot: Arc<Mutex<crate::leak_guard::LeakSnapshot>>,
) {

    try_elevate_scheduler_priority();

    mgd_common::sync_print!("[responder] PSI source: {}", monitor::psi::pressure_source());

    let mut psi_elevated_pct = crate::config::get().psi.elevated_pct;
    let mut psi_subprocess = monitor::psi::PsiSubprocess::new(psi_elevated_pct);
    let mut psi_trigger = if psi_subprocess.is_none() {
        monitor::psi::PsiTrigger::new(psi_elevated_pct).ok()
    } else {
        None
    };
    if psi_subprocess.is_some() {
        mgd_common::sync_print!("[responder] PSI kernel trigger armed via mgd-psi-trigger (zero-CPU idle).");
    } else if let Some(t) = &psi_trigger {
        mgd_common::sync_print!("[responder] PSI epoll trigger registered on {} (zero-CPU idle).", t.source);
    } else {
        mgd_common::sync_print!("[responder] PSI kernel trigger unavailable (mgd-psi-trigger not found or cap_perfmon absent; cgroup file not writable) — 5s polling.");
    }

    let mut last_level = PressureLevel::Normal;
    let mut score_tracker = ScoreTracker::new();
    let mut state_machine = StateMachine::new();
    let mut throttle = crate::throttle::ThrottleManager::new();
    let mut idle_reclaim_pid_tracker: HashMap<Pid, std::time::Instant> = HashMap::new();
    let mut idle_freeze_pid_tracker: HashMap<Pid, std::time::Instant> = HashMap::new();
    let mut last_idle_reclaim_check = std::time::Instant::now();
    let mut last_active_pid = None;
    let mut recently_killed_cgroups: HashMap<String, std::time::Instant> = HashMap::new();
    let mut sustained_critical_swap_start: Option<std::time::Instant> = None;
    let mut memcap = crate::throttle::MemCapManager::new();
    let mut sustained_emergency_start: Option<std::time::Instant> = None;
    let mut hibernate_triggered = false;
    let mut spike_tracker = crate::spike_mode::SpikeTracker::new();
    let mut leak_tracker = crate::leak_guard::LeakTracker::new();

    loop {
        if crate::lifecycle::should_shutdown() {
            throttle.restore_all();
            memcap.restore_all();
            for cg in spike_tracker.throttled_cgroup_paths() {
                let _ = crate::throttle::write_cgroup_cpu_weight(&cg, 100);
            }
            for v in spike_tracker.all_victims() {
                let _ = crate::executor::freezer::unfreeze_checked(v.pid, v.start_time);
            }
            // Wake maintenance so it exits immediately instead of blocking up to 60s.
            let (lock, cvar) = &*reclaim_wake;
            if let Ok(_g) = lock.lock() { cvar.notify_all(); }
            return;
        }

        if crate::lifecycle::should_reload() {
            crate::config::reload();
            crate::plugin_server::broadcast_config_reload();
        }


        let cfg = crate::config::get();


        if (cfg.psi.elevated_pct - psi_elevated_pct).abs() > 0.001 {
            psi_elevated_pct = cfg.psi.elevated_pct;
            psi_subprocess = monitor::psi::PsiSubprocess::new(psi_elevated_pct);
            if psi_subprocess.is_some() {
                mgd_common::sync_print!("[responder] PSI trigger respawned at elevated_pct={:.1}%.", psi_elevated_pct);
            }
        }

        if last_level == PressureLevel::Normal {
            let helper_died = if let Some(sub) = &psi_subprocess {
                match sub.wait(5000) {
                    monitor::psi::WaitResult::Event => false,
                    monitor::psi::WaitResult::Timeout => {
                        idle_timeout_reclaim(&cfg, &frozen, &spike_tracker, &log,
                            &mut idle_reclaim_pid_tracker, &mut idle_freeze_pid_tracker,
                            &mut last_idle_reclaim_check);
                        continue; // no pressure event → skip full cycle
                    }
                    monitor::psi::WaitResult::HelperDied => true,
                }
            } else if let Some(trigger) = &psi_trigger {
                if !trigger.wait(5000) {
                    idle_timeout_reclaim(&cfg, &frozen, &spike_tracker, &log,
                        &mut idle_reclaim_pid_tracker, &mut idle_freeze_pid_tracker,
                        &mut last_idle_reclaim_check);
                    continue;
                }
                false
            } else {
                thread::sleep(Duration::from_secs(5));
                false
            };
            if helper_died {

                if crate::lifecycle::should_shutdown() {
                    continue;
                }
                mgd_common::sync_print!("[psi] mgd-psi-trigger exited — attempting respawn");
                psi_subprocess = monitor::psi::PsiSubprocess::new(psi_elevated_pct);
                if psi_subprocess.is_none() && psi_trigger.is_none() {
                    psi_trigger = monitor::psi::PsiTrigger::new(psi_elevated_pct).ok();
                    if let Some(t) = &psi_trigger {
                        mgd_common::sync_print!("[psi] subprocess respawn failed — fell back to epoll trigger on {}", t.source);
                    }
                }
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
        let score = score_tracker.update(pressure.some_avg10, &meminfo, now);

        let target_state = scoring::target_state_for(score.p_score, score.trend, pressure.some_avg10, score.swap_io_kbs);
        if state_machine.advance(target_state, score.trend) {
            mgd_common::sync_print!("[controller] Instant escalation triggered due to rapid pressure spike (trend: {:.3})", score.trend);
        }

        let mut effective_level = state_machine.current.to_pressure_level();

        let swap_used_pct = meminfo.swap_used_pct();

        let swap_exhausted = meminfo.swap_total_kb.0 > 0 && swap_used_pct >= 95.0;

        let prev_effective = effective_level;
        let prev_sustained = sustained_critical_swap_start;
        (effective_level, sustained_critical_swap_start) = scoring::apply_swap_overrides(
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
                    score.p_score
                );
            }
        }

        last_level = effective_level;

        // Hibernate last-resort: if Emergency sustained beyond threshold (disabled by default)
        if effective_level >= PressureLevel::Emergency {
            let start = sustained_emergency_start.get_or_insert(now);
            let threshold = cfg.emergency_hibernate_after_sec;
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


        let mut procs = monitor::process::list_processes();

        // Background CPU Throttling and Idle cgroup reclaim manager
        let active_pid = crate::plugin_server::get_active_foreground_pid();
        let now_inst = std::time::Instant::now();
        let idle_reclaim_interval_elapsed = now_inst.duration_since(last_idle_reclaim_check).as_secs() >= cfg.idle_reclaim_global_cooldown_sec;
        let active_pid_changed = active_pid != last_active_pid;

        if active_pid_changed || idle_reclaim_interval_elapsed {
            last_active_pid = active_pid;
            if idle_reclaim_interval_elapsed {
                last_idle_reclaim_check = now_inst;
            }

            // Unfreeze idle-frozen process when it becomes the active window
            if active_pid_changed
                && let Some(apid) = active_pid {
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

            let frozen_set = excluded_pids(&frozen, &spike_tracker, false);
            let plan_procs: Vec<&Process> = procs.iter()
                .filter(|p| !frozen_set.contains(&p.pid))
                .collect();

            throttle.update(&plan_procs, active_pid, effective_level >= PressureLevel::Elevated, pressure.some_avg10, &cfg);
            *throttle_snapshot.lock().unwrap() = throttle.snapshot();

            memcap.update(&plan_procs, active_pid, &effective_level, &cfg);

            if effective_level == PressureLevel::Normal
                && cfg.idle_reclaim_enabled {
                    idle_reclaim::check_idle_process_reclaim(&cfg, &plan_procs, active_pid, &mut idle_reclaim_pid_tracker, &mut idle_freeze_pid_tracker, &frozen, &log);
                }
        }

        if effective_level < PressureLevel::High {
            memcap.restore_all();
        }


        spike::run_spike_cycle(&cfg, &mut spike_tracker, &frozen, &log, meminfo.available_kb, &spike_snapshot, &procs);


        leak::run_leak_guard_cycle(&cfg, &mut leak_tracker, &log, &event_log, &leak_snapshot, &procs);

        if effective_level == PressureLevel::Normal {
            continue;
        }

        procs.sort_by_key(|p| std::cmp::Reverse(p.rss_kb));

        print_status(&pressure, &effective_level, &procs, &meminfo, &frozen, &cfg);

        crate::plugin_server::broadcast_pressure(effective_level.as_str());

        compact_zram::compact_zram(&effective_level, &log, &cfg);

        let now_inst = std::time::Instant::now();
        recently_killed_cgroups.retain(|_, time| now_inst.duration_since(*time).as_secs() < 45);


        let frozen_set = excluded_pids(&frozen, &spike_tracker, true);
        let plan_procs: Vec<&Process> = procs.iter()
            .filter(|p| !frozen_set.contains(&p.pid))
            .filter(|p| {
                if let Some(ref cgroup_path) = p.cgroup_path
                    && recently_killed_cgroups.contains_key(cgroup_path) {
                        return false; // Skip recently targeted cgroup
                    }
                true
            })
            .collect();

        let active_pid = crate::plugin_server::get_active_foreground_pid();
        early_reclaim::check_early_process_reclaim(&effective_level, &plan_procs, active_pid, &log, &cfg);
        cache_drop::check_cache_drop(&effective_level, &log, &cfg);

        let decisions = plan(&effective_level, &plan_procs, meminfo.available_kb, meminfo.total_kb, swap_exhausted, &cfg);
        if decisions.is_empty() {
            mgd_common::sync_print!("✓ No action needed.");
        } else {
            let destructive_count = execute::execute_plan(
                &decisions, &plan_procs, &frozen, &checkpointed,
                &log, &event_log, &mut recently_killed_cgroups,
                &mut crate::executor::RealSink,
            );
            // Ring the doorbell — recovery thread may have new work.
            let (lock, cvar) = &*recovery_wake;
            *lock.lock().unwrap() = true;
            cvar.notify_one();

            if destructive_count > 0 {
                let (lock, cvar) = &*reclaim_wake;
                *lock.lock().unwrap() = true;
                cvar.notify_one();
            }
        }


        {
            let frozen_n = frozen.lock().unwrap().count();
            let throttled_n = throttle_snapshot.lock().unwrap()
                .values()
                .filter(|s| **s != crate::throttle::ThrottledState::None)
                .count();
            let attribution = format!(
                "psi={:.1}% swap={:.0}% gpu={:.1}% swap_io={:.0}KB/s score={:.2} trend={:+.3} state={:?} level={} | frozen={} throttled={} memcap={} spike={}/{} decisions={}",
                pressure.some_avg10, swap_used_pct, score.gpu_val * 100.0, score.swap_io_kbs,
                score.p_score, score.trend, state_machine.current, effective_level,
                frozen_n, throttled_n, memcap.capped_count(),
                spike_tracker.spike_pids().len(), spike_tracker.victim_pids().len(),
                decisions.len(),
            );
            mgd_common::sync_print!("[cycle] {attribution}");
            log.log_system(LogAction::Cycle, "cycle",
                           meminfo.available_kb.mib(), &attribution);
        }
    }
}

fn print_status(
    pressure: &monitor::psi::MemoryPressure,
    effective_level: &PressureLevel,
    procs: &[monitor::process::Process],
    meminfo: &MemInfo,
    frozen: &Arc<Mutex<FrozenRegistry>>,
    cfg: &CompiledConfig,
) {
    let (total_rss, _) = procs.iter()
        .fold((Kb(0), Kb(0)), |(r, s), p| (r.saturating_add(p.rss_kb), s.saturating_add(p.swap_kb)));
    let frozen_count = frozen.lock().unwrap().count();

    mgd_common::sync_print!(
        "\n[responder] [{effective_level}] some avg10={:.2}% | RAM {:.0}/{:.0}MB | Swap {:.0}% | Avail {:.0}MB | Procs {} | Frozen {}",
        pressure.some_avg10,
        total_rss.mib(),
        meminfo.total_kb.mib(),
        meminfo.swap_used_pct(),
        meminfo.available_kb.mib(),
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
            p.rss_kb.mib(),
            p.swap_kb.mib(),
            p.oom_score,
            get_priority(&p.name, p.exe_basename.as_deref(), cfg),
            marker,
        );
    }
}

#[cfg(test)]
mod tests {
    // ── ThrottledState ───────────────────────────────────────────────────────

    #[test]
    fn throttle_state_eq() {
        use crate::throttle::ThrottledState;
        assert_eq!(ThrottledState::None, ThrottledState::None);
        assert_ne!(ThrottledState::None, ThrottledState::WeightOnly);
        assert_ne!(ThrottledState::WeightOnly, ThrottledState::Full);
    }
}
