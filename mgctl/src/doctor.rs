//! `mgctl doctor` — read-only environment introspection.
//!
//! Reports: detected GPU, swap backend, desktop environment, compositor, PSI
//! availability, active plugin binaries, and calibration status.
//! Makes zero state mutations and sends nothing to the daemon socket.

use std::fs;
use std::path::Path;

// ── Colour helpers ────────────────────────────────────────────────────────────

fn ok(s: &str)   -> String { format!("\x1b[32m✓\x1b[0m {s}") }
fn warn(s: &str) -> String { format!("\x1b[33m⚠\x1b[0m {s}") }
fn skip(s: &str) -> String { format!("\x1b[90m✗\x1b[0m {s}") }
fn bold(s: &str) -> String { format!("\x1b[1m{s}\x1b[0m") }

// ── GPU detection ─────────────────────────────────────────────────────────────

#[derive(Debug)]
enum GpuVendor { Intel, Amd, Nvidia, Unknown }

struct GpuInfo {
    vendor: GpuVendor,
    name: String,
}

fn detect_gpu() -> Option<GpuInfo> {
    let drm_dir = Path::new("/sys/class/drm");
    let Ok(entries) = fs::read_dir(drm_dir) else { return None };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        // Only top-level card nodes (card0, card1 — not card1-DP-1 etc.)
        let name = path.file_name()?.to_string_lossy().to_string();
        if !name.starts_with("card") || name.contains('-') { continue }

        let vendor_path = path.join("device/vendor");
        let vendor_id = fs::read_to_string(&vendor_path)
            .unwrap_or_default()
            .trim()
            .to_string();

        let uevent = fs::read_to_string(path.join("device/uevent"))
            .unwrap_or_default();

        let driver = uevent.lines()
            .find(|l| l.starts_with("DRIVER="))
            .and_then(|l| l.strip_prefix("DRIVER="))
            .unwrap_or("unknown")
            .to_string();

        let (vendor, label) = match vendor_id.as_str() {
            "0x8086" => (GpuVendor::Intel, format!("Intel iGPU ({})", driver)),
            "0x1002" => (GpuVendor::Amd,   format!("AMD GPU ({})", driver)),
            "0x10de" => (GpuVendor::Nvidia, format!("NVIDIA GPU ({})", driver)),
            _        => (GpuVendor::Unknown, format!("Unknown GPU (vendor {})", vendor_id)),
        };

        return Some(GpuInfo { vendor, name: label });
    }
    None
}

// ── Swap / zram detection ─────────────────────────────────────────────────────

struct SwapInfo {
    has_zram: bool,
    devices: Vec<String>,
}

fn detect_swap() -> SwapInfo {
    let mut devices = Vec::new();
    let mut has_zram = false;

    if let Ok(content) = fs::read_to_string("/proc/swaps") {
        for line in content.lines().skip(1) {
            let dev = line.split_whitespace().next().unwrap_or("").to_string();
            if dev.contains("zram") { has_zram = true; }
            if !dev.is_empty() { devices.push(dev); }
        }
    }
    SwapInfo { has_zram, devices }
}

// ── Desktop environment detection ─────────────────────────────────────────────

struct DesktopInfo {
    de: String,
    compositor: String,
    session: String, // "wayland" or "x11"
}

fn detect_desktop() -> DesktopInfo {
    let de_env = std::env::var("XDG_CURRENT_DESKTOP")
        .or_else(|_| std::env::var("DESKTOP_SESSION"))
        .unwrap_or_else(|_| "unknown".to_string());

    let session = if std::env::var("WAYLAND_DISPLAY").is_ok() {
        "wayland".to_string()
    } else if std::env::var("DISPLAY").is_ok() {
        "x11".to_string()
    } else {
        "headless".to_string()
    };

    // Try to get real version from running process
    let plasma_version = get_version_from_proc("plasmashell");
    let gnome_version  = get_version_from_proc("gnome-shell");

    let (de, compositor) = if de_env.to_uppercase().contains("KDE") || plasma_version.is_some() {
        let ver = plasma_version.as_deref().unwrap_or("?");
        let comp = if session == "wayland" { "KWin Wayland" } else { "KWin X11" };
        (format!("KDE Plasma {ver}"), comp.to_string())
    } else if de_env.to_uppercase().contains("GNOME") || gnome_version.is_some() {
        let ver = gnome_version.as_deref().unwrap_or("?");
        let comp = if session == "wayland" { "Mutter (Wayland)" } else { "Mutter (X11)" };
        (format!("GNOME Shell {ver}"), comp.to_string())
    } else if de_env.to_uppercase().contains("COSMIC") {
        ("COSMIC DE".to_string(), "cosmic-comp".to_string())
    } else {
        (de_env.clone(), format!("unknown ({})", session))
    };

    DesktopInfo { de, compositor, session }
}

