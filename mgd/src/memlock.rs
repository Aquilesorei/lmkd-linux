use mgd_common::output::locked_print;

/// Check `CapEff` in /proc/self/status for CAP_IPC_LOCK (bit 14).
fn has_cap_ipc_lock() -> bool {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else { return false };
    status.lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
        .map(|caps| caps & (1 << 14) != 0) // CAP_IPC_LOCK = 14
        .unwrap_or(false)
}

pub fn try_lock_memory() {
    let unlimited = has_cap_ipc_lock() || unsafe {
        let mut rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut rl) == 0
            && rl.rlim_cur == libc::RLIM_INFINITY
    };

    unsafe {
        if unlimited {
            if libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) == 0 {
                locked_print("[core] mlockall(MCL_CURRENT|MCL_FUTURE): all pages locked in RAM");
                return;
            }
            mgd_common::sync_print!(
                "[core] Warning: mlockall failed despite unlimited memlock: {}",
                std::io::Error::last_os_error()
            );
        }
        if libc::mlockall(libc::MCL_CURRENT) == 0 {
            locked_print(
                "[core] mlockall(MCL_CURRENT): current pages locked; future allocations \
                 unlocked (grant cap_ipc_lock on mgd for full locking — see install.sh)"
            );
        } else {
            mgd_common::sync_print!(
                "[core] Running without mlockall (RLIMIT_MEMLOCK too small, no CAP_IPC_LOCK): {}",
                std::io::Error::last_os_error()
            );
        }
    }
}