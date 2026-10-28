//! Local sequential encode (opaque HEVC NVENC / alpha VP9). No chunking.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::gpu::GpuChoice;
use crate::probe::{
    audio_bitrate_k, audio_codec_name, file_info, require_tools, thumbnail_rgba_at,
    video_ceiling_k, video_codec_name, video_has_alpha, video_pix_fmt, RgbaThumb,
};
use crate::targets::{
    audio_encode_for, delivery_pix_fmt, AlphaVideo, AudioEncode, OpaqueVideo, OutContainer,
};

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
    pub gpu: GpuChoice,
    pub preserve_obs_metadata: bool,
    /// Ceiling from 1s video peak when true; otherwise average rate.
    pub use_peak_bitrate: bool,
    /// NVENC for HEVC/H.264/AV1 when true; CPU (libx265/x264/aom) when false. VP8/VP9 always CPU.
    pub use_hardware: bool,
    /// When set, replaces automatic video ceiling (maxrate) in kbps.
    pub maxrate_override_k: Option<u32>,
    pub opaque_video: OpaqueVideo,
    pub opaque_container: OutContainer,
    pub alpha_video: AlphaVideo,
    pub alpha_container: OutContainer,
    /// When set, only encode these basenames (right-click selection). `None` = all inputs.
    pub only_names: Option<Vec<String>>,
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

fn deliver_path(input: &Path, output_dir: &Path, container: OutContainer) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default();
    output_dir.join(stem).with_extension(container.ext())
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

fn append_libvpx_opaque_args(cmd: &mut Vec<String>, encoder: &str, ceiling: &str, bufsize: &str) {
    // Constrained CRF0; libvpx ≥1.17 needs -b:v with -maxrate. VP8/VP9 path is 4:2:0-only here.
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
        "0".into(),
        "-b:v".into(),
        ceiling.into(),
        "-maxrate".into(),
        ceiling.into(),
        "-bufsize".into(),
        bufsize.into(),
    ]);
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
    preserve_obs: bool,
    use_hardware: bool,
    opaque_video: OpaqueVideo,
    opaque_container: OutContainer,
    alpha_video: AlphaVideo,
    alpha_container: OutContainer,
) -> Vec<String> {
    let ceiling = format!("{bitrate_k}k");
    let bufsize = format!("{}k", bitrate_k * 2);
    let out_container = if has_alpha {
        alpha_container
    } else {
        opaque_container
    };
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
    if has_alpha && (vcodec == "vp9" || vcodec == "vp8") {
        // Prefer matching decoder when re-wrapping alpha VP8/VP9.
        cmd.push("-c:v".into());
        cmd.push(if vcodec == "vp9" {
            "libvpx-vp9".into()
        } else {
            "libvpx".into()
        });
    }
    cmd.push("-i".into());
    cmd.push(input.to_string_lossy().into_owned());

    let use_nvenc = use_hardware && opaque_video.is_nvenc() && !has_alpha;
    let src_pix = video_pix_fmt(info);
    let out_pix = delivery_pix_fmt(&src_pix, has_alpha, use_hardware, opaque_video);

    if has_alpha {
        // Alpha is VP8/VP9 via libvpx — NVENC has no VP8/VP9 encoder (α or otherwise).
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
            "0".into(),
            "-b:v".into(),
            ceiling.clone(),
            "-maxrate".into(),
            ceiling,
            "-bufsize".into(),
            bufsize,
        ]);
        append_audio_args(&mut cmd, input, info, out_container);
    } else if opaque_video.is_libvpx() {
        append_libvpx_opaque_args(
            &mut cmd,
            opaque_video.ffmpeg_encoder(false),
            &ceiling,
            &bufsize,
        );
        append_audio_args(&mut cmd, input, info, out_container);
    } else if !use_nvenc {
        // CPU HEVC/H.264/AV1 — keep source chroma/bit-depth; CRF0 capped at source ceiling.
        let enc = opaque_video.ffmpeg_encoder(false);
        cmd.extend([
            "-c:v".into(),
            enc.into(),
            "-pix_fmt".into(),
            out_pix.into(),
            "-crf".into(),
            "0".into(),
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
        append_audio_args(&mut cmd, input, info, out_container);
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
            "0".into(),
            "-b:v".into(),
            "0".into(),
            "-maxrate".into(),
            ceiling,
            "-bufsize".into(),
            bufsize,
        ]);
        append_nvenc_profile(&mut cmd, opaque_video, out_pix);
        append_audio_args(&mut cmd, input, info, out_container);
    }

    if preserve_obs {
        // Keep creation_time / chapters / tags from OBS Hybrid MP4 across re-encode.
        cmd.extend([
            "-map_metadata".into(),
            "0".into(),
            "-map_chapters".into(),
            "0".into(),
        ]);
    }

    cmd.push("-avoid_negative_ts".into());
    cmd.push("make_zero".into());
    cmd.push(output.to_string_lossy().into_owned());
    cmd
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

