use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use regex::Regex;

use crate::monitor::process::Process;

// ── Public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum SpikePhase {
    Observing,
    Tracking,
}

pub struct SpikeVictim {
    pub pid: u32,
    pub name: String,
    pub start_time: u64,
    pub frozen_for_spike_pid: u32,
    pub frozen_at: Instant,
}

pub enum SpikeDecision {
    FreezeForHeadroom { needed_kb: u64 },
    ThrottleSpike     { spike_pid: u32, cgroup_path: String },
    RestoreThrottle   { spike_pid: u32, cgroup_path: String },
}

pub struct SpikeSnapshot {
    pub active:  Vec<(u32, String, SpikePhase, u64, usize, bool)>,
    pub victims: Vec<(u32, String, u32)>,
}

// ── Internal types ────────────────────────────────────────────────────────────

struct RssSample {
    rss_kb: u64,
    taken_at: Instant,
}

struct SpikeState {
    pid: u32,
    name: String,
    phase: SpikePhase,
    samples: VecDeque<RssSample>,
    initial_rss_kb: u64,
    rss_max_kb: u64,
    has_peaked: bool,
    has_oscillated: bool,
    cpu_throttled: bool,
    cgroup_path: Option<String>,
    #[allow(dead_code)] // read in test assertions
    force_included: bool,
}

// ── Params — borrowed view of the `[spike_mode]` config fields ────────────────

pub(crate) struct Params<'a> {
    pub window_sec:              u64,
    pub min_rss_kb:              u64,
    pub growth_threshold_kb:     u64,
    pub majflt_threshold:        u64,
    pub oscillation_drop_factor: f64,
    pub min_samples:             usize,
    pub headroom_factor:         f64,
    pub cpu_threshold_pct:       f32,
    pub include:                 Vec<&'a Regex>,
    pub exclude:                 Vec<&'a Regex>,
}

impl<'a> Params<'a> {
    /// Borrow the spike-mode fields from a cycle-scoped config snapshot.
    /// The caller gates on `cfg.spike_mode_enabled`.
    pub(crate) fn from_config(cfg: &'a crate::config::CompiledConfig) -> Params<'a> {
        Params {
            window_sec:              cfg.spike_window_sec,
            min_rss_kb:              cfg.spike_min_rss_kb,
            growth_threshold_kb:     cfg.spike_growth_threshold_kb,
            majflt_threshold:        cfg.spike_majflt_threshold,
            oscillation_drop_factor: cfg.spike_oscillation_drop_factor,
            min_samples:             cfg.spike_min_samples,
            headroom_factor:         cfg.spike_headroom_factor,
            cpu_threshold_pct:       cfg.spike_cpu_threshold_pct,
            include:                 cfg.spike_include.iter().collect(),
            exclude:                 cfg.spike_exclude.iter().collect(),
        }
    }
}

#[cfg(test)]
impl<'a> Params<'a> {
    pub(crate) fn default_test() -> Params<'static> {
        Params {
            window_sec:              120,
            min_rss_kb:              524_288,
            growth_threshold_kb:     102_400,
            majflt_threshold:        500,
            oscillation_drop_factor: 0.90,
            min_samples:             6,
            headroom_factor:         1.25,
            cpu_threshold_pct:       80.0,
            include:                 vec![],
            exclude:                 vec![],
        }
    }
}

// ── Persistence helpers ───────────────────────────────────────────────────────

fn state_dir() -> PathBuf {
    mgd_common::util::home_dir().join(".local/share/mgd/state")
}

// ── SpikeTracker ──────────────────────────────────────────────────────────────

/// All state is owned by the evictor thread. No Arc/Mutex needed.
pub struct SpikeTracker {
    spikes:           HashMap<u32, SpikeState>,
    victims:          HashMap<u32, SpikeVictim>,
    prev_rss:         HashMap<u32, u64>,   // for growth-signal on new candidates
    prev_majflt:      HashMap<u32, u64>,   // for majflt-signal on new candidates
    // scratch buffers — cleared and reused every cycle
    scratch_live:     HashSet<u32>,
    scratch_rss_d:    HashMap<u32, u64>,
    scratch_majflt_d: HashMap<u32, u64>,
}

