//! mgd-gpu-intel — Intel Iris Xe / UMA fdinfo GPU residency watcher.
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::io::{BufRead, BufReader, Write};
use std::thread;
use std::time::Duration;
use mgd_common::protocol::{Metric, PluginMessage};

const PLUGIN_NAME: &str = "mgd-gpu-intel";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let stream = mgd_common::plugin::connect_and_identify(PLUGIN_NAME, VERSION, vec!["gpu_residency"]);

    let mut writer = stream.try_clone().expect("clone stream");
    
    // Background reader just to drain socket so buffer doesn't fill up
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        while reader.read_line(&mut line).is_ok() && !line.is_empty() {
            line.clear();
        }
        std::process::exit(0);
    });

    loop {
        if let Ok(entries) = fs::read_dir("/proc") {
            let own_uid = unsafe { libc::geteuid() };
            for entry in entries.filter_map(|e| e.ok()) {
                let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
                
                // Only scan our own processes
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

// process_gpu_kb and parse_mem_kb are now in mgd_common::gpu
