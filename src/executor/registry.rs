use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Tracks processes that have been frozen by the daemon
/// so they can be unfrozen when pressure drops
pub struct FrozenRegistry {
    /// pid -> (name, frozen_at_timestamp)
    frozen: HashMap<u32, (String, u64)>,
}

impl FrozenRegistry {
    pub fn new() -> Self {
        FrozenRegistry {
            frozen: HashMap::new(),
        }
    }

    /// Record a process as frozen
    pub fn add(&mut self, pid: u32, name: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.frozen.insert(pid, (name.to_string(), now));
    }

    /// Remove a process from the registry (after unfreezing)
    pub fn remove(&mut self, pid: u32) {
        self.frozen.remove(&pid);
    }

    /// Get all frozen PIDs
    pub fn frozen_pids(&self) -> Vec<u32> {
        self.frozen.keys().cloned().collect()
    }

    /// Unix timestamp when pid was frozen (0 if not found)
    pub fn frozen_at(&self, pid: u32) -> u64 {
        self.frozen.get(&pid).map(|(_, ts)| *ts).unwrap_or(0)
    }

    /// Check if a PID is tracked as frozen
    pub fn is_frozen(&self, pid: u32) -> bool {
        self.frozen.contains_key(&pid)
    }

    /// How many processes are currently frozen
    pub fn count(&self) -> usize {
        self.frozen.len()
    }

    /// Get frozen process info for display
    #[allow(dead_code)]
    pub fn list(&self) -> Vec<(u32, String, u64)> {
        self.frozen.iter()
            .map(|(pid, (name, ts))| (*pid, name.clone(), *ts))
            .collect()
    }
}

/// Tracks processes that were checkpointed (state saved to disk, then killed)
/// so they can be restored when pressure drops
pub struct CheckpointRegistry {
    /// pid -> (name, snapshot_dir, rss_kb at checkpoint time, restore attempts)
    checkpointed: HashMap<u32, (String, PathBuf, u64, u32)>,
}

impl CheckpointRegistry {
    pub fn new() -> Self {
        CheckpointRegistry { checkpointed: HashMap::new() }
    }

    pub fn add(&mut self, pid: u32, name: &str, snapshot_dir: PathBuf, rss_kb: u64) {
        self.checkpointed.insert(pid, (name.to_string(), snapshot_dir, rss_kb, 0));
    }

    pub fn remove(&mut self, pid: u32) {
        self.checkpointed.remove(&pid);
    }

    pub fn increment_attempts(&mut self, pid: u32) {
        if let Some(entry) = self.checkpointed.get_mut(&pid) {
            entry.3 += 1;
        }
    }

    /// Returns entries sorted by RSS ascending (lightest first).
    pub fn entries_lightest_first(&self) -> Vec<(u32, String, PathBuf, u64, u32)> {
        let mut v: Vec<_> = self.checkpointed.iter()
            .map(|(pid, (name, dir, rss, attempts))| (*pid, name.clone(), dir.clone(), *rss, *attempts))
            .collect();
        v.sort_by_key(|(_, _, _, rss, _)| *rss);
        v
    }

    pub fn count(&self) -> usize {
        self.checkpointed.len()
    }
}
