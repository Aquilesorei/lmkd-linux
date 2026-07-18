use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

// Hot-reloadable config: RwLock'd Arc so SIGHUP can swap it while readers
// keep their cycle-scoped snapshot.
static CONFIG: std::sync::OnceLock<RwLock<Arc<CompiledConfig>>> = std::sync::OnceLock::new();

const BUILTIN_CONFIG: &str = include_str!("../../config/priorities.toml");

fn config_cell() -> &'static RwLock<Arc<CompiledConfig>> {
    CONFIG.get_or_init(|| RwLock::new(Arc::new(load())))
}

/// Snapshot of the current config — a cheap Arc clone; no lock is held after
/// return, so the snapshot may live across blocking work. Called once per
/// cycle/request at composition roots (thread loop tops, IPC dispatch, main);
/// everything below receives `&CompiledConfig`. After a reload the next
/// snapshot sees the new config.
pub fn get() -> Arc<CompiledConfig> {
    config_cell().read().unwrap().clone()
}

/// Reload config from disk (called when SIGHUP received).
pub fn reload() {
    let new_cfg = Arc::new(load());
    *config_cell().write().unwrap() = new_cfg;
    mgd_common::output::locked_eprint("[config] Reloaded.");
}

/// Deterministic fixture for unit tests: built-in TOML with a fixed 15% target
/// (the RAM-scaled fallback would depend on the test machine's RAM).
#[cfg(test)]
pub(crate) fn test_config() -> CompiledConfig {
    let mut cfg = compile(BUILTIN_CONFIG).expect("built-in config must be valid");
    cfg.target_available_pct = 15.0;
    cfg
}

// ── raw TOML structs ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RawConfig {
    #[serde(default)]
    defaults: Defaults,
    #[serde(default)]
    apps: Vec<AppEntry>,
    #[serde(default)]
    protect: Vec<ProtectEntry>,
    #[serde(default)]
    category_priorities: HashMap<String, u8>,
    #[serde(default)]
    zram: Zram,
    #[serde(default)]
    reclaim: Reclaim,
    #[serde(default)]
    cache_drop: CacheDrop,
    #[serde(default)]
    idle_reclaim: IdleReclaim,
    #[serde(default)]
    thresholds: Thresholds,
    #[serde(default)]
    psi: Psi,
    #[serde(default)]
    emergency: EmergencyConfig,
    #[serde(default)]
    spike_mode: SpikeMode,
    #[serde(default)]
    throttle: ThrottleConfig,
}

#[derive(Deserialize)]
struct ThrottleConfig {
    #[serde(default)]
    exclude: Vec<String>,
    /// Force-release a throttled cgroup after this many continuous seconds
    /// if raw PSI shows no active stall — prevents residual swap% alone
    /// (via the composite pressure score) from pinning background daemons
    /// throttled indefinitely after the real pressure event has passed.
    #[serde(default = "default_throttle_max_duration_sec")]
    max_duration_sec: u64,
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        ThrottleConfig {
            exclude: Vec::new(),
            max_duration_sec: default_throttle_max_duration_sec(),
        }
    }
}

fn default_throttle_max_duration_sec() -> u64 { 300 }

/// `[psi]` — pressure-tier boundaries (some_avg10 %) and the full_avg10
/// accelerator floor. Defaults match the long-standing built-in values; an
/// invalid combination (non-increasing tiers, out of range) falls back to
/// defaults with a warning rather than producing nonsense levels.
#[derive(Deserialize)]
struct Psi {
    #[serde(default = "default_psi_elevated")]
    elevated_pct: f64,
    #[serde(default = "default_psi_high")]
    high_pct: f64,
    #[serde(default = "default_psi_critical")]
    critical_pct: f64,
    #[serde(default = "default_psi_emergency")]
    emergency_pct: f64,
    #[serde(default = "default_psi_full_critical")]
    full_critical_pct: f64,
}

impl Default for Psi {
    fn default() -> Self {
        Psi {
            elevated_pct: default_psi_elevated(),
            high_pct: default_psi_high(),
            critical_pct: default_psi_critical(),
            emergency_pct: default_psi_emergency(),
            full_critical_pct: default_psi_full_critical(),
        }
    }
}

