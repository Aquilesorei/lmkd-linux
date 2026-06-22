pub mod freezer;
pub mod killer;
pub mod registry;
pub mod checkpoint;

use std::fs;

#[derive(Debug)]
pub struct OpResult {
    pub success: bool,
    pub error: Option<String>,
}

impl OpResult {
    pub fn success() -> Self {
        Self { success: true, error: None }
    }

    pub fn fail<S: Into<String>>(error: S) -> Self {
        Self { success: false, error: Some(error.into()) }
    }
}
/// Read process start time from /proc/pid/stat (field 22, clock ticks since boot).
/// Uses rsplit_once to find the *last* ") " — handles comm names containing ")".
pub fn read_start_time(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    after_comm.split_whitespace().nth(19).and_then(|s| s.parse().ok())
}
