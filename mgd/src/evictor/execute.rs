use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use mgd_common::logger::{LogAction, Logger};

use crate::engine::decision::{Action, Decision};
use crate::executor::ActionSink;
use crate::executor::registry::{FrozenRegistry, CheckpointRegistry};
use crate::monitor::process::Process;

#[allow(clippy::too_many_arguments)] // the sink is the 8th; bundling shared registries into a struct adds no clarity
pub(crate) fn execute_plan(
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

        crate::executor::checkpoint::mark_binary_failed(&d.name);

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

#[cfg(test)]
mod tests {
    use super::*;
    use mgd_common::types::{Kb, Pid};

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