fn default_psi_elevated() -> f64 { 5.0 }
fn default_psi_high() -> f64 { 25.0 }
fn default_psi_critical() -> f64 { 50.0 }
fn default_psi_emergency() -> f64 { 70.0 }
fn default_psi_full_critical() -> f64 { 20.0 }

/// `[thresholds]` — optional user override for the free-RAM target.
/// If not set, the daemon derives the target from total RAM (RAM-scaling).
/// Run `mgctl calibrate --apply` to populate this automatically.
#[derive(Deserialize, Default)]
struct Thresholds {
    /// Override the RAM-scaled free-RAM target (percentage 1–100).
    /// Example: target_available_pct = 18
    target_available_pct: Option<f64>,
}


/// `[zram]` — compaction pre-action (on by default). See priorities.toml.
#[derive(Deserialize)]
struct Zram {
    #[serde(default = "default_zram_compact")]
    compact_on_elevated: bool,
    #[serde(default = "default_zram_min_used")]
    min_used_mb: u64,
}

impl Default for Zram {
    fn default() -> Self {
        Zram {
            compact_on_elevated: default_zram_compact(),
            min_used_mb: default_zram_min_used(),
        }
    }
}

fn default_zram_compact() -> bool { true }
fn default_zram_min_used() -> u64 { 128 }

/// `[reclaim]` — proactive swap reclaim (PRIVILEGED, off by default; needs the
/// capped helper). Gates live in the daemon. See priorities.toml.
#[derive(Deserialize)]
struct Reclaim {
    #[serde(default)] // off unless explicitly enabled
    proactive_swap_reclaim: bool,
    #[serde(default = "default_reclaim_threshold_pct")]
    threshold_pct: f64,
    #[serde(default = "default_reclaim_cooldown")]
    cooldown_min: u64,
    #[serde(default = "default_reclaim_min_used")]
    min_zram_used_mb: u64,
    /// OOM guard: require MemAvailable > decompressed_footprint × this.
    #[serde(default = "default_reclaim_headroom_mult")]
    decompressed_headroom_mult: f64,
}

impl Default for Reclaim {
    fn default() -> Self {
        Reclaim {
            proactive_swap_reclaim: false,
            threshold_pct: default_reclaim_threshold_pct(),
            cooldown_min: default_reclaim_cooldown(),
            min_zram_used_mb: default_reclaim_min_used(),
            decompressed_headroom_mult: default_reclaim_headroom_mult(),
        }
    }
}

fn default_reclaim_threshold_pct() -> f64 { 30.0 }
fn default_reclaim_cooldown() -> u64 { 10 }
fn default_reclaim_min_used() -> u64 { 2048 }
fn default_reclaim_headroom_mult() -> f64 { 1.5 }

/// `[cache_drop]` — page-cache drop pre-action (on by default, no-op until
/// `paths` is set). See priorities.toml.
#[derive(Deserialize)]
struct CacheDrop {
    #[serde(default = "default_cache_enabled")]
    enabled: bool,
    #[serde(default = "default_cache_trigger")]
    trigger_level: String,
    #[serde(default = "default_cache_cooldown")]
    cooldown_min: u64,
    #[serde(default)]
    paths: Vec<String>,
}

impl Default for CacheDrop {
    fn default() -> Self {
        CacheDrop {
            enabled: default_cache_enabled(),
            trigger_level: default_cache_trigger(),
            cooldown_min: default_cache_cooldown(),
            paths: Vec::new(),
        }
    }
}

fn default_cache_enabled() -> bool { true }
fn default_cache_trigger() -> String { "High".to_string() }
fn default_cache_cooldown() -> u64 { 5 }

