use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(super) fn scan_desktop_files(category_priorities: &HashMap<String, u8>) -> HashMap<String, u8> {
    let mut index = HashMap::new();
    let home = mgd_common::util::home_dir();
    // User dirs first so or_insert() first-wins gives user overrides priority over system.
    let dirs = [
        home.join(".local/share/applications"),
        home.join(".local/share/flatpak/exports/share/applications"),
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/var/lib/flatpak/exports/share/applications"),
    ];
    for dir in &dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            if let Some((exe, prio)) = parse_desktop_file(&path, category_priorities) {
                index.entry(exe).or_insert(prio);
            }
        }
    }
    index
}

fn parse_desktop_file(path: &Path, category_priorities: &HashMap<String, u8>) -> Option<(String, u8)> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut exe_basename: Option<String> = None;
    // Borrow slices directly from content — no String allocation per category.
    let mut categories: Vec<&str> = vec![];
    // Only parse keys from the [Desktop Entry] section; skip [Desktop Action *] etc.
    let mut in_desktop_entry = false;

    for line in content.lines() {
        if line.starts_with('[') {
            in_desktop_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_desktop_entry {
            continue;
        }
        if line.starts_with("Exec=") && exe_basename.is_none() {
            let rest = &line["Exec=".len()..];
            // Use `else { continue }` instead of `?` so a blank Exec= skips only this line.
            let Some(binary) = rest.split_whitespace().next() else { continue };
            let Some(name) = Path::new(binary).file_name() else { continue };
            exe_basename = Some(name.to_string_lossy().into_owned());
        } else if let Some(rest) = line.strip_prefix("Categories=") {
            categories = rest
                .split(';')
                .filter(|s| !s.is_empty())
                .collect();
        }
    }

    let exe = exe_basename?;
    // Use max priority across all matching categories: the most expendable category wins,
    // ensuring the process is not under-prioritised due to incidental low-priority categories.
    let prio = categories.iter()
        .filter_map(|cat| category_priorities.get(*cat).copied())
        .max()?;
    Some((exe, prio))
}
