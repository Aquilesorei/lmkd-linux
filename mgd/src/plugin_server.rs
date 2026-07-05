use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, LazyLock};
use std::sync::mpsc::{channel, Sender};
use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant};

use mgd_common::protocol::{CoreMessage, PluginMessage, PluginAction};

use crate::executor::registry::{FrozenRegistry, CheckpointRegistry};

/// Senders to all active plugin connections
static PLUGIN_CLIENTS: LazyLock<Mutex<Vec<Sender<CoreMessage>>>> = LazyLock::new(|| Mutex::new(Vec::new()));

const GPU_CACHE_TTL: Duration = Duration::from_secs(30);

/// Cached GPU observations from plugins. Keyed by pid; entries expire after
/// GPU_CACHE_TTL so dead processes don't accumulate stale data.
static GPU_CACHE: LazyLock<Mutex<HashMap<u32, (mgd_common::gpu::SingleProcessGpuMemory, Instant)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Active foreground process PID reported by DE plugins
static ACTIVE_FOREGROUND_PID: LazyLock<Mutex<Option<u32>>> = LazyLock::new(|| Mutex::new(None));

/// Tracked child processes for crash-restart watchdog. Keyed by plugin name.
static PLUGIN_CHILDREN: LazyLock<Mutex<HashMap<String, (std::process::Child, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn get_gpu_kb(pid: u32) -> u64 {
    let now = Instant::now();
    GPU_CACHE.lock().unwrap()
        .get(&pid)
        .filter(|(_, at)| now.duration_since(*at) < GPU_CACHE_TTL)
        .map(|(s, _)| s.resident_kb)
        .unwrap_or(0)
}

pub fn get_gpu_stats(pid: u32) -> mgd_common::gpu::SingleProcessGpuMemory {
    let now = Instant::now();
    GPU_CACHE.lock().unwrap()
        .get(&pid)
        .filter(|(_, at)| now.duration_since(*at) < GPU_CACHE_TTL)
        .map(|(s, _)| s.clone())
        .unwrap_or_default()
}

/// Upsert a single GPU metric field for `pid`. Refreshes TTL; prunes stale entries.
pub fn set_gpu_observation(pid: u32, metric: &mgd_common::protocol::Metric, kb: u64) {
    use mgd_common::protocol::Metric;
    let now = Instant::now();
    let mut cache = GPU_CACHE.lock().unwrap();
    cache.retain(|_, (_, at)| now.duration_since(*at) < GPU_CACHE_TTL);
    let entry = cache.entry(pid).or_insert_with(|| (mgd_common::gpu::SingleProcessGpuMemory::default(), now));
    entry.1 = now;
    match metric {
        Metric::GpuResidentKb  => entry.0.resident_kb  = kb,
        Metric::GpuSharedKb    => entry.0.shared_kb     = kb,
        Metric::GpuTotalKb     => entry.0.total_kb      = kb,
        Metric::GpuPurgeableKb => entry.0.purgeable_kb  = kb,
        _ => {}
    }
}

/// True GPU pressure KB: resident minus shared (deduplicates compositor dma-buf).
pub fn get_total_gpu_kb() -> u64 {
    let now = Instant::now();
    GPU_CACHE.lock().unwrap()
        .values()
        .filter(|(_, at)| now.duration_since(*at) < GPU_CACHE_TTL)
        .map(|(s, _)| s.resident_kb.saturating_sub(s.shared_kb))
        .sum()
}

/// Returns (pid_count, pressure_kb, newest_obs_age_secs) from the live GPU cache.
pub fn gpu_cache_snapshot() -> (usize, u64, Option<u64>) {
    let now = Instant::now();
    let cache = GPU_CACHE.lock().unwrap();
    let live: Vec<_> = cache.values()
        .filter(|(_, at)| now.duration_since(*at) < GPU_CACHE_TTL)
        .collect();
    if live.is_empty() {
        return (0, 0, None);
    }
    let pressure_kb: u64 = live.iter().map(|(s, _)| s.resident_kb.saturating_sub(s.shared_kb)).sum();
    let newest_age = live.iter()
        .map(|(_, at)| now.duration_since(*at).as_secs())
        .min();
    (live.len(), pressure_kb, newest_age)
}

pub fn get_active_foreground_pid() -> Option<u32> {
    *ACTIVE_FOREGROUND_PID.lock().unwrap()
}

pub fn set_active_foreground_pid(pid: Option<u32>) {
    *ACTIVE_FOREGROUND_PID.lock().unwrap() = pid;
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
    } else if desktop.contains("gnome") || desktop.contains("cosmic") {
        // mgd-gnome / mgd-cosmic are todo!() scaffolds: they panic on startup and
        // the watchdog would respawn them every 60s forever. Don't spawn until
        // they're implemented.
        mgd_common::sync_print!("[core] No working plugin for desktop '{}' yet — skipping", desktop);
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
        Ok(child) => {
            mgd_common::sync_print!("[core] Autospawned plugin: {}", name);
            PLUGIN_CHILDREN.lock().unwrap().insert(name.to_string(), (child, Instant::now()));
        }
        Err(e) => mgd_common::sync_print!("[core] Failed to spawn {}: {}", name, e),
    }
}

