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
/// Returns 0–100 (higher = kill first).
pub fn default_priority(name: &str) -> u8 {
    crate::config::get().priority_for(name)
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
    procs: &[Process],
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

    // Sort candidates: highest priority number first (least important first)
    // Filter out tiny processes — not worth the overhead
    let mut candidates: Vec<&Process> = procs.iter()
        .filter(|p| p.rss_kb > 10 * 1024) // ignore processes using < 10MB
        .collect();

    candidates.sort_by(|a, b| {
        let pa = default_priority(&a.name);
        let pb = default_priority(&b.name);
        pb.cmp(&pa).then(b.rss_kb.cmp(&a.rss_kb))
    });

    let mut decisions = vec![];

    for proc in candidates {
        // Covered enough — stop here, don't kill more than needed
        if deficit <= 0 {
            break;
        }

        let prio = default_priority(&proc.name);

        // Hard rule: never touch SYSTEM or CRITICAL tier (priority <= 19)
        if prio <= 19 {
            continue;
        }

        // How much of this process is already in swap vs RAM?
        // If >50% is already in swap, checkpointing is wasteful — just kill it
        let total_memory = proc.rss_kb + proc.swap_kb;
        let swap_ratio = if total_memory > 0 {
            proc.swap_kb as f64 / total_memory as f64
        } else {
            0.0
        };

        let action = decide_action(level, prio, swap_ratio);

        let reason = format!(
            "priority={prio} rss={:.0}MB swap={:.0}MB swap_ratio={:.0}% deficit={:.0}MB",
            proc.rss_kb as f64 / 1024.0,
            proc.swap_kb as f64 / 1024.0,
            swap_ratio * 100.0,
            deficit as f64 / 1024.0,
        );

        // Reduce deficit by this process's RSS (what we'd free by killing it)
        deficit -= proc.rss_kb as i64;

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
/// its priority tier, and how much of it is already in swap.
fn decide_action(level: &PressureLevel, prio: u8, swap_ratio: f64) -> Action {
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
