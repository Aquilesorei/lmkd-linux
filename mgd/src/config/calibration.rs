use super::CompiledConfig;

#[derive(serde::Deserialize, Default)]
struct CalibrationSuggestion {
    #[serde(default)]
    psi: CalibrationPsi,
}

#[derive(serde::Deserialize, Default)]
struct CalibrationPsi {
    elevated_pct: Option<f64>,
    full_critical_pct: Option<f64>,
}

pub(super) fn apply_calibration_overlay(cfg: &mut CompiledConfig) {
    if cfg!(test) {
        return;
    }
    let path = mgd_common::util::home_dir()
        .join(".local/share/mgd/calibration_suggestion.toml");
    let Ok(content) = std::fs::read_to_string(&path) else { return };
    let Ok(suggestion) = toml::from_str::<CalibrationSuggestion>(&content) else { return };
    let mut applied = false;
    if let Some(v) = suggestion.psi.elevated_pct {
        cfg.psi.elevated_pct = v;
        applied = true;
    }
    if let Some(v) = suggestion.psi.full_critical_pct {
        cfg.psi.full_critical_pct = v;
        applied = true;
    }
    if applied {
        eprintln!(
            "[config] Calibration overlay applied: elevated_pct={:.1} full_critical_pct={:.1}",
            cfg.psi.elevated_pct, cfg.psi.full_critical_pct,
        );
    }
}

// ── RAM-scaling helpers ───────────────────────────────────────────────────────

/// Returns the appropriate free-RAM target percentage for this machine's total RAM.
/// Larger machines need less proportional headroom; smaller machines need more.
///
/// Scaling table:
///   < 8 GB   → 20%   (tight machines — compositor takes a big share)
///   8–16 GB  → 15%   (typical laptop — original conservative default)
///   16–32 GB → 12%   (workstation — comfortable headroom without waste)
///   > 32 GB  → 10%   (server/high-RAM — proportional guard is still ample)
pub(super) fn ram_scaled_target_pct() -> f64 {
    let total_kb = crate::monitor::meminfo::read_meminfo().total_kb;
    let total_gb = total_kb.0 as f64 / (1024.0 * 1024.0);
    if      total_gb < 8.0  { 20.0 }
    else if total_gb < 16.0 { 15.0 }
    else if total_gb < 32.0 { 12.0 }
    else                    { 10.0 }
}

/// Try to load target_available_pct from `mgctl calibrate` output.
/// Returns None if no calibration file exists or it cannot be parsed.
pub(super) fn load_calibrated_target_pct() -> Option<f64> {
    if cfg!(test) {
        return None;
    }
    let path = mgd_common::util::home_dir()
        .join(".config/mgd/calibration.toml");
    let content = std::fs::read_to_string(&path).ok()?;
    parse_calibrated_target_pct(&content)
}

fn parse_calibrated_target_pct(content: &str) -> Option<f64> {
    for line in content.lines() {
        if let Some(rest) = line.trim().strip_prefix("target_available_pct")
            && let Some(val) = rest.split('=').nth(1) {
                let num: String = val.trim().chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                if let Ok(pct) = num.trim().parse::<f64>() {
                    return Some(pct.clamp(5.0, 50.0));
                }
            }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_calibrated_target_pct() {
        let content = "\
[thresholds]
target_available_pct = 35      # swap onset was at 6000MB
psi_recovery_secs    = 5
";
        assert_eq!(parse_calibrated_target_pct(content), Some(35.0));

        let content_no_space = "target_available_pct=22.5";
        assert_eq!(parse_calibrated_target_pct(content_no_space), Some(22.5));

        let content_invalid = "target_available_pct = invalid";
        assert_eq!(parse_calibrated_target_pct(content_invalid), None);
    }
}
