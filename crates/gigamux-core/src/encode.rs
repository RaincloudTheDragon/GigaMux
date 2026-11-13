//! Local sequential encode (opaque NVENC / VP9 QSV / CPU; alpha libvpx or QSV+libvpx hybrid). No chunking.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::chunk::{
    chunk_frame_range, chunk_output_name, estimate_total_frames, list_chunk_paths,
    parse_chunk_filename, parse_fps, write_ffconcat, CHUNK_SEEK_PAD_S,
};
use crate::flamenco::{FarmChunkCheck, FarmCleanup, FarmFinalCheck, FarmVmafSpec, FlamencoIntegrity};
use crate::vmaf::vmaf_sidecar_path;
use crate::gpu::{encoder_metadata_tags, GpuChoice, HwCatalog};
use crate::job_log::{JobLog, JobLogKind};
use crate::probe::{
    audio_bitrate_k, audio_codec_name, file_info, require_tools, thumbnail_rgba_at,
    video_ceiling_k, video_codec_name, video_color_meta, video_has_alpha, video_pix_fmt,
    RgbaThumb, VideoColorMeta,
};
use crate::prefs::EncodeSlot;
use crate::targets::{
    audio_encode_for, delivery_pix_fmt, AlphaVideo, AudioEncode, OpaqueVideo, OutContainer,
    TestBatch,
};
use crate::webm_alpha::{mux_vp9_alpha_webm, wants_qsv_alpha_hybrid};

/// Frame-accurate farm chunk trim (video-only encode).
#[derive(Debug, Clone, Copy)]
struct ChunkTrim {
    start_frame: u64,
    frame_count: u64,
    fps: f64,
}

/// Same family as check `INPUT_EXTS` — what the batch encoder will pick up.
const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "mov", "webm", "m4v", "avi", "mpg", "mpeg", "m2v", "m2ts", "mts", "ts", "mxf",
    "ogv", "ogg", "wmv", "flv", "3gp", "3g2",
];

/// Progress / log events from a batch encode.
#[derive(Debug, Clone)]
pub enum EncodeEvent {
    Log(String),
    /// Work queue sized (files that still need encoding).
    BatchStarted {
        total_files: usize,
        total_duration_s: f64,
    },
    FileStarted {
        name: String,
        index: usize,
        total: usize,
        duration_s: f64,
    },
    /// Live ffmpeg out-time for the current file (seconds).
    FileProgress { out_time_s: f64 },
    /// Source frame at current out-time (throttled; for UI live preview).
    LiveThumb { name: String, thumb: RgbaThumb },
    FileSkipped { name: String },
    FileDone { name: String },
    FileFailed { name: String, error: String },
    Finished { ok: usize, skipped: usize, failed: usize },
}

/// Options for one batch run.
#[derive(Debug, Clone)]
pub struct EncodeOptions {
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
    /// Legacy; prefer each [`EncodeSlot::gpu`]. Kept so older call sites still compile.
    pub gpu: GpuChoice,
    /// Local probe snapshot for `GigaMux_hw` labels (generation / device).
    pub hw: HwCatalog,
    pub preserve_obs_metadata: bool,
    /// Write `GigaMux_encoder` / `GigaMux_hw` into outputs.
    pub write_encoder_metadata: bool,
    /// Ceiling from 1s video peak when true; otherwise average rate.
    pub use_peak_bitrate: bool,
    /// Multi full-frame A/B: write `test1_*`…`testN_*` with `slot1`… matching `test_slot_count`.
    pub test_mode: bool,
    /// Active test slot count when `test_mode` (clamped 2..=4). Ignored when test mode off.
    pub test_slot_count: u8,
    /// Primary encode settings (also used when `test_mode` is off).
    pub slot1: EncodeSlot,
    /// Secondary encode settings (test mode only).
    pub slot2: EncodeSlot,
    /// Third encode settings (test mode, count >= 3).
    pub slot3: EncodeSlot,
    /// Fourth encode settings (test mode, count >= 4).
    pub slot4: EncodeSlot,
    /// When set, only encode these basenames (right-click selection). `None` = all inputs.
    pub only_names: Option<Vec<String>>,
}

impl EncodeOptions {
    fn slot_for(&self, batch: TestBatch) -> &EncodeSlot {
        match batch {
            TestBatch::One => &self.slot1,
            TestBatch::Two => &self.slot2,
            TestBatch::Three => &self.slot3,
            TestBatch::Four => &self.slot4,
        }
    }
}

/// One-file encode request for Flamenco (stock worker `exec` ffmpeg).
#[derive(Debug, Clone)]
pub struct FarmEncodeRequest {
    pub input: PathBuf,
    pub output_dir: PathBuf,
    /// Legacy fallback when `slot.gpu` is unused by older callers; prefer `slot.gpu`.
    pub gpu: GpuChoice,
    pub hw: HwCatalog,
    pub preserve_obs_metadata: bool,
    pub write_encoder_metadata: bool,
    pub use_peak_bitrate: bool,
    pub slot: EncodeSlot,
    /// `None` = normal deliver name; `Some` = `test1_` / `test2_` prefix.
    pub test_batch: Option<TestBatch>,
    /// `1` = single full encode; `>1` = frame-chunked farm graph (alpha / software; opaque HW may ignore).
    pub chunks: u32,
    /// Hybrid only: when true and `chunks > 1`, also split the QSV color IVF across workers.
    pub chunk_qsv_color: bool,
}

/// ffmpeg argv for a Flamenco `misc`/`exec` task (`argv[0]` is the program name).
#[derive(Debug, Clone)]
pub struct FarmFfmpegCommand {
    pub output: PathBuf,
    /// Full argv including leading `"ffmpeg"` (or replacement exe basename).
    pub argv: Vec<String>,
    pub bitrate_k: u32,
    pub bitrate_label: String,
}

/// Chunked Flamenco job: N video-only encodes + stage mux (ffconcat+audio) + remux.
#[derive(Debug, Clone)]
pub struct FarmChunkedJob {
    pub final_output: PathBuf,
    pub chunk_paths: Vec<PathBuf>,
    pub encode_argvs: Vec<Vec<String>>,
    /// Stage: ffconcat video + source audio/subs → `{stem}.vmux.mkv` (no `.vcat` intermediate).
    pub mux_argv: Vec<String>,
    /// Remux: `.vmux.mkv` → final deliver (`-c copy`, all streams); separate Flamenco task.
    pub mux_remux_argv: Vec<String>,
    /// Sum of chunk durations (ensure-out expect for vmux / final).
    pub out_expect_dur_s: f64,
    /// Written by the submitter onto shared storage before POST.
    pub ffconcat_path: PathBuf,
    pub chunk_durations_s: Vec<f64>,
    pub bitrate_k: u32,
    pub bitrate_label: String,
}

/// One encode stage in a farm QSV+libvpx alpha hybrid job.
#[derive(Debug, Clone)]
pub struct FarmHybridEncodeTask {
    pub name: String,
    pub task_type: String,
    pub argv: Vec<String>,
    /// When set, task uses `ensure-chunk` (path + expect_dur).
    pub ensure: Option<FarmChunkCheck>,
}

/// Farm QSV color + libvpx alpha → Rust remux → audio deliver.
/// Intermediates sit flat in the output dir (opaque-style `{stem}.cNNNofMMM.ivf` for alpha chunks).
#[derive(Debug, Clone)]
pub struct FarmHybridJob {
    pub final_output: PathBuf,
    pub color_tasks: Vec<FarmHybridEncodeTask>,
    pub alpha_tasks: Vec<FarmHybridEncodeTask>,
    /// ffmpeg concat demuxer → single `{stem}.color.ivf` (when color was chunked).
    pub color_concat_argv: Option<Vec<String>>,
    /// ffmpeg concat demuxer → single `{stem}.alpha.ivf` (when alpha was chunked).
    pub alpha_concat_argv: Option<Vec<String>>,
    /// ffconcat lists the submitter must write before POST.
    pub ffconcat_paths: Vec<(PathBuf, Vec<PathBuf>, Vec<f64>)>,
    /// `gigamux-check` args: `mux-alpha-webm` + paths + optional colour tags.
    pub remux_check_args: Vec<String>,
    pub audio_argv: Vec<String>,
    /// Video-only expect (IVF concat `ensure-out`); ignores longer audio on the source.
    pub video_expect_dur_s: f64,
    /// Container/format expect for the final audio mux deliver (may include A/V skew).
    pub out_expect_dur_s: f64,
    pub bitrate_k: u32,
    pub bitrate_label: String,
}

