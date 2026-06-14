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
            OpResult { pid, success: true, error: None }
        }
        None => {
            OpResult { pid, success: true, error: None }
        }
        _ => send_signal(pid, libc::SIGCONT),
    }
}

/// Freeze only if the PID's start_time matches expectations (not recycled).
pub fn freeze_checked(pid: u32, expected_start_time: u64) -> OpResult {
    match read_start_time(pid) {
        Some(st) if st != expected_start_time => {
            OpResult { pid, success: false, error: Some("PID recycled — aborting freeze".into()) }
        }
        None => {
            OpResult { pid, success: false, error: Some("process gone".into()) }
        }
        _ => send_signal(pid, libc::SIGSTOP),
    }
}

fn send_signal(pid: u32, signal: i32) -> OpResult {
    let result = unsafe { libc::kill(pid as i32, signal) };
    if result == 0 {
        OpResult { pid, success: true, error: None }
    } else {
        OpResult { pid, success: false, error: Some(io::Error::last_os_error().to_string()) }
    }
}
