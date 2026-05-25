mod monitor;
mod engine;
mod executor;
mod logger;

use std::thread;
use std::time::Duration;
use engine::decision::Action;
use executor::registry::FrozenRegistry;
use crate::logger::{LogEntry, Logger};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 3 {
        let pid: u32 = match args[2].parse() {
            Ok(p) => p,
            Err(_) => { eprintln!("Error: PID must be a number"); return; }
        };
        match args[1].as_str() {
            "freeze" => {
                let result = executor::freezer::freeze(pid);
                if result.success {
                    println!("✓ Frozen PID {pid} — resume with: mgd unfreeze {pid}");
                } else {
                    eprintln!("✗ Failed: {}", result.error.unwrap());
                }
            }
            "unfreeze" => {
                let result = executor::freezer::unfreeze(pid);
                if result.success {
                    println!("✓ Unfrozen PID {pid}");
                } else {
                    eprintln!("✗ Failed: {}", result.error.unwrap());
                }
            }
            _ => eprintln!("Usage:\n  mgd freeze <pid>\n  mgd unfreeze <pid>"),
        }
        return;
    }

    println!("Memory Guardian v0.1.0");
    println!("Press Ctrl+C to stop\n");

    // Registry tracks what we froze so we can unfreeze later
    let mut registry = FrozenRegistry::new();
    let log = Logger::new();
    loop {
        let pressure = match monitor::psi::read_pressure() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("PSI error: {e}");
                thread::sleep(Duration::from_millis(2000));
                continue;
            }
        };

        let level = monitor::psi::pressure_level(&pressure);
        let (available_kb, total_kb) = read_meminfo();

        println!(
            "[{level}] some avg10={:.2}% avg60={:.2}% | full avg10={:.2}%",
            pressure.some_avg10,
            pressure.some_avg60,
            pressure.full_avg10,
        );

        let mut procs = monitor::process::list_processes();
        procs.sort_by(|a, b| b.rss_kb.cmp(&a.rss_kb));

        let (total_rss, total_swap) = procs.iter()
            .fold((0u64, 0u64), |(r, s), p| (r + p.rss_kb, s + p.swap_kb));

        println!(
            "RAM: {:.0}MB used / {:.0}MB total | Swap: {:.0}MB | \
             Available: {:.0}MB | Processes: {} | Frozen: {}",
            total_rss as f64 / 1024.0,
            total_kb as f64 / 1024.0,
            total_swap as f64 / 1024.0,
            available_kb as f64 / 1024.0,
            procs.len(),
            registry.count(),
        );

        println!("\n{:<8} {:<22} {:>8} {:>8} {:>5}  PRIORITY",
                 "PID", "NAME", "RSS(MB)", "SWP(MB)", "OOM");
        println!("{}", "-".repeat(65));
        for p in procs.iter().take(10) {
            let frozen_marker = if registry.is_frozen(p.pid) { " ❄" } else { "" };
            println!("{:<8} {:<22} {:>8.1} {:>8.1} {:>5}  {}{}",
                     p.pid, p.name,
                     p.rss_kb as f64 / 1024.0,
                     p.swap_kb as f64 / 1024.0,
                     p.oom_score,
                     engine::decision::default_priority(&p.name),
                     frozen_marker,
            );
        }

        // When pressure drops back to Normal, unfreeze everything
        if level == monitor::psi::PressureLevel::Normal && registry.count() > 0 {
            println!("\n🌡 Pressure dropped — unfreezing {} processes", registry.count());
            let pids = registry.frozen_pids();
            for pid in pids {
                let result = executor::freezer::unfreeze(pid);
                if result.success {
                    println!("  ✓ Unfroze PID {pid}");
                } else {
                    println!("  ✗ Failed to unfreeze PID {pid}: {}",
                             result.error.unwrap_or_default());
                }
                registry.remove(pid);
            }
        }

        // Execute plan when pressure is elevated
        let plan = engine::decision::plan(&level, &procs, available_kb, total_kb);
        if plan.is_empty() {
            println!("\n✓ No action needed.");
        } else {
            println!("\n⚡ EXECUTING actions:");
            println!("{:<10} {:<8} {:<22} {:>8}  RESULT",
                     "ACTION", "PID", "NAME", "RSS(MB)");
            println!("{}", "-".repeat(70));

            for d in &plan {
                // Don't re-freeze already frozen processes
                if registry.is_frozen(d.pid) {
                    continue;
                }

                let result_str = match d.action {
                    Action::Freeze => {
                        let r = executor::freezer::freeze(d.pid);
                        let result = if r.success {
                            registry.add(d.pid, &d.name);
                            "frozen".to_string()
                        } else {
                            format!("freeze_failed: {}", r.error.unwrap_or_default())
                        };
                        log.log(&LogEntry::new("FREEZE", d.pid, &d.name, d.rss_mb, &result));
                        format!("✓ {result}")
                    }
                    Action::Terminate => {
                        let r = executor::killer::terminate(d.pid);
                        let result = if r.success {
                            "terminated".to_string()
                        } else {
                            format!("terminate_failed: {}", r.error.unwrap_or_default())
                        };
                        log.log(&LogEntry::new("TERMINATE", d.pid, &d.name, d.rss_mb, &result));
                        format!("✓ {result}")
                    }
                    Action::Kill => {
                        let r = executor::killer::kill(d.pid);
                        let result = if r.success {
                            "killed".to_string()
                        } else {
                            format!("kill_failed: {}", r.error.unwrap_or_default())
                        };
                        log.log(&LogEntry::new("KILL", d.pid, &d.name, d.rss_mb, &result));
                        format!("✓ {result}")
                    }
                    Action::Checkpoint => {
                        let r = executor::checkpoint::checkpoint(d.pid, &d.name);
                        let result = if r.success {
                            format!("checkpointed → {:?}", r.snapshot_dir.unwrap())
                        } else {
                            let kr = executor::killer::kill(d.pid);
                            if kr.success {
                                "killed (CRIU failed)".to_string()
                            } else {
                                format!("kill_failed: {}", kr.error.unwrap_or_default())
                            }
                        };
                        log.log(&LogEntry::new("CHECKPOINT", d.pid, &d.name, d.rss_mb, &result));
                        result
                    }
                    Action::None => "skipped".to_string(),
                };

                println!("{:<10} {:<8} {:<22} {:>8.1}  {}",
                         d.action, d.pid, d.name, d.rss_mb, result_str);
            }
        }

        println!();
        thread::sleep(Duration::from_millis(5000));
    }
}

fn read_meminfo() -> (u64, u64) {
    let Ok(content) = std::fs::read_to_string("/proc/meminfo") else {
        return (0, 0);
    };
    let mut total = 0u64;
    let mut available = 0u64;
    for line in content.lines() {
        if line.starts_with("MemTotal:") {
            total = line.split_whitespace().nth(1)
                .and_then(|s| s.parse().ok()).unwrap_or(0);
        }
        if line.starts_with("MemAvailable:") {
            available = line.split_whitespace().nth(1)
                .and_then(|s| s.parse().ok()).unwrap_or(0);
        }
    }
    (available, total)
}
