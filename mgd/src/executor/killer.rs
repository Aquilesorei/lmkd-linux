use std::io;
use std::time::Duration;
use std::thread;

use super::{read_start_time, OpResult};

/// Send SIGTERM — graceful shutdown request.
/// Gives the process 5 seconds to clean up before SIGKILL.
pub fn sigterm(pid: u32) -> OpResult {
    let original_start = read_start_time(pid);

    let result = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    if result != 0 {
        return OpResult::fail(pid,format!("SIGTERM failed: {}", io::Error::last_os_error()));
    }

    for _ in 0..10 {
        thread::sleep(Duration::from_millis(500));
        if !process_exists(pid) {
            return OpResult::success(pid);
        }
    }

    // PID reused by a different process — original is gone
    if original_start.is_some() && read_start_time(pid) != original_start {
        return OpResult::success(pid);
    }

    sigkill(pid)
}

/// Send SIGKILL — immediate forced termination.
pub fn sigkill(pid: u32) -> OpResult {
    let result = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    if result == 0 {
        OpResult::success(pid)
    } else {
        OpResult::fail(pid, format!("SIGKILL failed: {}", io::Error::last_os_error()))
    }
}

fn process_exists(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
