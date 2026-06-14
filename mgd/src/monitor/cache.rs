//! Page-cache drop via `posix_fadvise(DONTNEED)` over configured dir trees,
//! run before freezing apps at High+ pressure.
//!
//! DONTNEED drops clean pages immediately and leaves dirty pages for the kernel
//! to write back first, so no data is lost. Paths take a leading `~` and one `*`
//! per segment; expansion is hand-rolled to avoid a glob dependency.

use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

const MAX_DEPTH: usize = 8;
/// Global across all patterns in one `drop_caches` call — bounds inline work.
const MAX_FILES: usize = 50_000;

pub struct CacheDropResult {
    pub pattern: String,
    pub files_advised: usize,
    pub bytes_advised: u64,
}

pub fn drop_caches(patterns: &[String]) -> Vec<CacheDropResult> {
    let mut budget = MAX_FILES;
    patterns
        .iter()
        .map(|pattern| {
            let mut files_advised = 0usize;
            let mut bytes_advised = 0u64;
            for resolved in expand_path(pattern) {
                if budget == 0 {
                    break;
                }
                advise_tree(&resolved, 0, &mut budget, &mut files_advised, &mut bytes_advised);
            }
            CacheDropResult {
                pattern: pattern.clone(),
                files_advised,
                bytes_advised,
            }
        })
        .collect()
}

fn advise_tree(
    path: &Path,
    depth: usize,
    budget: &mut usize,
    files: &mut usize,
    bytes: &mut u64,
) {
    if depth > MAX_DEPTH || *budget == 0 {
        return;
    }

    let Ok(meta) = fs::symlink_metadata(path) else { return };
    let ft = meta.file_type();

    // Don't follow symlinks: they could point outside the configured tree.
    if ft.is_symlink() {
        return;
    }

    if ft.is_file() {
        if let Some(n) = advise_drop_file(path) {
            *files += 1;
            *bytes += n;
            *budget -= 1;
        }
        return;
    }

    if ft.is_dir() {
        let Ok(entries) = fs::read_dir(path) else { return };
        for entry in entries.filter_map(|e| e.ok()) {
            if *budget == 0 {
                return;
            }
            advise_tree(&entry.path(), depth + 1, budget, files, bytes);
        }
    }
}

/// fadvise(DONTNEED) one file, opened read-only. Returns bytes advised.
fn advise_drop_file(path: &Path) -> Option<u64> {
    let file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return Some(0);
    }
    let ret = unsafe {
        libc::posix_fadvise(file.as_raw_fd(), 0, len as libc::off_t, libc::POSIX_FADV_DONTNEED)
    };
    // posix_fadvise returns the errno directly; it does not set the errno global.
    if ret == 0 { Some(len) } else { None }
}

// ── path expansion ────────────────────────────────────────────────────────────

/// Expand `~` and one `*` per segment into existing paths. Resolved from `/`, so
/// patterns must be absolute or `~`-rooted (a relative pattern walks from root,
/// not cwd). Only existing paths are returned.
pub fn expand_path(pattern: &str) -> Vec<PathBuf> {
    let expanded = expand_tilde(pattern);
    let segments: Vec<&str> = expanded
        .to_str()
        .unwrap_or("")
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    let mut frontier: Vec<PathBuf> = vec![PathBuf::from("/")];
    for seg in segments {
        let mut next = Vec::new();
        for base in &frontier {
            if seg.contains('*') {
                let Ok(entries) = fs::read_dir(base) else { continue };
                for entry in entries.filter_map(|e| e.ok()) {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if segment_matches(seg, &name) {
                        next.push(base.join(&*name));
                    }
                }
            } else {
                let candidate = base.join(seg);
                if candidate.exists() {
                    next.push(candidate);
                }
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }
    frontier
}

fn expand_tilde(pattern: &str) -> PathBuf {
    if let Some(rest) = pattern.strip_prefix("~/") {
        mgd_common::util::home_dir().join(rest)
    } else if pattern == "~" {
        mgd_common::util::home_dir()
    } else {
        PathBuf::from(pattern)
    }
}

/// Match one segment against a name with a single optional `*` (any run,
/// including empty); no `*` is an exact match.
fn segment_matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((prefix, suffix)) => {
            name.len() >= prefix.len() + suffix.len()
                && name.starts_with(prefix)
                && name.ends_with(suffix)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_exact_match() {
        assert!(segment_matches("target", "target"));
        assert!(!segment_matches("target", "targets"));
    }

    #[test]
    fn segment_star_prefix() {
        assert!(segment_matches("*.cache", "build.cache"));
        assert!(segment_matches("*.cache", ".cache")); // empty run
        assert!(!segment_matches("*.cache", "cache.txt"));
    }

    #[test]
    fn segment_star_suffix() {
        assert!(segment_matches("node_*", "node_modules"));
        assert!(segment_matches("node_*", "node_")); // empty run
        assert!(!segment_matches("node_*", "mynode_x"));
    }

    #[test]
    fn segment_bare_star_matches_anything() {
        assert!(segment_matches("*", "anything"));
        assert!(segment_matches("*", ""));
    }

    #[test]
    fn segment_star_no_overlap() {
        // prefix+suffix must not overlap to match a too-short name.
        assert!(!segment_matches("ab*ba", "aba"));
        assert!(segment_matches("ab*ba", "abba"));
        assert!(segment_matches("ab*ba", "abXba"));
    }

    #[test]
    fn tilde_expands_to_home() {
        let home = mgd_common::util::home_dir();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/foo/bar"), home.join("foo/bar"));
        assert_eq!(expand_tilde("/etc/x"), PathBuf::from("/etc/x"));
    }

    #[test]
    fn expand_literal_existing_path() {
        let v = expand_path("/tmp");
        assert!(v.iter().any(|p| p == Path::new("/tmp")));
    }

    #[test]
    fn expand_nonexistent_yields_empty() {
        assert!(expand_path("/this/does/not/exist/anywhere").is_empty());
    }

    #[test]
    fn expand_wildcard_lists_matches() {
        let v = expand_path("/usr/*");
        assert!(!v.is_empty());
        assert!(v.iter().all(|p| p.starts_with("/usr/")));
    }

    #[test]
    fn temp_drop_caches_walks_and_advises_real_tree() {
        let root = std::env::temp_dir().join(format!("mgd_cache_it_{}", std::process::id()));
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(root.join("big.bin"), vec![0u8; 4 * 1024 * 1024]).unwrap();
        fs::write(sub.join("small.bin"), vec![0u8; 1024 * 1024]).unwrap();

        let pat = root.to_string_lossy().to_string();
        let results = drop_caches(std::slice::from_ref(&pat));
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.pattern, pat);
        assert_eq!(r.files_advised, 2);
        assert_eq!(r.bytes_advised, 5 * 1024 * 1024);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn budget_is_shared_across_patterns() {
        let base = std::env::temp_dir().join(format!("mgd_cache_budget_{}", std::process::id()));
        let a = base.join("a");
        let b = base.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        for d in [&a, &b] {
            for i in 0..5 {
                fs::write(d.join(format!("f{i}.bin")), b"x").unwrap();
            }
        }
        let pats = vec![a.to_string_lossy().to_string(), b.to_string_lossy().to_string()];
        let total: usize = drop_caches(&pats).iter().map(|r| r.files_advised).sum();
        assert!(total <= MAX_FILES);
        assert_eq!(total, 10);

        fs::remove_dir_all(&base).unwrap();
    }
}
