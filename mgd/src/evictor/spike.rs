use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use mgd_common::types::{Kb, Pid};
use mgd_common::logger::{LogAction, Logger};

use crate::config::CompiledConfig;
use crate::engine::decision::get_priority;
use crate::executor::registry::FrozenRegistry;
use crate::monitor::process::Process;

/// Release victims (spike exited / timed out / orphaned / manually requested via
/// `mgctl unfreeze`). Called both from the full cycle below and from
/// `idle_timeout_reclaim`'s calm-tick path — release must not be gated behind a
/// pressure event, or victims of a spike process that never exits (e.g. a browser
/// tab) stay frozen forever once pressure drops back to Normal.
pub(crate) fn release_victims(
    cfg: &CompiledConfig,
    spike_tracker: &mut crate::spike_mode::SpikeTracker,
    log: &Logger,
    spike_procs: &[Process],
    manual_release: &[Pid],
) {
    let live_pids: HashSet<Pid> = spike_procs.iter().map(|p| p.pid).collect();

    // Unfreeze victims when their spike process exits
    let exited: Vec<Pid> = spike_tracker.spike_pids()
        .into_iter().filter(|pid| !live_pids.contains(pid)).collect();
    let mut total_released = 0usize;
    let mut last_spike_name: Option<String> = None;
    for spike_pid in exited {
        let victims = spike_tracker.on_spike_exit(spike_pid);
        // on_spike_exit returns names before draining; capture name from first victim
        if let Some(v) = victims.first() {
            last_spike_name = spike_procs.iter()
                .find(|p| p.pid == v.frozen_for_spike_pid)
                .map(|p| p.name.clone());
        }
        for v in victims {
            let r = crate::executor::freezer::unfreeze_checked(v.pid, v.start_time);
            if r.success {
                mgd_common::sync_print!(
                    "[spike] Unfroze {} (PID {}) — spike PID {} exited",
                    v.name, v.pid, v.frozen_for_spike_pid
                );
                log.log(LogAction::SpikeUnfreeze, v.pid, &v.name, 0.0, "spike exited");
                total_released += 1;
            }
        }
    }
    if total_released > 0 {
        let spike_name = last_spike_name.as_deref().unwrap_or("build");
        let msg = format!("Build session ended — {} process{} resumed",
            total_released, if total_released == 1 { "" } else { "es" });
        let _ = std::process::Command::new("notify-send")
            .args(["--urgency=low", "--app-name=mgd", spike_name, &msg])
            .spawn();
    }

    // Release victims that have been frozen beyond max_victim_freeze_sec
    let max_secs = cfg.spike_max_victim_freeze_sec;
    for v in spike_tracker.drain_timed_out_victims(max_secs) {
        let r = crate::executor::freezer::unfreeze_checked(v.pid, v.start_time);
        if r.success {
            mgd_common::sync_print!(
                "[spike] Released timed-out victim {} (PID {}): frozen >{}s",
                v.name, v.pid, max_secs
            );
            log.log(LogAction::SpikeUnfreezeTimeout, v.pid, &v.name, 0.0, "max_victim_freeze_sec");
        }
    }

    // Release victims whose initiator spike already exited but were deferred
    // (frozen_for_spike_pid no longer in the active spike set).
    for v in spike_tracker.drain_orphaned_victims() {
        let r = crate::executor::freezer::unfreeze_checked(v.pid, v.start_time);
        if r.success {
            mgd_common::sync_print!(
                "[spike] Released orphaned victim {} (PID {}): spike PID {} already gone",
                v.name, v.pid, v.frozen_for_spike_pid
            );
            log.log(LogAction::SpikeUnfreezeOrphan, v.pid, &v.name, 0.0, "initiator exited");
        }
    }

    // Release victims manually requested via `mgctl unfreeze` (queued by the IPC
    // thread — it can't mutate spike_tracker directly, since only this thread owns it)
    if !manual_release.is_empty() {
        for v in spike_tracker.release_requested(manual_release) {
            let r = crate::executor::freezer::unfreeze_checked(v.pid, v.start_time);
            if r.success {
                mgd_common::sync_print!(
                    "[spike] Released victim {} (PID {}) on manual request",
                    v.name, v.pid
                );
                log.log(LogAction::SpikeUnfreezeManual, v.pid, &v.name, 0.0, "mgctl unfreeze");
            }
        }
    }
}

