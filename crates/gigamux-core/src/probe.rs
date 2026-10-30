//! ffprobe helpers: file info, alpha detect, peak bitrate for encode ceilings.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

const DEFAULT_BITRATE_K: u32 = 20_000;
// Explicit alpha pix_fmts; yuva*/gbra* families are also matched by prefix in video_has_alpha.
const ALPHA_PIX_FMTS: &[&str] = &[
    "yuva420p",
    "yuva422p",
    "yuva444p",
    "gbrap",
    "rgba",
    "bgra",
    "argb",
    "abgr",
    "rgba64le",
    "rgba64be",
    "bgra64le",
    "bgra64be",
    "ya8",
    "ya16le",
    "ya16be",
    "ayuv64le",
    "ayuv64be",
];

/// Run ffprobe JSON for format + streams + chapters.
pub fn file_info(path: &Path) -> Result<Value, String> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            "-show_chapters",
            path.to_str().ok_or("non-utf8 path")?,
        ])
        .output()
        .map_err(|e| format!("ffprobe failed to start: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("ffprobe error: {err}"));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("ffprobe json: {e}"))
}

/// Constant vs variable frame timing (r_frame_rate vs avg_frame_rate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FrameTiming {
    Cfr,
    Vfr,
    Unknown,
}

/// Source media summary for the preview pane.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MediaSummary {
    pub container: String,
    pub duration_s: f64,
    pub file_size_bytes: u64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub timing: FrameTiming,
    /// Display codec (e.g. HEVC) — never invents nvHEVC.
    pub video_codec: String,
    pub video_profile: Option<String>,
    pub audio_codec: String,
    pub audio_profile: Option<String>,
    pub audio_channels: u32,
    pub audio_layout: String,
    pub audio_sample_rate: u32,
    pub audio_track_count: usize,
    pub peak_video_kbps: u32,
    /// Average video-stream kbps (stream bit_rate, else packet sample).
    pub avg_video_kbps: u32,
    pub peak_audio_kbps: u32,
    pub chapter_count: usize,
    /// True when chapters are present (OBS Hybrid MP4 often embeds these).
    pub has_obs_chapters: bool,
    /// True when format/stream tags explicitly mention OBS.
    pub from_obs: bool,
    /// Format `encoder` tag when present (e.g. Lavf… / obs-studio).
    pub encoder: Option<String>,
    pub has_alpha: bool,
    /// First video stream `pix_fmt` (e.g. yuv422p); empty when unknown.
    #[serde(default)]
    pub pix_fmt: String,
}

impl MediaSummary {
    /// Bump when summary fields change so UI preview cache re-fetches.
    pub const SCHEMA: u32 = 4;
}

