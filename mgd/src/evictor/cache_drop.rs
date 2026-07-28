use std::sync::atomic::{AtomicU64, Ordering};
use mgd_common::util::unix_timestamp_secs;
use mgd_common::logger::{LogAction, Logger};

use crate::config::CompiledConfig;
use crate::monitor::psi::PressureLevel;

/// Unix-seconds of the last page-cache drop (0 = never).
static LAST_CACHE_DROP: AtomicU64 = AtomicU64::new(0);

pub(super) fn gate_state() -> u64 {
    LAST_CACHE_DROP.load(Ordering::Relaxed)
}

pub(crate) fn check_cache_drop(level: &PressureLevel, log: &Logger, cfg: &CompiledConfig) {
    if !cfg.cache_drop_enabled || cfg.cache_drop_paths.is_empty() {
        return;
    }
    if *level < cfg.cache_drop_trigger {
        return;
    }

    let now = unix_timestamp_secs();
    let last = LAST_CACHE_DROP.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < cfg.cache_drop_cooldown_secs {
        return;
    }
    // Arm up-front: the walk is the cost being rate-limited.
    LAST_CACHE_DROP.store(now, Ordering::Relaxed);

    let mut total_files = 0usize;
    let mut total_bytes = 0u64;
    for r in crate::monitor::cache::drop_caches(&cfg.cache_drop_paths) {
        if r.files_advised > 0 {
            log.log_system(LogAction::Cache, &r.pattern,
                (r.bytes_advised / (1024 * 1024)) as f64,
                &format!("advised {} files", r.files_advised));
        }
        total_files += r.files_advised;
        total_bytes += r.bytes_advised;
    }

    if total_files > 0 {
        let mb = total_bytes / (1024 * 1024);
        mgd_common::sync_print!("[cache] dropped cache for {total_files} files (~{mb}MB advised) before freeze");
    }
}
