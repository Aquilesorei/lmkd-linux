/// mgctl — control client for the mgd daemon.
mod calibrate;
mod doctor;
mod watch;
///
/// Talks to the running mgd daemon via its Unix domain socket for live
/// introspection, and shells out to `systemctl --user` for lifecycle control
/// (which can't go over the socket — start/stop/restart must work when the
/// daemon is down, or must outlive the daemon process itself).
///
/// Usage:
///   mgctl status              — pressure, avail RAM, swap, managed counts  (socket)
///   mgctl list                — frozen, checkpointed, and throttled procs  (socket)
///   mgctl ps                  — all live monitored processes with CPU/RSS   (socket)
///   mgctl info <pid|name>     — per-process detail: RSS, swap, priority    (socket)
///   mgctl events              — recent daemon actions (ring buffer)         (socket)
///   mgctl watch               — live refreshing dashboard                  (socket)
///   mgctl freeze <pid|name>   — manually freeze by PID or name             (socket)
///   mgctl unfreeze <pid|name> — manually unfreeze by PID or name           (socket)
///   mgctl restore <pid|name>  — restore a checkpointed process             (socket)
///   mgctl kill <pid|name>     — manually SIGKILL by PID or name            (socket)
///   mgctl reload              — hot-reload daemon config without restart    (socket)
///   mgctl restart             — restart the mgd service                 (systemctl)
///   mgctl start               — start the mgd service                   (systemctl)
///   mgctl stop                — stop the mgd service                    (systemctl)
///   mgctl service             — show systemd unit state                (systemctl)
///   mgctl logs [-f]           — show daemon logs (-f to follow)         (journalctl)

use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

const SERVICE: &str = "mgd.service";

fn usage() {
    eprintln!("Usage:");
    eprintln!("  mgctl status                  (pressure, avail RAM, swap, managed process counts)");
    eprintln!("  mgctl list                    (all frozen, checkpointed, and throttled processes)");
    eprintln!("  mgctl ps                      (all live monitored processes: RSS, swap, CPU, priority)");
    eprintln!("  mgctl info <pid|name>         (per-process detail: RSS, swap, priority, state)");
    eprintln!("  mgctl events                  (recent daemon actions: freeze/kill/checkpoint history)");
    eprintln!("  mgctl watch                   (live refreshing dashboard: status + ps + events)");
    eprintln!("  mgctl freeze <pid|name>       (manually freeze a process)");
    eprintln!("  mgctl unfreeze <pid|name>     (manually unfreeze a frozen process)");
    eprintln!("  mgctl restore <pid|name>      (restore a checkpointed process)");
    eprintln!("  mgctl kill <pid|name>         (manually SIGKILL a process)");
    eprintln!("  mgctl reload                  (hot-reload config without restart)");
    eprintln!("  mgctl restart | start | stop");
    eprintln!("  mgctl service                 (systemd unit state)");
    eprintln!("  mgctl logs [-f]");
    eprintln!("  mgctl calibrate [--dry-run]   (derive per-machine thresholds via active sweep)");
    eprintln!("  mgctl calibrate --apply       (apply active-sweep calibration)");
    eprintln!("  mgctl calibrate --passive-apply  (apply daemon passive [psi] suggestion)");
    eprintln!("  mgctl doctor                  (environment + feature report)");
    eprintln!("  mgctl spike-status            (spike mode: tracked heavy processes + frozen victims)");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        usage();
        std::process::exit(1);
    }

    let cmd = args[1].as_str();

    // Lifecycle commands and standalone commands are handled without the socket.
    match cmd {
        "restart" | "start" | "stop" => std::process::exit(run_systemctl(cmd)),
        "service"   => std::process::exit(run_service_status()),
        "logs"      => std::process::exit(run_logs(&args[2..])),
        "calibrate" => std::process::exit(calibrate::run(&args[2..])),
        "doctor"    => std::process::exit(doctor::run()),
        "watch"     => std::process::exit(watch::run()),
        _ => {}
    }

    let request = match cmd {
        "status"        => "status".to_string(),
        "list"          => "list".to_string(),
        "ps"            => "ps".to_string(),
        "events"        => "events".to_string(),
        "reload"        => "reload".to_string(),
        "spike-status"  => "spike-status".to_string(),
        "unfreeze" | "freeze" | "restore" | "info" | "kill" => {
            if args.len() < 3 {
                eprintln!("Usage: mgctl {cmd} <pid|name>");
                std::process::exit(1);
            }
            format!("{cmd} {}", args[2])
        }
        other => {
            eprintln!("mgctl: unknown command '{other}'");
            usage();
            std::process::exit(1);
        }
    };

    match query_socket(&request, 5) {
        Ok(res) => {
            println!("{res}");
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

pub(crate) fn query_socket(cmd: &str, timeout_secs: u64) -> Result<String, String> {
    let path = mgd_common::socket::socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        format!(
            "mgctl: cannot connect to mgd socket at {path:?}: {e}\n       Is mgd running? (systemctl --user status mgd)"
        )
    })?;

    stream.set_write_timeout(Some(Duration::from_secs(timeout_secs))).ok();
    stream.set_read_timeout(Some(Duration::from_secs(timeout_secs))).ok();

    writeln!(stream, "{cmd}").map_err(|e| format!("mgctl: write error: {e}"))?;

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_to_string(&mut response).map_err(|e| format!("mgctl: read error: {e}"))?;

    let response = response.trim();
    if let Some(rest) = response.strip_prefix("OK ") {
        Ok(rest.to_string())
    } else if let Some(rest) = response.strip_prefix("ERR ") {
        Err(format!("error: {rest}"))
    } else {
        Ok(response.to_string())
    }
}

