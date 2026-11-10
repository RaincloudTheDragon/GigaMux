//! VMAF compare: input (reference) vs output (distorted) via ffmpeg `libvmaf`.

use std::collections::{HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;

use crate::check::{check_chunk_file, is_chunk_filename, list_media};
use crate::chunk::{
    chunk_frame_range, estimate_total_frames, parse_chunk_filename, parse_fps, CHUNK_SEEK_PAD_S,
};
use crate::job_log::{JobLog, JobLogKind};
use crate::probe::{file_info, video_has_alpha};
use crate::targets::{OutputTargets, TestBatch};

/// Optional reference trim for chunk VMAF (distorted is already the chunk, PTS0).
#[derive(Debug, Clone, Copy)]
pub struct VmafRefTrim {
    pub start_frame: u64,
    pub frame_count: u64,
    pub fps: f64,
}

/// One file pair’s VMAF outcome.
#[derive(Debug, Clone)]
pub struct VmafResult {
    pub input: String,
    pub output: String,
    pub score: Option<f64>,
    pub error: Option<String>,
}

/// Batch VMAF report.
#[derive(Debug, Clone, Default)]
pub struct VmafReport {
    pub results: Vec<VmafResult>,
}

/// Progress / log events (mirrors encode so the UI can reuse the dual-bar ETA stack).
#[derive(Debug, Clone)]
pub enum VmafEvent {
    Log(String),
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
    /// Live ffmpeg out-time for the current pair (seconds).
    FileProgress { out_time_s: f64 },
    /// `name` = input basename; `output` = scored encode basename (`testN_*` in test mode).
    FileDone {
        name: String,
        output: String,
        score: f64,
    },
    FileFailed { name: String, error: String },
    Finished { ok: usize, failed: usize },
}

/// True when this ffmpeg build exposes the `libvmaf` filter.
pub fn libvmaf_available() -> bool {
    let Ok(out) = Command::new("ffmpeg")
        .args(["-hide_banner", "-filters"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().any(|l| l.contains("libvmaf"))
}

fn parse_vmaf_score(line: &str) -> Option<f64> {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"VMAF score:\s*([0-9]+(?:\.[0-9]+)?)").expect("vmaf re"));
    re.captures(line)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
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

fn format_duration_s(info: &Value) -> f64 {
    info.get("format")
        .and_then(|f| f.get("duration"))
        .and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(0.0)
}

fn first_video_stream<'a>(info: &'a Value) -> Option<&'a Value> {
    info.get("streams")?
        .as_array()?
        .iter()
        .find(|s| s.get("codec_type").and_then(|t| t.as_str()) == Some("video"))
}

fn probe_video_fps(info: &Value) -> Option<f64> {
    let v = first_video_stream(info)?;
    let rate = v
        .get("r_frame_rate")
        .and_then(|x| x.as_str())
        .or_else(|| v.get("avg_frame_rate").and_then(|x| x.as_str()))?;
    parse_fps(rate)
}

fn probe_nb_frames(info: &Value) -> Option<u64> {
    let v = first_video_stream(info)?;
    v.get("nb_frames")
        .and_then(|x| {
            x.as_u64()
                .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
        })
        .filter(|&n| n > 0)
}

fn n_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, 32))
        .unwrap_or(8)
}

/// Keep recent non-progress stderr lines for failure messages (cap by count).
fn push_stderr_tail(tail: &mut VecDeque<String>, line: &str) {
    let t = line.trim();
    if t.is_empty() {
        return;
    }
    // Drop ffmpeg `-stats` progress spam; keep errors / filter notes.
    if t.starts_with("frame=") || (t.contains("time=") && t.contains("bitrate=")) {
        return;
    }
    const CAP: usize = 8;
    if tail.len() >= CAP {
        tail.pop_front();
    }
    tail.push_back(t.to_string());
}

fn format_ffmpeg_fail(status: &std::process::ExitStatus, tail: &VecDeque<String>) -> String {
    if tail.is_empty() {
        return format!("ffmpeg exited with {status}");
    }
    let mut snippet = tail.iter().cloned().collect::<Vec<_>>().join(" | ");
    const MAX: usize = 400;
    if snippet.len() > MAX {
        snippet.truncate(MAX);
        snippet.push('…');
    }
    format!("ffmpeg exited with {status}: {snippet}")
}

