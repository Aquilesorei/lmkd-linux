//! mgd-zram-reclaim — privileged swap-reclaim helper (Phase 3 / PRIVILEGE_DESIGN §2).
//!
//! Cycles every active **zram** swap device through `swapoff(2)` + `swapon(2)`,
//! which forces the kernel to pull all compressed pages back into RAM and then
//! re-enables the (now empty) device. This is how post-pressure RAM is returned
//! from zram. `swapoff`/`swapon` are genuine `CAP_SYS_ADMIN` syscalls — there is
//! no narrower capability — so this lives in a separate, minimal binary carrying
//! only that one capability (`setcap cap_sys_admin+ep`), never SUID-root.
//!
//! The binary is `0750 root:mgd`, so ANY mgd-group process can invoke it — not
//! just the daemon. It therefore cannot rely on the daemon's gating: the one
//! invariant that prevents catastrophe (don't pull so much back into RAM that
//! the system OOMs) is enforced HERE, before any swapoff, using only
//! world-readable /proc/meminfo + /sys mm_stat (no extra privilege). The daemon
//! keeps its own stricter, configurable headroom policy on top of this floor.
//!
//! Exit codes (consumed by the daemon to decide whether to disable the feature):
//!   0  all devices reclaimed, or no zram swap present (nothing to do)
//!   1  a swapoff/swapon failed for a non-privilege reason (transient)
//!   2  swapoff failed with EPERM — binary is not capped (persistent)
//!   3  refused: insufficient RAM headroom to safely reclaim (safety floor)
//!   4  could not read /proc/meminfo (cannot prove it is safe → refuse)

use std::ffi::CString;
use std::fs;

// Linux swap(2) flags — not exported by libc 0.2, defined here per <sys/swap.h>.
const SWAP_FLAG_PREFER: i32 = 0x8000;
const SWAP_FLAG_PRIO_MASK: i32 = 0x7fff;

// Exit codes — see module docs.
const EXIT_OK: i32 = 0;
const EXIT_TRANSIENT: i32 = 1;
const EXIT_EPERM: i32 = 2;
const EXIT_REFUSED_UNSAFE: i32 = 3;
const EXIT_NO_MEMINFO: i32 = 4;

/// One zram swap device and the priority it had in /proc/swaps, so swapon can
/// restore the exact same priority instead of letting the kernel auto-assign.
struct ZramSwap {
    /// Absolute device path, e.g. "/dev/zram0".
    path: String,
    /// Swap priority from /proc/swaps (may be negative if kernel-assigned).
    priority: i32,
}

fn main() {
    // Defense in depth: never read or act on the environment. `+ep` binaries
    // already run in glibc secure-execution mode (LD_* neutralized); we read no
    // env var and make no subprocess regardless.

    let devices = match parse_zram_swaps(&read_proc_swaps()) {
        v if v.is_empty() => {
            // Nothing mounted as zram swap — not an error, just nothing to do.
            eprintln!("mgd-zram-reclaim: no zram swap devices found, nothing to do");
            std::process::exit(EXIT_OK);
        }
        v => v,
    };

    // ── self-guard (the catastrophe gate) ────────────────────────────────────
    // Refuse if the decompressed footprint would not safely fit in RAM. This is
    // the same OOM invariant the daemon checks, re-enforced here so a DIRECT
    // group invocation (bypassing the daemon) still cannot self-OOM the box.
    // Uses only world-readable nodes — no extra privilege needed to be safe.
    match headroom_safe(&devices) {
        HeadroomCheck::Safe => {}
        HeadroomCheck::Unsafe { avail_mb, decompressed_mb } => {
            eprintln!(
                "mgd-zram-reclaim: refusing — MemAvailable {avail_mb}MB <= decompressed \
                 footprint {decompressed_mb}MB; reclaim would risk OOM"
            );
            std::process::exit(EXIT_REFUSED_UNSAFE);
        }
        HeadroomCheck::Unknown => {
            eprintln!("mgd-zram-reclaim: refusing — cannot read /proc/meminfo to prove it is safe");
            std::process::exit(EXIT_NO_MEMINFO);
        }
    }

    let mut transient = 0u32;
    let mut eperm = 0u32;
    for dev in &devices {
        match reclaim_one(dev) {
            Ok(()) => eprintln!("mgd-zram-reclaim: {} reclaimed (swapoff+swapon)", dev.path),
            Err(ReclaimErr::Eperm(m)) => {
                eprintln!("mgd-zram-reclaim: {}: {m}", dev.path);
                eperm += 1;
            }
            Err(ReclaimErr::Other(m)) => {
                eprintln!("mgd-zram-reclaim: {}: {m}", dev.path);
                transient += 1;
            }
        }
    }

    // EPERM is reported distinctly so the daemon can tell "uncapped binary"
    // (persistent — disable) from a transient kernel error (retry next cycle).
    let code = if eperm > 0 {
        EXIT_EPERM
    } else if transient > 0 {
        EXIT_TRANSIENT
    } else {
        EXIT_OK
    };
    std::process::exit(code);
}

