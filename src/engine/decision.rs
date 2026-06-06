use crate::monitor::process::Process;
use crate::monitor::psi::PressureLevel;

/// What the daemon will do to a process when memory pressure is detected.
/// Actions are ordered by severity — escalation goes from Freeze up to Kill.
#[derive(Debug, PartialEq)]
pub enum Action {
    /// No action needed. Pressure is below all thresholds.
    None,

    /// Send SIGSTOP to pause the process.
    /// Reversible instantly with SIGCONT.
    /// Does not free RAM directly but stops the process from allocating more,
    /// and allows the kernel to reclaim its pages if needed.
    Freeze,

    /// Use CRIU to dump the full process state to disk, then kill it.
    /// Frees all RSS immediately.
    /// Reversible — user can restore the process exactly where it left off.
    /// Only used for processes marked checkpoint=true in config.
    Checkpoint,

    /// Send SIGTERM — ask the process to shut down gracefully.
    /// Gives the process 5 seconds to clean up before SIGKILL.
    /// Not reversible — process must be relaunched manually.
    /// Used when checkpoint=false and pressure is Critical.
    Terminate,

    /// Send SIGKILL — immediate forced termination, no cleanup.
    /// Last resort. Used at Emergency pressure or when SIGTERM times out.
    /// Not reversible.
    Kill,
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Action::None       => write!(f, "NONE      "),
            Action::Freeze     => write!(f, "FREEZE    "),
            Action::Checkpoint => write!(f, "CHECKPOINT"),
            Action::Terminate  => write!(f, "TERMINATE "),
            Action::Kill       => write!(f, "KILL      "),
        }
    }
}

#[allow(dead_code)]
pub struct Decision {
    pub pid: u32,
    pub name: String,
    pub action: Action,
    pub rss_mb: f64,
    pub swap_mb: f64,
    pub reason: String,
}

/// Priority tier based on process name — delegates to loaded config.
/// Falls back to .desktop category lookup via exe_basename if no regex matches.
/// Returns 0–100 (higher = kill first).
pub fn get_priority(name: &str, exe_basename: Option<&str>) -> u8 {
    crate::config::get().priority_for(name, exe_basename)
}

