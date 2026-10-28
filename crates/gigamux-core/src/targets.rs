//! Output encode targets (opaque / α codec × container) and audio remux rules.

use serde::{Deserialize, Serialize};

/// Opaque (non-alpha) video encoder choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OpaqueVideo {
    Hevc,
    H264,
    #[default]
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

    /// True when this codec can use NVENC (`use_hardware`).
    pub fn is_nvenc(self) -> bool {
        matches!(self, Self::Hevc | Self::H264 | Self::Av1)
    }

    /// ffmpeg `-c:v` — NVENC when `hardware` and codec supports it, else CPU.
    pub fn ffmpeg_encoder(self, hardware: bool) -> &'static str {
        match self {
            Self::Hevc if hardware => "hevc_nvenc",
            Self::Hevc => "libx265",
            Self::H264 if hardware => "h264_nvenc",
            Self::H264 => "libx264",
            Self::Av1 if hardware => "av1_nvenc",
            Self::Av1 => "libaom-av1",
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
        // Alpha is always libvpx — NVENC has no VP8/VP9 encoder.
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

/// Map a source `pix_fmt` to an encoder-friendly form (keeps chroma + bit depth).
pub fn software_pix_fmt(src_pix: &str) -> &'static str {
    match src_pix.trim().to_ascii_lowercase().as_str() {
        "yuv420p10le" | "yuv420p10be" | "p010le" | "p010be" => "yuv420p10le",
        "yuv422p" | "yuvj422p" => "yuv422p",
        "yuv422p10le" | "yuv422p10be" => "yuv422p10le",
        "yuv444p" | "yuvj444p" | "gbrp" => "yuv444p",
        "yuv444p10le" | "yuv444p10be" | "gbrp10le" | "yuv444p16le" => "yuv444p10le",
        // Default / already-420 / unknown → 8-bit 4:2:0.
        _ => "yuv420p",
    }
}

/// Chroma (420/422/444) + bit depth (8/10) for convert detection.
fn pix_chroma_depth(pix: &str) -> (u16, u8) {
    let p = pix.trim().to_ascii_lowercase();
    let depth = if p.contains("10")
        || p.contains("12")
        || p.contains("16")
        || p.starts_with("p010")
        || p.starts_with("p012")
        || p.starts_with("p016")
        || p.starts_with("p210")
        || p.starts_with("p212")
        || p.starts_with("p216")
    {
        10
    } else {
        8
    };
    let chroma = if p.contains("444") || p == "gbrp" || p == "gbrap" {
        444
    } else if p.contains("422") || p == "nv16" {
        422
    } else {
        420
    };
    (chroma, depth)
}

/// True when two pix_fmts are the same chroma family + bit depth (e.g. yuv420p10le ≡ p010le).
fn same_chroma_depth(a: &str, b: &str) -> bool {
    pix_chroma_depth(a) == pix_chroma_depth(b)
}

/// Closest NVENC-friendly format to `src_pix` for this codec (GeForce-class capabilities).
/// Matches source when possible; otherwise keeps bit depth and drops unsupported chroma (no 4:2:2).
pub fn nvenc_pix_fmt(src_pix: &str, opaque: OpaqueVideo) -> &'static str {
    let ideal = software_pix_fmt(src_pix);
    match opaque {
        OpaqueVideo::Hevc => match ideal {
            "yuv420p" => "yuv420p",
            "yuv420p10le" => "p010le",
            // 4:2:2 not available on consumer NVENC — keep depth, go 4:2:0.
            "yuv422p" => "yuv420p",
            "yuv422p10le" => "p010le",
            "yuv444p" => "yuv444p",
            "yuv444p10le" => "yuv444p16le",
            _ => "yuv420p",
        },
        OpaqueVideo::H264 => match ideal {
            "yuv420p" => "yuv420p",
            // h264_nvenc on GeForce: no 10-bit.
            "yuv420p10le" | "yuv422p" | "yuv422p10le" => "yuv420p",
            "yuv444p" => "yuv444p",
            "yuv444p10le" => "yuv444p",
            _ => "yuv420p",
        },
        // AV1 NVENC (Ada+): treat as 4:2:0 (+10-bit); no reliable 4:2:2/4:4:4 here.
        OpaqueVideo::Av1 => match ideal {
            "yuv420p10le" | "yuv422p10le" | "yuv444p10le" => "p010le",
            _ => "yuv420p",
        },
        OpaqueVideo::Vp9 | OpaqueVideo::Vp8 => "yuv420p",
    }
}

/// Pixel format we will feed the encoder for this target.
/// CPU keeps source chroma; NVENC matches when the codec/GPU allows, else nearest fallback.
pub fn delivery_pix_fmt(
    src_pix: &str,
    has_alpha: bool,
    use_hardware: bool,
    opaque: OpaqueVideo,
) -> &'static str {
    if has_alpha {
        return "yuva420p";
    }
    if opaque.is_libvpx() {
        return "yuv420p";
    }
    if use_hardware && opaque.is_nvenc() {
        return nvenc_pix_fmt(src_pix, opaque);
    }
    software_pix_fmt(src_pix)
}

/// Short human label for an ffprobe `pix_fmt` (chroma + bit depth).
pub fn pix_fmt_label(pix: &str) -> String {
    let p = pix.to_ascii_lowercase();
    match p.as_str() {
        "" => "unknown".into(),
        "yuv420p" | "yuvj420p" | "nv12" | "nv21" => "8-bit 4:2:0".into(),
        "yuv420p10le" | "yuv420p10be" | "p010le" | "p010be" => "10-bit 4:2:0".into(),
        "yuv422p" | "yuvj422p" => "8-bit 4:2:2".into(),
        "yuv422p10le" | "yuv422p10be" => "10-bit 4:2:2".into(),
        "yuv444p" | "yuvj444p" | "gbrp" => "8-bit 4:4:4".into(),
        "yuv444p10le" | "yuv444p10be" | "gbrp10le" | "yuv444p16le" => "10-bit 4:4:4".into(),
        "yuva420p" => "8-bit 4:2:0 + α".into(),
        "yuva422p" => "8-bit 4:2:2 + α".into(),
        "yuva444p" | "gbrap" => "8-bit 4:4:4 + α".into(),
        other => other.into(),
    }
}

/// True when encode will change chroma and/or bit-depth vs the source ideal.
/// α tagging alone (e.g. `yuv420p` + alpha_mode → `yuva420p`) is not a chroma convert.
pub fn pix_fmt_will_convert(
    src_pix: &str,
    has_alpha: bool,
    use_hardware: bool,
    opaque: OpaqueVideo,
) -> bool {
    let s = src_pix.trim().to_ascii_lowercase();
    if s.is_empty() {
        return false;
    }
    let dest = delivery_pix_fmt(&s, has_alpha, use_hardware, opaque);
    if has_alpha {
        return !same_chroma_depth(&s, dest);
    }
    let ideal = software_pix_fmt(&s);
    !same_chroma_depth(ideal, dest)
}

/// Amber UI copy when chroma/bit-depth will be converted; `None` if matched.
pub fn pix_fmt_convert_warning(
    src_pix: &str,
    has_alpha: bool,
    use_hardware: bool,
    opaque: OpaqueVideo,
) -> Option<String> {
    if !pix_fmt_will_convert(src_pix, has_alpha, use_hardware, opaque) {
        return None;
    }
    let dest = delivery_pix_fmt(src_pix, has_alpha, use_hardware, opaque);
    Some(format!(
        "{} will be converted to {}",
        pix_fmt_label(src_pix),
        pix_fmt_label(dest)
    ))
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
            opaque_video: OpaqueVideo::Av1,
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
