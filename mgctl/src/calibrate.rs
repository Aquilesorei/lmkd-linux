//! `mgctl calibrate` — controlled pressure sweep to derive per-machine thresholds.
//!
//! 3-phase protocol:
//!   Phase 1 — idle baseline (60s): record PSI + MemAvailable fingerprint
//!   Phase 2 — controlled sweep: +200 MB every 20s, stop at PSI inflection or swap spike
//!   Phase 3 — recovery curve (60s): watch PSI return to baseline
//!
//! Output: ~/.local/share/mgd/calibration.json   (machine data)
//!         ~/.config/mgd/calibration.toml         (user-reviewable suggested thresholds)
//!
//! Safety: never runs if battery < 30%, thermal throttling, or system already under PSI load.
//! All allocated memory is freed on SIGINT/SIGTERM before exit.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

// ── Safety guards ─────────────────────────────────────────────────────────────

/// Returns true if system is on battery with < 30% charge.
fn battery_low() -> bool {
    let Ok(entries) = fs::read_dir("/sys/class/power_supply") else { return false };
    for entry in entries.filter_map(|e| e.ok()) {
        let base = entry.path();
        let type_path = base.join("type");
        let Ok(typ) = fs::read_to_string(&type_path) else { continue };
        if typ.trim() != "Battery" { continue }

        let status = fs::read_to_string(base.join("status"))
            .unwrap_or_default();
        if status.trim() == "Discharging" {
            let cap: u32 = fs::read_to_string(base.join("capacity"))
                .unwrap_or_default()
                .trim()
                .parse()
                .unwrap_or(100);
            if cap < 30 {
                return true;
            }
        }
    }
    false
}

/// Returns true if any thermal zone is throttling.
fn thermal_throttling() -> bool {
    let Ok(entries) = fs::read_dir("/sys/class/thermal") else { return false };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path().join("throttle_count");
        if let Ok(val) = fs::read_to_string(&path)
            && val.trim().parse::<u64>().unwrap_or(0) > 0 {
                return true;
            }
    }
    false
}

// ── PSI + meminfo helpers ─────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct PsiSnapshot {
    some_avg10: f64,
    full_avg10: f64,
}

fn read_psi() -> Option<PsiSnapshot> {
    let content = fs::read_to_string("/proc/pressure/memory").ok()?;
    let mut snap = PsiSnapshot::default();
    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 { continue }
        let get = |prefix: &str| -> f64 {
            parts.iter()
                .find(|p| p.starts_with(prefix))
                .and_then(|p| mgd_common::psi::parse_kv(p, prefix).ok())
                .unwrap_or(0.0)
        };
        match parts.first().copied() {
            Some("some") => snap.some_avg10 = get("avg10="),
            Some("full") => snap.full_avg10 = get("avg10="),
            _ => {}
        }
    }
    Some(snap)
}

fn read_swap_in_kb() -> u64 {
    fs::read_to_string("/proc/vmstat").ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("pswpin "))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0)
}

// ── Controlled allocator (Phase 2) ────────────────────────────────────────────

/// A chunk of memory locked via mmap that can be released cleanly.
struct MemChunk {
    ptr: *mut libc::c_void,
    size: usize,
}

unsafe impl Send for MemChunk {}

impl MemChunk {
    /// Allocate `size` bytes of anonymous memory and populate it (MADV_POPULATE_READ).
    fn alloc(size: usize) -> Option<Self> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED { return None; }

        // Touch each page to actually consume physical RAM.
        // MADV_POPULATE_READ is Linux 5.14+; fall back to manual touch if unavailable.
        let ret = unsafe { libc::madvise(ptr, size, libc::MADV_POPULATE_READ) };
        if ret != 0 {
            // Fallback: write one byte per page.
            let page = 4096usize;
            let mut cursor = ptr as usize;
            let end = cursor + size;
            while cursor < end {
                unsafe { *(cursor as *mut u8) = 1 };
                cursor += page;
            }
        }
        Some(MemChunk { ptr, size })
    }
}

impl Drop for MemChunk {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr, self.size); }
    }
}

// ── Output paths ──────────────────────────────────────────────────────────────

fn json_path() -> PathBuf {
    mgd_common::util::home_dir()
        .join(".local/share/mgd/calibration.json")
}

fn toml_path() -> PathBuf {
    mgd_common::util::home_dir()
        .join(".config/mgd/calibration.toml")
}

// ── Main entry ────────────────────────────────────────────────────────────────

