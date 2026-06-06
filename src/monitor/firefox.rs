//! Firefox preventive-memory watcher.
//! Firefox preventive GC watcher.
//!
//! Firefox doesn't release RSS after tabs close. At Normal pressure only, nudge
//! its internal GC via SIGUSR1 to keep it lean. Under pressure the evictor
//! handles Firefox via the priority system; the two must not act on it at once.

use crate::monitor::process::Process;

/// Firefox content-process comm names (15-char truncated). Mirror the
/// `browser-isolated` regex in priorities.toml.
const CONTENT_NAMES: &[&str] = &[
    "Isolated",
    "Isolated Servic",
    "Isolated Web Co",
    "Privileged Cont",
    "Socket Process",
    "RDD Process",
    "Utility Process",
    "Web Content",
    "WebExtensions",
];

/// True for the main `firefox` process or any of its content processes.
fn is_firefox_related(name: &str) -> bool {
    name == "firefox" || CONTENT_NAMES.contains(&name)
}

/// Total RSS (KB) across all Firefox processes (main + content), plus the list of
/// their PIDs. Reuses the already-collected `Process` slice — does not touch /proc.
pub fn firefox_total_rss_kb(procs: &[Process]) -> (u64, Vec<u32>) {
    let mut total = 0u64;
    let mut pids = Vec::new();
    for p in procs.iter().filter(|p| is_firefox_related(&p.name)) {
        total = total.saturating_add(p.rss_kb);
        pids.push(p.pid);
    }
    (total, pids)
}

/// Send SIGUSR1 to the main Firefox process (content processes are left alone).
/// SIGUSR1 triggers Firefox's internal GC cycle — non-disruptive, Firefox keeps
/// running. The main process is the one named `firefox` with the lowest PID
/// (content processes are forked later, so they always have higher PIDs).
/// Returns the signalled PID, or Err if there is no main process / the signal failed.
pub fn trigger_firefox_gc(procs: &[Process]) -> Result<u32, String> {
    let main_pid = procs.iter()
        .filter(|p| p.name == "firefox")
        .map(|p| p.pid)
        .min()
        .ok_or_else(|| "no main firefox process found".to_string())?;

    // SIGUSR1 via libc::kill — same primitive as executor/killer.rs.
    let rc = unsafe { libc::kill(main_pid as i32, libc::SIGUSR1) };
    if rc == 0 {
        Ok(main_pid)
    } else {
        Err(format!("SIGUSR1 to pid {main_pid} failed: {}", std::io::Error::last_os_error()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, name: &str, rss_kb: u64) -> Process {
        Process { pid, name: name.into(), exe_basename: None, rss_kb, swap_kb: 0, oom_score: 0 }
    }

    #[test]
    fn sums_main_and_content_processes() {
        let procs = vec![
            proc(100, "firefox", 1_000_000),
            proc(200, "Isolated Web Co", 500_000),
            proc(201, "Web Content", 300_000),
            proc(300, "konsole", 999_999), // unrelated, excluded
        ];
        let (total, pids) = firefox_total_rss_kb(&procs);
        assert_eq!(total, 1_800_000);
        assert_eq!(pids, vec![100, 200, 201]);
    }

    #[test]
    fn no_firefox_yields_empty() {
        let procs = vec![proc(300, "konsole", 1000)];
        let (total, pids) = firefox_total_rss_kb(&procs);
        assert_eq!(total, 0);
        assert!(pids.is_empty());
    }

    #[test]
    fn gc_errors_when_no_main_process() {
        // Only content processes present — no main `firefox` to signal.
        let procs = vec![proc(200, "Isolated Web Co", 500_000)];
        assert!(trigger_firefox_gc(&procs).is_err());
    }
}
