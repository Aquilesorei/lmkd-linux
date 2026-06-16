/// IPC server — Unix domain socket at `$XDG_RUNTIME_DIR/mgd.sock`.
///
/// Wire protocol (newline-delimited plain text):
///   Request:  <command> [arg]\n   (e.g. "status\n", "unfreeze firefox\n")
///   Response: OK <data>\n  |  ERR <message>\n
///
/// Connections are served by short-lived threads, capped at MAX_CONNECTIONS.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use std::collections::HashMap;
use crate::executor::registry::{CheckpointRegistry, FrozenRegistry};
use crate::monitor;
use crate::throttle::{ThrottledState, get_process_cgroup_path};

type ThrottleSnapshot = Arc<Mutex<HashMap<String, ThrottledState>>>;

const MAX_CONNECTIONS: usize = 32;

/// RAII guard that decrements active_conns on drop — runs even if the thread panics.
struct ConnGuard(Arc<AtomicUsize>);
impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

// ── server ───────────────────────────────────────────────────────────────────

/// Bind to `path`, handling a stale socket from a previous crash.
/// Returns None if another mgd instance is already listening.
fn bind_socket(path: &std::path::Path) -> Option<UnixListener> {
    match UnixListener::bind(path) {
        Ok(l) => return Some(l),
        Err(e) if e.kind() != std::io::ErrorKind::AddrInUse => {
            eprintln!("[ipc] Failed to bind socket {path:?}: {e}");
            return None;
        }
        _ => {}
    }
    // EADDRINUSE — check whether it's truly stale (no one listening)
    if UnixStream::connect(path).is_ok() {
        eprintln!("[ipc] Another mgd instance is already running on {path:?}");
        return None;
    }
    // Stale socket — remove and retry
    let _ = std::fs::remove_file(path);
    match UnixListener::bind(path) {
        Ok(l) => Some(l),
        Err(e) => { eprintln!("[ipc] Failed to rebind socket {path:?}: {e}"); None }
    }
}

pub fn run_server(
    frozen: Arc<Mutex<FrozenRegistry>>,
    checkpointed: Arc<Mutex<CheckpointRegistry>>,
    throttle_snapshot: ThrottleSnapshot,
) {
    let path = mgd_common::socket::socket_path();

    let listener = match bind_socket(&path) {
        Some(l) => l,
        None => return,
    };

    // Non-blocking accept so the thread can exit cleanly on shutdown
    listener.set_nonblocking(true).ok();

    let active_conns = Arc::new(AtomicUsize::new(0));

    while !crate::should_shutdown() {
        match listener.accept() {
            Ok((stream, _)) => {
                if active_conns.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                    // Drop stream — connection limit reached
                    continue;
                }
                let f = Arc::clone(&frozen);
                let c = Arc::clone(&checkpointed);
                let t = Arc::clone(&throttle_snapshot);
                let a = Arc::clone(&active_conns);
                a.fetch_add(1, Ordering::Relaxed);
                thread::spawn(move || {
                    let _guard = ConnGuard(a); // decrements on drop, even on panic
                    route_ipc_connection(stream, f, c, t);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("[ipc] accept error: {e}");
                thread::sleep(Duration::from_millis(500));
            }
        }
    }

    let _ = std::fs::remove_file(&path);
}

// ── per-connection handler ───────────────────────────────────────────────────

/// Accepts a raw `UnixStream` and routes it based on its initial payload.
///
/// If the first line is a JSON payload (starts with `{`), the connection is treated 
/// as a long-lived plugin session and handed off to `serve_plugin_connection`.
/// Otherwise, it's treated as a short-lived `mgctl` command (e.g., `status`, `reload`), 
/// which is processed synchronously before the connection is dropped.
fn route_ipc_connection(
    mut stream: UnixStream,
    frozen: Arc<Mutex<FrozenRegistry>>,
    checkpointed: Arc<Mutex<CheckpointRegistry>>,
    throttle_snapshot: ThrottleSnapshot,
) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

    let cloned = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => {
            let _ = writeln!(stream, "ERR internal error: fd clone failed");
            return;
        }
    };
    let mut reader = BufReader::new(cloned);
    let mut line = String::new();

    if reader.read_line(&mut line).is_err() || line.is_empty() {
        return;
    }

    if line.starts_with('{') {
        // Handoff to plugin server (which loops and takes over the connection)
        crate::plugin_server::serve_plugin_connection(stream, line, frozen, checkpointed);
        return;
    }

    let response = dispatch(line.trim(), &frozen, &checkpointed, &throttle_snapshot);
    let _ = writeln!(stream, "{response}");
}

