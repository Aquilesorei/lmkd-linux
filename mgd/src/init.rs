use crate::executor::registry::{CheckpointRegistry, FrozenRegistry};
use crate::{config, events, executor, leak_guard, lifecycle, maintenance, memlock, plugin_server, spike_mode, throttle};
use mgd_common::logger::Logger;
use mgd_common::types::Pid;

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};

pub struct AppState {
    pub frozen: Arc<Mutex<FrozenRegistry>>,
    pub checkpointed: Arc<Mutex<CheckpointRegistry>>,
    pub calibrator: Arc<Mutex<maintenance::Calibrator>>,
    pub logger: Arc<Logger>,
    pub recovery_wake: Arc<(Mutex<bool>, Condvar)>,
    pub reclaim_wake: Arc<(Mutex<bool>, Condvar)>,
    pub throttle_snapshot: Arc<Mutex<HashMap<String, throttle::ThrottledState>>>,
    pub event_log: events::EventLog, // <- swap in the real type from new_log()
    pub spike_snapshot: Arc<Mutex<spike_mode::SpikeSnapshot>>,
    pub leak_snapshot: Arc<Mutex<leak_guard::LeakSnapshot>>,
    /// Pids an IPC `unfreeze` call wants released early from `SpikeTracker.victims`
    /// (owned solely by the evictor thread — IPC can't mutate it directly). Drained
    /// by the evictor every cycle, including PSI-timeout calm ticks.
    pub spike_release_requests: Arc<Mutex<Vec<Pid>>>,
}

pub fn initialize() -> AppState {
    memlock::try_lock_memory();

    let frozen = Arc::new(Mutex::new(FrozenRegistry::load()));
    let checkpointed = Arc::new(Mutex::new(CheckpointRegistry::load()));
    spike_mode::SpikeTracker::load_and_unfreeze_victims();

    cleanup_orphaned_snapshots(&checkpointed);
    print_startup_banner();

    plugin_server::init_plugins();

    let calibrator = Arc::new(Mutex::new(maintenance::load_calibrator()));
    let logger = Arc::new(Logger::new(config::get().log_keep));

    lifecycle::register_signal_handlers();

    AppState {
        frozen,
        checkpointed,
        calibrator,
        logger,
        recovery_wake: Arc::new((Mutex::new(false), Condvar::new())),
        reclaim_wake: Arc::new((Mutex::new(false), Condvar::new())),
        throttle_snapshot: Arc::new(Mutex::new(HashMap::new())),
        event_log: events::new_log(),
        spike_snapshot: Arc::new(Mutex::new(spike_mode::SpikeSnapshot { active: vec![], victims: vec![] })),
        leak_snapshot: Arc::new(Mutex::new(leak_guard::LeakSnapshot { groups: vec![] })),
        spike_release_requests: Arc::new(Mutex::new(Vec::new())),
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
            if !active_dirs.contains(&path)
                && std::fs::remove_dir_all(&path).is_ok() {
                mgd_common::sync_print!("[startup] Removed orphaned snapshot: {:?}", path);
            }
        }
    }
}


