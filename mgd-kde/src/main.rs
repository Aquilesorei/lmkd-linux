//! mgd-kde — KDE Plasma 6+ plasmashell + plasma-discover watcher plugin.
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use mgd_common::protocol::{CoreMessage, PluginAction, PluginMessage};

const PLUGIN_NAME: &str = "mgd-kde";
const VERSION: &str = env!("CARGO_PKG_VERSION");

static LAST_PLASMA_RESTART: AtomicU64 = AtomicU64::new(0);
static LAST_PD_REAP: AtomicU64 = AtomicU64::new(0);

struct DiscoverTracker {
    pid: u32,
    ticks: u64,
    idle_since: u64,
}

fn main() {
    let stream = mgd_common::plugin::connect_and_identify(PLUGIN_NAME, VERSION, vec!["idle_reap"]);

    let mut writer = stream.try_clone().expect("clone stream");
    let mut reader = BufReader::new(stream);
    
    let current_level = Arc::new(Mutex::new("normal".to_string()));
    let level_clone = current_level.clone();
    
    let gpu_kb_cache = Arc::new(Mutex::new(0u64));
    let gpu_kb_clone = gpu_kb_cache.clone();

    thread::spawn(move || {
        let mut line = String::new();
        while reader.read_line(&mut line).is_ok() && !line.is_empty() {
            if let Ok(msg) = serde_json::from_str::<CoreMessage>(&line) {
                match msg {
                    CoreMessage::PressureChanged { level } => {
                        *level_clone.lock().unwrap() = level;
                    }
                    CoreMessage::GpuObservation { kb, .. } => {
                        *gpu_kb_clone.lock().unwrap() = kb;
                    }
                    CoreMessage::ActionResponse { action, approved, reason } => {
                        if !approved {
                            mgd_common::sync_print!("[mgd-kde] action denied: {:?}", reason);
                        } else if let PluginAction::RestartProcess { name } = action {
                            if name == "plasmashell" {
                                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                                LAST_PLASMA_RESTART.store(now, Ordering::SeqCst);
                                let _ = std::process::Command::new("systemctl")
                                    .arg("--user")
                                    .arg("restart")
                                    .arg("plasma-plasmashell.service")
                                    .spawn();
                            }
                        }
                    }
                    CoreMessage::Shutdown => {
                        std::process::exit(0);
                    }
                }
            }
            line.clear();
        }
    });

    let mut pd_tracker = DiscoverTracker {
        pid: 0,
        ticks: 0,
        idle_since: 0,
    };

    loop {
        let level = current_level.lock().unwrap().clone();
        if level == "normal" {
            check_plasma_discover(&mut writer, &mut pd_tracker);
        }
        
        check_plasma_gpu(&mut writer, &gpu_kb_cache);

        thread::sleep(Duration::from_secs(10));
    }
}

fn get_process_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let parts: Vec<&str> = stat.split_whitespace().collect();
    if parts.len() > 14 {
        let utime: u64 = parts[13].parse().unwrap_or(0);
        let stime: u64 = parts[14].parse().unwrap_or(0);
        Some(utime + stime)
    } else {
        None
    }
}

fn check_plasma_gpu(writer: &mut UnixStream, cache: &Arc<Mutex<u64>>) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let last_restart = LAST_PLASMA_RESTART.load(Ordering::SeqCst);
    if now.saturating_sub(last_restart) < 600 {
        return; // 10 minute cooldown
    }

    let Some(pid) = mgd_common::process::find_pid_by_name("plasmashell") else { return };
    
    // Request latest GPU stats for this PID
    let req = PluginMessage::QueryGpu { pid };
    let _ = writeln!(writer, "{}", serde_json::to_string(&req).unwrap());
    
    // Use the currently cached value
    let gpu_kb = *cache.lock().unwrap();

    if gpu_kb / 1024 > 250 {
        let act = PluginMessage::ActionRequest {
            plugin: PLUGIN_NAME.to_string(),
            action: PluginAction::RestartProcess { name: "plasmashell".to_string() },
            reason: format!("gpu memory {}MB > 250MB", gpu_kb / 1024),
        };
        let _ = writeln!(writer, "{}", serde_json::to_string(&act).unwrap());
        // Reset cache so we don't spam requests while waiting for core response
        *cache.lock().unwrap() = 0;
    }
}

fn check_plasma_discover(writer: &mut UnixStream, tracker: &mut DiscoverTracker) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    
    let last_reap = LAST_PD_REAP.load(Ordering::SeqCst);
    if now.saturating_sub(last_reap) < 60 {
        return; // 1 minute cooldown between reap attempts
    }

    let Some(pid) = mgd_common::process::find_pid_by_name("plasma-discover") else { 
        tracker.pid = 0;
        return; 
    };

    if tracker.pid != pid {
        tracker.pid = pid;
        tracker.ticks = get_process_ticks(pid).unwrap_or(0);
        tracker.idle_since = now;
        return;
    }

    let current_ticks = get_process_ticks(pid).unwrap_or(0);
    if current_ticks > tracker.ticks {
        tracker.ticks = current_ticks;
        tracker.idle_since = now;
        return; // not idle
    }

    if now.saturating_sub(tracker.idle_since) >= 60 {
        // Idle for at least 60 seconds
        let status_path = format!("/proc/{pid}/status");
        let Ok(status) = fs::read_to_string(&status_path) else { return };
        let mut rss_kb = 0;
        for line in status.lines() {
            if let Some(r) = line.strip_prefix("VmRSS:") {
                rss_kb = r.split_whitespace().next().unwrap_or("0").parse().unwrap_or(0);
                break;
            }
        }

        if rss_kb / 1024 > 150 {
            let req = PluginMessage::ActionRequest {
                plugin: PLUGIN_NAME.to_string(),
                action: PluginAction::KillPid { pid },
                reason: format!("RSS {}MB > 150MB and idle for 60s", rss_kb / 1024),
            };
            let _ = writeln!(writer, "{}", serde_json::to_string(&req).unwrap());
            LAST_PD_REAP.store(now, Ordering::SeqCst);
        }
    }
}
