//! Per-process GPU memory accounting via DRM fdinfo, and the plasmashell
//! leak-restart watcher.
//!
//! On Intel UMA, GPU memory comes from system RAM. DRM fdinfo
//! (`/proc/<pid>/fdinfo/*`) is readable by the fd owner without privilege,
//! unlike `intel_gpu_top` (CAP_PERFMON).

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const GPU_CACHE_TTL: Duration = Duration::from_secs(30);

/// pid -> (resident KiB, sampled-at). Keyed on pid alone, no recycle guard: the
/// value feeds sort ranking only, never the deficit, so a recycled pid mis-ranks
/// one candidate for at most one TTL window.
static GPU_CACHE: LazyLock<Mutex<HashMap<u32, (u64, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// [`process_gpu_kb`] memoized per pid for `GPU_CACHE_TTL`. 0 if not a GPU
/// client. The fdinfo walk costs ~tens of ms; the cache keeps it off the hot
/// path during a sustained pressure episode.
pub fn process_gpu_kb_cached(pid: u32) -> u64 {
    let now = Instant::now();
    if let Ok(cache) = GPU_CACHE.lock()
        && let Some(&(kb, at)) = cache.get(&pid)
        && now.duration_since(at) < GPU_CACHE_TTL
    {
        return kb;
    }

    let kb = process_gpu_kb(pid).unwrap_or(0);

    if let Ok(mut cache) = GPU_CACHE.lock() {
        cache.retain(|_, (_, at)| now.duration_since(*at) < GPU_CACHE_TTL);
        cache.insert(pid, (kb, now));
    }
    kb
}

/// Resident GPU memory (KiB) summed across the process's DRM clients, or None if
/// it has none. A DRM fd is dup'd many times but each dup reports identical
/// per-client totals, so dedup by `drm-client-id`.
///
/// Only fds symlinked into `/dev/dri/` have their fdinfo read; the readlink gate
/// keeps non-DRM fds (the bulk) to one cheap syscall.
pub fn process_gpu_kb(pid: u32) -> Option<u64> {
    let fd_dir = format!("/proc/{pid}/fd");
    let entries = fs::read_dir(&fd_dir).ok()?;

    let mut per_client: HashMap<String, u64> = HashMap::new();
    let mut saw_drm = false;

    for entry in entries.filter_map(|e| e.ok()) {
        match fs::read_link(entry.path()) {
            Ok(target) if target.to_string_lossy().starts_with("/dev/dri/") => {}
            _ => continue,
        }

        // fdinfo path mirrors the fd number. A read race (fd closed) is not fatal.
        let fdinfo = format!("/proc/{pid}/fdinfo/{}", entry.file_name().to_string_lossy());
        let Ok(content) = fs::read_to_string(&fdinfo) else { continue };

        let mut client_id: Option<&str> = None;
        let mut resident_kb: u64 = 0;
        let mut has_resident = false;

        for line in content.lines() {
            if let Some(v) = line.strip_prefix("drm-client-id:") {
                client_id = Some(v.trim());
            } else if let Some(v) = line.strip_prefix("drm-resident-") {
                // `drm-resident-<region>:\t<value> <unit>`
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
            Some(id) => { per_client.insert(id.to_string(), resident_kb); }
            // Pre-client-id kernels: synthetic per-fd key avoids collision.
            None => { per_client.insert(format!("fd:{:?}", entry.file_name()), resident_kb); }
        }
    }

    if !saw_drm {
        return None;
    }
    Some(per_client.values().copied().fold(0u64, u64::saturating_add))
}

/// Parse a DRM fdinfo size (`2247000 KiB` / `4 MiB` / `512`) into KiB. A bare
/// number is bytes, per the fdinfo spec.
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

/// `kquitapp6 plasmashell` then `kstart plasmashell`, with a 2s settle. Err if
/// either binary is missing or quit fails — caller skips without arming cooldown.
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
        assert_eq!(parse_mem_kb("4096"), Some(4)); // bare = bytes
        assert_eq!(parse_mem_kb("garbage"), None);
    }

    #[test]
    fn cached_is_consistent_and_hits() {
        // Test runner pid is not a DRM client: exercises the 0 fast path + hit.
        let pid = std::process::id();
        let first = process_gpu_kb_cached(pid);
        let second = process_gpu_kb_cached(pid);
        assert_eq!(first, second);
    }
}
