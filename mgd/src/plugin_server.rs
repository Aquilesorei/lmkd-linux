use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, LazyLock};
use std::sync::mpsc::{channel, Sender};
use std::collections::HashMap;
use std::thread;

use mgd_common::protocol::{CoreMessage, PluginMessage, PluginAction};

use crate::executor::registry::{FrozenRegistry, CheckpointRegistry};

/// Senders to all active plugin connections
static PLUGIN_CLIENTS: LazyLock<Mutex<Vec<Sender<CoreMessage>>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Cached observations from plugins
static GPU_CACHE: LazyLock<Mutex<HashMap<u32, u64>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn get_gpu_kb(pid: u32) -> u64 {
    *GPU_CACHE.lock().unwrap().get(&pid).unwrap_or(&0)
}

pub fn set_gpu_kb(pid: u32, kb: u64) {
    GPU_CACHE.lock().unwrap().insert(pid, kb);
}

/// Detect environment and spawn appropriate plugins.
pub fn init_plugins() {
    // 1. Detect Desktop Environment
    init_de_environment();

    // 2. Detect GPU Driver
   init_gpu_driver();
}


fn init_de_environment(){
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
    if desktop.contains("kde") {
        spawn_plugin_binary("mgd-kde");
    } else if desktop.contains("gnome") {
        spawn_plugin_binary("mgd-gnome");
    } else if desktop.contains("cosmic") {
        spawn_plugin_binary("mgd-cosmic");
    }
}

fn init_gpu_driver(){
    if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path().join("device/driver");
            if let Ok(target) = std::fs::read_link(&path) {
                let driver = target.file_name().unwrap_or_default().to_string_lossy();
                if driver == "i915" || driver == "xe" {
                    spawn_plugin_binary("mgd-gpu-intel");
                    break;
                } else if driver == "amdgpu" {
                    spawn_plugin_binary("mgd-gpu-amd");
                    break;
                }
            }
        }
    }
}
fn spawn_plugin_binary(name: &str) {
    // Check next to the current mgd executable first (good for dev)
    let exe = std::env::current_exe().unwrap_or_default();
    let dir = exe.parent().unwrap_or(std::path::Path::new("."));
    let mut candidate = dir.join(name);
    
    if !candidate.exists() {
        // Fallback to ~/.local/bin/ which is where install.sh places them
        if let Some(home) = std::env::var_os("HOME") {
            let mut path = std::path::PathBuf::from(home);
            path.push(".local");
            path.push("bin");
            path.push(name);
            if path.exists() {
                candidate = path;
            } else {
                candidate = std::path::PathBuf::from(name);
            }
        } else {
            candidate = std::path::PathBuf::from(name);
        }
    }

    match std::process::Command::new(&candidate).spawn() {
        Ok(_) => mgd_common::sync_print!("[core] Autospawned plugin: {}", name),
        Err(e) => mgd_common::sync_print!("[core] Failed to spawn {}: {}", name, e),
    }
}

/// Broadcast a new pressure level to all connected plugins.
pub fn broadcast_pressure(level: &str) {
    let msg = CoreMessage::PressureChanged { level: level.to_string() };
    let mut clients = PLUGIN_CLIENTS.lock().unwrap();
    clients.retain(|tx| tx.send(msg.clone()).is_ok());
}



/// Takes ownership of a `UnixStream` that has been identified as a plugin connection.
/// 
/// This function converts the socket into a bidirectional session:
/// 1. It spawns a background writer thread that listens for `CoreMessage` broadcasts 
///    (e.g., global pressure changes, approval responses) and flushes them to the plugin.
/// 2. It blocks the current thread in a loop, continually parsing incoming `PluginMessage`s 
///    (e.g., GPU observations, ActionRequests) and routing them to the Core.
pub fn serve_plugin_connection(
    stream: UnixStream,
    first_line: String,
    _frozen: Arc<Mutex<FrozenRegistry>>,
    _checkpointed: Arc<Mutex<CheckpointRegistry>>,
) {
    // Disable timeout for long-lived connection
    stream.set_read_timeout(None).ok();

    // Channel for pushing CoreMessages to this specific connection
    let (tx, rx) = channel::<CoreMessage>();
    PLUGIN_CLIENTS.lock().unwrap().push(tx);

    let mut writer = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };

    // Spawn a writer thread to push CoreMessages to the socket
    thread::spawn(move || {
        while let Ok(msg) = rx.recv() {
            if let Ok(json) = serde_json::to_string(&msg) {
                if writeln!(writer, "{}", json).is_err() {
                    break;
                }
            }
        }
    });

    // Process the first line
    process_plugin_line(&first_line);

    // Continue reading
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    while reader.read_line(&mut line).is_ok() && !line.is_empty() {
        process_plugin_line(&line);
        line.clear();
    }
}

fn process_plugin_line(line: &str) {
    let Ok(msg) = serde_json::from_str::<PluginMessage>(line) else { return };
    match msg {
        PluginMessage::Identify { name, version, .. } => {
            mgd_common::sync_print!("[plugin] Connected: {} v{}", name, version);
        }
        PluginMessage::Observation { plugin: _, metric, pid, value } => {
            if let Some(p) = pid {
                if let mgd_common::protocol::Metric::GpuResidentKb = metric {
                    set_gpu_kb(p, value as u64);
                }
            }
        }
        PluginMessage::ActionRequest { plugin, action, reason } => {
            mgd_common::sync_print!("[plugin] {} requested action {:?}: {}", plugin, action, reason);
            
            let approved = true;
            let denial_reason = None;

            // Core could implement rate-limiting or policy checks here.
            // For now, we approve RestartProcess and KillPid natively.
            match &action {
                PluginAction::KillPid { pid } => {
                    let pid = *pid;
                    std::thread::spawn(move || {
                        let _ = crate::executor::killer::terminate(pid);
                    });
                }
                _ => {} // Other actions (like RestartProcess) are delegated back to the plugin to execute
            }

            // Send approval back so the plugin can execute its own specific logic (e.g. restarting a DE service)
            let response = CoreMessage::ActionResponse { 
                action, 
                approved, 
                reason: denial_reason 
            };
            
            let mut clients = PLUGIN_CLIENTS.lock().unwrap();
            clients.retain(|tx| tx.send(response.clone()).is_ok());
        }
        PluginMessage::QueryGpu { pid } => {
            let kb = get_gpu_kb(pid);
            let response = CoreMessage::GpuObservation { pid, kb };
            let mut clients = PLUGIN_CLIENTS.lock().unwrap();
            clients.retain(|tx| tx.send(response.clone()).is_ok());
        }
    }
}