/// List encodeable media in a directory (non-recursive).
pub fn list_inputs(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read input dir: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read dir entry: {e}"))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();
        if VIDEO_EXTS.contains(&ext.as_str()) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn deliver_path(
    input: &Path,
    output_dir: &Path,
    container: OutContainer,
    batch: Option<TestBatch>,
) -> PathBuf {
    // Last-dot only: `foo.ts.mp4` stem is `foo.ts` (a name with a dot, not a second extension).
    // Do not use Path::with_extension on the stem — it would replace `.ts` and yield `foo.mp4`.
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("out");
    let file_stem = match batch {
        Some(b) => format!("{}{stem}", b.prefix()),
        None => stem.to_string(),
    };
    output_dir.join(format!("{file_stem}.{}", container.ext()))
}

fn append_audio_args(cmd: &mut Vec<String>, input: &Path, info: &serde_json::Value, out: OutContainer) {
    let raw = audio_codec_name(info);
    match audio_encode_for(&raw, out) {
        AudioEncode::Copy => {
            cmd.extend(["-c:a".into(), "copy".into(), "-map".into(), "0".into()]);
        }
        AudioEncode::Aac => {
            let a_k = audio_bitrate_k(input);
            cmd.extend([
                "-c:a".into(),
                "aac".into(),
                "-b:a".into(),
                format!("{a_k}k"),
                "-map".into(),
                "0".into(),
            ]);
        }
        AudioEncode::Opus => {
            let a_k = audio_bitrate_k(input);
            cmd.extend([
                "-c:a".into(),
                "libopus".into(),
                "-b:a".into(),
                format!("{a_k}k"),
                "-map".into(),
                "0".into(),
            ]);
        }
    }
}

fn append_libvpx_opaque_args(
    cmd: &mut Vec<String>,
    encoder: &str,
    ceiling: &str,
    bufsize: &str,
    cq: u32,
) {
    // Constrained CRF; libvpx ≥1.17 needs -b:v with -maxrate. VP8/VP9 path is 4:2:0-only here.
    cmd.extend([
        "-c:v".into(),
        encoder.into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-auto-alt-ref".into(),
        "0".into(),
        "-deadline".into(),
        "good".into(),
        "-cpu-used".into(),
        "0".into(),
        "-threads".into(),
        "0".into(),
        "-tile-columns".into(),
        "2".into(),
        "-tile-rows".into(),
        "1".into(),
        "-lag-in-frames".into(),
        "25".into(),
        "-row-mt".into(),
        "1".into(),
        "-crf".into(),
        cq.to_string(),
        "-b:v".into(),
        ceiling.into(),
        "-maxrate".into(),
        ceiling.into(),
        "-bufsize".into(),
        bufsize.into(),
    ]);
}

/// Opaque `vp9_qsv` — VBR capped at source ceiling (no CRF0/CQ0 equivalent on QSV).
fn append_vp9_qsv_args(cmd: &mut Vec<String>, pix: &str, ceiling: &str, bufsize: &str) {
    cmd.extend([
        "-c:v".into(),
        "vp9_qsv".into(),
        "-pix_fmt".into(),
        pix.into(),
        "-preset".into(),
        "slow".into(),
        // Regular keyframes so Premiere/Influx can seek (hybrid remux writes Cues on these).
        "-g".into(),
        "60".into(),
        "-b:v".into(),
        ceiling.into(),
        "-maxrate".into(),
        ceiling.into(),
        "-bufsize".into(),
        bufsize.into(),
    ]);
}

/// ffmpeg `-color_*` flags from probed source tags (keeps pc/bt709 from becoming unmarked).
fn append_color_meta_args(cmd: &mut Vec<String>, meta: &VideoColorMeta) {
    if let Some(r) = meta.range.as_deref() {
        cmd.extend(["-color_range".into(), r.into()]);
    }
    if let Some(s) = meta.space.as_deref() {
        cmd.extend(["-colorspace".into(), s.into()]);
    }
    if let Some(p) = meta.primaries.as_deref() {
        cmd.extend(["-color_primaries".into(), p.into()]);
    }
    if let Some(t) = meta.transfer.as_deref() {
        cmd.extend(["-color_trc".into(), t.into()]);
    }
}

/// `setparams=` fragment to re-assert colour after `format=` (avoids silent tv default).
fn setparams_color_filter(meta: &VideoColorMeta) -> String {
    let mut parts = Vec::new();
    if let Some(r) = meta.range.as_deref() {
        // ffmpeg setparams wants jpeg/mpeg; map pc→jpeg, tv→mpeg.
        let range = match r {
            "pc" | "jpeg" => "jpeg",
            "tv" | "mpeg" => "mpeg",
            other => other,
        };
        parts.push(format!("range={range}"));
    }
    if let Some(s) = meta.space.as_deref() {
        parts.push(format!("colorspace={s}"));
    }
    if let Some(p) = meta.primaries.as_deref() {
        parts.push(format!("color_primaries={p}"));
    }
    if let Some(t) = meta.transfer.as_deref() {
        parts.push(format!("color_trc={t}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(",setparams={}", parts.join(":"))
    }
}

/// Dual-output ffmpeg: color → `vp9_qsv` IVF, alpha plane → `libvpx-vp9` IVF.
fn build_qsv_alpha_dual_argv(
    input: &Path,
    info: &serde_json::Value,
    color_ivf: &Path,
    alpha_ivf: &Path,
    bitrate_k: u32,
    cq: u32,
    color_meta: &VideoColorMeta,
) -> Vec<String> {
    let ceiling = format!("{bitrate_k}k");
    let bufsize = format!("{}k", bitrate_k * 2);
    let mut cmd = vec![
        "ffmpeg".into(),
        "-v".into(),
        "info".into(),
        "-stats".into(),
        "-stats_period".into(),
        "1.0".into(),
        "-y".into(),
        "-init_hw_device".into(),
        "qsv=hw".into(),
    ];
    let vcodec = video_codec_name(info);
    if vcodec == "vp9" || vcodec == "vp8" {
        cmd.push("-c:v".into());
        cmd.push(if vcodec == "vp9" {
            "libvpx-vp9".into()
        } else {
            "libvpx".into()
        });
    }
    cmd.push("-i".into());
    cmd.push(input.to_string_lossy().into_owned());
    // Split YUVA → nv12 color for QSV + gray-as-yuv420p alpha for libvpx; re-assert colour tags.
    let sp = setparams_color_filter(color_meta);
    let filter = format!(
        "[0:v]format=yuva420p,split=2[c0][a0];[c0]format=nv12{sp}[c];[a0]alphaextract,format=yuv420p{sp}[a]"
    );
    cmd.extend([
        "-filter_complex".into(),
        filter,
        "-map".into(),
        "[c]".into(),
        "-an".into(),
    ]);
    append_vp9_qsv_args(&mut cmd, "nv12", &ceiling, &bufsize);
    append_color_meta_args(&mut cmd, color_meta);
    cmd.push(color_ivf.to_string_lossy().into_owned());
    cmd.extend([
        "-map".into(),
        "[a]".into(),
        "-an".into(),
        "-c:v".into(),
        "libvpx-vp9".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-auto-alt-ref".into(),
        "0".into(),
        "-deadline".into(),
        "good".into(),
        "-cpu-used".into(),
        "0".into(),
        "-threads".into(),
        "0".into(),
        "-tile-columns".into(),
        "2".into(),
        "-tile-rows".into(),
        "1".into(),
        "-lag-in-frames".into(),
        "25".into(),
        "-row-mt".into(),
        "1".into(),
        // Match color GOP so α BlockAdditional stays aligned on keyframes.
        "-g".into(),
        "60".into(),
        "-keyint_min".into(),
        "60".into(),
        "-crf".into(),
        cq.to_string(),
        "-b:v".into(),
        ceiling.clone(),
        "-maxrate".into(),
        ceiling,
        "-bufsize".into(),
        bufsize,
    ]);
    append_color_meta_args(&mut cmd, color_meta);
    cmd.push(alpha_ivf.to_string_lossy().into_owned());
    cmd
}

/// Mux hybrid video-only WebM with audio from the source (WebM-safe codecs).
fn build_hybrid_audio_argv(
    video_webm: &Path,
    source: &Path,
    info: &serde_json::Value,
    output: &Path,
    preserve_obs: bool,
    color_meta: &VideoColorMeta,
) -> Vec<String> {
    let mut cmd = vec![
        "ffmpeg".into(),
        "-v".into(),
        "info".into(),
        "-y".into(),
        "-i".into(),
        video_webm.to_string_lossy().into_owned(),
        "-i".into(),
        source.to_string_lossy().into_owned(),
        "-map".into(),
        "0:v".into(),
        "-map".into(),
        "1:a?".into(),
        "-c:v".into(),
        "copy".into(),
    ];
    let raw = audio_codec_name(info);
    if raw.is_empty() {
        cmd.extend(["-an".into()]);
    } else {
        match audio_encode_for(&raw, OutContainer::Webm) {
            AudioEncode::Copy => cmd.extend(["-c:a".into(), "copy".into()]),
            AudioEncode::Opus | AudioEncode::Aac => {
                // WebM wants Opus; AAC path shouldn't run for .webm but re-encode to Opus anyway.
                let a_k = audio_bitrate_k(source);
                cmd.extend([
                    "-c:a".into(),
                    "libopus".into(),
                    "-b:a".into(),
                    format!("{a_k}k"),
                ]);
            }
        }
    }
    if preserve_obs {
        cmd.extend([
            "-map_metadata".into(),
            "1".into(),
            "-map_chapters".into(),
            "1".into(),
        ]);
    }
    // Lavf `-c:v copy` drops Matroska Colour from the Rust remux; re-stamp source range/matrix.
    append_color_meta_args(&mut cmd, color_meta);
    // End with the video plane; source audio is often slightly longer; leaving it pads format duration and makes ensure-out (video expect) fail even when the VP9 bitstream is complete.
    cmd.push("-shortest".into());
    cmd.push("-avoid_negative_ts".into());
    cmd.push("make_zero".into());
    cmd.push(output.to_string_lossy().into_owned());
    cmd
}

/// QSV color + libvpx alpha → WebM BlockAdditional; caller falls back to full libvpx on `Err`.
fn run_qsv_alpha_hybrid(
    input: &Path,
    output: &Path,
    info: &serde_json::Value,
    bitrate_k: u32,
    cq: u32,
    preserve_obs: bool,
    cancel: &AtomicBool,
    mut on_line: impl FnMut(String, Option<f64>),
) -> Result<(), String> {
    let tmp = output.with_extension("hybrid_tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("hybrid temp dir: {e}"))?;
    let color_ivf = tmp.join("color.ivf");
    let alpha_ivf = tmp.join("alpha.ivf");
    let video_webm = tmp.join("video.webm");

    let cleanup = |tmp: &Path| {
        let _ = std::fs::remove_dir_all(tmp);
    };

    let color_meta = video_color_meta(info);
    let dual = build_qsv_alpha_dual_argv(
        input,
        info,
        &color_ivf,
        &alpha_ivf,
        bitrate_k,
        cq,
        &color_meta,
    );
    on_line("hybrid: dual encode (vp9_qsv color + libvpx alpha)".into(), None);
    if let Err(e) = run_ffmpeg(&dual, cancel, &mut on_line) {
        cleanup(&tmp);
        return Err(format!("hybrid dual encode: {e}"));
    }
    if cancel.load(Ordering::Relaxed) {
        cleanup(&tmp);
        return Err("cancelled".into());
    }

    on_line("hybrid: remux WebM AlphaMode + BlockAdditional".into(), None);
    if let Err(e) = mux_vp9_alpha_webm(&color_ivf, &alpha_ivf, &video_webm, &color_meta) {
        cleanup(&tmp);
        return Err(format!("hybrid remux: {e}"));
    }

    let audio_argv =
        build_hybrid_audio_argv(&video_webm, input, info, output, preserve_obs, &color_meta);
    on_line("hybrid: mux audio".into(), None);
    if let Err(e) = run_ffmpeg(&audio_argv, cancel, &mut on_line) {
        cleanup(&tmp);
        let _ = std::fs::remove_file(output);
        return Err(format!("hybrid audio mux: {e}"));
    }

    cleanup(&tmp);
    Ok(())
}

/// libx264 needs an explicit profile for 4:2:2 / 4:4:4 / 10-bit.
fn append_x264_profile(cmd: &mut Vec<String>, pix: &str) {
    let profile = match pix {
        "yuv420p10le" => Some("high10"),
        "yuv422p" => Some("high422"),
        "yuv422p10le" => Some("high422"),
        "yuv444p" | "yuv444p10le" => Some("high444"),
        _ => None,
    };
    if let Some(p) = profile {
        cmd.extend(["-profile:v".into(), p.into()]);
    }
}

/// NVENC profile when pix_fmt needs Main10 / High 4:4:4.
fn append_nvenc_profile(cmd: &mut Vec<String>, opaque: OpaqueVideo, pix: &str) {
    let profile = match (opaque, pix) {
        (OpaqueVideo::Hevc, "p010le") => Some("main10"),
        (OpaqueVideo::H264, "yuv444p") => Some("high444p"),
        _ => None,
    };
    if let Some(p) = profile {
        cmd.extend(["-profile:v".into(), p.into()]);
    }
}

fn build_cmd(
    input: &Path,
    output: &Path,
    info: &serde_json::Value,
    has_alpha: bool,
    gpu: GpuChoice,
    bitrate_k: u32,
    cq: u32,
    preserve_obs: bool,
    write_encoder_meta: bool,
    use_hardware: bool,
    opaque_video: OpaqueVideo,
    opaque_container: OutContainer,
    alpha_video: AlphaVideo,
    alpha_container: OutContainer,
    chunk: Option<ChunkTrim>,
    // Optional `GigaMux_hw` value (NVENC gen / QSV); encoder name is derived from the path.
    hw_meta: Option<String>,
) -> Vec<String> {
    let ceiling = format!("{bitrate_k}k");
    let bufsize = format!("{}k", bitrate_k * 2);
    let cq_s = cq.to_string();
    let out_container = if has_alpha {
        alpha_container
    } else {
        opaque_container
    };
    let use_nvenc = use_hardware && opaque_video.is_nvenc() && !has_alpha;
    let use_qsv = use_hardware && opaque_video.is_qsv() && !has_alpha;
    let video_only = chunk.is_some();

    let mut cmd = vec![
        "ffmpeg".into(),
        "-v".into(),
        "info".into(),
        "-stats".into(),
        "-stats_period".into(),
        "1.0".into(),
        "-y".into(),
    ];
    // QSV needs an explicit device on Windows; software decode → iGPU encode is fine.
    if use_qsv {
        cmd.extend(["-init_hw_device".into(), "qsv=hw".into()]);
    }

    let vcodec = video_codec_name(info);
    if has_alpha && (vcodec == "vp9" || vcodec == "vp8") {
        // Prefer matching decoder when re-wrapping alpha VP8/VP9.
        cmd.push("-c:v".into());
        cmd.push(if vcodec == "vp9" {
            "libvpx-vp9".into()
        } else {
            "libvpx".into()
        });
    }
    // Frame-accurate chunk: rough -ss pad + post-decode trim + hard frame cap.
    if let Some(tr) = chunk {
        let pad_frames = ((tr.fps * CHUNK_SEEK_PAD_S).round() as u64).max(1);
        let pre_frame = tr.start_frame.saturating_sub(pad_frames);
        if pre_frame > 0 {
            cmd.extend(["-ss".into(), format!("{:.6}", pre_frame as f64 / tr.fps)]);
        }
        cmd.push("-i".into());
        cmd.push(input.to_string_lossy().into_owned());
        let rel_start = tr.start_frame - pre_frame;
        let rel_end = rel_start + tr.frame_count;
        cmd.extend([
            "-vf".into(),
            format!("trim=start_frame={rel_start}:end_frame={rel_end},setpts=PTS-STARTPTS"),
            "-an".into(),
            "-map".into(),
            "0:v:0".into(),
        ]);
    } else {
        cmd.push("-i".into());
        cmd.push(input.to_string_lossy().into_owned());
    }

    let src_pix = video_pix_fmt(info);
    let out_pix = delivery_pix_fmt(&src_pix, has_alpha, use_hardware, opaque_video);

    if has_alpha {
        // Alpha is VP8/VP9 via libvpx — HW has no VP8/VP9 alpha encoder.
        cmd.extend([
            "-c:v".into(),
            alpha_video.ffmpeg_encoder().into(),
            "-pix_fmt".into(),
            out_pix.into(),
            "-auto-alt-ref".into(),
            "0".into(),
            "-deadline".into(),
            "good".into(),
            "-cpu-used".into(),
            "0".into(),
            "-threads".into(),
            "0".into(),
            "-tile-columns".into(),
            "2".into(),
            "-tile-rows".into(),
            "1".into(),
            "-lag-in-frames".into(),
            "25".into(),
            "-row-mt".into(),
            "1".into(),
            "-crf".into(),
            cq_s.clone(),
            "-b:v".into(),
            ceiling.clone(),
            "-maxrate".into(),
            ceiling,
            "-bufsize".into(),
            bufsize,
        ]);
        if !video_only {
            append_audio_args(&mut cmd, input, info, out_container);
        }
    } else if use_qsv {
        // QSV VP9 is VBR-only — no CQ/CRF knob; cq_override is ignored here.
        append_vp9_qsv_args(&mut cmd, out_pix, &ceiling, &bufsize);
        append_color_meta_args(&mut cmd, &video_color_meta(info));
        if !video_only {
            append_audio_args(&mut cmd, input, info, out_container);
        }
    } else if opaque_video.is_libvpx() {
        append_libvpx_opaque_args(
            &mut cmd,
            opaque_video.ffmpeg_encoder(false),
            &ceiling,
            &bufsize,
            cq,
        );
        if !video_only {
            append_audio_args(&mut cmd, input, info, out_container);
        }
    } else if !use_nvenc {
        // CPU HEVC/H.264/AV1 — keep source chroma/bit-depth; CRF capped at source ceiling.
        let enc = opaque_video.ffmpeg_encoder(false);
        cmd.extend([
            "-c:v".into(),
            enc.into(),
            "-pix_fmt".into(),
            out_pix.into(),
            "-crf".into(),
            cq_s,
            "-b:v".into(),
            ceiling.clone(),
            "-maxrate".into(),
            ceiling,
            "-bufsize".into(),
            bufsize,
        ]);
        if enc == "libaom-av1" {
            cmd.extend(["-cpu-used".into(), "4".into(), "-row-mt".into(), "1".into()]);
        } else if enc == "libx265" {
            cmd.extend(["-preset".into(), "medium".into()]);
        } else if enc == "libx264" {
            cmd.extend(["-preset".into(), "medium".into()]);
            append_x264_profile(&mut cmd, out_pix);
        }
        if !video_only {
            append_audio_args(&mut cmd, input, info, out_container);
        }
    } else {
        // NVENC: match source chroma/depth when the codec allows (no 4:2:2 on GeForce).
        cmd.extend([
            "-c:v".into(),
            opaque_video.ffmpeg_encoder(true).into(),
            "-pix_fmt".into(),
            out_pix.into(),
            "-gpu".into(),
            gpu.ffmpeg_arg(),
            "-preset".into(),
            "p7".into(),
            "-tune".into(),
            "hq".into(),
            "-rc".into(),
            "vbr".into(),
            "-rc-lookahead".into(),
            "32".into(),
            "-spatial-aq".into(),
            "1".into(),
            "-aq-strength".into(),
            "15".into(),
            "-cq".into(),
            cq_s,
            "-b:v".into(),
            "0".into(),
            "-maxrate".into(),
            ceiling,
            "-bufsize".into(),
            bufsize,
        ]);
        append_nvenc_profile(&mut cmd, opaque_video, out_pix);
        if !video_only {
            append_audio_args(&mut cmd, input, info, out_container);
        }
    }

    if let Some(tr) = chunk {
        let chunk_dur = tr.frame_count as f64 / tr.fps;
        cmd.extend([
            "-frames:v".into(),
            tr.frame_count.to_string(),
            "-t".into(),
            format!("{chunk_dur:.6}"),
            "-force_key_frames".into(),
            "expr:eq(n,0)".into(),
        ]);
    }

    if preserve_obs && !video_only {
        // Keep creation_time / chapters / tags from OBS Hybrid MP4 across re-encode.
        cmd.extend([
            "-map_metadata".into(),
            "0".into(),
            "-map_chapters".into(),
            "0".into(),
        ]);
    }

    if write_encoder_meta && !video_only {
        let enc_name = if has_alpha {
            alpha_video.ffmpeg_encoder()
        } else if use_nvenc || use_qsv {
            opaque_video.ffmpeg_encoder(true)
        } else {
            opaque_video.ffmpeg_encoder(false)
        };
        let hw = if use_nvenc {
            hw_meta
                .clone()
                .or_else(|| Some(format!("NVENC_gpu{}", gpu.ffmpeg_arg())))
        } else if use_qsv {
            hw_meta.clone().or_else(|| Some("QSV".into()))
        } else {
            None
        };
        append_gigamux_metadata(&mut cmd, out_container, &encoder_metadata_tags(enc_name, hw.as_deref()));
    }

    cmd.push("-avoid_negative_ts".into());
    cmd.push("make_zero".into());
    cmd.push(output.to_string_lossy().into_owned());
    cmd
}

/// Stamp GigaMux_* / encoder tags. MP4 needs `use_metadata_tags` or custom keys stay invisible to ffprobe (Lavf still owns `encoder`).
fn append_gigamux_metadata(cmd: &mut Vec<String>, container: OutContainer, tags: &[(String, String)]) {
    if tags.is_empty() {
        return;
    }
    if container == OutContainer::Mp4 {
        cmd.extend(["-movflags".into(), "+use_metadata_tags".into()]);
    }
    for (k, v) in tags {
        cmd.extend(["-metadata".into(), format!("{k}={v}")]);
    }
}

/// Parse `time=HH:MM:SS.cs` or `time=123.45` from an ffmpeg progress line.
fn parse_ffmpeg_out_time(line: &str) -> Option<f64> {
    let raw = line.split("time=").nth(1)?.split_whitespace().next()?;
    if raw == "N/A" {
        return None;
    }
    if raw.contains(':') {
        let mut parts = raw.split(':');
        let h: f64 = parts.next()?.parse().ok()?;
        let m: f64 = parts.next()?.parse().ok()?;
        let s: f64 = parts.next()?.parse().ok()?;
        Some(h * 3600.0 + m * 60.0 + s)
    } else {
        raw.parse().ok()
    }
}

/// Run ffmpeg and stream stderr live (progress uses `\r`; other lines use `\n`).
/// `on_line` gets `(log_text, optional out_time_seconds from time=)`.
/// Cancel is polled on a timer (not gated on stderr), then the process tree is force-killed.
fn run_ffmpeg(
    argv: &[String],
    cancel: &AtomicBool,
    mut on_line: impl FnMut(String, Option<f64>),
) -> Result<(), String> {
    let (prog, args) = argv.split_first().ok_or_else(|| "empty ffmpeg argv".to_string())?;
    let spawned = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ffmpeg spawn: {e}"))?;
    // Tree-kill on drop if still running (Chocolatey shim must not orphan real ffmpeg).
    let mut child = crate::ffmpeg_active::TrackedFfmpegChild::new(spawned);
    // UI may Shutter-style suspend/resume this tree while the encode runs.
    let _active = crate::ffmpeg_active::ActiveFfmpegGuard::new(child.id());

    let mut stderr = child
        .stderr()
        .take()
        .ok_or_else(|| "ffmpeg stderr missing".to_string())?;

    // Reader thread so cancel isn't stuck behind a blocking stderr read (VP9 can stall progress).
    let (tx, rx) = mpsc::channel::<Result<Vec<u8>, String>>();
    thread::spawn(move || {
        let mut line = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match stderr.read(&mut buf) {
                Ok(0) => {
                    if !line.is_empty() {
                        let _ = tx.send(Ok(std::mem::take(&mut line)));
                    }
                    break;
                }
                Ok(n) => {
                    for &b in &buf[..n] {
                        if b == b'\n' || b == b'\r' {
                            if !line.is_empty() {
                                if tx.send(Ok(std::mem::take(&mut line))).is_err() {
                                    return;
                                }
                            }
                        } else if b != 0 {
                            line.push(b);
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    let _ = tx.send(Err(format!("ffmpeg read: {e}")));
                    break;
                }
            }
        }
    });

    let emit = |raw: &[u8], on_line: &mut dyn FnMut(String, Option<f64>)| {
        let text = String::from_utf8_lossy(raw);
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if text.starts_with("frame=") || text.contains(" fps=") {
            let t = parse_ffmpeg_out_time(text);
            on_line(format!("Progress: {text}"), t);
        } else {
            on_line(format!("FFmpeg: {text}"), None);
        }
    };

    let poll = Duration::from_millis(50);
    loop {
        if cancel.load(Ordering::Relaxed) {
            child.force_kill();
            return Err("cancelled".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // Drain remaining stderr lines briefly so the last progress/error shows up.
                let deadline = Instant::now() + Duration::from_millis(200);
                while Instant::now() < deadline {
                    match rx.recv_timeout(Duration::from_millis(20)) {
                        Ok(Ok(raw)) => emit(&raw, &mut on_line),
                        Ok(Err(e)) => return Err(e),
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                if status.success() {
                    return Ok(());
                }
                if cancel.load(Ordering::Relaxed) {
                    child.force_kill();
                    return Err("cancelled".into());
                }
                return Err(format!("ffmpeg exited with {status}"));
            }
            Ok(None) => {}
            Err(e) => {
                child.force_kill();
                return Err(format!("ffmpeg wait: {e}"));
            }
        }
        match rx.recv_timeout(poll) {
            Ok(Ok(raw)) => emit(&raw, &mut on_line),
            Ok(Err(e)) => {
                child.force_kill();
                return Err(e);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Reader exited (EOF); keep polling try_wait for the exit status.
                continue;
            }
        }
    }
}

fn format_duration_s(info: &serde_json::Value) -> f64 {
    info.pointer("/format/duration")
        .and_then(parse_ffprobe_duration)
        .unwrap_or(0.0)
        .max(0.0)
}

/// Parse ffprobe duration: bare seconds, or Matroska-style `HH:MM:SS.nnn` tags.
fn parse_ffprobe_duration(v: &serde_json::Value) -> Option<f64> {
    if let Some(f) = v.as_f64() {
        return Some(f).filter(|x| *x > 0.0);
    }
    let s = v.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(f) = s.parse::<f64>() {
        return Some(f).filter(|x| *x > 0.0);
    }
    // `00:00:05.817000000` (stream/format tags.DURATION).
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: f64 = parts[0].parse().ok()?;
    let m: f64 = parts[1].parse().ok()?;
    let sec: f64 = parts[2].parse().ok()?;
    let total = h * 3600.0 + m * 60.0 + sec;
    Some(total).filter(|x| *x > 0.0)
}

/// Video bitstream length for video-only farm expects (`ensure-chunk` / IVF concat).
/// Prefers `nb_frames`/fps, then stream duration tags, not container duration (often audio-led).
fn video_stream_duration_s(info: &serde_json::Value) -> f64 {
    if let Some(v) = first_video_stream(info) {
        if let (Some(fps), Some(n)) = (probe_video_fps(info), probe_nb_frames(info)) {
            if fps > 0.0 {
                return n as f64 / fps;
            }
        }
        if let Some(d) = v.get("duration").and_then(parse_ffprobe_duration) {
            return d;
        }
        if let Some(d) = v
            .pointer("/tags/DURATION")
            .or_else(|| v.pointer("/tags/duration"))
            .and_then(parse_ffprobe_duration)
        {
            return d;
        }
    }
    format_duration_s(info)
}

/// Encode all matching files in `opts.input_dir` into `opts.output_dir` sequentially.
/// Calls `on_event` for UI logging. Honors `cancel` between files.
/// Also writes a gzipped per-job log under `%APPDATA%\GigaMux\config\logs\`.
pub fn run_batch(
    opts: EncodeOptions,
    cancel: Arc<AtomicBool>,
    mut on_event: impl FnMut(EncodeEvent),
) {
    // Durable job log (best-effort); UI still gets every event via the wrapped callback.
    let (mut job_log, start_msg) = match JobLog::start(JobLogKind::Encode) {
        Ok(log) => {
            let msg = format!("job log: {}", log.path().display());
            (Some(log), msg)
        }
        Err(e) => (None, format!("job log unavailable: {e}")),
    };
    let mut on_event = |ev: EncodeEvent| {
        if let Some(log) = job_log.as_mut() {
            log.record_encode(&ev);
        }
        on_event(ev);
    };
    on_event(EncodeEvent::Log(start_msg));

    if let Err(e) = require_tools() {
        on_event(EncodeEvent::Log(e));
        on_event(EncodeEvent::Finished {
            ok: 0,
            skipped: 0,
            failed: 0,
        });
        return;
    }
    if !opts.input_dir.is_dir() {
        on_event(EncodeEvent::Log("input folder missing or not a directory".into()));
        on_event(EncodeEvent::Finished {
            ok: 0,
            skipped: 0,
            failed: 0,
        });
        return;
    }
    if let Err(e) = std::fs::create_dir_all(&opts.output_dir) {
        on_event(EncodeEvent::Log(format!("create output dir: {e}")));
        on_event(EncodeEvent::Finished {
            ok: 0,
            skipped: 0,
            failed: 0,
        });
        return;
    }

    let mut inputs = match list_inputs(&opts.input_dir) {
        Ok(v) => v,
        Err(e) => {
            on_event(EncodeEvent::Log(e));
            on_event(EncodeEvent::Finished {
                ok: 0,
                skipped: 0,
                failed: 0,
            });
            return;
        }
    };
    if let Some(only) = &opts.only_names {
        inputs.retain(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| only.iter().any(|o| o == n))
        });
    }
    if inputs.is_empty() {
        on_event(EncodeEvent::Log(
            if opts.only_names.is_some() {
                "no matching input file to encode".into()
            } else {
                "no supported video files in input folder".into()
            },
        ));
        on_event(EncodeEvent::Finished {
            ok: 0,
            skipped: 0,
            failed: 0,
        });
        return;
    }

    let mut ok = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    // Probe first so BatchStarted knows duration-weighted totals (skip-if-exists counted separately).
    struct WorkItem {
        input: PathBuf,
        /// Input basename (for logs / live thumb key).
        name: String,
        info: serde_json::Value,
        has_alpha: bool,
        output: PathBuf,
        duration_s: f64,
        slot: EncodeSlot,
        /// Display label when test mode (`Test1`…`TestN`); empty otherwise.
        pass_label: String,
    }

    // One pass, or N test passes per input depending on test_mode + test_slot_count.
    let passes: Vec<(Option<TestBatch>, EncodeSlot)> = if opts.test_mode {
        let n = opts.test_slot_count.clamp(2, 4) as usize;
        TestBatch::ALL[..n]
            .iter()
            .map(|&b| (Some(b), opts.slot_for(b).clone()))
            .collect()
    } else {
        vec![(None, opts.slot1.clone())]
    };

    let mut work = Vec::new();
    for input in inputs {
        if cancel.load(Ordering::Relaxed) {
            on_event(EncodeEvent::Log("cancelled".into()));
            on_event(EncodeEvent::Finished {
                ok,
                skipped,
                failed,
            });
            return;
        }
        let name = input
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| input.display().to_string());
        let info = match file_info(&input) {
            Ok(v) => v,
            Err(e) => {
                failed += 1;
                on_event(EncodeEvent::FileFailed {
                    name: name.clone(),
                    error: e,
                });
                continue;
            }
        };
        let has_alpha = video_has_alpha(&info);
        let duration_s = format_duration_s(&info);
        for (batch, slot) in &passes {
            let container = slot.container_for(has_alpha);
            let output = deliver_path(&input, &opts.output_dir, container, *batch);
            let out_name = output
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| output.display().to_string());
            if output.is_file() {
                skipped += 1;
                on_event(EncodeEvent::FileSkipped {
                    name: out_name,
                });
                continue;
            }
            let pass_label = batch.map(|b| b.label().to_string()).unwrap_or_default();
            work.push(WorkItem {
                input: input.clone(),
                name: name.clone(),
                info: info.clone(),
                has_alpha,
                output,
                duration_s,
                slot: slot.clone(),
                pass_label,
            });
        }
    }

    let total_files = work.len();
    let total_duration_s: f64 = work.iter().map(|w| w.duration_s).sum();
    on_event(EncodeEvent::BatchStarted {
        total_files,
        total_duration_s,
    });

    for (i, item) in work.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            on_event(EncodeEvent::Log("cancelled".into()));
            break;
        }
        let WorkItem {
            input,
            name,
            info,
            has_alpha,
            output,
            duration_s,
            slot,
            pass_label,
        } = item;

        let out_name = output
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| output.display().to_string());

        on_event(EncodeEvent::FileStarted {
            // Keep input basename so UI live thumbs / per-file logs still match.
            name: name.clone(),
            index: i + 1,
            total: total_files,
            duration_s,
        });
        let (auto_k, auto_label) =
            video_ceiling_k(&input, &info, opts.use_peak_bitrate);
        // Manual override replaces auto ceiling so users can raise (or lower) maxrate.
        let (bitrate_k, src_label) = match slot.maxrate_override_k.filter(|&k| k > 0) {
            Some(k) => (k, format!("override {k}k (auto {auto_k}k / {auto_label})")),
            None => (auto_k, auto_label),
        };
        // None → CQ0/CRF0 (default near-lossless); Some(n) → opt-in quality trade.
        let cq = slot.cq_override.unwrap_or(0);
        let use_nvenc = slot.use_hardware && slot.opaque_video.is_nvenc() && !has_alpha;
        let use_qsv = slot.use_hardware && slot.opaque_video.is_qsv() && !has_alpha;
        let use_hybrid = wants_qsv_alpha_hybrid(
            slot.use_hardware,
            has_alpha,
            slot.alpha_video == AlphaVideo::Vp9,
            slot.alpha_container == OutContainer::Webm,
        );
        let path_label = if has_alpha {
            format!("α {} {}", slot.alpha_video.label(), slot.alpha_container.label())
        } else {
            format!(
                "opaque {} {}",
                slot.opaque_video.label(),
                slot.opaque_container.label()
            )
        };
        // Prefer a local device matching the slot's NVENC gen pin when present.
        let slot_gpu = opts.hw.resolve_gpu_for_slot(slot.nvenc_gen, slot.gpu);
        let device = if use_hybrid {
            match &opts.hw.qsv {
                Some(q) => format!("qsv+libvpx hybrid ({})", q.name),
                None => "qsv+libvpx hybrid".into(),
            }
        } else if use_nvenc {
            match opts.hw.resolve_nvenc(slot_gpu) {
                Some(d) => format!("gpu={} ({})", d.index, d.generation.label()),
                None => format!("gpu={}", slot_gpu.ffmpeg_arg()),
            }
        } else if use_qsv {
            match &opts.hw.qsv {
                Some(q) => format!("qsv={}", q.label),
                None => format!("qsv={}", slot.opaque_video.ffmpeg_encoder(true)),
            }
        } else {
            format!(
                "cpu={}",
                if has_alpha {
                    slot.alpha_video.ffmpeg_encoder()
                } else {
                    slot.opaque_video.ffmpeg_encoder(false)
                }
            )
        };
        let pass_tag = if pass_label.is_empty() {
            String::new()
        } else {
            format!("[{pass_label}] ")
        };
        on_event(EncodeEvent::Log(format!(
            "{pass_tag}{name}: {path_label} → {out_name}, {bitrate_k}k ({src_label}), cq={cq}, {device}",
        )));

        // Live preview: grab source frames at out-time on a side thread (throttled).
        let (thumb_tx, thumb_rx) = mpsc::channel::<(String, RgbaThumb)>();
        let thumb_busy = Arc::new(AtomicBool::new(false));
        let mut last_thumb = Instant::now()
            .checked_sub(Duration::from_secs(2))
            .unwrap_or_else(Instant::now);
        let flush_thumbs = |on_event: &mut dyn FnMut(EncodeEvent)| {
            while let Ok((n, thumb)) = thumb_rx.try_recv() {
                on_event(EncodeEvent::LiveThumb { name: n, thumb });
            }
        };
        let mut on_ffmpeg_line = |line: String, out_time: Option<f64>| {
            flush_thumbs(&mut on_event);
            if let Some(t) = out_time {
                on_event(EncodeEvent::FileProgress { out_time_s: t });
                // ~1 Hz max; skip if a grab is already running (encode stays primary).
                if last_thumb.elapsed() >= Duration::from_millis(900)
                    && !thumb_busy.swap(true, Ordering::Relaxed)
                {
                    last_thumb = Instant::now();
                    let path = input.clone();
                    let n = name.clone();
                    let busy = Arc::clone(&thumb_busy);
                    let tx = thumb_tx.clone();
                    // Cap seek slightly before EOF so ffmpeg always has a frame.
                    let seek = if duration_s > 0.2 {
                        t.min(duration_s - 0.05).max(0.0)
                    } else {
                        t.max(0.0)
                    };
                    thread::spawn(move || {
                        if let Ok(thumb) = thumbnail_rgba_at(&path, seek) {
                            let _ = tx.send((n, thumb));
                        }
                        busy.store(false, Ordering::Relaxed);
                    });
                }
            }
            on_event(EncodeEvent::Log(line));
        };

        let encode_result = if use_hybrid {
            match run_qsv_alpha_hybrid(
                &input,
                &output,
                &info,
                bitrate_k,
                cq,
                opts.preserve_obs_metadata,
                &cancel,
                &mut on_ffmpeg_line,
            ) {
                Ok(()) => Ok(()),
                Err(e) if e == "cancelled" => Err(e),
                Err(e) => {
                    on_ffmpeg_line(
                        format!("{name}: hybrid failed ({e}); falling back to libvpx-vp9"),
                        None,
                    );
                    let argv = build_cmd(
                        &input,
                        &output,
                        &info,
                        has_alpha,
                        slot_gpu,
                        bitrate_k,
                        cq,
                        opts.preserve_obs_metadata,
                        opts.write_encoder_metadata,
                        false, // force CPU α path
                        slot.opaque_video,
                        slot.opaque_container,
                        slot.alpha_video,
                        slot.alpha_container,
                        None,
                        None,
                    );
                    run_ffmpeg(&argv, &cancel, &mut on_ffmpeg_line)
                }
            }
        } else {
            let hw_meta = if use_nvenc {
                Some(opts.hw.nvenc_meta_hw(slot_gpu))
            } else if use_qsv {
                Some(
                    opts.hw
                        .qsv
                        .as_ref()
                        .map(|q| q.meta_hw_tag())
                        .unwrap_or_else(|| "QSV".into()),
                )
            } else {
                None
            };
            let argv = build_cmd(
                &input,
                &output,
                &info,
                has_alpha,
                slot_gpu,
                bitrate_k,
                cq,
                opts.preserve_obs_metadata,
                opts.write_encoder_metadata,
                slot.use_hardware,
                slot.opaque_video,
                slot.opaque_container,
                slot.alpha_video,
                slot.alpha_container,
                None,
                hw_meta,
            );
            run_ffmpeg(&argv, &cancel, &mut on_ffmpeg_line)
        };

        match encode_result {
            Ok(()) => {
                flush_thumbs(&mut on_event);
                ok += 1;
                on_event(EncodeEvent::FileDone { name: name.clone() });
            }
            Err(e) => {
                flush_thumbs(&mut on_event);
                failed += 1;
                let _ = std::fs::remove_file(&output);
                on_event(EncodeEvent::FileFailed { name, error: e });
            }
        }
    }

    on_event(EncodeEvent::Finished {
        ok,
        skipped,
        failed,
    });
}