#[derive(Deserialize)]
struct IdleReclaim {
    #[serde(default = "default_idle_reclaim_enabled")]
    enabled: bool,
    #[serde(default = "default_idle_reclaim_sec")]
    idle_sec: u64,
    #[serde(default = "default_idle_reclaim_rss_min_mb")]
    rss_min_mb: u64,
    #[serde(default = "default_idle_reclaim_reclaim_pct")]
    reclaim_pct: u64,
    #[serde(default = "default_idle_reclaim_global_cooldown_sec")]
    global_cooldown_sec: u64,
    #[serde(default = "default_idle_reclaim_max_swap_occupancy_pct")]
    max_swap_occupancy_pct: f64,
    #[serde(default)]
    freeze_after_sec: Option<u64>,
    /// Reclaim cold pages from important (priority < 50) processes when idle.
    #[serde(default)]
    important_enabled: bool,
    /// Priority floor for important-tier reclaim (processes with priority >= this AND < 50).
    #[serde(default = "default_idle_reclaim_important_min_priority")]
    important_min_priority: u8,
    /// Seconds backgrounded before important-tier processes qualify for reclaim.
    #[serde(default = "default_idle_reclaim_important_idle_sec")]
    important_idle_sec: u64,
    /// Percentage of RSS to reclaim per cycle for important-tier processes.
    #[serde(default = "default_idle_reclaim_important_pct")]
    important_pct: u64,
}

impl Default for IdleReclaim {
    fn default() -> Self {
        IdleReclaim {
            enabled: default_idle_reclaim_enabled(),
            idle_sec: default_idle_reclaim_sec(),
            rss_min_mb: default_idle_reclaim_rss_min_mb(),
            reclaim_pct: default_idle_reclaim_reclaim_pct(),
            global_cooldown_sec: default_idle_reclaim_global_cooldown_sec(),
            max_swap_occupancy_pct: default_idle_reclaim_max_swap_occupancy_pct(),
            freeze_after_sec: None,
            important_enabled: false,
            important_min_priority: default_idle_reclaim_important_min_priority(),
            important_idle_sec: default_idle_reclaim_important_idle_sec(),
            important_pct: default_idle_reclaim_important_pct(),
        }
    }
}

fn default_idle_reclaim_enabled() -> bool { true }
fn default_idle_reclaim_sec() -> u64 { 180 }
fn default_idle_reclaim_rss_min_mb() -> u64 { 50 }
fn default_idle_reclaim_reclaim_pct() -> u64 { 20 }
fn default_idle_reclaim_global_cooldown_sec() -> u64 { 30 }
fn default_idle_reclaim_max_swap_occupancy_pct() -> f64 { 60.0 }
fn default_idle_reclaim_important_min_priority() -> u8 { 20 }
fn default_idle_reclaim_important_idle_sec() -> u64 { 300 }
fn default_idle_reclaim_important_pct() -> u64 { 10 }

/// `[emergency]` — last-resort actions when pressure stays at Emergency level.
#[derive(Deserialize)]
#[derive(Default)]
struct EmergencyConfig {
    /// Seconds of sustained Emergency before triggering `systemctl hibernate`.
    /// 0 (default) = disabled. Requires working hibernate (swap partition ≥ RAM).
    #[serde(default)]
    hibernate_after_sec: u64,
}


#[derive(Deserialize)]
struct SpikeMode {
    #[serde(default = "default_spike_enabled")]
    enabled: bool,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    victim_exclude: Vec<String>,
    #[serde(default = "default_spike_window_sec")]
    window_sec: u64,
    #[serde(default = "default_spike_headroom_factor")]
    headroom_factor: f64,
    #[serde(default = "default_spike_min_rss_kb")]
    min_rss_kb: u64,
    #[serde(default = "default_spike_growth_threshold_kb")]
    growth_threshold_kb: u64,
    #[serde(default = "default_spike_majflt_threshold")]
    majflt_threshold: u64,
    #[serde(default = "default_spike_oscillation_drop_factor")]
    oscillation_drop_factor: f64,
    #[serde(default = "default_spike_cpu_threshold_pct")]
    cpu_threshold_pct: f32,
    #[serde(default = "default_spike_throttled_cpu_weight")]
    throttled_cpu_weight: u32,
    #[serde(default = "default_spike_min_samples")]
    min_samples: usize,
    #[serde(default)]
    max_victim_freeze_sec: u64,
}

