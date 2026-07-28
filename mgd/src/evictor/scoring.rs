

use mgd_common::types::Kb;
use crate::monitor::meminfo::MemInfo;
use crate::monitor::psi::PressureLevel;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ControlState {
    Calm,
    Warning,
    Evicting,
    Critical,
    Emergency,
}

impl ControlState {
    pub(crate) fn to_pressure_level(self) -> PressureLevel {
        match self {
            ControlState::Calm => PressureLevel::Normal,
            ControlState::Warning => PressureLevel::Elevated,
            ControlState::Evicting => PressureLevel::High,
            ControlState::Critical => PressureLevel::Critical,
            ControlState::Emergency => PressureLevel::Emergency,
        }
    }
}

/// Below this raw swap I/O rate (KB/s) there's no meaningful swap churn happening.
const SWAP_IO_LIVE_FLOOR_KBS: f64 = 1000.0;
/// Below this raw PSI (`some_avg10`, percent) there's no active stall happening.
const PSI_LIVE_FLOOR: f64 = 0.5;

pub(crate) fn target_state_for(p_score: f64, trend: f64, psi_some_avg10: f64, swap_io_kbs: f64) -> ControlState {
    if p_score >= 0.70 || (p_score >= 0.55 && trend > 0.05) {
        ControlState::Emergency
    } else if p_score >= 0.50 || (p_score >= 0.35 && trend > 0.03) {
        ControlState::Critical
    } else if p_score >= 0.30 || (p_score >= 0.20 && trend > 0.02) {
        ControlState::Evicting
    } else if p_score >= 0.15 {
        if psi_some_avg10 > PSI_LIVE_FLOOR || swap_io_kbs > SWAP_IO_LIVE_FLOOR_KBS {
            ControlState::Warning
        } else {
            ControlState::Calm
        }
    } else {
        ControlState::Calm
    }
}


pub(crate) struct StateMachine {
    pub(crate) current: ControlState,
    pending: ControlState,
    pending_ticks: usize,
}

impl StateMachine {
    pub(crate) fn new() -> Self {
        Self { current: ControlState::Calm, pending: ControlState::Calm, pending_ticks: 0 }
    }

    /// Advance one tick toward `target`. Pure — no I/O, no logging.
    /// Returns true when an instant escalation fired (caller logs it).
    pub(crate) fn advance(&mut self, target: ControlState, trend: f64) -> bool {
        if target > self.current {
            // Escalation: needs 2 ticks of persistence, unless it's a massive spike (instant)
            let instant_escalate = (target == ControlState::Emergency || target == ControlState::Critical) && trend > 0.08;
            if instant_escalate {
                self.current = target;
                self.pending = target;
                self.pending_ticks = 0;
                return true;
            }
            if target == self.pending {
                self.pending_ticks += 1;
                if self.pending_ticks >= 2 {
                    self.current = target;
                    self.pending_ticks = 0;
                }
            } else {
                self.pending = target;
                self.pending_ticks = 1;
            }
        } else if target < self.current {
            // Recovery: needs longer persistence
            let required_ticks = match target {
                ControlState::Calm => 12,    // 1 minute of Calm at 5s polling
                ControlState::Warning => 6, // 30s
                _ => 4,                     // 20s
            };
            if target == self.pending {
                self.pending_ticks += 1;
                if self.pending_ticks >= required_ticks {
                    self.current = target;
                    self.pending_ticks = 0;
                }
            } else {
                self.pending = target;
                self.pending_ticks = 1;
            }
        } else {
            // Target matches current state: reset pending state
            self.pending = self.current;
            self.pending_ticks = 0;
        }
        false
    }
}

/// Rolling inputs for the composite pressure score: previous score/time for
/// the trend derivative, previous vmstat counters for the swap I/O rate.
pub(crate) struct ScoreTracker {
    last_score: f64,
    last_time: std::time::Instant,
    last_pswpin: u64,
    last_pswpout: u64,
}

/// One cycle's composite score plus the inputs kept for attribution logging.
pub(crate) struct CycleScore {
    pub(crate) p_score: f64,
    pub(crate) trend: f64,
    pub(crate) swap_io_kbs: f64,
    pub(crate) gpu_val: f64,
}

impl ScoreTracker {
    pub(crate) fn new() -> Self {
        let (last_pswpin, last_pswpout) = crate::monitor::meminfo::read_vmstat_swap_counters();
        Self { last_score: 0.0, last_time: std::time::Instant::now(), last_pswpin, last_pswpout }
    }

    /// Read the per-cycle inputs (vmstat counters, GPU cache) and fold them in.
    pub(crate) fn update(&mut self, some_avg10: f64, meminfo: &MemInfo, now: std::time::Instant) -> CycleScore {
        let (pswpin, pswpout) = crate::monitor::meminfo::read_vmstat_swap_counters();
        let gpu_kb = crate::plugin_server::get_total_gpu_kb();
        self.update_with(some_avg10, meminfo, pswpin, pswpout, gpu_kb, now)
    }

