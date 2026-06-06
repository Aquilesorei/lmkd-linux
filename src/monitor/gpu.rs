//! plasmashell GPU-memory leak watcher.
//!
//! On KDE Plasma + Intel UMA GPUs (Iris Xe), GPU memory is allocated from system
//! RAM, and plasmashell is known to leak it over days of uptime. We read per-process
//! GPU residency from `/proc/<pid>/fdinfo/*` — the kernel's DRM fdinfo accounting,
//! readable by the fd owner with NO elevated privileges (unlike `intel_gpu_top`,
//! which needs CAP_PERFMON for global counters).
//!
//! When usage crosses a threshold, `restart_plasmashell()` cycles it via
//! `kquitapp6` + `kstart`, reclaiming the memory immediately.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

/// Sum of resident GPU memory (KiB) across all of this process's DRM clients.
///
/// Returns `None` if the process has no DRM fds (not a GPU client) or fdinfo
/// can't be read. A process dup's its DRM fd many times but each fd reports the
/// same per-client totals, so we deduplicate by `drm-client-id` to avoid
/// multiply-counting one allocation.
pub fn process_gpu_kb(pid: u32) -> Option<u64> {
    let dir = format!("/proc/{pid}/fdinfo");
    let entries = fs::read_dir(&dir).ok()?;

    // client-id → resident KiB. HashMap dedups dup'd fds of the same DRM client.
    let mut per_client: HashMap<String, u64> = HashMap::new();
    let mut saw_drm = false;

    for entry in entries.filter_map(|e| e.ok()) {
        // fdinfo files are tiny; a read error on one fd (e.g. it closed) is not fatal.
        let Ok(content) = fs::read_to_string(entry.path()) else { continue };

        let mut client_id: Option<&str> = None;
        let mut resident_kb: u64 = 0;
        let mut has_resident = false;

        for line in content.lines() {
            if let Some(v) = line.strip_prefix("drm-client-id:") {
                client_id = Some(v.trim());
            } else if let Some(v) = line.strip_prefix("drm-resident-") {
                // Key form: `drm-resident-<region>:\t<value> <unit>`
                if let Some(kb) = v.split_once(':').and_then(|(_region, val)| parse_mem_kb(val.trim())) {
                    resident_kb = resident_kb.saturating_add(kb);
                    has_resident = true;
                }
            }
        }

        if !has_resident {
            continue;
        }
        saw_drm = true;
        match client_id {
            // Same client seen via another fd → identical totals, keep one copy.
            Some(id) => { per_client.insert(id.to_string(), resident_kb); }
            // No client-id (older kernels): fall back to per-fd accumulation under
            // a synthetic key so it still contributes without colliding.
            None => { per_client.insert(format!("fd:{:?}", entry.file_name()), resident_kb); }
        }
    }

    if !saw_drm {
        return None;
    }
    Some(per_client.values().copied().fold(0u64, u64::saturating_add))
}

/// Parse a DRM fdinfo memory value like `2247000 KiB` / `4 MiB` / `512` into KiB.
/// A bare number with no unit is treated as bytes (per the DRM fdinfo spec default).
fn parse_mem_kb(s: &str) -> Option<u64> {
    let mut parts = s.split_whitespace();
    let num: u64 = parts.next()?.parse().ok()?;
    match parts.next() {
        Some("KiB") => Some(num),
        Some("MiB") => Some(num.saturating_mul(1024)),
        Some("GiB") => Some(num.saturating_mul(1024 * 1024)),
        Some("B") | None => Some(num / 1024),
        _ => None,
    }
}

/// Restart plasmashell: graceful `kquitapp6 plasmashell`, then `kstart plasmashell`.
/// Waits 2s to let it come back up. Returns `Err` (caller logs + skips, does NOT
/// arm the cooldown) if either binary is missing or the quit step fails.
pub fn restart_plasmashell() -> Result<(), String> {
    if which("kquitapp6").is_none() {
        return Err("kquitapp6 not found in PATH".into());
    }
    if which("kstart").is_none() {
        return Err("kstart not found in PATH".into());
    }

    match Command::new("kquitapp6").arg("plasmashell").status() {
        Ok(status) if status.success() => {}
        Ok(status) => return Err(format!("kquitapp6 exited with {status}")),
        Err(e) => return Err(format!("kquitapp6 failed to spawn: {e}")),
    }

    match Command::new("kstart").arg("plasmashell").status() {
        Ok(status) if status.success() => {}
        Ok(status) => return Err(format!("kstart exited with {status}")),
        Err(e) => return Err(format!("kstart failed to spawn: {e}")),
    }

    // Give the new plasmashell time to start and re-allocate its baseline GPU mem.
    thread::sleep(Duration::from_secs(2));
    Ok(())
}

/// Minimal PATH lookup — avoids shelling out to `which`. Returns the first
/// matching executable path, or None if not found.
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

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match fs::metadata(p) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mem_kb_units() {
        assert_eq!(parse_mem_kb("2247000 KiB"), Some(2_247_000));
        assert_eq!(parse_mem_kb("4 MiB"), Some(4096));
        assert_eq!(parse_mem_kb("1 GiB"), Some(1024 * 1024));
        assert_eq!(parse_mem_kb("2048 B"), Some(2));
        // Bare number = bytes per DRM spec.
        assert_eq!(parse_mem_kb("4096"), Some(4));
        assert_eq!(parse_mem_kb("garbage"), None);
    }
}
