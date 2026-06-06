use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::error::MgdError;

#[derive(Debug, Clone)]
pub struct Process {
    pub pid: u32,
    pub name: String,
    /// Basename of /proc/PID/exe — untruncated, used for .desktop category lookup.
    pub exe_basename: Option<String>,
    pub rss_kb: u64,
    pub swap_kb: u64,
    pub oom_score: i32,
}

/// Returns true if a process belongs to our user session and we can signal it.
/// Combines UID ownership check with cgroup placement (user.slice).
fn is_user_managed(pid: u32) -> bool {
    let our_uid = unsafe { libc::geteuid() };

    let proc_dir = format!("/proc/{pid}");
    let uid_matches = match fs::metadata(&proc_dir) {
        Ok(meta) => meta.uid() == our_uid,
        Err(_) => return false,
    };
    if !uid_matches {
        return false;
    }

    let Ok(cgroup) = fs::read_to_string(format!("/proc/{pid}/cgroup")) else {
        return false;
    };
    cgroup.lines().any(|line| line.contains("/user.slice/") || line.contains("/user@"))
}

/// Read all user processes from /proc, excluding ourselves and system processes
pub fn list_processes() -> Vec<Process> {
    let own_pid = std::process::id();
    let Ok(entries) = fs::read_dir("/proc") else {
        return vec![];
    };

    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok().map(|pid| (pid, e.path())))
        .filter(|(pid, _)| *pid != own_pid && is_user_managed(*pid))
        .filter_map(|(pid, path)| read_process(pid, &path).ok())
        .collect()
}

fn read_process(pid: u32, path: &Path) -> Result<Process, MgdError> {
    let status = fs::read_to_string(path.join("status"))?;

    let name = parse_status_field(&status, "Name:")
        .unwrap_or_else(|| "unknown".to_string());

    let rss_kb = parse_status_kb(&status, "VmRSS:")
        .unwrap_or(0);

    let swap_kb = parse_status_kb(&status, "VmSwap:")
        .unwrap_or(0);

    let oom_score = fs::read_to_string(path.join("oom_score"))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(0);

    let exe_basename = fs::read_link(path.join("exe"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));

    Ok(Process { pid, name, exe_basename, rss_kb, swap_kb, oom_score })
}

/// Sum of utime+stime (clock ticks) for a process, from /proc/<pid>/stat.
/// Used for idle detection — sample twice `n` secs apart and compare the delta;
/// a zero delta means the process burned no CPU in that window.
pub fn cpu_jiffies(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_cpu_jiffies(&stat)
}

/// Parse utime+stime from a /proc/<pid>/stat line. The comm field (2nd) is
/// wrapped in parens and may itself contain spaces or ')', so split after the
/// final ')': the remaining whitespace fields start at `state` (field 3). The
/// index offset is therefore (1-indexed field − 3): utime (14) → index 11,
/// stime (15) → index 12.
fn parse_cpu_jiffies(stat: &str) -> Option<u64> {
    let rparen = stat.rfind(')')?;
    let fields: Vec<&str> = stat[rparen + 1..].split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    utime.checked_add(stime)
}

fn parse_status_field(status: &str, field: &str) -> Option<String> {
    status.lines()
        .find(|l| l.starts_with(field))?
        .split_whitespace()
        .nth(1)
        .map(|s| s.to_string())
}

fn parse_status_kb(status: &str, field: &str) -> Option<u64> {
    status.lines()
        .find(|l| l.starts_with(field))?
        .split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cpu_jiffies_simple() {
        // fields 1..15: pid comm state ppid pgrp session tty_nr tpgid flags
        //               minflt cminflt majflt cmajflt utime stime
        let stat = "1234 (bash) S 1 1234 1234 0 -1 0 100 0 0 0 50 25 0 0\n";
        assert_eq!(parse_cpu_jiffies(stat), Some(75));
    }

    #[test]
    fn test_cpu_jiffies_comm_with_spaces_and_parens() {
        // comm contains a space and a ')': must split after the *final* ')'.
        let stat = "42 (weird )(name) S 1 42 42 0 -1 0 10 0 0 0 7 3 0 0";
        assert_eq!(parse_cpu_jiffies(stat), Some(10));
    }

    #[test]
    fn test_cpu_jiffies_truncated() {
        let stat = "1 (init) S 0";
        assert_eq!(parse_cpu_jiffies(stat), None);
    }
}
