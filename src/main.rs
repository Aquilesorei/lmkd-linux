mod config;
mod error;
mod monitor;
mod engine;
mod executor;
mod logger;
mod output;
mod evictor;
mod recovery;
mod maintenance;
mod util;
mod ipc;

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use executor::registry::{FrozenRegistry, CheckpointRegistry};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static RELOAD_CONFIG: AtomicBool = AtomicBool::new(false);

pub fn should_shutdown() -> bool {
    SHUTDOWN.load(Ordering::Relaxed)
}

pub fn should_reload() -> bool {
    RELOAD_CONFIG.swap(false, Ordering::Relaxed)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // ── CLI subcommands (spawned separately, talk to running daemon via socket) ──
    if args.len() >= 2 {
        match args[1].as_str() {
            // Legacy direct-signal commands (still work without daemon)
            "freeze" if args.len() == 3 => {
                let pid: u32 = match args[2].parse() {
                    Ok(p) => p,
                    Err(_) => { eprintln!("Error: PID must be a number"); return; }
                };
                let r = executor::freezer::freeze(pid);
                if r.success { println!("✓ Frozen PID {pid}"); }
                else { eprintln!("✗ Failed: {}", r.error.unwrap_or_default()); }
                return;
            }
            "unfreeze" if args.len() == 3 => {
                let pid: u32 = match args[2].parse() {
                    Ok(p) => p,
                    Err(_) => { eprintln!("Error: PID must be a number"); return; }
                };
                let r = executor::freezer::unfreeze(pid);
                if r.success { println!("✓ Unfrozen PID {pid}"); }
                else { eprintln!("✗ Failed: {}", r.error.unwrap_or_default()); }
                return;
            }
            "freeze" | "unfreeze" => {
                eprintln!("Usage: mgd {} <pid>", args[1]);
                return;
            }
            other => {
                eprintln!("mgd: unknown subcommand '{other}'\nUsage: mgd freeze <pid> | mgd unfreeze <pid>");
                return;
            }
        }
    }

    cleanup_orphaned_snapshots();

    println!("Memory Guardian v0.3.0");
    println!("  PressureResponder:  5s poll (freeze/checkpoint/kill)");
    println!("  RecoveryManager:    3s poll (unfreeze/restore)");
    println!("  MaintenanceManager: 60s poll (idle reaps, housekeeping)");
    println!("  IPC socket:         {}", lmkd_linux::socket_path().display());
    match executor::checkpoint::criu_path() {
        Some(p) => println!(
            "  CRIU:               {} (checkpoint enabled; needs cap_checkpoint_restore,cap_sys_ptrace — falls back to kill if unprivileged)",
            p.display()
        ),
        None => println!("  CRIU:               not found (checkpoint disabled — will SIGKILL instead)"),
    }
    println!("Press Ctrl+C to stop\n");

    let frozen = Arc::new(Mutex::new(FrozenRegistry::new()));
    let checkpointed = Arc::new(Mutex::new(CheckpointRegistry::new()));

    // Single shared logger — both actors write to one session file (and rotation
    // runs once, not twice). Logger::log() takes &self, so Arc sharing is safe.
    let logger = Arc::new(logger::Logger::new());

    // ── Signal handlers ─────────────────────────────────────────────────────
    // All three signal handlers are async-signal-safe: they only store to
    // AtomicBool with Relaxed, which compiles to a single `mov` on x86/ARM.
    unsafe {
        libc::signal(libc::SIGINT,  handle_sigterm as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handle_sigterm as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP,  handle_sighup  as *const () as libc::sighandler_t);
    }

    // ── Actor threads ────────────────────────────────────────────────────────
    let f1 = Arc::clone(&frozen);
    let c1 = Arc::clone(&checkpointed);
    let l1 = Arc::clone(&logger);
    let responder = thread::spawn(move || evictor::run(f1, c1, l1));

    let f2 = Arc::clone(&frozen);
    let c2 = Arc::clone(&checkpointed);
    let l2 = Arc::clone(&logger);
    let recovery = thread::spawn(move || recovery::run(f2, c2, l2));

    let f3 = Arc::clone(&frozen);
    let c3 = Arc::clone(&checkpointed);
    let ipc = thread::spawn(move || ipc::run_server(f3, c3));

    let l4 = Arc::clone(&logger);
    let maintenance = thread::spawn(move || maintenance::run(l4));

    // Block until the actors exit (they check should_shutdown() each iteration)
    let _ = responder.join();
    let _ = recovery.join();
    let _ = ipc.join();
    let _ = maintenance.join();

    // Final unfreeze sweep — no race: both actors are done, no new freezes possible
    shutdown_unfreeze(&frozen);
}

/// Remove any snapshot directories left behind by a previous daemon crash.
/// CheckpointRegistry is in-memory only — orphaned snapshots would never be cleaned
/// up otherwise, leaking potentially hundreds of MB across restarts.
fn cleanup_orphaned_snapshots() {
    let dir = util::home_dir().join(".local/share/mgd/snapshots");
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for entry in entries.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if std::fs::remove_dir_all(entry.path()).is_ok() {
                output::locked_print(&format!("[startup] Removed orphaned snapshot: {:?}", entry.path()));
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

    output::locked_print("\n[shutdown] Unfreezing all frozen processes...");
    for (pid, st) in &entries {
        let r = executor::freezer::unfreeze_checked(*pid, *st);
        if r.success {
            output::locked_print(&format!("  ✓ Unfroze PID {pid}"));
        } else {
            output::locked_eprint(&format!("  ✗ PID {pid}: {}", r.error.unwrap_or_default()));
        }
    }
    output::locked_print("[shutdown] Done.");
}

/// SIGINT / SIGTERM → graceful shutdown with unfreeze sweep
extern "C" fn handle_sigterm(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

/// SIGHUP → reload config on next responder cycle
extern "C" fn handle_sighup(_: libc::c_int) {
    RELOAD_CONFIG.store(true, Ordering::Relaxed);
}
