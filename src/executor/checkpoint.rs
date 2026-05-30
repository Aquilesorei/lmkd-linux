use std::process::Command;
use std::fs;
use std::io;
use std::path::PathBuf;

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

/// Checkpoint a process using CRIU — save state to disk and kill it.
/// Falls back to SIGSTOP if CRIU fails.
pub fn checkpoint(pid: u32, name: &str) -> CheckpointResult {
    // Sanitize comm name: kernel allows '/' in prctl-set names, which would escape
    // the snapshots directory. Replace any non-alphanumeric char with '_'.
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

    let output = Command::new("criu")
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
            // CRIU succeeded — now kill the process (state is saved)
            let kill_ret = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            if kill_ret != 0 {
                let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
                // ESRCH = process already gone, which is fine
                if errno != libc::ESRCH {
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
            // CRIU failed — clean up partial snapshot, caller decides what to do
            let _ = fs::remove_dir_all(&snapshot_dir);
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            CheckpointResult {
                pid,
                success: false,
                snapshot_dir: None,
                error: Some(stderr),
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

    let output = Command::new("criu")
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
            RestoreResult {
                success: false,
                error: Some(format!("CRIU restore failed: {stderr}")),
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
