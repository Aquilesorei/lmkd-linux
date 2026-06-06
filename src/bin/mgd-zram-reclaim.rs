//! mgd-zram-reclaim — privileged swap-reclaim helper (Phase 3 / PRIVILEGE_DESIGN §2).
//!
//! Cycles every active **zram** swap device through `swapoff(2)` + `swapon(2)`,
//! which forces the kernel to pull all compressed pages back into RAM and then
//! re-enables the (now empty) device. This is how post-pressure RAM is returned
//! from zram. `swapoff`/`swapon` are genuine `CAP_SYS_ADMIN` syscalls — there is
//! no narrower capability — so this lives in a separate, minimal binary carrying
//! only that one capability (`setcap cap_sys_admin+ep`), never SUID-root.
//! All *policy* (when to run, headroom/OOM gating, cooldown) lives in the
//! unprivileged daemon. This binary is deliberately dumb: it reclaims and exits.
//!
//! Exit status: 0 = all devices reclaimed (or none present, nothing to do);
//! non-zero = at least one swapoff/swapon failed (caller treats as "unavailable"
//! / disables the feature).

use std::ffi::CString;
use std::fs;

// Linux swap(2) flags — not exported by libc 0.2, defined here per <sys/swap.h>.
const SWAP_FLAG_PREFER: i32 = 0x8000;
const SWAP_FLAG_PRIO_MASK: i32 = 0x7fff;

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
    // already run in glibc secure-execution mode (LD_* neutralized); we drop the
    // rest too so nothing downstream can be influenced by inherited env.
    // (We make no subprocess and read no env var; this is belt-and-suspenders.)

    let devices = match parse_zram_swaps(&read_proc_swaps()) {
        v if v.is_empty() => {
            // Nothing mounted as zram swap — not an error, just nothing to do.
            eprintln!("mgd-zram-reclaim: no zram swap devices found, nothing to do");
            std::process::exit(0);
        }
        v => v,
    };

    let mut failures = 0u32;
    for dev in &devices {
        if let Err(msg) = reclaim_one(dev) {
            eprintln!("mgd-zram-reclaim: {}: {msg}", dev.path);
            failures += 1;
        } else {
            eprintln!("mgd-zram-reclaim: {} reclaimed (swapoff+swapon)", dev.path);
        }
    }

    std::process::exit(if failures == 0 { 0 } else { 1 });
}

/// Reclaim a single device: swapoff (pull pages back to RAM) then swapon
/// (re-enable empty), with SIGINT/TERM/HUP/QUIT blocked across the pair so an
/// interrupt can never leave this device disabled.
fn reclaim_one(dev: &ZramSwap) -> Result<(), String> {
    let path = CString::new(dev.path.as_bytes())
        .map_err(|_| "device path contains NUL".to_string())?;

    // ── critical section: block terminating signals ──────────────────────────
    let saved = block_signals();

    let off = unsafe { libc::swapoff(path.as_ptr()) };
    if off != 0 {
        let err = last_errno_string();
        // Device is still on (swapoff failed) — system not stranded. Restore the
        // signal mask and report.
        restore_signals(&saved);
        return Err(format!("swapoff failed: {err}"));
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
        Some(e) => Err(format!("swapon failed after swapoff (device left OFF!): {e}")),
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
/// Format: `Filename  Type  Size  Used  Priority`
fn parse_zram_swaps(swaps: &str) -> Vec<ZramSwap> {
    swaps
        .lines()
        .skip(1) // header
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let path = cols.next()?;
            // Validate: the device basename must start with "zram".
            let base = path.rsplit('/').next()?;
            if !base.starts_with("zram") {
                return None;
            }
            // Priority is the 5th column; default to -1 (kernel-assigned) if absent.
            let priority = cols.nth(3).and_then(|p| p.parse::<i32>().ok()).unwrap_or(-1);
            Some(ZramSwap { path: path.to_string(), priority })
        })
        .collect()
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
