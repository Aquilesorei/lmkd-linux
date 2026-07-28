use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use mgd_common::types::{Kb, Pid};
use mgd_common::util::unix_timestamp_secs;
use mgd_common::logger::{LogAction, Logger};

use crate::config::CompiledConfig;
use crate::executor::registry::FrozenRegistry;
use crate::monitor;
use crate::monitor::process::Process;

use super::reclaim_primitive::{try_reclaim_cgroup, ReclaimOutcome};

/// Unix-seconds of the last successful idle cgroup reclaim (0 = never).
static LAST_IDLE_RECLAIM: AtomicU64 = AtomicU64::new(0);

pub(super) fn gate_state() -> u64 {
    LAST_IDLE_RECLAIM.load(Ordering::Relaxed)
}

/// Idle-cycle work on PSI trigger timeout: no pressure event fired, so run idle
/// cgroup reclaim on its cooldown. Shared by the subprocess and direct-trigger
/// wait paths; the caller `continue`s afterwards to skip the full cycle.
pub(crate) fn idle_timeout_reclaim(
    cfg: &CompiledConfig,
    frozen: &Arc<Mutex<FrozenRegistry>>,
    spike_tracker: &crate::spike_mode::SpikeTracker,
    log: &Logger,
    idle_reclaim_pid_tracker: &mut HashMap<Pid, std::time::Instant>,
    idle_freeze_pid_tracker: &mut HashMap<Pid, std::time::Instant>,
    last_idle_reclaim_check: &mut std::time::Instant,
) {
    if !cfg.idle_reclaim_enabled {
        return;
    }
    let now_inst = std::time::Instant::now();
    if now_inst.duration_since(*last_idle_reclaim_check).as_secs()
        < cfg.idle_reclaim_global_cooldown_sec
    {
        return;
    }
    *last_idle_reclaim_check = now_inst;
    let procs = monitor::process::list_processes();
    let frozen_set = super::excluded_pids(frozen, spike_tracker, false);
    let plan_procs: Vec<&Process> =
        procs.iter().filter(|p| !frozen_set.contains(&p.pid)).collect();
    let active_pid = crate::plugin_server::get_active_foreground_pid();
    check_idle_process_reclaim(
        cfg, &plan_procs, active_pid,
        idle_reclaim_pid_tracker, idle_freeze_pid_tracker,
        frozen, log,
    );
}

