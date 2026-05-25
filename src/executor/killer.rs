use std::io;
use std::time::Duration;
use std::thread;
use std::fs;

#[derive(Debug)]
#[allow(dead_code)]
pub struct KillResult {
    pub pid: u32,
    pub success: bool,
    pub error: Option<String>,
}

/// Send SIGTERM — graceful shutdown request.
/// Gives the process 5 seconds to clean up before SIGKILL.
pub fn terminate(pid: u32) -> KillResult {
    // Record start time before SIGTERM to detect PID reuse before SIGKILL
    let original_start = read_start_time(pid);

    let result = unsafe { libc::kill(pid as i32, libc::SIGTERM) };

    if result != 0 {
        let err = io::Error::last_os_error();
        return KillResult {
            pid,
            success: false,
            error: Some(format!("SIGTERM failed: {err}")),
        };
    }

    // Wait up to 5 seconds for the process to exit
    for _ in 0..10 {
        thread::sleep(Duration::from_millis(500));
        if !process_exists(pid) {
            return KillResult { pid, success: true, error: None };
        }
    }

    // If the PID was reused by a different process, don't SIGKILL the wrong victim
    if original_start.is_some() && read_start_time(pid) != original_start {
        return KillResult { pid, success: true, error: None };
    }

    // Still alive after 5s — escalate to SIGKILL
    kill(pid)
}

/// Read the process start time from /proc/pid/stat to detect PID reuse.
/// starttime is field 22 (1-based), which is index 19 after the "(comm) " prefix.
fn read_start_time(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // stat format: "pid (comm) state ..." — comm may contain spaces, parse past closing ')'
    let after_comm = stat.splitn(2, ") ").nth(1)?;
    after_comm.split_whitespace().nth(19).and_then(|s| s.parse().ok())
}

/// Send SIGKILL — immediate forced termination.
/// No cleanup, no grace period.
pub fn kill(pid: u32) -> KillResult {
    let result = unsafe { libc::kill(pid as i32, libc::SIGKILL) };

    if result == 0 {
        KillResult { pid, success: true, error: None }
    } else {
        let err = io::Error::last_os_error();
        KillResult {
            pid,
            success: false,
            error: Some(format!("SIGKILL failed: {err}")),
        }
    }
}

/// Check if a process still exists
fn process_exists(pid: u32) -> bool {
    // kill(pid, 0) = check if process exists without sending a signal
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0
}