/// Run `systemctl --user <verb> mgd.service`, inheriting stdio so the user sees
/// systemd's own output/errors. Returns the child's exit code (0 on success).
fn run_systemctl(verb: &str) -> i32 {
    match std::process::Command::new("systemctl")
        .args(["--user", verb, SERVICE])
        .status()
    {
        Ok(s) => {
            if s.success() {
                // `start`/`stop` are silent on success; echo a confirmation.
                println!("mgd {verb}: ok");
                0
            } else {
                s.code().unwrap_or(1)
            }
        }
        Err(e) => {
            eprintln!("mgctl: failed to run systemctl: {e}");
            1
        }
    }
}

/// Show the systemd unit state via `systemctl --user status` (active/inactive,
/// main PID, uptime, and the last few log lines). Uses --no-pager so output
/// isn't swallowed by a pager in non-interactive use.
fn run_service_status() -> i32 {
    match std::process::Command::new("systemctl")
        .args(["--user", "status", SERVICE, "--no-pager"])
        .status()
    {
        // systemctl status exits non-zero when the unit is inactive/failed; that
        // is informational here, not a mgctl error — pass the code through.
        Ok(s) => s.code().unwrap_or(0),
        Err(e) => {
            eprintln!("mgctl: failed to run systemctl: {e}");
            1
        }
    }
}

/// Run `journalctl --user -u mgd.service`, passing through `-f`/`--follow` if
/// given. Inherits stdio so logs stream straight to the terminal.
fn run_logs(rest: &[String]) -> i32 {
    let mut cmd = std::process::Command::new("journalctl");
    cmd.args(["--user", "-u", SERVICE]);
    match rest.first().map(String::as_str) {
        Some("-f") | Some("--follow") => { cmd.arg("-f"); }
        Some(other) => {
            eprintln!("mgctl logs: unknown option '{other}' (only -f/--follow supported)");
            return 1;
        }
        None => { cmd.args(["-n", "50", "--no-pager"]); }
    }
    match cmd.status() {
        Ok(s) => s.code().unwrap_or(0),
        Err(e) => {
            eprintln!("mgctl: failed to run journalctl: {e}");
            1
        }
    }
}