/// Calculate how much RAM we need to free (in KB)
pub fn ram_deficit_kb(available_kb: u64, total_kb: u64) -> i64 {
    let target_kb = (total_kb as f64 * 0.15) as u64; // want 15% free
    target_kb as i64 - available_kb as i64
}
/// Given pressure level + process list, decide what to do (DRY RUN).
///
/// Logic:
/// 1. Calculate how much RAM we need to free (deficit)
/// 2. Sort processes by priority (least important first)
/// 3. For each candidate, decide the action based on BOTH pressure level
///    AND process-specific properties (priority, swap ratio, size)
/// 4. Stop as soon as deficit is covered — never kill more than needed
pub fn plan(
    level: &PressureLevel,
    procs: &[&Process],
    available_kb: u64,
    total_kb: u64,
) -> Vec<Decision> {
    // No pressure = no action
    if *level == PressureLevel::Normal {
        return vec![];
    }

    // How much RAM do we need to free?
    let mut deficit = ram_deficit_kb(available_kb, total_kb);
    if deficit <= 0 {
        return vec![];
    }

    // Acquire config once for the entire plan() call — avoids O(n) lock cycles.
    let cfg = crate::config::get();

    // Sort candidates: highest priority number first (least important first)
    // Filter out tiny processes — not worth the overhead.
    // Include swap in the size check: a mostly-swapped process is still reclaimable.
    let mut candidates: Vec<(u8, &Process)> = procs.iter()
        .filter(|p| p.rss_kb + p.swap_kb > 10 * 1024) // ignore processes using < 10MB total
        .map(|p| (cfg.priority_for(&p.name, p.exe_basename.as_deref()), *p))
        .collect();

    candidates.sort_by(|(pa, a), (pb, b)| {
        pb.cmp(pa)
            .then(b.rss_kb.cmp(&a.rss_kb))
            .then(b.oom_score.cmp(&a.oom_score))
    });

    let mut decisions = vec![];

    for (prio, proc) in candidates {
        if deficit <= 0 {
            break;
        }

        // Hard rule 1: never touch SYSTEM or CRITICAL tier (priority <= 19)
        if prio <= 19 {
            continue;
        }

        // Hard rule 2: never touch user-configured protect list
        if cfg.is_protected(&proc.name) {
            continue;
        }

        let total_memory = proc.rss_kb + proc.swap_kb;
        let swap_ratio = if total_memory > 0 {
            proc.swap_kb as f64 / total_memory as f64
        } else {
            0.0
        };

        // Per-process checkpoint override from config
        let checkpoint_override = cfg.checkpoint_override(&proc.name);
        let action = decide_action(level, prio, swap_ratio, checkpoint_override);

        // Skip no-ops — don't waste a decision slot or count toward deficit
        if action == Action::None {
            continue;
        }

        let reason = format!(
            "priority={prio} rss={:.0}MB swap={:.0}MB swap_ratio={:.0}% deficit={:.0}MB",
            proc.rss_kb as f64 / 1024.0,
            proc.swap_kb as f64 / 1024.0,
            swap_ratio * 100.0,
            deficit as f64 / 1024.0,
        );

        // Freeze frees no RAM directly — SIGSTOP only stops the process from
        // allocating/re-faulting; any reclaim is indirect and handled by the
        // kernel later. So it must NOT count toward the deficit, otherwise we'd
        // believe memory was freed when none was. Expendable processes are still
        // frozen (to stop the bleeding); we just don't let that close the loop.
        // Kill/Terminate/Checkpoint free the full RSS within this cycle; the next
        // 5s cycle re-measures available_kb, so any terminate lag self-corrects.
        let freed = match action {
            Action::Freeze => 0,
            _ => proc.rss_kb as i64,
        };
        deficit -= freed;

        decisions.push(Decision {
            pid: proc.pid,
            name: proc.name.clone(),
            action,
            rss_mb: proc.rss_kb as f64 / 1024.0,
            swap_mb: proc.swap_kb as f64 / 1024.0,
            reason,
        });
    }

    decisions
}