pub fn run(args: &[String]) -> i32 {
    let dry_run        = args.iter().any(|a| a == "--dry-run");
    let apply          = args.iter().any(|a| a == "--apply");
    let passive_apply  = args.iter().any(|a| a == "--passive-apply");

    if apply         { return do_apply(); }
    if passive_apply { return do_passive_apply(); }

    // ── Pre-flight safety checks ──────────────────────────────────────────────
    if battery_low() {
        eprintln!("mgctl calibrate: battery < 30% — refusing to run. Plug in first.");
        return 1;
    }
    if thermal_throttling() {
        eprintln!("mgctl calibrate: thermal throttling detected — refusing to run.");
        return 1;
    }
    let Ok(baseline_check) = read_psi().ok_or("cannot read PSI") else {
        eprintln!("mgctl calibrate: /proc/pressure/memory unavailable — is CONFIG_PSI enabled?");
        return 1;
    };
    if baseline_check.some_avg10 > 2.0 {
        eprintln!(
            "mgctl calibrate: system already under pressure (PSI some_avg10={:.1}%) — wait for idle.",
            baseline_check.some_avg10
        );
        return 1;
    }

    // Install signal handler so Ctrl-C releases all memory before exit.
    unsafe {
        libc::signal(libc::SIGINT,  handle_interrupt as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handle_interrupt as *const () as libc::sighandler_t);
    }

    println!("mgctl calibrate: pre-flight OK");
    println!();

    // ── Phase 1: idle baseline ────────────────────────────────────────────────
    println!("Phase 1/3 — idle baseline (60s)...");
    let mut psi_samples: Vec<f64> = Vec::new();
    let mut swap_samples: Vec<u64> = Vec::new();

    for i in 0..12 {
        if INTERRUPTED.load(Ordering::Relaxed) { return cleanup_interrupted(); }
        thread::sleep(Duration::from_secs(5));
        if let Some(p) = read_psi() {
            psi_samples.push(p.full_avg10);
        }
        swap_samples.push(read_swap_in_kb());
        print!("  [{:>2}/12] PSI full_avg10={:.2}%  MemAvailable={:.0}MB\r",
            i + 1,
            psi_samples.last().copied().unwrap_or(0.0),
            mgd_common::meminfo::read_available_kb() as f64 / 1024.0);
        let _ = std::io::stdout().flush();
    }
    println!();

    let baseline_psi_full = psi_samples.iter().copied().sum::<f64>() / psi_samples.len().max(1) as f64;
    let baseline_psi_some = read_psi().map(|p| p.some_avg10).unwrap_or(0.0);
    let baseline_swap_in  = *swap_samples.last().unwrap_or(&0);
    let total_kb          = mgd_common::meminfo::read_total_kb();

    println!("  Baseline: PSI full_avg10={:.2}%  some_avg10={:.2}%  RAM={:.0}MB",
        baseline_psi_full, baseline_psi_some, total_kb as f64 / 1024.0);
    println!();

    // ── Phase 2: controlled pressure sweep ───────────────────────────────────
    println!("Phase 2/3 — pressure sweep (+200 MB every 20s)...");
    println!("  Press Ctrl-C at any time to abort safely.");

    const STEP_MB: usize     = 200;
    const STEP_BYTES: usize  = STEP_MB * 1024 * 1024;
    const MAX_STEPS: usize   = 30;      // cap at 6 GB total to be safe

    let mut chunks: Vec<MemChunk>  = Vec::new();
    let mut allocated_mb           = 0usize;
    let mut swap_onset_mb: Option<u64> = None;
    let mut stop_psi_full          = 0.0f64;

    'sweep: for step in 0..MAX_STEPS {
        if INTERRUPTED.load(Ordering::Relaxed) {
            drop(chunks);
            return cleanup_interrupted();
        }

        // Allocate next chunk.
        match MemChunk::alloc(STEP_BYTES) {
            Some(chunk) => {
                chunks.push(chunk);
                allocated_mb += STEP_MB;
            }
            None => {
                println!("  mmap failed at {allocated_mb}MB — stopping sweep.");
                break;
            }
        }

        // Wait and sample.
        for tick in 0..4 { // 4 × 5s = 20s
            thread::sleep(Duration::from_secs(5));
            if INTERRUPTED.load(Ordering::Relaxed) {
                drop(chunks);
                return cleanup_interrupted();
            }
            let cur_psi  = read_psi().unwrap_or_default();
            let cur_swap = read_swap_in_kb();
            let swap_rate = cur_swap.saturating_sub(baseline_swap_in);
            print!("  [step {:>2} tick {}/4] allocated={:>5}MB  PSI full={:.2}%  swap_in_delta={}KB\r",
                step + 1, tick + 1, allocated_mb,
                cur_psi.full_avg10, swap_rate);
            let _ = std::io::stdout().flush();

            // STOP conditions
            if cur_psi.full_avg10 > 15.0 {
                println!();
                println!("  ⚠ PSI full_avg10={:.2}% > 15% — inflection point reached.", cur_psi.full_avg10);
                stop_psi_full = cur_psi.full_avg10;
                break 'sweep;
            }
            if swap_rate > 50_000 { // 50 MB of swap-in delta
                println!();
                println!("  ⚠ Swap-in spike detected ({} KB delta) — onset point reached.", swap_rate);
                swap_onset_mb = Some(allocated_mb as u64);
                break 'sweep;
            }
        }
    }

    let swap_onset_mb = swap_onset_mb.unwrap_or(allocated_mb as u64);
    println!();
    println!("  Sweep complete: onset at ~{}MB allocated  PSI_full_at_stop={:.2}%",
        swap_onset_mb, stop_psi_full);

    // Release all allocated memory immediately.
    drop(chunks);
    println!("  Memory released.");
    println!();

    // ── Phase 3: recovery curve ───────────────────────────────────────────────
    println!("Phase 3/3 — recovery observation (up to 60s)...");
    let recovery_start = now_secs();
    let mut psi_recovered_secs = 60u64;

    for _ in 0..12 {
        thread::sleep(Duration::from_secs(5));
        if INTERRUPTED.load(Ordering::Relaxed) { return cleanup_interrupted(); }
        let cur = read_psi().unwrap_or_default();
        print!("  PSI full_avg10={:.2}%  (baseline was {:.2}%)\r",
            cur.full_avg10, baseline_psi_full);
        let _ = std::io::stdout().flush();
        // "recovered" when within 110% of baseline
        if cur.full_avg10 <= baseline_psi_full * 1.1 + 0.1 {
            psi_recovered_secs = now_secs() - recovery_start;
            println!();
            println!("  ✓ PSI returned to baseline in {}s", psi_recovered_secs);
            break;
        }
    }
    println!();

    // ── Derive thresholds ─────────────────────────────────────────────────────
    // target_available_pct: swap_onset / total_ram + 3% safety headroom, clamped 10–35%.
    let total_mb = total_kb / 1024;
    let raw_pct = if total_mb > 0 {
        (swap_onset_mb as f64 / total_mb as f64 * 100.0 + 3.0).round() as u32
    } else { 18 };
    let target_available_pct = raw_pct.clamp(10, 35);

    println!("─────────────────────────────────────────────────");
    println!("Calibration results:");
    println!("  total_ram_mb          = {}", total_mb);
    println!("  swap_onset_mb         = {}", swap_onset_mb);
    println!("  psi_recovery_secs     = {}", psi_recovered_secs);
    println!("  baseline_psi_full     = {:.2}%", baseline_psi_full);
    println!("  baseline_psi_some     = {:.2}%", baseline_psi_some);
    println!("  → target_available_pct= {}%  (was 15% conservative default)", target_available_pct);
    println!("─────────────────────────────────────────────────");

    if dry_run {
        println!("\n[dry-run] No files written.");
        return 0;
    }

    // ── Write outputs ─────────────────────────────────────────────────────────
    let ts = chrono_now();

    let json = serde_json::json!({
        "calibrated_at":          ts,
        "total_ram_mb":           total_mb,
        "swap_onset_mb":          swap_onset_mb,
        "psi_recovery_secs":      psi_recovered_secs,
        "baseline_psi_some_avg10": baseline_psi_some,
        "baseline_psi_full_avg10": baseline_psi_full,
    });

    let json_out = json_path();
    if let Some(parent) = json_out.parent() { let _ = fs::create_dir_all(parent); }
    match fs::write(&json_out, serde_json::to_string_pretty(&json).unwrap()) {
        Ok(_)  => println!("Wrote machine data → {}", json_out.display()),
        Err(e) => eprintln!("Warning: could not write {}: {e}", json_out.display()),
    }

    let toml = format!(
        "# Generated by mgctl calibrate ({ts})\n\
         # Review these values, then run: mgctl calibrate --apply\n\
         # They will replace the built-in conservative defaults in the daemon.\n\
         \n\
         [thresholds]\n\
         target_available_pct = {target_available_pct:<6}  # swap onset was at {swap_onset_mb}MB / {total_mb}MB RAM + 3% headroom\n\
         psi_recovery_secs    = {psi_recovered_secs:<6}  # seconds PSI took to return to baseline after load\n"
    );

    let toml_out = toml_path();
    if let Some(parent) = toml_out.parent() { let _ = fs::create_dir_all(parent); }
    match fs::write(&toml_out, &toml) {
        Ok(_)  => println!("Wrote suggested config → {}", toml_out.display()),
        Err(e) => eprintln!("Warning: could not write {}: {e}", toml_out.display()),
    }

    println!();
    println!("Review the suggested config, then apply with:");
    println!("  mgctl calibrate --apply");
    0
}

