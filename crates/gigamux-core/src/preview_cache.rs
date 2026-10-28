//! Persistent preview meta + thumbnail cache under AppData (survives across sessions).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::prefs::prefs_dir;
use crate::probe::{MediaSummary, RgbaThumb};

/// On-disk index (`preview_cache.json`); RGBA bytes live beside it in `preview_thumbs/`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistedPreviewCache {
    /// Must match [`MediaSummary::SCHEMA`] or the load is discarded.
    pub schema: u32,
    pub entries: HashMap<String, PersistedPreviewEntry>,
}

/// One file's cached probe + optional thumb file name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedPreviewEntry {
    pub path: PathBuf,
    pub size: u64,
    pub mtime_secs: i64,
    pub summary: Result<MediaSummary, String>,
    /// Relative file under `preview_thumbs/` when a thumb was saved.
    #[serde(default)]
    pub thumb_file: Option<String>,
    pub thumb_width: u32,
    pub thumb_height: u32,
    /// When thumb probe failed (no `thumb_file`).
    #[serde(default)]
    pub thumb_err: Option<String>,
    /// False when only the fast probe ran (peaks still need enrich). Old caches default true.
    #[serde(default = "default_true")]
    pub bitrates_ready: bool,
}

fn default_true() -> bool {
    true
}

/// Size + mtime used to invalidate stale cache rows.
pub fn file_fingerprint(path: &Path) -> Option<(u64, i64)> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some((meta.len(), mtime))
}

fn cache_index_path() -> Option<PathBuf> {
    prefs_dir().map(|d| d.join("preview_cache.json"))
}

fn thumbs_dir() -> Option<PathBuf> {
    prefs_dir().map(|d| d.join("preview_thumbs"))
}

/// Safe filename for a UI cache key (`i:foo.mp4` → `i_foo.mp4.rgba`).
fn thumb_filename(key: &str) -> String {
    let safe: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{safe}.rgba")
}

/// Load cache from AppData; empty if missing, corrupt, or schema mismatch.
pub fn load(schema: u32) -> PersistedPreviewCache {
    let Some(path) = cache_index_path() else {
        return PersistedPreviewCache::default();
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return PersistedPreviewCache::default();
    };
    let Ok(mut disk) = serde_json::from_str::<PersistedPreviewCache>(&text) else {
        return PersistedPreviewCache::default();
    };
    if disk.schema != schema {
        return PersistedPreviewCache::default();
    }
    // Drop entries whose thumb file vanished.
    let thumb_root = thumbs_dir();
    disk.entries.retain(|_, e| {
        if let Some(name) = &e.thumb_file {
            let Some(root) = thumb_root.as_ref() else {
                return false;
            };
            root.join(name).is_file()
        } else {
            true
        }
    });
    disk
}

/// Read RGBA bytes for a persisted thumb entry.
pub fn load_thumb(entry: &PersistedPreviewEntry) -> Result<RgbaThumb, String> {
    if let Some(err) = &entry.thumb_err {
        return Err(err.clone());
    }
    let name = entry
        .thumb_file
        .as_ref()
        .ok_or_else(|| "no thumb on disk".to_string())?;
    let path = thumbs_dir()
        .ok_or_else(|| "could not resolve AppData config dir".to_string())?
        .join(name);
    let rgba = fs::read(&path).map_err(|e| format!("read thumb: {e}"))?;
    let expect = (entry.thumb_width as usize).saturating_mul(entry.thumb_height as usize) * 4;
    if rgba.len() != expect {
        return Err(format!(
            "thumb size mismatch ({} bytes, expected {expect})",
            rgba.len()
        ));
    }
    Ok(RgbaThumb {
        width: entry.thumb_width,
        height: entry.thumb_height,
        rgba,
    })
}

/// Write index JSON. Caller should [`write_thumb`] first; orphans under `preview_thumbs/` are pruned.
pub fn save(cache: &PersistedPreviewCache) -> Result<(), String> {
    let dir = prefs_dir().ok_or_else(|| "could not resolve AppData config dir".to_string())?;
    fs::create_dir_all(&dir).map_err(|e| format!("create prefs dir: {e}"))?;
    let thumb_root = dir.join("preview_thumbs");
    fs::create_dir_all(&thumb_root).map_err(|e| format!("create thumbs dir: {e}"))?;

    let index_path = dir.join("preview_cache.json");
    let text =
        serde_json::to_string_pretty(cache).map_err(|e| format!("serialize preview cache: {e}"))?;
    fs::write(&index_path, text).map_err(|e| format!("write preview_cache.json: {e}"))?;

    // Drop thumb files no longer referenced by the index.
    let keep: HashMap<&str, ()> = cache
        .entries
        .values()
        .filter_map(|e| e.thumb_file.as_deref().map(|n| (n, ())))
        .collect();
    if let Ok(rd) = fs::read_dir(&thumb_root) {
        for ent in rd.flatten() {
            let name = ent.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !keep.contains_key(name) {
                let _ = fs::remove_file(ent.path());
            }
        }
    }
    Ok(())
}

/// Persist one entry's thumb bytes (call before [`save`] so `thumb_file` is set).
pub fn write_thumb(key: &str, thumb: &RgbaThumb) -> Result<String, String> {
    let root = thumbs_dir().ok_or_else(|| "could not resolve AppData config dir".to_string())?;
    fs::create_dir_all(&root).map_err(|e| format!("create thumbs dir: {e}"))?;
    let name = thumb_filename(key);
    fs::write(root.join(&name), &thumb.rgba).map_err(|e| format!("write thumb: {e}"))?;
    Ok(name)
}

/// Delete index + all thumb files.
pub fn clear() -> Result<(), String> {
    if let Some(path) = cache_index_path() {
        let _ = fs::remove_file(path);
    }
    if let Some(root) = thumbs_dir() {
        if root.is_dir() {
            fs::remove_dir_all(&root).map_err(|e| format!("remove thumbs: {e}"))?;
        }
    }
    Ok(())
}

/// True when `path` still matches the fingerprint stored on `entry`.
pub fn entry_fresh(entry: &PersistedPreviewEntry, path: &Path) -> bool {
    let Some((size, mtime)) = file_fingerprint(path) else {
        return false;
    };
    entry.size == size && entry.mtime_secs == mtime && paths_match(&entry.path, path)
}

fn paths_match(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => {
            let a = a.to_string_lossy().replace('/', "\\").to_lowercase();
            let b = b.to_string_lossy().replace('/', "\\").to_lowercase();
            a == b
        }
    }
}
