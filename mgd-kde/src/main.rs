//! mgd-kde — KDE Plasma 6+ plasmashell + plasma-discover watcher plugin.
use std::fs;
use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;
use mgd_common::util::unix_timestamp_secs;
use mgd_common::protocol::{CoreMessage, PluginAction, PluginMessage};
use mgd_common::types::Pid;

const PLUGIN_NAME: &str = "mgd-kde";
const VERSION: &str = env!("CARGO_PKG_VERSION");

static LAST_PLASMA_RESTART: AtomicU64 = AtomicU64::new(0);
static LAST_PD_REAP: AtomicU64 = AtomicU64::new(0);

struct PidCache {
    pid: u32,
    start_time: u64,
}

fn read_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.split_whitespace().nth(21)?.parse().ok()
}

fn resolve_pid(name: &str, cache: &mut Option<PidCache>) -> Option<u32> {
    if let Some(c) = cache.as_ref() {
        if read_start_time(c.pid) == Some(c.start_time) {
            return Some(c.pid);
        }
        *cache = None;
    }
    let pid = mgd_common::process::find_pid_by_name(name)?;
    *cache = Some(PidCache { pid, start_time: read_start_time(pid)? });
    Some(pid)
}

struct DiscoverTracker {
    pid: u32,
    ticks: u64,
    idle_since: u64,
}

fn main() {
    let stream = mgd_common::plugin::connect_and_identify(PLUGIN_NAME, VERSION, vec!["idle_reap"]);

    let mut writer = stream.try_clone().expect("clone stream");

    // Load KWin active window tracker script and start the journal watcher
    if load_kwin_script().is_some() {
        mgd_common::sync_print!("[mgd-kde] KWin active window tracker script loaded and running.");
        watch_active_window(writer.try_clone().expect("clone writer"));
    } else {
        mgd_common::sync_print!("[mgd-kde] Failed to load KWin active window tracker script. Foreground PID tracking unavailable.");
    }
    
    let current_level = Arc::new(Mutex::new("normal".to_string()));
    let level_clone = current_level.clone();
    
    let gpu_kb_cache = Arc::new(Mutex::new(0u64));
    let gpu_kb_clone = gpu_kb_cache.clone();

    thread::spawn(move || {
        mgd_common::plugin::drain_lines(stream, |line| {
            if let Ok(msg) = serde_json::from_str::<CoreMessage>(line) {
                match msg {
                    CoreMessage::PressureChanged { level } => {
                        *level_clone.lock().unwrap() = level;
                    }
                    CoreMessage::GpuObservation { kb, .. } => {
                        *gpu_kb_clone.lock().unwrap() = kb.0;
                    }
                    CoreMessage::ActionResponse { action, approved, reason } => {
                        if !approved {
                            mgd_common::sync_print!("[mgd-kde] action denied: {:?}", reason);
                        } else if let PluginAction::RestartProcess { name } = action
                            && name == "plasmashell" {
                                let now = unix_timestamp_secs();
                                LAST_PLASMA_RESTART.store(now, Ordering::SeqCst);
                                let _ = std::process::Command::new("systemctl")
                                    .arg("--user")
                                    .arg("restart")
                                    .arg("plasma-plasmashell.service")
                                    .spawn();
                            }
                    }
                    CoreMessage::ConfigReload => {
                        *PLUGIN_CONFIG.lock().unwrap() = None;
                    }
                    CoreMessage::Shutdown => {
                        std::process::exit(0);
                    }
                }
            }
        });
    });

    let mut pd_tracker = DiscoverTracker {
        pid: 0,
        ticks: 0,
        idle_since: 0,
    };

    let mut plasmashell_pid_cache: Option<PidCache> = None;
    let mut discover_pid_cache: Option<PidCache> = None;

    loop {
        let level = current_level.lock().unwrap().clone();
        if level == "normal" {
            check_plasma_discover(&mut writer, &mut pd_tracker, &mut discover_pid_cache);
        }

        check_plasma_gpu(&mut writer, &gpu_kb_cache, &mut plasmashell_pid_cache);

        thread::sleep(Duration::from_secs(10));
    }
}

fn default_plasma_cooldown_min() -> u64 { 30 }
fn default_plasma_gpu_threshold_mb() -> u64 { 1024 }
fn default_discover_cooldown_min() -> u64 { 30 }
fn default_discover_idle_check_secs() -> u64 { 60 }
fn default_discover_rss_threshold_mb() -> u64 { 400 }