impl SpikeTracker {
    pub fn new() -> Self {
        SpikeTracker {
            spikes:           HashMap::new(),
            victims:          HashMap::new(),
            prev_rss:         HashMap::new(),
            prev_majflt:      HashMap::new(),
            scratch_live:     HashSet::new(),
            scratch_rss_d:    HashMap::new(),
            scratch_majflt_d: HashMap::new(),
        }
    }

    /// Update spike tracking and return decisions for this cycle.
    /// RAM decisions come before CPU decisions in the returned Vec.
    /// The caller builds `Params` via `Params::from_config()` from its
    /// cycle-scoped config borrow (tests inject values directly).
    pub(crate) fn update(
        &mut self,
        procs: &[Process],
        available_kb: u64,
        p: &Params,
    ) -> Vec<SpikeDecision> {
        let now = Instant::now();
        let window = Duration::from_secs(p.window_sec);

        self.scratch_live.clear();
        self.scratch_live.extend(procs.iter().map(|pr| pr.pid));

        // ── Step 0: Compute per-PID deltas BEFORE updating caches ────────────
        // Deltas are used in steps 1 and 2; caches must not be updated yet.
        self.scratch_rss_d.clear();
        for pr in procs {
            let prev = self.prev_rss.get(&pr.pid).copied().unwrap_or(pr.rss_kb);
            self.scratch_rss_d.insert(pr.pid, pr.rss_kb.saturating_sub(prev));
        }

        self.scratch_majflt_d.clear();
        for pr in procs {
            let prev = self.prev_majflt.get(&pr.pid).copied().unwrap_or(pr.majflt);
            self.scratch_majflt_d.insert(pr.pid, pr.majflt.saturating_sub(prev));
        }

        // ── Step 1: Register new candidates ──────────────────────────────────
        for proc in procs {
            if self.spikes.contains_key(&proc.pid) {
                continue;
            }
            if p.exclude.iter().any(|re| re.is_match(&proc.name)) {
                continue;
            }
            let force_included = p.include.iter().any(|re| re.is_match(&proc.name));
            let growth_signal = self.scratch_rss_d.get(&proc.pid).copied().unwrap_or(0) > p.growth_threshold_kb;
            let majflt_signal = self.scratch_majflt_d.get(&proc.pid).copied().unwrap_or(0) > p.majflt_threshold;
            let is_behavioral = proc.rss_kb >= p.min_rss_kb && (growth_signal || majflt_signal);

            if force_included || is_behavioral {
                mgd_common::sync_print!(
                    "[spike] tracking new candidate: {} (PID {}, rss={:.0}MB, force={})",
                    proc.name, proc.pid, proc.rss_kb as f64 / 1024.0, force_included
                );
                // Use prev_rss as baseline so a process detected at its peak
                // (large delta triggers registration) can still satisfy has_peaked.
                let baseline_kb = self.prev_rss.get(&proc.pid).copied().unwrap_or(proc.rss_kb);
                self.spikes.insert(proc.pid, SpikeState {
                    pid:            proc.pid,
                    name:           proc.name.clone(),
                    phase:          SpikePhase::Observing,
                    samples:        VecDeque::new(),
                    initial_rss_kb: baseline_kb,
                    rss_max_kb:     baseline_kb,
                    has_peaked:     false,
                    has_oscillated: false,
                    cpu_throttled:  false,
                    cgroup_path:    proc.cgroup_path.clone(),
                    force_included,
                });
            }
        }

        // ── Step 2: Update all tracked states ────────────────────────────────
        for proc in procs {
            let Some(state) = self.spikes.get_mut(&proc.pid) else { continue };

            state.cgroup_path = proc.cgroup_path.clone();

            // Push new RSS sample
            state.samples.push_back(RssSample { rss_kb: proc.rss_kb, taken_at: now });

            // Evict samples outside the rolling window
            while state.samples.front()
                .map(|s| now.duration_since(s.taken_at) > window)
                .unwrap_or(false)
            {
                state.samples.pop_front();
            }

            // Recompute rss_max from the live window
            state.rss_max_kb = state.samples.iter()
                .map(|s| s.rss_kb)
                .max()
                .unwrap_or(proc.rss_kb);

            // has_peaked: rss_max grew meaningfully above initial RSS
            if !state.has_peaked
                && state.rss_max_kb > state.initial_rss_kb.saturating_add(p.growth_threshold_kb)
            {
                state.has_peaked = true;
                mgd_common::sync_print!(
                    "[spike] {} (PID {}) peaked at {:.0}MB",
                    state.name, state.pid, state.rss_max_kb as f64 / 1024.0
                );
            }

            // has_oscillated: current RSS dropped ≥(1-drop_factor) from the peak
            if state.has_peaked && !state.has_oscillated {
                let valley_floor = (state.rss_max_kb as f64 * p.oscillation_drop_factor) as u64;
                if proc.rss_kb < valley_floor {
                    state.has_oscillated = true;
                    mgd_common::sync_print!(
                        "[spike] {} (PID {}) oscillated: peak={:.0}MB valley={:.0}MB",
                        state.name, state.pid,
                        state.rss_max_kb as f64 / 1024.0,
                        proc.rss_kb as f64 / 1024.0
                    );
                }
            }

            // Phase transition: Observing → Tracking
            if state.phase == SpikePhase::Observing
                && state.has_peaked
                && state.has_oscillated
                && state.samples.len() >= p.min_samples
            {
                state.phase = SpikePhase::Tracking;
                mgd_common::sync_print!(
                    "[spike] {} (PID {}) → Tracking (rss_max={:.0}MB, {} samples)",
                    state.name, state.pid,
                    state.rss_max_kb as f64 / 1024.0,
                    state.samples.len()
                );
            }
        }

        // ── Step 3: Advance caches for ALL live procs ─────────────────────────
        // Done AFTER steps 1 and 2 so deltas computed in step 0 are valid.
        for proc in procs {
            self.prev_rss.insert(proc.pid, proc.rss_kb);
            self.prev_majflt.insert(proc.pid, proc.majflt);
        }
        self.prev_rss.retain(|pid, _| self.scratch_live.contains(pid));
        self.prev_majflt.retain(|pid, _| self.scratch_live.contains(pid));

        // ── Step 4: Collect decisions — RAM first, CPU after ─────────────────

        let mut decisions: Vec<SpikeDecision> = Vec::new();

        // RAM check: required headroom = sum of rss_max across Tracking states
        let sum_rss_max_kb: u64 = self.spikes.values()
            .filter(|s| s.phase == SpikePhase::Tracking)
            .map(|s| s.rss_max_kb)
            .sum();
        if sum_rss_max_kb > 0 {
            let required_kb = (sum_rss_max_kb as f64 * p.headroom_factor) as u64;
            if available_kb < required_kb {
                decisions.push(SpikeDecision::FreezeForHeadroom {
                    needed_kb: required_kb - available_kb,
                });
            }
        }

        // CPU check: per Tracking state; skip foreground PID to avoid user-visible lag
        let foreground_pid = crate::plugin_server::get_active_foreground_pid();
        for proc in procs {
            let Some(state) = self.spikes.get_mut(&proc.pid) else { continue };
            if state.phase != SpikePhase::Tracking { continue; }
            if foreground_pid == Some(proc.pid) { continue; }

            let Some(ref cg) = state.cgroup_path.clone() else { continue };

            if proc.cpu_pct >= p.cpu_threshold_pct && !state.cpu_throttled {
                state.cpu_throttled = true;
                decisions.push(SpikeDecision::ThrottleSpike {
                    spike_pid:   proc.pid,
                    cgroup_path: cg.clone(),
                });
            } else if proc.cpu_pct < p.cpu_threshold_pct && state.cpu_throttled {
                state.cpu_throttled = false;
                decisions.push(SpikeDecision::RestoreThrottle {
                    spike_pid:   proc.pid,
                    cgroup_path: cg.clone(),
                });
            }
        }

        decisions
    }

