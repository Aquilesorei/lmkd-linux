//! Detects a process family — grouped by exact `/proc/PID/cmdline`, not by
//! exe_basename (too coarse: every python-based daemon on the box would
//! collapse into one bucket) — whose live-instance count only ever grows and
//! is never reaped. This is the "leaking MCP server" shape: a supervisor
//! keeps spawning fresh workers instead of reusing one persistent connection,
//! and nothing ever exits. Grouping by cmdline instead of a single tracked
//! PID (contrast `spike_mode.rs`, which tracks one process's RSS oscillation)
//! catches the failure mode regardless of which binary is doing it.
//!
//! Priority/protect filtering (never touch priority <= 19 or `[[protect]]`
//! matches) happens at the call site in `evictor.rs`, same as spike_mode's
//! victim selection — this module only applies its own `exclude` regex.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use mgd_common::types::{Kb, Pid};
use regex::Regex;

use crate::monitor::process::Process;

// ── Public types ─────────────────────────────────────────────────────────────

pub enum LeakDecision {
    TerminateStale { pid: Pid, name: String, group: String },
}

pub struct LeakSnapshot {
    pub groups: Vec<(String /* display */, usize /* count */, Kb /* total_rss */, bool /* leaking */)>,
}

// ── Internal types ────────────────────────────────────────────────────────────

struct CountSample {
    count: usize,
    taken_at: Instant,
}

struct GroupState {
    samples: VecDeque<CountSample>,
    cooldown_until: Option<Instant>,
    last_display: String,
    last_count: usize,
    last_total_rss: Kb,
    last_leaking: bool,
}

pub struct LeakTracker {
    groups: HashMap<String, GroupState>,
}

impl Default for LeakTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl LeakTracker {
    pub fn new() -> Self {
        LeakTracker { groups: HashMap::new() }
    }

    pub fn snapshot(&self) -> LeakSnapshot {
        LeakSnapshot {
            groups: self.groups.values()
                .map(|g| (g.last_display.clone(), g.last_count, g.last_total_rss, g.last_leaking))
                .collect(),
        }
    }
}

// ── Params — borrowed view of the `[process_leak_guard]` config fields ───────

pub(crate) struct Params<'a> {
    pub window_sec:          u64,
    pub min_group_count:     usize,
    pub min_group_rss:       Kb,
    pub growth_over_window:  usize,
    pub min_samples:         usize,
    pub keep_newest:         usize,
    pub cooldown_sec:        u64,
    pub exclude:             Vec<&'a Regex>,
}

impl<'a> Params<'a> {
    /// Borrow the leak-guard fields from a cycle-scoped config snapshot.
    /// The caller gates on `cfg.leak_guard_enabled`.
    pub(crate) fn from_config(cfg: &'a crate::config::CompiledConfig) -> Params<'a> {
        Params {
            window_sec:         cfg.leak_guard_window_sec,
            min_group_count:    cfg.leak_guard_min_group_count,
            min_group_rss:      Kb(cfg.leak_guard_min_group_rss_kb),
            growth_over_window: cfg.leak_guard_growth_over_window,
            min_samples:        cfg.leak_guard_min_samples,
            keep_newest:        cfg.leak_guard_keep_newest,
            cooldown_sec:       cfg.leak_guard_cooldown_sec,
            exclude:            cfg.leak_guard_exclude.iter().collect(),
        }
    }
}

#[cfg(test)]
impl<'a> Params<'a> {
    pub(crate) fn default_test() -> Params<'static> {
        Params {
            window_sec:         600,
            min_group_count:    4,
            min_group_rss:      Kb(1_000),
            growth_over_window: 3,
            min_samples:        3,
            keep_newest:        1,
            cooldown_sec:       30,
            exclude:            vec![],
        }
    }
}

fn group_key(proc: &Process) -> String {
    if proc.cmdline.is_empty() {
        format!("noargs:{}", proc.name)
    } else {
        proc.cmdline.clone()
    }
}

impl LeakTracker {