/// RGBA thumbnail extracted via ffmpeg (fixed canvas for easy UI upload).
#[derive(Debug, Clone)]
pub struct RgbaThumb {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub const THUMB_W: u32 = 320;
pub const THUMB_H: u32 = 180;

fn first_video_stream(info: &Value) -> Option<&Value> {
    let streams = info.get("streams")?.as_array()?;
    streams.iter().find(|s| {
        s.get("codec_type").and_then(|v| v.as_str()) == Some("video")
            || (s.get("width").is_some()
                && s.get("channels").is_none()
                && s.get("codec_type").and_then(|v| v.as_str()) != Some("audio"))
    })
}

/// True when the first video stream has alpha (pix_fmt, ProRes 4444 profile, or alpha_mode tag).
pub fn video_has_alpha(info: &Value) -> bool {
    let Some(stream) = first_video_stream(info) else {
        return false;
    };
    let pix = stream
        .get("pix_fmt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    if pix.starts_with("yuva") || pix.starts_with("gbra") || ALPHA_PIX_FMTS.contains(&pix.as_str())
    {
        return true;
    }
    // ProRes 4444 / 4444 XQ carry alpha even when pix_fmt probing is thin.
    let codec = stream
        .get("codec_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if codec.starts_with("prores") {
        let profile = stream
            .get("profile")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if profile.contains("4444") {
            return true;
        }
    }
    let tags = stream.get("tags").and_then(|t| t.as_object());
    let Some(tags) = tags else {
        return false;
    };
    for (k, v) in tags {
        if k.eq_ignore_ascii_case("alpha_mode") {
            let mode = v.as_str().unwrap_or("").to_lowercase();
            return matches!(mode.as_str(), "1" | "true" | "yes");
        }
    }
    false
}

/// Display codec name for list/meta chips (α is a separate chip in the UI).
pub fn codec_tag(summary: &MediaSummary) -> String {
    summary.video_codec.clone()
}

/// Cool→warm heat for UI chips: `0.0` = primitive/inefficient, `1.0` = mature/efficient.
pub fn codec_heat(display_codec: &str) -> f32 {
    let name = display_codec.trim();
    match name {
        "AV1" => 1.0,
        "HEVC" => 0.88,
        "VP9" => 0.76,
        "H.264" => 0.58,
        "VP8" => 0.46,
        // Mature mezzanine / archive — capable, but bitrate-heavy → mid-cool.
        "ProRes" | "DNxHR" | "FFV1" | "CineForm" => 0.38,
        "DNxHD" | "Ut Video" | "HAP" => 0.32,
        "MPEG-4" | "Theora" => 0.28,
        "MPEG-2" => 0.22,
        "MJPEG" => 0.16,
        "PNG" | "Animation" => 0.10,
        "Raw" | "—" | "" => 0.05,
        _ => 0.34,
    }
}

/// Cool→warm heat for container/extension chips (`0.0` cool … `1.0` warm delivery).
/// Accepts `"mp4"`, `".mp4"`, or `"MP4"`.
pub fn container_heat(ext_or_label: &str) -> f32 {
    let e = ext_or_label
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    match e.as_str() {
        "mp4" => 1.0,
        "webm" => 0.78,
        "mkv" => 0.55,
        "mov" => 0.38,
        "m4v" => 0.72,
        "ogv" | "ogg" => 0.30,
        "mpg" | "mpeg" => 0.22,
        "avi" => 0.12,
        "mxf" => 0.28,
        _ => 0.35,
    }
}

/// Compact list/meta chip for chroma (+ bit depth when not 8-bit). `None` if unknown.
pub fn pix_fmt_chip_tag(pix: &str) -> Option<String> {
    let p = pix.trim().to_ascii_lowercase();
    if p.is_empty() {
        return None;
    }
    let tag = match p.as_str() {
        "yuv420p" | "yuvj420p" | "nv12" | "nv21" | "yuva420p" => "4:2:0",
        "yuv420p10le" | "yuv420p10be" | "p010le" | "p010be" => "4:2:0 10",
        "yuv422p" | "yuvj422p" | "yuva422p" => "4:2:2",
        "yuv422p10le" | "yuv422p10be" => "4:2:2 10",
        "yuv444p" | "yuvj444p" | "gbrp" | "yuva444p" | "gbrap" => "4:4:4",
        "yuv444p10le" | "yuv444p10be" | "gbrp10le" => "4:4:4 10",
        other => other,
    };
    Some(tag.into())
}

/// Minimum duration for peak÷avg to mean anything (1s peak buckets need several samples).
const RATE_MODE_MIN_DURATION_S: f64 = 3.0;

/// Peak may sit this far above avg and still count as CBR (OBS/NVENC CBR VBV overshoot).
const RATE_MODE_CBR_PEAK_RATIO: f32 = 1.35;

/// Detect source CBR vs VBR from enriched peak/avg once both are known.
/// Near-flat peak≈avg → CBR; peak clearly above average → VBR.
/// Returns `None` when rates are missing or the clip is too short for the heuristic.
pub fn rate_mode_chip_tag(avg_kbps: u32, peak_kbps: u32, duration_s: f64) -> Option<&'static str> {
    if avg_kbps == 0 || peak_kbps == 0 || duration_s < RATE_MODE_MIN_DURATION_S {
        return None;
    }
    let ratio = peak_kbps as f32 / avg_kbps as f32;
    // 1.20 was too tight: OBS CBR 20M often shows ~1.22–1.30 on 1s peak samples.
    if ratio <= RATE_MODE_CBR_PEAK_RATIO {
        Some("CBR")
    } else {
        Some("VBR")
    }
}

/// Cool→warm heat for rate-mode chips: CBR cooler (locked), VBR warmer (variable).
pub fn rate_mode_heat(tag: &str) -> f32 {
    match tag.trim().to_ascii_uppercase().as_str() {
        "CBR" => 0.28,
        "VBR" => 0.82,
        _ => 0.45,
    }
}

/// Cool→warm heat for chroma chips: narrower sampling cooler, wider warmer (4:2:0 → 4:4:4).
pub fn pix_fmt_heat(chip_or_pix: &str) -> f32 {
    let t = chip_or_pix.trim().to_ascii_lowercase();
    // Match chip tags first, then raw pix_fmt. 10-bit sits slightly warmer than 8-bit at the same chroma.
    if t.starts_with("4:2:0") || matches!(t.as_str(), "yuv420p" | "yuvj420p" | "nv12" | "nv21" | "yuva420p")
    {
        if t.contains("10") || t.contains("p10") || t.starts_with("p010") {
            0.28
        } else {
            0.18
        }
    } else if t.starts_with("4:2:2")
        || matches!(t.as_str(), "yuv422p" | "yuvj422p" | "yuva422p")
        || t.contains("422p10")
    {
        if t.contains("10") || t.contains("p10") {
            0.62
        } else {
            0.52
        }
    } else if t.starts_with("4:4:4")
        || matches!(t.as_str(), "yuv444p" | "yuvj444p" | "gbrp" | "yuva444p" | "gbrap")
        || t.contains("444p10")
    {
        if t.contains("10") || t.contains("p10") || t.contains("16") {
            1.0
        } else {
            0.88
        }
    } else {
        0.45
    }
}

/// First video stream `pix_fmt` (lowercase), or empty.
pub fn video_pix_fmt(info: &Value) -> String {
    first_video_stream(info)
        .and_then(|v| v.get("pix_fmt"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

pub fn video_codec_name(info: &Value) -> String {
    first_video_stream(info)
        .and_then(|s| s.get("codec_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase()
}

/// Source video colour tags from ffprobe (range / matrix / primaries / transfer).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VideoColorMeta {
    /// `tv` / `mpeg` = limited; `pc` / `jpeg` = full.
    pub range: Option<String>,
    pub space: Option<String>,
    pub primaries: Option<String>,
    pub transfer: Option<String>,
}

impl VideoColorMeta {
    pub fn is_empty(&self) -> bool {
        self.range.is_none()
            && self.space.is_none()
            && self.primaries.is_none()
            && self.transfer.is_none()
    }

    /// True when source is tagged full range (`pc` / `jpeg`).
    pub fn is_full_range(&self) -> bool {
        matches!(
            self.range.as_deref().map(str::to_ascii_lowercase).as_deref(),
            Some("pc" | "jpeg")
        )
    }
}

/// First video stream colour metadata (empty fields when ffprobe omitted them).
pub fn video_color_meta(info: &Value) -> VideoColorMeta {
    let Some(s) = first_video_stream(info) else {
        return VideoColorMeta::default();
    };
    let get = |k: &str| {
        s.get(k)
            .and_then(|v| v.as_str())
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty() && v != "unknown")
    };
    VideoColorMeta {
        range: get("color_range"),
        space: get("color_space"),
        primaries: get("color_primaries"),
        transfer: get("color_transfer"),
    }
}

/// Raw first-audio `codec_name` (lowercase), empty if none.
pub fn audio_codec_name(info: &Value) -> String {
    first_audio_stream(info)
        .and_then(|s| s.get("codec_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase()
}

fn stream_bitrate_bps(path: &Path, stream_spec: &str) -> Option<u64> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            stream_spec,
            "-show_entries",
            "stream=bit_rate",
            "-of",
            "default=nw=1:nk=1",
            path.to_str()?,
        ])
        .output()
        .ok()?;
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let n: u64 = raw.parse().ok()?;
    (n > 0).then_some(n)
}

fn packet_bitrate_bps(path: &Path, stream_spec: &str, seconds: u32, start_s: f64) -> Option<u64> {
    let interval = if start_s > 0.0 {
        format!("{start_s}%+{seconds}")
    } else {
        format!("%+{seconds}")
    };
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            stream_spec,
            "-read_intervals",
            &interval,
            "-show_entries",
            "packet=size",
            "-of",
            "csv=p=0",
            path.to_str()?,
        ])
        .output()
        .ok()?;
    let mut total: u64 = 0;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim().trim_end_matches(',');
        if let Ok(n) = line.parse::<u64>() {
            total += n;
        }
    }
    if total == 0 {
        return None;
    }
    Some((total * 8) / u64::from(seconds.max(1)))
}

