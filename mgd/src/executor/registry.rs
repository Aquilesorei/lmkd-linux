use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use std::path::PathBuf;

use mgd_common::types::{Kb, Pid};


fn state_dir() -> PathBuf {
    mgd_common::util::home_dir().join(".local/share/mgd/state")
}

fn persist_json<T: serde::Serialize>(filename: &str, data: &T) {
    // Unit tests exercise registry mutations with fixture data; they must never
    // clobber the real daemon's state files under ~/.local/share/mgd/state.
    if cfg!(test) {
        return;
    }
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
    frozen: HashMap<Pid, (String, u64, u64)>,
}

impl FrozenRegistry {
    pub fn new() -> Self {
        FrozenRegistry { frozen: HashMap::new() }
    }

    pub fn load() -> Self {
        let path = state_dir().join("frozen.json");
        if let Ok(data) = std::fs::read_to_string(&path)
            && let Ok(reg) = serde_json::from_str(&data) {
                return reg;
            }
        Self::new()
    }

    pub fn save(&self) {
        persist_json("frozen.json", self);
    }

    /// Record a process as frozen, capturing its start_time for PID-recycle detection.
    /// Returns false if start_time can't be read (process vanished) — caller should unfreeze.
    pub fn add(&mut self, pid: Pid, name: &str) -> bool {
        let Some(start_time) = super::read_start_time(pid) else {
            return false;
        };
        let now = mgd_common::util::unix_timestamp_secs();
        self.frozen.insert(pid, (name.to_string(), now, start_time));
        self.save();
        true
    }

    pub fn remove(&mut self, pid: Pid) {
        if self.frozen.remove(&pid).is_some() {
            self.save();
        }
    }

    pub fn frozen_pids(&self) -> Vec<Pid> { self.frozen.keys().cloned().collect() }

    /// Unix timestamp when pid was frozen (0 if not found)
    pub fn frozen_at(&self, pid: Pid) -> u64 {
        self.frozen.get(&pid).map(|(_, ts, _)| *ts).unwrap_or(0)
    }

    /// Process name recorded at freeze
    pub fn name(&self, pid: Pid) -> &str {
        self.frozen.get(&pid).map(|(n, _, _)| n.as_str()).unwrap_or("")
    }

    /// Start time recorded at freeze (for PID recycle check)
    pub fn start_time(&self, pid: Pid) -> u64 {
        self.frozen.get(&pid).map(|(_, _, st)| *st).unwrap_or(0)
    }

    pub fn is_frozen(&self, pid: Pid) -> bool { self.frozen.contains_key(&pid) }

    pub fn count(&self) -> usize { self.frozen.len() }

    pub fn list(&self) -> Vec<(Pid, String, u64)> {
        self.frozen.iter()
            .map(|(pid, (name, ts, _))| (*pid, name.clone(), *ts))
            .collect()
    }
}

/// Tracks processes that were checkpointed (state saved to disk, then killed)
/// so they can be restored when pressure drops
#[derive(Serialize, Deserialize)]
pub struct CheckpointRegistry {
    /// pid -> (name, snapshot_dir, rss at checkpoint time, restore attempts)
    checkpointed: HashMap<Pid, (String, PathBuf, Kb, u32)>,
}

impl CheckpointRegistry {
    pub fn new() -> Self {
        CheckpointRegistry { checkpointed: HashMap::new() }
    }

    pub fn load() -> Self {
        let path = state_dir().join("checkpoint.json");
        if let Ok(data) = std::fs::read_to_string(&path)
            && let Ok(reg) = serde_json::from_str(&data) {
                return reg;
            }
        Self::new()
    }

    pub fn save(&self) {
        persist_json("checkpoint.json", self);
    }

    pub fn add(&mut self, pid: Pid, name: &str, snapshot_dir: PathBuf, rss: Kb) {
        self.checkpointed.insert(pid, (name.to_string(), snapshot_dir, rss, 0));
        self.save();
    }

    pub fn remove(&mut self, pid: Pid) {
        if self.checkpointed.remove(&pid).is_some() {
            self.save();
        }
    }

    pub fn increment_attempts(&mut self, pid: Pid) {
        if let Some(entry) = self.checkpointed.get_mut(&pid) {
            entry.3 += 1;
            self.save();
        }
    }

    /// Returns entries sorted by RSS ascending (lightest first).
    pub fn entries_lightest_first(&self) -> Vec<(Pid, String, PathBuf, Kb, u32)> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pre-newtype on-disk formats must load unchanged and re-serialize
    /// byte-identically — `#[serde(transparent)]` on Pid/Kb guarantees no
    /// migration of ~/.local/share/mgd/state/ is needed.
    #[test]
    fn frozen_state_file_format_round_trips_unchanged() {
        let fixture = r#"{"frozen":{"1234":["firefox",1751700000,54321]}}"#;
        let reg: FrozenRegistry = serde_json::from_str(fixture).unwrap();
        assert!(reg.is_frozen(Pid(1234)));
        assert_eq!(reg.name(Pid(1234)), "firefox");
        assert_eq!(reg.frozen_at(Pid(1234)), 1_751_700_000);
        assert_eq!(reg.start_time(Pid(1234)), 54_321);
        assert_eq!(serde_json::to_string(&reg).unwrap(), fixture);
    }

    #[test]
    fn checkpoint_state_file_format_round_trips_unchanged() {
        let fixture = r#"{"checkpointed":{"999":["idea","/home/u/.local/share/mgd/snapshots/999_idea",2048000,1]}}"#;
        let reg: CheckpointRegistry = serde_json::from_str(fixture).unwrap();
        let entries = reg.entries_lightest_first();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, Pid(999));
        assert_eq!(entries[0].3, Kb(2_048_000));
        assert_eq!(entries[0].4, 1);
        assert_eq!(serde_json::to_string(&reg).unwrap(), fixture);
    }
}
