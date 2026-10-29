//! VMAF compare: input (reference) vs output (distorted) via ffmpeg `libvmaf`.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;

use crate::check::list_media;
use crate::job_log::{JobLog, JobLogKind};
use crate::probe::{file_info, video_has_alpha};
use crate::targets::OutputTargets;

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
    FileDone { name: String, score: f64 },
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

fn n_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, 32))
        .unwrap_or(8)
}

fn force_kill(child: &mut Child) {
    let pid = child.id();
    #[cfg(windows)]
    {
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

/// Run libvmaf: `reference` = original, `distorted` = encode. Returns pooled score.
/// `on_update` gets `(optional log line, optional out_time_s)`.
pub fn run_vmaf_pair(
    reference: &Path,
    distorted: &Path,
    cancel: &AtomicBool,
    mut on_update: impl FnMut(Option<String>, Option<f64>),
) -> Result<f64, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    let threads = n_threads().to_string();
    let filter = format!(
        "[0:v]setpts=PTS-STARTPTS[ref];[1:v]setpts=PTS-STARTPTS[dist];[ref][dist]libvmaf=n_threads={threads}"
    );
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-stats",
            "-stats_period",
            "0.5",
            "-i",
            &reference.to_string_lossy(),
            "-i",
            &distorted.to_string_lossy(),
            "-filter_complex",
            &filter,
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ffmpeg spawn: {e}"))?;

    let mut stderr = child
        .stderr
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
    let poll = Duration::from_millis(50);
    loop {
        if cancel.load(Ordering::Relaxed) {
            force_kill(&mut child);
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
                    return Err("cancelled".into());
                }
                if !status.success() && score.is_none() {
                    return Err(format!("ffmpeg exited with {status}"));
                }
                break;
            }
            Ok(None) => {}
            Err(e) => {
                force_kill(&mut child);
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
                force_kill(&mut child);
                return Err(e);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => continue,
        }
    }

    score.ok_or_else(|| "VMAF score not found in ffmpeg output (is libvmaf enabled?)".into())
}

/// (input_name, output_name, input_path, output_path, duration_s)
fn resolve_pairs(
    input_dir: &Path,
    output_dir: &Path,
    targets: OutputTargets,
    only_basename: Option<&str>,
) -> Result<Vec<(String, String, PathBuf, PathBuf, f64)>, String> {
    let inputs = list_media(input_dir, true);
    let outputs = list_media(output_dir, false);
    let output_set: HashSet<String> = outputs
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();

    let mut pairs = Vec::new();
    for inp in &inputs {
        let Some(in_name) = inp.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if let Some(only) = only_basename {
            let in_stem = Path::new(&in_name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(&in_name);
            let only_stem = Path::new(only)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(only);
            if in_name != only && in_stem != only_stem {
                continue;
            }
        }
        let info = match file_info(inp) {
            Ok(i) => i,
            Err(e) => return Err(format!("{in_name}: probe failed: {e}")),
        };
        let has_alpha = video_has_alpha(&info);
        let out_name = targets.expected_name(&in_name, has_alpha);
        if !output_set.contains(&out_name) {
            if only_basename.is_some() {
                return Err(format!(
                    "no output for {in_name} (expected {out_name})"
                ));
            }
            continue;
        }
        let duration_s = format_duration_s(&info);
        pairs.push((
            in_name,
            out_name.clone(),
            inp.clone(),
            output_dir.join(&out_name),
            duration_s,
        ));
    }
    if pairs.is_empty() {
        return Err(if only_basename.is_some() {
            "no matching input/output pair for VMAF".into()
        } else {
            "no input/output pairs to score".into()
        });
    }
    Ok(pairs)
}

/// Score one pair (`only_basename`) or every matched pair under the folders.
/// Also writes a gzipped per-job log under `%APPDATA%\GigaMux\config\logs\`.
pub fn run_vmaf(
    input_dir: &Path,
    output_dir: &Path,
    targets: OutputTargets,
    only_basename: Option<String>,
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
        targets,
        only_basename.as_deref(),
    ) {
        Ok(p) => p,
        Err(e) => {
            on_event(VmafEvent::Log(e.clone()));
            on_event(VmafEvent::Finished { ok: 0, failed: 0 });
            return Err(e);
        }
    };
    let total_files = pairs.len();
    let total_duration_s: f64 = pairs.iter().map(|p| p.4.max(0.0)).sum();
    on_event(VmafEvent::BatchStarted {
        total_files,
        total_duration_s,
    });

    let mut report = VmafReport::default();
    let mut ok = 0usize;
    let mut failed = 0usize;
    for (i, (in_name, out_name, in_path, out_path, duration_s)) in pairs.iter().enumerate() {
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
        }) {
            Ok(score) => {
                ok += 1;
                on_event(VmafEvent::Log(format!("VMAF {in_name}: {score:.4}")));
                on_event(VmafEvent::FileDone {
                    name: in_name.clone(),
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
