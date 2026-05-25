use std::fs;
use std::num::ParseFloatError;

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

#[derive(Debug, PartialEq)]
pub enum PressureLevel {
    Normal,
    Elevated,
    High,
    Critical,
    Emergency,
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

#[derive(Debug)]
pub enum PsiError {
    Io(std::io::Error),
    Parse(String),
}

impl std::fmt::Display for PsiError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            PsiError::Io(e) => write!(f, "IO error: {e}"),
            PsiError::Parse(s) => write!(f, "Parse error: {s}"),
        }
    }
}

impl From<std::io::Error> for PsiError {
    fn from(e: std::io::Error) -> Self {
        PsiError::Io(e)
    }
}

impl From<ParseFloatError> for PsiError {
    fn from(e: ParseFloatError) -> Self {
        PsiError::Parse(e.to_string())
    }
}

/// Reads and parses /proc/pressure/memory
pub fn read_pressure() -> Result<MemoryPressure, PsiError> {
    let content = fs::read_to_string("/proc/pressure/memory")?;
    parse_pressure(&content)
}

/// Parses PSI format:
/// some avg10=0.00 avg60=0.00 avg300=0.00 total=133037586
/// full avg10=0.00 avg60=0.00 avg300=0.00 total=124524209
fn parse_pressure(content: &str) -> Result<MemoryPressure, PsiError> {
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

fn parse_kv(s: &str, prefix: &str) -> Result<f64, PsiError> {
    s.strip_prefix(prefix)
        .ok_or_else(|| PsiError::Parse(format!("expected '{prefix}', got '{s}'")))?
        .parse::<f64>()
        .map_err(PsiError::from)
}

fn parse_kv_u64(s: &str, prefix: &str) -> Result<u64, PsiError> {
    s.strip_prefix(prefix)
        .ok_or_else(|| PsiError::Parse(format!("expected '{prefix}', got '{s}'")))?
        .parse::<u64>()
        .map_err(|e| PsiError::Parse(e.to_string()))
}

/// Maps pressure values to action levels (from design spec thresholds)
pub fn pressure_level(p: &MemoryPressure) -> PressureLevel {
    match p.some_avg10 {
        x if x >= 70.0 => PressureLevel::Emergency,
        x if x >= 50.0 => PressureLevel::Critical,
        x if x >= 25.0 => PressureLevel::High,
        x if x >= 0.01 => PressureLevel::Elevated,
        _               => PressureLevel::Normal,
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
    fn test_parse_high_pressure() {
        let input = "some avg10=35.00 avg60=20.00 avg300=10.00 total=999\n\
                     full avg10=15.00 avg60=8.00 avg300=4.00 total=888\n";
        let p = parse_pressure(input).unwrap();
        assert_eq!(pressure_level(&p), PressureLevel::High);
    }

    #[test]
    fn test_pressure_levels() {
        let mut p = parse_pressure(
            "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n\
             full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"
        ).unwrap();

        p.some_avg10 = 0.005;
        assert_eq!(pressure_level(&p), PressureLevel::Normal);
        p.some_avg10 = 5.0;
        assert_eq!(pressure_level(&p), PressureLevel::Elevated);
        p.some_avg10 = 30.0;
        assert_eq!(pressure_level(&p), PressureLevel::High);
        p.some_avg10 = 60.0;
        assert_eq!(pressure_level(&p), PressureLevel::Critical);
        p.some_avg10 = 75.0;
        assert_eq!(pressure_level(&p), PressureLevel::Emergency);
    }
}
