use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use std::path::PathBuf;


fn state_dir() -> PathBuf {
    mgd_common::util::home_dir().join(".local/share/mgd/state")
}

fn persist_json<T: serde::Serialize>(filename: &str, data: &T) {
    let path = state_dir().join(filename);
    if let Ok(json) = serde_json::to_string(data) {
        let _ = mgd_common::util::write_file_atomic(&path, &json);
    }
}

/// Tracks processes that have been frozen by the daemon
/// so they can be unfrozen when pressure drops
#[derive(Serialize, Deserialize)]
pub struct FrozenRegistry {
    /// pid -> (name, frozen_at_timestamp, start_time from /proc/pid/stat)
    frozen: HashMap<u32, (String, u64, u64)>,
}

impl FrozenRegistry {
    pub fn new() -> Self {
        FrozenRegistry { frozen: HashMap::new() }
    }

    pub fn load() -> Self {
        let path = state_dir().join("frozen.json");
        if let Ok(data) = std::fs::read_to_string(&path) {
            if let Ok(reg) = serde_json::from_str(&data) {
                return reg;
            }
        }
        Self::new()
    }

    pub fn save(&self) {
        persist_json("frozen.json", self);
    }

    /// Record a process as frozen, capturing its start_time for PID-recycle detection.
    /// Returns false if start_time can't be read (process vanished) — caller should unfreeze.
    pub fn add(&mut self, pid: u32, name: &str) -> bool {
        let Some(start_time) = super::read_start_time(pid) else {
            return false;
        };
        let now = mgd_common::util::unix_timestamp_secs();
        self.frozen.insert(pid, (name.to_string(), now, start_time));
        self.save();
        true
    }

    pub fn remove(&mut self, pid: u32) { 
        if self.frozen.remove(&pid).is_some() {
            self.save();
        }
    }

    pub fn frozen_pids(&self) -> Vec<u32> { self.frozen.keys().cloned().collect() }

    /// Unix timestamp when pid was frozen (0 if not found)
    pub fn frozen_at(&self, pid: u32) -> u64 {
        self.frozen.get(&pid).map(|(_, ts, _)| *ts).unwrap_or(0)
    }

    /// Process name recorded at freeze
    pub fn name(&self, pid: u32) -> &str {
        self.frozen.get(&pid).map(|(n, _, _)| n.as_str()).unwrap_or("")
    }

    /// Start time recorded at freeze (for PID recycle check)
    pub fn start_time(&self, pid: u32) -> u64 {
        self.frozen.get(&pid).map(|(_, _, st)| *st).unwrap_or(0)
    }

    pub fn is_frozen(&self, pid: u32) -> bool { self.frozen.contains_key(&pid) }

    pub fn count(&self) -> usize { self.frozen.len() }

    pub fn list(&self) -> Vec<(u32, String, u64)> {
        self.frozen.iter()
            .map(|(pid, (name, ts, _))| (*pid, name.clone(), *ts))
            .collect()
    }
}

/// Tracks processes that were checkpointed (state saved to disk, then killed)
/// so they can be restored when pressure drops
#[derive(Serialize, Deserialize)]
pub struct CheckpointRegistry {
    /// pid -> (name, snapshot_dir, rss_kb at checkpoint time, restore attempts)
    checkpointed: HashMap<u32, (String, PathBuf, u64, u32)>,
}

impl CheckpointRegistry {
    pub fn new() -> Self {
        CheckpointRegistry { checkpointed: HashMap::new() }
    }

    pub fn load() -> Self {
        let path = state_dir().join("checkpoint.json");
        if let Ok(data) = std::fs::read_to_string(&path) {
            if let Ok(reg) = serde_json::from_str(&data) {
                return reg;
            }
        }
        Self::new()
    }

    pub fn save(&self) {
        persist_json("checkpoint.json", self);
    }

    pub fn add(&mut self, pid: u32, name: &str, snapshot_dir: PathBuf, rss_kb: u64) {
        self.checkpointed.insert(pid, (name.to_string(), snapshot_dir, rss_kb, 0));
        self.save();
    }

    pub fn remove(&mut self, pid: u32) {
        if self.checkpointed.remove(&pid).is_some() {
            self.save();
        }
    }

    pub fn increment_attempts(&mut self, pid: u32) {
        if let Some(entry) = self.checkpointed.get_mut(&pid) {
            entry.3 += 1;
            self.save();
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