enum HeadroomCheck {
    Safe,
    Unsafe { avail_mb: u64, decompressed_mb: u64 },
    Unknown,
}

/// The hard safety floor: the total decompressed footprint of the pools we are
/// about to reclaim must fit within currently-available RAM. We require
/// MemAvailable strictly greater than the decompressed total — a bare "it fits"
/// bound (ratio 1.0), deliberately NOT stricter than the daemon's default
/// configurable margin (1.5×), so this floor never rejects a daemon-approved
/// reclaim; it only catches direct/misconfigured invocations that would OOM.
fn headroom_safe(devices: &[ZramSwap]) -> HeadroomCheck {
    let Some(avail_mb) = read_mem_available_mb() else { return HeadroomCheck::Unknown };
    let decompressed_mb: u64 = devices
        .iter()
        .filter_map(|d| device_basename(&d.path))
        .filter_map(zram_orig_mb)
        .sum();
    if is_headroom_safe(avail_mb, decompressed_mb) {
        HeadroomCheck::Safe
    } else {
        HeadroomCheck::Unsafe { avail_mb, decompressed_mb }
    }
}

/// Pure safety predicate: available RAM must STRICTLY exceed the decompressed
/// footprint about to be pulled back in. Bare "it fits" bound (no margin) so
/// this floor never rejects a reclaim the daemon's stricter 1.5× policy allowed;
/// it exists only to stop a direct/misconfigured invocation from self-OOMing.
fn is_headroom_safe(avail_mb: u64, decompressed_mb: u64) -> bool {
    avail_mb > decompressed_mb
}

/// Failure kind from a single device reclaim, so the caller can distinguish a
/// persistent privilege problem (uncapped binary) from a transient kernel error.
enum ReclaimErr {
    /// swapoff returned EPERM — the binary lacks CAP_SYS_ADMIN (not capped).
    Eperm(String),
    /// Any other failure (transient kernel error, swapon failure, etc.).
    Other(String),
}

/// Reclaim a single device: swapoff (pull pages back to RAM) then swapon
/// (re-enable empty), with SIGINT/TERM/HUP/QUIT blocked across the pair so an
/// interrupt can never leave this device disabled.
fn reclaim_one(dev: &ZramSwap) -> Result<(), ReclaimErr> {
    let path = CString::new(dev.path.as_bytes())
        .map_err(|_| ReclaimErr::Other("device path contains NUL".to_string()))?;

    // ── critical section: block terminating signals ──────────────────────────
    let saved = block_signals();

    let off = unsafe { libc::swapoff(path.as_ptr()) };
    if off != 0 {
        let errno = std::io::Error::last_os_error();
        let is_eperm = errno.raw_os_error() == Some(libc::EPERM);
        // Device is still on (swapoff failed) — system not stranded. Restore the
        // signal mask and report.
        restore_signals(&saved);
        let msg = format!("swapoff failed: {errno}");
        return Err(if is_eperm { ReclaimErr::Eperm(msg) } else { ReclaimErr::Other(msg) });
    }

    // swapoff succeeded → device is OFF. We MUST re-enable it before returning.
    let flags = swapon_flags(dev.priority);
    let mut on = unsafe { libc::swapon(path.as_ptr(), flags) };
    if on != 0 {
        // Critical: retry a few times before giving up so a transient failure
        // doesn't strand the system without this swap device.
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            on = unsafe { libc::swapon(path.as_ptr(), flags) };
            if on == 0 {
                break;
            }
        }
    }
    let on_err = if on != 0 { Some(last_errno_string()) } else { None };

    restore_signals(&saved);
    // ── end critical section ─────────────────────────────────────────────────

    match on_err {
        None => Ok(()),
        // A swapon failure after a successful swapoff is "other" (transient),
        // never EPERM: swapoff already proved the cap is present.
        Some(e) => Err(ReclaimErr::Other(format!("swapon failed after swapoff (device left OFF!): {e}"))),
    }
}