/// Hard-stop ffmpeg. On Windows, `Child::kill` alone often leaves shim/worker trees alive.
fn force_kill_ffmpeg(child: &mut Child) {
    let pid = child.id();
    #[cfg(windows)]
    {
        // /T = process tree (shims + real ffmpeg.exe); /F = force.
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
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
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ffmpeg spawn: {e}"))?;

    let mut stderr = child
        .stderr
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
            force_kill_ffmpeg(&mut child);
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
                    return Err("cancelled".into());
                }
                return Err(format!("ffmpeg exited with {status}"));
            }
            Ok(None) => {}
            Err(e) => {
                force_kill_ffmpeg(&mut child);
                return Err(format!("ffmpeg wait: {e}"));
            }
        }
        match rx.recv_timeout(poll) {
            Ok(Ok(raw)) => emit(&raw, &mut on_line),
            Ok(Err(e)) => {
                force_kill_ffmpeg(&mut child);
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
        .and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(0.0)
        .max(0.0)
}

/// Encode all matching files in `opts.input_dir` into `opts.output_dir` sequentially.
/// Calls `on_event` for UI logging. Honors `cancel` between files.
pub fn run_batch(
    opts: EncodeOptions,
    cancel: Arc<AtomicBool>,
    mut on_event: impl FnMut(EncodeEvent),
) {
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
        name: String,
        info: serde_json::Value,
        has_alpha: bool,
        output: PathBuf,
        duration_s: f64,
    }
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
        let container = if has_alpha {
            opts.alpha_container
        } else {
            opts.opaque_container
        };
        let output = deliver_path(&input, &opts.output_dir, container);
        if output.is_file() {
            skipped += 1;
            on_event(EncodeEvent::FileSkipped { name });
            continue;
        }
        let duration_s = format_duration_s(&info);
        work.push(WorkItem {
            input,
            name,
            info,
            has_alpha,
            output,
            duration_s,
        });
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
        } = item;

        on_event(EncodeEvent::FileStarted {
            name: name.clone(),
            index: i + 1,
            total: total_files,
            duration_s,
        });
        let (auto_k, auto_label) =
            video_ceiling_k(&input, &info, opts.use_peak_bitrate);
        // Manual override replaces auto ceiling so users can raise (or lower) maxrate.
        let (bitrate_k, src_label) = match opts.maxrate_override_k.filter(|&k| k > 0) {
            Some(k) => (k, format!("override {k}k (auto {auto_k}k / {auto_label})")),
            None => (auto_k, auto_label),
        };
        let use_nvenc = opts.use_hardware && opts.opaque_video.is_nvenc() && !has_alpha;
        let path_label = if has_alpha {
            format!("α {} {}", opts.alpha_video.label(), opts.alpha_container.label())
        } else {
            format!(
                "opaque {} {}",
                opts.opaque_video.label(),
                opts.opaque_container.label()
            )
        };
        let device = if use_nvenc {
            format!("gpu={}", opts.gpu.ffmpeg_arg())
        } else {
            format!(
                "cpu={}",
                if has_alpha {
                    opts.alpha_video.ffmpeg_encoder()
                } else {
                    opts.opaque_video.ffmpeg_encoder(false)
                }
            )
        };
        on_event(EncodeEvent::Log(format!(
            "{name}: {path_label} → {}, {bitrate_k}k ({src_label}), {device}",
            output
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )));

        let argv = build_cmd(
            &input,
            &output,
            &info,
            has_alpha,
            opts.gpu,
            bitrate_k,
            opts.preserve_obs_metadata,
            opts.use_hardware,
            opts.opaque_video,
            opts.opaque_container,
            opts.alpha_video,
            opts.alpha_container,
        );
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
        match run_ffmpeg(&argv, &cancel, |line, out_time| {
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
        }) {
            Ok(()) => {
                flush_thumbs(&mut on_event);
                ok += 1;
                on_event(EncodeEvent::FileDone { name });
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
