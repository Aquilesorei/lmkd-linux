//! Per-process GPU memory accounting via DRM fdinfo, and the plasmashell
//! leak-restart watcher.
//!
//! On Intel UMA, GPU memory comes from system RAM. DRM fdinfo
//! (`/proc/<pid>/fdinfo/*`) is readable by the fd owner without privilege,
//! unlike `intel_gpu_top` (CAP_PERFMON).

use std::thread;
use std::time::Duration;

/// `kquitapp6 plasmashell` then `kstart plasmashell`, with a 2s settle. Err if
/// either binary is missing or quit fails — caller skips without arming cooldown.
pub fn restart_plasmashell() -> Result<(), String> {
    if which("kquitapp6").is_none() {
        return Err("kquitapp6 not found in PATH".into());
    }
    if which("kstart").is_none() {
        return Err("kstart not found in PATH".into());
    }

    match std::process::Command::new("kquitapp6").arg("plasmashell").status() {
        Ok(status) if status.success() => {}
        Ok(status) => return Err(format!("kquitapp6 exited with {status}")),
        Err(e) => return Err(format!("kquitapp6 failed to spawn: {e}")),
    }

    match std::process::Command::new("kstart").arg("plasmashell").status() {
        Ok(status) if status.success() => {}
        Ok(status) => return Err(format!("kstart exited with {status}")),
        Err(e) => return Err(format!("kstart failed to spawn: {e}")),
    }

    thread::sleep(Duration::from_secs(2));
    Ok(())
}

/// First executable named `bin` on PATH, or None.
fn which(bin: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if is_executable(&candidate) {
            return candidate.to_str().map(String::from);
        }
    }
    None
}

fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_parse_mem_kb_units() {
        assert_eq!(mgd_common::gpu::parse_mem_kb("2247000 KiB"), Some(2_247_000));
        assert_eq!(mgd_common::gpu::parse_mem_kb("4 MiB"), Some(4096));
        assert_eq!(mgd_common::gpu::parse_mem_kb("1 GiB"), Some(1024 * 1024));
        assert_eq!(mgd_common::gpu::parse_mem_kb("2048 B"), Some(2));
        assert_eq!(mgd_common::gpu::parse_mem_kb("4096"), Some(4)); // bare = bytes
        assert_eq!(mgd_common::gpu::parse_mem_kb("garbage"), None);
    }

}
