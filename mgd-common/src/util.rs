use std::path::PathBuf;

/// Return the current user's home directory.
///
/// Uses `$HOME`; falls back to `/tmp` if unset (daemon safety net).
pub fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// Current time as seconds since the Unix epoch. Returns 0 on the (impossible) pre-epoch error.
pub fn unix_timestamp_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read the unified cgroup v2 path for `pid` from `/proc/<pid>/cgroup`.
/// Returns `None` if the file is unreadable or the process is in the root cgroup.
pub fn read_process_cgroup_path(pid: u32) -> Option<String> {
    let content = std::fs::read_to_string(format!("/proc/{}/cgroup", pid)).ok()?;
    for line in content.lines() {
        if let Some(path) = line.strip_prefix("0::") {
            let path = path.trim();
            if path != "/" {
                return Some(path.to_string());
            }
        }
    }
    None
}
