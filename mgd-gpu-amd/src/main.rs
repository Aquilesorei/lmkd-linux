//! mgd-gpu-amd — AMD APU / UMA GPU residency watcher plugin for mgd.
//!
//! Uses the same DRM fdinfo accounting as mgd-gpu-intel; the kernel interface
//! is driver-independent (drm-client-id + drm-resident-* fields).
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::thread;
use std::time::Duration;
use mgd_common::protocol::{Metric, PluginMessage};

const PLUGIN_NAME: &str = "mgd-gpu-amd";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let stream = mgd_common::plugin::connect_and_identify(PLUGIN_NAME, VERSION, vec!["gpu_residency"]);

    let mut writer = stream.try_clone().expect("clone stream");

    thread::spawn(move || {
        mgd_common::plugin::drain_lines(stream, |_| {});
        std::process::exit(0);
    });

    loop {
        if let Ok(entries) = fs::read_dir("/proc") {
            let own_uid = mgd_common::util::current_uid();
            for entry in entries.filter_map(|e| e.ok()) {
                let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };

                let Ok(meta) = fs::metadata(entry.path()) else { continue };
                if meta.uid() != own_uid { continue; }

                if let Some(gpu_kb) = mgd_common::gpu::process_gpu_kb(pid) {
                    let obs = PluginMessage::Observation {
                        plugin: PLUGIN_NAME.to_string(),
                        metric: Metric::GpuResidentKb,
                        pid: Some(pid),
                        value: gpu_kb as f64,
                    };
                    let _ = writeln!(writer, "{}", serde_json::to_string(&obs).unwrap());
                }
            }
        }
        thread::sleep(Duration::from_secs(5));
    }
}
