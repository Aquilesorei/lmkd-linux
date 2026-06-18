use std::collections::{HashMap, HashSet};
use crate::monitor::process::Process;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThrottledState {
    None,
    WeightOnly,
    Full,
}

pub(crate) struct ThrottleManager {
    states:  HashMap<String, ThrottledState>,
    tracker: HashMap<String, std::time::Instant>,
}

impl ThrottleManager {
    pub(crate) fn new() -> Self {
        Self {
            states:  HashMap::new(),
            tracker: HashMap::new(),
        }
    }

    pub(crate) fn update(&mut self, plan_procs: &[&Process], active_pid: Option<u32>) {
        let mut cgroup_groups: HashMap<String, Vec<&Process>> = HashMap::new();
        for p in plan_procs {
            if let Some(path) = p.cgroup_path.clone() {
                cgroup_groups.entry(path).or_default().push(p);
            }
        }

        let foreground_cgroup = active_pid.and_then(|apid| {
            plan_procs.iter()
                .find(|p| p.pid == apid)
                .and_then(|p| p.cgroup_path.clone())
        });

        let active_cgroups: HashSet<&String> = cgroup_groups.keys().collect();
        self.tracker.retain(|p, _| active_cgroups.contains(p));
        self.states.retain(|p, _| active_cgroups.contains(p));

        for (cgroup_path, processes) in &cgroup_groups {
            let current = self.states.get(cgroup_path).copied().unwrap_or(ThrottledState::None);

            if Some(cgroup_path) == foreground_cgroup.as_ref() {
                if current != ThrottledState::None {
                    restore_cgroup_cpu(cgroup_path);
                    self.states.insert(cgroup_path.clone(), ThrottledState::None);
                    mgd_common::sync_print!(
                        "[throttle] Restored foreground cgroup {} to normal CPU shares",
                        cgroup_path
                    );
                }
                self.tracker.remove(cgroup_path);
                continue;
            }

            let mut min_priority = 100u8;
            let mut debug_name = String::new();
            for p in processes {
                let prio = crate::engine::decision::get_priority(&p.name, p.exe_basename.as_deref());
                if prio < min_priority {
                    min_priority = prio;
                    debug_name = p.name.clone();
                }
            }

            if min_priority < 60 {
                if current != ThrottledState::None {
                    restore_cgroup_cpu(cgroup_path);
                    self.states.insert(cgroup_path.clone(), ThrottledState::None);
                    mgd_common::sync_print!(
                        "[throttle] Restored background cgroup {} to normal CPU shares (priority < 60)",
                        cgroup_path
                    );
                }
                self.tracker.remove(cgroup_path);
                continue;
            }

            let background_duration = self.tracker
                .entry(cgroup_path.clone())
                .or_insert_with(std::time::Instant::now)
                .elapsed()
                .as_secs();

            let target = if background_duration >= 10 {
                if min_priority >= 80 { ThrottledState::Full } else { ThrottledState::WeightOnly }
            } else {
                ThrottledState::None
            };

            if target != current {
                match target {
                    ThrottledState::None => {
                        restore_cgroup_cpu(cgroup_path);
                        mgd_common::sync_print!("[throttle] Unthrottled cgroup {}", cgroup_path);
                    }
                    ThrottledState::WeightOnly => {
                        if write_cgroup_cpu_weight(cgroup_path, 1).is_ok() {
                            let _ = write_cgroup_cpu_max(cgroup_path, "max 100000");
                            mgd_common::sync_print!(
                                "[throttle] Set weight=1 for background cgroup {} (e.g. {})",
                                cgroup_path, debug_name
                            );
                        }
                    }
                    ThrottledState::Full => {
                        if write_cgroup_cpu_weight(cgroup_path, 1).is_ok()
                            && write_cgroup_cpu_max(cgroup_path, "50000 100000").is_ok()
                        {
                            mgd_common::sync_print!(
                                "[throttle] Capped CPU & weight=1 for low-priority cgroup {} (e.g. {})",
                                cgroup_path, debug_name
                            );
                        }
                    }
                }
                self.states.insert(cgroup_path.clone(), target);
            }
        }
    }

    /// Restore all throttled cgroups to default CPU shares. Called on daemon shutdown.
    pub(crate) fn restore_all(&self) {
        for path in self.states.keys() {
            restore_cgroup_cpu(path);
        }
    }

    pub(crate) fn snapshot(&self) -> HashMap<String, ThrottledState> {
        self.states.clone()
    }
}

pub(crate) fn cgroup_sysfs_path(cgroup_path: &str, attr: &str) -> std::path::PathBuf {
    std::path::Path::new("/sys/fs/cgroup")
        .join(cgroup_path.trim_start_matches('/'))
        .join(attr)
}

fn restore_cgroup_cpu(path: &str) {
    let _ = write_cgroup_cpu_weight(path, 100);
    let _ = write_cgroup_cpu_max(path, "max 100000");
}

pub(crate) fn write_cgroup_cpu_weight(cgroup_path: &str, weight: u32) -> Result<(), std::io::Error> {
    let path = cgroup_sysfs_path(cgroup_path, "cpu.weight");
    if path.exists() {
        std::fs::write(&path, format!("{}", weight))?;
        return Ok(());
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, "cpu.weight not found"))
}

pub(crate) fn write_cgroup_cpu_max(cgroup_path: &str, max_limit: &str) -> Result<(), std::io::Error> {
    let path = cgroup_sysfs_path(cgroup_path, "cpu.max");
    if path.exists() {
        std::fs::write(&path, max_limit)?;
        return Ok(());
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, "cpu.max not found"))
}
