//! Passive PSI calibration


use serde::{Deserialize, Serialize};

use crate::monitor::psi::PsiThresholds;

pub const BINS: usize = 100;

const STALL_FULL_PCT: f64 = 1.0;

const ELEVATED_MARGIN_PCT: f64 = 2.0;

const MIN_OBSERVED_SECS: u64 = 24 * 3600;
const MIN_STALL_EVENTS: u32 = 10;
const MIN_STALL_SECS: u64 = 60;

#[derive(Clone, Serialize, Deserialize)]
pub struct CalibratorState {
    benign_secs: Vec<u64>,
    stall_secs: Vec<u64>,
    full_secs: Vec<u64>,
    pub observed_secs: u64,
    pub stall_events: u32,
    pub max_full_avg10: f64,
}

impl Default for CalibratorState {
    fn default() -> Self {
        CalibratorState {
            benign_secs: vec![0; BINS],
            stall_secs: vec![0; BINS],
            full_secs: vec![0; BINS],
            observed_secs: 0,
            stall_events: 0,
            max_full_avg10: 0.0,
        }
    }
}

pub struct Suggestion {
    pub elevated_pct: f64,
    pub full_critical_pct: f64,
    pub high_pct: f64,
    pub critical_pct: f64,
    pub emergency_pct: f64,
    // Provenance.
    pub observed_hours: f64,
    pub stall_events: u32,
    pub benign_p95: f64,
    pub stall_onset_p10: f64,
}

pub struct Calibrator {
    state: CalibratorState,
    in_stall: bool,
    dirty: bool,
}

fn bin(pct: f64) -> usize {
    (pct.max(0.0) as usize).min(BINS - 1)
}

impl Calibrator {
    pub fn new() -> Self {
        Calibrator { state: CalibratorState::default(), in_stall: false, dirty: false }
    }

    pub fn from_state(state: CalibratorState) -> Self {
        // Reject histograms of the wrong width (e.g. hand-edited file) rather
        // than index out of bounds later.
        if state.benign_secs.len() != BINS
            || state.stall_secs.len() != BINS
            || state.full_secs.len() != BINS
        {
            return Self::new();
        }
        Calibrator { state, in_stall: false, dirty: false }
    }

    pub fn observe(
        &mut self,
        some_avg10: f64,
        full_avg10: f64,
        intervention_active: bool,
        weight_secs: u64,
    ) {
        let stalling = full_avg10 >= STALL_FULL_PCT;

        if intervention_active {
            self.in_stall = stalling;
            return;
        }

        if stalling && !self.in_stall {
            self.state.stall_events = self.state.stall_events.saturating_add(1);
        }
        self.in_stall = stalling;

        self.state.observed_secs = self.state.observed_secs.saturating_add(weight_secs);
        if full_avg10 > self.state.max_full_avg10 {
            self.state.max_full_avg10 = full_avg10;
        }

        let b = bin(some_avg10);
        if stalling {
            self.state.stall_secs[b] = self.state.stall_secs[b].saturating_add(weight_secs);
            let fb = bin(full_avg10);
            self.state.full_secs[fb] = self.state.full_secs[fb].saturating_add(weight_secs);
        } else {
            self.state.benign_secs[b] = self.state.benign_secs[b].saturating_add(weight_secs);
        }
        self.dirty = true;
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    #[cfg(test)]
    pub fn state(&self) -> &CalibratorState {
        &self.state
    }

    pub fn to_toml(&mut self) -> String {
        self.dirty = false;
        toml::to_string(&self.state).unwrap_or_default()
    }

    pub fn from_toml(s: &str) -> Option<Self> {
        toml::from_str::<CalibratorState>(s).ok().map(Self::from_state)
    }

    pub fn suggest(&self) -> Option<Suggestion> {
        let st = &self.state;
        let stall_total: u64 = st.stall_secs.iter().sum();
        if st.observed_secs < MIN_OBSERVED_SECS
            || st.stall_events < MIN_STALL_EVENTS
            || stall_total < MIN_STALL_SECS
        {
            return None;
        }


        let benign_p95 = percentile(&st.benign_secs, 95.0).unwrap_or(0.0);
        // Where real stalls begin: low percentile of some_avg10 during stalls.
        let stall_onset_p10 = percentile(&st.stall_secs, 10.0).unwrap_or(100.0);

        let mut elevated = (benign_p95 + ELEVATED_MARGIN_PCT).clamp(2.0, 15.0);
        if stall_onset_p10 < elevated {
            elevated = stall_onset_p10.max(2.0);
        }


        let full_critical = percentile(&st.full_secs, 95.0)
            .unwrap_or(20.0)
            .clamp(5.0, 50.0);


        let scale = elevated / 5.0;
        let emergency = (70.0 * scale).min(100.0);
        let critical = (50.0 * scale).min(emergency - 1.0);
        let high = (25.0 * scale).min(critical - 1.0);

        Some(Suggestion {
            elevated_pct: elevated,
            full_critical_pct: full_critical,
            high_pct: high,
            critical_pct: critical,
            emergency_pct: emergency,
            observed_hours: st.observed_secs as f64 / 3600.0,
            stall_events: st.stall_events,
            benign_p95,
            stall_onset_p10,
        })
    }
}


fn percentile(hist: &[u64], pct: f64) -> Option<f64> {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return None;
    }
    let target = (total as f64 * pct / 100.0).ceil() as u64;
    let mut cum = 0u64;
    for (i, &w) in hist.iter().enumerate() {
        cum += w;
        if cum >= target {
            return Some((i + 1) as f64);
        }
    }
    Some(BINS as f64)
}


