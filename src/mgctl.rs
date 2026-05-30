/// mgctl — control client for the mgd daemon.
///
/// Talks to the running mgd daemon via its Unix domain socket.
/// All commands are forwarded and the response is printed.
///
/// Usage:
///   mgctl status              — show current pressure + frozen count
///   mgctl list                — list all frozen processes
///   mgctl unfreeze <pid|name> — manually unfreeze by PID or name substring
///   mgctl reload              — hot-reload daemon config without restart
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage:");
        eprintln!("  mgctl status");
        eprintln!("  mgctl list");
        eprintln!("  mgctl unfreeze <pid|name>");
        eprintln!("  mgctl reload");
        std::process::exit(1);
    }

    let cmd = args[1].as_str();

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
