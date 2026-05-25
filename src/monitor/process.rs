use std::fs;
use std::path::Path;

#[derive(Debug)]
pub struct Process {
    pub pid: u32,
    pub name: String,
    pub rss_kb: u64,
    pub swap_kb: u64,
    pub oom_score: i32,
}

#[derive(Debug)]
pub enum ProcError {
    Io(std::io::Error),
    Parse(String),
}

impl From<std::io::Error> for ProcError {
    fn from(e: std::io::Error) -> Self {
        ProcError::Io(e)
    }
}

impl std::fmt::Display for ProcError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ProcError::Io(e) => write!(f, "IO: {e}"),
            ProcError::Parse(s) => write!(f, "Parse: {s}"),
        }
    }
}

/// Read all user processes from /proc
pub fn list_processes() -> Vec<Process> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return vec![];
    };

    let vec1 = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok().map(|pid| (pid, e.path())))
        .filter_map(|(pid, path)| read_process(pid, &path).ok())
        .collect();
    vec1
}

fn read_process(pid: u32, path: &Path) -> Result<Process, ProcError> {
    let status = fs::read_to_string(path.join("status"))?;

    let name = parse_status_field(&status, "Name:")
        .unwrap_or("unknown".to_string());

    let rss_kb = parse_status_kb(&status, "VmRSS:")
        .unwrap_or(0);

    let swap_kb = parse_status_kb(&status, "VmSwap:")
        .unwrap_or(0);

    let oom_score = fs::read_to_string(path.join("oom_score"))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(0);

    Ok(Process { pid, name, rss_kb, swap_kb, oom_score })
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