use mgd_common::types::Kb;

/// Tracks an exponential moving average of available RAM during Normal pressure.
/// Used by `RecoveryManager` to gate CRIU restores: restoring a large process
/// when RAM is already borderline would immediately re-trigger eviction.
pub struct HealthBaseline {
    avg_available_kb: f64,
    samples: u32,
}

impl HealthBaseline {
    pub fn new() -> Self {
        HealthBaseline { avg_available_kb: 0.0, samples: 0 }
    }

    /// Feed a calm-pressure sample into the EMA. Only called from
    /// `RecoveryManager` while the system is at Normal pressure.
    pub fn observe(&mut self, available: Kb) {
        const ALPHA: f64 = 0.05; // converges over ~60 samples (~3 min at 3s tick)
        if self.samples == 0 {
            self.avg_available_kb = available.0 as f64;
        } else {
            self.avg_available_kb = ALPHA * available.0 as f64
                + (1.0 - ALPHA) * self.avg_available_kb;
        }
        self.samples = self.samples.saturating_add(1);
    }


    /// Returns `true` when it is safe to restore `process_rss` worth of RAM
    /// without immediately falling back into pressure. Cold baseline (< 10
    /// samples) uses a conservative 10% of total RAM; warm baseline uses 85% of
    /// the EMA to allow normal fluctuation without blocking all restores.
    /// Intervention-tainted samples (evictor active) are excluded by the caller
    /// so calibration never builds on pressure it is treating.
    pub fn safe_to_restore(&self, available: Kb, total: Kb, process_rss: Kb) -> bool {
        let after = available.saturating_sub(process_rss);
        if self.samples < 10 {
            // Not enough data — use conservative 10% of total
            after.0 > total.0 * 10 / 100
        } else {
            after.0 as f64 > self.avg_available_kb * 0.85
        }
    }

    pub fn baseline_mb(&self) -> f64 {
        self.avg_available_kb / 1024.0
    }

    pub fn samples(&self) -> u32 {
        self.samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOTAL_KB: u64 = 16 * 1024 * 1024; // 16 GB

    #[test]
    fn cold_uses_10pct_of_total() {
        let b = HealthBaseline::new();
        let floor = TOTAL_KB * 10 / 100;
        assert!(b.safe_to_restore(Kb(floor + 1), Kb(TOTAL_KB), Kb(0)));
        assert!(!b.safe_to_restore(Kb(floor), Kb(TOTAL_KB), Kb(0)));
    }

    #[test]
    fn warm_uses_85pct_of_ema() {
        let mut b = HealthBaseline::new();
        let target_kb: u64 = 4 * 1024 * 1024; // 4 GB
        for _ in 0..20 { b.observe(Kb(target_kb)); }
        // EMA converges to target_kb; threshold = target * 0.85
        let threshold = (target_kb as f64 * 0.85) as u64;
        assert!(b.safe_to_restore(Kb(threshold + 1024), Kb(TOTAL_KB), Kb(0)));
        assert!(!b.safe_to_restore(Kb(threshold - 1024), Kb(TOTAL_KB), Kb(0)));
    }

    #[test]
    fn process_rss_subtracted_before_check() {
        let mut b = HealthBaseline::new();
        let target_kb: u64 = 4 * 1024 * 1024;
        for _ in 0..20 { b.observe(Kb(target_kb)); }
        let threshold = (target_kb as f64 * 0.85) as u64;
        let avail = threshold + 512 * 1024;
        // Without RSS: safe
        assert!(b.safe_to_restore(Kb(avail), Kb(TOTAL_KB), Kb(0)));
        // After subtracting 512 MB for the process: below threshold
        assert!(!b.safe_to_restore(Kb(avail), Kb(TOTAL_KB), Kb(512 * 1024)));
    }

    #[test]
    fn ema_first_sample_sets_directly() {
        let mut b = HealthBaseline::new();
        b.observe(Kb(8_000_000));
        assert_eq!(b.samples(), 1);
        assert!((b.baseline_mb() - 8_000_000.0 / 1024.0).abs() < 1.0);
    }

    #[test]
    fn ema_converges_toward_new_value() {
        let mut b = HealthBaseline::new();
        // Start at 8 GB
        for _ in 0..20 { b.observe(Kb(8 * 1024 * 1024)); }
        let high = b.baseline_mb();
        // Now feed 2 GB — EMA should drift down
        for _ in 0..60 { b.observe(Kb(2 * 1024 * 1024)); }
        assert!(b.baseline_mb() < high);
        // After 60 samples at alpha=0.05, should be reasonably close to 2 GB
        assert!(b.baseline_mb() < 4096.0);
    }

    #[test]
    fn cold_boundary_exactly_10pct_is_unsafe() {
        let b = HealthBaseline::new();
        assert!(!b.safe_to_restore(Kb(TOTAL_KB * 10 / 100), Kb(TOTAL_KB), Kb(0)));
    }
}