/// Build swapon flags. If the device had a non-negative priority, restore it
/// explicitly via SWAP_FLAG_PREFER; otherwise pass 0 and let the kernel assign.
fn swapon_flags(priority: i32) -> libc::c_int {
    if priority >= 0 {
        (SWAP_FLAG_PREFER | (priority & SWAP_FLAG_PRIO_MASK)) as libc::c_int
    } else {
        0
    }
}

/// Block SIGINT/SIGTERM/SIGHUP/SIGQUIT, returning the previous mask to restore.
fn block_signals() -> libc::sigset_t {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGHUP);
        libc::sigaddset(&mut set, libc::SIGQUIT);
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigprocmask(libc::SIG_BLOCK, &set, &mut old);
        old
    }
}

fn restore_signals(old: &libc::sigset_t) {
    unsafe {
        libc::sigprocmask(libc::SIG_SETMASK, old, std::ptr::null_mut());
    }
}

fn last_errno_string() -> String {
    std::io::Error::last_os_error().to_string()
}

fn read_proc_swaps() -> String {
    fs::read_to_string("/proc/swaps").unwrap_or_default()
}

/// Parse /proc/swaps into the list of **zram** swap devices with their
/// priorities. Non-zram swaps (real disk partitions/files) are skipped — they
/// must never be disabled by this tool. The header line is skipped.
///
/// Validation anchors on the canonical device path `/dev/zram<N>` rather than a
/// basename `starts_with("zram")`, so a swapfile that merely happens to be named
/// `zram-something` is NOT mistaken for a zram block device.
///
/// Format: `Filename  Type  Size  Used  Priority`
fn parse_zram_swaps(swaps: &str) -> Vec<ZramSwap> {
    swaps
        .lines()
        .skip(1) // header
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let path = cols.next()?;
            if !is_zram_device_path(path) {
                return None;
            }
            // Priority is the 5th column; default to -1 (kernel-assigned) if absent.
            let priority = cols.nth(3).and_then(|p| p.parse::<i32>().ok()).unwrap_or(-1);
            Some(ZramSwap { path: path.to_string(), priority })
        })
        .collect()
}

