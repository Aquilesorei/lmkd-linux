use std::io;

/// Result of a freeze/unfreeze operation
#[derive(Debug)]
pub struct FreezeResult {
    pub pid: u32,
    pub action: FreezeAction,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug)]
pub enum FreezeAction {
    Freeze,
    Unfreeze,
}

impl std::fmt::Display for FreezeAction {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            FreezeAction::Freeze   => write!(f, "FREEZE"),
            FreezeAction::Unfreeze => write!(f, "UNFREEZE"),
        }
    }
}

/// Send SIGSTOP to a process — pauses it completely.
/// The process remains in memory but stops executing.
/// Reversible with unfreeze().
pub fn freeze(pid: u32) -> FreezeResult {
    send_signal(pid, libc::SIGSTOP, FreezeAction::Freeze)
}

/// Send SIGCONT to a process — resumes a frozen process.
pub fn unfreeze(pid: u32) -> FreezeResult {
    send_signal(pid, libc::SIGCONT, FreezeAction::Unfreeze)
}

fn send_signal(pid: u32, signal: i32, action: FreezeAction) -> FreezeResult {
    let result = unsafe { libc::kill(pid as i32, signal) };

    if result == 0 {
        FreezeResult { pid, action, success: true, error: None }
    } else {
        let err = io::Error::last_os_error();
        FreezeResult {
            pid,
            action,
            success: false,
            error: Some(err.to_string()),
        }
    }
}