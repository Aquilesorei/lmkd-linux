use std::fs;

/// Read utime+stime ticks from /proc/<pid>/stat. Returns None on any parse failure.
pub fn read_proc_cpu_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // After the last ')': state(0) ppid(1) ... utime(11) stime(12)
    let after_comm = stat.rsplit_once(") ")?.1;
    let mut it = after_comm.split_whitespace().skip(11);
    let utime: u64 = it.next()?.parse().ok()?;
    let stime: u64 = it.next()?.parse().ok()?;
    Some(utime + stime)
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
