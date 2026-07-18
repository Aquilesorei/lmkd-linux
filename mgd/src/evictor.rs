
use std::collections::{HashSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use mgd_common::types::{Kb, Pid};
use mgd_common::util::unix_timestamp_secs;


use crate::config::CompiledConfig;
use crate::engine::decision::{Action, Decision, plan, get_priority};
use crate::executor::ActionSink;
use crate::executor::registry::{FrozenRegistry, CheckpointRegistry};
use mgd_common::logger::{LogAction, Logger};
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

/// Below this raw swap I/O rate (KB/s) there's no meaningful swap churn happening.
const SWAP_IO_LIVE_FLOOR_KBS: f64 = 1000.0;
/// Below this raw PSI (`some_avg10`, percent) there's no active stall happening.
const PSI_LIVE_FLOOR: f64 = 0.5;

/// Map composite pressure score + trend to the target control state.
/// A rising trend lowers the score needed to escalate.
///
/// Evicting/Critical/Emergency all require `p_score` past a floor that swap's
/// max weight (0.20 of 1.0) cannot reach alone — only the Warning floor
/// (0.15) is reachable by residual swap% by itself (once swap_used_pct >=
/// 75%), with zero PSI/GPU/swap-I/O. Without a liveness check, a stable-but-
/// high swap plateau pins Warning/Elevated forever, since target_state_for
/// never returns Calm and the 12-tick Calm-recovery hysteresis never gets a
/// chance to start counting down. Gate the Warning floor on PSI or swap I/O
/// actually being live so a truly quiet plateau can recover. The genuinely
/// dangerous near-full-swap case is handled separately by
/// `apply_swap_overrides()` (forces Critical/Emergency at 95%/98% swap),
/// independent of this function.
fn target_state_for(p_score: f64, trend: f64, psi_some_avg10: f64, swap_io_kbs: f64) -> ControlState {
    if p_score >= 0.70 || (p_score >= 0.55 && trend > 0.05) {
        ControlState::Emergency
    } else if p_score >= 0.50 || (p_score >= 0.35 && trend > 0.03) {
        ControlState::Critical
    } else if p_score >= 0.30 || (p_score >= 0.20 && trend > 0.02) {
        ControlState::Evicting
    } else if p_score >= 0.15 {
        if psi_some_avg10 > PSI_LIVE_FLOOR || swap_io_kbs > SWAP_IO_LIVE_FLOOR_KBS {
            ControlState::Warning
        } else {
            ControlState::Calm
        }
    } else {
        ControlState::Calm
    }
}

/// Hysteresis state machine over `ControlState`. Escalation needs 2 consecutive
/// ticks of the same target (instant on a sharp spike to Critical/Emergency);
/// recovery needs 4–12 ticks depending on how far down the target is.
struct StateMachine {
    current: ControlState,
    pending: ControlState,
    pending_ticks: usize,
}

impl StateMachine {
    fn new() -> Self {
        Self { current: ControlState::Calm, pending: ControlState::Calm, pending_ticks: 0 }
    }

    /// Advance one tick toward `target`. Pure — no I/O, no logging.
    /// Returns true when an instant escalation fired (caller logs it).
    fn advance(&mut self, target: ControlState, trend: f64) -> bool {
        if target > self.current {
            // Escalation: needs 2 ticks of persistence, unless it's a massive spike (instant)
            let instant_escalate = (target == ControlState::Emergency || target == ControlState::Critical) && trend > 0.08;
            if instant_escalate {
                self.current = target;
                self.pending = target;
                self.pending_ticks = 0;
                return true;
            }
            if target == self.pending {
                self.pending_ticks += 1;
                if self.pending_ticks >= 2 {
                    self.current = target;
                    self.pending_ticks = 0;
                }
            } else {
                self.pending = target;
                self.pending_ticks = 1;
            }
        } else if target < self.current {
            // Recovery: needs longer persistence
            let required_ticks = match target {
                ControlState::Calm => 12,    // 1 minute of Calm at 5s polling
                ControlState::Warning => 6, // 30s
                _ => 4,                     // 20s
            };
            if target == self.pending {
                self.pending_ticks += 1;
                if self.pending_ticks >= required_ticks {
                    self.current = target;
                    self.pending_ticks = 0;
                }
            } else {
                self.pending = target;
                self.pending_ticks = 1;
            }
        } else {
            // Target matches current state: reset pending state
            self.pending = self.current;
            self.pending_ticks = 0;
        }
        false
    }
}

/// Rolling inputs for the composite pressure score: previous score/time for
/// the trend derivative, previous vmstat counters for the swap I/O rate.
struct ScoreTracker {
    last_score: f64,
    last_time: std::time::Instant,
    last_pswpin: u64,
    last_pswpout: u64,
}

/// One cycle's composite score plus the inputs kept for attribution logging.
struct CycleScore {
    p_score: f64,
    trend: f64,
    swap_io_kbs: f64,
    gpu_val: f64,
}

impl ScoreTracker {
    fn new() -> Self {
        let (last_pswpin, last_pswpout) = crate::monitor::meminfo::read_vmstat_swap_counters();
        Self { last_score: 0.0, last_time: std::time::Instant::now(), last_pswpin, last_pswpout }
    }

    /// Read the per-cycle inputs (vmstat counters, GPU cache) and fold them in.
    fn update(&mut self, some_avg10: f64, meminfo: &MemInfo, now: std::time::Instant) -> CycleScore {
        let (pswpin, pswpout) = crate::monitor::meminfo::read_vmstat_swap_counters();
        let gpu_kb = crate::plugin_server::get_total_gpu_kb();
        self.update_with(some_avg10, meminfo, pswpin, pswpout, gpu_kb, now)
    }

    /// Pure: composite score = 55% PSI + 20% swap used + 15% GPU UMA + 10% swap I/O
    /// rate, plus trend (dP/dt). Swap I/O is pswpin+pswpout delta over the cycle,
    /// pages → KB/s, normalized at 50 MB/s (51200 KB/s) total — above that = thrash.
    fn update_with(
        &mut self,
        some_avg10: f64,
        meminfo: &MemInfo,
        cur_pswpin: u64,
        cur_pswpout: u64,
        total_gpu: Kb,
        now: std::time::Instant,
    ) -> CycleScore {
        let dt = now.duration_since(self.last_time).as_secs_f64();

        let swap_io_pages = cur_pswpin.saturating_sub(self.last_pswpin)
            .saturating_add(cur_pswpout.saturating_sub(self.last_pswpout));
        self.last_pswpin = cur_pswpin;
        self.last_pswpout = cur_pswpout;
        let swap_io_kbs = if dt > 0.1 { (swap_io_pages * 4) as f64 / dt } else { 0.0 }; // 4 KB/page — x86_64
        let swap_io_val = (swap_io_kbs / 51200.0).clamp(0.0, 1.0);

        let psi_val = (some_avg10 / 100.0).clamp(0.0, 1.0);
        let swap_val = if meminfo.swap_total_kb.0 > 0 {
            (meminfo.swap_used_pct() / 100.0).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let gpu_val = if meminfo.total_kb.0 > 0 {
            (total_gpu.0 as f64 / meminfo.total_kb.0 as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let p_score = 0.55 * psi_val + 0.20 * swap_val + 0.15 * gpu_val + 0.10 * swap_io_val;

        let trend = if dt > 0.1 {
            (p_score - self.last_score) / dt
        } else {
            0.0
        };
        self.last_score = p_score;
        self.last_time = now;

        CycleScore { p_score, trend, swap_io_kbs, gpu_val }
    }
}

/// Set once on zram-compact EACCES (grant absent) to log only once per session.
static ZRAM_COMPACT_DISABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Unix-seconds of the last page-cache drop (0 = never).
static LAST_CACHE_DROP: AtomicU64 = AtomicU64::new(0);

/// Unix-seconds of the last early background process reclaim.
static LAST_EARLY_RECLAIM: AtomicU64 = AtomicU64::new(0);

/// Unix-seconds of the last successful zram compaction (0 = never).
static LAST_ZRAM_COMPACT: AtomicU64 = AtomicU64::new(0);

/// Unix-seconds of the last successful idle cgroup reclaim (0 = never).
static LAST_IDLE_RECLAIM: AtomicU64 = AtomicU64::new(0);

/// Feature gate state for `mgctl status` — read-only accessors over the
/// evictor's private statics so the IPC thread can report per-feature state.
pub(crate) struct FeatureGates {
    pub zram_compact_disabled: bool,
    pub last_zram_compact: u64,
    pub last_cache_drop: u64,
    pub last_early_reclaim: u64,
    pub last_idle_reclaim: u64,
}

pub(crate) fn feature_gates() -> FeatureGates {
    FeatureGates {
        zram_compact_disabled: ZRAM_COMPACT_DISABLED.load(Ordering::Relaxed),
        last_zram_compact: LAST_ZRAM_COMPACT.load(Ordering::Relaxed),
        last_cache_drop: LAST_CACHE_DROP.load(Ordering::Relaxed),
        last_early_reclaim: LAST_EARLY_RECLAIM.load(Ordering::Relaxed),
        last_idle_reclaim: LAST_IDLE_RECLAIM.load(Ordering::Relaxed),
    }
}

#[allow(clippy::too_many_arguments)] // one-shot thread entry point wired in main.rs; a params struct adds no clarity
/// Elevate the *calling thread* to SCHED_RR prio 20 (falls back to nice -20).
/// On Linux both `sched_setscheduler(0, ..)` and `setpriority(PRIO_PROCESS, 0, ..)`
/// are per-thread, and spawned threads inherit policy — so this lives in the
/// evictor and must only be called from inside `run()`, never before
/// `thread::spawn` in main (that would put the blocking-I/O maintenance thread
/// and IPC/recovery on the RT budget too).
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
) {
    // RT priority for this thread only. Called here (not in main) so the
    // IPC/recovery/maintenance threads don't inherit SCHED_RR — maintenance
    // does blocking disk I/O and must not burn RT budget.
    try_elevate_scheduler_priority();

    mgd_common::sync_print!("[responder] PSI source: {}", monitor::psi::pressure_source());

    // Try subprocess trigger first (mgd-psi-trigger with cap_perfmon+ep).
    // Falls back to direct PsiTrigger (works without cap on older kernels),
    // then 5s polling.
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

    loop {
        if crate::should_shutdown() {
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

        if crate::should_reload() {
            crate::config::reload();
            crate::plugin_server::broadcast_config_reload();
        }

        // One config snapshot per cycle (cheap Arc clone, no lock held);
        // a reload swaps the global and is picked up here next iteration.
        let cfg = crate::config::get();

        // Respawn PSI subprocess if elevated_pct changed on reload (new threshold
        // needs a fresh fd — the kernel trigger can't be re-armed on an existing fd).
        if (cfg.psi.elevated_pct - psi_elevated_pct).abs() > 0.001 {
            psi_elevated_pct = cfg.psi.elevated_pct;
            psi_subprocess = monitor::psi::PsiSubprocess::new(psi_elevated_pct);
            if psi_subprocess.is_some() {
                mgd_common::sync_print!("[responder] PSI trigger respawned at elevated_pct={:.1}%.", psi_elevated_pct);
            }
        }

        // Zero-CPU idle: if pressure was Normal last cycle, block on the kernel
        // PSI trigger instead of doing expensive /proc/pid walks.
        // Timeout 5s just to re-check shutdown flags and maintain the loop pulse.
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
                // Helper death during shutdown is systemd killing the cgroup,
                // not a crash — let the loop-top guard handle teardown.
                if crate::should_shutdown() {
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

        let target_state = target_state_for(score.p_score, score.trend, pressure.some_avg10, score.swap_io_kbs);
        if state_machine.advance(target_state, score.trend) {
            mgd_common::sync_print!("[controller] Instant escalation triggered due to rapid pressure spike (trend: {:.3})", score.trend);
        }

        let mut effective_level = state_machine.current.to_pressure_level();

        let swap_used_pct = meminfo.swap_used_pct();
        // swap_exhausted (≥95%) is forwarded to plan() as a per-process Kill escalator for
        // prio ≥80 (expendable tier). Distinct from apply_swap_overrides() which raises the
        // *pressure level* — both run every cycle.
        let swap_exhausted = meminfo.swap_total_kb.0 > 0 && swap_used_pct >= 95.0;

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
                    score.p_score
                );
            }
        }

        last_level = effective_level.clone();

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

            let procs = monitor::process::list_processes();
            let frozen_set: HashSet<Pid> = frozen.lock().unwrap().frozen_pids().into_iter()
                .chain(spike_tracker.victim_pids())
                .collect();
            let plan_procs: Vec<&Process> = procs.iter()
                .filter(|p| !frozen_set.contains(&p.pid))
                .collect();

            // Update CPU throttling (tiered, debounced) — only at Elevated+ pressure.
            // psi_some_avg10 lets ThrottleManager force-release a cgroup that's been
            // throttled past cfg.throttle_max_duration_sec with no active stall, even
            // if residual swap% alone is still keeping effective_level at Elevated.
            throttle.update(&plan_procs, active_pid, effective_level >= PressureLevel::Elevated, pressure.some_avg10, &cfg);
            *throttle_snapshot.lock().unwrap() = throttle.snapshot();

            // Cap memory.max on expendable background cgroups at High+ pressure
            memcap.update(&plan_procs, active_pid, &effective_level, &cfg);

            // Idle cgroup reclaim: only runs at Normal/Calm pressure
            if effective_level == PressureLevel::Normal
                && cfg.idle_reclaim_enabled {
                    check_idle_process_reclaim(&cfg, &plan_procs, active_pid, &mut idle_reclaim_pid_tracker, &mut idle_freeze_pid_tracker, &frozen, &log);
                }
        }

        // Restore memory caps when pressure drops below High
        if effective_level < PressureLevel::High {
            memcap.restore_all();
        }

        // ── Spike mode: runs every cycle, even at Normal PSI ─────────────────
        run_spike_cycle(&cfg, &mut spike_tracker, &frozen, &log, meminfo.available_kb, &spike_snapshot);

        if effective_level == PressureLevel::Normal {
            continue;
        }

        let mut procs = monitor::process::list_processes();
        procs.sort_by_key(|p| std::cmp::Reverse(p.rss_kb));

        print_status(&pressure, &effective_level, &procs, &meminfo, &frozen, &cfg);

        crate::plugin_server::broadcast_pressure(&effective_level.to_string());

        // System pre-actions run before plan() so their freed RAM shrinks the
        // deficit. zram compact (cheaper) first, then cache drop.
        compact_zram(&effective_level, &log, &cfg);

        // Cleanup expired recently killed cgroups (cooldown = 45s)
        let now_inst = std::time::Instant::now();
        recently_killed_cgroups.retain(|_, time| now_inst.duration_since(*time).as_secs() < 45);

        // Exclude frozen PIDs, spike PIDs (active build/IDE processes), and spike victims
        // from plan() candidates. Spike processes are protected while tracked — killing
        // them mid-build corrupts the output and defeats the whole point of spike mode.
        let spike_pids_active = spike_tracker.spike_pids();
        let spike_victim_pids = spike_tracker.victim_pids();
        let frozen_set: HashSet<Pid> = frozen.lock().unwrap().frozen_pids().into_iter()
            .chain(spike_pids_active)
            .chain(spike_victim_pids)
            .collect();
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
        check_early_process_reclaim(&effective_level, &plan_procs, active_pid, &log, &cfg);
        check_cache_drop(&effective_level, &log, &cfg);

        let decisions = plan(&effective_level, &plan_procs, meminfo.available_kb, meminfo.total_kb, swap_exhausted, &cfg);
        if decisions.is_empty() {
            mgd_common::sync_print!("✓ No action needed.");
        } else {
            let destructive_count = execute_plan(
                &decisions, &plan_procs, &frozen, &checkpointed,
                &log, &event_log, &mut recently_killed_cgroups,
                &mut crate::executor::RealSink,
            );
            // Ring the doorbell — recovery thread may have new work.
            let (lock, cvar) = &*recovery_wake;
            *lock.lock().unwrap() = true;
            cvar.notify_one();

            // Kills/checkpoints freed RAM + zram slots — wake maintenance so it
            // can attempt proactive swap reclaim while headroom still exists.
            if destructive_count > 0 {
                let (lock, cvar) = &*reclaim_wake;
                *lock.lock().unwrap() = true;
                cvar.notify_one();
            }
        }

        // Cycle attribution: one grep-able line per active cycle tying the
        // composite score inputs to what each feature did. The 3am question
        // "which feature acted and why" is answered here, not by archaeology.
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

/// Idle-cycle work on PSI trigger timeout: no pressure event fired, so run idle
/// cgroup reclaim on its cooldown. Shared by the subprocess and direct-trigger
/// wait paths; the caller `continue`s afterwards to skip the full cycle.
fn idle_timeout_reclaim(
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
    let frozen_set: HashSet<Pid> =
        frozen.lock().unwrap().frozen_pids().into_iter()
            .chain(spike_tracker.victim_pids())
            .collect();
    let plan_procs: Vec<&Process> =
        procs.iter().filter(|p| !frozen_set.contains(&p.pid)).collect();
    let active_pid = crate::plugin_server::get_active_foreground_pid();
    check_idle_process_reclaim(
        cfg, &plan_procs, active_pid,
        idle_reclaim_pid_tracker, idle_freeze_pid_tracker,
        frozen, log,
    );
}

/// Spike-mode cycle: release victims (spike exited / timed out / orphaned), feed
/// the tracker, and execute its decisions. Runs every cycle, even at Normal PSI —
/// spike exit detection and proactive headroom management are independent of
/// reactive eviction and must not be gated by the Normal continue.
fn run_spike_cycle(
    cfg: &CompiledConfig,
    spike_tracker: &mut crate::spike_mode::SpikeTracker,
    frozen: &Arc<Mutex<FrozenRegistry>>,
    log: &Logger,
    available: Kb,
    spike_snapshot: &Arc<Mutex<crate::spike_mode::SpikeSnapshot>>,
) {
    let spike_procs = monitor::process::list_processes();
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

    // Update tracker and execute decisions
    let spike_decisions = if cfg.spike_mode_enabled {
        spike_tracker.update(&spike_procs, available, &crate::spike_mode::Params::from_config(cfg))
    } else {
        vec![]
    };
    for decision in spike_decisions {
        match decision {
            crate::spike_mode::SpikeDecision::FreezeForHeadroom { needed } => {
                let spike_pids  = spike_tracker.spike_pids();
                let victim_pids = spike_tracker.victim_pids();
                let exclude: HashSet<Pid> = frozen.lock().unwrap().frozen_pids()
                    .into_iter()
                    .chain(spike_pids.iter().copied())
                    .chain(victim_pids.iter().copied())
                    .collect();
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
                            let _ = reclaim_cgroup(cg, proc.rss_kb.bytes());
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

/// Execute the planned decisions. Returns the number of synchronous destructive
/// actions (Kill, Checkpoint) — Terminate is async (SIGTERM→5s→SIGKILL), its RAM
/// isn't freed yet when the caller signals maintenance, so it doesn't count.
#[allow(clippy::too_many_arguments)] // the sink is the 8th; bundling shared registries into a struct adds no clarity
fn execute_plan(
    decisions: &[Decision],
    plan_procs: &[&Process],
    frozen: &Arc<Mutex<FrozenRegistry>>,
    checkpointed: &Arc<Mutex<CheckpointRegistry>>,
    log: &Logger,
    event_log: &crate::events::EventLog,
    recently_killed_cgroups: &mut HashMap<String, std::time::Instant>,
    sink: &mut impl ActionSink,
) -> u32 {
    mgd_common::sync_print!("⚡ EXECUTING:");
    let mut destructive_count = 0u32;
    for d in decisions {
        if frozen.lock().unwrap().is_frozen(d.pid) { continue; }

        let result_str = execute_decision(d, frozen, checkpointed, log, event_log, sink);
        mgd_common::sync_print!("  {:<10} {:<8} {:<22} {:>6.1}MB  {}", d.action, d.pid, d.name, d.rss.mib(), result_str);

        // Post-freeze reclaim: push full RSS to zram while the process is immobile.
        // SIGSTOP guarantees no re-faults, so 100% is safe (unlike active-process
        // early reclaim which caps at 50% to preserve a working set).
        if d.action == Action::Freeze && result_str == "frozen"
            && let Some(cgroup_path) = plan_procs.iter()
                .find(|p| p.pid == d.pid)
                .and_then(|p| p.cgroup_path.as_deref())
            {
                let reclaim_bytes = d.rss.bytes();
                match sink.reclaim(cgroup_path, reclaim_bytes) {
                    Ok(true) => {
                        mgd_common::sync_print!(
                            "[reclaim] Post-freeze: pushed ~{:.0}MB from PID {} ({}) to zram",
                            d.rss.mib(), d.pid, d.name
                        );
                        log.log(LogAction::FreezeReclaim, d.pid, &d.name,
                                d.rss.mib(), "pushed to zram after pressure freeze");
                    }
                    Ok(false) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => {}
                }
            }

        // Terminate is async (SIGTERM→5s→SIGKILL): RAM not freed yet when we
        // signal maintenance. Only count synchronous kills (Kill, Checkpoint).
        if matches!(d.action, Action::Kill | Action::Checkpoint) {
            destructive_count += 1;
        }

        // Track recently terminated/killed cgroups
        if d.action == Action::Kill || d.action == Action::Terminate {
            let cgroup_path = plan_procs.iter()
                .find(|p| p.pid == d.pid)
                .and_then(|p| p.cgroup_path.clone())
                .or_else(|| mgd_common::util::read_process_cgroup_path(d.pid.0));
            if let Some(cgroup_path) = cgroup_path {
                recently_killed_cgroups.insert(cgroup_path, std::time::Instant::now());
            }
        }
    }
    destructive_count
}

/// Pure: given current effective pressure + swap stats + sustained-start timer,
/// returns the updated (effective_level, sustained_critical_swap_start).
/// No I/O, no logging — caller owns those responsibilities.
pub(crate) fn apply_swap_overrides(
    mut effective: PressureLevel,
    swap_used_pct: f64,
    swap_total: Kb,
    sustained_start: Option<std::time::Instant>,
    now: std::time::Instant,
) -> (PressureLevel, Option<std::time::Instant>) {
    let swap_exhausted = swap_total.0 > 0 && swap_used_pct >= 95.0;
    if swap_exhausted && effective < PressureLevel::Critical {
        effective = PressureLevel::Critical;
    }

    let swap_near_full = swap_total.0 > 0 && swap_used_pct >= 98.0;
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
    pub important_enabled: bool,
    pub important_min_priority: u8,
    pub important_idle_sec: u64,
    pub important_pct: u64,
}

/// Pure: returns (pid, reclaim_bytes) pairs for processes eligible for idle cgroup reclaim.
/// Keyed by PID in background_tracker. No cgroup writes — caller handles I/O.
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


/// Compact zram at Elevated+ to free fragmented pages before touching a process.
/// No-op unless `[zram] compact_on_elevated = true`; skips pools < `min_used_mb`.
/// EACCES (grant absent) disables the feature for the session.
fn compact_zram(level: &PressureLevel, log: &Logger, cfg: &CompiledConfig) {
    if *level < PressureLevel::Elevated {
        return;
    }
    if ZRAM_COMPACT_DISABLED.load(Ordering::Relaxed) {
        return;
    }
    if !cfg.compact_zram_on_elevated {
        return;
    }
    let min_used_mb = cfg.zram_min_used_mb;

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
                log.log_system(LogAction::Zram, &device, reclaimed as f64,
                    &format!("compacted {before_mb}MB->{after_mb}MB"));
                LAST_ZRAM_COMPACT.store(unix_timestamp_secs(), Ordering::Relaxed);
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                ZRAM_COMPACT_DISABLED.store(true, Ordering::Relaxed);
                mgd_common::sync_print!(
                    "[zram] compact unavailable ({device}): sysfs grant absent — disabling for \
                     session. See docs/PRIVILEGE_DESIGN.md §1."
                );
                log.log_system(LogAction::Zram, &device, 0.0, "unavailable: EACCES (grant absent)");
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
fn check_cache_drop(level: &PressureLevel, log: &Logger, cfg: &CompiledConfig) {
    if !cfg.cache_drop_enabled || cfg.cache_drop_paths.is_empty() {
        return;
    }
    if *level < cfg.cache_drop_trigger {
        return;
    }

    let now = unix_timestamp_secs();
    let last = LAST_CACHE_DROP.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < cfg.cache_drop_cooldown_secs {
        return;
    }
    // Arm up-front: the walk is the cost being rate-limited.
    LAST_CACHE_DROP.store(now, Ordering::Relaxed);

    let mut total_files = 0usize;
    let mut total_bytes = 0u64;
    for r in crate::monitor::cache::drop_caches(&cfg.cache_drop_paths) {
        if r.files_advised > 0 {
            log.log_system(LogAction::Cache, &r.pattern,
                (r.bytes_advised / (1024 * 1024)) as f64,
                &format!("advised {} files", r.files_advised));
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

fn execute_decision(
    d: &Decision,
    frozen: &Arc<Mutex<FrozenRegistry>>,
    checkpointed: &Arc<Mutex<CheckpointRegistry>>,
    log: &Logger,
    event_log: &crate::events::EventLog,
    sink: &mut impl ActionSink,
) -> String {
    let (action, s) = match d.action {
        Action::Freeze => (LogAction::Freeze, freeze_process(d, frozen, sink)),
        Action::Terminate => (LogAction::Terminate, terminate_process(d, sink)),
        Action::Kill => (LogAction::Kill, kill_process(d, sink)),
        Action::Checkpoint => execute_checkpoint(d, checkpointed, sink),
        Action::None => return String::new(),
    };
    let detail = format!("{s} [{}]", d.reason);
    log.log(action, d.pid, &d.name, d.rss.mib(), &detail);
    crate::events::push(event_log, action, d.pid, &d.name, &detail);
    s
}

fn freeze_process(d: &Decision, frozen: &Arc<Mutex<FrozenRegistry>>, sink: &mut impl ActionSink) -> String {
    // Sink aborts if start_time is gone rather than freeze a recycled PID.
    let r = sink.freeze(d.pid);
    if r.success {
        if frozen.lock().unwrap().add(d.pid, &d.name) {
            "frozen".into()
        } else {
            sink.unfreeze(d.pid);
            "aborted: process vanished before fingerprint".into()
        }
    } else {
        // "process vanished" is the benign PID-recycle race (not a real
        // freeze failure) — keep it in the same "aborted:" bucket as the
        // fingerprint-mismatch case above so log/event greps for "aborted:"
        // vs "fail:" keep distinguishing races from real failures.
        let msg = r.error.unwrap_or_default();
        if msg.starts_with("process vanished") {
            format!("aborted: {msg}")
        } else {
            format!("fail: {msg}")
        }
    }
}

fn terminate_process(d: &Decision, sink: &mut impl ActionSink) -> String {
    sink.terminate(d.pid);
    "terminating (async SIGTERM→SIGKILL)".into()
}

fn kill_process(d: &Decision, sink: &mut impl ActionSink) -> String {
    let r = sink.kill(d.pid);

    match r.error {
        None => "killed".to_string(),
        Some(err) => format!("fail: {}", err),
    }
}

fn execute_checkpoint(d: &Decision, checkpointed: &Arc<Mutex<CheckpointRegistry>>, sink: &mut impl ActionSink) -> (LogAction, String) {
    let r = sink.checkpoint(d.pid, &d.name);
    if r.success {
        let dir = r.snapshot_dir.unwrap();
        checkpointed.lock().unwrap()
            .add(d.pid, &d.name, dir.clone(), d.rss);
        (LogAction::Checkpoint, format!("checkpointed → {dir:?}"))
    } else {
        // Dump failed — this binary is not safely checkpointable; record it so
        // future cycles skip CRIU for it and route directly here.
        crate::executor::checkpoint::mark_binary_failed(&d.name);
        // Fall back using the same prio logic as when cp_supported=false:
        // prio >= 60 → Terminate (async, graceful); prio < 60 → Kill (protected process,
        // dump likely failed due to complex state, don't wait for SIGTERM).
        if d.prio >= 60 {
            terminate_process(d, sink);
            (LogAction::Terminate, format!("terminating (CRIU failed: {})", r.error.unwrap_or_default()))
        } else {
            let kr = sink.kill(d.pid);
            if kr.success {
                (LogAction::Kill, format!("killed (CRIU failed: {})", r.error.unwrap_or_default()))
            } else {
                (LogAction::Kill, format!("kill_fail: {}", kr.error.unwrap_or_default()))
            }
        }
    }
}

fn is_cgroup_leaf(cgroup_path: &str) -> bool {
    let sysfs_dir = crate::throttle::cgroup_sysfs_path(cgroup_path, "");
    std::fs::read_dir(&sysfs_dir)
        .map(|entries| {
            !entries
                .filter_map(|e| e.ok())
                .any(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
        })
        .unwrap_or(true) // fail open: can't read → assume leaf, attempt reclaim
}

pub(crate) fn reclaim_cgroup(cgroup_path: &str, bytes_size: u64) -> Result<bool, std::io::Error> {

    if bytes_size == 0 {
        return Ok(false);
    }

    if cgroup_path == "/" || cgroup_path.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cannot reclaim root cgroup",
        ));
    }
    if !is_cgroup_leaf(cgroup_path) {
        return Ok(false); // non-leaf: skip silently, caller must not log or reset timers
    }
    let reclaim_path = crate::throttle::cgroup_sysfs_path(cgroup_path, "memory.reclaim");
    if reclaim_path.exists() {
        match std::fs::write(&reclaim_path, format!("{}", bytes_size)) {
            Ok(()) => return Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(false),
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, "cgroup memory.reclaim not found"))
}

fn check_early_process_reclaim(
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

    // Filter candidate background processes:
    // - Priority >= 50 (expendable/user apps)
    // - RSS > 20MB
    // - Not the active foreground process
    // prio >= 60 processes are handled by plan() → Action::Freeze + post-freeze reclaim,
    // which reclaims 100% RSS after SIGSTOP (no re-fault risk). Restricting to prio < 60
    // here avoids a redundant memory.reclaim write on those same pids.
    // Known edge: if swap_exhausted causes a Kill (prio>=80) to break the plan() loop
    // before a prio[60,79] process is reached, that process misses both paths for that
    // cycle. Accepted — swap_exhausted+Critical is already a crisis; next cycle corrects.
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
        let reclaim_bytes_size = p.rss_kb.percent_of(50).bytes(); // reclaim 50% of RSS
        let Some(cgroup) = p.cgroup_path.as_deref() else { continue };
        match reclaim_cgroup(cgroup, reclaim_bytes_size) {
            Ok(true) => {
                mgd_common::sync_print!(
                    "[reclaim] Proactively pushed ~{}MB of background PID {} ({}) to Zram",
                    reclaim_bytes_size / (1024 * 1024),
                    p.pid,
                    p.name
                );
                log.log(LogAction::EarlyReclaim, p.pid, &p.name,
                    (reclaim_bytes_size / (1024 * 1024)) as f64, "pushed to zram via cgroup reclaim");
            }
            Ok(false) => {}
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

        match reclaim_cgroup(cgroup, *bytes_to_reclaim_size) {
            Ok(true) => {
                mgd_common::sync_print!(
                    "[reclaim] Proactively reclaimed ~{}MB from idle background process {} (PID {})",
                    bytes_to_reclaim_size / (1024 * 1024),
                    name,
                    pid
                );
                if let Some(p) = proc_entry {
                    log.log(LogAction::EarlyReclaim, p.pid, &p.name,
                        (*bytes_to_reclaim_size / (1024 * 1024)) as f64,
                        "proactively pushed idle process to zram");
                }
                // Reset timer → serves as per-process cooldown
                pid_tracker.insert(*pid, std::time::Instant::now());
                LAST_IDLE_RECLAIM.store(unix_timestamp_secs(), Ordering::Relaxed);
            }
            Ok(false) => {}
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


#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::psi::PressureLevel;
    use crate::monitor::process::Process;
    use std::collections::HashMap;
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

    // ── target_state_for / StateMachine ──────────────────────────────────────

    #[test]
    fn target_state_thresholds() {
        // psi_some_avg10 = 5.0 (clearly live) so the Warning-tier liveness
        // gate doesn't interfere with these threshold checks.
        assert_eq!(target_state_for(0.0, 0.0, 5.0, 0.0), ControlState::Calm);
        assert_eq!(target_state_for(0.15, 0.0, 5.0, 0.0), ControlState::Warning);
        assert_eq!(target_state_for(0.30, 0.0, 5.0, 0.0), ControlState::Evicting);
        assert_eq!(target_state_for(0.50, 0.0, 5.0, 0.0), ControlState::Critical);
        assert_eq!(target_state_for(0.70, 0.0, 5.0, 0.0), ControlState::Emergency);
    }

    #[test]
    fn target_state_rising_trend_lowers_bar() {
        assert_eq!(target_state_for(0.21, 0.03, 5.0, 0.0), ControlState::Evicting);
        assert_eq!(target_state_for(0.36, 0.04, 5.0, 0.0), ControlState::Critical);
        assert_eq!(target_state_for(0.56, 0.06, 5.0, 0.0), ControlState::Emergency);
        // Same scores without the trend stay one tier lower
        assert_eq!(target_state_for(0.21, 0.0, 5.0, 0.0), ControlState::Warning);
        assert_eq!(target_state_for(0.36, 0.0, 5.0, 0.0), ControlState::Evicting);
        assert_eq!(target_state_for(0.56, 0.0, 5.0, 0.0), ControlState::Critical);
    }

    #[test]
    fn warning_floor_needs_liveness() {
        // Stale swap alone (p_score=0.15 from swap_val=0.75 * 0.20) with zero
        // PSI and zero swap I/O — no longer stuck at Warning, recovers to Calm.
        assert_eq!(target_state_for(0.15, 0.0, 0.0, 0.0), ControlState::Calm);
    }

    #[test]
    fn warning_floor_fires_on_live_psi() {
        assert_eq!(target_state_for(0.15, 0.0, 5.0, 0.0), ControlState::Warning);
    }

    #[test]
    fn warning_floor_fires_on_live_swap_io() {
        assert_eq!(target_state_for(0.15, 0.0, 0.0, 5000.0), ControlState::Warning);
    }

    #[test]
    fn warning_floor_respects_psi_floor() {
        // Just below both floors — still Calm, not Warning.
        assert_eq!(
            target_state_for(0.15, 0.0, PSI_LIVE_FLOOR - 0.1, SWAP_IO_LIVE_FLOOR_KBS - 1.0),
            ControlState::Calm
        );
    }

    #[test]
    fn escalation_needs_two_ticks() {
        let mut sm = StateMachine::new();
        assert!(!sm.advance(ControlState::Evicting, 0.0));
        assert_eq!(sm.current, ControlState::Calm);
        assert!(!sm.advance(ControlState::Evicting, 0.0));
        assert_eq!(sm.current, ControlState::Evicting);
    }

    #[test]
    fn instant_escalation_on_sharp_spike() {
        let mut sm = StateMachine::new();
        assert!(sm.advance(ControlState::Emergency, 0.09));
        assert_eq!(sm.current, ControlState::Emergency);
    }

    #[test]
    fn no_instant_escalation_below_critical() {
        let mut sm = StateMachine::new();
        // Sharp trend but target only Evicting — still needs 2 ticks.
        assert!(!sm.advance(ControlState::Evicting, 0.09));
        assert_eq!(sm.current, ControlState::Calm);
    }

    #[test]
    fn escalation_target_change_resets_ticks() {
        let mut sm = StateMachine::new();
        sm.advance(ControlState::Warning, 0.0);
        sm.advance(ControlState::Evicting, 0.0); // pending switches — tick count restarts
        assert_eq!(sm.current, ControlState::Calm);
        sm.advance(ControlState::Evicting, 0.0);
        assert_eq!(sm.current, ControlState::Evicting);
    }

    #[test]
    fn recovery_to_calm_needs_twelve_ticks() {
        let mut sm = StateMachine::new();
        sm.advance(ControlState::Critical, 0.09); // instant escalate
        assert_eq!(sm.current, ControlState::Critical);
        for _ in 0..11 {
            sm.advance(ControlState::Calm, 0.0);
            assert_eq!(sm.current, ControlState::Critical);
        }
        sm.advance(ControlState::Calm, 0.0);
        assert_eq!(sm.current, ControlState::Calm);
    }

    #[test]
    fn matching_target_resets_pending() {
        let mut sm = StateMachine::new();
        sm.advance(ControlState::Warning, 0.0); // pending=Warning, 1 tick
        sm.advance(ControlState::Calm, 0.0);    // target==current → pending reset
        sm.advance(ControlState::Warning, 0.0); // must start over
        assert_eq!(sm.current, ControlState::Calm);
        sm.advance(ControlState::Warning, 0.0);
        assert_eq!(sm.current, ControlState::Warning);
    }

    // ── ScoreTracker ─────────────────────────────────────────────────────────

    fn make_meminfo(total_kb: u64, swap_total_kb: u64, swap_free_kb: u64) -> MemInfo {
        MemInfo { available_kb: Kb(total_kb / 2), total_kb: Kb(total_kb), swap_free_kb: Kb(swap_free_kb), swap_total_kb: Kb(swap_total_kb) }
    }

    fn fresh_tracker(now: Instant) -> ScoreTracker {
        ScoreTracker { last_score: 0.0, last_time: now, last_pswpin: 0, last_pswpout: 0 }
    }

    #[test]
    fn score_psi_only() {
        let t0 = Instant::now();
        let mut st = fresh_tracker(t0);
        let mi = make_meminfo(16_000_000, 0, 0);
        let s = st.update_with(50.0, &mi, 0, 0, Kb(0), t0 + Duration::from_secs(5));
        // 55% weight on PSI 0.5, everything else zero
        assert!((s.p_score - 0.275).abs() < 1e-9);
        assert_eq!(s.gpu_val, 0.0);
        assert_eq!(s.swap_io_kbs, 0.0);
    }

    #[test]
    fn score_all_components_maxed_hits_one() {
        let t0 = Instant::now();
        let mut st = fresh_tracker(t0);
        let mi = make_meminfo(16_000_000, 12_000_000, 0); // swap 100% used
        // GPU = total RAM, swap I/O far over 50 MB/s → every component clamps to 1.0
        let s = st.update_with(200.0, &mi, 10_000_000, 10_000_000, Kb(16_000_000), t0 + Duration::from_secs(5));
        assert!((s.p_score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn swap_io_rate_and_trend() {
        let t0 = Instant::now();
        let mut st = fresh_tracker(t0);
        let mi = make_meminfo(16_000_000, 0, 0);
        // 12800 pages × 4KB / 5s = 10240 KB/s → io_val 0.2 → score 0.02
        let s = st.update_with(0.0, &mi, 12800, 0, Kb(0), t0 + Duration::from_secs(5));
        assert!((s.swap_io_kbs - 10240.0).abs() < 1e-6);
        assert!((s.p_score - 0.02).abs() < 1e-9);
        assert!((s.trend - 0.02 / 5.0).abs() < 1e-9);
        // Next cycle, no new I/O: score falls back to 0, trend goes negative
        let s2 = st.update_with(0.0, &mi, 12800, 0, Kb(0), t0 + Duration::from_secs(10));
        assert_eq!(s2.p_score, 0.0);
        assert!(s2.trend < 0.0);
    }

    #[test]
    fn tiny_dt_yields_zero_trend_and_io() {
        let t0 = Instant::now();
        let mut st = fresh_tracker(t0);
        let mi = make_meminfo(16_000_000, 0, 0);
        // dt below the 0.1s floor: swap I/O rate and trend are suppressed
        let s = st.update_with(80.0, &mi, 99999, 99999, Kb(0), t0);
        assert_eq!(s.swap_io_kbs, 0.0);
        assert_eq!(s.trend, 0.0);
    }

    // ── apply_swap_overrides ─────────────────────────────────────────────────

    #[test]
    fn swap_below_95_no_override() {
        let now = Instant::now();
        let (level, sustained) = apply_swap_overrides(PressureLevel::Elevated, 94.9, Kb(10_000_000), None, now);
        assert_eq!(level, PressureLevel::Elevated);
        assert!(sustained.is_none());
    }

    #[test]
    fn swap_95_forces_critical() {
        let now = Instant::now();
        let (level, _) = apply_swap_overrides(PressureLevel::Elevated, 95.0, Kb(10_000_000), None, now);
        assert_eq!(level, PressureLevel::Critical);
    }

    #[test]
    fn swap_95_no_override_when_already_critical() {
        let now = Instant::now();
        let (level, _) = apply_swap_overrides(PressureLevel::Critical, 95.0, Kb(10_000_000), None, now);
        assert_eq!(level, PressureLevel::Critical);
    }

    #[test]
    fn swap_no_device_no_override() {
        let now = Instant::now();
        let (level, _) = apply_swap_overrides(PressureLevel::Elevated, 99.0, Kb(0), None, now);
        assert_eq!(level, PressureLevel::Elevated);
    }

    #[test]
    fn sustained_critical_swap_escalates_emergency() {
        let start = Instant::now() - Duration::from_secs(46);
        let now = Instant::now();
        let (level, _) = apply_swap_overrides(PressureLevel::Critical, 98.5, Kb(10_000_000), Some(start), now);
        assert_eq!(level, PressureLevel::Emergency);
    }

    #[test]
    fn sustained_critical_swap_not_yet_45s() {
        let start = Instant::now() - Duration::from_secs(44);
        let now = Instant::now();
        let (level, _) = apply_swap_overrides(PressureLevel::Critical, 98.5, Kb(10_000_000), Some(start), now);
        assert_eq!(level, PressureLevel::Critical);
    }

    #[test]
    fn sustained_critical_swap_resets_when_swap_drops() {
        let start = Instant::now() - Duration::from_secs(50);
        let now = Instant::now();
        let (_, sustained) = apply_swap_overrides(PressureLevel::Critical, 97.9, Kb(10_000_000), Some(start), now);
        assert!(sustained.is_none(), "timer must reset when swap < 98%");
    }

    #[test]
    fn swap_98_starts_sustained_timer() {
        let now = Instant::now();
        let (_, sustained) = apply_swap_overrides(PressureLevel::Critical, 98.0, Kb(10_000_000), None, now);
        assert!(sustained.is_some(), "timer must start at >=98% swap + Critical");
    }

    #[test]
    fn swap_98_preserves_existing_timer() {
        let start = Instant::now() - Duration::from_secs(10);
        let now = Instant::now();
        let (_, sustained) = apply_swap_overrides(PressureLevel::Critical, 98.0, Kb(10_000_000), Some(start), now);
        assert!(sustained.unwrap().elapsed().as_secs() >= 10, "existing timer must not be reset");
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

    // ── ThrottledState ───────────────────────────────────────────────────────

    #[test]
    fn throttle_state_eq() {
        use crate::throttle::ThrottledState;
        assert_eq!(ThrottledState::None, ThrottledState::None);
        assert_ne!(ThrottledState::None, ThrottledState::WeightOnly);
        assert_ne!(ThrottledState::WeightOnly, ThrottledState::Full);
    }

    // ── execute_plan / execute_decision (MockSink) ───────────────────────────

    use crate::executor::OpResult;
    use crate::executor::checkpoint::CheckpointResult;

    #[derive(Clone, Copy)]
    enum ReclaimScript { OkTrue, OkFalse, WouldBlock }

    /// Records every sink call in order; return values are scriptable per test.
    struct MockSink {
        calls: Vec<(&'static str, Pid)>,
        reclaims: Vec<(String, u64)>,
        freeze_ok: bool,
        checkpoint_ok: bool,
        reclaim_script: ReclaimScript,
    }

    impl MockSink {
        fn new() -> Self {
            Self {
                calls: Vec::new(),
                reclaims: Vec::new(),
                freeze_ok: true,
                checkpoint_ok: true,
                reclaim_script: ReclaimScript::OkTrue,
            }
        }

        fn names(&self) -> Vec<&'static str> {
            self.calls.iter().map(|(n, _)| *n).collect()
        }
    }

    impl ActionSink for MockSink {
        fn freeze(&mut self, pid: Pid) -> OpResult {
            self.calls.push(("freeze", pid));
            if self.freeze_ok { OpResult::success() } else { OpResult::fail("scripted freeze failure") }
        }

        fn unfreeze(&mut self, pid: Pid) -> OpResult {
            self.calls.push(("unfreeze", pid));
            OpResult::success()
        }

        fn terminate(&mut self, pid: Pid) -> OpResult {
            self.calls.push(("terminate", pid));
            OpResult::success()
        }

        fn kill(&mut self, pid: Pid) -> OpResult {
            self.calls.push(("kill", pid));
            OpResult::success()
        }

        fn checkpoint(&mut self, pid: Pid, _name: &str) -> CheckpointResult {
            self.calls.push(("checkpoint", pid));
            if self.checkpoint_ok {
                CheckpointResult::ok(pid, std::path::PathBuf::from("/tmp/mock-snap"))
            } else {
                CheckpointResult::err(pid, "scripted CRIU failure")
            }
        }

        fn reclaim(&mut self, cgroup: &str, bytes: u64) -> std::io::Result<bool> {
            self.reclaims.push((cgroup.to_string(), bytes));
            match self.reclaim_script {
                ReclaimScript::OkTrue => Ok(true),
                ReclaimScript::OkFalse => Ok(false),
                ReclaimScript::WouldBlock =>
                    Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "cgroup busy")),
            }
        }
    }

    fn decision(pid: u32, name: &str, action: Action, prio: u8) -> Decision {
        Decision {
            pid: Pid(pid),
            name: name.to_string(),
            action,
            rss: Kb(200 * 1024),
            reason: "test".into(),
            prio,
        }
    }

    fn exec_fixture() -> (Arc<Mutex<FrozenRegistry>>, Arc<Mutex<CheckpointRegistry>>, Logger, crate::events::EventLog) {
        (
            Arc::new(Mutex::new(FrozenRegistry::new())),
            Arc::new(Mutex::new(CheckpointRegistry::new())),
            Logger::null(),
            crate::events::new_log(),
        )
    }

    #[test]
    fn execute_plan_skips_already_frozen_pid() {
        let (frozen, checkpointed, log, events) = exec_fixture();
        // Deserialize a fixture entry — add() would need a live /proc entry.
        *frozen.lock().unwrap() =
            serde_json::from_str(r#"{"frozen":{"4242":["stale",0,1]}}"#).unwrap();
        let decisions = vec![decision(4242, "stale", Action::Kill, 80)];
        let mut sink = MockSink::new();
        let n = execute_plan(&decisions, &[], &frozen, &checkpointed, &log, &events,
            &mut HashMap::new(), &mut sink);
        assert_eq!(n, 0);
        assert!(sink.calls.is_empty(), "sink must never be called for a frozen PID");
    }

    #[test]
    fn destructive_count_counts_kill_and_checkpoint_not_freeze_or_terminate() {
        let (frozen, checkpointed, log, events) = exec_fixture();
        let decisions = vec![
            decision(900_001, "a", Action::Kill, 80),
            decision(900_002, "b", Action::Terminate, 80),
            decision(900_003, "c", Action::Checkpoint, 80),
            decision(900_004, "d", Action::Freeze, 80),
        ];
        let mut sink = MockSink::new();
        let n = execute_plan(&decisions, &[], &frozen, &checkpointed, &log, &events,
            &mut HashMap::new(), &mut sink);
        // Kill + Checkpoint are synchronous frees; Terminate is async (RAM not
        // freed yet), Freeze frees nothing.
        assert_eq!(n, 2);
        assert_eq!(checkpointed.lock().unwrap().count(), 1);
    }

    #[test]
    fn post_freeze_reclaim_fires_on_frozen_result_with_cgroup() {
        let (frozen, checkpointed, log, events) = exec_fixture();
        // Own PID: registry fingerprint (start_time re-read) succeeds, so the
        // result string is "frozen" — the only path that arms post-freeze reclaim.
        let me = std::process::id();
        let mut p = make_process(me, "self", 200 * 1024);
        p.cgroup_path = Some("/user.slice/test.scope".to_string());
        let procs = [&p];
        let decisions = vec![decision(me, "self", Action::Freeze, 80)];
        let mut sink = MockSink::new();
        execute_plan(&decisions, &procs, &frozen, &checkpointed, &log, &events,
            &mut HashMap::new(), &mut sink);
        assert_eq!(sink.names(), vec!["freeze"]);
        assert_eq!(sink.reclaims, vec![("/user.slice/test.scope".to_string(), Kb(200 * 1024).bytes())]);
        assert!(frozen.lock().unwrap().is_frozen(Pid(me)));
    }

    #[test]
    fn post_freeze_reclaim_skipped_without_frozen_result_or_cgroup() {
        // Failed freeze → no reclaim, even with a cgroup present.
        let (frozen, checkpointed, log, events) = exec_fixture();
        let me = std::process::id();
        let mut p = make_process(me, "self", 200 * 1024);
        p.cgroup_path = Some("/user.slice/test.scope".to_string());
        let mut sink = MockSink::new();
        sink.freeze_ok = false;
        execute_plan(&[decision(me, "self", Action::Freeze, 80)], &[&p], &frozen,
            &checkpointed, &log, &events, &mut HashMap::new(), &mut sink);
        assert!(sink.reclaims.is_empty(), "no reclaim after a failed freeze");
        assert!(!frozen.lock().unwrap().is_frozen(Pid(me)));

        // Successful freeze but no cgroup known → no reclaim.
        let (frozen, checkpointed, log, events) = exec_fixture();
        let p = make_process(me, "self", 200 * 1024); // cgroup_path: None
        let mut sink = MockSink::new();
        execute_plan(&[decision(me, "self", Action::Freeze, 80)], &[&p], &frozen,
            &checkpointed, &log, &events, &mut HashMap::new(), &mut sink);
        assert!(sink.reclaims.is_empty(), "no reclaim without a cgroup path");
        assert!(frozen.lock().unwrap().is_frozen(Pid(me)));
    }

    #[test]
    fn post_freeze_reclaim_ok_false_and_wouldblock_are_silent() {
        // Ok(false) and WouldBlock must not disturb execution: freeze stays
        // registered, no fallback action fires, destructive count stays 0.
        for script in [ReclaimScript::OkFalse, ReclaimScript::WouldBlock] {
            let (frozen, checkpointed, log, events) = exec_fixture();
            let me = std::process::id();
            let mut p = make_process(me, "self", 200 * 1024);
            p.cgroup_path = Some("/user.slice/test.scope".to_string());
            let mut sink = MockSink::new();
            sink.reclaim_script = script;
            let n = execute_plan(&[decision(me, "self", Action::Freeze, 80)], &[&p],
                &frozen, &checkpointed, &log, &events, &mut HashMap::new(), &mut sink);
            assert_eq!(n, 0);
            assert_eq!(sink.names(), vec!["freeze"]);
            assert_eq!(sink.reclaims.len(), 1);
            assert!(frozen.lock().unwrap().is_frozen(Pid(me)));
        }
    }

    #[test]
    fn freeze_rolls_back_when_registry_fingerprint_fails() {
        // Nonexistent PID: sink freeze succeeds (scripted) but the registry
        // start_time re-read fails → unfreeze rollback, nothing registered.
        let (frozen, checkpointed, log, events) = exec_fixture();
        let decisions = vec![decision(900_005, "ghost", Action::Freeze, 80)];
        let mut sink = MockSink::new();
        execute_plan(&decisions, &[], &frozen, &checkpointed, &log, &events,
            &mut HashMap::new(), &mut sink);
        assert_eq!(sink.names(), vec!["freeze", "unfreeze"]);
        assert!(sink.reclaims.is_empty());
        assert_eq!(frozen.lock().unwrap().count(), 0);
    }

    #[test]
    fn checkpoint_failure_falls_back_by_priority() {
        let (frozen, checkpointed, log, events) = exec_fixture();
        let decisions = vec![
            // prio >= 60 → async terminate (graceful); prio < 60 → immediate kill.
            decision(900_006, "cp-fail-expendable-t", Action::Checkpoint, 60),
            decision(900_007, "cp-fail-protected-t", Action::Checkpoint, 30),
        ];
        let mut sink = MockSink::new();
        sink.checkpoint_ok = false;
        let n = execute_plan(&decisions, &[], &frozen, &checkpointed, &log, &events,
            &mut HashMap::new(), &mut sink);
        assert_eq!(sink.names(), vec!["checkpoint", "terminate", "checkpoint", "kill"]);
        assert_eq!(checkpointed.lock().unwrap().count(), 0, "failed dumps must not be registered");
        assert_eq!(n, 2); // Checkpoint decisions count destructive even via fallback
    }

    #[test]
    fn recently_killed_cgroups_tracks_kill_and_terminate_only() {
        let (frozen, checkpointed, log, events) = exec_fixture();
        let mut a = make_process(900_008, "kill-me", 100 * 1024);
        a.cgroup_path = Some("/u/kill.scope".to_string());
        let mut b = make_process(900_009, "term-me", 100 * 1024);
        b.cgroup_path = Some("/u/term.scope".to_string());
        let mut c = make_process(900_010, "freeze-me", 100 * 1024);
        c.cgroup_path = Some("/u/freeze.scope".to_string());
        let procs = [&a, &b, &c];
        let decisions = vec![
            decision(900_008, "kill-me", Action::Kill, 80),
            decision(900_009, "term-me", Action::Terminate, 80),
            decision(900_010, "freeze-me", Action::Freeze, 80),
        ];
        let mut recently = HashMap::new();
        let mut sink = MockSink::new();
        execute_plan(&decisions, &procs, &frozen, &checkpointed, &log, &events,
            &mut recently, &mut sink);
        assert!(recently.contains_key("/u/kill.scope"));
        assert!(recently.contains_key("/u/term.scope"));
        assert!(!recently.contains_key("/u/freeze.scope"), "freeze must not arm the kill cooldown");
        assert_eq!(recently.len(), 2);
    }
}
