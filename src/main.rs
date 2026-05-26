mod config;
mod monitor;
mod engine;
mod executor;
mod logger;
mod responder;
mod recovery;

use std::sync::{Arc, Mutex};
use std::thread;
use executor::registry::{FrozenRegistry, CheckpointRegistry};

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

    let f1 = Arc::clone(&frozen);
    let c1 = Arc::clone(&checkpointed);
    let responder = thread::spawn(move || responder::run(f1, c1));

    let f2 = Arc::clone(&frozen);
    let c2 = Arc::clone(&checkpointed);
    let recovery = thread::spawn(move || recovery::run(f2, c2));

    responder.join().unwrap();
    recovery.join().unwrap();
}

pub fn read_meminfo() -> (u64, u64) {
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