#[derive(serde::Deserialize, Clone)]
struct PlasmaConfig {
    #[serde(default = "default_plasma_cooldown_min")]
    min_restart_interval_min: u64,
    #[serde(default = "default_plasma_gpu_threshold_mb")]
    gpu_leak_threshold_mb: u64,
}

impl Default for PlasmaConfig {
    fn default() -> Self {
        PlasmaConfig {
            min_restart_interval_min: default_plasma_cooldown_min(),
            gpu_leak_threshold_mb: default_plasma_gpu_threshold_mb(),
        }
    }
}

#[derive(serde::Deserialize, Clone)]
struct PlasmaDiscoverConfig {
    #[serde(default = "default_discover_cooldown_min")]
    cooldown_min: u64,
    #[serde(default = "default_discover_idle_check_secs")]
    idle_check_secs: u64,
    #[serde(default = "default_discover_rss_threshold_mb")]
    rss_threshold_mb: u64,
}

impl Default for PlasmaDiscoverConfig {
    fn default() -> Self {
        PlasmaDiscoverConfig {
            cooldown_min: default_discover_cooldown_min(),
            idle_check_secs: default_discover_idle_check_secs(),
            rss_threshold_mb: default_discover_rss_threshold_mb(),
        }
    }
}

#[derive(serde::Deserialize, Default, Clone)]
struct PluginConfig {
    #[serde(default)]
    plasma: PlasmaConfig,
    #[serde(default)]
    plasma_discover: PlasmaDiscoverConfig,
}

static PLUGIN_CONFIG: std::sync::Mutex<Option<PluginConfig>> = std::sync::Mutex::new(None);

fn load_plugin_config() -> PluginConfig {
    let mut guard = PLUGIN_CONFIG.lock().unwrap();
    if let Some(ref cfg) = *guard {
        return cfg.clone();
    }
    let cfg = load_plugin_config_from_disk();
    *guard = Some(cfg.clone());
    cfg
}

fn load_plugin_config_from_disk() -> PluginConfig {
    let home = std::env::var("HOME").unwrap_or_default();
    let paths = [
        std::path::PathBuf::from(&home).join(".config/mgd/priorities.toml"),
        std::path::PathBuf::from("/etc/mgd/priorities.toml"),
    ];
    for path in &paths {
        if let Ok(content) = std::fs::read_to_string(path)
            && let Ok(cfg) = toml::from_str(&content) {
                return cfg;
            }
    }
    PluginConfig::default()
}

fn check_plasma_gpu(writer: &mut UnixStream, cache: &Arc<Mutex<u64>>, pid_cache: &mut Option<PidCache>) {
    let now = unix_timestamp_secs();
    let last_restart = LAST_PLASMA_RESTART.load(Ordering::SeqCst);
    let cfg = load_plugin_config();
    if now.saturating_sub(last_restart) < cfg.plasma.min_restart_interval_min * 60 {
        return; // cooldown
    }

    let Some(pid) = resolve_pid("plasmashell", pid_cache) else { return };

    // Request latest GPU stats for this PID
    let req = PluginMessage::QueryGpu { pid: Pid(pid) };
    if writeln!(writer, "{}", serde_json::to_string(&req).unwrap()).is_err() {
        std::process::exit(1);
    }

    // Use the currently cached value
    let gpu_kb = *cache.lock().unwrap();

    if gpu_kb / 1024 > cfg.plasma.gpu_leak_threshold_mb {
        let act = PluginMessage::ActionRequest {
            plugin: PLUGIN_NAME.to_string(),
            action: PluginAction::RestartProcess { name: "plasmashell".to_string() },
            reason: format!("gpu memory {}MB > {}MB", gpu_kb / 1024, cfg.plasma.gpu_leak_threshold_mb),
        };
        if writeln!(writer, "{}", serde_json::to_string(&act).unwrap()).is_err() {
            std::process::exit(1);
        }
        // Reset cache so we don't spam requests while waiting for core response
        *cache.lock().unwrap() = 0;
    }
}

