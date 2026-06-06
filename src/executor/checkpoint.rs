use std::process::Command;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;

/// criu locations, probed in order. Never a PATH search: a capped criu invoked
/// by bare name would let a planted criu run with the caps. All root-controlled.
const CRIU_CANDIDATES: &[&str] = &[
    "/usr/sbin/criu",
    "/usr/bin/criu",
    "/sbin/criu",
    "/bin/criu",
    "/usr/local/sbin/criu",
    "/usr/local/bin/criu",
];

static CRIU_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Absolute path to criu, resolved once. None if not installed.
pub fn criu_path() -> Option<&'static PathBuf> {
    CRIU_PATH
        .get_or_init(|| resolve_in(CRIU_CANDIDATES))
        .as_ref()
}

/// First existing+executable candidate. Pure, for testing.
fn resolve_in(candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| is_executable(p))
}

fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

/// Whether criu stderr looks like a capability shortfall vs a per-process restore
/// failure — drives the "run setcap" hint. Inference-based to avoid a libcap dep.
pub fn looks_like_privilege_error(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("operation not permitted")
        || s.contains("eperm")
        || s.contains("cap_sys_ptrace")
        || s.contains("cap_checkpoint_restore")
        || s.contains("permission denied")
        || (s.contains("ptrace") && s.contains("denied"))
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct CheckpointResult {
    pub pid: u32,
    pub success: bool,
    pub snapshot_dir: Option<PathBuf>,
    pub error: Option<String>,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct RestoreResult {
    pub success: bool,
    pub error: Option<String>,
}

/// CRIU dump to disk, then SIGKILL on success.
pub fn checkpoint(pid: u32, name: &str) -> CheckpointResult {
    // comm may contain '/' (prctl-set), which would escape the snapshot dir.
    let safe_name: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .take(32)
        .collect();
    let snapshot_dir = crate::util::home_dir()
        .join(format!(".local/share/mgd/snapshots/{}_{}", pid, safe_name));

    if let Err(e) = fs::create_dir_all(&snapshot_dir) {
        return CheckpointResult {
            pid,
            success: false,
            snapshot_dir: None,
            error: Some(format!("Failed to create snapshot dir: {e}")),
        };
    }

    let snapshot_dir_str = match snapshot_dir.to_str() {
        Some(s) => s.to_string(),
        None => {
            return CheckpointResult {
                pid,
                success: false,
                snapshot_dir: None,
                error: Some("snapshot path contains non-UTF8 characters".to_string()),
            };
        }
    };

    let Some(criu) = criu_path() else {
        let _ = fs::remove_dir_all(&snapshot_dir);
        return CheckpointResult {
            pid,
            success: false,
            snapshot_dir: None,
            error: Some("criu not found (install criu to enable checkpointing)".to_string()),
        };
    };

    let output = Command::new(criu)
        .args([
            "dump",
            "--tree", &pid.to_string(),
            "--images-dir", &snapshot_dir_str,
            "--shell-job",
            "--leave-stopped",  // stop process after dump (we kill it next)
            "--ext-unix-sk",
            "--tcp-established",
            "--file-locks",
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            // State saved — kill the process.
            let kill_ret = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            if kill_ret != 0 {
                let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if errno != libc::ESRCH { // ESRCH = already gone
                    return CheckpointResult {
                        pid,
                        success: false,
                        snapshot_dir: None,
                        error: Some(format!("CRIU succeeded but SIGKILL failed: {}", io::Error::last_os_error())),
                    };
                }
            }
            CheckpointResult {
                pid,
                success: true,
                snapshot_dir: Some(snapshot_dir),
                error: None,
            }
        }
        Ok(out) => {
            let _ = fs::remove_dir_all(&snapshot_dir);
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let error = if looks_like_privilege_error(&stderr) {
                format!(
                    "criu lacks privilege (run: setcap cap_checkpoint_restore,cap_sys_ptrace+ep {}): {stderr}",
                    criu.display()
                )
            } else {
                stderr
            };
            CheckpointResult {
                pid,
                success: false,
                snapshot_dir: None,
                error: Some(error),
            }
        }
        Err(e) => {
            CheckpointResult {
                pid,
                success: false,
                snapshot_dir: None,
                error: Some(format!("Failed to run criu: {e}")),
            }
        }
    }
}

/// Restore a previously checkpointed process from disk.
#[allow(dead_code)]
pub fn restore(snapshot_dir: &std::path::Path) -> RestoreResult {
    if !snapshot_dir.exists() {
        return RestoreResult {
            success: false,
            error: Some(format!("Snapshot dir not found: {}", snapshot_dir.display())),
        };
    }

    let snapshot_dir_str = match snapshot_dir.to_str() {
        Some(s) => s.to_string(),
        None => {
            return RestoreResult {
                success: false,
                error: Some("snapshot path contains non-UTF8 characters".to_string()),
            };
        }
    };

    let Some(criu) = criu_path() else {
        return RestoreResult {
            success: false,
            error: Some("criu not found (install criu to enable restore)".to_string()),
        };
    };

    let output = Command::new(criu)
        .args([
            "restore",
            "--images-dir", &snapshot_dir_str,
            "--shell-job",
            "--restore-detached",
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            RestoreResult { success: true, error: None }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let error = if looks_like_privilege_error(&stderr) {
                format!(
                    "criu restore lacks privilege (run: setcap cap_checkpoint_restore,cap_sys_ptrace+ep {}; \
                     add cap_net_admin for --tcp-established): {stderr}",
                    criu.display()
                )
            } else {
                format!("CRIU restore failed: {stderr}")
            };
            RestoreResult {
                success: false,
                error: Some(error),
            }
        }
        Err(e) => {
            RestoreResult {
                success: false,
                error: Some(format!("Failed to run criu: {e}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_in_finds_first_executable() {
        let candidates = ["/nonexistent/criu", "/bin/sh"];
        assert_eq!(resolve_in(&candidates), Some(PathBuf::from("/bin/sh")));
    }

    #[test]
    fn resolve_in_none_when_all_absent() {
        let candidates = ["/nope/a", "/nope/b"];
        assert_eq!(resolve_in(&candidates), None);
    }

    #[test]
    fn resolve_in_respects_order() {
        let candidates = ["/bin/sh", "/bin/cat"]; // earlier wins
        assert_eq!(resolve_in(&candidates), Some(PathBuf::from("/bin/sh")));
    }

    #[test]
    fn resolve_in_empty_is_none() {
        assert_eq!(resolve_in(&[]), None);
    }

    #[test]
    fn privilege_error_detection() {
        assert!(looks_like_privilege_error("Operation not permitted"));
        assert!(looks_like_privilege_error("can't seize task: Operation not permitted"));
        assert!(looks_like_privilege_error("Unable to ptrace: permission denied"));
        assert!(looks_like_privilege_error("requires CAP_SYS_PTRACE"));
        // A normal restore failure must NOT be flagged as a privilege issue.
        assert!(!looks_like_privilege_error("Can't restore tcp connection: address in use"));
        assert!(!looks_like_privilege_error("image file not found"));
    }
}
