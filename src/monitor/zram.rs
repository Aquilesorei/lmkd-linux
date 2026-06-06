//! zram introspection + compaction.
//!
//! zram keeps swapped-out pages compressed in RAM, but its allocator
//! fragments over time: freed slots are not automatically coalesced, so the
//! pool can hold more RAM than its live compressed data needs. Writing `1` to
//! `/sys/block/<dev>/compact` triggers the kernel to repack live objects and
//! release whole empty pages back to the system — a cheap (~100ms) win with no
//! process touched.
//!
//! The compact sysfs node is `0200 root:root` by default, so the unprivileged
//! daemon cannot write it. The opt-in `packaging/mgd-zram.conf` tmpfiles grant
//! makes it group-writable by the `mgd` group (see docs/PRIVILEGE_DESIGN.md §1).
//! Without that grant the write fails with `EACCES` and the caller degrades
//! gracefully.

use std::fs;
use std::io;
use std::path::PathBuf;

/// zram swap devices currently active, by basename (e.g. `zram0`).
/// Parsed from /proc/swaps — only devices actually used as swap qualify, which
/// is what mgd cares about (zram used as a plain block device is out of scope).
pub fn zram_devices() -> Vec<String> {
    let content = fs::read_to_string("/proc/swaps").unwrap_or_default();
    parse_zram_devices(&content)
}

/// RAM the zram pool occupies right now, in MB (`mem_used_total` from
/// /sys/block/<dev>/mm_stat). This is the *compressed* footprint actually held
/// in RAM — the right figure for the min-used gate. Returns None if the node is
/// unreadable.
pub fn zram_used_mb(device: &str) -> Option<u64> {
    let path = format!("/sys/block/{device}/mm_stat");
    let content = fs::read_to_string(path).ok()?;
    parse_mem_used_bytes(&content).map(|b| b / (1024 * 1024))
}

/// The *decompressed* footprint of a zram pool in MB (`orig_data_size`, field 0
/// of mm_stat). This is the RAM that pages will occupy once swapped back in —
/// 2-3× the compressed `mem_used_total`. The proactive-reclaim headroom gate
/// must use THIS figure (not the compressed one) to avoid OOMing the system at
/// the moment all pages land back in RAM. Returns None if the node is unreadable.
pub fn zram_orig_mb(device: &str) -> Option<u64> {
    let path = format!("/sys/block/{device}/mm_stat");
    let content = fs::read_to_string(path).ok()?;
    parse_orig_data_bytes(&content).map(|b| b / (1024 * 1024))
}

/// Total decompressed footprint across all active zram swap devices, in MB.
/// The figure the reclaim headroom gate compares against MemAvailable.
pub fn zram_orig_mb_total() -> u64 {
    zram_devices().iter().filter_map(|d| zram_orig_mb(d)).sum()
}

/// Total compressed RAM held across all active zram swap devices, in MB.
/// Used for the min-used gate (skip reclaim when little is stored).
pub fn zram_used_mb_total() -> u64 {
    zram_devices().iter().filter_map(|d| zram_used_mb(d)).sum()
}

/// Trigger compaction on one device: write `1` to /sys/block/<dev>/compact.
/// `EACCES` here means the tmpfiles grant is absent (node still root-only).
pub fn compact(device: &str) -> io::Result<()> {
    let path: PathBuf = format!("/sys/block/{device}/compact").into();
    fs::write(path, b"1")
}

// ── pure parse helpers (unit-tested) ─────────────────────────────────────────

/// Extract zram swap-device basenames (e.g. `zram0`) from /proc/swaps content.
/// Validation anchors on the canonical `/dev/zram<N>` path, not a basename
/// `starts_with("zram")`, so a swapfile named like `zram-cache` is not mistaken
/// for a zram block device. The header line and disk/file swaps are skipped.
fn parse_zram_devices(swaps: &str) -> Vec<String> {
    swaps
        .lines()
        .skip(1) // header: "Filename  Type  Size  Used  Priority"
        .filter_map(|line| line.split_whitespace().next())
        .filter(|path| is_zram_device_path(path))
        .filter_map(|dev| dev.rsplit('/').next())
        .map(|s| s.to_string())
        .collect()
}

/// True only for a canonical zram block-device path: `/dev/zram` followed by at
/// least one ASCII digit (e.g. `/dev/zram0`). Rejects swapfiles, partitions,
/// and lookalike names such as `/swap/zram-cache`.
fn is_zram_device_path(path: &str) -> bool {
    match path.strip_prefix("/dev/zram") {
        Some(rest) => !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// Parse `mem_used_total` (bytes) — the 3rd whitespace field of mm_stat:
///   orig_data_size compr_data_size mem_used_total ...
fn parse_mem_used_bytes(mm_stat: &str) -> Option<u64> {
    mm_stat.split_whitespace().nth(2)?.parse().ok()
}

/// Parse `orig_data_size` (bytes) — the 1st whitespace field of mm_stat. This
/// is the uncompressed size of the stored data; the figure the reclaim headroom
/// gate must use (see zram_orig_mb).
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
    fn test_is_zram_device_path() {
        assert!(is_zram_device_path("/dev/zram0"));
        assert!(is_zram_device_path("/dev/zram10"));
        assert!(!is_zram_device_path("/dev/zram"));
        assert!(!is_zram_device_path("/swap/zram-cache"));
        assert!(!is_zram_device_path("/dev/nvme0n1p3"));
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
