use std::fs;

/// Read both cpu ticks and major page faults from /proc/<pid>/stat in a single read.
/// After the last ')': state(0) ppid(1)...minflt(7) cminflt(8) majflt(9)...utime(11) stime(12)
/// Returns (Option<utime+stime>, majflt).
pub fn read_proc_stat(pid: u32) -> (Option<u64>, u64) {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s,
        Err(_) => return (None, 0),
    };
    let after_comm = match stat.rsplit_once(") ") {
        Some((_, rest)) => rest,
        None => return (None, 0),
    };
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let majflt = fields.get(9).and_then(|s| s.parse().ok()).unwrap_or(0);
    let cpu_ticks = fields.get(11)
        .zip(fields.get(12))
        .and_then(|(u, s)| u.parse::<u64>().ok().zip(s.parse::<u64>().ok()))
        .map(|(u, s)| u + s);
    (cpu_ticks, majflt)
}

/// Read utime+stime ticks from /proc/<pid>/stat. Returns None on any parse failure.
pub fn read_proc_cpu_ticks(pid: u32) -> Option<u64> {
    read_proc_stat(pid).0
}

/// Read cumulative major page faults from /proc/<pid>/stat. Returns 0 on any parse failure.
pub fn read_proc_majflt(pid: u32) -> u64 {
    read_proc_stat(pid).1
}

/// Return true if the cgroup file content places the process in the user session.
/// Checks both "/user.slice/" and "/user@...service" patterns.
pub fn is_cgroup_in_user_slice(cgroup_content: &str) -> bool {
    cgroup_content.lines().any(|line| {
        line.contains("/user.slice/") || (line.contains("/user@") && line.contains(".service"))
    })
}

/// Finds the PID of a given process name belonging to the current user.
pub fn find_pid_by_name(target_name: &str) -> Option<u32> {
    let own_uid = unsafe { libc::geteuid() };
    let entries = fs::read_dir("/proc").ok()?;
    for entry in entries.filter_map(|e| e.ok()) {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
        
        let status_path = entry.path().join("status");
        if let Ok(status) = fs::read_to_string(&status_path) {
            let mut name = "";
            let mut uid = 0;
            for line in status.lines() {
                if let Some(n) = line.strip_prefix("Name:") {
                    name = n.trim();
                } else if let Some(u) = line.strip_prefix("Uid:") {
                    uid = u.split_whitespace().next().unwrap_or("0").parse().unwrap_or(0);
                }
            }
            if name == target_name && uid == own_uid {
                return Some(pid);
            }
        }
    }
    None
}
