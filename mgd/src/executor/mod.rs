pub mod freezer;
pub mod killer;
pub mod registry;
pub mod checkpoint;

use std::fs;

use mgd_common::types::Pid;

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
pub fn read_start_time(pid: Pid) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    after_comm.split_whitespace().nth(19).and_then(|s| s.parse().ok())
}

/// Port over the destructive side effects of plan execution. `execute_plan()` /
/// `execute_decision()` (evictor.rs) orchestrate through this seam so their
/// branching — skip-frozen, destructive counting, post-freeze reclaim, checkpoint
/// fallback — is unit-testable with a mock. Logging and event recording stay
/// outside the sink: they are already-tested infrastructure.
pub trait ActionSink {
    /// SIGSTOP with PID-recycle guard (start_time re-read before signalling).
    fn freeze(&mut self, pid: Pid) -> OpResult;
    /// SIGCONT — rollback for a freeze whose registry fingerprint failed.
    fn unfreeze(&mut self, pid: Pid) -> OpResult;
    /// SIGTERM→wait→SIGKILL, run asynchronously (returns immediately).
    fn terminate(&mut self, pid: Pid) -> OpResult;
    /// Immediate SIGKILL.
    fn kill(&mut self, pid: Pid) -> OpResult;
    /// CRIU dump via the mgd-checkpoint helper; SIGKILL on successful dump.
    fn checkpoint(&mut self, pid: Pid, name: &str) -> checkpoint::CheckpointResult;
    /// Write `memory.reclaim` on a cgroup. `Ok(true)` = wrote, `Ok(false)` =
    /// silently skipped (zero bytes, non-leaf, EACCES).
    fn reclaim(&mut self, cgroup: &str, bytes: u64) -> std::io::Result<bool>;
}

/// The only production `ActionSink` — delegates to the real executors.
pub struct RealSink;

impl ActionSink for RealSink {
    fn freeze(&mut self, pid: Pid) -> OpResult {
        // Abort if start_time is gone rather than freeze a recycled PID.
        match read_start_time(pid) {
            Some(st) => freezer::freeze_checked(pid, st),
            None => OpResult::fail("process vanished before freeze"),
        }
    }

    fn unfreeze(&mut self, pid: Pid) -> OpResult {
        freezer::unfreeze(pid)
    }

    fn terminate(&mut self, pid: Pid) -> OpResult {
        // SIGTERM→wait→SIGKILL blocks up to 5s; run it off-thread so the
        // responder isn't stalled per process at Critical.
        std::thread::spawn(move || { killer::sigterm(pid); });
        OpResult::success()
    }

    fn kill(&mut self, pid: Pid) -> OpResult {
        killer::sigkill(pid)
    }

    fn checkpoint(&mut self, pid: Pid, name: &str) -> checkpoint::CheckpointResult {
        checkpoint::checkpoint(pid, name)
    }

    fn reclaim(&mut self, cgroup: &str, bytes: u64) -> std::io::Result<bool> {
        crate::evictor::reclaim_cgroup(cgroup, bytes)
    }
}