fn dispatch(
    raw: &str,
    frozen: &Arc<Mutex<FrozenRegistry>>,
    checkpointed: &Arc<Mutex<CheckpointRegistry>>,
    throttle_snapshot: &ThrottleSnapshot,
) -> String {
    let parts: Vec<&str> = raw.splitn(2, ' ').collect();
    let cmd = parts[0];
    let arg = parts.get(1).copied().unwrap_or("").trim();

    match cmd {
        "status"   => cmd_status(frozen, checkpointed),
        "list"     => cmd_list(frozen, throttle_snapshot),
        "reload"   => cmd_reload(),
        "unfreeze" => {
            if arg.is_empty() {
                err("usage: unfreeze <pid|name>")
            } else {
                cmd_unfreeze(arg, frozen)
            }
        }
        _ => err(&format!("unknown command: {cmd}")),
    }
}

// ── commands ─────────────────────────────────────────────────────────────────

fn cmd_status(
    frozen: &Arc<Mutex<FrozenRegistry>>,
    checkpointed: &Arc<Mutex<CheckpointRegistry>>,
) -> String {
    let pressure = monitor::psi::read_pressure()
        .map(|p| {
            let level = monitor::psi::pressure_level(&p);
            format!("{level} (some_avg10={:.2}%)", p.some_avg10)
        })
        .unwrap_or_else(|_| "unavailable".into());

    let mem = monitor::meminfo::read_meminfo();
    let frozen_count = frozen.lock().unwrap().count();
    let cp_count = checkpointed.lock().unwrap().count();

    ok(&format!(
        "pressure={pressure} | avail={:.0}MB/{:.0}MB | frozen={frozen_count} | checkpointed={cp_count}",
        mem.available_kb as f64 / 1024.0,
        mem.total_kb as f64 / 1024.0,
    ))
}

fn cmd_list(frozen: &Arc<Mutex<FrozenRegistry>>, throttle_snapshot: &ThrottleSnapshot) -> String {
    let frozen_reg = frozen.lock().unwrap();
    let frozen_pids: std::collections::HashSet<u32> =
        frozen_reg.frozen_pids().into_iter().collect();
    let throttle = throttle_snapshot.lock().unwrap();
    let procs = monitor::process::list_processes();

    let mut lines: Vec<String> = Vec::new();

    // Frozen processes (from registry — may not appear in /proc if already killed)
    let mut frozen_entries = frozen_reg.list();
    frozen_entries.sort_by_key(|(pid, _, _)| *pid);
    for (pid, name, ts) in &frozen_entries {
        lines.push(format!("  pid={pid:<8} name={name:<24} frozen_at={ts} [FROZEN]"));
    }

    // Running processes that are throttled
    let mut throttled: Vec<(u32, &str, &str)> = procs
        .iter()
        .filter(|p| !frozen_pids.contains(&p.pid))
        .filter_map(|p| {
            let cgroup = get_process_cgroup_path(p.pid)?;
            let state = throttle.get(&cgroup)?;
            let tag = match state {
                ThrottledState::WeightOnly => "[THROTTLED:light]",
                ThrottledState::Full       => "[THROTTLED:heavy]",
                ThrottledState::None       => return None,
            };
            Some((p.pid, p.name.as_str(), tag))
        })
        .collect();
    throttled.sort_by_key(|(pid, _, _)| *pid);
    for (pid, name, tag) in &throttled {
        lines.push(format!("  pid={pid:<8} name={name:<24} {tag}"));
    }

    if lines.is_empty() {
        return ok("(no frozen or throttled processes)");
    }
    ok(&lines.join("\n"))
}

fn cmd_reload() -> String {
    crate::config::reload();
    ok("config reloaded")
}

fn cmd_unfreeze(arg: &str, frozen: &Arc<Mutex<FrozenRegistry>>) -> String {
    let target_pid: Option<u32> = arg.parse().ok();
    let arg_lower = arg.to_lowercase();

    let pids_to_unfreeze: Vec<(u32, u64)> = {
        let reg = frozen.lock().unwrap();
        reg.frozen_pids()
            .into_iter()
            .filter(|pid| {
                if let Some(tpid) = target_pid {
                    *pid == tpid
                } else {
                    reg.name(*pid).to_lowercase().contains(&arg_lower)
                }
            })
            .map(|pid| {
                let st = reg.start_time(pid);
                (pid, st)
            })
            .collect()
    };

    if pids_to_unfreeze.is_empty() {
        return err(&format!("no frozen process matching '{arg}'"));
    }

    let mut results = Vec::new();
    for (pid, start_time) in pids_to_unfreeze {
        let r = crate::executor::freezer::unfreeze_checked(pid, start_time);
        if r.success {
            frozen.lock().unwrap().remove(pid);
            results.push(format!("✓ unfroze pid={pid}"));
        } else {
            results.push(format!("✗ pid={pid}: {}", r.error.unwrap_or_default()));
        }
    }
    ok(&results.join("\n"))
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn ok(data: &str) -> String {
    format!("OK {data}")
}

fn err(msg: &str) -> String {
    format!("ERR {msg}")
}
