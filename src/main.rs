mod config;
mod error;
mod monitor;
mod engine;
mod executor;
mod logger;
mod output;
mod responder;
mod recovery;
mod util;

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use executor::registry::{FrozenRegistry, CheckpointRegistry};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

pub fn should_shutdown() -> bool {
    SHUTDOWN.load(Ordering::Relaxed)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 3 {
        let pid: u32 = match args[2].parse() {
            Ok(p) => p,
            Err(_) => { eprintln!("Error: PID must be a number"); return; }
        };
        match args[1].as_str() {
            "freeze" => {
                let r = executor::freezer::freeze(pid);
                if r.success { println!("✓ Frozen PID {pid}"); }
                else { eprintln!("✗ Failed: {}", r.error.unwrap_or_default()); }
            }
            "unfreeze" => {
                let r = executor::freezer::unfreeze(pid);
                if r.success { println!("✓ Unfrozen PID {pid}"); }
                else { eprintln!("✗ Failed: {}", r.error.unwrap_or_default()); }
            }
            _ => eprintln!("Usage:\n  mgd freeze <pid>\n  mgd unfreeze <pid>"),
        }
        return;
    }

    println!("Memory Guardian v0.2.0 — two-actor architecture");
    println!("  PressureResponder: 5s poll (freeze/checkpoint/kill)");
    println!("  RecoveryManager:   3s poll (unfreeze/restore)");
    println!("Press Ctrl+C to stop\n");

    let frozen = Arc::new(Mutex::new(FrozenRegistry::new()));
    let checkpointed = Arc::new(Mutex::new(CheckpointRegistry::new()));

    // Install signal handler: SIGINT sets the atomic flag.
    // AtomicBool::store with Relaxed compiles to a single `mov` on x86/ARM —
    // async-signal-safe in practice on all Linux targets.
    unsafe {
        libc::signal(libc::SIGINT, handle_sigint as *const () as libc::sighandler_t);
    }

    let f1 = Arc::clone(&frozen);
    let c1 = Arc::clone(&checkpointed);
    let responder = thread::spawn(move || responder::run(f1, c1));

    let f2 = Arc::clone(&frozen);
    let c2 = Arc::clone(&checkpointed);
    let recovery = thread::spawn(move || recovery::run(f2, c2));

    // Block until both actors exit (they check should_shutdown() each iteration)
    let _ = responder.join();
    let _ = recovery.join();

    // Final unfreeze sweep — no race: both actors are done, no new freezes possible
    shutdown_unfreeze(&frozen);
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

extern "C" fn handle_sigint(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}
