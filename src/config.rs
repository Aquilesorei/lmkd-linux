use regex::Regex;
use serde::Deserialize;
use std::path::PathBuf;
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
}

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
    /// (regex, priority, checkpoint_override)
    entries: Vec<(Regex, u8, Option<bool>)>,
    /// Patterns that must never be touched
    protected: Vec<Regex>,
    pub config_path: Option<PathBuf>,
}

impl CompiledConfig {
    pub fn priority_for(&self, process_name: &str) -> u8 {
        for (re, prio, _) in &self.entries {
            if re.is_match(process_name) {
                return *prio;
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

    Ok(CompiledConfig {
        default_priority: raw.defaults.priority,
        log_keep: raw.defaults.log_keep,
        entries,
        protected,
        config_path: None,
    })
}
