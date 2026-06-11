
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};


use crate::engine::health::HealthBaseline;
use crate::executor::registry::{FrozenRegistry, CheckpointRegistry};
use mgd_common::logger::{LogEntry, Logger};
use crate::monitor;

const MAX_RESTORE_ATTEMPTS: u32 = 3;
const MIN_FREEZE_AGE_SECS: u64 = 15;
/// Max processes to unfreeze per recovery cycle. Staggering (vs. releasing all
/// at once) lets PSI react between batches and avoids bouncing back into pressure.
const MAX_UNFREEZE_PER_CYCLE: usize = 4;

pub fn run(
    frozen: Arc<Mutex<FrozenRegistry>>,
    checkpointed: Arc<Mutex<CheckpointRegistry>>,
    log: Arc<Logger>,
    wake: Arc<(Mutex<bool>, Condvar)>,
) {
    let mut baseline = HealthBaseline::new();

    loop {
        if crate::should_shutdown() { return; }

        // Sleep until evictor rings the doorbell (or 5s timeout for shutdown check).
        // If a signal was already sent before we got here, `notified` is true
        // and we skip the wait entirely — no missed wakeups.
        {
            let frozen_empty = frozen.lock().unwrap().count() == 0;
            let cp_empty    = checkpointed.lock().unwrap().count() == 0;
            if frozen_empty && cp_empty {
                let (lock, cvar) = &*wake;
                let mut notified = lock.lock().unwrap();
                while !*notified {
                    let (guard, _) = cvar.wait_timeout(notified, Duration::from_secs(5)).unwrap();
                    notified = guard;
                    if crate::should_shutdown() { return; }
                }
                *notified = false;
            }
        }

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

        let meminfo = crate::monitor::meminfo::read_meminfo();
        baseline.observe(meminfo.available_kb);

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();

        // Unfreeze processes that have been frozen long enough
        {
            let mut reg = frozen.lock().unwrap();
            if reg.count() > 0 {
                mgd_common::sync_print!("\n[recovery] 🌡 Pressure normal — checking {} frozen processes", reg.count());
                let pids = reg.frozen_pids();
                let mut unfrozen_this_cycle = 0;
                for pid in pids {
                    if unfrozen_this_cycle >= MAX_UNFREEZE_PER_CYCLE {
                        mgd_common::sync_print!("  ⏸ Unfreeze cap reached ({MAX_UNFREEZE_PER_CYCLE}/cycle) — rest next cycle");
                        break;
                    }
                    // Headroom gate: only release more if RAM stays above baseline.
                    // Stops a barely-recovered system from oscillating back into pressure.
                    if !baseline.safe_to_restore(meminfo.available_kb, meminfo.total_kb, 0) {
                        mgd_common::sync_print!("  ⏸ RAM near baseline — pausing unfreeze until headroom recovers");
                        break;
                    }
                    let age = now.saturating_sub(reg.frozen_at(pid));
                    if age < MIN_FREEZE_AGE_SECS {
                        mgd_common::sync_print!("  ⏳ PID {pid} frozen only {age}s ago — waiting");
                        continue;
                    }
                    let st = reg.start_time(pid);
                    let r = crate::executor::freezer::unfreeze_checked(pid, st);
                    if r.success {
                        let name = reg.name(pid);
                        mgd_common::sync_print!("  ✓ Unfroze PID {pid} name={name} (frozen {age}s)");
                        log.log(&LogEntry::new("UNFREEZE", pid, name, 0.0, "unfrozen"));
                        reg.remove(pid);
                        unfrozen_this_cycle += 1;
                    } else {
                        mgd_common::sync_print!("  ✗ Unfreeze PID {pid} failed: {}", r.error.unwrap_or_default());
                    }
                }
            }
        }

        let meminfo = crate::monitor::meminfo::read_meminfo();
        let (available_kb, total_kb) = (meminfo.available_kb, meminfo.total_kb);

        // Restore one checkpointed process per cycle (lightest first)
        {
            let mut cp_reg = checkpointed.lock().unwrap();
            if cp_reg.count() > 0 {
                mgd_common::sync_print!(
                    "[recovery] 🔄 {} awaiting restore | baseline {:.0}MB (n={})",
                    cp_reg.count(), baseline.baseline_mb(), baseline.samples(),
                );
                let candidates = cp_reg.entries_lightest_first();
                if let Some((pid, name, snapshot_dir, rss_kb, attempts)) = candidates.into_iter().next() {
                    if attempts >= MAX_RESTORE_ATTEMPTS {
                        mgd_common::sync_print!("  ✗ {name} exceeded {MAX_RESTORE_ATTEMPTS} restore attempts — abandoning");
                        log.log(&LogEntry::new("RESTORE_ABANDON", pid, &name, rss_kb as f64 / 1024.0, "max attempts"));
                        let _ = std::fs::remove_dir_all(&snapshot_dir);
                        cp_reg.remove(pid);
                    } else if baseline.safe_to_restore(available_kb, total_kb, rss_kb) {
                        let result = crate::executor::checkpoint::restore(&snapshot_dir);
                        if result.success {
                            mgd_common::sync_print!("  ✓ Restored {name} (was PID {pid}, {:.0}MB)", rss_kb as f64 / 1024.0);
                            log.log(&LogEntry::new("RESTORE", pid, &name, rss_kb as f64 / 1024.0, "restored"));
                            let _ = std::fs::remove_dir_all(&snapshot_dir);
                            cp_reg.remove(pid);
                        } else {
                            let err = result.error.unwrap_or_default();
                            mgd_common::sync_print!("  ✗ Restore failed for {name} (attempt {}/{MAX_RESTORE_ATTEMPTS}): {err}", attempts + 1);
                            log.log(&LogEntry::new("RESTORE_FAIL", pid, &name, rss_kb as f64 / 1024.0, &err));
                            cp_reg.increment_attempts(pid);
                        }
                    } else {
                        mgd_common::sync_print!(
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
