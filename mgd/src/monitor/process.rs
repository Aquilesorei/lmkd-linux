use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use mgd_common::error::MgdError;

#[derive(Debug, Clone)]
pub struct Process {
    pub pid: u32,
    pub name: String,
    /// Basename of /proc/PID/exe — untruncated, used for .desktop category lookup.
    pub exe_basename: Option<String>,
    pub rss_kb: u64,
    pub swap_kb: u64,
    pub oom_score: i32,
    /// Unified cgroup v2 path (e.g. `/user.slice/user-1000.slice/…`), cached to
    /// avoid re-reading `/proc/PID/cgroup` multiple times per evictor cycle.
    pub cgroup_path: Option<String>,
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
        .filter(|(pid, _)| *pid != own_pid)
        .filter_map(|(pid, path)| read_process(pid, &path).ok())
        .collect()
}

fn read_process(pid: u32, path: &Path) -> Result<Process, MgdError> {
    let our_uid = unsafe { libc::geteuid() };
    let meta = fs::metadata(path)?;
    if meta.uid() != our_uid {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "not owned by user").into());
    }

    let cgroup_content = fs::read_to_string(path.join("cgroup"))?;
    if !cgroup_content.lines().any(|line| line.contains("/user.slice/") || (line.contains("/user@") && line.contains(".service"))) {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "not in user cgroup").into());
    }

    let mut cgroup_path = None;
    for line in cgroup_content.lines() {
        if let Some(p) = line.strip_prefix("0::") {
            let p = p.trim();
            if p != "/" {
                cgroup_path = Some(p.to_string());
                break;
            }
        }
    }

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

    Ok(Process { pid, name, exe_basename, rss_kb, swap_kb, oom_score, cgroup_path })
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