pub(crate) fn check_idle_process_reclaim(
    cfg: &CompiledConfig,
    plan_procs: &[&Process],
    active_pid: Option<Pid>,
    pid_tracker: &mut HashMap<Pid, std::time::Instant>,
    freeze_pid_tracker: &mut HashMap<Pid, std::time::Instant>,
    frozen: &Arc<Mutex<FrozenRegistry>>,
    log: &Logger,
) {
    let meminfo = crate::monitor::meminfo::read_meminfo();

    let swap_used_pct = if meminfo.swap_total_kb.0 > 0 {
        let pct = meminfo.swap_used_pct();
        // Hard gate: less than 1.5 GB swap free is too risky to push more
        if meminfo.swap_free_kb.0 / 1024 < 1500 {
            return;
        }
        pct
    } else {
        0.0
    };

    // Prune entries for processes no longer alive (shared by both reclaim and freeze trackers)
    let live_pids: HashSet<Pid> = plan_procs.iter().map(|p| p.pid).collect();
    pid_tracker.retain(|pid, _| live_pids.contains(pid));

    // Delegate candidate selection to the pure helper
    let idle_cfg = IdleReclaimConfig {
        max_swap_occupancy_pct: cfg.idle_reclaim_max_swap_occupancy_pct,
        idle_sec: cfg.idle_reclaim_sec,
        rss_min_mb: cfg.idle_reclaim_rss_min_mb,
        reclaim_pct: cfg.idle_reclaim_pct,
        important_enabled: cfg.idle_reclaim_important_enabled,
        important_min_priority: cfg.idle_reclaim_important_min_priority,
        important_idle_sec: cfg.idle_reclaim_important_idle_sec,
        important_pct: cfg.idle_reclaim_important_pct,
    };
    let candidates = select_idle_candidates(plan_procs, active_pid, pid_tracker, swap_used_pct, &idle_cfg, cfg);

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

        match try_reclaim_cgroup(cgroup, *bytes_to_reclaim_size) {
            ReclaimOutcome::Reclaimed => {
                let mib = Kb(*bytes_to_reclaim_size / 1024).mib();
                mgd_common::sync_print!(
                    "[reclaim] Proactively reclaimed ~{:.0}MB from idle background process {} (PID {})",
                    mib,
                    name,
                    pid
                );
                if let Some(p) = proc_entry {
                    log.log(LogAction::EarlyReclaim, p.pid, &p.name,
                        mib, "proactively pushed idle process to zram");
                }
                // Reset timer → serves as per-process cooldown
                pid_tracker.insert(*pid, std::time::Instant::now());
                LAST_IDLE_RECLAIM.store(unix_timestamp_secs(), Ordering::Relaxed);
            }
            ReclaimOutcome::Skipped => {}
            ReclaimOutcome::Blocked => {
                // EAGAIN: kernel has nothing reclaimable right now — not an error.
                // Reset timer so we back off for a full idle_sec before retrying.
                pid_tracker.insert(*pid, std::time::Instant::now());
            }
            ReclaimOutcome::Failed(e) => {
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

    if let Some(freeze_secs) = cfg.idle_reclaim_freeze_after_sec {
        let mut freeze_count = 0;
        for p in plan_procs {
            if freeze_count >= 2 { break; }
            if Some(p.pid) == active_pid { continue; }
            if p.rss_kb < Kb(cfg.idle_reclaim_rss_min_mb * 1024) { continue; }

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
                    log.log(LogAction::IdleFreeze, p.pid, &p.name,
                            p.rss_kb.mib(), "proactively froze idle background process");
                    freeze_pid_tracker.remove(&p.pid);
                    freeze_count += 1;
                } else {
                    crate::executor::freezer::unfreeze(p.pid);
                }
            }
        }
    }
}

pub(crate) struct IdleReclaimConfig {
    pub max_swap_occupancy_pct: f64,
    pub idle_sec: u64,
    pub rss_min_mb: u64,
    pub reclaim_pct: u64,
    pub important_enabled: bool,
    pub important_min_priority: u8,
    pub important_idle_sec: u64,
    pub important_pct: u64,
}