    pub(crate) fn update(&mut self, procs: &[Process], p: &Params) -> Vec<LeakDecision> {
        let now = Instant::now();

        let mut current_groups: HashMap<String, Vec<&Process>> = HashMap::new();
        for proc in procs {
            current_groups.entry(group_key(proc)).or_default().push(proc);
        }

        // Drop state for anything that fell below the floor or exited entirely.
        self.groups.retain(|key, _| {
            current_groups.get(key).map(|m| m.len()).unwrap_or(0) >= p.min_group_count
        });

        let mut decisions = Vec::new();

        for (key, members) in &current_groups {
            if members.len() < p.min_group_count {
                continue;
            }
            let representative = members[0];
            if p.exclude.iter().any(|re| {
                re.is_match(&representative.name)
                    || representative.exe_basename.as_deref().is_some_and(|e| re.is_match(e))
            }) {
                continue;
            }

            let total_rss: Kb = members.iter().map(|m| m.rss_kb).sum();
            let display = representative.exe_basename.clone().unwrap_or_else(|| representative.name.clone());

            let state = self.groups.entry(key.clone()).or_insert_with(|| GroupState {
                samples: VecDeque::new(),
                cooldown_until: None,
                last_display: display.clone(),
                last_count: 0,
                last_total_rss: Kb(0),
                last_leaking: false,
            });

            state.samples.push_back(CountSample { count: members.len(), taken_at: now });
            let window = Duration::from_secs(p.window_sec);
            while state.samples.front().is_some_and(|s| now.duration_since(s.taken_at) > window) {
                state.samples.pop_front();
            }

            let enough_samples = state.samples.len() >= p.min_samples;
            let never_shrunk = {
                let mut ok = true;
                let mut prev: Option<usize> = None;
                for s in &state.samples {
                    if let Some(p) = prev && s.count < p {
                        ok = false;
                        break;
                    }
                    prev = Some(s.count);
                }
                ok
            };
            let grown_enough = match (state.samples.front(), state.samples.back()) {
                (Some(first), Some(last)) => last.count.saturating_sub(first.count) >= p.growth_over_window,
                _ => false,
            };
            let rss_gate = total_rss.0 >= p.min_group_rss.0;
            let leaking = enough_samples && never_shrunk && grown_enough && rss_gate;

            state.last_display = display.clone();
            state.last_count = members.len();
            state.last_total_rss = total_rss;
            state.last_leaking = leaking;

            let in_cooldown = state.cooldown_until.is_some_and(|t| now < t);
            if leaking && !in_cooldown {
                // Newest-first by PID: Linux allocates PIDs monotonically within
                // a session (barring wraparound, which needs ~4M live processes
                // to matter here), so highest PID == most recently spawned. This
                // avoids a real /proc/PID/stat read inside otherwise-pure
                // decision logic (kept this module free of I/O, same as
                // spike_mode's update() — the PID-recycle guard belongs at
                // execution time, not here).
                let mut ordered: Vec<&Process> = members.clone();
                ordered.sort_by_key(|m| std::cmp::Reverse(m.pid.0));

                for proc in ordered.into_iter().skip(p.keep_newest) {
                    decisions.push(LeakDecision::TerminateStale {
                        pid: proc.pid,
                        name: proc.name.clone(),
                        group: display.clone(),
                    });
                }
                state.cooldown_until = Some(now + Duration::from_secs(p.cooldown_sec));
            }
        }

        decisions
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use mgd_common::types::Kb as K;

    fn make_process(pid: u32, name: &str, cmdline: &str, rss_kb: u64) -> Process {
        Process {
            pid: Pid(pid),
            name: name.to_string(),
            exe_basename: Some(name.to_string()),
            rss_kb: K(rss_kb),
            swap_kb: K(0),
            oom_score: 0,
            cgroup_path: None,
            cpu_pct: 0.0,
            majflt: 0,
            cmdline: cmdline.to_string(),
        }
    }

    /// A group of `count` processes sharing `cmdline`, PIDs starting at `base_pid`.
    fn group(base_pid: u32, count: u32, name: &str, cmdline: &str, rss_kb: u64) -> Vec<Process> {
        (0..count).map(|i| make_process(base_pid + i, name, cmdline, rss_kb)).collect()
    }

    // T1 ─ below min_group_count: never enters tracking, never flagged
    #[test]
    fn t1_below_min_count_not_tracked() {
        let mut t = LeakTracker::new();
        let p = Params::default_test(); // min_group_count = 4
        let procs = group(1, 3, "leaker", "leaker --serve", 10_000);
        for _ in 0..5 {
            let d = t.update(&procs, &p);
            assert!(d.is_empty());
        }
        assert!(t.snapshot().groups.is_empty());
    }

    // T2 ─ below min_group_rss: tracked, never flagged leaking
    #[test]
    fn t2_below_min_rss_not_flagged() {
        let mut t = LeakTracker::new();
        let mut p = Params::default_test();
        p.min_group_rss = K(1_000_000); // way above the tiny test RSS
        let mut pid = 1u32;
        for i in 0..5 {
            let procs = group(pid, 4 + i, "leaker", "leaker --serve", 10);
            pid += 4 + i;
            let d = t.update(&procs, &p);
            assert!(d.is_empty());
        }
        let snap = t.snapshot();
        assert!(!snap.groups.is_empty());
        assert!(snap.groups.iter().all(|(_, _, _, leaking)| !leaking));
    }

    // T3 ─ a shrink within the window clears never_shrunk — mirrors a legitimately
    // elastic multi-process app (tabs/workers coming and going), not a leak.
    #[test]
    fn t3_shrink_resets_never_shrunk() {
        let mut t = LeakTracker::new();
        let p = Params::default_test();
        let mut pid = 1u32;
        let counts = [4u32, 5, 6, 5, 6, 7]; // dips at index 3
        for c in counts {
            let procs = group(pid, c, "leaker", "leaker --serve", 10_000);
            pid += c + 1; // fresh PIDs each cycle so start_time ordering stays sane
            let d = t.update(&procs, &p);
            assert!(d.is_empty(), "should never flag while a shrink is still in the window");
        }
    }

    // T4 ─ strict monotonic growth past the gates flags leaking and emits
    // TerminateStale for all but keep_newest.
    #[test]
    fn t4_monotonic_growth_flags_leaking() {
        let mut t = LeakTracker::new();
        let p = Params::default_test(); // min_samples=3, growth_over_window=3, keep_newest=1
        let mut pid = 1u32;
        let mut decisions = Vec::new();
        for c in [4u32, 5, 6, 8] {
            let procs = group(pid, c, "leaker", "leaker --serve", 10_000);
            decisions = t.update(&procs, &p);
            pid += c;
        }
        assert_eq!(decisions.len(), 8 - 1); // all but keep_newest=1
        assert!(decisions.iter().all(|d| matches!(d, LeakDecision::TerminateStale { .. })));
        let snap = t.snapshot();
        assert!(snap.groups.iter().any(|(_, _, _, leaking)| *leaking));
    }

    // T5 ─ keep_newest respected: exactly N survive (are absent from decisions)
    #[test]
    fn t5_keep_newest_respected() {
        let mut t = LeakTracker::new();
        let mut p = Params::default_test();
        p.keep_newest = 2;
        let mut pid = 1u32;
        let mut decisions = Vec::new();
        for c in [4u32, 5, 6, 8] {
            let procs = group(pid, c, "leaker", "leaker --serve", 10_000);
            decisions = t.update(&procs, &p);
            pid += c;
        }
        assert_eq!(decisions.len(), 8 - 2);
    }

    // T6 ─ excluded pattern (matched against name/exe_basename) never tracked
    #[test]
    fn t6_excluded_pattern_never_tracked() {
        let mut t = LeakTracker::new();
        let mut p = Params::default_test();
        let re = Regex::new("^leaker$").unwrap();
        p.exclude = vec![&re];
        let mut pid = 1u32;
        for c in [4u32, 5, 6, 8] {
            let procs = group(pid, c, "leaker", "leaker --serve", 10_000);
            let d = t.update(&procs, &p);
            assert!(d.is_empty());
            pid += c;
        }
        assert!(t.snapshot().groups.is_empty());
    }


    #[test]
    fn t7_different_cmdline_same_exe_not_grouped() {
        let mut t = LeakTracker::new();
        let p = Params::default_test();
        let mut pid = 1u32;
        let mut last_decisions = Vec::new();
        let stable_pid = Pid(9000);
        for i in 0..4u32 {
            let mut procs = group(pid, 5 + i, "python3", "python3 chroma-mcp --serve", 10_000);
            pid += 5 + i;
            // A stable, non-growing python3 process with different args — must
            // never be swept into the leaking group's decisions.
            procs.push(make_process(stable_pid.0, "python3", "python3 /usr/sbin/tuned -l -P", 5_000));
            last_decisions = t.update(&procs, &p);
        }
        assert!(last_decisions.iter().all(|d| {
            let LeakDecision::TerminateStale { pid, .. } = d;
            *pid != stable_pid
        }));
    }

    // T8 ─ cooldown suppresses immediate re-trigger even though count is still high
    #[test]
    fn t8_cooldown_suppresses_immediate_retrigger() {
        let mut t = LeakTracker::new();
        let p = Params::default_test(); // cooldown_sec = 30
        let mut pid = 1u32;
        let mut fired_once = false;
        for c in [4u32, 5, 6, 8, 9, 10] {
            let procs = group(pid, c, "leaker", "leaker --serve", 10_000);
            pid += c;
            let d = t.update(&procs, &p);
            if !d.is_empty() {
                assert!(!fired_once, "should not re-fire again immediately within cooldown");
                fired_once = true;
            }
        }
        assert!(fired_once, "should have fired at least once");
    }

    // T9 ─ samples older than window_sec are pruned
    #[test]
    fn t9_window_eviction_drops_stale_samples() {
        let mut t = LeakTracker::new();
        let mut p = Params::default_test();
        p.window_sec = 0; // every sample is immediately "stale" on the next tick
        let procs = group(1, 5, "leaker", "leaker --serve", 10_000);
        t.update(&procs, &p);
        std::thread::sleep(Duration::from_millis(5));
        let d = t.update(&procs, &p);
        assert!(d.is_empty());
    }

}
