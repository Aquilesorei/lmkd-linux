use crate::monitor::process::Process;
use crate::monitor::psi::PressureLevel;

/// Action for a process under pressure, ordered by severity.
#[derive(Debug, PartialEq)]
pub enum Action {
    None,

    /// SIGSTOP. Reversible via SIGCONT. Frees no RAM directly — only stops
    /// further allocation; reclaim is left to the kernel.
    Freeze,

    /// CRIU dump to disk, then kill. Frees RSS; restorable. checkpoint=true only.
    Checkpoint,

    /// SIGTERM, then SIGKILL after a 5s grace. Not restorable.
    Terminate,

    /// SIGKILL. Last resort: Emergency, or SIGTERM timeout.
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

/// Priority 0-100 (higher = sacrifice first), from config (`[[apps]]` regex,
/// then `.desktop` category by exe_basename, then default).
pub fn get_priority(name: &str, exe_basename: Option<&str>) -> u8 {
    crate::config::get().priority_for(name, exe_basename)
}

/// KB to free to reach the configured free-RAM target.
/// The target percentage is RAM-scaled by default (see config::ram_scaled_target_pct),
/// and can be overridden via [thresholds] in priorities.toml or by mgctl calibrate.
/// Returns negative if already above the target (no action needed).
pub fn ram_deficit_kb(available_kb: u64, total_kb: u64) -> i64 {
    let pct = crate::config::get().target_available_pct / 100.0;
    let target_kb = (total_kb as f64 * pct) as u64;
    target_kb as i64 - available_kb as i64
}

/// Decide actions for the current pressure level (dry run — no side effects).
/// Walks candidates least-important-first, stopping once the deficit is covered.
pub fn plan(
    level: &PressureLevel,
    procs: &[&Process],
    available_kb: u64,
    total_kb: u64,
) -> Vec<Decision> {
    if *level == PressureLevel::Normal {
        return vec![];
    }

    let mut deficit = ram_deficit_kb(available_kb, total_kb);
    if deficit <= 0 {
        return vec![];
    }

    // One config read for the whole call.
    let cfg = crate::config::get();

  
    let count_gpu = *level >= PressureLevel::High;

    // (priority, sort_footprint_kb, proc). gpu read once per candidate.
    let mut candidates: Vec<(u8, u64, &Process)> = procs.iter()
        .filter(|p| p.rss_kb + p.swap_kb > 10 * 1024)
        .map(|p| {
            let prio = cfg.priority_for(&p.name, p.exe_basename.as_deref());
            let gpu_kb = if count_gpu {
                crate::plugin_server::get_gpu_kb(p.pid)
            } else {
                0
            };
            (prio, p.rss_kb.saturating_add(gpu_kb), *p)
        })
        .collect();

    candidates.sort_by(|(pa, fa, a), (pb, fb, b)| {
        pb.cmp(pa)
            .then(fb.cmp(fa)) // rss + resident GPU — sort only
            .then(b.oom_score.cmp(&a.oom_score))
    });

    let mut decisions = vec![];

    for (prio, _sort_footprint_kb, proc) in candidates {
        if deficit <= 0 {
            break;
        }

        // Never touch the system/critical tier or the protect list.
        if prio <= 19 {
            continue;
        }
        if cfg.is_protected(&proc.name) {
            continue;
        }

        let total_memory = proc.rss_kb + proc.swap_kb;
        let swap_ratio = if total_memory > 0 {
            proc.swap_kb as f64 / total_memory as f64
        } else {
            0.0
        };

        let checkpoint_override = cfg.checkpoint_override(&proc.name);
        let action = decide_action(level, prio, swap_ratio, checkpoint_override);

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

        // Freeze frees no RAM, so it doesn't count toward the deficit — expendable
        // procs are still frozen to stop the bleeding, but the loop keeps going.
        // Kill/Terminate/Checkpoint free full RSS; the next cycle re-measures.
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

/// Action for one process from pressure level, priority, swap ratio, and the
/// per-process checkpoint override.
fn decide_action(
    level: &PressureLevel,
    prio: u8,
    swap_ratio: f64,
    checkpoint_override: Option<bool>,
) -> Action {
    match level {
        PressureLevel::Normal => Action::None,

        PressureLevel::Elevated => {
            if prio >= 60 {
                Action::Freeze
            } else {
                Action::None
            }
        }

        PressureLevel::High => {
            if prio >= 80 {
                Action::Terminate
            } else if prio >= 60 {
                Action::Freeze
            } else {
                Action::None
            }
        }

        PressureLevel::Critical => {
            if let Some(cp) = checkpoint_override {
                return if cp { Action::Checkpoint } else {
                    if swap_ratio > 0.5 { Action::Kill } else { Action::Terminate }
                };
            }
            if swap_ratio > 0.5 {
                // Mostly in swap already — its data is effectively on disk, so
                // checkpointing buys nothing. Kill.
                Action::Kill
            } else if prio >= 75 {
                Action::Terminate
            } else {
                Action::Checkpoint
            }
        }

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
        let procs = vec![proc("some_app", 500_000, 0)]; // default priority 50
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
        // deficit = 16M*0.15 - 1M = 1.4M KB; first 2M kill covers it.
        let decisions = plan(&PressureLevel::Emergency, &procs.iter().collect::<Vec<_>>(), 1_000_000, 16_000_000);
        assert_eq!(decisions.len(), 1);
    }

    #[test]
    fn freeze_does_not_count_toward_deficit() {
        // Freeze credits nothing, so a tiny deficit must not stop the loop short:
        // all 4 expendable procs freeze.
        let procs = vec![
            proc("baloo_file_extractor", 200_000, 0),
            proc("baloo_file_extractor", 200_000, 0),
            proc("baloo_file_extractor", 200_000, 0),
            proc("baloo_file_extractor", 200_000, 0),
        ];
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
    fn high_pressure_deficit_credits_rss_only() {
        // Guard: GPU ranking must not leak into the deficit. Synthetic pids have
        // no GPU mem, so High behaves as RSS-only.
        let procs = vec![
            proc("msedge", 2_000_000, 0),
            proc("msedge", 2_000_000, 0),
        ];
        let decisions = plan(&PressureLevel::High, &procs.iter().collect::<Vec<_>>(), 1_000_000, 16_000_000);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].action, Action::Terminate);
        assert_eq!(decisions[0].rss_mb as u64, 1953); // 2_000_000 KB / 1024
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
