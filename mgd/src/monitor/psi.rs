use std::fs;
use std::os::fd::AsRawFd;
use std::sync::LazyLock;

use mgd_common::error::MgdError;
pub use mgd_common::psi::GLOBAL_PSI;

/// PSI source used by `read_pressure()`, resolved once at first use.
/// Prefers the systemd user-manager cgroup (`user@<uid>.service`) so levels
/// reflect only this session's pressure, not system-wide noise. Falls back to
/// the global /proc file on cgroup-v1 hosts or kernels without per-cgroup PSI.
/// Resolution logic lives in `mgd_common::psi` so `mgctl doctor` reports the
/// same source the daemon uses.
static PRESSURE_PATH: LazyLock<String> =
    LazyLock::new(mgd_common::psi::resolve_pressure_source);

/// The PSI file `read_pressure()` reads from (cgroup or global).
pub fn pressure_source() -> &'static str {
    &PRESSURE_PATH
}

/// Kernel PSI trigger for zero-CPU idle waiting.
pub struct PsiTrigger {
    file: std::fs::File,
    /// Which PSI file the trigger is armed on (cgroup or global).
    pub source: &'static str,
}

impl PsiTrigger {
    /// Creates a trigger that wakes up the thread when memory pressure hits
    /// the configured `[psi]` elevated_pct over a 1-second window.
    /// PSI format: `<some|full> <stall_us> <window_us>`
    /// Armed once at startup — changing elevated_pct re-arms on daemon
    /// restart, not on SIGHUP reload.
    ///
    /// Tries the per-session cgroup file first; falls back to the global
    /// /proc file when the cgroup file is root-owned (systemd doesn't chown
    /// the user@<uid>.service node itself).
    pub fn new() -> Result<Self, MgdError> {
        match Self::open(pressure_source()) {
            Err(_) if pressure_source() != GLOBAL_PSI => Self::open(GLOBAL_PSI),
            other => other,
        }
    }

    fn open(path: &'static str) -> Result<Self, MgdError> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;

        // elevated_pct of the 1,000,000 us window (default 5% → 50,000 us)
        let stall_us = (crate::config::get().psi.elevated_pct / 100.0 * 1_000_000.0) as u64;
        file.write_all(format!("some {stall_us} 1000000").as_bytes())?;

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

fn parse_kv(s: &str, prefix: &str) -> Result<f64, MgdError> {
    s.strip_prefix(prefix)
        .ok_or_else(|| MgdError::Parse(format!("expected '{prefix}', got '{s}'")))?
        .parse::<f64>()
        .map_err(MgdError::from)
}

fn parse_kv_u64(s: &str, prefix: &str) -> Result<u64, MgdError> {
    s.strip_prefix(prefix)
        .ok_or_else(|| MgdError::Parse(format!("expected '{prefix}', got '{s}'")))?
        .parse::<u64>()
        .map_err(|e| MgdError::Parse(e.to_string()))
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

/// Maps pressure values to action levels using the loaded `[psi]` config.
pub fn pressure_level(p: &MemoryPressure) -> PressureLevel {
    pressure_level_with(p, &crate::config::get().psi)
}

/// Pure mapping — config-free, used directly by tests.
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
        let mut t = PsiThresholds::default();
        t.high_pct = t.elevated_pct;
        assert!(!t.valid());

        let mut t = PsiThresholds::default();
        t.emergency_pct = 101.0;
        assert!(!t.valid());

        let mut t = PsiThresholds::default();
        t.full_critical_pct = 0.0;
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
