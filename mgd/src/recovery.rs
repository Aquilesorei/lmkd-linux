
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


fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}
pub fn run(
    frozen: Arc<Mutex<FrozenRegistry>>,
    checkpointed: Arc<Mutex<CheckpointRegistry>>,
    log: Arc<Logger>,
    wake: Arc<(Mutex<bool>, Condvar)>,
) {
    let mut baseline = HealthBaseline::new();

    loop {
        if crate::should_shutdown() { return; }

        wait_for_work(&frozen, &checkpointed, &wake);
        if crate::should_shutdown() { return; }

        let pressure = match monitor::psi::read_pressure() {
            Ok(p) => p,
            Err(_) => { thread::sleep(Duration::from_secs(3)); continue; }
        };
        if monitor::psi::pressure_level(&pressure) != monitor::psi::PressureLevel::Normal {
            thread::sleep(Duration::from_secs(3));
            continue;
        }

        let meminfo = crate::monitor::meminfo::read_meminfo();
        baseline.observe(meminfo.available_kb);
        let now = unix_now();

        unfreeze_pass(&frozen, &baseline, &meminfo, now, &log);

        let meminfo = crate::monitor::meminfo::read_meminfo();
        restore_pass(&checkpointed, &baseline, &meminfo, &log);

        thread::sleep(Duration::from_secs(3));
    }
}

fn wait_for_work(
    frozen: &Arc<Mutex<FrozenRegistry>>,
    checkpointed: &Arc<Mutex<CheckpointRegistry>>,
    wake: &Arc<(Mutex<bool>, Condvar)>,
) {
    let frozen_empty = frozen.lock().unwrap().count() == 0;
    let cp_empty = checkpointed.lock().unwrap().count() == 0;
    if frozen_empty && cp_empty {
        let (lock, cvar) = &**wake;
        let mut notified = lock.lock().unwrap();
        while !*notified {
            let (guard, _) = cvar.wait_timeout(notified, Duration::from_secs(5)).unwrap();
            notified = guard;
            if crate::should_shutdown() { return; }
        }
        *notified = false;
    }
}

fn unfreeze_pass(
    frozen: &Arc<Mutex<FrozenRegistry>>,
    baseline: &HealthBaseline,
    meminfo: &crate::monitor::meminfo::MemInfo,
    now: u64,
    log: &Arc<Logger>,
) {
    let mut reg = frozen.lock().unwrap();
    if reg.count() == 0 { return; }
    mgd_common::sync_print!("\n[recovery] 🌡 Pressure normal — checking {} frozen processes", reg.count());
    let mut unfrozen_this_cycle = 0;
    for pid in reg.frozen_pids() {
        if unfrozen_this_cycle >= MAX_UNFREEZE_PER_CYCLE {
            mgd_common::sync_print!("  ⏸ Unfreeze cap reached ({MAX_UNFREEZE_PER_CYCLE}/cycle) — rest next cycle");
            break;
        }
        if !baseline.safe_to_restore(meminfo.available_kb, meminfo.total_kb, 0) {
            mgd_common::sync_print!("  ⏸ RAM near baseline — pausing unfreeze until headroom recovers");
            break;
        }
        let age = now.saturating_sub(reg.frozen_at(pid));
        if age < MIN_FREEZE_AGE_SECS {
            mgd_common::sync_print!("  ⏳ PID {pid} frozen only {age}s ago — waiting");
            continue;
        }
        let r = crate::executor::freezer::unfreeze_checked(pid, reg.start_time(pid));
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

fn restore_pass(
    checkpointed: &Arc<Mutex<CheckpointRegistry>>,
    baseline: &HealthBaseline,
    meminfo: &crate::monitor::meminfo::MemInfo,
    log: &Arc<Logger>,
) {
    let mut cp_reg = checkpointed.lock().unwrap();
    if cp_reg.count() == 0 { return; }
    mgd_common::sync_print!(
        "[recovery] 🔄 {} awaiting restore | baseline {:.0}MB (n={})",
        cp_reg.count(), baseline.baseline_mb(), baseline.samples(),
    );
    let Some((pid, name, snapshot_dir, rss_kb, attempts)) =
        cp_reg.entries_lightest_first().into_iter().next()
    else { return };

    if attempts >= MAX_RESTORE_ATTEMPTS {
        mgd_common::sync_print!("  ✗ {name} exceeded {MAX_RESTORE_ATTEMPTS} restore attempts — abandoning");
        log.log(&LogEntry::new("RESTORE_ABANDON", pid, &name, rss_kb as f64 / 1024.0, "max attempts"));
        let _ = std::fs::remove_dir_all(&snapshot_dir);
        cp_reg.remove(pid);
    } else if baseline.safe_to_restore(meminfo.available_kb, meminfo.total_kb, rss_kb) {
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
            rss_kb as f64 / 1024.0, meminfo.available_kb as f64 / 1024.0,
        );
    }
}

