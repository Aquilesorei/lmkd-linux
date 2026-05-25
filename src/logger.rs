use std::fs::{OpenOptions, create_dir_all};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Logger {
    log_path: PathBuf,
}

impl Logger {
    pub fn new() -> Self {
        let log_dir = dirs_home().join("memlogs");
        let _ = create_dir_all(&log_dir);

        // New file per session, timestamped
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let log_path = log_dir.join(format!("mgd_{ts}.log"));

        Logger { log_path }
    }

    pub fn log(&self, entry: &LogEntry) {
        let line = format!(
            "[{}] {} pid={} name={} rss={:.0}MB result={}\n",
            entry.timestamp,
            entry.action,
            entry.pid,
            entry.name,
            entry.rss_mb,
            entry.result,
        );

        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }

    #[allow(dead_code)]
    pub fn log_pressure(&self, level: &str, avg10: f64, available_mb: f64) {
        let line = format!(
            "[{}] PRESSURE level={} avg10={:.2}% available={:.0}MB\n",
            timestamp_now(),
            level,
            avg10,
            available_mb,
        );

        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

pub struct LogEntry {
    pub timestamp: String,
    pub action: String,
    pub pid: u32,
    pub name: String,
    pub rss_mb: f64,
    pub result: String,
}

impl LogEntry {
    pub fn new(action: &str, pid: u32, name: &str, rss_mb: f64, result: &str) -> Self {
        LogEntry {
            timestamp: timestamp_now(),
            action: action.to_string(),
            pid,
            name: name.to_string(),
            rss_mb,
            result: result.to_string(),
        }
    }
}

fn timestamp_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}
