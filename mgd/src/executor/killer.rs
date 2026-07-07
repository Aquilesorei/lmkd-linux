use std::io;
use std::time::Duration;
use std::thread;

use mgd_common::types::Pid;

use super::{read_start_time, OpResult};

/// SIGTERM → 5s wait → SIGKILL.
pub fn sigterm(pid: Pid) -> OpResult {
    let original_start = read_start_time(pid);

    let result = unsafe { libc::kill(pid.0 as libc::pid_t, libc::SIGTERM) };
    if result != 0 {
        return OpResult::fail(format!("SIGTERM failed: {}", io::Error::last_os_error()));
    }

    for _ in 0..10 {
        thread::sleep(Duration::from_millis(500));
        if !process_exists(pid) {
            return OpResult::success();
        }
    }

    // PID reused by different process — original is gone
    if original_start.is_some() && read_start_time(pid) != original_start {
        return OpResult::success();
    }

    sigkill(pid)
}

pub fn sigkill(pid: Pid) -> OpResult {
    let result = unsafe { libc::kill(pid.0 as libc::pid_t, libc::SIGKILL) };
    if result == 0 {
        OpResult::success()
    } else {
        OpResult::fail(format!("SIGKILL failed: {}", io::Error::last_os_error()))
    }
}

fn process_exists(pid: Pid) -> bool {
    unsafe { libc::kill(pid.0 as libc::pid_t, 0) == 0 }
}