fn peak_bps_in_region(path: &Path, stream_spec: &str, start_s: f64, region_s: f64) -> Option<u64> {
    let interval = if start_s > 0.0 {
        format!("{start_s}%+{region_s}")
    } else {
        format!("%+{region_s}")
    };
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            stream_spec,
            "-read_intervals",
            &interval,
            "-show_entries",
            "packet=pts_time,size",
            "-of",
            "csv=p=0",
            path.to_str()?,
        ])
        .output()
        .ok()?;
    let mut buckets: HashMap<i64, u64> = HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let parts: Vec<&str> = line.split(',').map(str::trim).filter(|p| !p.is_empty()).collect();
        if parts.len() < 2 {
            continue;
        }
        let (Ok(a), Ok(b)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) else {
            continue;
        };
        let (size, pts) = if parts[0].chars().all(|c| c.is_ascii_digit())
            && !parts[1].chars().all(|c| c.is_ascii_digit())
        {
            (a as u64, b)
        } else if parts[1].chars().all(|c| c.is_ascii_digit())
            && !parts[0].chars().all(|c| c.is_ascii_digit())
        {
            (b as u64, a)
        } else if a > 1e6 && b < 1e6 {
            (a as u64, b)
        } else {
            (b as u64, a)
        };
        if size == 0 {
            continue;
        }
        let key = (pts / 1.0).floor() as i64;
        *buckets.entry(key).or_default() += size;
    }
    let peak_bytes = buckets.values().copied().max()?;
    Some((peak_bytes * 8) as u64)
}