fn check_plasma_discover(writer: &mut UnixStream, tracker: &mut DiscoverTracker, pid_cache: &mut Option<PidCache>) {
    let now = unix_timestamp_secs();
    let cfg = load_plugin_config();

    let last_reap = LAST_PD_REAP.load(Ordering::SeqCst);
    let reap_cooldown_secs = cfg.plasma_discover.cooldown_min * 60;
    if now.saturating_sub(last_reap) < reap_cooldown_secs {
        return; // cooldown between reap attempts
    }

    let Some(pid) = resolve_pid("plasma-discover", pid_cache) else {
        tracker.pid = 0;
        return;
    };

    if tracker.pid != pid {
        tracker.pid = pid;
        tracker.ticks = mgd_common::process::read_proc_cpu_ticks(pid).unwrap_or(0);
        tracker.idle_since = now;
        return;
    }

    let current_ticks = mgd_common::process::read_proc_cpu_ticks(pid).unwrap_or(0);
    if current_ticks > tracker.ticks {
        tracker.ticks = current_ticks;
        tracker.idle_since = now;
        return; // not idle
    }

    if now.saturating_sub(tracker.idle_since) >= cfg.plasma_discover.idle_check_secs {
        // Idle for at least configured seconds
        let status_path = format!("/proc/{pid}/status");
        let Ok(status) = fs::read_to_string(&status_path) else { return };
        let mut rss_kb = 0;
        for line in status.lines() {
            if let Some(r) = line.strip_prefix("VmRSS:") {
                rss_kb = r.split_whitespace().next().unwrap_or("0").parse().unwrap_or(0);
                break;
            }
        }

        if rss_kb / 1024 > cfg.plasma_discover.rss_threshold_mb {
            let req = PluginMessage::ActionRequest {
                plugin: PLUGIN_NAME.to_string(),
                action: PluginAction::KillPid { pid: Pid(pid) },
                reason: format!("RSS {}MB > {}MB and idle for {}s", rss_kb / 1024, cfg.plasma_discover.rss_threshold_mb, cfg.plasma_discover.idle_check_secs),
            };
            if writeln!(writer, "{}", serde_json::to_string(&req).unwrap()).is_err() {
                std::process::exit(1);
            }
            LAST_PD_REAP.store(now, Ordering::SeqCst);
        }
    }
}

fn load_kwin_script() -> Option<i32> {
    let home = std::env::var("HOME").ok()?;
    let script_path = format!("{}/.config/mgd/active_window.js", home);
    let script_content = r#"
        workspace.windowActivated.connect(function(window) {
            if (window && window.pid > 0) {
                print("ACTIVE_WINDOW_PID:" + window.pid);
            }
        });
    "#;
    std::fs::create_dir_all(format!("{}/.config/mgd", home)).ok()?;
    std::fs::write(&script_path, script_content).ok()?;

    // Unload first
    let _ = std::process::Command::new("busctl")
        .args(["--user", "call", "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "unloadScript", "s", "mgd-active-window"])
        .output();

    // Load
    let out = std::process::Command::new("busctl")
        .args(["--user", "call", "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "loadScript", "ss", &script_path, "mgd-active-window"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Parse "i <id>"
    let id_str = stdout.split_whitespace().nth(1)?;
    let id: i32 = id_str.parse().ok()?;
    
    // Run
    let script_obj_path = format!("/Scripting/Script{}", id);
    let run_out = std::process::Command::new("busctl")
        .args(["--user", "call", "org.kde.KWin", &script_obj_path, "org.kde.kwin.Script", "run"])
        .output();
    if let Ok(o) = run_out
        && o.status.success() {
            return Some(id);
        }
    None
}

fn watch_active_window(writer: std::os::unix::net::UnixStream) {
    let mut child = match std::process::Command::new("journalctl")
        .args(["--user", "-f", "-o", "cat", "-n", "0"])
        .stdout(std::process::Stdio::piped())
        .spawn() 
    {
        Ok(c) => c,
        Err(e) => {
            mgd_common::sync_print!("[mgd-kde] Failed to spawn journalctl: {}", e);
            return;
        }
    };

    let stdout = child.stdout.take().unwrap();
    let reader = std::io::BufReader::new(stdout);
    let mut writer_clone = writer.try_clone().expect("clone socket");

    std::thread::spawn(move || {
        let mut line = String::new();
        for line_res in reader.lines() {
            if let Ok(l) = line_res
                && let Some(pid_str) = l.trim().strip_prefix("ACTIVE_WINDOW_PID:")
                    && let Ok(pid) = pid_str.trim().parse::<u32>() {
                        let msg = PluginMessage::ActiveWindow { pid: Some(Pid(pid)) };
                        if let Ok(json) = serde_json::to_string(&msg)
                            && writeln!(writer_clone, "{}", json).is_err() {
                                break;
                            }
                    }
            line.clear();
        }
        let _ = child.kill();
    });
}

