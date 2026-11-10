//! Farm chunk split / naming / ffconcat helpers (based off old vod_chunkutil.py).

use std::path::{Path, PathBuf};

use regex::Regex;

/// Rough input `-ss` pad (seconds) before the exact frame cut — speeds demux; trim still exact.
pub const CHUNK_SEEK_PAD_S: f64 = 1.0;

/// `{stem}.c000of004.mp4` style name (0-based index).
pub fn chunk_output_name(stem: &str, index: u32, count: u32, ext: &str) -> String {
    let ext = ext.trim_start_matches('.');
    format!("{stem}.c{index:03}of{count:03}.{ext}")
}

/// Inverse of [`chunk_output_name`]: `(deliver_stem, index, count, ext)`.
/// Stem is the final deliver basename without `.cNNNofMMM` — not `Path::file_stem()`, which keeps the chunk tag.
pub fn parse_chunk_filename(name: &str) -> Option<(String, u32, u32, String)> {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)^(.+)\.c(\d{3})of(\d{3})\.(mp4|webm|mkv)$").expect("chunk name regex")
    });
    let caps = re.captures(name)?;
    let stem = caps.get(1)?.as_str().to_string();
    let index: u32 = caps.get(2)?.as_str().parse().ok()?;
    let count: u32 = caps.get(3)?.as_str().parse().ok()?;
    let ext = caps.get(4)?.as_str().to_string();
    if stem.is_empty() || count == 0 || index >= count {
        return None;
    }
    Some((stem, index, count, ext))
}

/// Contiguous frame slices; last chunk absorbs remainder. Returns `(start_frame, frame_count)`.
pub fn chunk_frame_range(total_frames: u64, index: u32, count: u32) -> Result<(u64, u64), String> {
    if count < 1 {
        return Err("chunk count must be >= 1".into());
    }
    if total_frames < 1 {
        return Err("total_frames must be >= 1".into());
    }
    if index >= count {
        return Err(format!("chunk index {index} out of range for count {count}"));
    }
    if count == 1 {
        return Ok((0, total_frames));
    }
    let base = total_frames / u64::from(count);
    let start = u64::from(index) * base;
    if index == count - 1 {
        Ok((start, total_frames - start))
    } else {
        Ok((start, base))
    }
}

/// Equal-duration slices; last chunk absorbs remainder. Returns `(start_s, dur_s)`.
pub fn chunk_time_range(duration: f64, index: u32, count: u32) -> Result<(f64, f64), String> {
    if count < 1 {
        return Err("chunk count must be >= 1".into());
    }
    if index >= count {
        return Err(format!("chunk index {index} out of range for count {count}"));
    }
    if count == 1 {
        return Ok((0.0, duration));
    }
    let base = duration / f64::from(count);
    let start = f64::from(index) * base;
    if index == count - 1 {
        Ok((start, (duration - start).max(0.0)))
    } else {
        Ok((start, base))
    }
}

/// Prefer `nb_frames` when valid; else `round(duration * fps)`.
pub fn estimate_total_frames(duration_s: f64, fps: f64, nb_frames: Option<u64>) -> Option<u64> {
    if let Some(n) = nb_frames {
        if n > 0 {
            return Some(n);
        }
    }
    if fps > 0.0 && duration_s > 0.0 {
        return Some((duration_s * fps).round().max(1.0) as u64);
    }
    None
}

/// Parse ffprobe `r_frame_rate` (`60/1` or `59.94`) to float fps.
pub fn parse_fps(r_frame_rate: &str) -> Option<f64> {
    let s = r_frame_rate.trim();
    if s.is_empty() || s == "0/0" || s.eq_ignore_ascii_case("N/A") {
        return None;
    }
    if let Some((num, den)) = s.split_once('/') {
        let den_f: f64 = den.parse().ok()?;
        if den_f == 0.0 {
            return None;
        }
        let num_f: f64 = num.parse().ok()?;
        return Some(num_f / den_f);
    }
    s.parse().ok()
}

/// Ordered chunk paths for a stem.
pub fn list_chunk_paths(output_dir: &Path, stem: &str, count: u32, ext: &str) -> Vec<PathBuf> {
    (0..count)
        .map(|i| output_dir.join(chunk_output_name(stem, i, count, ext)))
        .collect()
}

/// Write an ffconcat list with per-file `duration=` lines (video-led joins).
///
/// Paths in the same directory as `list_path` are written as basenames so mixed-OS Flamenco workers can open the list without Windows/Linux absolute prefixes.
pub fn write_ffconcat(
    chunk_paths: &[PathBuf],
    durations_s: &[f64],
    list_path: &Path,
) -> Result<(), String> {
    if chunk_paths.len() != durations_s.len() {
        return Err("ffconcat paths/durations length mismatch".into());
    }
    let list_parent = list_path.parent();
    let mut lines = vec!["ffconcat version 1.0".to_string()];
    for (p, dur) in chunk_paths.iter().zip(durations_s.iter()) {
        // Prefer basename when the chunk sits next to the list (farm-safe).
        let same_dir = match (list_parent, p.parent()) {
            (Some(lp), Some(pp)) => lp == pp,
            _ => false,
        };
        let ap = if same_dir {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string_lossy().into_owned())
        } else {
            p.canonicalize()
                .unwrap_or_else(|_| p.to_path_buf())
                .to_string_lossy()
                .replace('\\', "/")
        }
        .replace('\'', r"'\''");
        lines.push(format!("file '{ap}'"));
        if *dur > 0.0 {
            lines.push(format!("duration {dur:.6}"));
        }
    }
    let body = lines.join("\n") + "\n";
    if let Some(parent) = list_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("ffconcat mkdir: {e}"))?;
    }
    std::fs::write(list_path, body).map_err(|e| format!("ffconcat write: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_range_last_absorbs_remainder() {
        assert_eq!(chunk_frame_range(100, 0, 3).unwrap(), (0, 33));
        assert_eq!(chunk_frame_range(100, 1, 3).unwrap(), (33, 33));
        assert_eq!(chunk_frame_range(100, 2, 3).unwrap(), (66, 34));
    }

    #[test]
    fn chunk_name_format() {
        assert_eq!(
            chunk_output_name("sample.ts", 0, 4, "mp4"),
            "sample.ts.c000of004.mp4"
        );
    }

    #[test]
    fn parse_chunk_filename_round_trip() {
        let name = chunk_output_name("sample.ts", 2, 4, "mp4");
        let (stem, index, count, ext) = parse_chunk_filename(&name).expect("parse");
        assert_eq!(stem, "sample.ts");
        assert_eq!(index, 2);
        assert_eq!(count, 4);
        assert_eq!(ext, "mp4");
        assert_eq!(chunk_output_name(&stem, index, count, &ext), name);
        assert!(parse_chunk_filename("sample.ts.mp4").is_none());
        assert!(parse_chunk_filename("sample.ts.c004of004.mp4").is_none());
    }

    #[test]
    fn time_range_estimate() {
        let (s, d) = chunk_time_range(120.0, 0, 4).unwrap();
        assert!((s - 0.0).abs() < 1e-9 && (d - 30.0).abs() < 1e-9);
        let (s, d) = chunk_time_range(120.0, 3, 4).unwrap();
        assert!((s - 90.0).abs() < 1e-9 && (d - 30.0).abs() < 1e-9);
    }
}
