//! Output encode targets (opaque / α codec × container) and audio remux rules.

use serde::{Deserialize, Serialize};

/// Opaque (non-alpha) video encoder choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OpaqueVideo {
    #[default]
    Hevc,
    H264,
    Av1,
    Vp9,
    Vp8,
}

impl OpaqueVideo {
    pub const ALL: [Self; 5] = [Self::Hevc, Self::H264, Self::Av1, Self::Vp9, Self::Vp8];

    pub fn label(self) -> &'static str {
        match self {
            Self::Hevc => "HEVC",
            Self::H264 => "H.264",
            Self::Av1 => "AV1",
            Self::Vp9 => "VP9",
            Self::Vp8 => "VP8",
        }
    }

    /// Cycle HEVC → H.264 → AV1 → VP9 → VP8 → …
    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|&x| x == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    /// True for software libvpx opaque encodes (CRF0 path).
    pub fn is_libvpx(self) -> bool {
        matches!(self, Self::Vp9 | Self::Vp8)
    }

    /// ffmpeg `-c:v` (NVENC or libvpx).
    pub fn ffmpeg_encoder(self) -> &'static str {
        match self {
            Self::Hevc => "hevc_nvenc",
            Self::H264 => "h264_nvenc",
            Self::Av1 => "av1_nvenc",
            Self::Vp9 => "libvpx-vp9",
            Self::Vp8 => "libvpx",
        }
    }

    /// Expected ffprobe `codec_name` values for check.
    pub fn probe_names(self) -> &'static [&'static str] {
        match self {
            Self::Hevc => &["hevc", "h265"],
            Self::H264 => &["h264", "avc1"],
            Self::Av1 => &["av1"],
            Self::Vp9 => &["vp9"],
            Self::Vp8 => &["vp8"],
        }
    }
}

/// Alpha video encoder choice (libvpx).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AlphaVideo {
    #[default]
    Vp9,
    Vp8,
}

impl AlphaVideo {
    pub const ALL: [Self; 2] = [Self::Vp9, Self::Vp8];

    pub fn label(self) -> &'static str {
        match self {
            Self::Vp9 => "VP9",
            Self::Vp8 => "VP8",
        }
    }

    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|&x| x == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn ffmpeg_encoder(self) -> &'static str {
        match self {
            Self::Vp9 => "libvpx-vp9",
            Self::Vp8 => "libvpx",
        }
    }

    pub fn probe_names(self) -> &'static [&'static str] {
        match self {
            Self::Vp9 => &["vp9"],
            Self::Vp8 => &["vp8"],
        }
    }
}

/// Output container / file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutContainer {
    #[default]
    Mp4,
    Mkv,
    Webm,
}

impl OutContainer {
    pub const ALL: [Self; 3] = [Self::Mp4, Self::Mkv, Self::Webm];

    pub fn ext(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mkv => "mkv",
            Self::Webm => "webm",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mp4 => ".mp4",
            Self::Mkv => ".mkv",
            Self::Webm => ".webm",
        }
    }

    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|&x| x == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn from_ext(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "mp4" => Some(Self::Mp4),
            "mkv" => Some(Self::Mkv),
            "webm" => Some(Self::Webm),
            _ => None,
        }
    }
}

/// Opaque codec × container legality (plan matrix + opaque VP8/VP9).
pub fn opaque_pair_ok(video: OpaqueVideo, container: OutContainer) -> bool {
    match (video, container) {
        (OpaqueVideo::Hevc | OpaqueVideo::H264, OutContainer::Webm) => false,
        // VP8/VP9 + mp4/mkv/webm all allowed; AV1 all; HEVC/H.264 → mp4|mkv.
        _ => true,
    }
}

/// Next legal opaque container for `video` (skips illegal pairs).
pub fn cycle_opaque_container(video: OpaqueVideo, current: OutContainer) -> OutContainer {
    let mut c = current;
    for _ in 0..OutContainer::ALL.len() {
        c = c.next();
        if opaque_pair_ok(video, c) {
            return c;
        }
    }
    clamp_opaque_container(video, current)
}