pub fn render_suggestion(s: &Suggestion, current: &PsiThresholds, generated_unix_secs: u64) -> String {
    format!(
        "# Generated by mgd passive calibration (unix time {ts})\n\
         # Observed: {hours:.1}h of untreated pressure, {events} stall episodes.\n\
         #   benign some_avg10 p95 (noise ceiling) = {bp95:.1}%\n\
         #   stall-onset some_avg10 p10            = {sp10:.1}%\n\
         #\n\
         # Review, then paste into ~/.config/mgd/priorities.toml and run: mgctl reload\n\
         # (elevated_pct also re-arms the kernel trigger on daemon restart only.)\n\
         \n\
         [psi]\n\
         elevated_pct      = {elev:.1}    # current: {cur_elev:.1}\n\
         full_critical_pct = {fcrit:.1}    # current: {cur_fcrit:.1}\n\
         \n\
         # Upper tiers scaled from elevated_pct by the default ratios. Passive\n\
         # observation can't validate them (the daemon's own interventions cap\n\
         # how high pressure climbs) — uncomment only if you trust the scaling.\n\
         #high_pct      = {high:.1}    # current: {cur_high:.1}\n\
         #critical_pct  = {crit:.1}    # current: {cur_crit:.1}\n\
         #emergency_pct = {emerg:.1}    # current: {cur_emerg:.1}\n",
        ts = generated_unix_secs,
        hours = s.observed_hours,
        events = s.stall_events,
        bp95 = s.benign_p95,
        sp10 = s.stall_onset_p10,
        elev = s.elevated_pct,
        cur_elev = current.elevated_pct,
        fcrit = s.full_critical_pct,
        cur_fcrit = current.full_critical_pct,
        high = s.high_pct,
        cur_high = current.high_pct,
        crit = s.critical_pct,
        cur_crit = current.critical_pct,
        emerg = s.emergency_pct,
        cur_emerg = current.emergency_pct,
    )
}

#[cfg(test)]
mod tests {
    use super::*;


    fn seeded(benign_pct: f64, stall_some_pct: f64, full_pct: f64) -> Calibrator {
        let mut c = Calibrator::new();
        // Benign bulk: one big sample is fine — percentile is mass-weighted.
        c.observe(benign_pct, 0.0, false, MIN_OBSERVED_SECS);
        // 10 distinct stall episodes, 30s each, separated by benign samples.
        for _ in 0..MIN_STALL_EVENTS {
            c.observe(stall_some_pct, full_pct, false, 30);
            c.observe(benign_pct, 0.0, false, 5);
        }
        c
    }

    #[test]
    fn test_gates_block_until_enough_data() {
        let mut c = Calibrator::new();
        assert!(c.suggest().is_none());

        // Lots of hours but no stalls → still no suggestion.
        c.observe(1.0, 0.0, false, MIN_OBSERVED_SECS * 2);
        assert!(c.suggest().is_none());

        // Stall mass but too few distinct episodes.
        c.observe(20.0, 5.0, false, 600);
        assert!(c.suggest().is_none());
    }

