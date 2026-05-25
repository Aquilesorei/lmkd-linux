use std::collections::HashMap;
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
