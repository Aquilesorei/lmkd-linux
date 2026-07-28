use super::{compact_zram, cache_drop, idle_reclaim, early_reclaim};

/// Feature gate state for `mgctl status` — read-only accessors over the
/// evictor's private statics so the IPC thread can report per-feature state.
pub(crate) struct FeatureGates {
    pub zram_compact_disabled: bool,
    pub last_zram_compact: u64,
    pub last_cache_drop: u64,
    pub last_early_reclaim: u64,
    pub last_idle_reclaim: u64,
}

pub(crate) fn feature_gates() -> FeatureGates {
    let (zram_compact_disabled, last_zram_compact) = compact_zram::gate_state();
    FeatureGates {
        zram_compact_disabled,
        last_zram_compact,
        last_cache_drop: cache_drop::gate_state(),
        last_early_reclaim: early_reclaim::gate_state(),
        last_idle_reclaim: idle_reclaim::gate_state(),
    }
}