pub(crate) fn select_idle_candidates(
    procs: &[&crate::monitor::process::Process],
    active_pid: Option<Pid>,
    background_tracker: &std::collections::HashMap<Pid, std::time::Instant>,
    swap_used_pct: f64,
    idle_cfg: &IdleReclaimConfig,
    cfg: &CompiledConfig,
) -> Vec<(Pid, u64)> {
    if swap_used_pct > idle_cfg.max_swap_occupancy_pct {
        return vec![];
    }
    let mut candidates = vec![];
    for p in procs {
        if Some(p.pid) == active_pid {
            continue;
        }
        let prio = crate::engine::decision::get_priority(&p.name, p.exe_basename.as_deref(), cfg);
        let duration = background_tracker
            .get(&p.pid)
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);

        if prio >= 50 {
            if p.rss_kb < Kb(idle_cfg.rss_min_mb * 1024) { continue; }
            if duration < idle_cfg.idle_sec { continue; }
            let reclaim_bytes = p.rss_kb.percent_of(idle_cfg.reclaim_pct).bytes();
            candidates.push((p.pid, reclaim_bytes));
        } else if idle_cfg.important_enabled && prio >= idle_cfg.important_min_priority {
            if p.rss_kb < Kb(idle_cfg.rss_min_mb * 1024) { continue; }
            if duration < idle_cfg.important_idle_sec { continue; }
            let reclaim_bytes = p.rss_kb.percent_of(idle_cfg.important_pct).bytes();
            candidates.push((p.pid, reclaim_bytes));
        }
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;
    use std::time::{Duration, Instant};

    /// Shared fixture — compiled once (the .desktop scan is not free).
    static CFG: LazyLock<CompiledConfig> = LazyLock::new(crate::config::test_config);

    fn make_process(pid: u32, name: &str, rss_kb: u64) -> Process {
        Process {
            pid: Pid(pid),
            name: name.to_string(),
            exe_basename: None,
            rss_kb: Kb(rss_kb),
            swap_kb: Kb(0),
            oom_score: 0,
            cgroup_path: None,
            cpu_pct: 0.0,
            majflt: 0,
            cmdline: String::new(),
        }
    }

    fn default_idle_cfg() -> IdleReclaimConfig {
        IdleReclaimConfig { max_swap_occupancy_pct: 60.0, idle_sec: 180, rss_min_mb: 50, reclaim_pct: 20, important_enabled: false, important_min_priority: 20, important_idle_sec: 300, important_pct: 10 }
    }

    fn make_pid_tracker(pid: u32, secs_ago: u64) -> HashMap<Pid, Instant> {
        let mut m = HashMap::new();
        m.insert(Pid(pid), Instant::now() - Duration::from_secs(secs_ago));
        m
    }

    // ── select_idle_candidates ───────────────────────────────────────────────

    #[test]
    fn idle_reclaim_skips_foreground_pid() {
        let p = make_process(1234, "firefox", 200_000);
        let r = select_idle_candidates(&[&p], Some(Pid(1234)), &make_pid_tracker(1234, 300), 10.0, &default_idle_cfg(), &CFG);
        assert!(r.is_empty());
    }

    #[test]
    fn idle_reclaim_skips_rss_below_minimum() {
        let p = make_process(5678, "app", 40 * 1024); // 40 MB < 50 MB min
        let r = select_idle_candidates(&[&p], None, &make_pid_tracker(5678, 300), 10.0, &default_idle_cfg(), &CFG);
        assert!(r.is_empty());
    }

    #[test]
    fn idle_reclaim_skips_swap_saturated() {
        let p = make_process(5678, "app", 200_000);
        let r = select_idle_candidates(&[&p], None, &make_pid_tracker(5678, 300), 61.0, &default_idle_cfg(), &CFG);
        assert!(r.is_empty());
    }

    #[test]
    fn idle_reclaim_skips_not_yet_idle() {
        let p = make_process(5678, "app", 200_000);
        let r = select_idle_candidates(&[&p], None, &make_pid_tracker(5678, 100), 10.0, &default_idle_cfg(), &CFG);
        assert!(r.is_empty());
    }

    #[test]
    fn idle_reclaim_skips_not_in_tracker() {
        let p = make_process(5678, "app", 200_000);
        let r = select_idle_candidates(&[&p], None, &HashMap::<Pid, Instant>::new(), 10.0, &default_idle_cfg(), &CFG);
        assert!(r.is_empty());
    }

    #[test]
    fn idle_reclaim_selects_eligible() {
        let p = make_process(5678, "app", 200_000);
        let r = select_idle_candidates(&[&p], None, &make_pid_tracker(5678, 300), 10.0, &default_idle_cfg(), &CFG);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, Pid(5678));
        assert_eq!(r[0].1, 40_000 * 1024); // 20% of 200_000 KB * 1024
    }

    #[test]
    fn idle_reclaim_selects_multiple() {
        let p1 = make_process(100, "app_a", 100_000);
        let p2 = make_process(200, "app_b", 150_000);
        let mut tracker = make_pid_tracker(100, 300);
        tracker.insert(Pid(200), Instant::now() - Duration::from_secs(250));
        let r = select_idle_candidates(&[&p1, &p2], None, &tracker, 10.0, &IdleReclaimConfig { max_swap_occupancy_pct: 60.0, idle_sec: 180, rss_min_mb: 50, reclaim_pct: 10, important_enabled: false, important_min_priority: 20, important_idle_sec: 300, important_pct: 10 }, &CFG);
        assert_eq!(r.len(), 2);
        let pids: Vec<Pid> = r.iter().map(|(pid, _)| *pid).collect();
        assert!(pids.contains(&Pid(100)));
        assert!(pids.contains(&Pid(200)));
    }
}
