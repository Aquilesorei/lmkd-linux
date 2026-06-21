//! mgd-zram-reclaim — CAP_SYS_ADMIN helper: swapoff+swapon each zram device to
//! pull compressed pages back into RAM. Enforces its own OOM headroom floor.
//! Exit codes: 0 ok, 1 transient, 2 EPERM (not capped), 3 unsafe, 4 no meminfo.

use std::ffi::CString;
use std::fs;

use mgd_common::zram::is_zram_device_path;

// Linux swap(2) flags — not exported by libc 0.2. From <sys/swap.h>.
const SWAP_FLAG_PREFER: i32 = 0x8000;
const SWAP_FLAG_PRIO_MASK: i32 = 0x7fff;

const EXIT_OK: i32 = 0;
const EXIT_TRANSIENT: i32 = 1;
const EXIT_EPERM: i32 = 2;
const EXIT_REFUSED_UNSAFE: i32 = 3;
const EXIT_NO_MEMINFO: i32 = 4;

/// A zram swap device and its /proc/swaps priority, so swapon can restore it.
struct ZramSwap {
    path: String,
    /// May be negative (kernel-assigned).
    priority: i32,
}

fn main() {
    let devices = match parse_zram_swaps(&read_proc_swaps()) {
        v if v.is_empty() => {
            eprintln!("mgd-zram-reclaim: no zram swap devices found, nothing to do");
            std::process::exit(EXIT_OK);
        }
        v => v,
    };

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

/// Sum decompressed footprint across the devices and compare to MemAvailable.
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

/// Bare "it fits" bound (ratio 1.0) — never stricter than the daemon's 1.5x
/// policy, so it only catches direct/misconfigured invocations.
fn is_headroom_safe(avail_mb: u64, decompressed_mb: u64) -> bool {
    avail_mb > decompressed_mb
}

/// Distinguishes an uncapped binary (persistent) from a transient kernel error.
enum ReclaimErr {
    /// swapoff returned EPERM — binary lacks CAP_SYS_ADMIN.
    Eperm(String),
    Other(String),
}

/// swapoff then swapon one device, with SIGINT/TERM/HUP/QUIT blocked across the
/// pair so an interrupt can't leave it disabled.
fn reclaim_one(dev: &ZramSwap) -> Result<(), ReclaimErr> {
    let path = CString::new(dev.path.as_bytes())
        .map_err(|_| ReclaimErr::Other("device path contains NUL".to_string()))?;

    let saved = block_signals();

    let off = unsafe { libc::swapoff(path.as_ptr()) };
    if off != 0 {
        let errno = std::io::Error::last_os_error();
        let is_eperm = errno.raw_os_error() == Some(libc::EPERM);
        // swapoff failed → device still on, system not stranded.
        restore_signals(&saved);
        let msg = format!("swapoff failed: {errno}");
        return Err(if is_eperm { ReclaimErr::Eperm(msg) } else { ReclaimErr::Other(msg) });
    }

    // Device is now OFF — must re-enable before returning.
    let flags = swapon_flags(dev.priority);
    let mut on = unsafe { libc::swapon(path.as_ptr(), flags) };
    if on != 0 {
        // Retry before giving up — never strand the device on a transient failure.
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

    match on_err {
        None => Ok(()),
        // swapoff already proved the cap is present, so this is transient, not EPERM.
        Some(e) => Err(ReclaimErr::Other(format!("swapon failed after swapoff (device left OFF!): {e}"))),
    }
}

/// Restore the device's priority via SWAP_FLAG_PREFER, or let the kernel assign.
fn swapon_flags(priority: i32) -> libc::c_int {
    if priority >= 0 {
        (SWAP_FLAG_PREFER | (priority & SWAP_FLAG_PRIO_MASK)) as libc::c_int
    } else {
        0
    }
}

/// Block SIGINT/SIGTERM/SIGHUP/SIGQUIT, returning the previous mask.
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

/// zram swap devices with priorities from /proc/swaps. Anchored on `/dev/zram<N>`
/// so a swapfile named `zram-*` is never mistaken for a device — it must never
/// be disabled. Format: `Filename Type Size Used Priority`.
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
            // Priority is column 5; -1 (kernel-assigned) if absent.
            let priority = cols.nth(3).and_then(|p| p.parse::<i32>().ok()).unwrap_or(-1);
            Some(ZramSwap { path: path.to_string(), priority })
        })
        .collect()
}

fn device_basename(path: &str) -> Option<String> {
    path.rsplit('/').next().map(|s| s.to_string())
}

fn read_mem_available_mb() -> Option<u64> {
    let content = fs::read_to_string("/proc/meminfo").ok()?;
    let kb = content.lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .map(mgd_common::meminfo::parse_kb)?;
    Some(kb / 1024)
}

/// Decompressed footprint (`orig_data_size`, field 0 of mm_stat) of one device, MB.
fn zram_orig_mb(basename: String) -> Option<u64> {
    let content = fs::read_to_string(format!("/sys/block/{basename}/mm_stat")).ok()?;
    content.split_whitespace().next()?.parse::<u64>().ok().map(|b| b / (1024 * 1024))
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
        // A `zram-*` swapfile must not be treated as a zram device.
        let swaps = "Filename Type Size Used Priority\n\
                     /swap/zram-cache file 2097152 0 -3\n";
        assert!(parse_zram_swaps(swaps).is_empty());
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
