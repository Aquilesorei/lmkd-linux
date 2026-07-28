use std::path::PathBuf;

pub(super) fn try_user_config() -> Option<(String, Option<PathBuf>)> {
    if cfg!(test) {
        return None;
    }
    let path = mgd_common::util::home_dir().join(".config/mgd/priorities.toml");
    let content = std::fs::read_to_string(&path).ok()?;
    Some((content, Some(path)))
}

pub(super) fn try_system_config() -> Option<(String, Option<PathBuf>)> {
    if cfg!(test) {
        return None;
    }
    let path = PathBuf::from("/etc/mgd/priorities.toml");
    let content = std::fs::read_to_string(&path).ok()?;
    Some((content, Some(path)))
}
