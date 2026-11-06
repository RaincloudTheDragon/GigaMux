//! Farm chunk split / naming / ffconcat helpers (based off old vod_chunkutil.py).

use std::path::{Path, PathBuf};

/// Rough input `-ss` pad (seconds) before the exact frame cut — speeds demux; trim still exact.
pub const CHUNK_SEEK_PAD_S: f64 = 1.0;

/// `{stem}.c000of004.mp4` style name (0-based index).
pub fn chunk_output_name(stem: &str, index: u32, count: u32, ext: &str) -> String {
    let ext = ext.trim_start_matches('.');
    format!("{stem}.c{index:03}of{count:03}.{ext}")
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
pub fn write_ffconcat(
    chunk_paths: &[PathBuf],
    durations_s: &[f64],
    list_path: &Path,
) -> Result<(), String> {
    if chunk_paths.len() != durations_s.len() {
        return Err("ffconcat paths/durations length mismatch".into());
    }
    let mut lines = vec!["ffconcat version 1.0".to_string()];
    for (p, dur) in chunk_paths.iter().zip(durations_s.iter()) {
        let ap = p
            .canonicalize()
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .replace('\\', "/")
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
    fn time_range_estimate() {
        let (s, d) = chunk_time_range(120.0, 0, 4).unwrap();
        assert!((s - 0.0).abs() < 1e-9 && (d - 30.0).abs() < 1e-9);
        let (s, d) = chunk_time_range(120.0, 3, 4).unwrap();
        assert!((s - 90.0).abs() < 1e-9 && (d - 30.0).abs() < 1e-9);
    }
}
