use regex::Regex;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::OnceLock;

static CONFIG: OnceLock<CompiledConfig> = OnceLock::new();

const BUILTIN_CONFIG: &str = include_str!("../config/priorities.toml");

pub fn get() -> &'static CompiledConfig {
    CONFIG.get_or_init(load)
}

#[derive(Deserialize)]
struct RawConfig {
    #[serde(default)]
    defaults: Defaults,
    #[serde(default)]
    apps: Vec<AppEntry>,
}

#[derive(Deserialize)]
struct Defaults {
    #[serde(default = "default_fifty")]
    priority: u8,
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults { priority: 50 }
    }
}

fn default_fifty() -> u8 {
    50
}

#[derive(Deserialize)]
struct AppEntry {
    #[allow(dead_code)]
    name: String,
    pattern: String,
    priority: u8,
}

pub struct CompiledConfig {
    default_priority: u8,
    entries: Vec<(Regex, u8)>,
    pub config_path: Option<PathBuf>,
}

impl CompiledConfig {
    pub fn priority_for(&self, process_name: &str) -> u8 {
        for (re, prio) in &self.entries {
            if re.is_match(process_name) {
                return *prio;
            }
        }
        self.default_priority
    }
}

fn load() -> CompiledConfig {
    let (content, path) = try_user_config()
        .or_else(|| try_system_config())
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
    let path = std::env::var("HOME").ok()
        .map(|h| PathBuf::from(h).join(".config/mgd/priorities.toml"))?;
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
            Ok(re) => entries.push((re, app.priority)),
            Err(e) => eprintln!("mgd: skipping invalid regex '{}': {e}", app.pattern),
        }
    }
    Ok(CompiledConfig {
        default_priority: raw.defaults.priority,
        entries,
        config_path: None,
    })
}
