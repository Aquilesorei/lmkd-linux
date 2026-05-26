use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::sync_print;
use crate::engine::health::HealthBaseline;
use crate::executor::registry::{FrozenRegistry, CheckpointRegistry};
use crate::logger::{LogEntry, Logger};
use crate::monitor;

const MAX_RESTORE_ATTEMPTS: u32 = 3;
const MIN_FREEZE_AGE_SECS: u64 = 15;

pub fn run(
    frozen: Arc<Mutex<FrozenRegistry>>,
    checkpointed: Arc<Mutex<CheckpointRegistry>>,
) {
    let mut baseline = HealthBaseline::new();
    let log = Logger::new();

    loop {
        if crate::should_shutdown() { return; }
        let pressure = match monitor::psi::read_pressure() {
            Ok(p) => p,
            Err(_) => {
                thread::sleep(Duration::from_secs(3));
                continue;
            }
        };

        let level = monitor::psi::pressure_level(&pressure);

        if level != monitor::psi::PressureLevel::Normal {
            thread::sleep(Duration::from_secs(3));
            continue;
        }

        let (available_kb, _total_kb) = crate::monitor::meminfo::read_meminfo();
        baseline.observe(available_kb);

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();

        // Unfreeze processes that have been frozen long enough
        {
            let mut reg = frozen.lock().unwrap();
            if reg.count() > 0 {
                sync_print!("\n[recovery] 🌡 Pressure normal — checking {} frozen processes", reg.count());
                let pids = reg.frozen_pids();
                for pid in pids {
                    let age = now.saturating_sub(reg.frozen_at(pid));
                    if age < MIN_FREEZE_AGE_SECS {
                        sync_print!("  ⏳ PID {pid} frozen only {age}s ago — waiting");
                        continue;
                    }
                    let st = reg.start_time(pid);
                    let r = crate::executor::freezer::unfreeze_checked(pid, st);
                    if r.success {
                        let name = reg.name(pid);
                        sync_print!("  ✓ Unfroze PID {pid} name={name} (frozen {age}s)");
                        log.log(&LogEntry::new("UNFREEZE", pid, name, 0.0, "unfrozen"));
                        reg.remove(pid);
                    } else {
                        sync_print!("  ✗ Unfreeze PID {pid} failed: {}", r.error.unwrap_or_default());
                    }
                }
            }
        }

        let (available_kb, total_kb) = crate::monitor::meminfo::read_meminfo();

        // Restore one checkpointed process per cycle (lightest first)
        {
            let mut cp_reg = checkpointed.lock().unwrap();
            if cp_reg.count() > 0 {
                sync_print!(
                    "[recovery] 🔄 {} awaiting restore | baseline {:.0}MB (n={})",
                    cp_reg.count(), baseline.baseline_mb(), baseline.samples(),
                );
                let candidates = cp_reg.entries_lightest_first();
                if let Some((pid, name, snapshot_dir, rss_kb, attempts)) = candidates.into_iter().next() {
                    if attempts >= MAX_RESTORE_ATTEMPTS {
                        sync_print!("  ✗ {name} exceeded {MAX_RESTORE_ATTEMPTS} restore attempts — abandoning");
                        log.log(&LogEntry::new("RESTORE_ABANDON", pid, &name, rss_kb as f64 / 1024.0, "max attempts"));
                        let _ = std::fs::remove_dir_all(&snapshot_dir);
                        cp_reg.remove(pid);
                    } else if baseline.safe_to_restore(available_kb, total_kb, rss_kb) {
                        let result = crate::executor::checkpoint::restore(&snapshot_dir);
                        if result.success {
                            sync_print!("  ✓ Restored {name} (was PID {pid}, {:.0}MB)", rss_kb as f64 / 1024.0);
                            log.log(&LogEntry::new("RESTORE", pid, &name, rss_kb as f64 / 1024.0, "restored"));
                            let _ = std::fs::remove_dir_all(&snapshot_dir);
                            cp_reg.remove(pid);
                        } else {
                            let err = result.error.unwrap_or_default();
                            sync_print!("  ✗ Restore failed for {name} (attempt {}/{MAX_RESTORE_ATTEMPTS}): {err}", attempts + 1);
                            log.log(&LogEntry::new("RESTORE_FAIL", pid, &name, rss_kb as f64 / 1024.0, &err));
                            cp_reg.increment_attempts(pid);
                        }
                    } else {
                        sync_print!(
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
