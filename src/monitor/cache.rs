//! Page-cache drop — surgically evict stale file cache before freezing apps.
//!
//! Under pressure the kernel often evicts application pages to swap while
//! retaining file cache (build artifacts, node_modules, browser cache) that
//! won't be read again until the next build. At High pressure, before any
//! process is frozen, mgd tells the kernel to drop cache for a configured set of
//! directory trees via `posix_fadvise(POSIX_FADV_DONTNEED)`. This is surgical
//! (only the listed trees, never a global `drop_caches`), needs no privilege
//! (the files are the user's own), and is non-destructive — `DONTNEED` drops
//! *clean* cached pages immediately; dirty pages are written back first by the
//! kernel and only then evictable, so no data is lost.
//!
//! Paths support a leading `~` and a single `*` wildcard per path segment
//! (e.g. `~/projects/*/target`). Expansion is hand-rolled (no glob dependency)
//! per the project's libc/serde/toml/regex-only constraint.

use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// Cap on directory-tree recursion depth, so a misconfigured path like `~`
/// can't make mgd walk the whole home tree while under memory pressure.
const MAX_DEPTH: usize = 8;

/// Cap on the number of files advised per `drop_caches` call (GLOBAL across all
/// configured patterns, not per-pattern), so a multi-path config can't make mgd
/// walk an unbounded number of files inline in the pressure loop.
const MAX_FILES: usize = 50_000;

/// Outcome of dropping cache for one configured path pattern.
pub struct CacheDropResult {
    pub pattern: String,
    pub files_advised: usize,
    pub bytes_advised: u64,
}

/// Drop page cache for every configured path pattern, returning per-pattern
/// results. Each pattern is `~`/`*`-expanded, then each resolved directory tree
/// (or file) is walked and `POSIX_FADV_DONTNEED`-advised. The `MAX_FILES` budget
/// is shared across all patterns so total inline work stays bounded.
pub fn drop_caches(patterns: &[String]) -> Vec<CacheDropResult> {
    // Shared budget across every pattern in this call.
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

/// Recursively advise every regular file under `path` with `DONTNEED`, bounded
/// by depth and the shared file budget. `path` may be a file or a directory.
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

    // Never follow symlinks — a symlink could point outside the configured tree
    // (e.g. into / or another user's data).
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

/// `posix_fadvise(POSIX_FADV_DONTNEED)` on a single regular file. Returns the
/// file size in bytes advised, or None on open/fadvise failure (skipped). Opens
/// read-only — no write access, no truncation, never modifies file contents.
fn advise_drop_file(path: &Path) -> Option<u64> {
    let file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return Some(0);
    }
    let ret = unsafe {
        libc::posix_fadvise(file.as_raw_fd(), 0, len as libc::off_t, libc::POSIX_FADV_DONTNEED)
    };
    // posix_fadvise returns the errno directly (0 = success), does not set errno.
    if ret == 0 { Some(len) } else { None }
}

// ── path expansion (pure, unit-tested) ───────────────────────────────────────

/// Expand a configured pattern into concrete existing paths. Supports a leading
/// `~` (HOME) and a single `*` per segment. A segment with `*` is matched
/// against the actual directory contents; segments without `*` are taken
/// literally. Only paths that exist are returned.
///
/// Paths are resolved from the filesystem root: use absolute or `~`-rooted
/// patterns. A relative pattern (`foo/bar`) is walked from `/`, not the daemon's
/// cwd — cache-drop config should always be absolute or `~`-rooted.
pub fn expand_path(pattern: &str) -> Vec<PathBuf> {
    let expanded = expand_tilde(pattern);
    let segments: Vec<&str> = expanded
        .to_str()
        .unwrap_or("")
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    // Absolute paths only (after ~ expansion everything we care about is). Start
    // from root and walk segment by segment, fanning out on wildcard segments.
    let mut frontier: Vec<PathBuf> = vec![PathBuf::from("/")];
    for seg in segments {
        let mut next = Vec::new();
        for base in &frontier {
            if seg.contains('*') {
                // Wildcard: list base dir, keep entries whose name matches.
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

/// Replace a leading `~` with $HOME. Anything else is returned unchanged.
fn expand_tilde(pattern: &str) -> PathBuf {
    if let Some(rest) = pattern.strip_prefix("~/") {
        crate::util::home_dir().join(rest)
    } else if pattern == "~" {
        crate::util::home_dir()
    } else {
        PathBuf::from(pattern)
    }
}

/// Glob-match one path segment against one name, supporting a single `*`
/// wildcard (matching any run of characters, including empty). A segment with no
/// `*` is an exact match. This is deliberately simple — one `*` per segment is
/// all the config syntax promises.
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
        assert!(segment_matches("*.cache", ".cache")); // empty run before suffix
        assert!(!segment_matches("*.cache", "cache.txt"));
    }

    #[test]
    fn segment_star_suffix() {
        assert!(segment_matches("node_*", "node_modules"));
        assert!(segment_matches("node_*", "node_")); // empty run after prefix
        assert!(!segment_matches("node_*", "mynode_x"));
    }

    #[test]
    fn segment_bare_star_matches_anything() {
        assert!(segment_matches("*", "anything"));
        assert!(segment_matches("*", ""));
    }

    #[test]
    fn segment_star_no_overlap() {
        // prefix + suffix longer than the name must not match via overlap.
        assert!(!segment_matches("ab*ba", "aba")); // would need >=4 chars
        assert!(segment_matches("ab*ba", "abba"));
        assert!(segment_matches("ab*ba", "abXba"));
    }

    #[test]
    fn tilde_expands_to_home() {
        let home = crate::util::home_dir();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/foo/bar"), home.join("foo/bar"));
        // No leading ~: unchanged.
        assert_eq!(expand_tilde("/etc/x"), PathBuf::from("/etc/x"));
    }

    #[test]
    fn expand_literal_existing_path() {
        // /tmp always exists; a literal (no wildcard) path resolves to itself.
        let v = expand_path("/tmp");
        assert!(v.iter().any(|p| p == Path::new("/tmp")));
    }

    #[test]
    fn expand_nonexistent_yields_empty() {
        assert!(expand_path("/this/does/not/exist/anywhere").is_empty());
    }

    #[test]
    fn expand_wildcard_lists_matches() {
        // /usr/lib and /usr/bin etc. exist; "/usr/*" should fan out to several.
        let v = expand_path("/usr/*");
        assert!(!v.is_empty());
        assert!(v.iter().all(|p| p.starts_with("/usr/")));
    }

    #[test]
    fn temp_drop_caches_walks_and_advises_real_tree() {
        // Real end-to-end walk over a temp tree created on disk.
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
        // Two regular files found across root + sub.
        assert_eq!(r.files_advised, 2);
        assert_eq!(r.bytes_advised, 5 * 1024 * 1024);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn budget_is_shared_across_patterns() {
        // Two trees, MAX_FILES worth of files split across them, would exceed the
        // cap if the budget were per-pattern. Here we just assert the documented
        // invariant holds: total files advised never exceeds MAX_FILES.
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
        assert_eq!(total, 10); // all 10 fit under the cap

        fs::remove_dir_all(&base).unwrap();
    }
}
