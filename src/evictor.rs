use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::sync_print;
use crate::engine::decision::{Action, Decision, plan, get_priority};
use crate::executor::registry::{FrozenRegistry, CheckpointRegistry};
use crate::logger::{LogEntry, Logger};
use crate::monitor;
use crate::monitor::meminfo::MemInfo;
use crate::monitor::process::Process;
use crate::monitor::psi::PressureLevel;

pub fn run(
    frozen: Arc<Mutex<FrozenRegistry>>,
    checkpointed: Arc<Mutex<CheckpointRegistry>>,
) {
    let log = Logger::new();

    loop {
        if crate::should_shutdown() { return; }

        // SIGHUP: reload config before the next decision cycle
        if crate::should_reload() {
            crate::config::reload();
        }

        let pressure = match monitor::psi::read_pressure() {
            Ok(p) => p,
            Err(e) => {
                sync_print!("[responder] PSI error: {e}");
                thread::sleep(Duration::from_secs(5));
                continue;
            }
        };

        let level = monitor::psi::pressure_level(&pressure);
        let meminfo = crate::monitor::meminfo::read_meminfo();
        let effective_level = escalate_for_swap(&level, &meminfo);

        let mut procs = monitor::process::list_processes();
        procs.sort_by_key(|p| std::cmp::Reverse(p.rss_kb));

        print_status(&pressure, &effective_level, &procs, &meminfo, &frozen);

        // Fix 1: exclude already-frozen PIDs from plan so their RSS isn't
        // double-counted toward deficit, which would cause underkill.
        let frozen_set: HashSet<u32> = frozen.lock().unwrap().frozen_pids().into_iter().collect();
        let plan_procs: Vec<&Process> = procs.iter()
            .filter(|p| !frozen_set.contains(&p.pid))
            .collect();

        let decisions = plan(&effective_level, &plan_procs, meminfo.available_kb, meminfo.total_kb);
        if decisions.is_empty() {
            sync_print!("✓ No action needed.");
        } else {
            sync_print!("⚡ EXECUTING:");
            for d in &decisions {
                if frozen.lock().unwrap().is_frozen(d.pid) { continue; }

                let result_str = execute_decision(d, &frozen, &checkpointed, &log);
                sync_print!("  {:<10} {:<8} {:<22} {:>6.1}MB  {}", d.action, d.pid, d.name, d.rss_mb, result_str);
            }
        }

        thread::sleep(Duration::from_secs(5));
    }
}

/// Fix 2: escalate pressure level when swap is nearly exhausted.
/// PSI alone misses pre-OOM state when swap fills slowly — this catches it.
fn escalate_for_swap(level: &PressureLevel, m: &MemInfo) -> PressureLevel {
    if m.swap_total_kb < 256 * 1024 || m.swap_used_pct() < 85.0 {
        return level.clone();
    }
    match level {
        PressureLevel::Normal    => PressureLevel::Elevated,
        PressureLevel::Elevated  => PressureLevel::High,
        PressureLevel::High      => PressureLevel::Critical,
        PressureLevel::Critical  => PressureLevel::Emergency,
        PressureLevel::Emergency => PressureLevel::Emergency,
    }
}

fn print_status(
    pressure: &monitor::psi::MemoryPressure,
    effective_level: &PressureLevel,
    procs: &[monitor::process::Process],
    meminfo: &MemInfo,
    frozen: &Arc<Mutex<FrozenRegistry>>,
) {
    let (total_rss, _) = procs.iter()
        .fold((0u64, 0u64), |(r, s), p| (r.saturating_add(p.rss_kb), s.saturating_add(p.swap_kb)));
    let frozen_count = frozen.lock().unwrap().count();

    sync_print!(
        "\n[responder] [{effective_level}] some avg10={:.2}% | RAM {:.0}/{:.0}MB | Swap {:.0}% | Avail {:.0}MB | Procs {} | Frozen {}",
        pressure.some_avg10,
        total_rss as f64 / 1024.0,
        meminfo.total_kb as f64 / 1024.0,
        meminfo.swap_used_pct(),
        meminfo.available_kb as f64 / 1024.0,
        procs.len(),
        frozen_count,
    );

    sync_print!("{:<8} {:<22} {:>8} {:>8} {:>5}  PRI", "PID", "NAME", "RSS(MB)", "SWP(MB)", "OOM");
    sync_print!("{}", "-".repeat(65));
    let reg = frozen.lock().unwrap();
    for p in procs.iter().take(10) {
        let marker = if reg.is_frozen(p.pid) { " ❄" } else { "" };
        sync_print!("{:<8} {:<22} {:>8.1} {:>8.1} {:>5}  {}{}",
            p.pid, p.name,
            p.rss_kb as f64 / 1024.0,
            p.swap_kb as f64 / 1024.0,
            p.oom_score,
            get_priority(&p.name),
            marker,
        );
    }
}

fn execute_decision(
    d: &Decision,
    frozen: &Arc<Mutex<FrozenRegistry>>,
    checkpointed: &Arc<Mutex<CheckpointRegistry>>,
    log: &Logger,
) -> String {
    let (action_name, s) = match d.action {
        Action::Freeze => ("FREEZE", execute_freeze(d, frozen)),
        Action::Terminate => ("TERMINATE", execute_terminate(d)),
        Action::Kill => ("KILL", execute_kill(d)),
        Action::Checkpoint => ("CHECKPOINT", execute_checkpoint(d, checkpointed)),
        Action::None => return String::new(),
    };
    log.log(&LogEntry::new(action_name, d.pid, &d.name, d.rss_mb, &s));
    s
}

fn execute_freeze(d: &Decision, frozen: &Arc<Mutex<FrozenRegistry>>) -> String {
    // Read start_time first — if it's gone, abort rather than freeze a recycled PID.
    let st = match crate::executor::read_start_time(d.pid) {
        Some(t) => t,
        None => return "aborted: process vanished before freeze".into(),
    };
    let r = crate::executor::freezer::freeze_checked(d.pid, st);
    if r.success {
        if frozen.lock().unwrap().add(d.pid, &d.name) {
            "frozen".into()
        } else {
            crate::executor::freezer::unfreeze(d.pid);
            "aborted: process vanished before fingerprint".into()
        }
    } else {
        format!("fail: {}", r.error.unwrap_or_default())
    }
}

fn execute_terminate(d: &Decision) -> String {
    // Spawn the blocking SIGTERM→wait→SIGKILL sequence on a background thread so
    // the responder isn't stalled for up to 5s per process at Critical pressure.
    let pid = d.pid;
    std::thread::spawn(move || { crate::executor::killer::terminate(pid); });
    "terminating (async SIGTERM→SIGKILL)".into()
}

fn execute_kill(d: &Decision) -> String {
    let r = crate::executor::killer::kill(d.pid);
    if r.success { "killed".into() }
    else { format!("fail: {}", r.error.unwrap_or_default()) }
}

fn execute_checkpoint(d: &Decision, checkpointed: &Arc<Mutex<CheckpointRegistry>>) -> String {
    let r = crate::executor::checkpoint::checkpoint(d.pid, &d.name);
    if r.success {
        let dir = r.snapshot_dir.unwrap();
        checkpointed.lock().unwrap()
            .add(d.pid, &d.name, dir.clone(), d.rss_mb as u64 * 1024);
        format!("checkpointed → {dir:?}")
    } else {
        let kr = crate::executor::killer::kill(d.pid);
        if kr.success { "killed (CRIU failed)".into() }
        else { format!("kill_fail: {}", kr.error.unwrap_or_default()) }
    }
}
