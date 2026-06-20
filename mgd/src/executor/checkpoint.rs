use std::collections::HashSet;
use std::process::Command;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

static HELPER_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
static CHECKPOINT_DISABLED: AtomicBool = AtomicBool::new(false);
static CHECKPOINT_FAILED_BINARIES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn failed_binaries() -> &'static Mutex<HashSet<String>> {
    CHECKPOINT_FAILED_BINARIES.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Record that a checkpoint dump failed for this binary name so future pressure
/// cycles skip the CRIU path for it and go straight to the prio-based fallback.
/// Session-permanent by design: retrying a failed dump under Critical pressure adds
/// I/O cost to the exact moment the system can least afford it, with no different outcome guaranteed.
pub fn mark_binary_failed(name: &str) {
    failed_binaries().lock().unwrap().insert(name.to_string());
}

/// System-level check: helper present + not globally disabled.
pub fn is_checkpoint_supported() -> bool {
    let _ = helper_path();
    !CHECKPOINT_DISABLED.load(Ordering::Relaxed)
}

/// Per-process eligibility: system check AND binary not in the per-name failed set.
pub fn is_checkpoint_eligible(name: &str) -> bool {
    is_checkpoint_supported() && !failed_binaries().lock().unwrap().contains(name)
}

/// Force disable checkpointing (e.g. after a privilege or security error)
pub fn disable_checkpoint() {
    CHECKPOINT_DISABLED.store(true, Ordering::Relaxed);
}

/// Absolute path to the mgd-checkpoint helper, resolved once. None if not found.
pub fn helper_path() -> Option<&'static PathBuf> {
    let path = HELPER_PATH.get_or_init(|| {
        // Probe /usr/local/bin/mgd-checkpoint first
        let system_path = PathBuf::from("/usr/local/bin/mgd-checkpoint");
        if is_executable(&system_path) {
            return Some(system_path);
        }
        // Probe ~/.local/bin/mgd-checkpoint next
        let local_path = mgd_common::util::home_dir().join(".local/bin/mgd-checkpoint");
        if is_executable(&local_path) {
            return Some(local_path);
        }
        None
    });
    if path.is_none() {
        CHECKPOINT_DISABLED.store(true, Ordering::Relaxed);
    }
    path.as_ref()
}

fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

