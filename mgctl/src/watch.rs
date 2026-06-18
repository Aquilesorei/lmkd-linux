use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub fn run() -> i32 {
    loop {
        let status  = query("status");
        let managed = query("list");
        let ps      = query("ps");
        let events  = query("events");

        print!("\x1b[2J\x1b[H");
        println!("=== mgd watch ===  (Ctrl+C to exit)\n");
        println!("STATUS");
        println!("  {status}");
        println!();
        println!("MANAGED (frozen / checkpointed / throttled)");
        println!("{managed}");
        println!();
        println!("LIVE PROCESSES  (sorted by RSS)");
        println!("{ps}");
        println!();
        println!("RECENT EVENTS");
        println!("{events}");

        std::thread::sleep(Duration::from_secs(2));
    }
}

fn query(cmd: &str) -> String {
    let path = mgd_common::socket::socket_path();
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(_) => return "  (daemon not running)".to_string(),
    };
    stream.set_write_timeout(Some(Duration::from_secs(3))).ok();
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
    if writeln!(stream, "{cmd}").is_err() {
        return "  (write error)".to_string();
    }
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    let _ = reader.read_to_string(&mut response);
    let response = response.trim();
    if let Some(rest) = response.strip_prefix("OK ") {
        rest.to_string()
    } else if let Some(rest) = response.strip_prefix("ERR ") {
        format!("  error: {rest}")
    } else {
        response.to_string()
    }
}