/// Run libvmaf: `reference` = original, `distorted` = encode. Returns pooled score.
/// `ref_trim` seeks/trims the reference only (farm chunk vs matching source range).
/// `on_update` gets `(optional log line, optional out_time_s)`.
pub fn run_vmaf_pair(
    reference: &Path,
    distorted: &Path,
    cancel: &AtomicBool,
    mut on_update: impl FnMut(Option<String>, Option<f64>),
    ref_trim: Option<&VmafRefTrim>,
) -> Result<f64, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    let threads = n_threads().to_string();
    // Chunk trim: same pad + frame trim as encode `ChunkTrim` (reference only; distorted already PTS0).
    let (filter, seek_s, frames_v) = if let Some(tr) = ref_trim {
        let pad_frames = ((tr.fps * CHUNK_SEEK_PAD_S).round() as u64).max(1);
        let pre_frame = tr.start_frame.saturating_sub(pad_frames);
        let seek_s = if pre_frame > 0 {
            Some(pre_frame as f64 / tr.fps)
        } else {
            None
        };
        let rel_start = tr.start_frame - pre_frame;
        let rel_end = rel_start + tr.frame_count;
        let filter = format!(
            "[0:v]trim=start_frame={rel_start}:end_frame={rel_end},setpts=PTS-STARTPTS[ref];[1:v]setpts=PTS-STARTPTS[dist];[ref][dist]libvmaf=n_threads={threads}"
        );
        (filter, seek_s, Some(tr.frame_count))
    } else {
        let filter = format!(
            "[0:v]setpts=PTS-STARTPTS[ref];[1:v]setpts=PTS-STARTPTS[dist];[ref][dist]libvmaf=n_threads={threads}"
        );
        (filter, None, None)
    };

    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-stats".into(),
        "-stats_period".into(),
        "0.5".into(),
    ];
    if let Some(ss) = seek_s {
        args.extend(["-ss".into(), format!("{ss:.6}")]);
    }
    args.extend([
        "-i".into(),
        reference.to_string_lossy().into_owned(),
        "-i".into(),
        distorted.to_string_lossy().into_owned(),
        "-filter_complex".into(),
        filter,
    ]);
    if let Some(n) = frames_v {
        args.extend(["-frames:v".into(), n.to_string()]);
    }
    args.extend(["-f".into(), "null".into(), "-".into()]);

    let spawned = Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ffmpeg spawn: {e}"))?;
    // Tree-kill on drop if still running (Chocolatey shim must not orphan real ffmpeg).
    let mut child = crate::ffmpeg_active::TrackedFfmpegChild::new(spawned);
    // UI may Shutter-style suspend/resume this tree while the pair runs.
    let _active = crate::ffmpeg_active::ActiveFfmpegGuard::new(child.id());

    let mut stderr = child
        .stderr()
        .take()
        .ok_or_else(|| "ffmpeg stderr missing".to_string())?;

    // Byte reader so `\r` progress updates aren't stuck behind `\n`-only BufRead.
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
                            if !line.is_empty() && tx.send(Ok(std::mem::take(&mut line))).is_err() {
                                return;
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

    let mut score = None;
    let mut stderr_tail: VecDeque<String> = VecDeque::new();
    let poll = Duration::from_millis(50);
    loop {
        if cancel.load(Ordering::Relaxed) {
            child.force_kill();
            return Err("cancelled".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let deadline = Instant::now() + Duration::from_millis(200);
                while Instant::now() < deadline {
                    match rx.recv_timeout(Duration::from_millis(20)) {
                        Ok(Ok(raw)) => {
                            let text = String::from_utf8_lossy(&raw);
                            let t = text.trim();
                            push_stderr_tail(&mut stderr_tail, t);
                            if let Some(s) = parse_vmaf_score(t) {
                                score = Some(s);
                            }
                            let out_t = parse_ffmpeg_out_time(t);
                            let log = if t.contains("VMAF score") {
                                Some(t.to_string())
                            } else {
                                None
                            };
                            if log.is_some() || out_t.is_some() {
                                on_update(log, out_t);
                            }
                        }
                        Ok(Err(e)) => return Err(e),
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                if cancel.load(Ordering::Relaxed) {
                    // Shim may have exited while a grandchild encoder kept running.
                    child.force_kill();
                    return Err("cancelled".into());
                }
                if !status.success() && score.is_none() {
                    return Err(format_ffmpeg_fail(&status, &stderr_tail));
                }
                break;
            }
            Ok(None) => {}
            Err(e) => {
                child.force_kill();
                return Err(format!("ffmpeg wait: {e}"));
            }
        }
        match rx.recv_timeout(poll) {
            Ok(Ok(raw)) => {
                let text = String::from_utf8_lossy(&raw);
                let t = text.trim();
                if t.is_empty() {
                    continue;
                }
                push_stderr_tail(&mut stderr_tail, t);
                if let Some(s) = parse_vmaf_score(t) {
                    score = Some(s);
                }
                let out_t = parse_ffmpeg_out_time(t);
                let log = if t.contains("VMAF score") {
                    Some(t.to_string())
                } else {
                    None
                };
                if log.is_some() || out_t.is_some() {
                    on_update(log, out_t);
                }
            }
            Ok(Err(e)) => {
                child.force_kill();
                return Err(e);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => continue,
        }
    }

    score.ok_or_else(|| {
        let base = "VMAF score not found in ffmpeg output (is libvmaf enabled?)";
        if stderr_tail.is_empty() {
            base.into()
        } else {
            // Reuse fail formatter shape without inventing a fake exit status.
            let mut snippet = stderr_tail.iter().cloned().collect::<Vec<_>>().join(" | ");
            if snippet.len() > 400 {
                snippet.truncate(400);
                snippet.push('…');
            }
            format!("{base}: {snippet}")
        }
    })
}

/// True when `only` selects this input (or a final/test output for it). Chunk-only basenames are handled separately.
fn only_matches_input(only: &str, in_name: &str) -> bool {
    if is_chunk_filename(only) {
        return false;
    }
    let in_stem = Path::new(in_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(in_name);
    let only_stem = Path::new(only)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(only);
    let only_pair = crate::targets::pair_stem(only);
    in_name == only || in_stem == only_stem || in_stem == only_pair.as_str()
}

/// (input_name, output_name, input_path, output_path, duration_s, optional ref trim for chunks)
fn resolve_pairs(
    input_dir: &Path,
    output_dir: &Path,
    slots: &[(Option<TestBatch>, OutputTargets)],
    only_basename: Option<&str>,
    only_outputs: Option<&HashSet<String>>,
) -> Result<Vec<(String, String, PathBuf, PathBuf, f64, Option<VmafRefTrim>)>, String> {
    let inputs = list_media(input_dir, true);
    let outputs = list_media(output_dir, false);
    let output_set: HashSet<String> = outputs
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();

    // Per-input probe + deliver stems (reuse for finals and leftover farm chunks).
    struct InMeta {
        name: String,
        path: PathBuf,
        duration_s: f64,
        total_frames: Option<u64>,
        fps: Option<f64>,
        deliver_stems: HashSet<String>,
        has_alpha: bool,
    }
    let only_chunk_stem = only_basename
        .and_then(parse_chunk_filename)
        .map(|(stem, _, _, _)| stem);

    let mut metas: Vec<InMeta> = Vec::new();
    for inp in &inputs {
        let Some(in_name) = inp.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        // Input/final selection filters before probe; chunk selection filters after deliver stems.
        if let Some(only) = only_basename {
            if only_chunk_stem.is_none() && !only_matches_input(only, &in_name) {
                continue;
            }
        }
        let info = match file_info(inp) {
            Ok(i) => i,
            Err(e) => return Err(format!("{in_name}: probe failed: {e}")),
        };
        let has_alpha = video_has_alpha(&info);
        let duration_s = format_duration_s(&info);
        let fps = probe_video_fps(&info);
        let total_frames =
            estimate_total_frames(duration_s, fps.unwrap_or(0.0), probe_nb_frames(&info));
        let mut deliver_stems = HashSet::new();
        for &(batch, slot) in slots {
            let out_name = slot.expected_name_for(&in_name, has_alpha, batch);
            if let Some(s) = Path::new(&out_name).file_stem().and_then(|s| s.to_str()) {
                deliver_stems.insert(s.to_string());
            }
        }
        if let Some(s) = Path::new(&in_name).file_stem().and_then(|s| s.to_str()) {
            deliver_stems.insert(s.to_string());
        }
        if let Some(stem) = only_chunk_stem.as_ref() {
            if !deliver_stems.contains(stem) {
                continue;
            }
        }
        metas.push(InMeta {
            name: in_name,
            path: inp.clone(),
            duration_s,
            total_frames,
            fps,
            deliver_stems,
            has_alpha,
        });
    }

    let mut pairs = Vec::new();
    // Context-menu on a specific chunk: skip finals; only that chunk is scored below.
    if !only_basename.map(is_chunk_filename).unwrap_or(false) {
        for meta in &metas {
            for &(batch, slot) in slots {
                let out_name = slot.expected_name_for(&meta.name, meta.has_alpha, batch);
                if !output_set.contains(&out_name) {
                    continue;
                }
                // Auto-VMAF / filtered batches: only score listed output basenames.
                if let Some(allow) = only_outputs {
                    if !allow.contains(&out_name) {
                        continue;
                    }
                }
                pairs.push((
                    meta.name.clone(),
                    out_name.clone(),
                    meta.path.clone(),
                    output_dir.join(&out_name),
                    meta.duration_s,
                    None,
                ));
            }
        }
    }

    // Leftover farm chunks: parse basename → matching input → same frame range as submit-time encode.
    let only_chunk = only_basename.filter(|o| is_chunk_filename(o));
    let mut chunk_pairs: Vec<(String, u32, (String, String, PathBuf, PathBuf, f64, Option<VmafRefTrim>))> =
        Vec::new();
    for out_name in &output_set {
        let Some((stem, index, count, _)) = parse_chunk_filename(out_name) else {
            continue;
        };
        if let Some(only) = only_chunk {
            if only != out_name.as_str() {
                continue;
            }
        } else if only_basename.is_some() {
            // Input/final selection: metas are already filtered; require deliver-stem ownership.
            if !metas.iter().any(|m| m.deliver_stems.contains(&stem)) {
                continue;
            }
        }
        if let Some(allow) = only_outputs {
            if !allow.contains(out_name) {
                continue;
            }
        }
        let Some(meta) = metas.iter().find(|m| m.deliver_stems.contains(&stem)) else {
            continue;
        };
        let (Some(total_frames), Some(fps)) = (meta.total_frames, meta.fps) else {
            return Err(format!(
                "{}: cannot estimate frames/fps for chunk VMAF",
                meta.name
            ));
        };
        let (start_frame, frame_count) = chunk_frame_range(total_frames, index, count)?;
        let duration_s = frame_count as f64 / fps;
        let out_path = output_dir.join(out_name);
        // Same healthcheck as Check / ensure-chunk — skip incomplete mid-farm leftovers.
        if let Err(e) = check_chunk_file(&out_path, duration_s) {
            if only_chunk.is_some() {
                return Err(format!("{out_name}: {e}"));
            }
            continue;
        }
        chunk_pairs.push((
            stem,
            index,
            (
                meta.name.clone(),
                out_name.clone(),
                meta.path.clone(),
                out_path,
                duration_s,
                Some(VmafRefTrim {
                    start_frame,
                    frame_count,
                    fps,
                }),
            ),
        ));
    }
    // Stable log order: deliver stem, then chunk index (c000, c001, …).
    chunk_pairs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    pairs.extend(chunk_pairs.into_iter().map(|(_, _, p)| p));

    if pairs.is_empty() {
        // Allowlist filter with nothing left is a soft empty batch (caller logs).
        if only_outputs.is_some() {
            return Ok(pairs);
        }
        if let Some(only) = only_basename {
            if !is_chunk_filename(only) {
                if let Some(meta) = metas.first() {
                    let expected: Vec<String> = slots
                        .iter()
                        .map(|&(batch, slot)| {
                            slot.expected_name_for(&meta.name, meta.has_alpha, batch)
                        })
                        .collect();
                    return Err(format!(
                        "no output for {} (expected {})",
                        meta.name,
                        expected.join(" / ")
                    ));
                }
            }
            return Err("no matching input/output pair for VMAF".into());
        }
        return Err("no input/output pairs to score".into());
    }
    Ok(pairs)
}

/// Score one pair (`only_basename`), an output-name allowlist, or every matched pair.
/// Also writes a gzipped per-job log under `%APPDATA%\GigaMux\config\logs\`.
/// `slots` is non-test `(None, …)` or one entry per active test batch.
pub fn run_vmaf(
    input_dir: &Path,
    output_dir: &Path,
    slots: &[(Option<TestBatch>, OutputTargets)],
    only_basename: Option<String>,
    only_outputs: Option<HashSet<String>>,
    cancel: &AtomicBool,
    mut on_event: impl FnMut(VmafEvent),
) -> Result<VmafReport, String> {
    // Durable job log (best-effort); UI still gets every event via the wrapped callback.
    let (mut job_log, start_msg) = match JobLog::start(JobLogKind::Vmaf) {
        Ok(log) => {
            let msg = format!("job log: {}", log.path().display());
            (Some(log), msg)
        }
        Err(e) => (None, format!("job log unavailable: {e}")),
    };
    let mut on_event = |ev: VmafEvent| {
        if let Some(log) = job_log.as_mut() {
            log.record_vmaf(&ev);
        }
        on_event(ev);
    };
    on_event(VmafEvent::Log(start_msg));

    if !libvmaf_available() {
        let err =
            "ffmpeg has no libvmaf filter — install a full build (e.g. gyan.dev full)".to_string();
        on_event(VmafEvent::Log(err.clone()));
        on_event(VmafEvent::Finished { ok: 0, failed: 0 });
        return Err(err);
    }
    if !input_dir.is_dir() || !output_dir.is_dir() {
        let err = "input and output directories must both exist".to_string();
        on_event(VmafEvent::Log(err.clone()));
        on_event(VmafEvent::Finished { ok: 0, failed: 0 });
        return Err(err);
    }

    let pairs = match resolve_pairs(
        input_dir,
        output_dir,
        slots,
        only_basename.as_deref(),
        only_outputs.as_ref(),
    ) {
        Ok(p) => p,
        Err(e) => {
            on_event(VmafEvent::Log(e.clone()));
            on_event(VmafEvent::Finished { ok: 0, failed: 0 });
            return Err(e);
        }
    };
    if pairs.is_empty() {
        on_event(VmafEvent::Log("VMAF: nothing to score".into()));
        on_event(VmafEvent::Finished { ok: 0, failed: 0 });
        return Ok(VmafReport::default());
    }
    let total_files = pairs.len();
    let total_duration_s: f64 = pairs.iter().map(|p| p.4.max(0.0)).sum();
    on_event(VmafEvent::BatchStarted {
        total_files,
        total_duration_s,
    });

    let mut report = VmafReport::default();
    let mut ok = 0usize;
    let mut failed = 0usize;
    for (i, (in_name, out_name, in_path, out_path, duration_s, ref_trim)) in pairs.iter().enumerate()
    {
        if cancel.load(Ordering::Relaxed) {
            on_event(VmafEvent::Log("VMAF cancelled".into()));
            break;
        }
        on_event(VmafEvent::FileStarted {
            name: in_name.clone(),
            index: i + 1,
            total: total_files,
            duration_s: *duration_s,
        });
        on_event(VmafEvent::Log(format!(
            "VMAF {}/{}: {in_name} → {out_name}…",
            i + 1,
            total_files
        )));
        match run_vmaf_pair(in_path, out_path, cancel, |log, out_t| {
            if let Some(t) = out_t {
                on_event(VmafEvent::FileProgress { out_time_s: t });
            }
            if let Some(line) = log {
                on_event(VmafEvent::Log(line));
            }
        }, ref_trim.as_ref()) {
            Ok(score) => {
                ok += 1;
                on_event(VmafEvent::Log(format!("VMAF {in_name}: {score:.4}")));
                on_event(VmafEvent::FileDone {
                    name: in_name.clone(),
                    output: out_name.clone(),
                    score,
                });
                report.results.push(VmafResult {
                    input: in_name.clone(),
                    output: out_name.clone(),
                    score: Some(score),
                    error: None,
                });
            }
            Err(e) => {
                failed += 1;
                on_event(VmafEvent::Log(format!("VMAF {in_name}: failed — {e}")));
                on_event(VmafEvent::FileFailed {
                    name: in_name.clone(),
                    error: e.clone(),
                });
                report.results.push(VmafResult {
                    input: in_name.clone(),
                    output: out_name.clone(),
                    score: None,
                    error: Some(e),
                });
            }
        }
    }
    on_event(VmafEvent::Finished { ok, failed });
    Ok(report)
}
