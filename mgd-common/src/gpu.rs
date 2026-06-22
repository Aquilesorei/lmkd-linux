use std::collections::HashMap;
use std::fs;

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

    use std::os::unix::ffi::OsStrExt;
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
