mod config;
mod events;
mod monitor;
mod engine;
mod executor;
mod evictor;
mod recovery;
mod maintenance;
mod ipc;
mod plugin_server;
mod throttle;

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

    try_elevate_scheduler_priority();

    let frozen = Arc::new(Mutex::new(FrozenRegistry::load()));
    let checkpointed = Arc::new(Mutex::new(CheckpointRegistry::load()));

    cleanup_orphaned_snapshots(&checkpointed);
    print_startup_banner();

    plugin_server::init_plugins();

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


    let recovery_wake: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));

    // Throttle state snapshot: written by evictor, read by IPC for `mgctl list`.
    let throttle_snapshot: Arc<Mutex<std::collections::HashMap<String, throttle::ThrottledState>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));

    // Ring buffer of recent daemon actions (freeze/kill/checkpoint), readable via `mgctl events`.
    let event_log = events::new_log();

    let responder = {
        let f = Arc::clone(&frozen);
        let c = Arc::clone(&checkpointed);
        let l = Arc::clone(&logger);
        let w = Arc::clone(&recovery_wake);
        let cal = Arc::clone(&calibrator);
        let ts = Arc::clone(&throttle_snapshot);
        let el = Arc::clone(&event_log);
        thread::spawn(move || evictor::run(f, c, l, w, cal, ts, el))
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
        let ts = Arc::clone(&throttle_snapshot);
        let el = Arc::clone(&event_log);
        thread::spawn(move || ipc::run_server(f, c, ts, el))
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

    match executor::checkpoint::helper_path() {
        Some(p) => println!(
            "  Checkpoint Helper:  {} (checkpoint enabled; checks permissions and runs criu)",
            p.display()
        ),
        None => println!("  Checkpoint Helper:  not found (checkpoint disabled — will SIGKILL instead)"),
    }
    println!("Press Ctrl+C to stop\n");
}

/// Remove snapshot dirs not tracked in the persisted CheckpointRegistry.
fn cleanup_orphaned_snapshots(checkpointed: &Arc<Mutex<CheckpointRegistry>>) {
    let dir = mgd_common::util::home_dir().join(".local/share/mgd/snapshots");
    let Ok(entries) = std::fs::read_dir(&dir) else { return };

    let active_dirs: std::collections::HashSet<std::path::PathBuf> = {
        let reg = checkpointed.lock().unwrap();
        reg.entries_lightest_first()
            .into_iter()
            .map(|(_, _, path, _, _)| path)
            .collect()
    };

    for entry in entries.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let path = entry.path();
            if !active_dirs.contains(&path) {
                if std::fs::remove_dir_all(&path).is_ok() {
                    locked_print(&format!("[startup] Removed orphaned snapshot: {:?}", path));
                }
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

fn try_elevate_scheduler_priority() {
    unsafe {
        let mut param = libc::sched_param { sched_priority: 20 };
        // Set policy to SCHED_RR (Real-Time Round Robin) with priority 20
        if libc::sched_setscheduler(0, libc::SCHED_RR, &mut param) == 0 {
            locked_print("[core] Successfully set scheduler policy to SCHED_RR (priority 20)");
        } else {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EPERM) {
                // If unprivileged and CAP_SYS_NICE is missing, fall back to setting highest normal priority (nice -20)
                if libc::setpriority(libc::PRIO_PROCESS, 0, -20) == 0 {
                    locked_print("[core] Set scheduler priority to nice -20 (highest normal priority)");
                } else {
                    locked_print("[core] Running with standard priority (CAP_SYS_NICE missing for RT/Nice elevation)");
                }
            } else {
                locked_print(&format!("[core] Warning: failed to set scheduler policy: {}", err));
            }
        }
    }
}
