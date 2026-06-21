pub fn parse_kb(s: &str) -> u64 {
    s.split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0)
}

pub fn read_available_kb() -> u64 {
    read_meminfo_field("MemAvailable:")
}

pub fn read_total_kb() -> u64 {
    read_meminfo_field("MemTotal:")
}

fn read_meminfo_field(prefix: &str) -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(prefix))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0)
}
