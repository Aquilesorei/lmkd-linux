use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use crate::executor::registry::FrozenRegistry;
use mgd_common::output::locked_print;
use mgd_common::types::Pid;
use crate::executor;

static SHUTDOWN:      AtomicBool = AtomicBool::new(false);
static RELOAD_CONFIG: AtomicBool = AtomicBool::new(false);

pub fn should_shutdown() -> bool {
    SHUTDOWN.load(Ordering::Relaxed)
}

pub fn should_reload() -> bool {
    RELOAD_CONFIG.swap(false, Ordering::Relaxed)
}

pub fn register_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGINT,  handle_sigterm as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handle_sigterm as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP,  handle_sighup  as *const () as libc::sighandler_t);
    }
}

pub extern "C" fn handle_sigterm(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

pub extern "C" fn handle_sighup(_: libc::c_int) {
    RELOAD_CONFIG.store(true, Ordering::Relaxed);
}

/// Unfreeze all processes still in the registry after both actors have stopped.
pub fn shutdown_unfreeze(frozen: &Arc<Mutex<FrozenRegistry>>) {
    let reg = frozen.lock().unwrap();
    let entries: Vec<(Pid, u64)> = reg.frozen_pids().into_iter()
        .map(|pid| (pid, reg.start_time(pid)))
        .collect();
    drop(reg); // release lock before I/O

    if entries.is_empty() { return; }

    locked_print("\n[shutdown] Unfreezing all frozen processes...");
    for (pid, st) in &entries {
        let r = executor::freezer::unfreeze_checked(*pid, *st);
        if r.success {
            mgd_common::sync_print!("  ✓ Unfroze PID {pid}");
        } else {
            mgd_common::sync_eprint!("  ✗ PID {pid}: {}", r.error.unwrap_or_default());
        }
    }
    locked_print("[shutdown] Done.");
}