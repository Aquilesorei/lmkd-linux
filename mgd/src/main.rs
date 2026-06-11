mod config;
mod monitor;
mod engine;
mod executor;
mod evictor;
mod recovery;
mod maintenance;
mod ipc;
mod plugin_server;

use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use executor::registry::{FrozenRegistry, CheckpointRegistry};
use mgd_common::logger::Logger;
use mgd_common::output::locked_print;

static SHUTDOWN:      AtomicBool = AtomicBool::new(false);
static RELOAD_CONFIG: AtomicBool = AtomicBool::new(false);

pub fn should_shutdown() -> bool {
    SHUTDOWN.load(Ordering::Relaxed)
}

pub fn should_reload() -> bool {
    RELOAD_CONFIG.swap(false, Ordering::Relaxed)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if handle_legacy_cli(&args) {
        return;
    }

    cleanup_orphaned_snapshots();
    print_startup_banner();

    plugin_server::init_plugins();

    let frozen = Arc::new(Mutex::new(FrozenRegistry::load()));
    let checkpointed = Arc::new(Mutex::new(CheckpointRegistry::load()));

    // Passive calibration aggregates survive restarts (suggestions need days
    // of observation). Maintenance flushes periodically; main flushes at exit.
    let calibrator = Arc::new(Mutex::new(maintenance::load_calibrator()));

    let log_keep = config::get().log_keep;
    let logger = Arc::new(Logger::new(log_keep));

    // Signal handlers: async-signal-safe atomic stores only.
    unsafe {
        libc::signal(libc::SIGINT,  handle_sigterm as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handle_sigterm as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP,  handle_sighup  as *const () as libc::sighandler_t);
    }

    // Doorbell: evictor rings it when it adds to frozen/checkpointed registries.
    // Recovery sleeps on it when both registries are empty.
    let recovery_wake: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));

    // ── Actor threads ─────────────────────────────────────────────────────────
    let responder = {
        let f = Arc::clone(&frozen);
        let c = Arc::clone(&checkpointed);
        let l = Arc::clone(&logger);
        let w = Arc::clone(&recovery_wake);
        let cal = Arc::clone(&calibrator);
        thread::spawn(move || evictor::run(f, c, l, w, cal))
    };

    let recovery = {
        let f = Arc::clone(&frozen);
        let c = Arc::clone(&checkpointed);
        let l = Arc::clone(&logger);
        let w = Arc::clone(&recovery_wake);
        thread::spawn(move || recovery::run(f, c, l, w))
    };

    let ipc = {
        let f = Arc::clone(&frozen);
        let c = Arc::clone(&checkpointed);
        thread::spawn(move || ipc::run_server(f, c))
    };

    let maintenance = {
        let l = Arc::clone(&logger);
        let f = Arc::clone(&frozen);
        let c = Arc::clone(&checkpointed);
        let cal = Arc::clone(&calibrator);
        thread::spawn(move || maintenance::run(l, f, c, cal))
    };

    let _ = responder.join();
    let _ = recovery.join();
    let _ = ipc.join();
    let _ = maintenance.join();

    // Actors are done — no new freezes: safe to sweep.
    shutdown_unfreeze(&frozen);

    // Persist calibration aggregates gathered since the last periodic flush.
    maintenance::flush_calibration(&calibrator, &logger);
}

/// Handle `mgd freeze <pid>` and `mgd unfreeze <pid>`.
/// Returns `true` if a CLI command was handled, `false` otherwise.
fn handle_legacy_cli(args: &[String]) -> bool {
    if args.len() < 2 {
        return false;
    }
    match args[1].as_str() {
        "freeze" if args.len() == 3 => {
            let pid: u32 = match args[2].parse() {
                Ok(p) => p,
                Err(_) => { eprintln!("Error: PID must be a number"); return true; }
            };
            let r = executor::freezer::freeze(pid);
            if r.success { println!("✓ Frozen PID {pid}"); }
            else { eprintln!("✗ Failed: {}", r.error.unwrap_or_default()); }
            true
        }
        "unfreeze" if args.len() == 3 => {
            let pid: u32 = match args[2].parse() {
                Ok(p) => p,
                Err(_) => { eprintln!("Error: PID must be a number"); return true; }
            };
            let r = executor::freezer::unfreeze(pid);
            if r.success { println!("✓ Unfrozen PID {pid}"); }
            else { eprintln!("✗ Failed: {}", r.error.unwrap_or_default()); }
            true
        }
        "freeze" | "unfreeze" => {
            eprintln!("Usage: mgd {} <pid>", args[1]);
            true
        }
        other => {
            eprintln!("mgd: unknown subcommand '{other}'\nUsage: mgd freeze <pid> | mgd unfreeze <pid>");
            true
        }
    }
}

fn print_startup_banner() {
    println!("Memory Guardian v{}", env!("CARGO_PKG_VERSION"));
    println!("  PressureResponder:  PSI epoll trigger (zero-CPU idle)");
    println!("  RecoveryManager:    condvar sleep (wakes on freeze/checkpoint)");
    println!("  MaintenanceManager: 60s poll (idle reaps, housekeeping)");
    println!("  IPC socket:         {}", mgd_common::socket::socket_path().display());

    match executor::checkpoint::criu_path() {
        Some(p) => println!(
            "  CRIU:               {} (checkpoint enabled; needs cap_checkpoint_restore,cap_sys_ptrace — falls back to kill if unprivileged)",
            p.display()
        ),
        None => println!("  CRIU:               not found (checkpoint disabled — will SIGKILL instead)"),
    }
    println!("Press Ctrl+C to stop\n");
}

/// Remove snapshot dirs left by a previous crash — the registry isn't persisted.
fn cleanup_orphaned_snapshots() {
    let dir = mgd_common::util::home_dir().join(".local/share/mgd/snapshots");
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for entry in entries.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if std::fs::remove_dir_all(entry.path()).is_ok() {
                locked_print(&format!("[startup] Removed orphaned snapshot: {:?}", entry.path()));
            }
        }
    }
}

/// Unfreeze all processes still in the registry after both actors have stopped.
fn shutdown_unfreeze(frozen: &Arc<Mutex<FrozenRegistry>>) {
    let reg = frozen.lock().unwrap();
    let entries: Vec<(u32, u64)> = reg.frozen_pids().into_iter()
        .map(|pid| (pid, reg.start_time(pid)))
        .collect();
    drop(reg); // release lock before I/O

    if entries.is_empty() { return; }

    locked_print("\n[shutdown] Unfreezing all frozen processes...");
    for (pid, st) in &entries {
        let r = executor::freezer::unfreeze_checked(*pid, *st);
        if r.success {
            locked_print(&format!("  ✓ Unfroze PID {pid}"));
        } else {
            mgd_common::output::locked_eprint(&format!("  ✗ PID {pid}: {}", r.error.unwrap_or_default()));
        }
    }
    locked_print("[shutdown] Done.");
}

/// SIGINT / SIGTERM → graceful shutdown with unfreeze sweep
extern "C" fn handle_sigterm(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

/// SIGHUP → reload config on next responder cycle
extern "C" fn handle_sighup(_: libc::c_int) {
    RELOAD_CONFIG.store(true, Ordering::Relaxed);
}
