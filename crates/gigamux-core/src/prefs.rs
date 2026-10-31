//! AppData prefs (folders, GPU, OBS metadata, bitrate ceiling mode, output targets).

use std::fs;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::gpu::GpuChoice;
use crate::targets::{
    clamp_alpha_container, clamp_opaque_container, AlphaVideo, OpaqueVideo, OutContainer,
};

/// Persisted UI / encode settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prefs {
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
    pub gpu: GpuChoice,
    pub preserve_obs_metadata: bool,
    /// When true, encode ceiling uses 1s video peak; default is average rate (CBR-oriented).
    #[serde(default)]
    pub use_peak_bitrate: bool,
    /// NVENC for HEVC/H.264/AV1 when true; CPU encoders when false. Default on.
    #[serde(default = "default_true")]
    pub use_hardware: bool,
    /// When set, replaces the automatic video ceiling (maxrate / bufsize) in kbps.
    #[serde(default)]
    pub maxrate_override_k: Option<u32>,
    /// When set, replaces default CQ0/CRF0 (`-cq` / `-crf`). Scales differ by codec — not cross-calibrated.
    #[serde(default)]
    pub cq_override: Option<u32>,
    #[serde(default)]
    pub opaque_video: OpaqueVideo,
    #[serde(default)]
    pub opaque_container: OutContainer,
    #[serde(default)]
    pub alpha_video: AlphaVideo,
    #[serde(default)]
    pub alpha_container: OutContainer,
    /// Confirm before moving a file to the Recycle Bin. Default on.
    #[serde(default = "default_true")]
    pub warn_recycle_delete: bool,
    /// Confirm before a permanent (non–Recycle Bin) delete. Default on.
    #[serde(default = "default_true")]
    pub warn_permanent_delete: bool,
    /// Play a short system chime when an encode / VMAF / check batch finishes. Default on.
    #[serde(default = "default_true")]
    pub job_done_chime: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            input_dir: PathBuf::new(),
            output_dir: PathBuf::new(),
            gpu: GpuChoice::Any,
            preserve_obs_metadata: false,
            use_peak_bitrate: false,
            use_hardware: true,
            maxrate_override_k: None,
            cq_override: None,
            opaque_video: OpaqueVideo::Av1,
            opaque_container: OutContainer::Mp4,
            alpha_video: AlphaVideo::Vp9,
            alpha_container: OutContainer::Webm,
            warn_recycle_delete: true,
            warn_permanent_delete: true,
            job_done_chime: true,
        }
    }
}

impl Prefs {
    /// Clamp illegal codec×container pairs (load / import / toggle).
    pub fn sanitize_targets(&mut self) {
        self.opaque_container = clamp_opaque_container(self.opaque_video, self.opaque_container);
        self.alpha_container = clamp_alpha_container(self.alpha_video, self.alpha_container);
    }

    /// Container for a given alpha flag.
    pub fn container_for(&self, has_alpha: bool) -> OutContainer {
        if has_alpha {
            self.alpha_container
        } else {
            self.opaque_container
        }
    }
}

/// App config root — Windows: `%APPDATA%\GigaMux\config` (`directories` ProjectDirs).
pub fn prefs_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "GigaMux").map(|d| d.config_dir().to_path_buf())
}

/// App root — Windows: `%APPDATA%\GigaMux` (parent of `config`).
pub fn app_root() -> Option<PathBuf> {
    prefs_dir().and_then(|d| d.parent().map(|p| p.to_path_buf()))
}

/// Durable logs — Windows: `%APPDATA%\GigaMux\logs` (session + encode/VMAF job gz).
pub fn logs_dir() -> Option<PathBuf> {
    app_root().map(|d| d.join("logs"))
}

/// Create `logs/`, migrating leftovers from `config/session.log` and `config/logs/`.
pub fn ensure_logs_dir() -> Result<PathBuf, String> {
    let dir = logs_dir().ok_or_else(|| "could not resolve AppData logs dir".to_string())?;
    fs::create_dir_all(&dir).map_err(|e| format!("create logs dir: {e}"))?;
    migrate_legacy_logs(&dir);
    Ok(dir)
}

/// One-shot: move old config-scoped logs into `%APPDATA%\GigaMux\logs`.
fn migrate_legacy_logs(new_dir: &Path) {
    let Some(config) = prefs_dir() else {
        return;
    };
    let old_session = config.join("session.log");
    let new_session = new_dir.join("session.log");
    if old_session.is_file() && !new_session.exists() {
        let _ = fs::rename(&old_session, &new_session);
    }
    let old_jobs = config.join("logs");
    if old_jobs.is_dir() {
        if let Ok(entries) = fs::read_dir(&old_jobs) {
            for entry in entries.flatten() {
                let src = entry.path();
                if !src.is_file() {
                    continue;
                }
                let Some(name) = src.file_name() else {
                    continue;
                };
                let dst = new_dir.join(name);
                if dst.exists() {
                    let _ = fs::remove_file(&src);
                } else {
                    let _ = fs::rename(&src, &dst);
                }
            }
        }
        let _ = fs::remove_dir(&old_jobs);
    }
}

fn prefs_path() -> Option<PathBuf> {
    prefs_dir().map(|d| d.join("prefs.json"))
}

/// Load prefs from AppData; missing/corrupt → defaults.
pub fn load() -> Prefs {
    let Some(path) = prefs_path() else {
        return Prefs::default();
    };
    let mut prefs = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Prefs::default(),
    };
    prefs.sanitize_targets();
    prefs
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
    let mut prefs: Prefs =
        serde_json::from_str(&text).map_err(|e| format!("parse profile: {e}"))?;
    prefs.sanitize_targets();
    Ok(prefs)
}

/// `%APPDATA%\GigaMux\logs\session.log` — full durable transcript (UI + encode + VMAF).
pub fn session_log_path() -> Option<PathBuf> {
    logs_dir().map(|d| d.join("session.log"))
}

/// Load the tail of the on-disk session log (newest `max_lines` lines).
pub fn load_session_log(max_lines: usize) -> Vec<String> {
    let _ = ensure_logs_dir();
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

/// Append one session-log line (creates logs dir / file as needed).
pub fn append_session_log(line: &str) -> Result<(), String> {
    let dir = ensure_logs_dir()?;
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
    let dir = ensure_logs_dir()?;
    let path = dir.join("session.log");
    let mut text = lines.join("\n");
    if !lines.is_empty() {
        text.push('\n');
    }
    fs::write(&path, text).map_err(|e| format!("rewrite session.log: {e}"))
}

/// Delete `session.log` if present (also clears a leftover `config/session.log`).
pub fn clear_session_log() -> Result<(), String> {
    let _ = ensure_logs_dir();
    for path in [session_log_path(), prefs_dir().map(|d| d.join("session.log"))]
        .into_iter()
        .flatten()
    {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("remove {}: {e}", path.display())),
        }
    }
    Ok(())
}
