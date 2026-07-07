use std::collections::HashMap;
use std::fs;

/// Per-process GPU memory stats aggregated across all DRM clients.
/// Region suffix (`system0`, `smem`, `lmem`) is ignored — works for i915 and xe.
#[derive(Debug, Clone, Default)]
pub struct SingleProcessGpuMemory {
    /// Pages currently resident in system RAM (includes shared).
    pub resident_kb: u64,
    /// Imported dma-buf pages shared with other clients (compositor etc) — also
    /// counted in their resident_kb. Pressure term = resident_kb - shared_kb.
    pub shared_kb: u64,
    /// All GEM BOs the client has handles to: resident + non-resident + shared overhead.
    /// total - resident = non-resident BOs + shared overhead (not a reservation).
    pub total_kb: u64,
    /// Purgeable pages — shrinker can drop for free (no migration needed).
    pub purgeable_kb: u64,
}

/// GPU memory stats for a process, or None if it has no DRM fds.
/// DRM fds are deduped by `drm-client-id`; each dup reports identical totals.
/// Only fds symlinked into `/dev/dri/` are read; readlink gate keeps cost low.
pub fn get_process_gpu_stats(pid: u32) -> Option<SingleProcessGpuMemory> {
    use std::os::unix::ffi::OsStrExt;
    let fd_dir = format!("/proc/{pid}/fd"); //please excuse me for allocating  omg    crying
    let entries = fs::read_dir(&fd_dir).ok()?;

    // client_id → (resident_kb, shared_kb, total_kb, purgeable_kb)
    let mut per_client: HashMap<String, (u64, u64, u64, u64)> = HashMap::new();
    let mut saw_drm = false;

    let mut path_buf = Vec::with_capacity(64);
    path_buf.extend_from_slice(fd_dir.as_bytes());
    path_buf.push(b'/');
    let base_len = path_buf.len();

    for entry in entries.filter_map(|e| e.ok()) {
        let file_name = entry.file_name();

        path_buf.truncate(base_len);
        path_buf.extend_from_slice(file_name.as_bytes());
        path_buf.push(0);

        let mut link_buf = [0u8; 9];
        let n = unsafe {
            libc::readlink(
                path_buf.as_ptr() as *const libc::c_char,
                link_buf.as_mut_ptr() as *mut libc::c_char,
                link_buf.len(),
            )
        };

        if n != 9 || &link_buf != b"/dev/dri/" {
            continue;
        }

        let fdinfo = format!("/proc/{pid}/fdinfo/{}", entry.file_name().to_string_lossy());
        let Ok(content) = fs::read_to_string(&fdinfo) else { continue };

        let mut client_id: Option<String> = None;
        let mut resident_kb: u64 = 0;
        let mut shared_kb: u64 = 0;
        let mut total_kb: u64 = 0;
        let mut purgeable_kb: u64 = 0;
        let mut has_drm = false;

        for line in content.lines() {
            if let Some(v) = line.strip_prefix("drm-client-id:") {
                client_id = Some(v.trim().to_string());
            } else if let Some(kb) = line.strip_prefix("drm-resident-").and_then(drm_region_kb) {
                resident_kb = resident_kb.saturating_add(kb);
                has_drm = true;
            } else if let Some(kb) = line.strip_prefix("drm-shared-").and_then(drm_region_kb) {
                shared_kb = shared_kb.saturating_add(kb);
            } else if let Some(kb) = line.strip_prefix("drm-total-").and_then(drm_region_kb) {
                total_kb = total_kb.saturating_add(kb);
            } else if let Some(kb) = line.strip_prefix("drm-purgeable-").and_then(drm_region_kb) {
                purgeable_kb = purgeable_kb.saturating_add(kb);
            }
        }

        if !has_drm {
            continue;
        }
        saw_drm = true;
        let key = client_id.unwrap_or_else(|| format!("fd:{:?}", entry.file_name()));
        per_client.insert(key, (resident_kb, shared_kb, total_kb, purgeable_kb));
    }

    if !saw_drm {
        return None;
    }

    let mut stats = SingleProcessGpuMemory::default();
    for (r, s, t, p) in per_client.into_values() {
        stats.resident_kb  = stats.resident_kb.saturating_add(r);
        stats.shared_kb    = stats.shared_kb.saturating_add(s);
        stats.total_kb     = stats.total_kb.saturating_add(t);
        stats.purgeable_kb = stats.purgeable_kb.saturating_add(p);
    }
    Some(stats)
}


/// Write resident/total/purgeable observations for `pid` to `writer`.
/// Shared by mgd-gpu-intel and mgd-gpu-amd — same wire format, same metrics.
pub fn send_gpu_stats(writer: &mut impl std::io::Write, plugin: &str, pid: crate::types::Pid, stats: &SingleProcessGpuMemory) {
    use crate::protocol::{Metric, PluginMessage};
    for (metric, kb) in [
        (Metric::GpuResidentKb,  stats.resident_kb),
        (Metric::GpuSharedKb,    stats.shared_kb),
        (Metric::GpuTotalKb,     stats.total_kb),
        (Metric::GpuPurgeableKb, stats.purgeable_kb),
    ] {
        let obs = PluginMessage::Observation {
            plugin: plugin.to_string(),
            metric,
            pid: Some(pid),
            value: kb as f64,
        };
        let _ = writeln!(writer, "{}", serde_json::to_string(&obs).unwrap());
    }
}

/// Parse `<region>:<value> <unit>` — the part after stripping a `drm-<stat>-` prefix.
fn drm_region_kb(v: &str) -> Option<u64> {
    v.split_once(':').and_then(|(_, val)| parse_mem_kb(val.trim()))
}

/// Parse a DRM fdinfo size (`2247000 KiB` / `4 MiB` / `512`) into KiB. A bare
/// number is bytes, per the fdinfo spec.
pub fn parse_mem_kb(s: &str) -> Option<u64> {
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

#[cfg(test)]
mod tests {
    use super::parse_mem_kb;

    #[test]
    fn test_parse_mem_kb_units() {
        assert_eq!(parse_mem_kb("2247000 KiB"), Some(2_247_000));
        assert_eq!(parse_mem_kb("4 MiB"), Some(4096));
        assert_eq!(parse_mem_kb("1 GiB"), Some(1024 * 1024));
        assert_eq!(parse_mem_kb("2048 B"), Some(2));
        assert_eq!(parse_mem_kb("4096"), Some(4)); // bare = bytes
        assert_eq!(parse_mem_kb("garbage"), None);
    }
}
