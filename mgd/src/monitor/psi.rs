use std::fs;
use std::os::fd::AsRawFd;
use std::sync::LazyLock;

use mgd_common::error::MgdError;
use mgd_common::psi::{parse_kv, parse_kv_u64};
pub use mgd_common::psi::GLOBAL_PSI;


static PRESSURE_PATH: LazyLock<String> =
    LazyLock::new(mgd_common::psi::resolve_pressure_source);

/// The PSI file `read_pressure()` reads from (cgroup or global).
pub fn pressure_source() -> &'static str {
    &PRESSURE_PATH
}

/// Kernel PSI trigger for zero-CPU idle waiting.
pub struct PsiTrigger {
    file: std::fs::File,
    pub source: String,
}

impl PsiTrigger {

    pub fn new(elevated_pct: f64) -> Result<Self, MgdError> {
        // Kernel 7.x+: /proc/pressure/memory triggers return EINVAL; min window is 2s.
        // Walk the cgroup hierarchy upward to find the highest writable PSI file.
        if let Some(path) = mgd_common::psi::find_trigger_path() {
            return Self::open(path, elevated_pct);
        }
        // Last resort: global PSI file (works on older kernels).
        Self::open(GLOBAL_PSI.to_string(), elevated_pct)
    }

    fn open(path: String, elevated_pct: f64) -> Result<Self, MgdError> {
        use std::io::Write;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)?;

        // 2s window: valid on kernel <7.x ([500ms,10s]) and 7.x+ (min 2s, must be multiple of 2s).
        let window_us: u64 = 2_000_000;
        let stall_us = (elevated_pct / 100.0 * window_us as f64) as u64;
        (&file).write_all(format!("some {stall_us} {window_us}").as_bytes())?;

        Ok(PsiTrigger { file, source: path })
    }

    /// Block until the kernel signals memory pressure, or `timeout_ms` elapses.
    /// Returns `true` if woken by a pressure event, `false` on timeout.
    pub fn wait(&self, timeout_ms: i32) -> bool {
        let mut pfd = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLPRI,
            revents: 0,
        };
        
        unsafe {
            // poll returns > 0 on event, 0 on timeout, < 0 on error
            libc::poll(&mut pfd, 1, timeout_ms) > 0
        }
    }
}

// ── Subprocess-based PSI trigger (requires cap_perfmon on mgd-psi-trigger) ────

/// Spawns `mgd-psi-trigger <stall_us>` as a capped subprocess.
///
/// The helper opens `/proc/pressure/memory`, arms the trigger (needs
/// `cap_perfmon+ep`), and writes a single byte to stdout for each event.
/// The daemon polls the pipe with `POLLIN` — no privileged fd in the daemon.
/// On drop, the subprocess is killed and the kernel fd is released.
pub struct PsiSubprocess {
    child: std::process::Child,
    stdout_fd: i32,
    #[allow(dead_code)]
    stdout_owned: std::process::ChildStdout, // kept alive to prevent fd close
}

pub enum WaitResult {
    Event,
    Timeout,
    HelperDied,
}

fn psi_helper_candidates() -> Vec<std::path::PathBuf> {
    let mut v = vec![
        std::path::PathBuf::from("/usr/local/bin/mgd-psi-trigger"),
        std::path::PathBuf::from("/usr/bin/mgd-psi-trigger"),
    ];
    // Default install path (./install.sh without --privileged copies here)
    v.insert(0, mgd_common::util::home_dir().join(".local/bin/mgd-psi-trigger"));
    v
}

impl PsiSubprocess {
    pub fn new(elevated_pct: f64) -> Option<Self> {
        let stall_us = (elevated_pct / 100.0 * 1_000_000.0) as u64;

        let helper = psi_helper_candidates()
            .into_iter()
            .find(|p| p.exists())?;

        let mut child = std::process::Command::new(&helper)
            .arg(stall_us.to_string())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;

        let stdout = child.stdout.take()?;

        // 50ms: enough for a cold exec; longer delays mean cap_perfmon is absent.
        std::thread::sleep(std::time::Duration::from_millis(50));
        if let Ok(Some(status)) = child.try_wait() {
            let _ = child.wait();
            mgd_common::sync_print!(
                "[psi] mgd-psi-trigger exited immediately (status {:?}) — cap_perfmon absent or arm failed",
                status.code()
            );
            return None;
        }

        use std::os::unix::io::AsRawFd;
        let stdout_fd = stdout.as_raw_fd();

        Some(PsiSubprocess { child, stdout_fd, stdout_owned: stdout })
    }

    /// Block until the helper signals a pressure event, or `timeout_ms` elapses.
    pub fn wait(&self, timeout_ms: i32) -> WaitResult {
        let mut pfd = libc::pollfd {
            fd: self.stdout_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe {
            let r = libc::poll(&mut pfd, 1, timeout_ms);
            if r > 0 {
                if (pfd.revents & libc::POLLIN) != 0 {
                    let mut buf = [0u8; 1];
                    let n = libc::read(
                        self.stdout_fd,
                        buf.as_mut_ptr() as *mut libc::c_void,
                        1,
                    );
                    if n > 0 { WaitResult::Event } else { WaitResult::HelperDied }
                } else {
                    WaitResult::HelperDied // POLLHUP / POLLERR
                }
            } else {
                WaitResult::Timeout // timeout or EINTR
            }
        }
    }
}

impl Drop for PsiSubprocess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}


