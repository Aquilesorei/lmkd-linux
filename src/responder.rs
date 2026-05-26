use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::engine::decision::{Action, plan, default_priority};
use crate::executor::registry::{FrozenRegistry, CheckpointRegistry};
use crate::logger::{LogEntry, Logger};
use crate::monitor;

pub fn run(
    frozen: Arc<Mutex<FrozenRegistry>>,
    checkpointed: Arc<Mutex<CheckpointRegistry>>,
) {
    let log = Logger::new();

    loop {
        let pressure = match monitor::psi::read_pressure() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[responder] PSI error: {e}");
                thread::sleep(Duration::from_secs(5));
                continue;
            }
        };

        let level = monitor::psi::pressure_level(&pressure);
        let (available_kb, total_kb) = crate::read_meminfo();

        let mut procs = monitor::process::list_processes();
        procs.sort_by(|a, b| b.rss_kb.cmp(&a.rss_kb));

        let (total_rss, total_swap) = procs.iter()
            .fold((0u64, 0u64), |(r, s), p| (r.saturating_add(p.rss_kb), s.saturating_add(p.swap_kb)));

        let frozen_count = frozen.lock().unwrap().count();

        println!(
            "\n[responder] [{level}] some avg10={:.2}% | RAM {:.0}/{:.0}MB | Swap {:.0}MB | Avail {:.0}MB | Procs {} | Frozen {}",
            pressure.some_avg10,
            total_rss as f64 / 1024.0,
            total_kb as f64 / 1024.0,
            total_swap as f64 / 1024.0,
            available_kb as f64 / 1024.0,
            procs.len(),
            frozen_count,
        );

        // Top processes
        println!("{:<8} {:<22} {:>8} {:>8} {:>5}  PRI", "PID", "NAME", "RSS(MB)", "SWP(MB)", "OOM");
        println!("{}", "-".repeat(65));
        {
            let reg = frozen.lock().unwrap();
            for p in procs.iter().take(10) {
                let marker = if reg.is_frozen(p.pid) { " ❄" } else { "" };
                println!("{:<8} {:<22} {:>8.1} {:>8.1} {:>5}  {}{}",
                    p.pid, p.name,
                    p.rss_kb as f64 / 1024.0,
                    p.swap_kb as f64 / 1024.0,
                    p.oom_score,
                    default_priority(&p.name),
                    marker,
                );
            }
        }

        let decisions = plan(&level, &procs, available_kb, total_kb);
        if decisions.is_empty() {
            println!("✓ No action needed.");
        } else {
            println!("⚡ EXECUTING:");
            for d in &decisions {
                if d.action == Action::None { continue; }

                // Check frozen status — lock, read, release immediately
                if frozen.lock().unwrap().is_frozen(d.pid) { continue; }

                let result_str = match d.action {
                    Action::Freeze => {
                        // Execute outside lock
                        let r = crate::executor::freezer::freeze(d.pid);
                        let s = if r.success {
                            frozen.lock().unwrap().add(d.pid, &d.name);
                            "frozen".into()
                        } else {
                            format!("fail: {}", r.error.unwrap_or_default())
                        };
                        log.log(&LogEntry::new("FREEZE", d.pid, &d.name, d.rss_mb, &s));
                        s
                    }
                    Action::Terminate => {
                        let r = crate::executor::killer::terminate(d.pid);
                        let s = if r.success { "terminated".into() }
                            else { format!("fail: {}", r.error.unwrap_or_default()) };
                        log.log(&LogEntry::new("TERMINATE", d.pid, &d.name, d.rss_mb, &s));
                        s
                    }
                    Action::Kill => {
                        let r = crate::executor::killer::kill(d.pid);
                        let s = if r.success { "killed".into() }
                            else { format!("fail: {}", r.error.unwrap_or_default()) };
                        log.log(&LogEntry::new("KILL", d.pid, &d.name, d.rss_mb, &s));
                        s
                    }
                    Action::Checkpoint => {
                        // CRIU dump can be slow — run outside any lock
                        let r = crate::executor::checkpoint::checkpoint(d.pid, &d.name);
                        let s = if r.success {
                            let dir = r.snapshot_dir.unwrap();
                            checkpointed.lock().unwrap()
                                .add(d.pid, &d.name, dir.clone(), d.rss_mb as u64 * 1024);
                            format!("checkpointed → {dir:?}")
                        } else {
                            let kr = crate::executor::killer::kill(d.pid);
                            if kr.success { "killed (CRIU failed)".into() }
                            else { format!("kill_fail: {}", kr.error.unwrap_or_default()) }
                        };
                        log.log(&LogEntry::new("CHECKPOINT", d.pid, &d.name, d.rss_mb, &s));
                        s
                    }
                    Action::None => continue,
                };

                println!("  {:<10} {:<8} {:<22} {:>6.1}MB  {}", d.action, d.pid, d.name, d.rss_mb, result_str);
            }
        }

        thread::sleep(Duration::from_secs(5));
    }
}
