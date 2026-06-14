use std::fs;

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
