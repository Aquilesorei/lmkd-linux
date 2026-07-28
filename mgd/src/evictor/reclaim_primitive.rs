pub(crate) enum ReclaimOutcome {
    Reclaimed,
    Skipped,
    Blocked,
    Failed(std::io::Error),
}

fn is_cgroup_leaf(cgroup_path: &str) -> bool {
    let sysfs_dir = crate::throttle::cgroup_sysfs_path(cgroup_path, "");
    std::fs::read_dir(&sysfs_dir)
        .map(|entries| {
            !entries
                .filter_map(|e| e.ok())
                .any(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
        })
        .unwrap_or(true) // fail open: can't read → assume leaf, attempt reclaim
}

pub(crate) fn reclaim_cgroup(cgroup_path: &str, bytes_size: u64) -> Result<bool, std::io::Error> {

    if bytes_size == 0 {
        return Ok(false);
    }

    if cgroup_path == "/" || cgroup_path.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cannot reclaim root cgroup",
        ));
    }
    if !is_cgroup_leaf(cgroup_path) {
        return Ok(false);
    }
    let reclaim_path = crate::throttle::cgroup_sysfs_path(cgroup_path, "memory.reclaim");
    if reclaim_path.exists() {
        match std::fs::write(&reclaim_path, format!("{}", bytes_size)) {
            Ok(()) => return Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(false),
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, "cgroup memory.reclaim not found"))
}

pub(crate) fn try_reclaim_cgroup(cgroup_path: &str, bytes_size: u64) -> ReclaimOutcome {
    match reclaim_cgroup(cgroup_path, bytes_size) {
        Ok(true) => ReclaimOutcome::Reclaimed,
        Ok(false) => ReclaimOutcome::Skipped,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => ReclaimOutcome::Blocked,
        Err(e) => ReclaimOutcome::Failed(e),
    }
}
