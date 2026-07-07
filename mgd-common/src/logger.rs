use std::fs::{OpenOptions, create_dir_all, read_dir};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Copy, Clone)]
pub enum LogAction {
    Freeze,
    FreezeReclaim,
    IdleFreeze,
    Terminate,
    Kill,
    KillManual,
    Checkpoint,
    Unfreeze,
    Restore,
    RestoreAbandon,
    RestoreFail,
    Reclaim,
    EarlyReclaim,
    Zram,
    Cache,
    Calibrate,
    SpikeFreeze,
    SpikeUnfreeze,
    SpikeUnfreezeTimeout,
    SpikeUnfreezeOrphan,
    Cycle,
}

impl LogAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Freeze               => "FREEZE",
            Self::FreezeReclaim        => "FREEZE_RECLAIM",
            Self::IdleFreeze           => "IDLE_FREEZE",
            Self::Terminate            => "TERMINATE",
            Self::Kill                 => "KILL",
            Self::KillManual           => "KILL_MANUAL",
            Self::Checkpoint           => "CHECKPOINT",
            Self::Unfreeze             => "UNFREEZE",
            Self::Restore              => "RESTORE",
            Self::RestoreAbandon       => "RESTORE_ABANDON",
            Self::RestoreFail          => "RESTORE_FAIL",
            Self::Reclaim              => "RECLAIM",
            Self::EarlyReclaim         => "EARLY_RECLAIM",
            Self::Zram                 => "ZRAM",
            Self::Cache                => "CACHE",
            Self::Calibrate            => "CALIBRATE",
            Self::SpikeFreeze          => "SPIKE_FREEZE",
            Self::SpikeUnfreeze        => "SPIKE_UNFREEZE",
            Self::SpikeUnfreezeTimeout => "SPIKE_UNFREEZE_TIMEOUT",
            Self::SpikeUnfreezeOrphan  => "SPIKE_UNFREEZE_ORPHAN",
            Self::Cycle                => "CYCLE",
        }
    }
}

/// Session logger: one file per daemon run, time-stamped, with rotation.
pub struct Logger {
    log_path: PathBuf,
}

impl Logger {
    /// Create a new logger in `~/memlogs/`. Rotates old sessions keeping at most
    /// `log_keep` files (pass 0 to keep unlimited).
    pub fn new(log_keep: usize) -> Self {
        let log_dir = crate::util::home_dir().join("memlogs");
        let _ = create_dir_all(&log_dir);

        let log_path = log_dir.join(format!("mgd_{}.log", local_datetime_compact()));

        if log_keep > 0 {
            // saturating_sub(1): leave room for the file we are about to create
            rotate_logs(&log_dir, log_keep.saturating_sub(1));
        }

        Logger { log_path }
    }

    /// Logger that discards everything — for unit tests exercising code paths
    /// that take a `&Logger` without touching `~/memlogs`.
    pub fn null() -> Self {
        Logger { log_path: PathBuf::from("/dev/null") }
    }

    /// Append a structured action entry to the session log.
    pub fn log(&self, action: LogAction, pid: crate::types::Pid, name: &str, rss_mb: f64, result: &str) {
        self.write_line(&format!(
            "[{}] {} pid={} name={} rss={:.0}MB result={}",
            timestamp_now(), action.as_str(), pid, name, rss_mb, result,
        ));
    }

    /// Append a pressure snapshot entry to the session log.
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

// ── time helpers ─────────────────────────────────────────────────────────────

fn local_tm() -> libc::tm {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    tm
}

fn timestamp_now() -> String {
    let tm = local_tm();
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// `YYYY-MM-DD_HH-MM-SS` in local time — for the session log filename.
/// Zero-padded ISO order so plain string sort is chronological.
fn local_datetime_compact() -> String {
    let tm = local_tm();
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
        tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday,
        tm.tm_hour, tm.tm_min, tm.tm_sec,
    )
}

/// Keep only the `keep` most-recent `mgd_*.log` files; delete the rest.
fn rotate_logs(log_dir: &std::path::Path, keep: usize) {
    let mut log_files: Vec<(String, PathBuf)> = read_dir(log_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let s = e.file_name().to_string_lossy().into_owned();
            if s.starts_with("mgd_") && s.ends_with(".log") {
                Some((s, e.path()))
            } else {
                None
            }
        })
        .collect();

    // Newest first (lexicographic = chronological for the zero-padded stamp)
    log_files.sort_by(|(a, _), (b, _)| b.cmp(a));

    // Delete everything beyond `keep` (the oldest files)
    for (_, path) in log_files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(&path);
    }
}
