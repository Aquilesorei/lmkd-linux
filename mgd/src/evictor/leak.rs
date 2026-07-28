use std::sync::{Arc, Mutex};
use mgd_common::logger::{LogAction, Logger};

use crate::config::CompiledConfig;
use crate::engine::decision::get_priority;
use crate::monitor::process::Process;

pub(crate) fn run_leak_guard_cycle(
    cfg: &CompiledConfig,
    leak_tracker: &mut crate::leak_guard::LeakTracker,
    log: &Logger,
    event_log: &crate::events::EventLog,
    leak_snapshot: &Arc<Mutex<crate::leak_guard::LeakSnapshot>>,
    procs: &[Process],
) {
    if !cfg.leak_guard_enabled {
        *leak_snapshot.lock().unwrap() = leak_tracker.snapshot();
        return;
    }

    let candidates: Vec<Process> = procs.iter()
        .filter(|p| get_priority(&p.name, p.exe_basename.as_deref(), cfg) > 19)
        .filter(|p| !cfg.is_protected(&p.name))
        .cloned()
        .collect();

    let decisions = leak_tracker.update(&candidates, &crate::leak_guard::Params::from_config(cfg));
    for decision in decisions {
        let crate::leak_guard::LeakDecision::TerminateStale { pid, name, group } = decision;
        std::thread::spawn(move || { crate::executor::killer::sigterm(pid); });
        let detail = format!("leaked process family '{group}' — stale instance reaped");
        mgd_common::sync_print!("[leak_guard] Terminating {} (PID {}): {}", name, pid, detail);
        log.log(LogAction::LeakGuardKill, pid, &name, 0.0, &detail);
        crate::events::push(event_log, LogAction::LeakGuardKill, pid, &name, &detail);
    }
    *leak_snapshot.lock().unwrap() = leak_tracker.snapshot();
}