    /// Pure: composite score = 55% PSI + 20% swap used + 15% GPU UMA + 10% swap I/O
    /// rate, plus trend (dP/dt). Swap I/O is pswpin+pswpout delta over the cycle,
    /// pages → KB/s, normalized at 50 MB/s (51200 KB/s) total — above that = thrash.
    pub(crate) fn update_with(
        &mut self,
        some_avg10: f64,
        meminfo: &MemInfo,
        cur_pswpin: u64,
        cur_pswpout: u64,
        total_gpu: Kb,
        now: std::time::Instant,
    ) -> CycleScore {
        let dt = now.duration_since(self.last_time).as_secs_f64();

        let swap_io_pages = cur_pswpin.saturating_sub(self.last_pswpin)
            .saturating_add(cur_pswpout.saturating_sub(self.last_pswpout));
        self.last_pswpin = cur_pswpin;
        self.last_pswpout = cur_pswpout;
        let swap_io_kbs = if dt > 0.1 { (swap_io_pages * 4) as f64 / dt } else { 0.0 }; // 4 KB/page — x86_64
        let swap_io_val = (swap_io_kbs / 51200.0).clamp(0.0, 1.0);

        let psi_val = (some_avg10 / 100.0).clamp(0.0, 1.0);
        let swap_val = if meminfo.swap_total_kb.0 > 0 {
            (meminfo.swap_used_pct() / 100.0).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let gpu_val = if meminfo.total_kb.0 > 0 {
            (total_gpu.0 as f64 / meminfo.total_kb.0 as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let p_score = 0.55 * psi_val + 0.20 * swap_val + 0.15 * gpu_val + 0.10 * swap_io_val;

        let trend = if dt > 0.1 {
            (p_score - self.last_score) / dt
        } else {
            0.0
        };
        self.last_score = p_score;
        self.last_time = now;

        CycleScore { p_score, trend, swap_io_kbs, gpu_val }
    }
}

pub(crate) fn apply_swap_overrides(
    mut effective: PressureLevel,
    swap_used_pct: f64,
    swap_total: Kb,
    sustained_start: Option<std::time::Instant>,
    now: std::time::Instant,
) -> (PressureLevel, Option<std::time::Instant>) {
    let swap_exhausted = swap_total.0 > 0 && swap_used_pct >= 95.0;
    if swap_exhausted && effective < PressureLevel::Critical {
        effective = PressureLevel::Critical;
    }

    let swap_near_full = swap_total.0 > 0 && swap_used_pct >= 98.0;
    let new_sustained = if swap_near_full && effective >= PressureLevel::Critical {
        sustained_start.or(Some(now))
    } else if effective >= PressureLevel::Emergency {
        // Already at Emergency (score-driven or prior escalation) — preserve the timer so
        // a single cycle dip below 98% doesn't restart the 45 s window.
        sustained_start
    } else {
        None
    };

    if let Some(start) = new_sustained {
        let elapsed = now.duration_since(start).as_secs();
        if elapsed >= 45 && effective < PressureLevel::Emergency {
            effective = PressureLevel::Emergency;
        }
    }

    (effective, new_sustained)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // ── target_state_for / StateMachine ──────────────────────────────────────

    #[test]
    fn target_state_thresholds() {
        // psi_some_avg10 = 5.0 (clearly live) so the Warning-tier liveness
        // gate doesn't interfere with these threshold checks.
        assert_eq!(target_state_for(0.0, 0.0, 5.0, 0.0), ControlState::Calm);
        assert_eq!(target_state_for(0.15, 0.0, 5.0, 0.0), ControlState::Warning);
        assert_eq!(target_state_for(0.30, 0.0, 5.0, 0.0), ControlState::Evicting);
        assert_eq!(target_state_for(0.50, 0.0, 5.0, 0.0), ControlState::Critical);
        assert_eq!(target_state_for(0.70, 0.0, 5.0, 0.0), ControlState::Emergency);
    }

    #[test]
    fn target_state_rising_trend_lowers_bar() {
        assert_eq!(target_state_for(0.21, 0.03, 5.0, 0.0), ControlState::Evicting);
        assert_eq!(target_state_for(0.36, 0.04, 5.0, 0.0), ControlState::Critical);
        assert_eq!(target_state_for(0.56, 0.06, 5.0, 0.0), ControlState::Emergency);
        // Same scores without the trend stay one tier lower
        assert_eq!(target_state_for(0.21, 0.0, 5.0, 0.0), ControlState::Warning);
        assert_eq!(target_state_for(0.36, 0.0, 5.0, 0.0), ControlState::Evicting);
        assert_eq!(target_state_for(0.56, 0.0, 5.0, 0.0), ControlState::Critical);
    }

    #[test]
    fn warning_floor_needs_liveness() {
        // Stale swap alone (p_score=0.15 from swap_val=0.75 * 0.20) with zero
        // PSI and zero swap I/O — no longer stuck at Warning, recovers to Calm.
        assert_eq!(target_state_for(0.15, 0.0, 0.0, 0.0), ControlState::Calm);
    }

    #[test]
    fn warning_floor_fires_on_live_psi() {
        assert_eq!(target_state_for(0.15, 0.0, 5.0, 0.0), ControlState::Warning);
    }

    #[test]
    fn warning_floor_fires_on_live_swap_io() {
        assert_eq!(target_state_for(0.15, 0.0, 0.0, 5000.0), ControlState::Warning);
    }

    #[test]
    fn warning_floor_respects_psi_floor() {
        // Just below both floors — still Calm, not Warning.
        assert_eq!(
            target_state_for(0.15, 0.0, PSI_LIVE_FLOOR - 0.1, SWAP_IO_LIVE_FLOOR_KBS - 1.0),
            ControlState::Calm
        );
    }

    #[test]
    fn escalation_needs_two_ticks() {
        let mut sm = StateMachine::new();
        assert!(!sm.advance(ControlState::Evicting, 0.0));
        assert_eq!(sm.current, ControlState::Calm);
        assert!(!sm.advance(ControlState::Evicting, 0.0));
        assert_eq!(sm.current, ControlState::Evicting);
    }

    #[test]
    fn instant_escalation_on_sharp_spike() {
        let mut sm = StateMachine::new();
        assert!(sm.advance(ControlState::Emergency, 0.09));
        assert_eq!(sm.current, ControlState::Emergency);
    }

    #[test]
    fn no_instant_escalation_below_critical() {
        let mut sm = StateMachine::new();
        // Sharp trend but target only Evicting — still needs 2 ticks.
        assert!(!sm.advance(ControlState::Evicting, 0.09));
        assert_eq!(sm.current, ControlState::Calm);
    }

    #[test]
    fn escalation_target_change_resets_ticks() {
        let mut sm = StateMachine::new();
        sm.advance(ControlState::Warning, 0.0);
        sm.advance(ControlState::Evicting, 0.0); // pending switches — tick count restarts
        assert_eq!(sm.current, ControlState::Calm);
        sm.advance(ControlState::Evicting, 0.0);
        assert_eq!(sm.current, ControlState::Evicting);
    }

    #[test]
    fn recovery_to_calm_needs_twelve_ticks() {
        let mut sm = StateMachine::new();
        sm.advance(ControlState::Critical, 0.09); // instant escalate
        assert_eq!(sm.current, ControlState::Critical);
        for _ in 0..11 {
            sm.advance(ControlState::Calm, 0.0);
            assert_eq!(sm.current, ControlState::Critical);
        }
        sm.advance(ControlState::Calm, 0.0);
        assert_eq!(sm.current, ControlState::Calm);
    }

    #[test]
    fn matching_target_resets_pending() {
        let mut sm = StateMachine::new();
        sm.advance(ControlState::Warning, 0.0); // pending=Warning, 1 tick
        sm.advance(ControlState::Calm, 0.0);    // target==current → pending reset
        sm.advance(ControlState::Warning, 0.0); // must start over
        assert_eq!(sm.current, ControlState::Calm);
        sm.advance(ControlState::Warning, 0.0);
        assert_eq!(sm.current, ControlState::Warning);
    }

    // ── ScoreTracker ─────────────────────────────────────────────────────────

    fn make_meminfo(total_kb: u64, swap_total_kb: u64, swap_free_kb: u64) -> MemInfo {
        MemInfo { available_kb: Kb(total_kb / 2), total_kb: Kb(total_kb), swap_free_kb: Kb(swap_free_kb), swap_total_kb: Kb(swap_total_kb) }
    }

    fn fresh_tracker(now: Instant) -> ScoreTracker {
        ScoreTracker { last_score: 0.0, last_time: now, last_pswpin: 0, last_pswpout: 0 }
    }

    #[test]
    fn score_psi_only() {
        let t0 = Instant::now();
        let mut st = fresh_tracker(t0);
        let mi = make_meminfo(16_000_000, 0, 0);
        let s = st.update_with(50.0, &mi, 0, 0, Kb(0), t0 + Duration::from_secs(5));
        // 55% weight on PSI 0.5, everything else zero
        assert!((s.p_score - 0.275).abs() < 1e-9);
        assert_eq!(s.gpu_val, 0.0);
        assert_eq!(s.swap_io_kbs, 0.0);
    }

    #[test]
    fn score_all_components_maxed_hits_one() {
        let t0 = Instant::now();
        let mut st = fresh_tracker(t0);
        let mi = make_meminfo(16_000_000, 12_000_000, 0); // swap 100% used
        // GPU = total RAM, swap I/O far over 50 MB/s → every component clamps to 1.0
        let s = st.update_with(200.0, &mi, 10_000_000, 10_000_000, Kb(16_000_000), t0 + Duration::from_secs(5));
        assert!((s.p_score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn swap_io_rate_and_trend() {
        let t0 = Instant::now();
        let mut st = fresh_tracker(t0);
        let mi = make_meminfo(16_000_000, 0, 0);
        // 12800 pages × 4KB / 5s = 10240 KB/s → io_val 0.2 → score 0.02
        let s = st.update_with(0.0, &mi, 12800, 0, Kb(0), t0 + Duration::from_secs(5));
        assert!((s.swap_io_kbs - 10240.0).abs() < 1e-6);
        assert!((s.p_score - 0.02).abs() < 1e-9);
        assert!((s.trend - 0.02 / 5.0).abs() < 1e-9);
        // Next cycle, no new I/O: score falls back to 0, trend goes negative
        let s2 = st.update_with(0.0, &mi, 12800, 0, Kb(0), t0 + Duration::from_secs(10));
        assert_eq!(s2.p_score, 0.0);
        assert!(s2.trend < 0.0);
    }

    #[test]
    fn tiny_dt_yields_zero_trend_and_io() {
        let t0 = Instant::now();
        let mut st = fresh_tracker(t0);
        let mi = make_meminfo(16_000_000, 0, 0);
        // dt below the 0.1s floor: swap I/O rate and trend are suppressed
        let s = st.update_with(80.0, &mi, 99999, 99999, Kb(0), t0);
        assert_eq!(s.swap_io_kbs, 0.0);
        assert_eq!(s.trend, 0.0);
    }

    // ── apply_swap_overrides ─────────────────────────────────────────────────

    #[test]
    fn swap_below_95_no_override() {
        let now = Instant::now();
        let (level, sustained) = apply_swap_overrides(PressureLevel::Elevated, 94.9, Kb(10_000_000), None, now);
        assert_eq!(level, PressureLevel::Elevated);
        assert!(sustained.is_none());
    }

    #[test]
    fn swap_95_forces_critical() {
        let now = Instant::now();
        let (level, _) = apply_swap_overrides(PressureLevel::Elevated, 95.0, Kb(10_000_000), None, now);
        assert_eq!(level, PressureLevel::Critical);
    }

    #[test]
    fn swap_95_no_override_when_already_critical() {
        let now = Instant::now();
        let (level, _) = apply_swap_overrides(PressureLevel::Critical, 95.0, Kb(10_000_000), None, now);
        assert_eq!(level, PressureLevel::Critical);
    }

    #[test]
    fn swap_no_device_no_override() {
        let now = Instant::now();
        let (level, _) = apply_swap_overrides(PressureLevel::Elevated, 99.0, Kb(0), None, now);
        assert_eq!(level, PressureLevel::Elevated);
    }

    #[test]
    fn sustained_critical_swap_escalates_emergency() {
        let start = Instant::now() - Duration::from_secs(46);
        let now = Instant::now();
        let (level, _) = apply_swap_overrides(PressureLevel::Critical, 98.5, Kb(10_000_000), Some(start), now);
        assert_eq!(level, PressureLevel::Emergency);
    }

    #[test]
    fn sustained_critical_swap_not_yet_45s() {
        let start = Instant::now() - Duration::from_secs(44);
        let now = Instant::now();
        let (level, _) = apply_swap_overrides(PressureLevel::Critical, 98.5, Kb(10_000_000), Some(start), now);
        assert_eq!(level, PressureLevel::Critical);
    }

    #[test]
    fn sustained_critical_swap_resets_when_swap_drops() {
        let start = Instant::now() - Duration::from_secs(50);
        let now = Instant::now();
        let (_, sustained) = apply_swap_overrides(PressureLevel::Critical, 97.9, Kb(10_000_000), Some(start), now);
        assert!(sustained.is_none(), "timer must reset when swap < 98%");
    }

    #[test]
    fn swap_98_starts_sustained_timer() {
        let now = Instant::now();
        let (_, sustained) = apply_swap_overrides(PressureLevel::Critical, 98.0, Kb(10_000_000), None, now);
        assert!(sustained.is_some(), "timer must start at >=98% swap + Critical");
    }

    #[test]
    fn swap_98_preserves_existing_timer() {
        let start = Instant::now() - Duration::from_secs(10);
        let now = Instant::now();
        let (_, sustained) = apply_swap_overrides(PressureLevel::Critical, 98.0, Kb(10_000_000), Some(start), now);
        assert!(sustained.unwrap().elapsed().as_secs() >= 10, "existing timer must not be reset");
    }
}
