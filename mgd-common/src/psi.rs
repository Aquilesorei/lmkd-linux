//! PSI source resolution shared by the daemon (`mgd`) and the diagnostic
//! client (`mgctl doctor`), so both always agree on which file is in use.

use std::fs;

/// System-wide PSI — always present when CONFIG_PSI is enabled.
pub const GLOBAL_PSI: &str = "/proc/pressure/memory";

/// The per-session PSI file under the systemd user-manager cgroup.
/// Pressure here reflects only this session, not system-wide noise.
pub fn cgroup_psi_path() -> String {
    let uid = unsafe { libc::getuid() };
    format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/memory.pressure")
}

/// A PSI file is usable if it can be read and looks like PSI output.
/// (A cgroup file may exist but return ENOTSUP when PSI is compiled out.)
pub fn is_usable_psi_file(path: &str) -> bool {
    fs::read_to_string(path)
        .map(|c| c.starts_with("some "))
        .unwrap_or(false)
}

/// Resolve the PSI file to read: per-session cgroup first, global fallback
/// (cgroup-v1 hosts or kernels without per-cgroup PSI).
pub fn resolve_pressure_source() -> String {
    let cgroup = cgroup_psi_path();
    if is_usable_psi_file(&cgroup) {
        cgroup
    } else {
        GLOBAL_PSI.to_string()
    }
}

/// Whether a kernel PSI trigger can be armed on `path` (requires opening
/// read-write; the cgroup file is root-owned on systemd < 254). Probes by
/// opening only — no trigger is registered, and the fd is dropped here.
pub fn trigger_armable(path: &str) -> bool {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .is_ok()
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
