use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::engine::health::HealthBaseline;
use crate::executor::registry::{FrozenRegistry, CheckpointRegistry};
use crate::logger::{LogEntry, Logger};
use crate::monitor;

const MAX_RESTORE_ATTEMPTS: u32 = 3;
// Minimum seconds a process must have been frozen before recovery unfreezes it.
// Prevents recovery from immediately undoing a freeze the responder just issued.
const MIN_FREEZE_AGE_SECS: u64 = 15;

pub fn run(
    frozen: Arc<Mutex<FrozenRegistry>>,
    checkpointed: Arc<Mutex<CheckpointRegistry>>,
) {
    let mut baseline = HealthBaseline::new();
    let log = Logger::new();

    loop {
        let pressure = match monitor::psi::read_pressure() {
            Ok(p) => p,
            Err(_) => {
                thread::sleep(Duration::from_secs(3));
                continue;
            }
        };

        let level = monitor::psi::pressure_level(&pressure);

        // Only act when pressure is Normal
        if level != monitor::psi::PressureLevel::Normal {
            thread::sleep(Duration::from_secs(3));
            continue;
        }

        let (available_kb, total_kb) = crate::read_meminfo();
        baseline.observe(available_kb);

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();

        // Unfreeze processes that have been frozen long enough (hysteresis guard)
        {
            let mut reg = frozen.lock().unwrap();
            if reg.count() > 0 {
                println!("\n[recovery] 🌡 Pressure normal — checking {} frozen processes", reg.count());
                let pids = reg.frozen_pids();
                for pid in pids {
                    let age = now.saturating_sub(reg.frozen_at(pid));
                    if age < MIN_FREEZE_AGE_SECS {
                        println!("  ⏳ PID {pid} frozen only {age}s ago — waiting");
                        continue;
                    }
                    let r = crate::executor::freezer::unfreeze(pid);
                    if r.success {
                        println!("  ✓ Unfroze PID {pid} (frozen {age}s)");
                        log.log(&LogEntry::new("UNFREEZE", pid, "", 0.0, "unfrozen"));
                        reg.remove(pid);
                    } else if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                        println!("  ~ PID {pid} gone, removing");
                        reg.remove(pid);
                    } else {
                        println!("  ✗ Unfreeze PID {pid} failed: {}", r.error.unwrap_or_default());
                    }
                }
            }
        }

        // Re-sample meminfo — unfrozen processes may have started allocating
        let (available_kb, total_kb) = crate::read_meminfo();

        // Restore one checkpointed process per cycle (lightest first)
        {
            let mut cp_reg = checkpointed.lock().unwrap();
            if cp_reg.count() > 0 {
                println!(
                    "[recovery] 🔄 {} awaiting restore | baseline {:.0}MB (n={})",
                    cp_reg.count(), baseline.baseline_mb(), baseline.samples(),
                );
                let candidates = cp_reg.entries_lightest_first();
                if let Some((pid, name, snapshot_dir, rss_kb, attempts)) = candidates.into_iter().next() {
                    if attempts >= MAX_RESTORE_ATTEMPTS {
                        println!("  ✗ {name} exceeded {MAX_RESTORE_ATTEMPTS} restore attempts — abandoning");
                        log.log(&LogEntry::new("RESTORE_ABANDON", pid, &name, rss_kb as f64 / 1024.0, "max attempts exceeded"));
                        let _ = std::fs::remove_dir_all(&snapshot_dir);
                        cp_reg.remove(pid);
                    } else if baseline.safe_to_restore(available_kb, total_kb, rss_kb) {
                        let result = crate::executor::checkpoint::restore(&snapshot_dir);
                        if result.success {
                            println!("  ✓ Restored {name} (was PID {pid}, {:.0}MB)", rss_kb as f64 / 1024.0);
                            log.log(&LogEntry::new("RESTORE", pid, &name, rss_kb as f64 / 1024.0, "restored"));
                            let _ = std::fs::remove_dir_all(&snapshot_dir);
                            cp_reg.remove(pid);
                        } else {
                            let err = result.error.unwrap_or_default();
                            println!("  ✗ Restore failed for {name} (attempt {}/{MAX_RESTORE_ATTEMPTS}): {err}", attempts + 1);
                            log.log(&LogEntry::new("RESTORE_FAIL", pid, &name, rss_kb as f64 / 1024.0, &err));
                            cp_reg.increment_attempts(pid);
                        }
                    } else {
                        println!(
                            "  ⏸ Skip {name} ({:.0}MB) — RAM tight ({:.0}MB free)",
                            rss_kb as f64 / 1024.0, available_kb as f64 / 1024.0,
                        );
                    }
                }
            }
        }

        thread::sleep(Duration::from_secs(3));
    }
}