#[derive(Debug)]
#[allow(dead_code)]
pub struct MemoryPressure {
    pub some_avg10: f64,
    pub some_avg60: f64,
    pub some_avg300: f64,
    pub some_total: u64,
    pub full_avg10: f64,
    pub full_avg60: f64,
    pub full_avg300: f64,
    pub full_total: u64,
}

/// Variants are declared in ascending severity order, so the derived
/// `PartialOrd`/`Ord` lets callers gate on `level >= PressureLevel::Elevated`.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub enum PressureLevel {
    Normal,
    Elevated,
    High,
    Critical,
    Emergency,
}

impl PressureLevel {
    /// Parse a config trigger level (case-insensitive). Returns None for an
    /// unrecognised string so the caller can fall back to a default + warn.
    pub fn parse(s: &str) -> Option<PressureLevel> {
        match s.trim().to_ascii_lowercase().as_str() {
            "normal" => Some(PressureLevel::Normal),
            "elevated" => Some(PressureLevel::Elevated),
            "high" => Some(PressureLevel::High),
            "critical" => Some(PressureLevel::Critical),
            "emergency" => Some(PressureLevel::Emergency),
            _ => None,
        }
    }
}

impl std::fmt::Display for PressureLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            PressureLevel::Normal    => write!(f, "NORMAL   "),
            PressureLevel::Elevated  => write!(f, "ELEVATED "),
            PressureLevel::High      => write!(f, "HIGH     "),
            PressureLevel::Critical  => write!(f, "CRITICAL "),
            PressureLevel::Emergency => write!(f, "EMERGENCY"),
        }
    }
}

/// Reads and parses the resolved PSI source (per-session cgroup or global).
pub fn read_pressure() -> Result<MemoryPressure, MgdError> {
    let content = fs::read_to_string(pressure_source())?;
    parse_pressure(&content)
}

/// Parses PSI format:
/// some avg10=0.00 avg60=0.00 avg300=0.00 total=133037586
/// full avg10=0.00 avg60=0.00 avg300=0.00 total=124524209
fn parse_pressure(content: &str) -> Result<MemoryPressure, MgdError> {
    let mut some_avg10 = 0.0;
    let mut some_avg60 = 0.0;
    let mut some_avg300 = 0.0;
    let mut some_total = 0u64;
    let mut full_avg10 = 0.0;
    let mut full_avg60 = 0.0;
    let mut full_avg300 = 0.0;
    let mut full_total = 0u64;

    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 5 {
            continue;
        }

        let kind = parts[0];
        let avg10  = parse_kv(parts[1], "avg10=")?;
        let avg60  = parse_kv(parts[2], "avg60=")?;
        let avg300 = parse_kv(parts[3], "avg300=")?;
        let total  = parse_kv_u64(parts[4], "total=")?;

        match kind {
            "some" => {
                some_avg10 = avg10;
                some_avg60 = avg60;
                some_avg300 = avg300;
                some_total = total;
            }
            "full" => {
                full_avg10 = avg10;
                full_avg60 = avg60;
                full_avg300 = avg300;
                full_total = total;
            }
            _ => {}
        }
    }

    Ok(MemoryPressure {
        some_avg10, some_avg60, some_avg300, some_total,
        full_avg10, full_avg60, full_avg300, full_total,
    })
}

/// Tier boundaries mapping `some_avg10` to `PressureLevel`, plus the
/// `full_avg10` accelerator floor. Overridable via the `[psi]` config block;
/// defaults are the long-standing built-in values.
#[derive(Clone, Debug, PartialEq)]
pub struct PsiThresholds {
    pub elevated_pct: f64,
    pub high_pct: f64,
    pub critical_pct: f64,
    pub emergency_pct: f64,
    /// full_avg10 at/above this forces a Critical floor (ALL tasks stalled).
    pub full_critical_pct: f64,
}

impl Default for PsiThresholds {
    fn default() -> Self {
        PsiThresholds {
            elevated_pct: 5.0,
            high_pct: 25.0,
            critical_pct: 50.0,
            emergency_pct: 70.0,
            full_critical_pct: 20.0,
        }
    }
}

impl PsiThresholds {
    /// Tiers must be strictly increasing and within (0, 100] for the level
    /// mapping to make sense; the full accelerator only needs to be positive.
    pub fn valid(&self) -> bool {
        0.0 < self.elevated_pct
            && self.elevated_pct < self.high_pct
            && self.high_pct < self.critical_pct
            && self.critical_pct < self.emergency_pct
            && self.emergency_pct <= 100.0
            && self.full_critical_pct > 0.0
    }
}

