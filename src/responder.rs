use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::sync_print;
use crate::engine::decision::{Action, Decision, plan, default_priority};
use crate::executor::registry::{FrozenRegistry, CheckpointRegistry};
use crate::logger::{LogEntry, Logger};
use crate::monitor;

pub fn run(
    frozen: Arc<Mutex<FrozenRegistry>>,
    checkpointed: Arc<Mutex<CheckpointRegistry>>,
) {
    let log = Logger::new();

    loop {
        if crate::should_shutdown() { return; }
        let pressure = match monitor::psi::read_pressure() {
            Ok(p) => p,
            Err(e) => {
                sync_print!("[responder] PSI error: {e}");
                thread::sleep(Duration::from_secs(5));
                continue;
            }
        };

        let level = monitor::psi::pressure_level(&pressure);
        let (available_kb, total_kb) = crate::monitor::meminfo::read_meminfo();

        let mut procs = monitor::process::list_processes();
        procs.sort_by_key(|p| std::cmp::Reverse(p.rss_kb));

        print_status(&pressure, &procs, available_kb, total_kb, &frozen);

        let decisions = plan(&level, &procs, available_kb, total_kb);
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

fn print_status(
    pressure: &monitor::psi::MemoryPressure,
    procs: &[monitor::process::Process],
    available_kb: u64,
    total_kb: u64,
    frozen: &Arc<Mutex<FrozenRegistry>>,
) {
    let level = monitor::psi::pressure_level(pressure);
    let (total_rss, total_swap) = procs.iter()
        .fold((0u64, 0u64), |(r, s), p| (r.saturating_add(p.rss_kb), s.saturating_add(p.swap_kb)));
    let frozen_count = frozen.lock().unwrap().count();

    sync_print!(
        "\n[responder] [{level}] some avg10={:.2}% | RAM {:.0}/{:.0}MB | Swap {:.0}MB | Avail {:.0}MB | Procs {} | Frozen {}",
        pressure.some_avg10,
        total_rss as f64 / 1024.0,
        total_kb as f64 / 1024.0,
        total_swap as f64 / 1024.0,
        available_kb as f64 / 1024.0,
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
            default_priority(&p.name),
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
    let st = crate::executor::read_start_time(d.pid);
    let r = match st {
        Some(t) => crate::executor::freezer::freeze_checked(d.pid, t),
        None => crate::executor::freezer::freeze(d.pid),
    };
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
    let r = crate::executor::killer::terminate(d.pid);
    if r.success { "terminated".into() }
    else { format!("fail: {}", r.error.unwrap_or_default()) }
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