/// Decide the action for a single process based on pressure level,
/// its priority tier, how much of it is already in swap, and any
/// per-process checkpoint override from config.
fn decide_action(
    level: &PressureLevel,
    prio: u8,
    swap_ratio: f64,
    checkpoint_override: Option<bool>,
) -> Action {
    match level {
        PressureLevel::Normal => Action::None,

        // Elevated: just pause background processes, don't kill anything yet
        PressureLevel::Elevated => {
            if prio >= 60 {
                Action::Freeze  // pause low/expendable tier
            } else {
                Action::None    // leave normal/high tier alone
            }
        }

        // High: freeze everything low priority, start being more aggressive
        PressureLevel::High => {
            if prio >= 80 {
                Action::Terminate   // expendable tier — just kill it
            } else if prio >= 60 {
                Action::Freeze      // low tier — pause it
            } else {
                Action::None        // normal/high tier — leave alone
            }
        }

        // Critical: start freeing real memory
        PressureLevel::Critical => {
            // Per-process override takes priority over all heuristics
            if let Some(cp) = checkpoint_override {
                return if cp { Action::Checkpoint } else {
                    if swap_ratio > 0.5 { Action::Kill } else { Action::Terminate }
                };
            }
            if swap_ratio > 0.5 {
                // Already mostly in swap — checkpointing is pointless,
                // the data is already on disk effectively. Just kill it.
                Action::Kill
            } else if prio >= 75 {
                // Low/expendable tier — not worth saving, terminate
                Action::Terminate
            } else {
                // Normal tier with real RAM usage — checkpoint to preserve state
                Action::Checkpoint
            }
        }

        // Emergency: no time for grace, kill everything non-critical
        PressureLevel::Emergency => Action::Kill,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(name: &str, rss_kb: u64, swap_kb: u64) -> Process {
        Process { pid: 1000, name: name.to_string(), exe_basename: None, rss_kb, swap_kb, oom_score: 0 }
    }

    #[test]
    fn normal_pressure_produces_no_decisions() {
        let procs = vec![proc("firefox", 500_000, 0)];
        let decisions = plan(&PressureLevel::Normal, &procs.iter().collect::<Vec<_>>(), 4_000_000, 16_000_000);
        assert!(decisions.is_empty());
    }

    #[test]
    fn elevated_freezes_low_priority_only() {
        let procs = vec![proc("baloo_file_extractor", 200_000, 0)];
        let decisions = plan(&PressureLevel::Elevated, &procs.iter().collect::<Vec<_>>(), 1_000_000, 16_000_000);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].action, Action::Freeze);
    }

    #[test]
    fn elevated_skips_normal_tier_entirely() {
        // "some_app" default priority 50 — no decision emitted at all
        let procs = vec![proc("some_app", 500_000, 0)];
        let decisions = plan(&PressureLevel::Elevated, &procs.iter().collect::<Vec<_>>(), 1_000_000, 16_000_000);
        assert!(decisions.is_empty());
    }

    #[test]
    fn critical_never_touches_system_tier() {
        let procs = vec![proc("kwin_wayland", 300_000, 0)];
        let decisions = plan(&PressureLevel::Critical, &procs.iter().collect::<Vec<_>>(), 500_000, 16_000_000);
        assert!(decisions.is_empty());
    }

    #[test]
    fn critical_kills_high_swap_ratio() {
        let procs = vec![proc("msedge", 100_000, 200_000)];
        let decisions = plan(&PressureLevel::Critical, &procs.iter().collect::<Vec<_>>(), 500_000, 16_000_000);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].action, Action::Kill);
    }

    #[test]
    fn emergency_kills_everything_non_critical() {
        let procs = vec![proc("firefox", 500_000, 0)];
        let decisions = plan(&PressureLevel::Emergency, &procs.iter().collect::<Vec<_>>(), 500_000, 16_000_000);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].action, Action::Kill);
    }

    #[test]
    fn stops_when_deficit_covered() {
        let procs = vec![
            proc("msedge", 2_000_000, 0),
            proc("msedge", 2_000_000, 0),
        ];
        // deficit = 16M*0.15 - 1M = 1.4M KB. First kill (2M) covers it.
        let decisions = plan(&PressureLevel::Emergency, &procs.iter().collect::<Vec<_>>(), 1_000_000, 16_000_000);
        assert_eq!(decisions.len(), 1);
    }

    #[test]
    fn freeze_does_not_count_toward_deficit() {
        // Freeze frees no RAM, so it never reduces the deficit. Even a tiny
        // deficit must not stop us short: ALL expendable processes get frozen
        // ("stop the bleeding"), and the next cycle re-measures whether PSI dropped.
        let procs = vec![
            proc("baloo_file_extractor", 200_000, 0),
            proc("baloo_file_extractor", 200_000, 0),
            proc("baloo_file_extractor", 200_000, 0),
            proc("baloo_file_extractor", 200_000, 0),
        ];
        // available just 1MB below target → tiny deficit a single 200MB process
        // would have "covered" under the old 25% credit. All 4 must still freeze.
        let target = 16_000_000 * 15 / 100;
        let decisions = plan(&PressureLevel::Elevated, &procs.iter().collect::<Vec<_>>(), target as u64 - 1024, 16_000_000);
        assert_eq!(decisions.len(), 4);
        assert!(decisions.iter().all(|d| d.action == Action::Freeze));
    }

    #[test]
    fn ignores_tiny_processes() {
        let procs = vec![proc("tiny", 5_000, 0)];
        let decisions = plan(&PressureLevel::Emergency, &procs.iter().collect::<Vec<_>>(), 500_000, 16_000_000);
        assert!(decisions.is_empty());
    }

    #[test]
    fn ram_deficit_positive_when_low() {
        let deficit = ram_deficit_kb(1_000_000, 16_000_000);
        assert!(deficit > 0);
    }

    #[test]
    fn ram_deficit_negative_when_plenty() {
        let deficit = ram_deficit_kb(8_000_000, 16_000_000);
        assert!(deficit < 0);
    }
}
