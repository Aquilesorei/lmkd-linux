use std::io;
use std::time::Duration;
use std::thread;

use super::{read_start_time, OpResult};

/// SIGTERM → 5s wait → SIGKILL.
pub fn sigterm(pid: u32) -> OpResult {
    let original_start = read_start_time(pid);

    let result = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
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

pub fn sigkill(pid: u32) -> OpResult {
    let result = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    if result == 0 {
        OpResult::success()
    } else {
        OpResult::fail(format!("SIGKILL failed: {}", io::Error::last_os_error()))
    }
}

fn process_exists(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