/// Replace ffmpeg `-y` (overwrite) with `-n` (never overwrite) so farm workers match local skip-if-exists.
fn farm_argv_never_overwrite(argv: &mut [String]) {
    for a in argv.iter_mut() {
        if a == "-y" {
            *a = "-n".into();
            return;
        }
    }
}

/// Build a single-file ffmpeg argv for Flamenco (same flags as local encode).
/// Uses `-n` instead of `-y` so an existing deliver is never replaced on the worker.
/// QSV+libvpx alpha hybrid must use [`build_farm_hybrid_job`] instead.
pub fn build_farm_ffmpeg_argv(req: &FarmEncodeRequest) -> Result<FarmFfmpegCommand, String> {
    if !req.input.is_file() {
        return Err(format!("input not found: {}", req.input.display()));
    }
    if !req.output_dir.is_dir() {
        return Err(format!("output dir not found: {}", req.output_dir.display()));
    }
    let info = file_info(&req.input)?;
    let has_alpha = video_has_alpha(&info);
    if wants_qsv_alpha_hybrid(
        req.slot.use_hardware,
        has_alpha,
        req.slot.alpha_video == AlphaVideo::Vp9,
        req.slot.alpha_container == OutContainer::Webm,
    ) {
        return Err(
            "QSV+libvpx alpha hybrid needs build_farm_hybrid_job (not a single encode argv)".into(),
        );
    }
    let container = req.slot.container_for(has_alpha);
    let output = deliver_path(&req.input, &req.output_dir, container, req.test_batch);
    let (auto_k, auto_label) = video_ceiling_k(&req.input, &info, req.use_peak_bitrate);
    let (bitrate_k, bitrate_label) = match req.slot.maxrate_override_k.filter(|&k| k > 0) {
        Some(k) => (k, format!("override {k}k (auto {auto_k}k / {auto_label})")),
        None => (auto_k, auto_label),
    };
    let cq = req.slot.cq_override.unwrap_or(0);
    // Farm workers have their own GPU indexes — never forward the submitter's `-gpu N`.
    let gpu = GpuChoice::Any;
    let use_nvenc = req.slot.use_hardware && req.slot.opaque_video.is_nvenc() && !has_alpha;
    let use_qsv = req.slot.use_hardware && req.slot.opaque_video.is_qsv() && !has_alpha;
    let hw_meta = if use_nvenc {
        Some(match req.slot.nvenc_gen {
            Some(g) => crate::gpu::HwCatalog::nvenc_meta_hw_for_gen(g),
            None => req.hw.nvenc_meta_hw(gpu),
        })
    } else if use_qsv {
        Some(
            req.hw
                .qsv
                .as_ref()
                .map(|q| q.meta_hw_tag())
                .unwrap_or_else(|| "QSV".into()),
        )
    } else {
        None
    };
    let mut argv = build_cmd(
        &req.input,
        &output,
        &info,
        has_alpha,
        gpu,
        bitrate_k,
        cq,
        req.preserve_obs_metadata,
        req.write_encoder_metadata,
        req.slot.use_hardware,
        req.slot.opaque_video,
        req.slot.opaque_container,
        req.slot.alpha_video,
        req.slot.alpha_container,
        None,
        hw_meta,
    );
    farm_argv_never_overwrite(&mut argv);
    Ok(FarmFfmpegCommand {
        output,
        argv,
        bitrate_k,
        bitrate_label,
    })
}