impl Default for SpikeMode {
    fn default() -> Self {
        SpikeMode {
            enabled: default_spike_enabled(),
            include: vec![],
            exclude: vec![],
            victim_exclude: vec![],
            window_sec: default_spike_window_sec(),
            headroom_factor: default_spike_headroom_factor(),
            min_rss_kb: default_spike_min_rss_kb(),
            growth_threshold_kb: default_spike_growth_threshold_kb(),
            majflt_threshold: default_spike_majflt_threshold(),
            oscillation_drop_factor: default_spike_oscillation_drop_factor(),
            cpu_threshold_pct: default_spike_cpu_threshold_pct(),
            throttled_cpu_weight: default_spike_throttled_cpu_weight(),
            min_samples: default_spike_min_samples(),
            max_victim_freeze_sec: 0,
        }
    }
}

fn default_spike_enabled() -> bool { false }
fn default_spike_window_sec() -> u64 { 120 }
fn default_spike_headroom_factor() -> f64 { 1.25 }
fn default_spike_min_rss_kb() -> u64 { 524_288 }
fn default_spike_growth_threshold_kb() -> u64 { 102_400 }
fn default_spike_majflt_threshold() -> u64 { 500 }
fn default_spike_oscillation_drop_factor() -> f64 { 0.90 }
fn default_spike_cpu_threshold_pct() -> f32 { 80.0 }
fn default_spike_throttled_cpu_weight() -> u32 { 20 }
fn default_spike_min_samples() -> usize { 6 }

#[derive(Deserialize)]
struct Defaults {
    #[serde(default = "default_fifty")]
    priority: u8,
    /// Maximum number of log files to keep in ~/memlogs/ (0 = unlimited)
    #[serde(default = "default_log_keep")]
    log_keep: usize,
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults { priority: 50, log_keep: 10 }
    }
}

fn default_fifty() -> u8 { 50 }
fn default_log_keep() -> usize { 10 }

#[derive(Deserialize)]
struct AppEntry {
    #[allow(dead_code)]
    name: String,
    pattern: String,
    priority: u8,
    /// If Some(true), always prefer CRIU checkpoint over kill at Critical.
    /// If Some(false), never checkpoint — go straight to kill.
    /// If None, use default decision logic.
    #[serde(default)]
    checkpoint: Option<bool>,
    /// SIGTERM after this many seconds of CPU-idle at Normal pressure.
    #[serde(default)]
    auto_kill_idle_after: Option<u64>,
}

/// Entries in the [[protect]] table are never touched by mgd,
/// regardless of memory pressure level.
#[derive(Deserialize)]
struct ProtectEntry {
    #[allow(dead_code)]
    name: String,
    pattern: String,
}

// ── compiled config (regex pre-built) ────────────────────────────────────────

pub struct CompiledConfig {
    pub default_priority: u8,
    pub log_keep: usize,

    /// Target free-RAM percentage used by the deficit calculation.
    /// Derived from calibration if available, otherwise RAM-scaled:
    ///   < 8 GB  → 20%,  8–16 GB → 15%,  16–32 GB → 12%,  > 32 GB → 10%.
    /// Can also be overridden in [thresholds] target_available_pct.
    pub target_available_pct: f64,

