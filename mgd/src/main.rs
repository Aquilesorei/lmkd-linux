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
mod spike_mode;
mod leak_guard;
mod lifecycle;
mod memlock;
mod init;

use std::sync::Arc;
use std::thread;
use mgd_common::types::Pid;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if handle_legacy_cli(&args) {
        return;
    }

    let state = init::initialize();

    let pressure_responder = {
        let f = Arc::clone(&state.frozen);
        let c = Arc::clone(&state.checkpointed);
        let l = Arc::clone(&state.logger);
        let w = Arc::clone(&state.recovery_wake);
        let rw = Arc::clone(&state.reclaim_wake);
        let cal = Arc::clone(&state.calibrator);
        let ts = Arc::clone(&state.throttle_snapshot);
        let el = Arc::clone(&state.event_log);
        let ss = Arc::clone(&state.spike_snapshot);
        let ls = Arc::clone(&state.leak_snapshot);
        thread::spawn(move || evictor::run(f, c, l, w, rw, cal, ts, el, ss, ls))
    };

    let recovery_manager = {
        let f = Arc::clone(&state.frozen);
        let c = Arc::clone(&state.checkpointed);
        let l = Arc::clone(&state.logger);
        let w = Arc::clone(&state.recovery_wake);
        thread::spawn(move || recovery::run(f, c, l, w))
    };

    let ipc_server = {
        let f = Arc::clone(&state.frozen);
        let c = Arc::clone(&state.checkpointed);
        let ts = Arc::clone(&state.throttle_snapshot);
        let el = Arc::clone(&state.event_log);
        let ss = Arc::clone(&state.spike_snapshot);
        let ls = Arc::clone(&state.leak_snapshot);
        thread::spawn(move || ipc::run_server(f, c, ts, el, ss, ls))
    };

    let maintenance_manager = {
        let l = Arc::clone(&state.logger);
        let f = Arc::clone(&state.frozen);
        let c = Arc::clone(&state.checkpointed);
        let cal = Arc::clone(&state.calibrator);
        let rw = Arc::clone(&state.reclaim_wake);
        thread::spawn(move || maintenance::run(l, f, c, cal, rw))
    };

    let _ = pressure_responder.join();
    let _ = recovery_manager.join();
    let _ = ipc_server.join();
    let _ = maintenance_manager.join();

    plugin_server::shutdown_plugins();

    lifecycle::shutdown_unfreeze(&state.frozen);

    maintenance::flush_calibration(&state.calibrator, &state.logger, &config::get().psi);
}

fn handle_legacy_cli(args: &[String]) -> bool {
    if args.len() < 2 {
        return false;
    }
    match args[1].as_str() {
        "freeze" if args.len() == 3 => {
            let pid: Pid = match args[2].parse() {
                Ok(p) => Pid(p),
                Err(_) => { eprintln!("Error: PID must be a number"); return true; }
            };
            let r = executor::freezer::freeze(pid);
            if r.success { println!("✓ Frozen PID {pid}"); }
            else { eprintln!("✗ Failed: {}", r.error.unwrap_or_default()); }
            true
        }
        "unfreeze" if args.len() == 3 => {
            let pid: Pid = match args[2].parse() {
                Ok(p) => Pid(p),
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