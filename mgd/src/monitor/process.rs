use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use mgd_common::error::MgdError;

#[derive(Debug, Clone)]
pub struct Process {
    pub pid: u32,
    pub name: String,
    /// Basename of /proc/PID/exe — untruncated, used for .desktop category lookup.
    pub exe_basename: Option<String>,
    pub rss_kb: u64,
    pub swap_kb: u64,
    pub oom_score: i32,
    /// Unified cgroup v2 path (e.g. `/user.slice/user-1000.slice/…`), cached to
    /// avoid re-reading `/proc/PID/cgroup` multiple times per evictor cycle.
    pub cgroup_path: Option<String>,
    /// CPU usage percent over the last sample interval (0.0 on first observation).
    pub cpu_pct: f32,
    pub majflt: u64,
}

/// pid → (total_ticks, unix_timestamp_secs) from previous list_processes() call.
static CPU_CACHE: LazyLock<Mutex<HashMap<u32, (u64, u64)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static CLK_TCK: LazyLock<u64> =
    LazyLock::new(|| unsafe { libc::sysconf(libc::_SC_CLK_TCK).max(1) as u64 });

/// Read all user processes from /proc, excluding ourselves and system processes
pub fn list_processes() -> Vec<Process> {
    let own_pid = std::process::id();
    let Ok(entries) = fs::read_dir("/proc") else {
        return vec![];
    };

    let procs: Vec<Process> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok().map(|pid| (pid, e.path())))
        .filter(|(pid, _)| *pid != own_pid)
        .filter_map(|(pid, path)| read_process(pid, &path).ok())
        .collect();
    prune_cpu_cache(&procs);
    procs
}

fn read_process(pid: u32, path: &Path) -> Result<Process, MgdError> {
    let our_uid = mgd_common::util::current_uid();
    let meta = fs::metadata(path)?;
    if meta.uid() != our_uid {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "not owned by user").into());
    }

    let cgroup_content = fs::read_to_string(path.join("cgroup"))?;
    if !mgd_common::process::is_cgroup_in_user_slice(&cgroup_content) {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "not in user cgroup").into());
    }

    let mut cgroup_path = None;
    for line in cgroup_content.lines() {
        if let Some(p) = line.strip_prefix("0::") {
            let p = p.trim();
            if p != "/" {
                cgroup_path = Some(p.to_string());
                break;
            }
        }
    }

    let status = fs::read_to_string(path.join("status"))?;

    let name = parse_status_field(&status, "Name:")
        .unwrap_or_else(|| "unknown".to_string());

    let rss_kb = parse_status_kb(&status, "VmRSS:")
        .unwrap_or(0);

    let swap_kb = parse_status_kb(&status, "VmSwap:")
        .unwrap_or(0);

    let oom_score = fs::read_to_string(path.join("oom_score"))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(0);

    let exe_basename = fs::read_link(path.join("exe"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));

    let (ticks, majflt) = mgd_common::process::read_proc_stat(pid);
    let cpu_pct = compute_cpu_pct(pid, ticks);

    Ok(Process { pid, name, exe_basename, rss_kb, swap_kb, oom_score, cgroup_path, cpu_pct, majflt })
}


fn compute_cpu_pct(pid: u32, ticks: Option<u64>) -> f32 {
    let ticks = match ticks {
        Some(t) => t,
        None => return 0.0,
    };
    let now = mgd_common::util::unix_timestamp_secs();
    let mut cache = CPU_CACHE.lock().unwrap();
    let cpu_pct = if let Some(&(prev_ticks, prev_time)) = cache.get(&pid) {
        let delta_ticks = ticks.saturating_sub(prev_ticks) as f32;
        let delta_secs = now.saturating_sub(prev_time).max(1) as f32;
        (delta_ticks / *CLK_TCK as f32 / delta_secs * 100.0).min(100.0 * num_cpus())
    } else {
        0.0
    };
    cache.insert(pid, (ticks, now));
    cpu_pct
}

fn prune_cpu_cache(live_pids: &[Process]) {
    let mut cache = CPU_CACHE.lock().unwrap();
    cache.retain(|pid, _| live_pids.iter().any(|p| p.pid == *pid));
}

fn num_cpus() -> f32 {
    unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN).max(1) as f32 }
}

fn parse_status_field(status: &str, field: &str) -> Option<String> {
    status.lines()
        .find(|l| l.starts_with(field))?
        .split_whitespace()
        .nth(1)
        .map(|s| s.to_string())
}

fn parse_status_kb(status: &str, field: &str) -> Option<u64> {
    status.lines()
        .find(|l| l.starts_with(field))?
        .split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()
}