    /// zram compaction pre-action — on unless disabled in [zram].
    pub compact_zram_on_elevated: bool,
    /// Skip zram compaction when the pool holds less than this many MB.
    pub zram_min_used_mb: u64,
    /// Proactive swap reclaim (PRIVILEGED) — off unless enabled in [reclaim].
    pub proactive_swap_reclaim: bool,
    /// Only reclaim when swap is at least this % full.
    pub reclaim_threshold_pct: f64,
    /// Minimum seconds between proactive reclaim cycles (cooldown floor).
    pub reclaim_cooldown_secs: u64,
    /// Skip reclaim unless the zram pool holds at least this much compressed RAM.
    pub reclaim_min_zram_used_mb: u64,
    /// OOM guard: require MemAvailable > decompressed footprint × this multiplier.
    pub reclaim_headroom_mult: f64,
    /// Page-cache drop — on unless disabled in [cache_drop]; no-op with no paths.
    pub cache_drop_enabled: bool,
    /// Pressure level at/above which cache drop fires (parsed from trigger_level).
    pub cache_drop_trigger: crate::monitor::psi::PressureLevel,
    /// Pressure-tier boundaries from [psi] (validated; defaults if invalid).
    pub psi: crate::monitor::psi::PsiThresholds,
    /// Minimum seconds between cache-drop actions (cooldown floor).
    pub cache_drop_cooldown_secs: u64,
    /// Directory-tree patterns (~ and single-* per segment) to drop cache for.
    pub cache_drop_paths: Vec<String>,
    pub idle_reclaim_enabled: bool,
    pub idle_reclaim_sec: u64,
    pub idle_reclaim_rss_min_mb: u64,
    pub idle_reclaim_pct: u64,
    pub idle_reclaim_global_cooldown_sec: u64,
    pub idle_reclaim_max_swap_occupancy_pct: f64,
    pub idle_reclaim_freeze_after_sec: Option<u64>,
    pub idle_reclaim_important_enabled: bool,
    pub idle_reclaim_important_min_priority: u8,
    pub idle_reclaim_important_idle_sec: u64,
    pub idle_reclaim_important_pct: u64,
    pub emergency_hibernate_after_sec: u64,
    pub spike_mode_enabled: bool,
    pub spike_include: Vec<Regex>,
    pub spike_exclude: Vec<Regex>,
    pub spike_victim_exclude: Vec<Regex>,
    pub spike_window_sec: u64,
    pub spike_headroom_factor: f64,
    pub spike_min_rss_kb: u64,
    pub spike_growth_threshold_kb: u64,
    pub spike_majflt_threshold: u64,
    pub spike_oscillation_drop_factor: f64,
    pub spike_cpu_threshold_pct: f32,
    pub spike_throttled_cpu_weight: u32,
    pub spike_min_samples: usize,
    pub spike_max_victim_freeze_sec: u64,
    pub throttle_exclude: Vec<Regex>,
    pub throttle_max_duration_sec: u64,
    /// (regex, priority, checkpoint_override)
    entries: Vec<(Regex, u8, Option<bool>)>,
    /// (regex, idle_secs) — SIGTERM after this many CPU-idle seconds at Normal pressure
    pub auto_kill_rules: Vec<(Regex, u64)>,
    /// Patterns that must never be touched
    protected: Vec<Regex>,
    /// exe_basename → priority derived from .desktop Categories=
    desktop_index: HashMap<String, u8>,
    pub config_path: Option<PathBuf>,
}

impl CompiledConfig {
    pub fn priority_for(&self, process_name: &str, exe_basename: Option<&str>) -> u8 {
        for (re, prio, _) in &self.entries {
            if re.is_match(process_name) {
                return *prio;
            }
        }
        if let Some(exe) = exe_basename
            && let Some(&prio) = self.desktop_index.get(exe) {
                return prio;
            }
        self.default_priority
    }

    /// Returns the checkpoint override for a process name, if configured.
    /// - Some(true)  → always checkpoint at Critical
    /// - Some(false) → never checkpoint, go straight to kill
    /// - None        → use default decision logic
    pub fn checkpoint_override(&self, process_name: &str) -> Option<bool> {
        for (re, _, cp) in &self.entries {
            if re.is_match(process_name) {
                return *cp;
            }
        }
        None
    }

    pub fn auto_kill_idle_after_for(&self, name: &str) -> Option<u64> {
        self.auto_kill_rules.iter()
            .find(|(re, _)| re.is_match(name))
            .map(|(_, secs)| *secs)
    }

    /// Returns true if this process is on the protect list and must not be
    /// touched regardless of pressure level.
    pub fn is_protected(&self, process_name: &str) -> bool {
        // The hard-coded CRITICAL tier (priority <= 19) guard is a separate
        // layer; this checks user-supplied [[protect]] entries.
        self.protected.iter().any(|re| re.is_match(process_name))
    }
}

// ── loading ───────────────────────────────────────────────────────────────────

