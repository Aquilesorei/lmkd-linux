/// Tracks a rolling baseline of available RAM during healthy (Normal pressure) periods.
/// Used to decide whether it's safe to restore a checkpointed process.
pub struct HealthBaseline {
    avg_available_kb: f64,
    samples: u32,
}

impl HealthBaseline {
    pub fn new() -> Self {
        HealthBaseline { avg_available_kb: 0.0, samples: 0 }
    }

    /// Call every Normal-pressure cycle to update the learned baseline.
    pub fn observe(&mut self, available_kb: u64) {
        const ALPHA: f64 = 0.05; // converges over ~60 samples (~3 min at 3s tick)
        if self.samples == 0 {
            self.avg_available_kb = available_kb as f64;
        } else {
            self.avg_available_kb = ALPHA * available_kb as f64
                + (1.0 - ALPHA) * self.avg_available_kb;
        }
        self.samples = self.samples.saturating_add(1);
    }

    /// Is it safe to restore a process that will consume `process_rss_kb`?
    /// Requires available RAM after restore to remain above 85% of learned baseline.
    /// Falls back to a conservative 30% of total RAM if not enough samples yet.
    pub fn safe_to_restore(&self, available_kb: u64, total_kb: u64, process_rss_kb: u64) -> bool {
        let after_kb = available_kb.saturating_sub(process_rss_kb);
        if self.samples < 10 {
            // Not enough data — use conservative 10% of total
            after_kb > total_kb * 10 / 100
        } else {
            after_kb as f64 > self.avg_available_kb * 0.85
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
        assert!(b.safe_to_restore(floor + 1, TOTAL_KB, 0));
        assert!(!b.safe_to_restore(floor, TOTAL_KB, 0));
    }

    #[test]
    fn warm_uses_85pct_of_ema() {
        let mut b = HealthBaseline::new();
        let target_kb: u64 = 4 * 1024 * 1024; // 4 GB
        for _ in 0..20 { b.observe(target_kb); }
        // EMA converges to target_kb; threshold = target * 0.85
        let threshold = (target_kb as f64 * 0.85) as u64;
        assert!(b.safe_to_restore(threshold + 1024, TOTAL_KB, 0));
        assert!(!b.safe_to_restore(threshold - 1024, TOTAL_KB, 0));
    }

    #[test]
    fn process_rss_subtracted_before_check() {
        let mut b = HealthBaseline::new();
        let target_kb: u64 = 4 * 1024 * 1024;
        for _ in 0..20 { b.observe(target_kb); }
        let threshold = (target_kb as f64 * 0.85) as u64;
        let avail = threshold + 512 * 1024;
        // Without RSS: safe
        assert!(b.safe_to_restore(avail, TOTAL_KB, 0));
        // After subtracting 512 MB for the process: below threshold
        assert!(!b.safe_to_restore(avail, TOTAL_KB, 512 * 1024));
    }

    #[test]
    fn ema_first_sample_sets_directly() {
        let mut b = HealthBaseline::new();
        b.observe(8_000_000);
        assert_eq!(b.samples(), 1);
        assert!((b.baseline_mb() - 8_000_000.0 / 1024.0).abs() < 1.0);
    }

    #[test]
    fn ema_converges_toward_new_value() {
        let mut b = HealthBaseline::new();
        // Start at 8 GB
        for _ in 0..20 { b.observe(8 * 1024 * 1024); }
        let high = b.baseline_mb();
        // Now feed 2 GB — EMA should drift down
        for _ in 0..60 { b.observe(2 * 1024 * 1024); }
        assert!(b.baseline_mb() < high);
        // After 60 samples at alpha=0.05, should be reasonably close to 2 GB
        assert!(b.baseline_mb() < 4096.0);
    }

    #[test]
    fn cold_boundary_exactly_10pct_is_unsafe() {
        let b = HealthBaseline::new();
        assert!(!b.safe_to_restore(TOTAL_KB * 10 / 100, TOTAL_KB, 0));
    }
}