    /// Called when a spike PID exits. Returns victims to unfreeze.
    ///
    /// Releases victims whose `frozen_for_spike_pid` is no longer an active
    /// spike — this correctly handles both the single-spike case and the case
    /// where two unrelated processes (e.g. CLion + blender) are tracked
    /// simultaneously: CLion's victims are freed when CLion exits even if
    /// blender is still running.
    ///
    /// For co-session spikes (cargo + rustc): victims are assigned
    /// frozen_for_spike_pid = whichever spike pid was first in the set.
    /// Rustc typically exits before cargo, so cargo holds the victims until
    /// the build is fully done. If cargo exits first the victims are freed
    /// early; the evictor's reactive path re-freezes them if rustc still
    /// needs headroom — acceptable minor churn.
    pub fn on_spike_exit(&mut self, spike_pid: u32) -> Vec<SpikeVictim> {
        self.spikes.remove(&spike_pid);
        self.prev_rss.remove(&spike_pid);
        self.prev_majflt.remove(&spike_pid);

        if self.spikes.is_empty() {
            // Last spike — release everything.
            let victims: Vec<SpikeVictim> = self.victims.drain().map(|(_, v)| v).collect();
            self.persist_victims();
            return victims;
        }

        // Other spikes still active: release only victims tied to the dead spike
        // (frozen_for_spike_pid not in the remaining active set).
        let active: HashSet<u32> = self.spikes.keys().copied().collect();
        let to_release: Vec<u32> = self.victims.values()
            .filter(|v| !active.contains(&v.frozen_for_spike_pid))
            .map(|v| v.pid)
            .collect();
        if to_release.is_empty() {
            return vec![];
        }
        let victims: Vec<SpikeVictim> = to_release.iter()
            .filter_map(|pid| self.victims.remove(pid))
            .collect();
        self.persist_victims();
        victims
    }