/// Spike-mode cycle: release victims, feed the tracker, and execute its decisions.
/// Runs on every full cycle (a PSI event fired). `release_victims` above also runs
/// standalone on PSI-timeout "calm" ticks via `idle_timeout_reclaim`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_spike_cycle(
    cfg: &CompiledConfig,
    spike_tracker: &mut crate::spike_mode::SpikeTracker,
    frozen: &Arc<Mutex<FrozenRegistry>>,
    log: &Logger,
    available: Kb,
    spike_snapshot: &Arc<Mutex<crate::spike_mode::SpikeSnapshot>>,
    spike_procs: &[Process],
    manual_release: &[Pid],
) {
    release_victims(cfg, spike_tracker, log, spike_procs, manual_release);

    // Update tracker and execute decisions
    let spike_decisions = if cfg.spike_mode_enabled {
        spike_tracker.update(spike_procs, available, &crate::spike_mode::Params::from_config(cfg))
    } else {
        vec![]
    };
    for decision in spike_decisions {
        match decision {
            crate::spike_mode::SpikeDecision::FreezeForHeadroom { needed } => {
                let spike_pids = spike_tracker.spike_pids();
                let exclude = super::excluded_pids(frozen, spike_tracker, true);
                // Highest-priority (most expendable) first, then largest RSS
                let mut candidates: Vec<&Process> = spike_procs.iter()
                    .filter(|p| !exclude.contains(&p.pid))
                    .filter(|p| get_priority(&p.name, p.exe_basename.as_deref(), cfg) >= 60)
                    .filter(|p| !cfg.spike_victim_exclude.iter().any(|re| re.is_match(&p.name)))
                    .collect();
                candidates.sort_by(|a, b| {
                    let pa = get_priority(&a.name, a.exe_basename.as_deref(), cfg);
                    let pb = get_priority(&b.name, b.exe_basename.as_deref(), cfg);
                    pb.cmp(&pa).then(b.rss_kb.cmp(&a.rss_kb))
                });
                let mut needed = needed;
                for proc in candidates {
                    if needed.0 == 0 { break; }
                    let Some(st) = crate::executor::read_start_time(proc.pid) else { continue };
                    let r = crate::executor::freezer::freeze_checked(proc.pid, st);
                    if r.success {
                        mgd_common::sync_print!(
                            "[spike] Froze {} (PID {}, {:.0}MB) for headroom",
                            proc.name, proc.pid, proc.rss_kb.mib()
                        );
                        log.log(LogAction::SpikeFreeze, proc.pid, &proc.name,
                                proc.rss_kb.mib(), "proactive headroom");
                        spike_tracker.record_victim_frozen(crate::spike_mode::SpikeVictim {
                            pid: proc.pid,
                            name: proc.name.clone(),
                            start_time: st,
                            // unwrap_or(Pid::NONE) is unreachable in practice: this arm only
                            // runs for FreezeForHeadroom, which spike_mode only emits when
                            // sum_rss_max > 0, i.e. at least one Tracking-phase spike exists
                            // — so spike_pids is never empty here.
                            frozen_for_spike_pid: spike_pids.iter().next().copied().unwrap_or(Pid::NONE),
                            frozen_at: std::time::Instant::now(),
                        });
                        // Push victim RSS to zram; SIGSTOP means no re-faults so 100% is safe.
                        if let Some(cg) = proc.cgroup_path.as_deref() {
                            let _ = super::reclaim_primitive::reclaim_cgroup(cg, proc.rss_kb.bytes());
                        }
                        needed = needed.saturating_sub(proc.rss_kb);
                    }
                }
            }
            crate::spike_mode::SpikeDecision::ThrottleSpike { spike_pid, cgroup_path } => {
                let weight = cfg.spike_throttled_cpu_weight;
                let _ = crate::throttle::write_cgroup_cpu_weight(&cgroup_path, weight);
                mgd_common::sync_print!("[spike] Throttled PID {} cpu.weight={}", spike_pid, weight);
            }
            crate::spike_mode::SpikeDecision::RestoreThrottle { spike_pid, cgroup_path } => {
                let _ = crate::throttle::write_cgroup_cpu_weight(&cgroup_path, 100);
                mgd_common::sync_print!("[spike] Restored cpu.weight for PID {}", spike_pid);
            }
        }
    }
    *spike_snapshot.lock().unwrap() = spike_tracker.snapshot();
}