/// True only for a canonical zram block-device path: `/dev/zram` followed by at
/// least one ASCII digit (e.g. `/dev/zram0`). Rejects swapfiles, disk
/// partitions, and lookalike names such as `/swap/zram-cache`.
fn is_zram_device_path(path: &str) -> bool {
    match path.strip_prefix("/dev/zram") {
        Some(rest) => !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// Basename of a `/dev/zramN` path (e.g. "zram0"), for locating its sysfs node.
fn device_basename(path: &str) -> Option<String> {
    path.rsplit('/').next().map(|s| s.to_string())
}

/// MemAvailable in MB from /proc/meminfo (world-readable). None if unreadable.
fn read_mem_available_mb() -> Option<u64> {
    let content = fs::read_to_string("/proc/meminfo").ok()?;
    parse_mem_available_kb(&content).map(|kb| kb / 1024)
}

/// Decompressed footprint (orig_data_size, field 0 of mm_stat) of one zram
/// device, in MB. World-readable sysfs; None if unreadable.
fn zram_orig_mb(basename: String) -> Option<u64> {
    let content = fs::read_to_string(format!("/sys/block/{basename}/mm_stat")).ok()?;
    content.split_whitespace().next()?.parse::<u64>().ok().map(|b| b / (1024 * 1024))
}

/// Parse `MemAvailable:` (kB) out of /proc/meminfo content.
fn parse_mem_available_kb(meminfo: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_zram_swap_with_priority() {
        let swaps = "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n\
                     /dev/zram0                              partition\t8388604\t4398892\t100\n";
        let v = parse_zram_swaps(swaps);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].path, "/dev/zram0");
        assert_eq!(v[0].priority, 100);
    }

    #[test]
    fn skips_real_disk_swap() {
        let swaps = "Filename Type Size Used Priority\n\
                     /dev/nvme0n1p3 partition 16777212 0 -2\n\
                     /dev/zram0 partition 8388604 4398892 100\n";
        let v = parse_zram_swaps(swaps);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].path, "/dev/zram0");
    }

    #[test]
    fn skips_swapfile() {
        let swaps = "Filename Type Size Used Priority\n\
                     /swapfile file 2097152 0 -3\n";
        assert!(parse_zram_swaps(swaps).is_empty());
    }

    #[test]
    fn skips_zram_lookalike_swapfile() {
        // A swapfile named like a zram device must NOT be treated as one — the
        // old basename starts_with("zram") check would have wrongly matched this.
        let swaps = "Filename Type Size Used Priority\n\
                     /swap/zram-cache file 2097152 0 -3\n";
        assert!(parse_zram_swaps(swaps).is_empty());
    }

    #[test]
    fn is_zram_device_path_accepts_canonical() {
        assert!(is_zram_device_path("/dev/zram0"));
        assert!(is_zram_device_path("/dev/zram12"));
    }

    #[test]
    fn is_zram_device_path_rejects_lookalikes() {
        assert!(!is_zram_device_path("/dev/zram"));        // no number
        assert!(!is_zram_device_path("/dev/zrama"));       // non-digit suffix
        assert!(!is_zram_device_path("/swap/zram-cache")); // not under /dev
        assert!(!is_zram_device_path("/dev/zram0x"));      // trailing non-digit
        assert!(!is_zram_device_path("/dev/nvme0n1p3"));   // real disk
    }

    #[test]
    fn device_basename_extracts_name() {
        assert_eq!(device_basename("/dev/zram0").as_deref(), Some("zram0"));
    }

    #[test]
    fn headroom_safe_only_when_avail_strictly_exceeds() {
        assert!(is_headroom_safe(5000, 4000));  // fits with room
        assert!(!is_headroom_safe(4000, 4000)); // exactly equal → unsafe (strict)
        assert!(!is_headroom_safe(3000, 4000)); // would OOM
        assert!(is_headroom_safe(1, 0));        // nothing to reclaim
    }

    #[test]
    fn parse_mem_available_reads_field() {
        let meminfo = "MemTotal:       16314800 kB\n\
                       MemFree:          812044 kB\n\
                       MemAvailable:    9381234 kB\n";
        assert_eq!(parse_mem_available_kb(meminfo), Some(9381234));
    }

    #[test]
    fn parse_mem_available_missing_is_none() {
        assert_eq!(parse_mem_available_kb("MemTotal: 100 kB\n"), None);
    }

    #[test]
    fn negative_priority_means_no_prefer_flag() {
        assert_eq!(swapon_flags(-1), 0);
    }

    #[test]
    fn positive_priority_sets_prefer_flag() {
        // 100 with SWAP_FLAG_PREFER set
        assert_eq!(swapon_flags(100), (SWAP_FLAG_PREFER | 100) as libc::c_int);
    }

    #[test]
    fn empty_swaps_yields_no_devices() {
        assert!(parse_zram_swaps("Filename Type Size Used Priority\n").is_empty());
    }
}