/// Try to read version from a running process's /proc/[pid]/status + cmdline.
fn get_version_from_proc(name: &str) -> Option<String> {
    let Ok(entries) = fs::read_dir("/proc") else { return None };
    for entry in entries.filter_map(|e| e.ok()) {
        let pid_str = entry.file_name().to_string_lossy().to_string();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) { continue }
        let comm = fs::read_to_string(format!("/proc/{pid_str}/comm"))
            .unwrap_or_default();
        if !comm.trim().starts_with(name) { continue }

        // Read the binary path and try --version
        let exe = fs::read_link(format!("/proc/{pid_str}/exe")).ok()?;
        let out = std::process::Command::new(&exe)
            .arg("--version")
            .output()
            .ok()?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let combined = format!("{stdout}{stderr}");
        // Extract version number from first line like "plasmashell 6.6.5"
        for line in combined.lines() {
            for token in line.split_whitespace() {
                if token.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                    return Some(token.to_string());
                }
            }
        }
        return Some("installed".to_string());
    }
    None
}

// ── PSI availability ──────────────────────────────────────────────────────────

fn check_psi() -> bool {
    Path::new("/proc/pressure/memory").exists()
}

// ── Plugin binary detection ───────────────────────────────────────────────────

struct PluginStatus {
    name: &'static str,
    binary: &'static str,
    running: bool,
    installed: bool,
}

fn detect_plugins() -> Vec<PluginStatus> {
    let bin_dir = mgd_common::util::home_dir().join(".local/bin");
    let plugins: &[(&str, &str)] = &[
        ("mgd-gpu-intel", "mgd-gpu-intel"),
        ("mgd-gpu-amd",   "mgd-gpu-amd"),
        ("mgd-kde",       "mgd-kde"),
        ("mgd-gnome",     "mgd-gnome"),
        ("mgd-cosmic",    "mgd-cosmic"),
        ("mgd-zram",      "mgd-zram"),
    ];
    plugins.iter().map(|(name, binary)| {
        let installed = bin_dir.join(binary).exists()
            || which(binary);
        let running = process_running(binary);
        PluginStatus { name, binary, running, installed }
    }).collect()
}

fn which(binary: &str) -> bool {
    std::process::Command::new("which")
        .arg(binary)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn process_running(name: &str) -> bool {
    let Ok(entries) = fs::read_dir("/proc") else { return false };
    for entry in entries.filter_map(|e| e.ok()) {
        let pid_str = entry.file_name().to_string_lossy().to_string();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) { continue }
        let comm = fs::read_to_string(format!("/proc/{pid_str}/comm"))
            .unwrap_or_default();
        if comm.trim() == name { return true; }
    }
    false
}

// ── Calibration status ────────────────────────────────────────────────────────

struct CalibrationInfo {
    calibrated_at: Option<String>,
    target_available_pct: Option<u32>,
    swap_onset_mb: Option<u64>,
    psi_recovery_secs: Option<u64>,
}

fn read_calibration() -> CalibrationInfo {
    let json_path = mgd_common::util::home_dir()
        .join(".local/share/mgd/calibration.json");
    let toml_path = mgd_common::util::home_dir()
        .join(".config/mgd/calibration.toml");

    let mut info = CalibrationInfo {
        calibrated_at: None,
        target_available_pct: None,
        swap_onset_mb: None,
        psi_recovery_secs: None,
    };

    // Read machine data from JSON
    if let Ok(data) = fs::read_to_string(&json_path) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) {
            info.calibrated_at    = v["calibrated_at"].as_str().map(|s| s.to_string());
            info.swap_onset_mb    = v["swap_onset_mb"].as_u64();
            info.psi_recovery_secs= v["psi_recovery_secs"].as_u64();
        }
    }

    // Read derived threshold from TOML suggestion
    if let Ok(data) = fs::read_to_string(&toml_path) {
        for line in data.lines() {
            if let Some(rest) = line.trim().strip_prefix("target_available_pct") {
                if let Some(val) = rest.split('=').nth(1) {
                    let num: String = val.chars()
                        .take_while(|c| c.is_ascii_digit() || *c == ' ')
                        .collect();
                    info.target_available_pct = num.trim().parse().ok();
                }
            }
        }
    }

    info
}

// ── Main entry ────────────────────────────────────────────────────────────────

