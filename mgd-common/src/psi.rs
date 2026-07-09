use std::fs;

/// The global kernel PSI memory pressure file. Always present when
/// `CONFIG_PSI=y`; readable and writable without elevated privileges.
pub const GLOBAL_PSI: &str = "/proc/pressure/memory";

/// Per-session cgroup `memory.pressure` path for the current user. Preferred
/// over the global file because it only reflects processes in this user session,
/// avoiding noise from other users or system services.
pub fn cgroup_psi_path() -> String {
    let uid = unsafe { libc::getuid() };
    format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/memory.pressure")
}


fn is_usable_psi_file(path: &str) -> bool {
    fs::read_to_string(path)
        .map(|c| c.starts_with("some "))
        .unwrap_or(false)
}

/// Returns the best available PSI source: the per-session cgroup file when
/// readable, falling back to the global `/proc/pressure/memory`. This is
/// resolved once at daemon startup and reused across cycles.
pub fn resolve_pressure_source() -> String {
    let cgroup = cgroup_psi_path();
    if is_usable_psi_file(&cgroup) {
        cgroup
    } else {
        GLOBAL_PSI.to_string()
    }
}

/// Returns `true` if `path` can be opened read-write, which is the
/// precondition for arming a kernel PSI trigger on it. The cgroup file is
/// root-owned on systemd < 254; if this returns `false`, the daemon falls
/// back to the epoll trigger or 5 s polling.
pub fn trigger_armable(path: &str) -> bool {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .is_ok()
}

/// Walks the cgroup hierarchy upward from `/proc/self/cgroup` and returns the
/// highest-level `memory.pressure` file that is trigger-armable (read-write).
/// On kernel 7.x, `/proc/pressure/memory` trigger writes return EINVAL
/// unconditionally, so cgroup-level arming is the only working path.
/// Returns `None` when no writable pressure file is found (daemon falls back
/// to 5 s polling).
pub fn find_trigger_path() -> Option<String> {
    let cgroup_content = fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = cgroup_content
        .lines()
        .find(|l| l.starts_with("0::"))?
        .trim_start_matches("0::")
        .trim_matches('/');

    if rel.is_empty() {
        return None;
    }

    let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    let mut best: Option<String> = None;
    // Leaf → root: stop at first non-writable level; keep the highest writable.
    for len in (1..=parts.len()).rev() {
        let path = format!("/sys/fs/cgroup/{}/memory.pressure", parts[..len].join("/"));
        if trigger_armable(&path) {
            best = Some(path);
        } else {
            break;
        }
    }
    best
}

pub fn parse_kv(s: &str, prefix: &str) -> Result<f64, crate::error::MgdError> {
    s.strip_prefix(prefix)
        .ok_or_else(|| crate::error::MgdError::Parse(format!("expected '{prefix}', got '{s}'")))?
        .parse::<f64>()
        .map_err(crate::error::MgdError::from)
}

pub fn parse_kv_u64(s: &str, prefix: &str) -> Result<u64, crate::error::MgdError> {
    s.strip_prefix(prefix)
        .ok_or_else(|| crate::error::MgdError::Parse(format!("expected '{prefix}', got '{s}'")))?
        .parse::<u64>()
        .map_err(|e| crate::error::MgdError::Parse(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_usable_psi_file() {
        // Missing file -> unusable.
        assert!(!is_usable_psi_file("/nonexistent/memory.pressure"));

        // Valid PSI content -> usable.
        let dir = std::env::temp_dir();
        let good = dir.join("mgd_test_psi_good");
        fs::write(&good, "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n").unwrap();
        assert!(is_usable_psi_file(good.to_str().unwrap()));

        // Garbage content -> unusable.
        let bad = dir.join("mgd_test_psi_bad");
        fs::write(&bad, "not psi output\n").unwrap();
        assert!(!is_usable_psi_file(bad.to_str().unwrap()));

        let _ = fs::remove_file(good);
        let _ = fs::remove_file(bad);
    }

    #[test]
    fn test_cgroup_path_shape() {
        let p = cgroup_psi_path();
        assert!(p.starts_with("/sys/fs/cgroup/user.slice/user-"));
        assert!(p.ends_with("/memory.pressure"));
    }
}