fn first_video_stream(info: &serde_json::Value) -> Option<&serde_json::Value> {
    info.get("streams")?.as_array()?.iter().find(|s| {
        s.get("codec_type").and_then(|v| v.as_str()) == Some("video")
            || s.get("width").is_some()
    })
}

fn probe_video_fps(info: &serde_json::Value) -> Option<f64> {
    let v = first_video_stream(info)?;
    let rate = v
        .get("r_frame_rate")
        .and_then(|x| x.as_str())
        .or_else(|| v.get("avg_frame_rate").and_then(|x| x.as_str()))?;
    parse_fps(rate)
}

fn probe_nb_frames(info: &serde_json::Value) -> Option<u64> {
    let v = first_video_stream(info)?;
    v.get("nb_frames")
        .and_then(|x| {
            x.as_u64()
                .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
        })
        .filter(|&n| n > 0)
}

/// Build N video-only chunk encodes + concat + mux for Flamenco (vod_chunkutil model).
pub fn build_farm_chunked_job(req: &FarmEncodeRequest) -> Result<FarmChunkedJob, String> {
    let chunks = req.chunks.max(2);
    if !req.input.is_file() {
        return Err(format!("input not found: {}", req.input.display()));
    }
    if !req.output_dir.is_dir() {
        return Err(format!("output dir not found: {}", req.output_dir.display()));
    }
    let info = file_info(&req.input)?;
    let has_alpha = video_has_alpha(&info);
    if wants_qsv_alpha_hybrid(
        req.slot.use_hardware,
        has_alpha,
        req.slot.alpha_video == AlphaVideo::Vp9,
        req.slot.alpha_container == OutContainer::Webm,
    ) {
        return Err(
            "QSV+libvpx alpha hybrid needs build_farm_hybrid_job (not opaque chunk graph)".into(),
        );
    }
    let fps = probe_video_fps(&info).ok_or_else(|| "cannot probe video fps for chunking".to_string())?;
    // Frame split from video length; container duration can track longer audio and inflate expects.
    let duration_s = video_stream_duration_s(&info);
    let total_frames = estimate_total_frames(duration_s, fps, probe_nb_frames(&info))
        .ok_or_else(|| "cannot estimate frame count for chunking".to_string())?;
    let container = req.slot.container_for(has_alpha);
    let final_output = deliver_path(&req.input, &req.output_dir, container, req.test_batch);
    let stem = final_output
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("out");
    let ext = container.ext();
    let chunk_paths = list_chunk_paths(&req.output_dir, stem, chunks, ext);
    let (auto_k, auto_label) = video_ceiling_k(&req.input, &info, req.use_peak_bitrate);
    let (bitrate_k, bitrate_label) = match req.slot.maxrate_override_k.filter(|&k| k > 0) {
        Some(k) => (k, format!("override {k}k (auto {auto_k}k / {auto_label})")),
        None => (auto_k, auto_label),
    };
    let cq = req.slot.cq_override.unwrap_or(0);

    let mut encode_argvs = Vec::with_capacity(chunks as usize);
    let mut chunk_durations_s = Vec::with_capacity(chunks as usize);
    // Farm workers have their own GPU indexes — never forward the submitter's `-gpu N`.
    let gpu = GpuChoice::Any;
    for i in 0..chunks {
        let (start_frame, frame_count) = chunk_frame_range(total_frames, i, chunks)?;
        chunk_durations_s.push(frame_count as f64 / fps);
        let mut argv = build_cmd(
            &req.input,
            &chunk_paths[i as usize],
            &info,
            has_alpha,
            gpu,
            bitrate_k,
            cq,
            false, // chunk encodes are video-only; metadata remux happens at mux
            false,
            req.slot.use_hardware,
            req.slot.opaque_video,
            req.slot.opaque_container,
            req.slot.alpha_video,
            req.slot.alpha_container,
            Some(ChunkTrim {
                start_frame,
                frame_count,
                fps,
            }),
            None,
        );
        // Submitted as `-n`; `gigamux-check ensure-chunk` only forces `-y` when re-encoding a bad/missing file.
        farm_argv_never_overwrite(&mut argv);
        encode_argvs.push(argv);
    }

    let ffconcat_path = req.output_dir.join(format!("{stem}.ffconcat"));
    let vmux_path = req.output_dir.join(format!("{stem}.vmux.mkv"));
    let out_expect_dur_s: f64 = chunk_durations_s.iter().sum();

    // Encoder tags for stage mux + remux (remux alone would leave only Lavf encoder=).
    let gigamux_meta_tags: Vec<(String, String)> = if req.write_encoder_metadata {
        let use_nvenc = req.slot.use_hardware && req.slot.opaque_video.is_nvenc() && !has_alpha;
        let use_qsv = req.slot.use_hardware && req.slot.opaque_video.is_qsv() && !has_alpha;
        let enc_name = if has_alpha {
            req.slot.alpha_video.ffmpeg_encoder()
        } else if use_nvenc || use_qsv {
            req.slot.opaque_video.ffmpeg_encoder(true)
        } else {
            req.slot.opaque_video.ffmpeg_encoder(false)
        };
        let hw = if use_nvenc {
            Some(match req.slot.nvenc_gen {
                Some(g) => crate::gpu::HwCatalog::nvenc_meta_hw_for_gen(g),
                None => req.hw.nvenc_meta_hw(gpu),
            })
        } else if use_qsv {
            Some(
                req.hw
                    .qsv
                    .as_ref()
                    .map(|q| q.meta_hw_tag())
                    .unwrap_or_else(|| "QSV".into()),
            )
        } else {
            None
        };
        encoder_metadata_tags(enc_name, hw.as_deref())
    } else {
        Vec::new()
    };

    // Stage mux → Matroska in one pass: concat demuxer + source audio/subs (no `.vcat` write).
    // Skip avoid_negative_ts make_zero — it was shifting chunk video ~44ms ahead of source audio.
    let mut mux_argv = vec![
        "ffmpeg".into(),
        "-v".into(),
        "info".into(),
        "-y".into(),
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        ffconcat_path.to_string_lossy().into_owned(),
    ];
    // Pull source audio priming/start to the zero-based concat video timeline when needed.
    if let Some(a_start) = first_audio_start_s(&info) {
        if a_start.abs() > 0.001 {
            mux_argv.extend(["-itsoffset".into(), format!("{:.6}", -a_start)]);
        }
    }
    mux_argv.extend([
        "-i".into(),
        req.input.to_string_lossy().into_owned(),
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        "1:a?".into(),
        "-map".into(),
        "1:s?".into(),
        "-c:v".into(),
        "copy".into(),
        // Reset PTS so Linux Matroska mux does not leave ~44ms video-ahead of source audio.
        "-bsf:v".into(),
        "setts=ts=TS-STARTPTS".into(),
    ]);
    let raw = audio_codec_name(&info);
    if raw.is_empty() {
        mux_argv.push("-an".into());
    } else {
        match audio_encode_for(&raw, container) {
            AudioEncode::Copy => mux_argv.extend([
                "-c:a".into(),
                "copy".into(),
                "-bsf:a".into(),
                "setts=ts=TS-STARTPTS".into(),
            ]),
            AudioEncode::Aac => {
                let a_k = audio_bitrate_k(&req.input);
                mux_argv.extend([
                    "-c:a".into(),
                    "aac".into(),
                    "-b:a".into(),
                    format!("{a_k}k"),
                ]);
            }
            AudioEncode::Opus => {
                let a_k = audio_bitrate_k(&req.input);
                mux_argv.extend([
                    "-c:a".into(),
                    "libopus".into(),
                    "-b:a".into(),
                    format!("{a_k}k"),
                ]);
            }
        }
    }
    // Copy any mapped subtitle bitstreams (optional `1:s?` may map nothing).
    mux_argv.extend(["-c:s".into(), "copy".into()]);
    if req.preserve_obs_metadata {
        mux_argv.extend([
            "-map_metadata".into(),
            "1".into(),
            "-map_chapters".into(),
            "1".into(),
        ]);
    }
    // Intermediate Matroska — no movflags needed; remux re-stamps for MP4 visibility.
    append_gigamux_metadata(&mut mux_argv, OutContainer::Mkv, &gigamux_meta_tags);
    mux_argv.push(vmux_path.to_string_lossy().into_owned());

    // Remux → final container (bitstream copy; separate Flamenco task so soft-fail never re-joins).
    // setts on remux heals an already-skewed `.vmux.mkv` without re-joining chunks.
    let mut mux_remux_argv = vec![
        "ffmpeg".into(),
        "-v".into(),
        "info".into(),
        "-y".into(),
        "-i".into(),
        vmux_path.to_string_lossy().into_owned(),
        "-map".into(),
        "0".into(),
        "-c".into(),
        "copy".into(),
        "-bsf:v".into(),
        "setts=ts=TS-STARTPTS".into(),
    ];
    if !raw.is_empty() {
        mux_remux_argv.extend(["-bsf:a".into(), "setts=ts=TS-STARTPTS".into()]);
    }
    append_gigamux_metadata(&mut mux_remux_argv, container, &gigamux_meta_tags);
    mux_remux_argv.push(final_output.to_string_lossy().into_owned());

    Ok(FarmChunkedJob {
        final_output,
        chunk_paths,
        encode_argvs,
        mux_argv,
        mux_remux_argv,
        out_expect_dur_s,
        ffconcat_path,
        chunk_durations_s,
        bitrate_k,
        bitrate_label,
    })
}

