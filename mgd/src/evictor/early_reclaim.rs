use std::sync::atomic::{AtomicU64, Ordering};
use mgd_common::types::{Kb, Pid};
use mgd_common::util::unix_timestamp_secs;
use mgd_common::logger::{LogAction, Logger};

use crate::config::CompiledConfig;
use crate::engine::decision::get_priority;
use crate::monitor::process::Process;
use crate::monitor::psi::PressureLevel;

use super::reclaim_primitive::{try_reclaim_cgroup, ReclaimOutcome};

/// Unix-seconds of the last early background process reclaim.
static LAST_EARLY_RECLAIM: AtomicU64 = AtomicU64::new(0);

pub(super) fn gate_state() -> u64 {
    LAST_EARLY_RECLAIM.load(Ordering::Relaxed)
}

pub(crate) fn check_early_process_reclaim(
    level: &PressureLevel,
    plan_procs: &[&Process],
    active_pid: Option<Pid>,
    log: &Logger,
    cfg: &CompiledConfig,
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


    let mut targets: Vec<&Process> = plan_procs
        .iter()
        .filter(|p| {
            p.rss_kb > Kb(20_000)
                && Some(p.pid) != active_pid
                && {
                    let prio = get_priority(&p.name, p.exe_basename.as_deref(), cfg);
                    (50..60).contains(&prio)
                }
        })
        .copied()
        .collect();

    // Sort by RSS descending to target the largest background processes first
    targets.sort_by_key(|p| std::cmp::Reverse(p.rss_kb));

    for p in targets.iter().take(3) {
        let reclaim_kb = p.rss_kb.percent_of(50); // reclaim 50% of RSS
        let Some(cgroup) = p.cgroup_path.as_deref() else { continue };
        match try_reclaim_cgroup(cgroup, reclaim_kb.bytes()) {
            ReclaimOutcome::Reclaimed => {
                mgd_common::sync_print!(
                    "[reclaim] Proactively pushed ~{:.0}MB of background PID {} ({}) to Zram",
                    reclaim_kb.mib(),
                    p.pid,
                    p.name
                );
                log.log(LogAction::EarlyReclaim, p.pid, &p.name,
                    reclaim_kb.mib(), "pushed to zram via cgroup reclaim");
            }
            ReclaimOutcome::Skipped | ReclaimOutcome::Blocked => {
                // Blocked = EAGAIN, nothing reclaimable right now — skip silently.
            }
            ReclaimOutcome::Failed(e) => {
                mgd_common::sync_print!(
                    "[reclaim] Early reclaim failed for PID {} ({}): {}",
                    p.pid, p.name, e
                );
            }
        }
    }
}