/// Maps pressure values to action levels. Pure — callers pass the `[psi]`
/// thresholds from their cycle-scoped config borrow.
/// Uses full_avg10 as an accelerator — complete stalls indicate worse conditions.
pub fn pressure_level_with(p: &MemoryPressure, t: &PsiThresholds) -> PressureLevel {
    // full_avg10 over the floor means ALL tasks are stalled — Critical minimum
    if p.full_avg10 >= t.full_critical_pct {
        return match p.some_avg10 {
            x if x >= t.emergency_pct => PressureLevel::Emergency,
            _ => PressureLevel::Critical,
        };
    }

    match p.some_avg10 {
        x if x >= t.emergency_pct => PressureLevel::Emergency,
        x if x >= t.critical_pct  => PressureLevel::Critical,
        x if x >= t.high_pct      => PressureLevel::High,
        x if x >= t.elevated_pct  => PressureLevel::Elevated,
        _                          => PressureLevel::Normal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_normal() {
        let input = "some avg10=0.00 avg60=0.00 avg300=0.05 total=133033584\n\
                     full avg10=0.00 avg60=0.00 avg300=0.04 total=124520674\n";
        let p = parse_pressure(input).unwrap();
        assert_eq!(p.some_avg10, 0.0);
        assert_eq!(p.some_total, 133033584);
        assert_eq!(p.full_avg300, 0.04);
    }

    #[test]
    fn test_parse_pressure_level_str() {
        assert_eq!(PressureLevel::parse("High"), Some(PressureLevel::High));
        assert_eq!(PressureLevel::parse("  elevated "), Some(PressureLevel::Elevated));
        assert_eq!(PressureLevel::parse("EMERGENCY"), Some(PressureLevel::Emergency));
        assert_eq!(PressureLevel::parse("nonsense"), None);
    }

    #[test]
    fn test_parse_high_pressure() {
        let input = "some avg10=35.00 avg60=20.00 avg300=10.00 total=999\n\
                     full avg10=15.00 avg60=8.00 avg300=4.00 total=888\n";
        let p = parse_pressure(input).unwrap();
        assert_eq!(pressure_level_with(&p, &PsiThresholds::default()), PressureLevel::High);
    }

    #[test]
    fn test_pressure_levels() {
        let t = PsiThresholds::default();
        let mut p = parse_pressure(
            "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n\
             full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"
        ).unwrap();

        p.some_avg10 = 4.0;
        assert_eq!(pressure_level_with(&p, &t), PressureLevel::Normal);
        p.some_avg10 = 5.0;
        assert_eq!(pressure_level_with(&p, &t), PressureLevel::Elevated);
        p.some_avg10 = 30.0;
        assert_eq!(pressure_level_with(&p, &t), PressureLevel::High);
        p.some_avg10 = 60.0;
        assert_eq!(pressure_level_with(&p, &t), PressureLevel::Critical);
        p.some_avg10 = 75.0;
        assert_eq!(pressure_level_with(&p, &t), PressureLevel::Emergency);
    }

    #[test]
    fn test_custom_thresholds_shift_tiers() {
        let t = PsiThresholds {
            elevated_pct: 10.0,
            high_pct: 35.0,
            critical_pct: 60.0,
            emergency_pct: 80.0,
            full_critical_pct: 30.0,
        };
        let mut p = parse_pressure(
            "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n\
             full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"
        ).unwrap();

        // 5% is Elevated on defaults but Normal with elevated_pct = 10.
        p.some_avg10 = 5.0;
        assert_eq!(pressure_level_with(&p, &t), PressureLevel::Normal);
        p.some_avg10 = 10.0;
        assert_eq!(pressure_level_with(&p, &t), PressureLevel::Elevated);

        // full accelerator follows its raised floor too.
        p.full_avg10 = 25.0; // >= 20 default, < 30 custom
        assert_eq!(pressure_level_with(&p, &t), PressureLevel::Elevated);
        p.full_avg10 = 30.0;
        assert_eq!(pressure_level_with(&p, &t), PressureLevel::Critical);
    }

    #[test]
    fn test_thresholds_validity() {
        assert!(PsiThresholds::default().valid());

        // Non-increasing tiers are invalid.
        let t = PsiThresholds {
            high_pct: PsiThresholds::default().elevated_pct,
            ..Default::default()
        };
        assert!(!t.valid());

        let t = PsiThresholds {
            emergency_pct: 101.0,
            ..Default::default()
        };
        assert!(!t.valid());

        let t = PsiThresholds {
            full_critical_pct: 0.0,
            ..Default::default()
        };
        assert!(!t.valid());
    }

    // test_usable_psi_file lives in mgd_common::psi where the probe moved.

    #[test]
    fn test_full_avg10_accelerates_to_critical() {
        let t = PsiThresholds::default();
        let mut p = parse_pressure(
            "some avg10=10.00 avg60=0.00 avg300=0.00 total=0\n\
             full avg10=25.00 avg60=0.00 avg300=0.00 total=0\n"
        ).unwrap();
        // some_avg10=10 would normally be Elevated, but full_avg10=25 pushes to Critical
        assert_eq!(pressure_level_with(&p, &t), PressureLevel::Critical);

        // Emergency still wins when some is high enough
        p.some_avg10 = 75.0;
        assert_eq!(pressure_level_with(&p, &t), PressureLevel::Emergency);
    }
}