/// First audio stream `start_time` from ffprobe JSON (seconds), if present.
fn first_audio_start_s(info: &serde_json::Value) -> Option<f64> {
    let streams = info.get("streams")?.as_array()?;
    for s in streams {
        if s.get("codec_type").and_then(|v| v.as_str()) != Some("audio") {
            continue;
        }
        return s.get("start_time").and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|t| t.parse().ok()))
        });
    }
    None
}

/// Write the job's ffconcat list (call before submit so workers see it on shared storage).
pub fn write_farm_ffconcat(job: &FarmChunkedJob) -> Result<(), String> {
    write_ffconcat(&job.chunk_paths, &job.chunk_durations_s, &job.ffconcat_path)
}

/// Write hybrid IVF concat lists (call before submit).
pub fn write_farm_hybrid_ffconcats(job: &FarmHybridJob) -> Result<(), String> {
    for (list_path, paths, durs) in &job.ffconcat_paths {
        write_ffconcat(paths, durs, list_path)?;
    }
    Ok(())
}

fn build_hybrid_color_argv_impl(
    input: &Path,
    info: &serde_json::Value,
    color_ivf: &Path,
    bitrate_k: u32,
    color_meta: &VideoColorMeta,
    chunk: Option<ChunkTrim>,
    // Cap output frames so split color/alpha farm encodes stay remux-aligned.
    frames_v: Option<u64>,
) -> Vec<String> {
    let ceiling = format!("{bitrate_k}k");
    let bufsize = format!("{}k", bitrate_k * 2);
    let mut cmd = vec![
        "ffmpeg".into(),
        "-v".into(),
        "info".into(),
        "-stats".into(),
        "-stats_period".into(),
        "1.0".into(),
        "-y".into(),
        "-init_hw_device".into(),
        "qsv=hw".into(),
    ];
    let vcodec = video_codec_name(info);
    if vcodec == "vp9" || vcodec == "vp8" {
        cmd.push("-c:v".into());
        cmd.push(if vcodec == "vp9" {
            "libvpx-vp9".into()
        } else {
            "libvpx".into()
        });
    }
    let sp = setparams_color_filter(color_meta);
    let filter = if let Some(tr) = chunk {
        let pad_frames = ((tr.fps * CHUNK_SEEK_PAD_S).round() as u64).max(1);
        let pre_frame = tr.start_frame.saturating_sub(pad_frames);
        if pre_frame > 0 {
            cmd.extend(["-ss".into(), format!("{:.6}", pre_frame as f64 / tr.fps)]);
        }
        cmd.push("-i".into());
        cmd.push(input.to_string_lossy().into_owned());
        let rel_start = tr.start_frame - pre_frame;
        let rel_end = rel_start + tr.frame_count;
        format!(
            "[0:v]trim=start_frame={rel_start}:end_frame={rel_end},setpts=PTS-STARTPTS,format=yuva420p,format=nv12{sp}[c]"
        )
    } else {
        cmd.push("-i".into());
        cmd.push(input.to_string_lossy().into_owned());
        format!("[0:v]format=yuva420p,format=nv12{sp}[c]")
    };
    cmd.extend([
        "-filter_complex".into(),
        filter,
        "-map".into(),
        "[c]".into(),
        "-an".into(),
    ]);
    append_vp9_qsv_args(&mut cmd, "nv12", &ceiling, &bufsize);
    append_color_meta_args(&mut cmd, color_meta);
    if let Some(n) = frames_v.filter(|&n| n > 0) {
        cmd.extend(["-frames:v".into(), n.to_string()]);
    }
    cmd.push(color_ivf.to_string_lossy().into_owned());
    farm_argv_never_overwrite(&mut cmd);
    cmd
}