    pub fn spike_pids(&self) -> HashSet<u32> {
        self.spikes.keys().copied().collect()
    }

    pub fn victim_pids(&self) -> HashSet<u32> {
        self.victims.keys().copied().collect()
    }

    pub fn record_victim_frozen(&mut self, v: SpikeVictim) {
        self.victims.insert(v.pid, v);
        self.persist_victims();
    }

    /// Persist victim list so they can be unfrozen on daemon restart.
    /// Only serializes {pid, name, start_time} — Instant is not serializable.
    fn persist_victims(&self) {
        let dir = state_dir();
        let _ = fs::create_dir_all(&dir);
        let entries: Vec<serde_json::Value> = self.victims.values()
            .map(|v| serde_json::json!({
                "pid":        v.pid,
                "name":       v.name,
                "start_time": v.start_time,
            }))
            .collect();
        if let Ok(json) = serde_json::to_string(&entries) {
            let _ = mgd_common::util::write_file_atomic(&dir.join("spike_victims.json"), &json);
        }
    }

    /// Load any victims persisted by a previous daemon run and unfreeze them.
    /// Called once at startup, before the evictor thread starts.
    pub fn load_and_unfreeze_victims() {
        let path = state_dir().join("spike_victims.json");
        let Ok(data) = fs::read_to_string(&path) else { return };
        let _ = fs::remove_file(&path);
        let Ok(entries) = serde_json::from_str::<Vec<serde_json::Value>>(&data) else { return };
        for e in &entries {
            let pid        = e["pid"].as_u64().unwrap_or(0) as u32;
            let start_time = e["start_time"].as_u64().unwrap_or(0);
            let name       = e["name"].as_str().unwrap_or("?");
            if pid == 0 { continue; }
            let r = crate::executor::freezer::unfreeze_checked(pid, start_time);
            if r.success {
                mgd_common::sync_print!("[spike] Recovered victim {} (PID {}) after daemon restart", name, pid);
            }
        }
    }

    /// Release victims whose initiator spike has already exited (orphaned victims).
    /// Called every cycle so orphans don't wait for the next spike exit event.
    pub fn drain_orphaned_victims(&mut self) -> Vec<SpikeVictim> {
        if self.victims.is_empty() || self.spikes.is_empty() { return vec![]; }
        let active: HashSet<u32> = self.spikes.keys().copied().collect();
        let to_release: Vec<u32> = self.victims.values()
            .filter(|v| !active.contains(&v.frozen_for_spike_pid))
            .map(|v| v.pid)
            .collect();
        if to_release.is_empty() { return vec![]; }
        let victims: Vec<SpikeVictim> = to_release.iter()
            .filter_map(|pid| self.victims.remove(pid))
            .collect();
        self.persist_victims();
        victims
    }

