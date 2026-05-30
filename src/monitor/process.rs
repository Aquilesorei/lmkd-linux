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
