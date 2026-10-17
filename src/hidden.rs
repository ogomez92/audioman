//! Persistent list of applications hidden from Left/Right navigation.
//!
//! Stored as one process name per line in `%APPDATA%\audioman\hidden.txt`.
//! Names are matched case-insensitively against the session name (the exe stem
//! from `audio::process_name`, or "System sounds").

use std::path::PathBuf;

fn path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("audioman").join("hidden.txt"))
}

pub fn load() -> Vec<String> {
    let Some(p) = path() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(p) else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.is_empty() && !contains(&names, line) {
            names.push(line.to_string());
        }
    }
    names
}

pub fn save(names: &[String]) -> std::io::Result<()> {
    let p = path().ok_or_else(|| std::io::Error::other("APPDATA is not set"))?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = names.join("\r\n");
    text.push_str("\r\n");
    std::fs::write(p, text)
}

pub fn contains(names: &[String], name: &str) -> bool {
    names.iter().any(|n| same_name(n, name))
}

pub fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}
