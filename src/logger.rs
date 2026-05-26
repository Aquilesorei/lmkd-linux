use std::fs::{OpenOptions, create_dir_all};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Logger {
    log_path: PathBuf,
}

impl Logger {
    pub fn new() -> Self {
        let log_dir = crate::util::home_dir().join("memlogs");
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
        self.write_line(&format!(
            "[{}] {} pid={} name={} rss={:.0}MB result={}",
            timestamp_now(), entry.action, entry.pid, entry.name, entry.rss_mb, entry.result,
        ));
    }

    #[allow(dead_code)]
    pub fn log_pressure(&self, level: &str, avg10: f64, available_mb: f64) {
        self.write_line(&format!(
            "[{}] PRESSURE level={} avg10={:.2}% available={:.0}MB",
            timestamp_now(), level, avg10, available_mb,
        ));
    }

    fn write_line(&self, line: &str) {
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

pub struct LogEntry<'a> {
    pub action: &'a str,
    pub pid: u32,
    pub name: &'a str,
    pub rss_mb: f64,
    pub result: &'a str,
}

impl<'a> LogEntry<'a> {
    pub fn new(action: &'a str, pid: u32, name: &'a str, rss_mb: f64, result: &'a str) -> Self {
        LogEntry { action, pid, name, rss_mb, result }
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