    #[test]
    fn test_suggestion_from_clean_distribution() {
        // Noise ceiling ~3%, stalls start at some=20 with full=8.
        let c = seeded(3.0, 20.0, 8.0);
        let s = c.suggest().expect("gates satisfied");
        // benign p95 upper edge = 4.0, +2 margin = 6.0; stall onset 21 doesn't cap it.
        assert_eq!(s.elevated_pct, 6.0);
        // full p95 upper edge = 9.0, within [5, 50].
        assert_eq!(s.full_critical_pct, 9.0);
        // Upper tiers strictly increasing.
        assert!(s.elevated_pct < s.high_pct);
        assert!(s.high_pct < s.critical_pct);
        assert!(s.critical_pct < s.emergency_pct);
        assert!(s.emergency_pct <= 100.0);
    }

    #[test]
    fn test_stall_onset_caps_elevated() {
        // Noisy machine (benign up to 30%) but stalls already begin at some=8.
        let c = seeded(30.0, 8.0, 5.0);
        let s = c.suggest().unwrap();
        // benign p95 + margin would be 33 → clamp 15 → capped by stall onset 9.
        assert_eq!(s.elevated_pct, 9.0);
    }

    #[test]
    fn test_intervention_samples_excluded() {
        let mut c = Calibrator::new();
        c.observe(50.0, 30.0, true, 10_000);
        assert_eq!(c.state().observed_secs, 0);
        assert_eq!(c.state().stall_events, 0);
        assert!(!c.dirty());

        // Episode that started under intervention isn't counted again when the
        // intervention ends mid-stall.
        c.observe(50.0, 30.0, false, 5);
        assert_eq!(c.state().stall_events, 0);
        // A genuinely new episode after recovery is.
        c.observe(1.0, 0.0, false, 5);
        c.observe(40.0, 20.0, false, 5);
        assert_eq!(c.state().stall_events, 1);
    }

    #[test]
    fn test_stall_event_debounce() {
        let mut c = Calibrator::new();
        // One continuous episode sampled three times = one event.
        c.observe(30.0, 5.0, false, 5);
        c.observe(35.0, 8.0, false, 5);
        c.observe(30.0, 4.0, false, 5);
        assert_eq!(c.state().stall_events, 1);
        // Recovery then a second episode.
        c.observe(2.0, 0.0, false, 5);
        c.observe(30.0, 5.0, false, 5);
        assert_eq!(c.state().stall_events, 2);
    }

    #[test]
    fn test_toml_round_trip() {
        let mut c = seeded(3.0, 20.0, 8.0);
        assert!(c.dirty());
        let toml = c.to_toml();
        assert!(!c.dirty());

        let c2 = Calibrator::from_toml(&toml).expect("parses back");
        assert_eq!(c2.state().observed_secs, c.state().observed_secs);
        assert_eq!(c2.state().stall_events, c.state().stall_events);
        assert_eq!(c2.suggest().unwrap().elevated_pct, c.suggest().unwrap().elevated_pct);
    }

    #[test]
    fn test_from_state_rejects_wrong_histogram_width() {
        let mut st = CalibratorState::default();
        st.benign_secs = vec![0; 7];
        st.observed_secs = 999;
        let c = Calibrator::from_state(st);
        assert_eq!(c.state().observed_secs, 0); // reset to fresh
    }

    #[test]
    fn test_percentile_edges() {
        assert_eq!(percentile(&[0; BINS], 95.0), None);
        let mut h = vec![0u64; BINS];
        h[3] = 100;
        assert_eq!(percentile(&h, 50.0), Some(4.0)); // upper edge of bin 3
        h[80] = 1; // 1% outlier doesn't move p95
        assert_eq!(percentile(&h, 95.0), Some(4.0));
    }

    #[test]
    fn test_render_contains_psi_block() {
        let c = seeded(3.0, 20.0, 8.0);
        let s = c.suggest().unwrap();
        let out = render_suggestion(&s, &PsiThresholds::default(), 1_700_000_000);
        assert!(out.contains("[psi]"));
        assert!(out.contains("elevated_pct      = 6.0"));
        assert!(out.contains("#high_pct"));
        // Live values must form a valid partial override against defaults.
        assert!(s.elevated_pct < PsiThresholds::default().high_pct);
    }
}
