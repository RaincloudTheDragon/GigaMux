//! Local sequential encode (opaque HEVC NVENC / alpha VP9). No chunking.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::gpu::GpuChoice;
use crate::probe::{
    audio_bitrate_k, file_info, require_tools, video_bitrate_k, video_codec_name, video_has_alpha,
};

const VIDEO_EXTS: &[&str] = &["mp4", "mkv", "mov", "webm"];

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

fn deliver_path(input: &Path, output_dir: &Path, has_alpha: bool) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default();
    let ext = if has_alpha { "webm" } else { "mp4" };
    output_dir.join(stem).with_extension(ext)
}

fn build_cmd(
    input: &Path,
    output: &Path,
    info: &serde_json::Value,
    has_alpha: bool,
    gpu: GpuChoice,
    bitrate_k: u32,
    preserve_obs: bool,
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
    if has_alpha && (vcodec == "vp9" || vcodec == "vp8") {
        cmd.push("-c:v".into());
        cmd.push(if vcodec == "vp9" {
            "libvpx-vp9".into()
        } else {
            "libvpx".into()
        });
    }
    cmd.push("-i".into());
    cmd.push(input.to_string_lossy().into_owned());

    if has_alpha {
        cmd.extend([
            "-c:v".into(),
            "libvpx-vp9".into(),
            "-pix_fmt".into(),
            "yuva420p".into(),
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
            "10".into(),
            "-b:v".into(),
            ceiling,
            "-overshoot-pct".into(),
            "0".into(),
            "-c:a".into(),
            "copy".into(),
            "-map".into(),
            "0".into(),
        ]);
    } else {
        cmd.extend([
            "-c:v".into(),
            "hevc_nvenc".into(),
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
        let is_webm = input
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("webm"))
            .unwrap_or(false);
        if is_webm {
            let a_k = audio_bitrate_k(input);
            cmd.extend([
                "-c:a".into(),
                "aac".into(),
                "-b:a".into(),
                format!("{a_k}k"),
                "-map".into(),
                "0".into(),
            ]);
        } else {
            cmd.extend(["-c:a".into(), "copy".into(), "-map".into(), "0".into()]);
        }
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

/// Run ffmpeg and stream stderr live (progress uses `\r`; other lines use `\n`).
/// `on_line` gets `(log_text, optional out_time_seconds from time=)`.
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

    // Byte-wise so `\r` progress updates flush as lines (same as encode_VOD.py readline).
    let mut line = Vec::new();
    let mut buf = [0u8; 4096];
    let mut last_cancel_check = Instant::now();
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
    loop {
        if last_cancel_check.elapsed() > Duration::from_millis(200) {
            last_cancel_check = Instant::now();
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                return Err("cancelled".into());
            }
        }
        match stderr.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                for &b in &buf[..n] {
                    if b == b'\n' || b == b'\r' {
                        emit(&line, &mut on_line);
                        line.clear();
                    } else if b != 0 {
                        line.push(b);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("ffmpeg read: {e}"));
            }
        }
    }
    emit(&line, &mut on_line);

    let status = child.wait().map_err(|e| format!("ffmpeg wait: {e}"))?;
    if status.success() {
        Ok(())
    } else if cancel.load(Ordering::Relaxed) {
        Err("cancelled".into())
    } else {
        Err(format!("ffmpeg exited with {status}"))
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
                "no .mp4/.mkv/.mov/.webm files in input folder".into()
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
        let output = deliver_path(&input, &opts.output_dir, has_alpha);
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
        let (bitrate_k, src_label) = video_bitrate_k(&input, &info);
        on_event(EncodeEvent::Log(format!(
            "{name}: {} → {}, {bitrate_k}k ({src_label}), gpu={}",
            if has_alpha { "alpha VP9" } else { "opaque HEVC" },
            output
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            opts.gpu.ffmpeg_arg(),
        )));

        let argv = build_cmd(
            &input,
            &output,
            &info,
            has_alpha,
            opts.gpu,
            bitrate_k,
            opts.preserve_obs_metadata,
        );
        match run_ffmpeg(&argv, &cancel, |line, out_time| {
            if let Some(t) = out_time {
                on_event(EncodeEvent::FileProgress { out_time_s: t });
            }
            on_event(EncodeEvent::Log(line));
        }) {
            Ok(()) => {
                ok += 1;
                on_event(EncodeEvent::FileDone { name });
            }
            Err(e) => {
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
