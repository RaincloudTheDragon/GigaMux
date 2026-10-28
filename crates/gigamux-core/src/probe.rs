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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameTiming {
    Cfr,
    Vfr,
    Unknown,
}

/// Source media summary for the preview pane.
#[derive(Debug, Clone)]
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
}

impl MediaSummary {
    /// Bump when summary fields change so UI preview cache re-fetches.
    pub const SCHEMA: u32 = 3;
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

/// Compact list/meta tag from a summary: e.g. `VP9 α`, `HEVC`, `ProRes α`.
pub fn codec_tag(summary: &MediaSummary) -> String {
    if summary.has_alpha && summary.video_codec != "—" {
        format!("{} α", summary.video_codec)
    } else {
        summary.video_codec.clone()
    }
}

pub fn video_codec_name(info: &Value) -> String {
    first_video_stream(info)
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

/// Probe codecs, peak bitrates, duration, container, and chapter presence.
pub fn media_summary(path: &Path) -> Result<MediaSummary, String> {
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

    let (peak_video_kbps, _) = video_bitrate_k(path, &info);
    let avg_video_kbps = video_avg_bitrate_k(path, duration_s);
    let peak_audio_kbps = sampled_peak_bitrate_bps(path, "a:0", duration_s)
        .map(|bps| ((bps + 500) / 1000).max(1) as u32)
        .unwrap_or_else(|| audio_bitrate_k(path));

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
    })
}

/// Grab one letterboxed RGBA frame near 1s (or 10% of duration) via ffmpeg.
pub fn thumbnail_rgba(path: &Path, duration_s: f64) -> Result<RgbaThumb, String> {
    let seek = if duration_s > 2.0 {
        (duration_s * 0.1).clamp(0.5, 30.0)
    } else {
        0.0
    };
    let vf = format!(
        "scale={THUMB_W}:{THUMB_H}:force_original_aspect_ratio=decrease,pad={THUMB_W}:{THUMB_H}:(ow-iw)/2:(oh-ih)/2:black"
    );
    let out = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-ss",
            &format!("{seek:.3}"),
            "-i",
            path.to_str().ok_or("non-utf8 path")?,
            "-frames:v",
            "1",
            "-vf",
            &vf,
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-",
        ])
        .output()
        .map_err(|e| format!("ffmpeg thumb: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ffmpeg thumb failed: {}",
            String::from_utf8_lossy(&out.stderr)
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
