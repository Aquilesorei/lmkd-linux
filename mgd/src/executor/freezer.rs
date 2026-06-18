use std::io;

use super::{read_start_time, OpResult};

/// Send SIGSTOP to a process — pauses it completely.
pub fn freeze(pid: u32) -> OpResult {
    send_signal(pid, libc::SIGSTOP)
}

/// Send SIGCONT to a process — resumes a frozen process.
pub fn unfreeze(pid: u32) -> OpResult {
    send_signal(pid, libc::SIGCONT)
}

/// Unfreeze only if the process start_time still matches what we recorded.
/// Returns success=true (no-op) if PID was recycled — the original is gone.
pub fn unfreeze_checked(pid: u32, expected_start_time: u64) -> OpResult {
    match read_start_time(pid) {
        Some(st) if st != expected_start_time => {
            OpResult::success(pid)
        }
        None => {
            OpResult::success(pid)
        }
        _ => send_signal(pid, libc::SIGCONT),
    }
}

/// Freeze only if the PID's start_time matches expectations (not recycled).
pub fn freeze_checked(pid: u32, expected_start_time: u64) -> OpResult {
    match read_start_time(pid) {
        Some(st) if st != expected_start_time => {
            OpResult::fail(pid, "PID recycled — aborting freeze")
        }
        None => {
            OpResult::fail(pid, "process gone")
        }
        _ => send_signal(pid, libc::SIGSTOP),
    }
}

fn send_signal(pid: u32, signal: i32) -> OpResult {
    // Ensure the PID converts safely to an i32
    let target_pid = match i32::try_from(pid) {
        Ok(p) => p,
        Err(_) => return OpResult::fail(pid, "Invalid PID: exceeds i32 limits"),
    };

    let result = unsafe { libc::kill(target_pid, signal) };

    if result == 0 {
        OpResult::success(pid)
    } else {
        OpResult::fail(pid, std::io::Error::last_os_error().to_string())
    }
}