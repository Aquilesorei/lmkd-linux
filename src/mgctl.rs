/// mgctl — control client for the mgd daemon.
///
/// Talks to the running mgd daemon via its Unix domain socket for live
/// introspection, and shells out to `systemctl --user` for lifecycle control
/// (which can't go over the socket — start/stop/restart must work when the
/// daemon is down, or must outlive the daemon process itself).
///
/// Usage:
///   mgctl status              — show current pressure + frozen count   (socket)
///   mgctl list                — list all frozen processes              (socket)
///   mgctl unfreeze <pid|name> — manually unfreeze by PID or name        (socket)
///   mgctl reload              — hot-reload daemon config without restart (socket)
///   mgctl restart             — restart the mgd service              (systemctl)
///   mgctl start               — start the mgd service                (systemctl)
///   mgctl stop                — stop the mgd service                 (systemctl)
///   mgctl service             — show systemd unit status             (systemctl)
///   mgctl logs [-f]           — show daemon logs (-f to follow)      (journalctl)
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

const SERVICE: &str = "mgd.service";

fn usage() {
    eprintln!("Usage:");
    eprintln!("  mgctl status                  (daemon: pressure + frozen count)");
    eprintln!("  mgctl list");
    eprintln!("  mgctl unfreeze <pid|name>");
    eprintln!("  mgctl reload");
    eprintln!("  mgctl restart | start | stop");
    eprintln!("  mgctl service                 (systemd unit state)");
    eprintln!("  mgctl logs [-f]");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        usage();
        std::process::exit(1);
    }

    let cmd = args[1].as_str();

    // Lifecycle commands are handled out-of-band via systemd, not the socket.
    match cmd {
        "restart" | "start" | "stop" => std::process::exit(run_systemctl(cmd)),
        "service" => std::process::exit(run_service_status()),
        "logs" => std::process::exit(run_logs(&args[2..])),
        _ => {}
    }

    let request = match cmd {
        "status"   => "status".to_string(),
        "list"     => "list".to_string(),
        "reload"   => "reload".to_string(),
        "unfreeze" => {
            if args.len() < 3 {
                eprintln!("Usage: mgctl unfreeze <pid|name>");
                std::process::exit(1);
            }
            format!("unfreeze {}", args[2])
        }
        other => {
            eprintln!("mgctl: unknown command '{other}'");
            usage();
            std::process::exit(1);
        }
    };

    let path = lmkd_linux::socket_path();
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mgctl: cannot connect to mgd socket at {path:?}: {e}");
            eprintln!("       Is mgd running? (systemctl --user status mgd)");
            std::process::exit(1);
        }
    };

    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

    if let Err(e) = writeln!(stream, "{request}") {
        eprintln!("mgctl: write error: {e}");
        std::process::exit(1);
    }

    // Read entire response until server closes the connection — necessary because
    // multi-entry responses (e.g. "list") embed newlines inside the OK payload.
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    if let Err(e) = reader.read_to_string(&mut response) {
        eprintln!("mgctl: read error: {e}");
        std::process::exit(1);
    }

    let response = response.trim();
    if let Some(rest) = response.strip_prefix("OK ") {
        println!("{rest}");
    } else if let Some(rest) = response.strip_prefix("ERR ") {
        eprintln!("error: {rest}");
        std::process::exit(1);
    } else {
        println!("{response}");
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