// ── --passive-apply ───────────────────────────────────────────────────────────

/// Parse first non-commented `key = <number>` value from flat TOML text.
fn suggestion_value(data: &str, key: &str) -> Option<f64> {
    data.lines().find_map(|line| {
        let line = line.trim();
        if line.starts_with('#') { return None; }
        let rest = line.strip_prefix(key)?.trim_start();
        let val = rest.strip_prefix('=')?;
        val.split('#').next()?.trim().parse().ok()
    })
}

/// Update or add a `[psi]` block in priorities.toml content (string-level patch,
/// preserves all other content and comments).
fn patch_psi_block(content: &str, elevated_pct: f64, full_critical_pct: f64) -> String {
    let has_psi = content.lines().any(|l| l.trim() == "[psi]");

    if !has_psi {
        let sep = if content.is_empty() || content.ends_with('\n') { "" } else { "\n" };
        return format!(
            "{content}{sep}\n[psi]\nelevated_pct      = {elevated_pct:.1}\nfull_critical_pct = {full_critical_pct:.1}\n"
        );
    }

    let mut result: Vec<String> = Vec::new();
    let mut in_psi = false;
    let mut found_elevated = false;
    let mut found_full = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if in_psi {
                if !found_elevated {
                    result.push(format!("elevated_pct      = {elevated_pct:.1}"));
                }
                if !found_full {
                    result.push(format!("full_critical_pct = {full_critical_pct:.1}"));
                }
            }
            in_psi = trimmed == "[psi]";
            result.push(line.to_string());
            continue;
        }

        if in_psi && !trimmed.starts_with('#') {
            if trimmed.starts_with("elevated_pct") && trimmed.contains('=') {
                result.push(format!("elevated_pct      = {elevated_pct:.1}"));
                found_elevated = true;
                continue;
            }
            if trimmed.starts_with("full_critical_pct") && trimmed.contains('=') {
                result.push(format!("full_critical_pct = {full_critical_pct:.1}"));
                found_full = true;
                continue;
            }
        }

        result.push(line.to_string());
    }

    if in_psi {
        if !found_elevated {
            result.push(format!("elevated_pct      = {elevated_pct:.1}"));
        }
        if !found_full {
            result.push(format!("full_critical_pct = {full_critical_pct:.1}"));
        }
    }

    result.join("\n") + "\n"
}

