//! AppData prefs (input/output folders, GPU, OBS metadata flag).

use std::fs;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::gpu::GpuChoice;

/// Persisted UI / encode settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prefs {
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
    pub gpu: GpuChoice,
    pub preserve_obs_metadata: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            input_dir: PathBuf::new(),
            output_dir: PathBuf::new(),
            gpu: GpuChoice::Any,
            preserve_obs_metadata: false,
        }
    }
}

/// `%APPDATA%\GigaMux` (Windows) or platform equivalent.
pub fn prefs_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "GigaMux").map(|d| d.config_dir().to_path_buf())
}

fn prefs_path() -> Option<PathBuf> {
    prefs_dir().map(|d| d.join("prefs.json"))
}

/// Load prefs from AppData; missing/corrupt → defaults.
pub fn load() -> Prefs {
    let Some(path) = prefs_path() else {
        return Prefs::default();
    };
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Prefs::default(),
    }
}

/// Save prefs to AppData (creates directory if needed).
pub fn save(prefs: &Prefs) -> Result<(), String> {
    let dir = prefs_dir().ok_or_else(|| "could not resolve AppData config dir".to_string())?;
    fs::create_dir_all(&dir).map_err(|e| format!("create prefs dir: {e}"))?;
    let path = dir.join("prefs.json");
    let text = serde_json::to_string_pretty(prefs).map_err(|e| format!("serialize prefs: {e}"))?;
    fs::write(&path, text).map_err(|e| format!("write prefs: {e}"))?;
    Ok(())
}

/// True if path is a non-empty existing directory.
pub fn dir_ok(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.is_dir()
}

/// Export prefs as a portable profile JSON.
pub fn export_profile(prefs: &Prefs, path: &Path) -> Result<(), String> {
    let text = serde_json::to_string_pretty(prefs).map_err(|e| format!("serialize profile: {e}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create profile dir: {e}"))?;
    }
    fs::write(path, text).map_err(|e| format!("write profile: {e}"))
}

/// Import prefs from a profile JSON (does not auto-save to AppData).
pub fn import_profile(path: &Path) -> Result<Prefs, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read profile: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("parse profile: {e}"))
}

/// `%APPDATA%\GigaMux\session.log` — durable UI session log.
pub fn session_log_path() -> Option<PathBuf> {
    prefs_dir().map(|d| d.join("session.log"))
}

/// Load the tail of the on-disk session log (newest `max_lines` lines).
pub fn load_session_log(max_lines: usize) -> Vec<String> {
    let Some(path) = session_log_path() else {
        return Vec::new();
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    if lines.len() > max_lines {
        lines.drain(0..lines.len() - max_lines);
    }
    lines
}

/// Append one session-log line (creates AppData dir / file as needed).
pub fn append_session_log(line: &str) -> Result<(), String> {
    let dir = prefs_dir().ok_or_else(|| "could not resolve AppData config dir".to_string())?;
    fs::create_dir_all(&dir).map_err(|e| format!("create prefs dir: {e}"))?;
    let path = dir.join("session.log");
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open session.log: {e}"))?;
    writeln!(f, "{line}").map_err(|e| format!("write session.log: {e}"))
}

/// Replace on-disk session log with the in-memory tail (after trim).
pub fn rewrite_session_log(lines: &[String]) -> Result<(), String> {
    let dir = prefs_dir().ok_or_else(|| "could not resolve AppData config dir".to_string())?;
    fs::create_dir_all(&dir).map_err(|e| format!("create prefs dir: {e}"))?;
    let path = dir.join("session.log");
    let mut text = lines.join("\n");
    if !lines.is_empty() {
        text.push('\n');
    }
    fs::write(&path, text).map_err(|e| format!("rewrite session.log: {e}"))
}
