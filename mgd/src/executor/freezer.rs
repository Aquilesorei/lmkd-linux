use mgd_common::types::Pid;

use super::{read_start_time, OpResult};

pub fn freeze(pid: Pid) -> OpResult {
    send_signal(pid, libc::SIGSTOP)
}

pub fn unfreeze(pid: Pid) -> OpResult {
    send_signal(pid, libc::SIGCONT)
}

/// Returns success (no-op) if PID was recycled — original is gone.
pub fn unfreeze_checked(pid: Pid, expected_start_time: u64) -> OpResult {
    match read_start_time(pid) {
        Some(st) if st != expected_start_time => OpResult::success(),
        None => OpResult::success(),
        _ => send_signal(pid, libc::SIGCONT),
    }
}

/// Aborts if PID start_time changed (recycle guard).
pub fn freeze_checked(pid: Pid, expected_start_time: u64) -> OpResult {
    match read_start_time(pid) {
        Some(st) if st != expected_start_time => OpResult::fail("PID recycled — aborting freeze"),
        None => OpResult::fail("process gone"),
        _ => send_signal(pid, libc::SIGSTOP),
    }
}

fn send_signal(pid: Pid, signal: i32) -> OpResult {
    let target_pid = match i32::try_from(pid.0) {
        Ok(p) => p,
        Err(_) => return OpResult::fail("Invalid PID: exceeds i32 limits"),
    };
    let result = unsafe { libc::kill(target_pid, signal) };
    if result == 0 {
        OpResult::success()
    } else {
        OpResult::fail(std::io::Error::last_os_error().to_string())
    }
}