fn sampled_peak_bitrate_bps(path: &Path, stream_spec: &str, duration_s: f64) -> Option<u64> {
    if duration_s <= 0.0 {
        return peak_bps_in_region(path, stream_spec, 0.0, 60.0);
    }
    let region_s = 20.0_f64;
    let num_regions = if duration_s >= 160.0 {
        8
    } else {
        (duration_s / region_s.max(1.0)).floor().max(1.0) as usize
    };
    let mut peaks = Vec::new();
    for i in 0..num_regions {
        let start = if num_regions == 1 {
            0.0
        } else {
            let s = (duration_s - region_s) * (i as f64) / ((num_regions - 1) as f64);
            s.max(0.0).min((duration_s - region_s).max(0.0))
        };
        if let Some(p) = peak_bps_in_region(path, stream_spec, start, region_s.min(duration_s)) {
            peaks.push(p);
        }
    }
    peaks.into_iter().max()
}

fn video_peak_bitrate_bps(path: &Path, duration_s: f64) -> Option<u64> {
    sampled_peak_bitrate_bps(path, "v:0", duration_s)
}

fn first_audio_stream(info: &Value) -> Option<&Value> {
    info.get("streams")?
        .as_array()?
        .iter()
        .find(|s| s.get("codec_type").and_then(|v| v.as_str()) == Some("audio"))
}

