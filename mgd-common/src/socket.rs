use std::path::PathBuf;

/// Compute the XDG runtime socket path for the mgd daemon.
///
/// Precedence:
/// 1. `$XDG_RUNTIME_DIR/mgd.sock`
/// 2. `/tmp/mgd-<uid>.sock` (fallback when XDG_RUNTIME_DIR is unset)
pub fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("mgd.sock");
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/mgd-{uid}.sock"))
}