    /// Release victims frozen beyond `max_secs`. Returns drained victims for caller to unfreeze.
    pub fn drain_timed_out_victims(&mut self, max_secs: u64) -> Vec<SpikeVictim> {
        if max_secs == 0 { return vec![]; }
        let timed_out: Vec<u32> = self.victims.values()
            .filter(|v| v.frozen_at.elapsed().as_secs() >= max_secs)
            .map(|v| v.pid)
            .collect();
        if timed_out.is_empty() { return vec![]; }
        let victims: Vec<SpikeVictim> = timed_out.iter()
            .filter_map(|pid| self.victims.remove(pid))
            .collect();
        self.persist_victims();
        victims
    }

    /// Cgroup paths of spike processes currently CPU-throttled by spike mode.
    pub fn throttled_cgroup_paths(&self) -> Vec<String> {
        self.spikes.values()
            .filter(|s| s.cpu_throttled)
            .filter_map(|s| s.cgroup_path.clone())
            .collect()
    }

    pub fn all_victims(&self) -> impl Iterator<Item = &SpikeVictim> {
        self.victims.values()
    }

    pub fn snapshot(&self) -> SpikeSnapshot {
        SpikeSnapshot {
            active: self.spikes.values().map(|s| (
                s.pid,
                s.name.clone(),
                s.phase.clone(),
                s.rss_max_kb,
                s.samples.len(),
                s.cpu_throttled,
            )).collect(),
            victims: self.victims.values().map(|v| (
                v.pid,
                v.name.clone(),
                v.frozen_for_spike_pid,
            )).collect(),
        }
    }
}

// ── Helper for tests ──────────────────────────────────────────────────────────

