use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

// Hot-reloadable config: wrapped in an Arc<RwLock> so SIGHUP can swap it.
static CONFIG: std::sync::OnceLock<Arc<RwLock<CompiledConfig>>> = std::sync::OnceLock::new();

const BUILTIN_CONFIG: &str = include_str!("../config/priorities.toml");

pub fn get_arc() -> &'static Arc<RwLock<CompiledConfig>> {
    CONFIG.get_or_init(|| Arc::new(RwLock::new(load())))
}

/// Convenience wrapper — borrows a read guard long enough for a single call.
/// Most callers use this.
pub fn get() -> std::sync::RwLockReadGuard<'static, CompiledConfig> {
    get_arc().read().unwrap()
}

/// Reload config from disk (called when SIGHUP received).
pub fn reload() {
    let new_cfg = load();
    *get_arc().write().unwrap() = new_cfg;
    crate::output::locked_eprint("[config] Reloaded.");
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
    plasma: Plasma,
    #[serde(default)]
    firefox: Firefox,
}

/// Optional Firefox preventive-memory watcher. Disabled unless `watch_memory = true`.
/// Runs only at PressureLevel::Normal — see evictor::check_firefox_memory.
#[derive(Deserialize)]
struct Firefox {
    #[serde(default)]
    watch_memory: bool,
    #[serde(default = "default_ff_threshold")]
    rss_threshold_mb: u64,
    #[serde(default = "default_ff_cooldown")]
    gc_cooldown_min: u64,
    #[serde(default = "default_ff_warn")]
    warn_threshold_mb: u64,
}

impl Default for Firefox {
    fn default() -> Self {
        Firefox {
            watch_memory: false,
            rss_threshold_mb: default_ff_threshold(),
            gc_cooldown_min: default_ff_cooldown(),
            warn_threshold_mb: default_ff_warn(),
        }
    }
}

fn default_ff_threshold() -> u64 { 3072 }
fn default_ff_cooldown() -> u64 { 15 }
fn default_ff_warn() -> u64 { 4096 }

/// Optional plasmashell GPU-leak watcher (KDE Plasma + Intel UMA workaround).
/// Disabled unless `watch_gpu_leak = true`.
#[derive(Deserialize)]
struct Plasma {
    #[serde(default)]
    watch_gpu_leak: bool,
    #[serde(default = "default_gpu_threshold")]
    gpu_leak_threshold_mb: u64,
    #[serde(default = "default_restart_floor")]
    min_restart_interval_min: u64,
}

impl Default for Plasma {
    fn default() -> Self {
        Plasma {
            watch_gpu_leak: false,
            gpu_leak_threshold_mb: default_gpu_threshold(),
            min_restart_interval_min: default_restart_floor(),
        }
    }
}

fn default_gpu_threshold() -> u64 { 1024 }
fn default_restart_floor() -> u64 { 30 }

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
    /// Plasma GPU-leak watcher — off unless enabled in [plasma].
    pub watch_gpu_leak: bool,
    pub gpu_leak_threshold_mb: u64,
    /// Minimum seconds between plasmashell restarts (cooldown floor).
    pub min_restart_interval_secs: u64,
    /// Firefox preventive-memory watcher — off unless enabled in [firefox].
    pub watch_firefox: bool,
    pub firefox_rss_threshold_mb: u64,
    /// Minimum seconds between Firefox GC attempts (cooldown floor).
    pub firefox_gc_cooldown_secs: u64,
    pub firefox_warn_threshold_mb: u64,
    /// (regex, priority, checkpoint_override)
    entries: Vec<(Regex, u8, Option<bool>)>,
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
        if let Some(exe) = exe_basename {
            if let Some(&prio) = self.desktop_index.get(exe) {
                return prio;
            }
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
            cfg
        }
        Err(e) => {
            eprintln!("mgd: config error ({e}), falling back to built-in defaults");
            compile(BUILTIN_CONFIG).expect("built-in config must be valid")
        }
    }
}

fn try_user_config() -> Option<(String, Option<PathBuf>)> {
    let path = crate::util::home_dir().join(".config/mgd/priorities.toml");
    let content = std::fs::read_to_string(&path).ok()?;
    Some((content, Some(path)))
}

fn try_system_config() -> Option<(String, Option<PathBuf>)> {
    let path = PathBuf::from("/etc/mgd/priorities.toml");
    let content = std::fs::read_to_string(&path).ok()?;
    Some((content, Some(path)))
}

fn compile(content: &str) -> Result<CompiledConfig, String> {
    let raw: RawConfig = toml::from_str(content).map_err(|e| e.to_string())?;

    let mut entries = Vec::with_capacity(raw.apps.len());
    for app in raw.apps {
        match Regex::new(&app.pattern) {
            Ok(re) => entries.push((re, app.priority, app.checkpoint)),
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

    Ok(CompiledConfig {
        default_priority: raw.defaults.priority,
        log_keep: raw.defaults.log_keep,
        watch_gpu_leak: raw.plasma.watch_gpu_leak,
        gpu_leak_threshold_mb: raw.plasma.gpu_leak_threshold_mb,
        min_restart_interval_secs: raw.plasma.min_restart_interval_min.saturating_mul(60),
        watch_firefox: raw.firefox.watch_memory,
        firefox_rss_threshold_mb: raw.firefox.rss_threshold_mb,
        firefox_gc_cooldown_secs: raw.firefox.gc_cooldown_min.saturating_mul(60),
        firefox_warn_threshold_mb: raw.firefox.warn_threshold_mb,
        entries,
        protected,
        desktop_index,
        config_path: None,
    })
}

fn scan_desktop_files(category_priorities: &HashMap<String, u8>) -> HashMap<String, u8> {
    let mut index = HashMap::new();
    let home = crate::util::home_dir();
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

fn parse_desktop_file<'a>(path: &Path, category_priorities: &HashMap<String, u8>) -> Option<(String, u8)> {
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
        } else if line.starts_with("Categories=") {
            categories = line["Categories=".len()..]
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