fn build_hybrid_alpha_argv_impl(
    input: &Path,
    info: &serde_json::Value,
    alpha_ivf: &Path,
    bitrate_k: u32,
    cq: u32,
    color_meta: &VideoColorMeta,
    chunk: Option<ChunkTrim>,
    frames_v: Option<u64>,
) -> Vec<String> {
    let ceiling = format!("{bitrate_k}k");
    let bufsize = format!("{}k", bitrate_k * 2);
    let mut cmd = vec![
        "ffmpeg".into(),
        "-v".into(),
        "info".into(),
        "-stats".into(),
        "-stats_period".into(),
        "1.0".into(),
        "-y".into(),
    ];
    let vcodec = video_codec_name(info);
    if vcodec == "vp9" || vcodec == "vp8" {
        cmd.push("-c:v".into());
        cmd.push(if vcodec == "vp9" {
            "libvpx-vp9".into()
        } else {
            "libvpx".into()
        });
    }
    let sp = setparams_color_filter(color_meta);
    let filter = if let Some(tr) = chunk {
        let pad_frames = ((tr.fps * CHUNK_SEEK_PAD_S).round() as u64).max(1);
        let pre_frame = tr.start_frame.saturating_sub(pad_frames);
        if pre_frame > 0 {
            cmd.extend(["-ss".into(), format!("{:.6}", pre_frame as f64 / tr.fps)]);
        }
        cmd.push("-i".into());
        cmd.push(input.to_string_lossy().into_owned());
        let rel_start = tr.start_frame - pre_frame;
        let rel_end = rel_start + tr.frame_count;
        format!(
            "[0:v]trim=start_frame={rel_start}:end_frame={rel_end},setpts=PTS-STARTPTS,format=yuva420p,alphaextract,format=yuv420p{sp}[a]"
        )
    } else {
        cmd.push("-i".into());
        cmd.push(input.to_string_lossy().into_owned());
        format!("[0:v]format=yuva420p,alphaextract,format=yuv420p{sp}[a]")
    };
    cmd.extend([
        "-filter_complex".into(),
        filter,
        "-map".into(),
        "[a]".into(),
        "-an".into(),
        "-c:v".into(),
        "libvpx-vp9".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-auto-alt-ref".into(),
        "0".into(),
        "-deadline".into(),
        "good".into(),
        "-cpu-used".into(),
        "0".into(),
        "-threads".into(),
        "0".into(),
        "-tile-columns".into(),
        "2".into(),
        "-tile-rows".into(),
        "1".into(),
        "-lag-in-frames".into(),
        "25".into(),
        "-row-mt".into(),
        "1".into(),
        "-g".into(),
        "60".into(),
        "-keyint_min".into(),
        "60".into(),
        "-crf".into(),
        cq.to_string(),
        "-b:v".into(),
        ceiling.clone(),
        "-maxrate".into(),
        ceiling,
        "-bufsize".into(),
        bufsize,
    ]);
    append_color_meta_args(&mut cmd, color_meta);
    if let Some(n) = frames_v.filter(|&n| n > 0) {
        cmd.extend(["-frames:v".into(), n.to_string()]);
    }
    cmd.push(alpha_ivf.to_string_lossy().into_owned());
    farm_argv_never_overwrite(&mut cmd);
    cmd
}

