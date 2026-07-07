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
use mgd_common::types::Pid;
use crate::executor::registry::{CheckpointRegistry, FrozenRegistry};
use crate::monitor;
use crate::throttle::ThrottledState;

type ThrottleSnapshot = Arc<Mutex<HashMap<String, ThrottledState>>>;

const MAX_CONNECTIONS: usize = 32;

fn format_ts(ts: u64) -> String {
    // Simple HH:MM:SS formatting from unix epoch seconds (local time via libc)
    let t = ts as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

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
    event_log: crate::events::EventLog,
    spike_snapshot: Arc<Mutex<crate::spike_mode::SpikeSnapshot>>,
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
                let e = Arc::clone(&event_log);
                let s = Arc::clone(&spike_snapshot);
                let a = Arc::clone(&active_conns);
                a.fetch_add(1, Ordering::Relaxed);
                thread::spawn(move || {
                    let _guard = ConnGuard(a); // decrements on drop, even on panic
                    route_ipc_connection(stream, f, c, t, e, s);
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
    event_log: crate::events::EventLog,
    spike_snapshot: Arc<Mutex<crate::spike_mode::SpikeSnapshot>>,
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

    let response = dispatch(line.trim(), &frozen, &checkpointed, &throttle_snapshot, &event_log, &spike_snapshot);
    let _ = writeln!(stream, "{response}");
}

fn dispatch(
    raw: &str,
    frozen: &Arc<Mutex<FrozenRegistry>>,
    checkpointed: &Arc<Mutex<CheckpointRegistry>>,
    throttle_snapshot: &ThrottleSnapshot,
    event_log: &crate::events::EventLog,
    spike_snapshot: &Arc<Mutex<crate::spike_mode::SpikeSnapshot>>,
) -> String {
    let parts: Vec<&str> = raw.splitn(2, ' ').collect();
    let cmd = parts[0];
    let arg = parts.get(1).copied().unwrap_or("").trim();

    // Request-scoped config snapshot — commands below borrow it.
    let cfg = crate::config::get();

    match cmd {
        "status"   => cmd_status(frozen, checkpointed, throttle_snapshot, spike_snapshot, &cfg),
        "list"     => cmd_list(frozen, checkpointed, throttle_snapshot),
        "ps"       => cmd_ps(frozen, throttle_snapshot, &cfg),
        "events"   => cmd_events(event_log),
        "reload"   => cmd_reload(),
        "unfreeze" => {
            if arg.is_empty() {
                err("usage: unfreeze <pid|name>")
            } else {
                cmd_unfreeze(arg, frozen)
            }
        }
        "freeze" => {
            if arg.is_empty() {
                err("usage: freeze <pid|name>")
            } else {
                cmd_freeze(arg, frozen)
            }
        }
        "restore" => {
            if arg.is_empty() {
                err("usage: restore <pid|name>")
            } else {
                cmd_restore(arg, checkpointed)
            }
        }
        "kill" => {
            if arg.is_empty() {
                err("usage: kill <pid|name>")
            } else {
                cmd_kill(arg, event_log)
            }
        }
        "info" => {
            if arg.is_empty() {
                err("usage: info <pid|name>")
            } else {
                cmd_info(arg, frozen, checkpointed, throttle_snapshot, &cfg)
            }
        }
        "gpu-info"      => cmd_gpu_info(),
        "spike-status"  => cmd_spike_status(spike_snapshot),
        _ => err(&format!("unknown command: {cmd}")),
    }
}

// ── commands ─────────────────────────────────────────────────────────────────

fn cmd_status(
    frozen: &Arc<Mutex<FrozenRegistry>>,
    checkpointed: &Arc<Mutex<CheckpointRegistry>>,
    throttle_snapshot: &ThrottleSnapshot,
    spike_snapshot: &Arc<Mutex<crate::spike_mode::SpikeSnapshot>>,
    cfg: &crate::config::CompiledConfig,
) -> String {
    let pressure = monitor::psi::read_pressure()
        .map(|p| {
            let level = monitor::psi::pressure_level_with(&p, &cfg.psi);
            format!("{level} (some_avg10={:.2}%)", p.some_avg10)
        })
        .unwrap_or_else(|_| "unavailable".into());

    let mem = monitor::meminfo::read_meminfo();
    let frozen_count = frozen.lock().unwrap().count();
    let cp_count = checkpointed.lock().unwrap().count();
    let throttle_count = throttle_snapshot.lock().unwrap()
        .values()
        .filter(|s| **s != ThrottledState::None)
        .count();

    let swap_str = if mem.swap_total_kb.0 > 0 {
        format!(" | swap={:.0}%", mem.swap_used_pct())
    } else {
        " | swap=none".to_string()
    };

    let mut out = format!(
        "pressure={pressure} | avail={:.0}MB/{:.0}MB{swap_str} | frozen={frozen_count} | checkpointed={cp_count} | throttled={throttle_count}",
        mem.available_kb.mb(),
        mem.total_kb.mb(),
    );

    // Per-feature gate state: enabled? last fired? blocked by what?
    let gates = crate::evictor::feature_gates();
    let (last_reclaim, reclaim_disabled) = crate::maintenance::reclaim_gate();
    let (spike_tracked, spike_victims) = {
        let s = spike_snapshot.lock().unwrap();
        (s.active.len(), s.victims.len())
    };
    let fired = |ts: u64| if ts == 0 { "never".to_string() } else { format!("last={}", format_ts(ts)) };

    out.push_str("\nfeatures:");
    out.push_str(&format!(
        "\n  zram_compact       enabled={} {}{}",
        cfg.compact_zram_on_elevated, fired(gates.last_zram_compact),
        if gates.zram_compact_disabled { " [DISABLED: sysfs grant absent]" } else { "" },
    ));
    out.push_str(&format!(
        "\n  cache_drop         enabled={} trigger={} {}",
        cfg.cache_drop_enabled, cfg.cache_drop_trigger, fired(gates.last_cache_drop),
    ));
    out.push_str(&format!(
        "\n  early_reclaim      always-on (Elevated) {}",
        fired(gates.last_early_reclaim),
    ));
    out.push_str(&format!(
        "\n  idle_reclaim       enabled={} important={} {}",
        cfg.idle_reclaim_enabled, cfg.idle_reclaim_important_enabled, fired(gates.last_idle_reclaim),
    ));
    out.push_str(&format!(
        "\n  proactive_reclaim  enabled={} {}{}",
        cfg.proactive_swap_reclaim, fired(last_reclaim),
        if reclaim_disabled { " [DISABLED: helper absent/uncapped]" } else { "" },
    ));
    out.push_str(&format!(
        "\n  spike_mode         enabled={} tracked={} victims={}",
        cfg.spike_mode_enabled, spike_tracked, spike_victims,
    ));
    out.push_str(&format!(
        "\n  auto_kill_idle     rules={}",
        cfg.auto_kill_rules.len(),
    ));
    out.push_str(&format!(
        "\n  hibernate          {}",
        if cfg.emergency_hibernate_after_sec == 0 { "disabled".to_string() }
        else { format!("after {}s Emergency", cfg.emergency_hibernate_after_sec) },
    ));

    ok(&out)
}

fn cmd_list(
    frozen: &Arc<Mutex<FrozenRegistry>>,
    checkpointed: &Arc<Mutex<CheckpointRegistry>>,
    throttle_snapshot: &ThrottleSnapshot,
) -> String {
    let frozen_reg = frozen.lock().unwrap();
    let frozen_pids: std::collections::HashSet<Pid> =
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

    // Checkpointed processes (killed after snapshot — no longer in /proc)
    let cp_reg = checkpointed.lock().unwrap();
    let mut cp_entries = cp_reg.entries_lightest_first();
    cp_entries.sort_by_key(|(pid, _, _, _, _)| *pid);
    for (pid, name, _, rss, attempts) in &cp_entries {
        lines.push(format!(
            "  pid={pid:<8} name={name:<24} rss_at_cp={:.0}MB restore_attempts={attempts} [CHECKPOINTED]",
            rss.mb(),
        ));
    }

    // Running processes that are throttled
    let mut throttled: Vec<(Pid, &str, &str)> = procs
        .iter()
        .filter(|p| !frozen_pids.contains(&p.pid))
        .filter_map(|p| {
            let cgroup = p.cgroup_path.as_deref()?;
            let state = throttle.get(cgroup)?;
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
        return ok("(no frozen, checkpointed, or throttled processes)");
    }
    ok(&lines.join("\n"))
}

fn cmd_reload() -> String {
    crate::config::reload();
    crate::plugin_server::broadcast_config_reload();
    ok("config reloaded")
}

fn cmd_unfreeze(arg: &str, frozen: &Arc<Mutex<FrozenRegistry>>) -> String {
    let target_pid: Option<Pid> = arg.parse().ok().map(Pid);
    let arg_lower = arg.to_lowercase();

    let pids_to_unfreeze: Vec<(Pid, u64)> = {
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

fn cmd_freeze(arg: &str, frozen: &Arc<Mutex<FrozenRegistry>>) -> String {
    let target_pid: Option<Pid> = arg.parse().ok().map(Pid);
    let arg_lower = arg.to_lowercase();

    let procs = monitor::process::list_processes();
    let candidates: Vec<_> = procs
        .iter()
        .filter(|p| {
            if let Some(tpid) = target_pid { p.pid == tpid }
            else { p.name.to_lowercase().contains(&arg_lower) }
        })
        .collect();

    if candidates.is_empty() {
        return err(&format!("no running process matching '{arg}'"));
    }

    let mut results = Vec::new();
    for p in candidates {
        if frozen.lock().unwrap().is_frozen(p.pid) {
            results.push(format!("  pid={} ({}) already frozen", p.pid, p.name));
            continue;
        }
        let r = crate::executor::freezer::freeze(p.pid);
        if r.success {
            frozen.lock().unwrap().add(p.pid, &p.name);
            results.push(format!("✓ froze pid={} ({})", p.pid, p.name));
        } else {
            results.push(format!("✗ pid={} ({}): {}", p.pid, p.name, r.error.unwrap_or_default()));
        }
    }
    ok(&results.join("\n"))
}

fn cmd_restore(arg: &str, checkpointed: &Arc<Mutex<CheckpointRegistry>>) -> String {
    let target_pid: Option<Pid> = arg.parse().ok().map(Pid);
    let arg_lower = arg.to_lowercase();

    let entries: Vec<_> = {
        let reg = checkpointed.lock().unwrap();
        reg.entries_lightest_first()
            .into_iter()
            .filter(|(pid, name, _, _, _)| {
                if let Some(tpid) = target_pid { *pid == tpid }
                else { name.to_lowercase().contains(&arg_lower) }
            })
            .map(|(pid, name, dir, _, _)| (pid, name, dir))
            .collect()
    };

    if entries.is_empty() {
        return err(&format!("no checkpointed process matching '{arg}'"));
    }

    let mut results = Vec::new();
    for (pid, name, snapshot_dir) in entries {
        let r = crate::executor::checkpoint::restore(&snapshot_dir);
        if r.success {
            checkpointed.lock().unwrap().remove(pid);
            results.push(format!("✓ restored pid={pid} ({name})"));
        } else {
            results.push(format!("✗ pid={pid} ({name}): {}", r.error.unwrap_or_default()));
        }
    }
    ok(&results.join("\n"))
}

fn cmd_info(
    arg: &str,
    frozen: &Arc<Mutex<FrozenRegistry>>,
    checkpointed: &Arc<Mutex<CheckpointRegistry>>,
    throttle_snapshot: &ThrottleSnapshot,
    cfg: &crate::config::CompiledConfig,
) -> String {
    let target_pid: Option<Pid> = arg.parse().ok().map(Pid);
    let arg_lower = arg.to_lowercase();

    let procs = monitor::process::list_processes();
    let live: Vec<_> = procs
        .iter()
        .filter(|p| {
            if let Some(tpid) = target_pid { p.pid == tpid }
            else { p.name.to_lowercase().contains(&arg_lower) }
        })
        .collect();

    let frozen_reg = frozen.lock().unwrap();
    let cp_reg = checkpointed.lock().unwrap();
    let throttle = throttle_snapshot.lock().unwrap();
    let cp_entries: Vec<_> = cp_reg.entries_lightest_first()
        .into_iter()
        .filter(|(pid, name, _, _, _)| {
            if let Some(tpid) = target_pid { *pid == tpid }
            else { name.to_lowercase().contains(&arg_lower) }
        })
        .collect();

    if live.is_empty() && cp_entries.is_empty() {
        return err(&format!("no process matching '{arg}'"));
    }

    let mut lines = Vec::new();

    for p in live {
        let state = if frozen_reg.is_frozen(p.pid) {
            "FROZEN".to_string()
        } else {
            match p.cgroup_path.as_deref().and_then(|cg| throttle.get(cg)) {
                Some(ThrottledState::Full)       => "THROTTLED:heavy".to_string(),
                Some(ThrottledState::WeightOnly) => "THROTTLED:light".to_string(),
                _                                => "normal".to_string(),
            }
        };
        let priority = crate::engine::decision::get_priority(&p.name, p.exe_basename.as_deref(), cfg);
        lines.push(format!(
            "pid={:<8} name={:<24} state={:<16} rss={:.0}MB swap={:.0}MB oom={} priority={} cgroup={}",
            p.pid, p.name, state,
            p.rss_kb.mb(),
            p.swap_kb.mb(),
            p.oom_score, priority,
            p.cgroup_path.as_deref().unwrap_or("none"),
        ));
    }

    for (pid, name, _, rss, attempts) in cp_entries {
        lines.push(format!(
            "pid={:<8} name={:<24} state=CHECKPOINTED      rss_at_cp={:.0}MB restore_attempts={attempts}",
            pid, name, rss.mb(),
        ));
    }

    ok(&lines.join("\n"))
}

fn cmd_ps(
    frozen: &Arc<Mutex<FrozenRegistry>>,
    throttle_snapshot: &ThrottleSnapshot,
    cfg: &crate::config::CompiledConfig,
) -> String {
    let procs = monitor::process::list_processes();
    let frozen_reg = frozen.lock().unwrap();
    let throttle = throttle_snapshot.lock().unwrap();

    if procs.is_empty() {
        return ok("(no monitored processes)");
    }

    let mut lines = Vec::new();
    let mut sorted = procs;
    sorted.sort_by_key(|p| std::cmp::Reverse(p.rss_kb));

    for p in &sorted {
        let state = if frozen_reg.is_frozen(p.pid) {
            "FROZEN"
        } else {
            match p.cgroup_path.as_deref().and_then(|cg| throttle.get(cg)) {
                Some(ThrottledState::Full)       => "THROTTLED:heavy",
                Some(ThrottledState::WeightOnly) => "THROTTLED:light",
                _                                => "normal",
            }
        };
        let priority = crate::engine::decision::get_priority(&p.name, p.exe_basename.as_deref(), cfg);
        lines.push(format!(
            "  pid={:<7} name={:<22} rss={:>6.0}MB swap={:>5.0}MB cpu={:>5.1}% prio={:<3} state={}",
            p.pid, p.name,
            p.rss_kb.mb(),
            p.swap_kb.mb(),
            p.cpu_pct,
            priority,
            state,
        ));
    }
    ok(&lines.join("\n"))
}

fn cmd_kill(arg: &str, event_log: &crate::events::EventLog) -> String {
    let target_pid: Option<Pid> = arg.parse().ok().map(Pid);
    let arg_lower = arg.to_lowercase();

    let procs = monitor::process::list_processes();
    let candidates: Vec<_> = procs
        .iter()
        .filter(|p| {
            if let Some(tpid) = target_pid { p.pid == tpid }
            else { p.name.to_lowercase().contains(&arg_lower) }
        })
        .collect();

    if candidates.is_empty() {
        return err(&format!("no running process matching '{arg}'"));
    }

    let mut results = Vec::new();
    for p in candidates {
        let r = crate::executor::killer::sigkill(p.pid);
        if r.success {
            crate::events::push(event_log, mgd_common::logger::LogAction::KillManual, p.pid, &p.name, "killed via mgctl");
            results.push(format!("✓ killed pid={} ({})", p.pid, p.name));
        } else {
            results.push(format!("✗ pid={} ({}): {}", p.pid, p.name, r.error.unwrap_or_default()));
        }
    }
    ok(&results.join("\n"))
}

fn cmd_gpu_info() -> String {
    let (pids, total, newest_age) = crate::plugin_server::gpu_cache_snapshot();
    let age_str = newest_age.map(|a| format!("{a}s ago")).unwrap_or_else(|| "none".to_string());
    ok(&format!("gpu_pids={pids} total_kb={} newest_obs={age_str}", total.0))
}

fn cmd_spike_status(spike_snapshot: &Arc<Mutex<crate::spike_mode::SpikeSnapshot>>) -> String {
    use crate::spike_mode::SpikePhase;
    let snap = spike_snapshot.lock().unwrap();
    if snap.active.is_empty() {
        return ok("idle (no spike candidates active)");
    }
    let mut lines: Vec<String> = vec!["spike candidates:".to_string()];
    for (pid, name, phase, rss_max, samples, cpu_throttled) in &snap.active {
        let phase_str = if *phase == SpikePhase::Tracking { "tracking" } else { "observing" };
        lines.push(format!(
            "  pid={:<7} name={:<22} phase={:<10} rss_max={:.0}MB samples={} cpu_throttled={}",
            pid, name, phase_str, rss_max.mb(), samples, cpu_throttled
        ));
    }
    if !snap.victims.is_empty() {
        lines.push("frozen victims:".to_string());
        for (pid, name, for_spike) in &snap.victims {
            lines.push(format!("  pid={:<7} name={:<22} frozen_for_spike={}", pid, name, for_spike));
        }
    }
    ok(&lines.join("\n"))
}

fn cmd_events(event_log: &crate::events::EventLog) -> String {
    let q = event_log.lock().unwrap();
    if q.is_empty() {
        return ok("(no events recorded)");
    }
    let lines: Vec<String> = q.iter().map(|e| {
        format!("  {} {:<14} pid={:<7} name={:<22} {}", format_ts(e.timestamp), e.action.as_str(), e.pid, e.name, e.detail)
    }).collect();
    ok(&lines.join("\n"))
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn ok(data: &str) -> String {
    format!("OK {data}")
}

fn err(msg: &str) -> String {
    format!("ERR {msg}")
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::registry::{CheckpointRegistry, FrozenRegistry};
    use crate::events;
    use std::sync::{Arc, Mutex};
    use std::collections::HashMap;

    fn empty_frozen() -> Arc<Mutex<FrozenRegistry>> {
        Arc::new(Mutex::new(FrozenRegistry::new()))
    }

    fn empty_checkpointed() -> Arc<Mutex<CheckpointRegistry>> {
        Arc::new(Mutex::new(CheckpointRegistry::new()))
    }

    fn empty_throttle() -> Arc<Mutex<HashMap<String, ThrottledState>>> {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn empty_events() -> events::EventLog {
        events::new_log()
    }

    #[test]
    fn freeze_no_match_returns_err() {
        let r = cmd_freeze("no_such_process_xyz_99999", &empty_frozen());
        assert!(r.starts_with("ERR"), "expected ERR, got: {r}");
    }

    #[test]
    fn restore_no_match_returns_err() {
        let r = cmd_restore("no_such_process_xyz_99999", &empty_checkpointed());
        assert!(r.starts_with("ERR"), "expected ERR, got: {r}");
    }

    #[test]
    fn info_no_match_returns_err() {
        let r = cmd_info(
            "no_such_process_xyz_99999",
            &empty_frozen(),
            &empty_checkpointed(),
            &empty_throttle(),
            &crate::config::test_config(),
        );
        assert!(r.starts_with("ERR"), "expected ERR, got: {r}");
    }

    #[test]
    fn kill_no_match_returns_err() {
        let r = cmd_kill("no_such_process_xyz_99999", &empty_events());
        assert!(r.starts_with("ERR"), "expected ERR, got: {r}");
    }

    #[test]
    fn events_empty_returns_ok() {
        let r = cmd_events(&empty_events());
        assert!(r.starts_with("OK"), "expected OK, got: {r}");
    }

    #[test]
    fn events_records_and_retrieves() {
        let log = empty_events();
        events::push(&log, mgd_common::logger::LogAction::Freeze, Pid(42), "test_proc", "frozen");
        let r = cmd_events(&log);
        assert!(r.contains("FREEZE"), "expected FREEZE in: {r}");
        assert!(r.contains("test_proc"), "expected test_proc in: {r}");
    }

    #[test]
    fn ps_returns_ok() {
        let r = cmd_ps(&empty_frozen(), &empty_throttle(), &crate::config::test_config());
        assert!(r.starts_with("OK"), "expected OK, got: {r}");
    }
}
