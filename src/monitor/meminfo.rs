/// Returns (available_kb, total_kb) from /proc/meminfo.
pub fn read_meminfo() -> (u64, u64) {
    let Ok(content) = std::fs::read_to_string("/proc/meminfo") else {
        return (0, 0);
    };
    let mut total = 0u64;
    let mut available = 0u64;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total = v.split_whitespace().next().and_then(|s| s.parse().ok()).unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            available = v.split_whitespace().next().and_then(|s| s.parse().ok()).unwrap_or(0);
        }
    }
    (available, total)
}
