//! mgd-gpu-intel — Intel Iris Xe / UMA fdinfo GPU residency watcher.
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::io::Write;
use std::thread;
use std::time::{Duration, Instant};

const PLUGIN_NAME: &str = "mgd-gpu-intel";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const FULL_SCAN_INTERVAL: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_secs(5);

fn send_obs(writer: &mut impl Write, pid: u32, stats: &mgd_common::gpu::SingleProcessGpuMemory) {
    mgd_common::gpu::send_gpu_stats(writer, PLUGIN_NAME, mgd_common::types::Pid(pid), stats);
}

fn main() {
    let stream = mgd_common::plugin::connect_and_identify(PLUGIN_NAME, VERSION, vec!["gpu_residency"]);
    let mut writer = stream.try_clone().expect("clone stream");

    thread::spawn(move || {
        mgd_common::plugin::drain_lines(stream, |_| {});
        std::process::exit(0);
    });

    let own_uid = mgd_common::util::current_uid();
    // PIDs known to have GPU pages — only these are checked on fast cycles.
    let mut known_gpu_pids: HashSet<u32> = HashSet::new();
    // Force a full /proc scan on the first cycle.
    let mut last_full_scan = Instant::now()
        .checked_sub(FULL_SCAN_INTERVAL)
        .unwrap_or_else(Instant::now);

    loop {
        let do_full = last_full_scan.elapsed() >= FULL_SCAN_INTERVAL;

        if do_full {
            last_full_scan = Instant::now();
            let mut new_known: HashSet<u32> = HashSet::new();

            if let Ok(entries) = fs::read_dir("/proc") {
                for entry in entries.filter_map(|e| e.ok()) {
                    let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
                    let Ok(meta) = fs::metadata(entry.path()) else { continue };
                    if meta.uid() != own_uid { continue; }

                    if let Some(stats) = mgd_common::gpu::get_process_gpu_stats(pid) {
                        if stats.resident_kb > 0 { new_known.insert(pid); }
                        send_obs(&mut writer, pid, &stats);
                    }
                }
            }
            known_gpu_pids = new_known;
        } else {
            // Fast path: only probe PIDs that previously had GPU pages.
            known_gpu_pids.retain(|&pid| {
                match mgd_common::gpu::get_process_gpu_stats(pid) {
                    Some(stats) => { send_obs(&mut writer, pid, &stats); stats.resident_kb > 0 }
                    None => false,
                }
            });
        }

        thread::sleep(POLL_INTERVAL);
    }
}
