//! Persistent lists of applications and devices hidden from Left/Right
//! navigation.
//!
//! Applications: one process name per line in `%APPDATA%\audioman\hidden.txt`.
//! Names are matched case-insensitively against the session name (the exe stem
//! from `audio::process_name`, or "System sounds").
//!
//! Devices: one `<endpoint id>\t<friendly name>` per line in
//! `%APPDATA%\audioman\hidden_devices.txt`. The id is what's matched (so a
//! renamed device stays hidden); the name is only kept so a hidden device that
//! is currently unplugged can still be listed and unhidden.

use std::path::PathBuf;

/// `%APPDATA%\audioman\<file>`: where every persisted setting lives.
pub fn config_path(file: &str) -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("audioman").join(file))
}

const APPS_FILE: &str = "hidden.txt";
const DEVICES_FILE: &str = "hidden_devices.txt";

pub fn load() -> Vec<String> {
    load_lines(APPS_FILE)
}

pub fn save(names: &[String]) -> std::io::Result<()> {
    save_lines(APPS_FILE, names)
}

/// A remembered hidden device: endpoint id (as `devices::id_string`) + name.
#[derive(Clone)]
pub struct HiddenDevice {
    pub id: String,
    pub name: String,
}

pub fn load_devices() -> Vec<HiddenDevice> {
    let mut out: Vec<HiddenDevice> = Vec::new();
    for line in load_lines(DEVICES_FILE) {
        let (id, name) = line.split_once('\t').unwrap_or((line.as_str(), ""));
        let (id, name) = (id.trim(), name.trim());
        if !id.is_empty() && !device_hidden(&out, id) {
            out.push(HiddenDevice {
                id: id.to_string(),
                name: name.to_string(),
            });
        }
    }
    out
}

pub fn save_devices(devices: &[HiddenDevice]) -> std::io::Result<()> {
    let lines: Vec<String> = devices
        .iter()
        .map(|d| format!("{}\t{}", d.id, d.name))
        .collect();
    save_lines(DEVICES_FILE, &lines)
}

pub fn device_hidden(devices: &[HiddenDevice], id: &str) -> bool {
    devices.iter().any(|d| d.id.eq_ignore_ascii_case(id))
}

fn load_lines(file: &str) -> Vec<String> {
    let Some(p) = config_path(file) else {
        return Vec::new();
    };
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

fn save_lines(file: &str, names: &[String]) -> std::io::Result<()> {
    let p = config_path(file).ok_or_else(|| std::io::Error::other("APPDATA is not set"))?;
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
