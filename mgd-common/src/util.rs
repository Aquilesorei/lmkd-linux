use std::path::PathBuf;

/// Return the current user's home directory.
///
/// Uses `$HOME`; falls back to `/tmp` if unset (daemon safety net).
pub fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}