fn load() -> CompiledConfig {
    let (content, path) = try_user_config()
        .or_else(try_system_config)
        .unwrap_or_else(|| (BUILTIN_CONFIG.to_string(), None));

    match compile(&content) {
        Ok(mut cfg) => {
            cfg.config_path = path;
            apply_calibration_overlay(&mut cfg);
            cfg
        }
        Err(e) => {
            eprintln!("mgd: config error ({e}), falling back to built-in defaults");
            compile(BUILTIN_CONFIG).expect("built-in config must be valid")
        }
    }
}

// ── Calibration auto-apply ────────────────────────────────────────────────────

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

/// On every config load (startup + SIGHUP), overlay the two auto-calibrated
/// PSI thresholds from `calibration_suggestion.toml` if the file exists and
/// parses cleanly. Upper tiers (high/critical/emergency) are commented-out in
/// the suggestion file and thus ignored by the TOML parser — manual review
/// required before applying them.
fn apply_calibration_overlay(cfg: &mut CompiledConfig) {
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

fn try_user_config() -> Option<(String, Option<PathBuf>)> {
    if cfg!(test) {
        return None;
    }
    let path = mgd_common::util::home_dir().join(".config/mgd/priorities.toml");
    let content = std::fs::read_to_string(&path).ok()?;
    Some((content, Some(path)))
}

fn try_system_config() -> Option<(String, Option<PathBuf>)> {
    if cfg!(test) {
        return None;
    }
    let path = PathBuf::from("/etc/mgd/priorities.toml");
    let content = std::fs::read_to_string(&path).ok()?;
    Some((content, Some(path)))
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
fn ram_scaled_target_pct() -> f64 {
    let total_kb = crate::monitor::meminfo::read_meminfo().total_kb;
    let total_gb = total_kb.0 as f64 / (1024.0 * 1024.0);
    if      total_gb < 8.0  { 20.0 }
    else if total_gb < 16.0 { 15.0 }
    else if total_gb < 32.0 { 12.0 }
    else                    { 10.0 }
}

/// Try to load target_available_pct from `mgctl calibrate` output.
/// Returns None if no calibration file exists or it cannot be parsed.
fn load_calibrated_target_pct() -> Option<f64> {
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

fn compile(content: &str) -> Result<CompiledConfig, String> {
    let raw: RawConfig = toml::from_str(content).map_err(|e| e.to_string())?;

    // Resolve target_available_pct in priority order:
    //   1. [thresholds] override in config file (user explicit)
    //   2. Calibration file (~/.local/share/mgd/calibration.json)
    //   3. RAM-scaled default (safe for any machine, no config needed)
    let target_available_pct = raw.thresholds.target_available_pct
        .or_else(load_calibrated_target_pct)
        .unwrap_or_else(ram_scaled_target_pct);

    let mut entries = Vec::with_capacity(raw.apps.len());
    let mut auto_kill_rules = Vec::new();
    for app in raw.apps {
        match Regex::new(&app.pattern) {
            Ok(re) => {
                if let Some(secs) = app.auto_kill_idle_after {
                    auto_kill_rules.push((re.clone(), secs));
                }
                entries.push((re, app.priority, app.checkpoint));
            }
            Err(e) => eprintln!("mgd: skipping invalid regex '{}': {e}", app.pattern),
        }
    }

    let mut protected = Vec::with_capacity(raw.protect.len());
    for p in raw.protect {
        match Regex::new(&p.pattern) {
            Ok(re) => protected.push(re),
            Err(e) => eprintln!("mgd: skipping invalid protect regex '{}': {e}", p.pattern),
        }
    }

    let desktop_index = scan_desktop_files(&raw.category_priorities);

    let psi = crate::monitor::psi::PsiThresholds {
        elevated_pct: raw.psi.elevated_pct,
        high_pct: raw.psi.high_pct,
        critical_pct: raw.psi.critical_pct,
        emergency_pct: raw.psi.emergency_pct,
        full_critical_pct: raw.psi.full_critical_pct,
    };
    let psi = if psi.valid() {
        psi
    } else {
        eprintln!(
            "mgd: invalid [psi] thresholds (must be 0 < elevated < high < critical \
             < emergency <= 100, full_critical_pct > 0), using defaults"
        );
        crate::monitor::psi::PsiThresholds::default()
    };

    Ok(CompiledConfig {
        default_priority: raw.defaults.priority,
        log_keep: raw.defaults.log_keep,
        target_available_pct,

        compact_zram_on_elevated: raw.zram.compact_on_elevated,
        zram_min_used_mb: raw.zram.min_used_mb,
        proactive_swap_reclaim: raw.reclaim.proactive_swap_reclaim,
        reclaim_threshold_pct: raw.reclaim.threshold_pct,
        reclaim_cooldown_secs: raw.reclaim.cooldown_min.saturating_mul(60),
        reclaim_min_zram_used_mb: raw.reclaim.min_zram_used_mb,
        reclaim_headroom_mult: raw.reclaim.decompressed_headroom_mult,
        cache_drop_enabled: raw.cache_drop.enabled,
        cache_drop_trigger: crate::monitor::psi::PressureLevel::parse(&raw.cache_drop.trigger_level)
            .unwrap_or_else(|| {
                eprintln!(
                    "mgd: invalid [cache_drop] trigger_level '{}', defaulting to High",
                    raw.cache_drop.trigger_level
                );
                crate::monitor::psi::PressureLevel::High
            }),
        cache_drop_cooldown_secs: raw.cache_drop.cooldown_min.saturating_mul(60),
        cache_drop_paths: raw.cache_drop.paths,
        idle_reclaim_enabled: raw.idle_reclaim.enabled,
        idle_reclaim_sec: raw.idle_reclaim.idle_sec,
        idle_reclaim_rss_min_mb: raw.idle_reclaim.rss_min_mb,
        idle_reclaim_pct: raw.idle_reclaim.reclaim_pct,
        idle_reclaim_global_cooldown_sec: raw.idle_reclaim.global_cooldown_sec,
        idle_reclaim_max_swap_occupancy_pct: raw.idle_reclaim.max_swap_occupancy_pct,
        idle_reclaim_freeze_after_sec: raw.idle_reclaim.freeze_after_sec,
        idle_reclaim_important_enabled: raw.idle_reclaim.important_enabled,
        idle_reclaim_important_min_priority: raw.idle_reclaim.important_min_priority,
        idle_reclaim_important_idle_sec: raw.idle_reclaim.important_idle_sec,
        idle_reclaim_important_pct: raw.idle_reclaim.important_pct,
        emergency_hibernate_after_sec: raw.emergency.hibernate_after_sec,
        spike_mode_enabled: raw.spike_mode.enabled,
        spike_include: raw.spike_mode.include.iter()
            .filter_map(|p| Regex::new(p).map_err(|e| {
                mgd_common::output::locked_eprint(&format!("[config] invalid spike include pattern '{}': {e}", p));
            }).ok())
            .collect(),
        spike_exclude: raw.spike_mode.exclude.iter()
            .filter_map(|p| Regex::new(p).map_err(|e| {
                mgd_common::output::locked_eprint(&format!("[config] invalid spike exclude pattern '{}': {e}", p));
            }).ok())
            .collect(),
        spike_victim_exclude: raw.spike_mode.victim_exclude.iter()
            .filter_map(|p| Regex::new(p).map_err(|e| {
                mgd_common::output::locked_eprint(&format!("[config] invalid spike victim_exclude pattern '{}': {e}", p));
            }).ok())
            .collect(),
        spike_window_sec: raw.spike_mode.window_sec,
        spike_headroom_factor: raw.spike_mode.headroom_factor,
        spike_min_rss_kb: raw.spike_mode.min_rss_kb,
        spike_growth_threshold_kb: raw.spike_mode.growth_threshold_kb,
        spike_majflt_threshold: raw.spike_mode.majflt_threshold,
        spike_oscillation_drop_factor: raw.spike_mode.oscillation_drop_factor,
        spike_cpu_threshold_pct: raw.spike_mode.cpu_threshold_pct,
        spike_throttled_cpu_weight: raw.spike_mode.throttled_cpu_weight,
        spike_min_samples: raw.spike_mode.min_samples,
        spike_max_victim_freeze_sec: raw.spike_mode.max_victim_freeze_sec,
        throttle_exclude: raw.throttle.exclude.iter()
            .filter_map(|p| Regex::new(p).map_err(|e| {
                mgd_common::output::locked_eprint(&format!("[config] invalid throttle exclude pattern '{}': {e}", p));
            }).ok())
            .collect(),
        throttle_max_duration_sec: raw.throttle.max_duration_sec,
        psi,
        entries,
        auto_kill_rules,
        protected,
        desktop_index,
        config_path: None,
    })
}

fn scan_desktop_files(category_priorities: &HashMap<String, u8>) -> HashMap<String, u8> {
    let mut index = HashMap::new();
    let home = mgd_common::util::home_dir();
    // User dirs first so or_insert() first-wins gives user overrides priority over system.
    let dirs = [
        home.join(".local/share/applications"),
        home.join(".local/share/flatpak/exports/share/applications"),
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/var/lib/flatpak/exports/share/applications"),
    ];
    for dir in &dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            if let Some((exe, prio)) = parse_desktop_file(&path, category_priorities) {
                index.entry(exe).or_insert(prio);
            }
        }
    }
    index
}

fn parse_desktop_file(path: &Path, category_priorities: &HashMap<String, u8>) -> Option<(String, u8)> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut exe_basename: Option<String> = None;
    // Borrow slices directly from content — no String allocation per category.
    let mut categories: Vec<&str> = vec![];
    // Only parse keys from the [Desktop Entry] section; skip [Desktop Action *] etc.
    let mut in_desktop_entry = false;

    for line in content.lines() {
        if line.starts_with('[') {
            in_desktop_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_desktop_entry {
            continue;
        }
        if line.starts_with("Exec=") && exe_basename.is_none() {
            let rest = &line["Exec=".len()..];
            // Use `else { continue }` instead of `?` so a blank Exec= skips only this line.
            let Some(binary) = rest.split_whitespace().next() else { continue };
            let Some(name) = Path::new(binary).file_name() else { continue };
            exe_basename = Some(name.to_string_lossy().into_owned());
        } else if let Some(rest) = line.strip_prefix("Categories=") {
            categories = rest
                .split(';')
                .filter(|s| !s.is_empty())
                .collect();
        }
    }

    let exe = exe_basename?;
    // Use max priority across all matching categories: the most expendable category wins,
    // ensuring the process is not under-prioritised due to incidental low-priority categories.
    let prio = categories.iter()
        .filter_map(|cat| category_priorities.get(*cat).copied())
        .max()?;
    Some((exe, prio))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::psi::PsiThresholds;

    #[test]
    fn test_psi_block_overrides() {
        let cfg = compile(
            "[psi]\n\
             elevated_pct = 10.0\n\
             high_pct = 35.0\n\
             critical_pct = 60.0\n\
             emergency_pct = 80.0\n\
             full_critical_pct = 30.0\n",
        ).unwrap();
        assert_eq!(cfg.psi.elevated_pct, 10.0);
        assert_eq!(cfg.psi.high_pct, 35.0);
        assert_eq!(cfg.psi.full_critical_pct, 30.0);
    }

    #[test]
    fn test_psi_partial_block_keeps_other_defaults() {
        let cfg = compile("[psi]\nelevated_pct = 8.0\n").unwrap();
        assert_eq!(cfg.psi.elevated_pct, 8.0);
        assert_eq!(cfg.psi.high_pct, 25.0);
        assert_eq!(cfg.psi.emergency_pct, 70.0);
    }

    #[test]
    fn test_psi_defaults_when_absent() {
        let cfg = compile("").unwrap();
        assert_eq!(cfg.psi, PsiThresholds::default());
    }

    #[test]
    fn test_psi_invalid_falls_back_to_defaults() {
        // elevated >= high: rejected as a set, not silently reordered.
        let cfg = compile("[psi]\nelevated_pct = 50.0\nhigh_pct = 25.0\n").unwrap();
        assert_eq!(cfg.psi, PsiThresholds::default());
    }

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