fn json_f64(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn json_u32(v: &Value) -> Option<u32> {
    v.as_u64()
        .map(|n| n as u32)
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// Prefer the file extension; ffprobe often reports `mov,mp4,…` and the first token is `mov`.
fn container_label(path: &Path, format_name: Option<&str>) -> String {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext = ext.to_lowercase();
        if !ext.is_empty() {
            return ext;
        }
    }
    let Some(raw) = format_name.filter(|s| !s.is_empty()) else {
        return "?".into();
    };
    let parts: Vec<&str> = raw.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    // Prefer familiar names over the QuickTime alias that leads the ISOBMFF list.
    for prefer in ["mp4", "webm", "mkv", "mov", "m4a", "m4v"] {
        if parts.iter().any(|p| p.eq_ignore_ascii_case(prefer)) {
            return prefer.to_string();
        }
    }
    parts.first().copied().unwrap_or("?").to_string()
}

fn tag_strings(info: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut push_obj = |obj: Option<&serde_json::Map<String, Value>>| {
        if let Some(map) = obj {
            for (k, v) in map {
                out.push(k.clone());
                if let Some(s) = v.as_str() {
                    out.push(s.to_string());
                }
            }
        }
    };
    push_obj(info.pointer("/format/tags").and_then(|t| t.as_object()));
    if let Some(streams) = info.get("streams").and_then(|s| s.as_array()) {
        for s in streams {
            push_obj(s.get("tags").and_then(|t| t.as_object()));
        }
    }
    if let Some(chapters) = info.get("chapters").and_then(|c| c.as_array()) {
        for c in chapters {
            push_obj(c.get("tags").and_then(|t| t.as_object()));
        }
    }
    out
}

/// True when any format/stream/chapter tag key or value mentions OBS.
fn detect_obs_origin(info: &Value) -> (bool, Option<String>) {
    let encoder = info
        .pointer("/format/tags/encoder")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let from_obs = tag_strings(info)
        .iter()
        .any(|s| s.to_ascii_lowercase().contains("obs"));
    (from_obs, encoder)
}

fn parse_rate(raw: &str) -> Option<f64> {
    if raw.is_empty() || raw == "0/0" || raw == "N/A" {
        return None;
    }
    if let Some((n, d)) = raw.split_once('/') {
        let n: f64 = n.parse().ok()?;
        let d: f64 = d.parse().ok()?;
        if d == 0.0 {
            return None;
        }
        return Some(n / d);
    }
    raw.parse().ok()
}

fn stream_rate(stream: &Value, key: &str) -> Option<f64> {
    stream.get(key).and_then(|v| v.as_str()).and_then(parse_rate)
}

fn display_codec_name(raw: &str) -> String {
    match raw.to_ascii_lowercase().as_str() {
        "hevc" | "h265" => "HEVC".into(),
        "h264" | "avc1" => "H.264".into(),
        "vp9" => "VP9".into(),
        "vp8" => "VP8".into(),
        "av1" => "AV1".into(),
        "prores" | "prores_ks" | "prores_aw" => "ProRes".into(),
        "ffv1" => "FFV1".into(),
        "dnxhd" => "DNxHD".into(),
        "dnxhr" => "DNxHR".into(),
        "mpeg2video" | "mpeg2" => "MPEG-2".into(),
        "mpeg4" => "MPEG-4".into(),
        "theora" => "Theora".into(),
        "mjpeg" => "MJPEG".into(),
        "qtrle" => "Animation".into(),
        "png" => "PNG".into(),
        "hap" => "HAP".into(),
        "cfhd" => "CineForm".into(),
        "utvideo" => "Ut Video".into(),
        "rawvideo" => "Raw".into(),
        "aac" => "AAC".into(),
        "opus" => "Opus".into(),
        "mp3" => "MP3".into(),
        "flac" => "FLAC".into(),
        "pcm_s16le" | "pcm_s24le" | "pcm_s32le" | "pcm_f32le" => "PCM".into(),
        "" | "—" => "—".into(),
        other => other.to_ascii_uppercase(),
    }
}

fn frame_timing(r: Option<f64>, avg: Option<f64>) -> (f64, FrameTiming) {
    match (r, avg) {
        (Some(rr), Some(aa)) => {
            let fps = if aa > 0.0 { aa } else { rr };
            let timing = if (rr - aa).abs() <= (0.05_f64).max(rr * 0.002) {
                FrameTiming::Cfr
            } else {
                FrameTiming::Vfr
            };
            (fps, timing)
        }
        (Some(rr), None) => (rr, FrameTiming::Unknown),
        (None, Some(aa)) => (aa, FrameTiming::Unknown),
        (None, None) => (0.0, FrameTiming::Unknown),
    }
}

/// Human file size (e.g. `354 MB`).
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Output vs same-stem input size for UI (↓ green smaller / ↑ red larger).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeDelta {
    /// Output smaller; `pct` is reduction; `bytes` is absolute savings.
    Smaller { pct: u32, bytes: u64 },
    /// Output larger; `pct` is growth; `bytes` is absolute increase.
    Larger { pct: u32, bytes: u64 },
    Same,
}

/// Compare output size to input. `None` if input size unknown.
pub fn size_delta(input_bytes: u64, output_bytes: u64) -> Option<SizeDelta> {
    if input_bytes == 0 {
        return None;
    }
    let pct = (1.0 - output_bytes as f64 / input_bytes as f64) * 100.0;
    if pct >= 0.05 {
        Some(SizeDelta::Smaller {
            pct: pct.round() as u32,
            bytes: input_bytes.saturating_sub(output_bytes),
        })
    } else if pct <= -0.05 {
        Some(SizeDelta::Larger {
            pct: (-pct).round() as u32,
            bytes: output_bytes.saturating_sub(input_bytes),
        })
    } else {
        Some(SizeDelta::Same)
    }
}

/// Collapsing length: `M:SS` or `H:MM:SS` (no frame digits).
pub fn format_length(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "—".into();
    }
    let total = seconds.round() as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Bitrate kbps from a stream/format `bit_rate` field already in ffprobe JSON (no extra process).
fn json_bitrate_kbps(v: Option<&Value>) -> Option<u32> {
    let raw = v?;
    let bps = raw
        .as_u64()
        .or_else(|| raw.as_str().and_then(|s| s.parse().ok()))?;
    (bps > 0).then_some(((bps + 500) / 1000).max(1) as u32)
}

/// Fast list/chip probe: one `ffprobe` JSON — no packet peeks, no peak sampling.
pub fn media_summary_fast(path: &Path) -> Result<MediaSummary, String> {
    let info = file_info(path)?;
    let fmt = info.get("format").cloned().unwrap_or(Value::Null);
    let duration_s = fmt.get("duration").and_then(json_f64).unwrap_or(0.0);
    let file_size_bytes = fmt
        .get("size")
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(0);
    let container = container_label(path, fmt.get("format_name").and_then(|v| v.as_str()));

    let video = first_video_stream(&info);
    let audio = first_audio_stream(&info);
    let width = video.and_then(|v| v.get("width")).and_then(json_u32).unwrap_or(0);
    let height = video.and_then(|v| v.get("height")).and_then(json_u32).unwrap_or(0);
    let (fps, timing) = frame_timing(
        video.and_then(|v| stream_rate(v, "r_frame_rate")),
        video.and_then(|v| stream_rate(v, "avg_frame_rate")),
    );

    let video_codec = display_codec_name(
        video
            .and_then(|v| v.get("codec_name"))
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    );
    let video_profile = video
        .and_then(|v| v.get("profile"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != "unknown")
        .map(|s| s.to_string());

    let audio_streams: Vec<&Value> = info
        .get("streams")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter(|s| s.get("codec_type").and_then(|v| v.as_str()) == Some("audio"))
                .collect()
        })
        .unwrap_or_default();
    let audio_track_count = audio_streams.len();
    let audio_codec = display_codec_name(
        audio
            .and_then(|v| v.get("codec_name"))
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    );
    let audio_profile = audio
        .and_then(|v| v.get("profile"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != "unknown")
        .map(|s| s.to_string());
    let audio_channels = audio
        .and_then(|v| v.get("channels"))
        .and_then(json_u32)
        .unwrap_or(0);
    let audio_layout = audio
        .and_then(|v| v.get("channel_layout"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if audio_channels > 0 {
                format!("{audio_channels}ch")
            } else {
                "—".into()
            }
        });
    let audio_sample_rate = audio
        .and_then(|v| v.get("sample_rate"))
        .and_then(|v| {
            v.as_u64()
                .map(|n| n as u32)
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(0);

    // Cheap placeholders from tagged bit_rate only — UI shows "…" until enrich fills peeks.
    let avg_video_kbps = json_bitrate_kbps(video.and_then(|v| v.get("bit_rate")))
        .or_else(|| json_bitrate_kbps(fmt.get("bit_rate")))
        .unwrap_or(0);
    let peak_video_kbps = avg_video_kbps;
    let peak_audio_kbps = json_bitrate_kbps(audio.and_then(|v| v.get("bit_rate"))).unwrap_or(0);

    let chapter_count = info
        .get("chapters")
        .and_then(|c| c.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let (from_obs, encoder) = detect_obs_origin(&info);

    Ok(MediaSummary {
        container,
        duration_s,
        file_size_bytes,
        width,
        height,
        fps,
        timing,
        video_codec,
        video_profile,
        audio_codec,
        audio_profile,
        audio_channels,
        audio_layout,
        audio_sample_rate,
        audio_track_count,
        peak_video_kbps,
        avg_video_kbps,
        peak_audio_kbps,
        chapter_count,
        has_obs_chapters: chapter_count > 0,
        from_obs,
        encoder,
        has_alpha: video_has_alpha(&info),
        pix_fmt: video
            .and_then(|v| v.get("pix_fmt"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase(),
    })
}

/// Fill peak/avg bitrates via packet sampling (slow — defer after chips are on screen).
pub fn enrich_summary_bitrates(path: &Path, summary: &mut MediaSummary) {
    let info = file_info(path).ok();
    let (peak_video_kbps, _) = match &info {
        Some(info) => video_bitrate_k(path, info),
        None => (summary.peak_video_kbps, String::new()),
    };
    let avg_video_kbps = video_avg_bitrate_k(path, summary.duration_s);
    let peak_audio_kbps = sampled_peak_bitrate_bps(path, "a:0", summary.duration_s)
        .map(|bps| ((bps + 500) / 1000).max(1) as u32)
        .unwrap_or_else(|| audio_bitrate_k(path));
    summary.peak_video_kbps = peak_video_kbps;
    if avg_video_kbps > 0 {
        summary.avg_video_kbps = avg_video_kbps;
    }
    summary.peak_audio_kbps = peak_audio_kbps;
}

/// Full probe (fast + bitrate enrich). Prefer [`media_summary_fast`] + [`enrich_summary_bitrates`] for UI.
pub fn media_summary(path: &Path) -> Result<MediaSummary, String> {
    let mut s = media_summary_fast(path)?;
    enrich_summary_bitrates(path, &mut s);
    Ok(s)
}

/// Keep ffmpeg/ffprobe stderr short for UI — full dumps (e.g. per-packet AV1 decode spam) freeze egui.
pub fn truncate_cli_err(stderr: &str, max_chars: usize) -> String {
    let trimmed = stderr.trim();
    if trimmed.len() <= max_chars {
        return trimmed.to_string();
    }
    // Prefer the first meaningful lines (root cause) over the tail of packet spam.
    let mut out = String::new();
    for line in trimmed.lines().take(12) {
        if out.len() + line.len() + 1 > max_chars {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    if out.is_empty() {
        out.push_str(&trimmed[..max_chars]);
    }
    out.push_str("\n… (stderr truncated)");
    out
}

/// Grab one letterboxed RGBA frame near 1s (or 10% of duration) via ffmpeg.
pub fn thumbnail_rgba(path: &Path, duration_s: f64) -> Result<RgbaThumb, String> {
    let seek = if duration_s > 2.0 {
        (duration_s * 0.1).clamp(0.5, 30.0)
    } else {
        0.0
    };
    thumbnail_rgba_at(path, seek)
}

/// Grab one letterboxed RGBA frame at `seek_s` (input seek before decode).
pub fn thumbnail_rgba_at(path: &Path, seek_s: f64) -> Result<RgbaThumb, String> {
    let seek = seek_s.max(0.0);
    // Default decoder first; on AV1/libaom level failures retry NVDEC then generic `av1`.
    match thumbnail_rgba_at_with(path, seek, None, None) {
        Ok(t) => Ok(t),
        Err(e) if thumb_err_suggests_av1_decoder_gap(&e) => {
            // Keep full stderr in the Err so the UI can offer Copy while only painting a truncate.
            thumbnail_rgba_at_with(path, seek, Some("cuda"), Some("av1_cuvid"))
                .or_else(|_| thumbnail_rgba_at_with(path, seek, None, Some("av1")))
                .map_err(|e2| format!("{e}\n--- fallback ---\n{e2}"))
        }
        Err(e) => Err(e),
    }
}

fn thumb_err_suggests_av1_decoder_gap(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("libaom-av1")
        || e.contains("seq_level_idx")
        || e.contains("bitstream not supported")
        || e.contains("no sequence header")
}

fn thumbnail_rgba_at_with(
    path: &Path,
    seek: f64,
    hwaccel: Option<&str>,
    decoder: Option<&str>,
) -> Result<RgbaThumb, String> {
    let vf = format!(
        "scale={THUMB_W}:{THUMB_H}:force_original_aspect_ratio=decrease,pad={THUMB_W}:{THUMB_H}:(ow-iw)/2:(oh-ih)/2:black"
    );
    let path_s = path.to_str().ok_or("non-utf8 path")?;
    let seek_s = format!("{seek:.3}");
    let mut args: Vec<&str> = vec![
        "-hide_banner",
        "-loglevel",
        "error",
        // Bail on first decode failure — avoids multi‑thousand‑line libaom spam.
        "-xerror",
    ];
    if let Some(h) = hwaccel {
        args.extend(["-hwaccel", h]);
    }
    if let Some(d) = decoder {
        args.extend(["-c:v", d]);
    }
    args.extend([
        "-ss",
        &seek_s,
        "-i",
        path_s,
        "-frames:v",
        "1",
        "-vf",
        &vf,
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgba",
        "-",
    ]);
    let out = Command::new("ffmpeg")
        .args(&args)
        .output()
        .map_err(|e| format!("ffmpeg thumb: {e}"))?;
    if !out.status.success() {
        // Full stderr kept for Copy; UI truncates for display. `-xerror` keeps this short in practice.
        return Err(format!(
            "ffmpeg thumb failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let expect = (THUMB_W * THUMB_H * 4) as usize;
    if out.stdout.len() < expect {
        return Err(format!(
            "thumb truncated: got {} bytes, want {expect}",
            out.stdout.len()
        ));
    }
    Ok(RgbaThumb {
        width: THUMB_W,
        height: THUMB_H,
        rgba: out.stdout[..expect].to_vec(),
    })
}

fn format_duration_s(info: &Value) -> f64 {
    info.get("format")
        .and_then(|fmt| fmt.get("duration"))
        .and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_f64()))
        .unwrap_or(0.0)
}

/// Average **video-stream** kbps (v:0 only — never container/total).
fn video_avg_bitrate_k(path: &Path, duration_s: f64) -> u32 {
    if let Some(bps) = stream_bitrate_bps(path, "v:0") {
        return ((bps + 500) / 1000).max(1) as u32;
    }
    // Untagged streams (common on WebM): sample packets over a longer window.
    let sample_s = if duration_s > 0.0 {
        duration_s.min(120.0).max(1.0)
    } else {
        30.0
    };
    if let Some(bps) = packet_bitrate_bps(path, "v:0", sample_s.ceil() as u32, 0.0) {
        return ((bps + 500) / 1000).max(1) as u32;
    }
    0
}

fn video_duration_s(path: &Path, info: &Value) -> f64 {
    let mut dur = format_duration_s(info);
    if dur <= 0.0 {
        if let Ok(fresh) = file_info(path) {
            dur = format_duration_s(&fresh);
        }
    }
    dur
}

/// Peak **video-stream** kbps (v:0 packet peak; never container/total).
pub fn video_peak_bitrate_k(path: &Path, info: &Value) -> (u32, String) {
    let dur = video_duration_s(path, info);
    if let Some(bps) = video_peak_bitrate_bps(path, dur) {
        return (
            ((bps + 500) / 1000).max(1) as u32,
            "video peak 1s (v:0)".into(),
        );
    }
    if let Some(bps) = stream_bitrate_bps(path, "v:0") {
        return (
            ((bps + 500) / 1000).max(1) as u32,
            "video stream bit_rate (v:0)".into(),
        );
    }
    (
        DEFAULT_BITRATE_K,
        format!("default ({DEFAULT_BITRATE_K}k; video probe failed)"),
    )
}

/// Encode ceiling kbps: average by default (CBR-oriented); optional 1s peak.
pub fn video_ceiling_k(path: &Path, info: &Value, use_peak: bool) -> (u32, String) {
    if use_peak {
        return video_peak_bitrate_k(path, info);
    }
    let dur = video_duration_s(path, info);
    let avg = video_avg_bitrate_k(path, dur);
    if avg > 0 {
        return (avg, "video avg (v:0)".into());
    }
    // Avg probe empty — fall back through peak / stream / default.
    let (k, label) = video_peak_bitrate_k(path, info);
    (k, format!("{label}; avg unavailable"))
}

/// Peak **video-stream** kbps for preview / legacy callers.
pub fn video_bitrate_k(path: &Path, info: &Value) -> (u32, String) {
    video_peak_bitrate_k(path, info)
}

/// Source audio kbps for Opus/Vorbis→AAC remux.
pub fn audio_bitrate_k(path: &Path) -> u32 {
    if let Some(bps) = stream_bitrate_bps(path, "a:0") {
        return ((bps + 500) / 1000).max(1) as u32;
    }
    if let Some(bps) = packet_bitrate_bps(path, "a:0", 30, 0.0) {
        return ((bps + 500) / 1000).max(1) as u32;
    }
    192
}

/// Check ffmpeg/ffprobe are on PATH.
pub fn require_tools() -> Result<(), String> {
    for tool in ["ffmpeg", "ffprobe"] {
        Command::new(tool)
            .arg("-version")
            .output()
            .map_err(|_| format!("{tool} not found on PATH"))?;
    }
    Ok(())
}