/// Check all tracked plugins; restart any that have exited.
/// Called from the maintenance loop (~60s interval).
pub fn check_and_restart_plugins() {
    let now = Instant::now();
    let dead: Vec<String> = {
        let mut children = PLUGIN_CHILDREN.lock().unwrap();
        let dead: Vec<String> = children
            .iter_mut()
            .filter_map(|(name, (child, spawned_at))| {
                child.try_wait().ok().flatten().map(|status| {
                    let uptime = now.duration_since(*spawned_at).as_secs();
                    mgd_common::sync_print!(
                        "[core] Plugin {} exited ({}) after {}s — restarting",
                        name, status, uptime
                    );
                    name.clone()
                })
            })
            .collect();
        for name in &dead {
            children.remove(name);
        }
        dead
    };
    for name in dead {
        spawn_plugin_binary(&name);
    }
}

/// SIGTERM all tracked plugin children on daemon shutdown.
pub fn shutdown_plugins() {
    let mut children = PLUGIN_CHILDREN.lock().unwrap();
    for (name, (child, _)) in children.iter_mut() {
        let pid = child.id() as libc::pid_t;
        unsafe { libc::kill(pid, libc::SIGTERM); }
        mgd_common::sync_print!("[core] Sent SIGTERM to plugin {} (pid {})", name, pid);
    }
    children.clear();
}

/// Broadcast a new pressure level to all connected plugins.
pub fn broadcast_pressure(level: &str) {
    let msg = CoreMessage::PressureChanged { level: level.to_string() };
    let mut clients = PLUGIN_CLIENTS.lock().unwrap();
    clients.retain(|tx| tx.send(msg.clone()).is_ok());
}

pub fn broadcast_config_reload() {
    let msg = CoreMessage::ConfigReload;
    let mut clients = PLUGIN_CLIENTS.lock().unwrap();
    clients.retain(|tx| tx.send(msg.clone()).is_ok());
}




pub fn serve_plugin_connection(
    stream: UnixStream,
    first_line: String,
    frozen: Arc<Mutex<FrozenRegistry>>,
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
            if let Ok(json) = serde_json::to_string(&msg)
                && writeln!(writer, "{}", json).is_err() {
                    break;
                }
        }
    });

    // Process the first line
    process_plugin_line(&first_line, &frozen);

    // Continue reading
    mgd_common::plugin::drain_lines(stream, |line| {
        process_plugin_line(line, &frozen);
    });
}

fn process_plugin_line(line: &str, frozen: &Arc<Mutex<FrozenRegistry>>) {
    let Ok(msg) = serde_json::from_str::<PluginMessage>(line) else { return };
    match msg {
        PluginMessage::Identify { name, version, .. } => {
            mgd_common::sync_print!("[plugin] Connected: {} v{}", name, version);
        }
        PluginMessage::Observation { plugin: _, metric, pid, value } => {
            use mgd_common::protocol::Metric;
            if let Some(p) = pid
                && matches!(metric, Metric::GpuResidentKb | Metric::GpuSharedKb | Metric::GpuTotalKb | Metric::GpuPurgeableKb) {
                    set_gpu_observation(p, &metric, value as u64);
                }
        }
        PluginMessage::ActionRequest { plugin, action, reason } => {
            mgd_common::sync_print!("[plugin] {} requested action {:?}: {}", plugin, action, reason);

            let approved = true;
            let denial_reason = None;

            // Core could implement rate-limiting or policy checks here.
            // For now, we approve RestartProcess and KillPid natively.
            // Other actions (like RestartProcess) are delegated back to the plugin to execute.
            if let PluginAction::KillPid { pid } = &action {
                let pid = *pid;
                std::thread::spawn(move || {
                    let _ = crate::executor::killer::sigterm(pid);
                });
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
            let stats = get_gpu_stats(pid);
            let response = CoreMessage::GpuObservation {
                pid,
                kb: stats.resident_kb,
                shared_kb: stats.shared_kb,
                total_kb: stats.total_kb,
                purgeable_kb: stats.purgeable_kb,
            };
            let mut clients = PLUGIN_CLIENTS.lock().unwrap();
            clients.retain(|tx| tx.send(response.clone()).is_ok());
        }
        PluginMessage::ActiveWindow { pid } => {
            set_active_foreground_pid(pid);
            // If the newly-active process is frozen, unfreeze it immediately
            // rather than waiting for the next RecoveryManager poll.
            if let Some(p) = pid {
                let (is_frozen, start_time) = {
                    let reg = frozen.lock().unwrap();
                    (reg.is_frozen(p), reg.start_time(p))
                };
                if is_frozen {
                    let frozen_clone = Arc::clone(frozen);
                    thread::spawn(move || {
                        let r = crate::executor::freezer::unfreeze_checked(p, start_time);
                        if r.success {
                            frozen_clone.lock().unwrap().remove(p);
                            mgd_common::sync_print!(
                                "[recovery] Instant-unfroze foreground PID {} (active window)", p
                            );
                        }
                    });
                }
            }
        }
    }
}