fn do_passive_apply() -> i32 {
    let sug_path = mgd_common::util::home_dir()
        .join(".local/share/mgd/calibration_suggestion.toml");

    let sug_data = match fs::read_to_string(&sug_path) {
        Ok(d) => d,
        Err(_) => {
            eprintln!("mgctl calibrate --passive-apply: no suggestion at {}", sug_path.display());
            eprintln!("Run mgd for ≥24h with ≥10 stall episodes first, then check: mgctl doctor");
            return 1;
        }
    };

    let elevated_pct = match suggestion_value(&sug_data, "elevated_pct") {
        Some(v) => v,
        None => {
            eprintln!("mgctl calibrate --passive-apply: could not parse elevated_pct");
            return 1;
        }
    };
    let full_critical_pct = match suggestion_value(&sug_data, "full_critical_pct") {
        Some(v) => v,
        None => {
            eprintln!("mgctl calibrate --passive-apply: could not parse full_critical_pct");
            return 1;
        }
    };

    let config_path = mgd_common::util::home_dir().join(".config/mgd/priorities.toml");
    if let Some(parent) = config_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let existing = fs::read_to_string(&config_path).unwrap_or_default();
    let patched = patch_psi_block(&existing, elevated_pct, full_critical_pct);

    if let Err(e) = fs::write(&config_path, &patched) {
        eprintln!("mgctl calibrate --passive-apply: write failed: {e}");
        return 1;
    }

    println!("Applied [psi] suggestion → {}", config_path.display());
    println!("  elevated_pct      = {elevated_pct:.1}");
    println!("  full_critical_pct = {full_critical_pct:.1}");
    println!("  Note: elevated_pct re-arms the kernel trigger only on daemon restart.");

    match crate::query_socket("reload", 5) {
        Ok(_) => println!("Daemon reloaded."),
        Err(_) => println!("Daemon not running — changes take effect on next start."),
    }
    0
}