/// Next legal α container for `video`.
pub fn cycle_alpha_container(video: AlphaVideo, current: OutContainer) -> OutContainer {
    let mut c = current;
    for _ in 0..OutContainer::ALL.len() {
        c = c.next();
        if alpha_pair_ok(video, c) {
            return c;
        }
    }
    clamp_alpha_container(video, current)
}

/// α codec × container legality (no α → mp4).
pub fn alpha_pair_ok(_video: AlphaVideo, container: OutContainer) -> bool {
    !matches!(container, OutContainer::Mp4)
}

/// First legal container for an opaque codec (prefers current if ok).
pub fn clamp_opaque_container(video: OpaqueVideo, container: OutContainer) -> OutContainer {
    if opaque_pair_ok(video, container) {
        return container;
    }
    OutContainer::ALL
        .into_iter()
        .find(|&c| opaque_pair_ok(video, c))
        .unwrap_or(OutContainer::Mp4)
}

/// First legal container for an α codec (prefers current if ok).
pub fn clamp_alpha_container(video: AlphaVideo, container: OutContainer) -> OutContainer {
    if alpha_pair_ok(video, container) {
        return container;
    }
    OutContainer::ALL
        .into_iter()
        .find(|&c| alpha_pair_ok(video, c))
        .unwrap_or(OutContainer::Webm)
}

/// How audio should be encoded for the destination container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioEncode {
    Copy,
    Aac,
    Opus,
}

/// Decide copy vs recode from raw ffprobe audio `codec_name` + output container.
pub fn audio_encode_for(audio_codec_raw: &str, out: OutContainer) -> AudioEncode {
    let c = audio_codec_raw.to_ascii_lowercase();
    if c.is_empty() {
        return AudioEncode::Copy;
    }
    match out {
        OutContainer::Mkv => AudioEncode::Copy,
        OutContainer::Mp4 => {
            // Opus/Vorbis/FLAC/raw PCM are not remux-friendly into mp4 for our pipeline.
            if matches!(
                c.as_str(),
                "opus"
                    | "vorbis"
                    | "flac"
                    | "libopus"
                    | "pcm_s16le"
                    | "pcm_s24le"
                    | "pcm_s32le"
                    | "pcm_f32le"
                    | "pcm_u8"
            ) {
                AudioEncode::Aac
            } else {
                AudioEncode::Copy
            }
        }
        OutContainer::Webm => {
            // WebM wants Opus/Vorbis; AAC/MP3/etc. must be re-encoded.
            if matches!(c.as_str(), "opus" | "vorbis" | "libopus") {
                AudioEncode::Copy
            } else {
                AudioEncode::Opus
            }
        }
    }
}

/// True when destination forces an audio re-encode (not stream copy).
pub fn audio_must_recode(audio_codec_raw: &str, out: OutContainer) -> bool {
    !matches!(audio_encode_for(audio_codec_raw, out), AudioEncode::Copy)
}

/// Snapshot of encode target prefs for check / encode callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputTargets {
    pub opaque_video: OpaqueVideo,
    pub opaque_container: OutContainer,
    pub alpha_video: AlphaVideo,
    pub alpha_container: OutContainer,
}

impl Default for OutputTargets {
    fn default() -> Self {
        Self {
            opaque_video: OpaqueVideo::Hevc,
            opaque_container: OutContainer::Mp4,
            alpha_video: AlphaVideo::Vp9,
            alpha_container: OutContainer::Webm,
        }
    }
}

impl OutputTargets {
    pub fn container_for(self, has_alpha: bool) -> OutContainer {
        if has_alpha {
            self.alpha_container
        } else {
            self.opaque_container
        }
    }

    pub fn expected_name(self, input_name: &str, has_alpha: bool) -> String {
        let stem = std::path::Path::new(input_name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(input_name);
        format!("{stem}.{}", self.container_for(has_alpha).ext())
    }

    pub fn kind_label(self, has_alpha: bool) -> String {
        if has_alpha {
            format!("{}+α {}", self.alpha_video.label(), self.alpha_container.label())
        } else {
            format!("{} {}", self.opaque_video.label(), self.opaque_container.label())
        }
    }
}