pub fn run() -> i32 {
    let gpu     = detect_gpu();
    let swap    = detect_swap();
    let desktop = detect_desktop();
    let psi_ok  = check_psi();
    let plugins = detect_plugins();
    let cal     = read_calibration();

    println!("{}", bold("mgd doctor — environment report"));
    println!();

    // ── Environment ───────────────────────────────────────────────────────────
    println!("{}", bold("Environment:"));

    match &gpu {
        Some(g) => println!("  {:12} {}", "GPU:", g.name),
        None    => println!("  {:12} not detected", "GPU:"),
    }

    if swap.devices.is_empty() {
        println!("  {:12} none", "Swap:");
    } else {
        for dev in &swap.devices {
            println!("  {:12} {}", "Swap:", dev);
        }
    }

    println!("  {:12} {} ({})", "Desktop:", desktop.de, desktop.session);
    println!("  {:12} {}", "Compositor:", desktop.compositor);
    println!();

    // ── Core features ─────────────────────────────────────────────────────────
    println!("{}", bold("Core features:"));

    if psi_ok {
        println!("  {}", ok("PSI monitoring (/proc/pressure/memory)"));
    } else {
        println!("  {}", warn("PSI unavailable — kernel CONFIG_PSI not enabled"));
    }

    let daemon_running = process_running("mgd");
    if daemon_running {
        println!("  {}", ok("mgd daemon running"));
    } else {
        println!("  {}", warn("mgd daemon not running (start: systemctl --user start mgd)"));
    }
    println!();

    // ── Plugins ───────────────────────────────────────────────────────────────
    println!("{}", bold("Plugins:"));

    // Determine which plugins are relevant for this hardware
    let gpu_vendor_str = gpu.as_ref().map(|g| match g.vendor {
        GpuVendor::Intel  => "intel",
        GpuVendor::Amd    => "amd",
        GpuVendor::Nvidia => "nvidia",
        GpuVendor::Unknown => "unknown",
    }).unwrap_or("unknown");

    let de_lower = desktop.de.to_lowercase();

    for p in &plugins {
        // Determine if this plugin is applicable to this system
        let applicable = match p.binary {
            "mgd-gpu-intel"  => gpu_vendor_str == "intel",
            "mgd-gpu-amd"    => gpu_vendor_str == "amd",
            "mgd-kde"        => de_lower.contains("kde") || de_lower.contains("plasma"),
            "mgd-gnome"      => de_lower.contains("gnome"),
            "mgd-cosmic"     => de_lower.contains("cosmic"),
            "mgd-zram"       => swap.has_zram,
            _                => true,
        };

        let line = if !applicable {
            skip(&format!("{:<18} (not applicable on this system)", p.name))
        } else if p.running {
            ok(&format!("{:<18} running", p.name))
        } else if p.installed {
            warn(&format!("{:<18} installed but not running", p.name))
        } else {
            warn(&format!("{:<18} not installed (run ./install.sh)", p.name))
        };
        println!("  {line}");
    }
    println!();

    // ── Thresholds / calibration ──────────────────────────────────────────────
    println!("{}", bold("Thresholds:"));

    if let Some(ref ts) = cal.calibrated_at {
        println!("  Using calibration from timestamp {ts}");
        if let Some(pct) = cal.target_available_pct {
            println!("  {:30} {}%", "target_available_pct:", pct);
        }
        if let Some(mb) = cal.swap_onset_mb {
            println!("  {:30} {} MB", "swap_onset_mb:", mb);
        }
        if let Some(secs) = cal.psi_recovery_secs {
            println!("  {:30} {}s", "psi_recovery_secs:", secs);
        }
    } else {
        println!("  {}", warn("No calibration data — using built-in conservative defaults (15% target)"));
        println!("  Run: mgctl calibrate");
    }
    println!();

    // ── Privilege / caps ──────────────────────────────────────────────────────
    println!("{}", bold("Privileges:"));

    check_cap("criu",              "CRIU checkpoint/restore");
    check_cap("mgd-zram-reclaim",  "zram proactive reclaim (CAP_SYS_ADMIN)");

    println!();
    println!("{}", bold("To re-run after changes:  mgctl doctor"));

    0
}

fn check_cap(binary: &str, label: &str) {
    // Use getcap to check file capabilities
    let output = std::process::Command::new("getcap")
        .arg(format!("/usr/bin/{binary}"))
        .output()
        .or_else(|_| std::process::Command::new("getcap")
            .arg(mgd_common::util::home_dir().join(format!(".local/bin/{binary}")))
            .output());

    match output {
        Ok(o) if o.status.success() => {
            let caps = String::from_utf8_lossy(&o.stdout);
            let caps = caps.trim();
            if caps.is_empty() {
                println!("  {}", warn(&format!("{label} — no capabilities set")));
            } else {
                println!("  {}", ok(&format!("{label} — {}", caps.split_whitespace().last().unwrap_or("?"))));
            }
        }
        _ => println!("  {}", skip(&format!("{binary} not found"))),
    }
}
