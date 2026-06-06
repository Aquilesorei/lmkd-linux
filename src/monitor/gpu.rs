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
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// TTL for the per-PID GPU residency cache. GPU footprint moves slowly relative
/// to the 5s evictor cycle, so a value stale by ≤30s is fine for SORT RANKING
/// (its only consumer). During a sustained High+ pressure episode this means the
/// fdinfo walk is paid once on entry, not every cycle.
const GPU_CACHE_TTL: Duration = Duration::from_secs(30);

/// pid → (resident KiB, sampled-at). Module-global; advisory ranking data only.
/// Keyed on pid alone (no start_time recycle guard): a recycled PID at worst
/// mis-ranks one candidate for up to one TTL window, and this value is NEVER
/// credited toward the deficit — so the blast radius is a single sort position.
static GPU_CACHE: LazyLock<Mutex<HashMap<u32, (u64, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Cached form of [`process_gpu_kb`], collapsing the per-fd fdinfo walk to once
/// per [`GPU_CACHE_TTL`] per PID. Returns resident GPU KiB (0 if not a GPU client
/// or unreadable). This is the entry point the evictor's `plan()` uses, so the
/// ~60ms full sweep is paid only on the first High+ cycle of a pressure episode.
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
        // Prune expired entries so the map can't grow without bound as GPU PIDs
        // churn over the daemon's lifetime.
        cache.retain(|_, (_, at)| now.duration_since(*at) < GPU_CACHE_TTL);
        cache.insert(pid, (kb, now));
    }
    kb
}

/// Sum of resident GPU memory (KiB) across all of this process's DRM clients.
///
/// Returns `None` if the process has no DRM fds (not a GPU client) or fdinfo
/// can't be read. A process dup's its DRM fd many times but each fd reports the
/// same per-client totals, so we deduplicate by `drm-client-id` to avoid
/// multiply-counting one allocation.
///
/// Walk is gated on `/proc/<pid>/fd` symlinks: only fds pointing into `/dev/dri/`
/// have their fdinfo read. `readlink` is one cheap syscall, so non-GPU processes
/// (and a GPU process's many non-DRM fds) cost a readdir + readlinks and no file
/// reads at all — the bulk of the sweep cost lands only on real DRM clients.
pub fn process_gpu_kb(pid: u32) -> Option<u64> {
    let fd_dir = format!("/proc/{pid}/fd");
    let entries = fs::read_dir(&fd_dir).ok()?;

    // client-id → resident KiB. HashMap dedups dup'd fds of the same DRM client.
    let mut per_client: HashMap<String, u64> = HashMap::new();
    let mut saw_drm = false;

    for entry in entries.filter_map(|e| e.ok()) {
        // Cheap gate: skip any fd that isn't a DRM device node before touching fdinfo.
        match fs::read_link(entry.path()) {
            Ok(target) if target.to_string_lossy().starts_with("/dev/dri/") => {}
            _ => continue,
        }

        // fdinfo mirrors the fd number: /proc/<pid>/fdinfo/<fd>. Tiny file; a read
        // error on one fd (e.g. it closed between readdir and read) is not fatal.
        let fdinfo = format!("/proc/{pid}/fdinfo/{}", entry.file_name().to_string_lossy());
        let Ok(content) = fs::read_to_string(&fdinfo) else { continue };

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

    #[test]
    fn cached_is_consistent_and_hits() {
        // Own PID (test runner) is not a DRM client, so this exercises the
        // non-GPU fast path: first call computes (0), second is a cache hit.
        // Asserts the cache returns a stable value and never panics.
        let pid = std::process::id();
        let first = process_gpu_kb_cached(pid);
        let second = process_gpu_kb_cached(pid);
        assert_eq!(first, second);
    }
}