fn ivf_concat_argv(ffconcat: &Path, out_ivf: &Path) -> Vec<String> {
    let mut cmd = vec![
        "ffmpeg".into(),
        "-v".into(),
        "info".into(),
        "-y".into(),
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        ffconcat.to_string_lossy().into_owned(),
        "-c".into(),
        "copy".into(),
        out_ivf.to_string_lossy().into_owned(),
    ];
    farm_argv_never_overwrite(&mut cmd);
    cmd
}

fn color_meta_check_tags(meta: &VideoColorMeta) -> [String; 4] {
    [
        meta.range.clone().unwrap_or_else(|| "-".into()),
        meta.space.clone().unwrap_or_else(|| "-".into()),
        meta.primaries.clone().unwrap_or_else(|| "-".into()),
        meta.transfer.clone().unwrap_or_else(|| "-".into()),
    ]
}

/// Build a farm QSV+libvpx alpha hybrid job (chunkless color by default; optional color chunks).
pub fn build_farm_hybrid_job(req: &FarmEncodeRequest) -> Result<FarmHybridJob, String> {
    if !req.input.is_file() {
        return Err(format!("input not found: {}", req.input.display()));
    }
    if !req.output_dir.is_dir() {
        return Err(format!("output dir not found: {}", req.output_dir.display()));
    }
    let info = file_info(&req.input)?;
    let has_alpha = video_has_alpha(&info);
    if !wants_qsv_alpha_hybrid(
        req.slot.use_hardware,
        has_alpha,
        req.slot.alpha_video == AlphaVideo::Vp9,
        req.slot.alpha_container == OutContainer::Webm,
    ) {
        return Err("build_farm_hybrid_job: slot/input is not QSV+libvpx alpha hybrid".into());
    }
    let container = OutContainer::Webm;
    let final_output = deliver_path(&req.input, &req.output_dir, container, req.test_batch);
    let stem = final_output
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("out")
        .to_string();
    // Drop stale flat IVF / plane temps (and legacy `{stem}.hybrid/`) so ensure-chunk `-n` cannot reuse wrong splits.
    clear_flat_hybrid_temps(&req.output_dir, &stem)?;

    let (auto_k, auto_label) = video_ceiling_k(&req.input, &info, req.use_peak_bitrate);
    let (bitrate_k, bitrate_label) = match req.slot.maxrate_override_k.filter(|&k| k > 0) {
        Some(k) => (k, format!("override {k}k (auto {auto_k}k / {auto_label})")),
        None => (auto_k, auto_label),
    };
    let cq = req.slot.cq_override.unwrap_or(0);
    let color_meta = video_color_meta(&info);
    // IVF / concat expects track the video stream; final audio mux uses container duration (audio may be longer).
    let video_dur = video_stream_duration_s(&info);
    let format_dur = format_duration_s(&info);

    let alpha_chunks = req.chunks.max(1);
    let color_chunks = if req.chunk_qsv_color && alpha_chunks > 1 {
        alpha_chunks
    } else {
        1
    };

    // Always probe frames so split color/alpha encodes get matching `-frames:v` (remux requires equal IVF counts).
    let fps = probe_video_fps(&info);
    let total_frames = fps.and_then(|f| estimate_total_frames(video_dur, f, probe_nb_frames(&info)));
    if (alpha_chunks > 1 || color_chunks > 1) && (fps.is_none() || total_frames.is_none()) {
        return Err("cannot probe fps/frames for hybrid chunking".into());
    }

    // Same layout as opaque chunks: planes + chunk IVFs sit next to the deliver in output_dir.
    let color_ivf = req.output_dir.join(format!("{stem}.color.ivf"));
    let alpha_ivf = req.output_dir.join(format!("{stem}.alpha.ivf"));
    let video_webm = req.output_dir.join(format!("{stem}.vmux.webm"));

    let mut ffconcat_paths = Vec::new();
    let mut color_tasks = Vec::new();
    let mut color_concat_argv = None;
    if color_chunks == 1 {
        let argv = build_hybrid_color_argv_impl(
            &req.input,
            &info,
            &color_ivf,
            bitrate_k,
            &color_meta,
            None,
            total_frames,
        );
        color_tasks.push(FarmHybridEncodeTask {
            name: "encode-color".into(),
            task_type: "qsv".into(),
            argv,
            ensure: Some(FarmChunkCheck {
                path: path_string(&color_ivf),
                expect_dur: video_dur,
            }),
        });
    } else {
        let fps = fps.unwrap();
        let total_frames = total_frames.unwrap();
        let mut chunk_paths = Vec::new();
        let mut chunk_durs = Vec::new();
        let color_stem = format!("{stem}.color");
        for i in 0..color_chunks {
            let (start_frame, frame_count) = chunk_frame_range(total_frames, i, color_chunks)?;
            let dur = frame_count as f64 / fps;
            chunk_durs.push(dur);
            let out = req
                .output_dir
                .join(chunk_output_name(&color_stem, i, color_chunks, "ivf"));
            let argv = build_hybrid_color_argv_impl(
                &req.input,
                &info,
                &out,
                bitrate_k,
                &color_meta,
                Some(ChunkTrim {
                    start_frame,
                    frame_count,
                    fps,
                }),
                Some(frame_count),
            );
            color_tasks.push(FarmHybridEncodeTask {
                name: format!("encode-color-c{i}"),
                task_type: "qsv".into(),
                argv,
                ensure: Some(FarmChunkCheck {
                    path: path_string(&out),
                    expect_dur: dur,
                }),
            });
            chunk_paths.push(out);
        }
        let list = req.output_dir.join(format!("{stem}.color.ffconcat"));
        color_concat_argv = Some(ivf_concat_argv(&list, &color_ivf));
        ffconcat_paths.push((list, chunk_paths, chunk_durs));
    }

    let mut alpha_tasks = Vec::new();
    let mut alpha_concat_argv = None;
    if alpha_chunks == 1 {
        let argv = build_hybrid_alpha_argv_impl(
            &req.input,
            &info,
            &alpha_ivf,
            bitrate_k,
            cq,
            &color_meta,
            None,
            total_frames,
        );
        alpha_tasks.push(FarmHybridEncodeTask {
            name: "encode-alpha".into(),
            task_type: "cpu".into(),
            argv,
            ensure: Some(FarmChunkCheck {
                path: path_string(&alpha_ivf),
                expect_dur: video_dur,
            }),
        });
    } else {
        let fps = fps.unwrap();
        let total_frames = total_frames.unwrap();
        let mut chunk_paths = Vec::new();
        let mut chunk_durs = Vec::new();
        for i in 0..alpha_chunks {
            let (start_frame, frame_count) = chunk_frame_range(total_frames, i, alpha_chunks)?;
            let dur = frame_count as f64 / fps;
            chunk_durs.push(dur);
            // Deliver stem + opaque-style chunk tag → chunk VMAF / Check discover these like webm chunks.
            let out = req
                .output_dir
                .join(chunk_output_name(&stem, i, alpha_chunks, "ivf"));
            let argv = build_hybrid_alpha_argv_impl(
                &req.input,
                &info,
                &out,
                bitrate_k,
                cq,
                &color_meta,
                Some(ChunkTrim {
                    start_frame,
                    frame_count,
                    fps,
                }),
                Some(frame_count),
            );
            alpha_tasks.push(FarmHybridEncodeTask {
                name: format!("encode-alpha-c{i}"),
                task_type: "cpu".into(),
                argv,
                ensure: Some(FarmChunkCheck {
                    path: path_string(&out),
                    expect_dur: dur,
                }),
            });
            chunk_paths.push(out);
        }
        let list = req.output_dir.join(format!("{stem}.ffconcat"));
        alpha_concat_argv = Some(ivf_concat_argv(&list, &alpha_ivf));
        ffconcat_paths.push((list, chunk_paths, chunk_durs));
    }

    let tags = color_meta_check_tags(&color_meta);
    let remux_check_args = vec![
        "mux-alpha-webm".into(),
        path_string(&color_ivf),
        path_string(&alpha_ivf),
        path_string(&video_webm),
        tags[0].clone(),
        tags[1].clone(),
        tags[2].clone(),
        tags[3].clone(),
    ];

    let mut audio_argv = build_hybrid_audio_argv(
        &video_webm,
        &req.input,
        &info,
        &final_output,
        req.preserve_obs_metadata,
        &color_meta,
    );
    // Stamp encoder metadata before the output path.
    if req.write_encoder_metadata {
        let hw = req
            .hw
            .qsv
            .as_ref()
            .map(|q| q.meta_hw_tag())
            .unwrap_or_else(|| "QSV".into());
        let tags = encoder_metadata_tags("vp9_qsv+libvpx", Some(&hw));
        let out = audio_argv.pop();
        append_gigamux_metadata(&mut audio_argv, OutContainer::Webm, &tags);
        if let Some(o) = out {
            audio_argv.push(o);
        }
    }
    farm_argv_never_overwrite(&mut audio_argv);

    Ok(FarmHybridJob {
        final_output,
        color_tasks,
        alpha_tasks,
        color_concat_argv,
        alpha_concat_argv,
        ffconcat_paths,
        remux_check_args,
        audio_argv,
        video_expect_dur_s: video_dur,
        // Prefer container duration for final A/V; fall back to video when format tag is missing.
        out_expect_dur_s: if format_dur > 0.0 {
            format_dur
        } else {
            video_dur
        },
        bitrate_k,
        bitrate_label,
    })
}

