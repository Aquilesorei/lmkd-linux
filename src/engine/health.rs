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