// ── --apply ───────────────────────────────────────────────────────────────────

fn do_apply() -> i32 {
    let src = toml_path();
    if !src.exists() {
        eprintln!("mgctl calibrate --apply: no calibration.toml found at {}", src.display());
        eprintln!("Run 'mgctl calibrate' first.");
        return 1;
    }

    let cfg_dir = mgd_common::util::home_dir().join(".config/mgd");
    let _ = fs::create_dir_all(&cfg_dir);

    // Append or create a calibration override in the config dir.
    // We write a separate file that mgd can include — the daemon reads priorities.toml;
    // the calibration suggestions show users what to paste if they want to override.
    println!("Calibration file is at: {}", src.display());
    println!();
    println!("To apply, paste the [thresholds] block into ~/.config/mgd/priorities.toml");
    println!("and run: mgctl reload");
    println!();
    println!("(Full auto-merge will be implemented in a future version.)");
    0
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn now_secs() -> u64 {
    mgd_common::util::unix_timestamp_secs()
}

fn chrono_now() -> String {
    let secs = now_secs();
    format!("{}", secs) // good enough for the JSON timestamp; no chrono dep needed
}

fn cleanup_interrupted() -> i32 {
    eprintln!("\nmgctl calibrate: interrupted — all memory freed. No files written.");
    1
}

extern "C" fn handle_interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestion_value_parses_plain() {
        let data = "elevated_pct = 5.5\nfull_critical_pct = 20.0\n";
        assert_eq!(suggestion_value(data, "elevated_pct"), Some(5.5));
        assert_eq!(suggestion_value(data, "full_critical_pct"), Some(20.0));
    }

    #[test]
    fn suggestion_value_ignores_commented_lines() {
        let data = "#elevated_pct = 99.0\nelevated_pct = 5.5\n";
        assert_eq!(suggestion_value(data, "elevated_pct"), Some(5.5));
    }

    #[test]
    fn suggestion_value_strips_inline_comment() {
        let data = "elevated_pct      = 5.5    # current: 5.0\n";
        assert_eq!(suggestion_value(data, "elevated_pct"), Some(5.5));
    }

    #[test]
    fn suggestion_value_returns_none_when_absent() {
        assert_eq!(suggestion_value("other = 1.0\n", "elevated_pct"), None);
    }

    #[test]
    fn patch_psi_block_appends_when_no_section() {
        let config = "[priorities]\nsome = 1\n";
        let result = patch_psi_block(config, 5.5, 20.0);
        assert!(result.contains("[psi]"), "should have [psi]: {result}");
        assert!(result.contains("elevated_pct      = 5.5"));
        assert!(result.contains("full_critical_pct = 20.0"));
        assert!(result.contains("[priorities]"));
    }

    #[test]
    fn patch_psi_block_updates_existing_values() {
        let config = "[psi]\nelevated_pct = 3.0\nfull_critical_pct = 15.0\n";
        let result = patch_psi_block(config, 5.5, 20.0);
        assert!(result.contains("elevated_pct      = 5.5"));
        assert!(result.contains("full_critical_pct = 20.0"));
        assert!(!result.contains("= 3.0"));
        assert!(!result.contains("= 15.0"));
    }

    #[test]
    fn patch_psi_block_inserts_missing_keys_into_existing_section() {
        let config = "[psi]\n";
        let result = patch_psi_block(config, 5.5, 20.0);
        assert!(result.contains("elevated_pct      = 5.5"));
        assert!(result.contains("full_critical_pct = 20.0"));
    }

    #[test]
    fn patch_psi_block_empty_config() {
        let result = patch_psi_block("", 5.5, 20.0);
        assert!(result.contains("[psi]"));
        assert!(result.contains("elevated_pct      = 5.5"));
        assert!(result.contains("full_critical_pct = 20.0"));
    }

    #[test]
    fn patch_psi_block_preserves_other_sections() {
        let config = "[zram]\ncompact_on_elevated = true\n\n[psi]\nelevated_pct = 3.0\n\n[idle_reclaim]\nenabled = false\n";
        let result = patch_psi_block(config, 6.0, 22.0);
        assert!(result.contains("[zram]"));
        assert!(result.contains("[idle_reclaim]"));
        assert!(result.contains("elevated_pct      = 6.0"));
        assert!(result.contains("full_critical_pct = 22.0"));
    }
}