/// Wipe flat hybrid temps for `stem` in `output_dir` (planes, IVF chunks, ffconcats, legacy `.hybrid/`).
fn clear_flat_hybrid_temps(output_dir: &Path, stem: &str) -> Result<(), String> {
    let legacy = output_dir.join(format!("{stem}.hybrid"));
    if legacy.is_dir() {
        std::fs::remove_dir_all(&legacy).map_err(|e| format!("clear legacy hybrid dir: {e}"))?;
    }
    let remove = |path: PathBuf| -> Result<(), String> {
        if path.is_file() {
            std::fs::remove_file(&path).map_err(|e| format!("clear {}: {e}", path.display()))?;
        }
        Ok(())
    };
    remove(output_dir.join(format!("{stem}.color.ivf")))?;
    remove(output_dir.join(format!("{stem}.alpha.ivf")))?;
    remove(output_dir.join(format!("{stem}.vmux.webm")))?;
    remove(output_dir.join(format!("{stem}.ffconcat")))?;
    remove(output_dir.join(format!("{stem}.color.ffconcat")))?;
    // Any prior chunk count: delete matching IVF chunk names (alpha + optional color plane).
    let entries = std::fs::read_dir(output_dir).map_err(|e| format!("read output dir: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read output entry: {e}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some((chunk_stem, _, _, ext)) = parse_chunk_filename(name) else {
            continue;
        };
        if !ext.eq_ignore_ascii_case("ivf") {
            continue;
        }
        if chunk_stem == stem || chunk_stem == format!("{stem}.color") {
            remove(entry.path())?;
        }
    }
    Ok(())
}

/// Integrity for a farm hybrid job (final + cleanup of flat plane/chunk IVFs like opaque).
pub fn farm_integrity_for_hybrid(
    req: &FarmEncodeRequest,
    job: &FarmHybridJob,
) -> FlamencoIntegrity {
    let stem = job
        .final_output
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("out")
        .to_string();
    // Alpha plane uses deliver-stem chunk names; cleanup also removes color plane + `.ivf` siblings.
    let alpha_chunks = job.alpha_tasks.len() as u32;
    FlamencoIntegrity {
        chunk_checks: Vec::new(),
        final_check: Some(farm_final_check(&req.input, &job.final_output, &req.slot)),
        cleanup: Some(FarmCleanup {
            dir: path_string(&req.output_dir),
            stem,
            ext: "webm".into(),
            chunks: if alpha_chunks > 1 { alpha_chunks } else { 0 },
        }),
        ..FlamencoIntegrity::default()
    }
}

fn path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn farm_final_check(input: &Path, output: &Path, slot: &EncodeSlot) -> FarmFinalCheck {
    FarmFinalCheck {
        input: path_string(input),
        output: path_string(output),
        opaque_video: slot.opaque_video,
        opaque_container: slot.opaque_container,
        alpha_video: slot.alpha_video,
        alpha_container: slot.alpha_container,
    }
}

/// Paths for trailing / separate farm VMAF (`ensure-vmaf` + sidecar next to deliver).
pub fn farm_vmaf_spec(input: &Path, output: &Path) -> FarmVmafSpec {
    FarmVmafSpec {
        input: path_string(input),
        output: path_string(output),
        sidecar: path_string(&vmaf_sidecar_path(output)),
    }
}

/// Hybrid integrity for a single-file farm encode (final check only; no chunk cleanup).
pub fn farm_integrity_for_single(
    req: &FarmEncodeRequest,
    cmd: &FarmFfmpegCommand,
) -> FlamencoIntegrity {
    FlamencoIntegrity {
        final_check: Some(farm_final_check(&req.input, &cmd.output, &req.slot)),
        cleanup: None,
        ..FlamencoIntegrity::default()
    }
}

/// Hybrid integrity for a chunked farm job (per-chunk expects + final + cleanup).
pub fn farm_integrity_for_chunked(
    req: &FarmEncodeRequest,
    job: &FarmChunkedJob,
) -> FlamencoIntegrity {
    let stem = job
        .final_output
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("out")
        .to_string();
    let ext = job
        .final_output
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mp4")
        .to_string();
    let chunk_checks: Vec<FarmChunkCheck> = job
        .chunk_paths
        .iter()
        .zip(job.chunk_durations_s.iter())
        .map(|(p, &dur)| FarmChunkCheck {
            path: path_string(p),
            expect_dur: dur,
        })
        .collect();
    let chunks = chunk_checks.len() as u32;
    FlamencoIntegrity {
        chunk_checks,
        final_check: Some(farm_final_check(&req.input, &job.final_output, &req.slot)),
        cleanup: Some(FarmCleanup {
            dir: path_string(&req.output_dir),
            stem,
            ext,
            chunks,
        }),
        ..FlamencoIntegrity::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stub_yuva_info() -> serde_json::Value {
        serde_json::json!({
            "streams": [{
                "codec_type": "video",
                "codec_name": "vp9",
                "pix_fmt": "yuva420p",
                "width": 64,
                "height": 64,
                "r_frame_rate": "30/1",
                "color_range": "tv",
                "color_space": "bt709",
            }],
            "format": { "duration": "2.0" }
        })
    }

    #[test]
    fn video_stream_duration_ignores_longer_audio_container() {
        // Farm IVF ensure must not use format duration when audio pads the container.
        let info = serde_json::json!({
            "streams": [
                {
                    "codec_type": "video",
                    "codec_name": "vp9",
                    "duration": "5.817000",
                    "r_frame_rate": "60/1",
                    "nb_frames": "349",
                },
                { "codec_type": "audio", "duration": "5.942000" }
            ],
            "format": { "duration": "5.942000" }
        });
        let video = video_stream_duration_s(&info);
        let format = format_duration_s(&info);
        assert!((video - 5.817).abs() < 0.001, "got video={video}");
        assert!((format - 5.942).abs() < 0.001, "got format={format}");
        assert!(format - video > 0.1);
    }

    #[test]
    fn parse_matroska_duration_tag() {
        let v = serde_json::json!("00:00:05.817000000");
        assert!((parse_ffprobe_duration(&v).unwrap() - 5.817).abs() < 0.001);
    }

    #[test]
    fn hybrid_color_argv_is_qsv_ivf() {
        let info = stub_yuva_info();
        let meta = video_color_meta(&info);
        let argv = build_hybrid_color_argv_impl(
            Path::new("in.webm"),
            &info,
            Path::new("color.ivf"),
            4000,
            &meta,
            None,
            Some(60),
        );
        assert!(argv.iter().any(|a| a == "vp9_qsv"));
        assert_eq!(argv.last().map(String::as_str), Some("color.ivf"));
        assert!(argv.iter().any(|a| a == "-n"));
        assert!(argv.windows(2).any(|w| w[0] == "-frames:v" && w[1] == "60"));
    }

    #[test]
    fn hybrid_alpha_argv_is_libvpx_ivf() {
        let info = stub_yuva_info();
        let meta = video_color_meta(&info);
        let argv = build_hybrid_alpha_argv_impl(
            Path::new("in.webm"),
            &info,
            Path::new("alpha.ivf"),
            4000,
            0,
            &meta,
            None,
            Some(60),
        );
        assert!(argv.iter().any(|a| a == "libvpx-vp9"));
        assert!(argv.iter().any(|a| a.contains("alphaextract")));
        assert_eq!(argv.last().map(String::as_str), Some("alpha.ivf"));
    }

    #[test]
    fn hybrid_job_settings_chunkless_color_by_default() {
        // Construct a minimal FarmHybridJob as build_farm_hybrid_job would with chunks=1.
        let job = FarmHybridJob {
            final_output: PathBuf::from("out/clip.webm"),
            color_tasks: vec![FarmHybridEncodeTask {
                name: "encode-color".into(),
                task_type: "qsv".into(),
                argv: vec!["ffmpeg".into(), "-n".into(), "color.ivf".into()],
                ensure: None,
            }],
            alpha_tasks: vec![FarmHybridEncodeTask {
                name: "encode-alpha".into(),
                task_type: "cpu".into(),
                argv: vec!["ffmpeg".into(), "-n".into(), "alpha.ivf".into()],
                ensure: None,
            }],
            color_concat_argv: None,
            alpha_concat_argv: None,
            ffconcat_paths: Vec::new(),
            remux_check_args: vec![
                "mux-alpha-webm".into(),
                "c.ivf".into(),
                "a.ivf".into(),
                "v.webm".into(),
                "-".into(),
                "-".into(),
                "-".into(),
                "-".into(),
            ],
            audio_argv: vec!["ffmpeg".into(), "out.webm".into()],
            video_expect_dur_s: 2.0,
            out_expect_dur_s: 2.0,
            bitrate_k: 4000,
            bitrate_label: "test".into(),
        };
        assert_eq!(job.color_tasks.len(), 1);
        assert_eq!(job.color_tasks[0].task_type, "qsv");
        assert_eq!(job.alpha_tasks[0].task_type, "cpu");
        assert!(job.color_concat_argv.is_none());
        assert_eq!(job.remux_check_args[0], "mux-alpha-webm");
        let submitted = crate::flamenco::make_gigamux_hybrid_job(
            "test",
            None,
            &job,
            None,
            crate::flamenco::CancelExecMode::Direct,
            None,
        )
        .unwrap();
        let hybrid = submitted.settings.get("hybrid_json").and_then(|v| v.as_str()).unwrap();
        assert!(hybrid.contains("encode-color"));
        assert!(hybrid.contains("\"qsv\""));
        assert!(hybrid.contains("mux-alpha-webm"));
    }

    #[test]
    fn mp4_metadata_gets_use_metadata_tags() {
        let tags = encoder_metadata_tags("hevc_nvenc", Some("NVENC_Ada"));
        let mut mp4 = Vec::new();
        append_gigamux_metadata(&mut mp4, OutContainer::Mp4, &tags);
        assert!(mp4.windows(2).any(|w| w[0] == "-movflags" && w[1] == "+use_metadata_tags"));
        assert!(mp4.iter().any(|a| a.starts_with("GigaMux_version=")));
        let mut mkv = Vec::new();
        append_gigamux_metadata(&mut mkv, OutContainer::Mkv, &tags);
        assert!(!mkv.iter().any(|a| a.contains("use_metadata_tags")));
    }

    #[test]
    fn deliver_path_keeps_dots_in_stem() {
        // Filesystems treat only the final suffix as the extension; so do we.
        let out = deliver_path(
            Path::new("in/sample.ts.mp4"),
            Path::new("out"),
            OutContainer::Mp4,
            None,
        );
        assert_eq!(out.file_name().and_then(|s| s.to_str()), Some("sample.ts.mp4"));
        let test = deliver_path(
            Path::new("clip.ts.mp4"),
            Path::new("out"),
            OutContainer::Mp4,
            Some(TestBatch::One),
        );
        assert_eq!(
            test.file_name().and_then(|s| s.to_str()),
            Some("test1_clip.ts.mp4")
        );
    }
}
