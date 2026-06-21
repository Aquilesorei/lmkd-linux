/// `/dev/zram` followed by digits — rejects partitions and lookalike swapfiles.
pub fn is_zram_device_path(path: &str) -> bool {
    match path.strip_prefix("/dev/zram") {
        Some(rest) => !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_zram_device_path() {
        assert!(is_zram_device_path("/dev/zram0"));
        assert!(is_zram_device_path("/dev/zram10"));
        assert!(is_zram_device_path("/dev/zram12"));
        assert!(!is_zram_device_path("/dev/zram"));        // no number
        assert!(!is_zram_device_path("/dev/zrama"));       // non-digit suffix
        assert!(!is_zram_device_path("/dev/zram0x"));      // trailing non-digit
        assert!(!is_zram_device_path("/swap/zram-cache")); // not under /dev
        assert!(!is_zram_device_path("/dev/nvme0n1p3"));   // real disk
    }
}