#[cfg(test)]
fn make_process(pid: u32, name: &str, rss_kb: u64, cpu_pct: f32, majflt: u64) -> Process {
    Process {
        pid,
        name: name.to_string(),
        exe_basename: None,
        rss_kb,
        swap_kb: 0,
        oom_score: 0,
        cgroup_path: Some(format!("/user.slice/app-{pid}.scope")),
        cpu_pct,
        majflt,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> Params<'static> { Params::default_test() }

    fn p_fast() -> Params<'static> {
        Params { min_samples: 1, window_sec: 10, min_rss_kb: 0,
                 growth_threshold_kb: 100_000, ..Params::default_test() }
    }

    /// Drive a tracker through N RSS values for one PID, returns tracker.
    fn feed(pid: u32, name: &str, rss_vals: &[u64], params: &Params) -> SpikeTracker {
        let mut t = SpikeTracker::new();
        for &rss in rss_vals {
            t.update(&[make_process(pid, name, rss, 0.0, 0)], u64::MAX, params);
        }
        t
    }

    fn make_state_tracking(pid: u32, name: &str, rss_max_kb: u64) -> SpikeState {
        SpikeState {
            pid, name: name.to_string(), phase: SpikePhase::Tracking,
            samples: VecDeque::new(), initial_rss_kb: 0, rss_max_kb,
            has_peaked: true, has_oscillated: true, cpu_throttled: false,
            cgroup_path: Some(format!("/user.slice/app-{pid}.scope")),
            force_included: false,
        }
    }

    fn make_victim(pid: u32, name: &str, for_spike: u32) -> SpikeVictim {
        SpikeVictim {
            pid, name: name.to_string(), start_time: 0,
            frozen_for_spike_pid: for_spike, frozen_at: Instant::now(),
        }
    }

    // T1 ─ window eviction drops old samples
    #[test]
    fn t1_window_eviction() {
        let params = Params {
            window_sec: 0, min_rss_kb: 0, growth_threshold_kb: 0,
            min_samples: 1, ..p()
        };
        let mut t = feed(1, "blender", &[600_000], &params);
        t.update(&[make_process(1, "blender", 700_000, 0.0, 0)], u64::MAX, &params);
        let s = t.spikes.get(&1).unwrap();
        assert_eq!(s.samples.len(), 1);
        assert_eq!(s.samples[0].rss_kb, 700_000);
    }

    // T2 ─ rss_max recomputed correctly after eviction
    // Direct insertion avoids behavioral detection (shrinking process has delta=0).
    #[test]
    fn t2_rss_max_after_eviction() {
        let params = Params { window_sec: 0, ..p() };
        let mut t = SpikeTracker::new();
        t.spikes.insert(1, make_state_tracking(1, "blender", 0));
        // Push peak sample; window_sec=0 means it evicts as soon as time advances.
        t.update(&[make_process(1, "blender", 2_000_000, 0.0, 0)], u64::MAX, &params);
        // Next cycle: old sample evicted, rss_max recalculated from surviving samples.
        t.update(&[make_process(1, "blender", 800_000, 0.0, 0)], u64::MAX, &params);
        assert_eq!(t.spikes.get(&1).unwrap().rss_max_kb, 800_000);
    }

    // T3 ─ FreezeForHeadroom when available < rss_max * factor
    #[test]
    fn t3_freeze_headroom_when_insufficient() {
        // peak 4.2 GB, valley 3.5 GB (>10% drop) → Tracking
        let mut t = feed(1, "cargo", &[500_000, 4_200_000, 3_500_000], &p_fast());
        assert_eq!(t.spikes.get(&1).unwrap().phase, SpikePhase::Tracking);
        let rss_max = t.spikes.get(&1).unwrap().rss_max_kb;
        let available = (rss_max as f64 * 1.0) as u64; // < headroom_factor 1.25
        let d = t.update(&[make_process(1, "cargo", 3_500_000, 0.0, 0)], available, &p_fast());
        assert!(d.iter().any(|x| matches!(x, SpikeDecision::FreezeForHeadroom { .. })));
    }

    // T4 ─ no freeze when headroom sufficient
    #[test]
    fn t4_no_freeze_when_sufficient() {
        let mut t = feed(1, "cargo", &[500_000, 4_200_000, 3_500_000], &p_fast());
        let rss_max = t.spikes.get(&1).unwrap().rss_max_kb;
        let available = (rss_max as f64 * 2.0) as u64;
        let d = t.update(&[make_process(1, "cargo", 3_500_000, 0.0, 0)], available, &p_fast());
        assert!(!d.iter().any(|x| matches!(x, SpikeDecision::FreezeForHeadroom { .. })));
    }

    // T5 ─ ThrottleSpike when cpu >= threshold
    #[test]
    fn t5_throttle_spike_high_cpu() {
        let mut t = feed(1, "blender", &[500_000, 4_200_000, 3_500_000], &p_fast());
        let d = t.update(&[make_process(1, "blender", 3_500_000, 90.0, 0)], u64::MAX, &p_fast());
        assert!(d.iter().any(|x| matches!(x, SpikeDecision::ThrottleSpike { .. })));
    }

    // T6 ─ RestoreThrottle when CPU drops
    #[test]
    fn t6_restore_throttle_on_cpu_drop() {
        let mut t = feed(1, "blender", &[500_000, 4_200_000, 3_500_000], &p_fast());
        t.update(&[make_process(1, "blender", 3_500_000, 90.0, 0)], u64::MAX, &p_fast());
        assert!(t.spikes.get(&1).unwrap().cpu_throttled);
        let d = t.update(&[make_process(1, "blender", 3_500_000, 10.0, 0)], u64::MAX, &p_fast());
        assert!(d.iter().any(|x| matches!(x, SpikeDecision::RestoreThrottle { .. })));
        assert!(!t.spikes.get(&1).unwrap().cpu_throttled);
    }

    // T7 ─ no CPU throttle decisions while still Observing
    #[test]
    fn t7_no_cpu_throttle_while_observing() {
        // Only 1 sample with default min_samples=6 → Observing
        let params = Params { min_rss_kb: 0, cpu_threshold_pct: 80.0, ..p() };
        let mut t = SpikeTracker::new();
        let d = t.update(&[make_process(1, "blender", 600_000, 95.0, 0)], u64::MAX, &params);
        assert!(!d.iter().any(|x| matches!(x, SpikeDecision::ThrottleSpike { .. })));
    }

    // T8 ─ on_spike_exit releases per-initiator, not all-or-nothing
    // Victim 10 is frozen_for spike 1 (cargo); victim 20 is frozen_for spike 2 (rustc).
    // When cargo exits, its victim is freed immediately even though rustc still runs.
    // When rustc exits (last spike), its victim is freed via the drain-all path.
    #[test]
    fn t8_per_initiator_release() {
        let mut t = SpikeTracker::new();
        t.spikes.insert(1, make_state_tracking(1, "cargo", 1_000_000));
        t.spikes.insert(2, make_state_tracking(2, "rustc", 500_000));
        t.victims.insert(10, make_victim(10, "firefox", 1)); // frozen_for cargo
        t.victims.insert(20, make_victim(20, "spotify", 2)); // frozen_for rustc

        // cargo exits: only its victim (firefox) is released; rustc's (spotify) stays
        let v = t.on_spike_exit(1);
        assert_eq!(v.len(), 1, "cargo's victim released immediately");
        assert_eq!(v[0].pid, 10);
        assert!(!t.victims.contains_key(&10));
        assert!(t.victims.contains_key(&20), "spotify held for rustc");

        // rustc exits (last spike): drain-all releases spotify
        let v = t.on_spike_exit(2);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].pid, 20);
        assert!(t.victims.is_empty());
    }

    // T9 ─ no action before min_samples reached
    #[test]
    fn t9_no_action_before_min_samples() {
        let params = Params { min_rss_kb: 0, growth_threshold_kb: 100_000,
                              min_samples: 6, ..p() };
        // 3 samples (< 6): oscillation seen but not enough samples
        let mut t = feed(1, "cargo", &[500_000, 4_200_000, 3_500_000], &params);
        assert_eq!(t.spikes.get(&1).unwrap().phase, SpikePhase::Observing);
        let d = t.update(&[make_process(1, "cargo", 3_500_000, 0.0, 0)], 0, &params);
        assert!(d.is_empty());
    }

    // T10 ─ multiple spikes: headroom = sum of rss_max values
    #[test]
    fn t10_multiple_spikes_headroom_sum() {
        let params = Params { headroom_factor: 1.0, ..p_fast() };
        let mut t = SpikeTracker::new();
        // Directly insert two Tracking states with known rss_max
        t.spikes.insert(1, make_state_tracking(1, "cargo",   4_000_000));
        t.spikes.insert(2, make_state_tracking(2, "blender", 3_000_000));
        let procs = vec![
            make_process(1, "cargo",   4_000_000, 0.0, 0),
            make_process(2, "blender", 3_000_000, 0.0, 0),
        ];
        // available < 4_000_000 + 3_000_000 = 7_000_000 → freeze
        let d = t.update(&procs, 6_000_000, &params);
        assert!(d.iter().any(|x| matches!(x, SpikeDecision::FreezeForHeadroom { .. })));
    }

    // T11 ─ monotonic startup stays Observing
    #[test]
    fn t11_monotonic_startup_stays_observing() {
        let params = Params { min_rss_kb: 0, growth_threshold_kb: 100_000,
                              min_samples: 1, ..p() };
        let t = feed(1, "idea",
            &[1_000_000, 2_000_000, 3_000_000, 4_000_000, 5_000_000, 6_000_000],
            &params);
        let s = t.spikes.get(&1).unwrap();
        assert_eq!(s.phase, SpikePhase::Observing);
        assert!(s.has_peaked);
        assert!(!s.has_oscillated);
    }

    // T12 ─ small wiggle (<10% drop) does NOT promote to Tracking
    #[test]
    fn t12_small_wiggle_not_promoted() {
        let params = Params { min_rss_kb: 0, growth_threshold_kb: 100_000,
                              min_samples: 1, oscillation_drop_factor: 0.90, ..p() };
        // Peak 4.2 GB, then 4.18 GB — only ~0.5% drop
        let t = feed(1, "idea", &[500_000, 4_200_000, 4_180_000], &params);
        let s = t.spikes.get(&1).unwrap();
        assert!(s.has_peaked);
        assert!(!s.has_oscillated, "0.5% drop is not a real valley");
        assert_eq!(s.phase, SpikePhase::Observing);
    }

    // T13 ─ genuine peak + ≥10% drop promotes to Tracking
    #[test]
    fn t13_genuine_oscillation_promotes_to_tracking() {
        let params = Params { min_rss_kb: 0, growth_threshold_kb: 100_000,
                              min_samples: 1, oscillation_drop_factor: 0.90, ..p() };
        // Peak 4.2 GB, valley 3.4 GB — ~19% drop
        let t = feed(1, "cargo", &[500_000, 4_200_000, 3_400_000], &params);
        let s = t.spikes.get(&1).unwrap();
        assert!(s.has_peaked);
        assert!(s.has_oscillated);
        assert_eq!(s.phase, SpikePhase::Tracking);
    }

    // T14 ─ excluded process never tracked
    #[test]
    fn t14_excluded_never_tracked() {
        let excl = Regex::new("^blender$").unwrap();
        let params = Params {
            min_rss_kb: 0, majflt_threshold: 0,
            exclude: vec![&excl], ..p()
        };
        let mut t = SpikeTracker::new();
        t.update(&[make_process(1, "blender", 10_000_000, 0.0, 9999)], 0, &params);
        assert!(t.spikes.is_empty());
    }

    // T15 ─ force-include bypasses behavioral gate but still requires oscillation
    #[test]
    fn t15_force_include_still_requires_oscillation() {
        let incl = Regex::new("^plasmashell$").unwrap();
        let params = Params {
            min_rss_kb: u64::MAX, // would filter it out behaviorally
            min_samples: 1, growth_threshold_kb: 100_000,
            oscillation_drop_factor: 0.90,
            include: vec![&incl], ..p()
        };
        // Monotonic growth — tracked but stays Observing
        let mut t = feed(1, "plasmashell", &[200_000, 300_000, 400_000], &params);
        let s = t.spikes.get(&1).unwrap();
        assert!(s.force_included);
        assert_eq!(s.phase, SpikePhase::Observing);
        // No decisions at Observing
        let d = t.update(&[make_process(1, "plasmashell", 400_000, 0.0, 0)], 0, &params);
        assert!(d.is_empty());
    }

    // T16 ─ needed_kb == exact deficit
    #[test]
    fn t16_needed_kb_equals_deficit() {
        let mut t = feed(1, "cargo", &[500_000, 4_200_000, 3_500_000], &p_fast());
        let rss_max = t.spikes.get(&1).unwrap().rss_max_kb;
        let required = (rss_max as f64 * 1.25) as u64;
        let available = required.saturating_sub(500_000);
        let d = t.update(&[make_process(1, "cargo", 3_500_000, 0.0, 0)], available, &p_fast());
        let SpikeDecision::FreezeForHeadroom { needed_kb } = d.into_iter().next().unwrap() else {
            panic!("expected FreezeForHeadroom");
        };
        assert_eq!(needed_kb, required - available);
    }

    // T17 ─ majflt_delta computed from prev cache, not instantaneous value
    #[test]
    fn t17_majflt_delta_from_cache() {
        let params = Params {
            min_rss_kb: 0,
            growth_threshold_kb: u64::MAX, // disable growth signal
            majflt_threshold: 100,
            min_samples: 1, ..p()
        };
        let mut t = SpikeTracker::new();
        // Cycle 1: cumulative majflt=50, no prev → delta=0, below threshold → not tracked
        t.update(&[make_process(1, "ld", 600_000, 0.0, 50)], u64::MAX, &params);
        assert!(t.spikes.is_empty(), "delta=0 on first observation, should not track");
        assert_eq!(*t.prev_majflt.get(&1).unwrap(), 50, "prev_majflt must be updated");

        // Cycle 2: cumulative=250, delta=200 > 100 → should track now
        t.update(&[make_process(1, "ld", 600_000, 0.0, 250)], u64::MAX, &params);
        assert!(!t.spikes.is_empty(), "delta=200>100 should trigger tracking");
        assert_eq!(*t.prev_majflt.get(&1).unwrap(), 250);
    }
}
