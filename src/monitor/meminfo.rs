pub struct MemInfo {
    pub available_kb: u64,
    pub total_kb: u64,
    pub swap_free_kb: u64,
    pub swap_total_kb: u64,
}

impl MemInfo {
    pub fn swap_used_pct(&self) -> f64 {
        if self.swap_total_kb == 0 { return 0.0; }
        (self.swap_total_kb - self.swap_free_kb) as f64 / self.swap_total_kb as f64 * 100.0
    }
}

/// Returns memory info from /proc/meminfo.
pub fn read_meminfo() -> MemInfo {
    let Ok(content) = std::fs::read_to_string("/proc/meminfo") else {
        return MemInfo { available_kb: 0, total_kb: 0, swap_free_kb: 0, swap_total_kb: 0 };
    };
    let mut total = 0u64;
    let mut available = 0u64;
    let mut swap_total = 0u64;
    let mut swap_free = 0u64;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total = parse_kb(v);
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            available = parse_kb(v);
        } else if let Some(v) = line.strip_prefix("SwapTotal:") {
            swap_total = parse_kb(v);
        } else if let Some(v) = line.strip_prefix("SwapFree:") {
            swap_free = parse_kb(v);
        }
    }
    MemInfo { available_kb: available, total_kb: total, swap_free_kb: swap_free, swap_total_kb: swap_total }
}

fn parse_kb(s: &str) -> u64 {
    s.split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0)
}
