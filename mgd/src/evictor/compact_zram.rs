use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use mgd_common::util::unix_timestamp_secs;
use mgd_common::logger::{LogAction, Logger};

use crate::config::CompiledConfig;
use crate::monitor::psi::PressureLevel;

/// Set once on zram-compact EACCES (grant absent) to log only once per session.
static ZRAM_COMPACT_DISABLED: AtomicBool = AtomicBool::new(false);

/// Unix-seconds of the last successful zram compaction (0 = never).
static LAST_ZRAM_COMPACT: AtomicU64 = AtomicU64::new(0);

pub(super) fn gate_state() -> (bool, u64) {
    (ZRAM_COMPACT_DISABLED.load(Ordering::Relaxed), LAST_ZRAM_COMPACT.load(Ordering::Relaxed))
}

pub(crate) fn compact_zram(level: &PressureLevel, log: &Logger, cfg: &CompiledConfig) {
    if *level < PressureLevel::Elevated {
        return;
    }
    if ZRAM_COMPACT_DISABLED.load(Ordering::Relaxed) {
        return;
    }
    if !cfg.compact_zram_on_elevated {
        return;
    }
    let min_used_mb = cfg.zram_min_used_mb;

    for device in crate::monitor::zram::zram_devices() {
        // Gate before compacting; skip a device whose used-RAM is unreadable.
        let Some(before_mb) = crate::monitor::zram::zram_used_mb(&device) else { continue };
        if before_mb < min_used_mb {
            continue;
        }

        match crate::monitor::zram::compact(&device) {
            Ok(()) => {
                let after_mb = crate::monitor::zram::zram_used_mb(&device).unwrap_or(before_mb);
                let reclaimed = before_mb.saturating_sub(after_mb);
                mgd_common::sync_print!(
                    "[zram] compacted {device} — {before_mb}MB→{after_mb}MB used ({reclaimed}MB reclaimed)"
                );
                log.log_system(LogAction::Zram, &device, reclaimed as f64,
                    &format!("compacted {before_mb}MB->{after_mb}MB"));
                LAST_ZRAM_COMPACT.store(unix_timestamp_secs(), Ordering::Relaxed);
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                ZRAM_COMPACT_DISABLED.store(true, Ordering::Relaxed);
                mgd_common::sync_print!(
                    "[zram] compact unavailable ({device}): sysfs grant absent — disabling for \
                     session. See docs/PRIVILEGE_DESIGN.md §1."
                );
                log.log_system(LogAction::Zram, &device, 0.0, "unavailable: EACCES (grant absent)");
                return;
            }
            Err(e) => {
                mgd_common::sync_print!("[zram] compact failed on {device}: {e}");
            }
        }
    }
}