/// Whether stderr looks like a capability/privilege shortfall or a security error
pub fn looks_like_privilege_error(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("operation not permitted")
        || s.contains("eperm")
        || s.contains("cap_sys_ptrace")
        || s.contains("cap_checkpoint_restore")
        || s.contains("permission denied")
        || (s.contains("ptrace") && s.contains("denied"))
        || s.contains("security error")
        || s.contains("capability warning")
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct CheckpointResult {
    pub pid: u32,
    pub success: bool,
    pub snapshot_dir: Option<PathBuf>,
    pub error: Option<String>,
}

impl CheckpointResult {
    pub fn ok(pid: u32, snapshot_dir: PathBuf) -> Self {
        Self { pid, success: true, snapshot_dir: Some(snapshot_dir), error: None }
    }

    pub fn err(pid: u32, error: impl Into<String>) -> Self {
        Self { pid, success: false, snapshot_dir: None, error: Some(error.into()) }
    }
}



#[derive(Debug)]
#[allow(dead_code)]
pub struct RestoreResult {
    pub success: bool,
    pub error: Option<String>,
}

impl RestoreResult {
    pub fn ok() -> Self {
        Self { success: true, error: None }
    }

    pub fn err(error: impl Into<String>) -> Self {
        Self { success: false, error: Some(error.into()) }
    }
}



/// Checkpoint a process using the mgd-checkpoint helper wrapper, then SIGKILL on success.
pub fn checkpoint(pid: u32, name: &str) -> CheckpointResult {
    // comm may contain '/' (prctl-set), which would escape the snapshot dir.
    let safe_name: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .take(32)
        .collect();
    let snapshot_dir = mgd_common::util::home_dir()
        .join(format!(".local/share/mgd/snapshots/{}_{}", pid, safe_name));

    if let Err(e) = fs::create_dir_all(&snapshot_dir) {
        return CheckpointResult::err(pid,format!("Failed to create snapshot dir: {e}"));
    }

    let snapshot_dir_str = match snapshot_dir.to_str() {
        Some(s) => s.to_string(),
        None => {
            return CheckpointResult::err(pid,"snapshot path contains non-UTF8 characters");
        }
    };

    let Some(helper) = helper_path() else {
        let _ = fs::remove_dir_all(&snapshot_dir);
        return CheckpointResult::err(
            pid,
            "mgd-checkpoint helper not found (please install it to enable checkpointing)"
        );
    };

    let output = Command::new(helper)
        .args([
            "dump",
            &pid.to_string(),
            &snapshot_dir_str,
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            // State saved — kill the process.
            let kill_ret = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            if kill_ret != 0 {
                let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if errno != libc::ESRCH { // ESRCH = already gone
                    return CheckpointResult::err(pid,format!("CRIU succeeded but SIGKILL failed: {}", io::Error::last_os_error()))
                }
            }
            CheckpointResult::ok(pid, snapshot_dir)
        }
        Ok(out) => {
            let _ = fs::remove_dir_all(&snapshot_dir);
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let is_priv_err = looks_like_privilege_error(&stderr) || out.status.code() == Some(2); // 2 is EXIT_SECURITY_FAIL
            if is_priv_err {
                disable_checkpoint();
            }
            let error = if is_priv_err {
                format!(
                    "mgd-checkpoint lacks privilege (run: sudo setcap cap_checkpoint_restore,cap_sys_ptrace,cap_net_admin+ep {}): {stderr}",
                    helper.display()
                )
            } else {
                stderr
            };
            CheckpointResult::err(pid, error)
        }
        Err(e) => {
            CheckpointResult::err(pid,format!("Failed to run mgd-checkpoint helper: {e}"))
        }
    }
}

/// Restore a previously checkpointed process from disk.
#[allow(dead_code)]
pub fn restore(snapshot_dir: &std::path::Path) -> RestoreResult {
    if !snapshot_dir.exists() {
        return RestoreResult::err(format!("Snapshot dir not found: {}", snapshot_dir.display()))
    }

    let snapshot_dir_str = match snapshot_dir.to_str() {
        Some(s) => s.to_string(),
        None => {
            return RestoreResult::err("snapshot path contains non-UTF8 characters")
        }
    };

    let Some(helper) = helper_path() else {
        return RestoreResult::err("mgd-checkpoint helper not found (please install it to enable restore)")
    };

    let output = Command::new(helper)
        .args([
            "restore",
            &snapshot_dir_str,
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            RestoreResult::ok()
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let is_priv_err = looks_like_privilege_error(&stderr) || out.status.code() == Some(2); // 2 is EXIT_SECURITY_FAIL
            let error = if is_priv_err {
                format!(
                    "mgd-checkpoint restore lacks privilege (run: sudo setcap cap_checkpoint_restore,cap_sys_ptrace,cap_net_admin+ep {}): {stderr}",
                    helper.display()
                )
            } else {
                format!("mgd-checkpoint restore failed: {stderr}")
            };
            RestoreResult::err(error)
        }
        Err(e) => {
            RestoreResult::err(format!("Failed to run mgd-checkpoint helper: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privilege_error_detection() {
        assert!(looks_like_privilege_error("Operation not permitted"));
        assert!(looks_like_privilege_error("can't seize task: Operation not permitted"));
        assert!(looks_like_privilege_error("Unable to ptrace: permission denied"));
        assert!(looks_like_privilege_error("requires CAP_SYS_PTRACE"));
        assert!(looks_like_privilege_error("Security Error: target process is not owned by the calling user"));
        assert!(looks_like_privilege_error("Capability Warning: capget failed"));
        // A normal restore failure must NOT be flagged as a privilege issue.
        assert!(!looks_like_privilege_error("Can't restore tcp connection: address in use"));
        assert!(!looks_like_privilege_error("image file not found"));
    }
}
