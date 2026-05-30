use std::path::PathBuf;

/// Socket path shared by the daemon (ipc.rs) and the control client (mgctl).
pub fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("mgd.sock");
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/mgd-{uid}.sock"))
}
