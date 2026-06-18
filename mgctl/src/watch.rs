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
    match crate::query_socket(cmd, 3) {
        Ok(res) => res,
        Err(e) => {
            if e.contains("cannot connect") {
                "  (daemon not running)".to_string()
            } else if e.contains("write error") {
                "  (write error)".to_string()
            } else {
                format!("  {e}")
            }
        }
    }
}
