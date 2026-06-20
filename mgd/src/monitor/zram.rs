//! zram introspection and compaction.
//!
//! Compacting (`echo 1 > /sys/block/<dev>/compact`) repacks the pool and frees
//! pages stranded by allocator fragmentation. The node is `0200 root:root`; the
//! opt-in tmpfiles grant (`packaging/mgd-zram.conf`) makes it group-writable.
//! Without it, `compact` returns EACCES and the caller degrades.

use std::fs;
use std::io;
use std::path::PathBuf;

use mgd_common::zram::is_zram_device_path;

/// Active zram swap devices by basename (e.g. `zram0`), from /proc/swaps.
pub fn zram_devices() -> Vec<String> {
    let content = fs::read_to_string("/proc/swaps").unwrap_or_default();
    parse_zram_devices(&content)
}

/// Compressed RAM the pool holds, MB (`mem_used_total`). For the min-used gate.
pub fn zram_used_mb(device: &str) -> Option<u64> {
    let path = format!("/sys/block/{device}/mm_stat");
    let content = fs::read_to_string(path).ok()?;
    parse_mem_used_bytes(&content).map(|b| b / (1024 * 1024))
}

/// Decompressed footprint, MB (`orig_data_size`) — 2-3x the compressed figure.
/// The reclaim headroom gate uses this: it's the RAM pages reclaim into.
pub fn zram_orig_mb(device: &str) -> Option<u64> {
    let path = format!("/sys/block/{device}/mm_stat");
    let content = fs::read_to_string(path).ok()?;
    parse_orig_data_bytes(&content).map(|b| b / (1024 * 1024))
}

/// Total decompressed footprint across all zram swap devices, MB.
pub fn zram_orig_mb_total() -> u64 {
    zram_devices().iter().filter_map(|d| zram_orig_mb(d)).sum()
}

/// Total compressed RAM across all zram swap devices, MB.
pub fn zram_used_mb_total() -> u64 {
    zram_devices().iter().filter_map(|d| zram_used_mb(d)).sum()
}

/// Compact one device. EACCES means the tmpfiles grant is absent.
pub fn compact(device: &str) -> io::Result<()> {
    let path: PathBuf = format!("/sys/block/{device}/compact").into();
    fs::write(path, b"1")
}

// ── parse helpers ─────────────────────────────────────────────────────────────

/// zram swap basenames from /proc/swaps. Anchored on `/dev/zram<N>` so a
/// swapfile named `zram-cache` isn't mistaken for a device.
fn parse_zram_devices(swaps: &str) -> Vec<String> {
    swaps
        .lines()
        .skip(1) // header
        .filter_map(|line| line.split_whitespace().next())
        .filter(|path| is_zram_device_path(path))
        .filter_map(|dev| dev.rsplit('/').next())
        .map(|s| s.to_string())
        .collect()
}

/// `mem_used_total` — field 2 (0-indexed) of mm_stat:
/// `orig_data_size compr_data_size mem_used_total ...`
fn parse_mem_used_bytes(mm_stat: &str) -> Option<u64> {
    mm_stat.split_whitespace().nth(2)?.parse().ok()
}

/// `orig_data_size` — field 0 of mm_stat.
fn parse_orig_data_bytes(mm_stat: &str) -> Option<u64> {
    mm_stat.split_whitespace().next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_zram_devices_basic() {
        let swaps = "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n\
                     /dev/zram0                              partition\t8388604\t\t1024\t\t100\n";
        assert_eq!(parse_zram_devices(swaps), vec!["zram0"]);
    }

    #[test]
    fn test_parse_zram_devices_skips_disk_swap() {
        let swaps = "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n\
                     /dev/nvme0n1p3                          partition\t16777212\t0\t\t-2\n\
                     /dev/zram0                              partition\t8388604\t1024\t100\n";
        assert_eq!(parse_zram_devices(swaps), vec!["zram0"]);
    }

    #[test]
    fn test_parse_zram_devices_multiple() {
        let swaps = "Filename Type Size Used Priority\n\
                     /dev/zram0 partition 100 0 100\n\
                     /dev/zram1 partition 100 0 100\n";
        assert_eq!(parse_zram_devices(swaps), vec!["zram0", "zram1"]);
    }

    #[test]
    fn test_parse_zram_devices_empty() {
        // Header only, no swap devices.
        let swaps = "Filename Type Size Used Priority\n";
        assert!(parse_zram_devices(swaps).is_empty());
    }

    #[test]
    fn test_parse_zram_devices_skips_lookalike_swapfile() {
        // A swapfile named like a zram device must not be picked up — anchored
        // /dev/zram<N> validation rejects it.
        let swaps = "Filename Type Size Used Priority\n\
                     /swap/zram-cache file 2097152 0 -3\n";
        assert!(parse_zram_devices(swaps).is_empty());
    }

    #[test]
    fn test_parse_mem_used_bytes() {
        // orig=4194304 compr=1048576 mem_used_total=2097152 ...
        let mm_stat = "4194304 1048576 2097152 0 8388608 0 0 0 0\n";
        assert_eq!(parse_mem_used_bytes(mm_stat), Some(2097152));
    }

    #[test]
    fn test_parse_mem_used_bytes_truncated() {
        assert_eq!(parse_mem_used_bytes("4194304 1048576"), None);
    }

    #[test]
    fn test_parse_orig_data_bytes() {
        // orig_data_size is field 0.
        let mm_stat = "4194304 1048576 2097152 0 8388608 0 0 0 0\n";
        assert_eq!(parse_orig_data_bytes(mm_stat), Some(4194304));
    }

    #[test]
    fn test_parse_orig_data_bytes_empty() {
        assert_eq!(parse_orig_data_bytes(""), None);
    }
}
