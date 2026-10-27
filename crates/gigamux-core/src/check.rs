//! Verify input/output encode pairs (port of check_files.py).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use regex::Regex;
use serde_json::Value;

use crate::probe::{file_info, video_has_alpha};

const INPUT_EXTS: &[&str] = &["mp4", "mkv", "mov", "webm"];
const OUTPUT_EXTS: &[&str] = &["mp4", "webm"];
const STREAM_END_SLACK_S: f64 = 1.0;
const STREAM_END_SLACK_FRAC: f64 = 0.002;

/// One reported mismatch or probe failure.
#[derive(Debug, Clone)]
pub struct CheckIssue {
    pub input: Option<String>,
    pub output: Option<String>,
    pub message: String,
}

/// Full directory check result.
#[derive(Debug, Clone, Default)]
pub struct CheckReport {
    pub issues: Vec<CheckIssue>,
    pub ok_pairs: usize,
}

impl CheckReport {
    pub fn ok(&self) -> bool {
        self.issues.is_empty()
    }
}

/// Farm intermediate like `stem.c000of004.webm` — not a final deliver.
pub fn is_chunk_filename(name: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)^.+\.c\d{3}of\d{3}\.(mp4|webm)$").expect("chunk name regex")
    });
    re.is_match(name)
}

fn ext_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default()
}

fn list_named(dir: &Path, exts: &[&str]) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("read dir: {e}"))? {
        let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if exts.contains(&ext_of(name).as_str()) {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

fn expected_output_name(input_name: &str, has_alpha: bool) -> String {
    let stem = Path::new(input_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(input_name);
    format!("{stem}.{}", if has_alpha { "webm" } else { "mp4" })
}

fn first_video(info: &Value) -> Option<&Value> {
    info.get("streams")?
        .as_array()?
        .iter()
        .find(|s| s.get("codec_type").and_then(|v| v.as_str()) == Some("video"))
}

fn format_duration(info: &Value) -> Option<f64> {
    info.pointer("/format/duration").and_then(|v| {
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

fn implied_bitrate_bps(info: &Value) -> Option<f64> {
    let size = info.pointer("/format/size").and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })?;
    let dur = format_duration(info)?;
    if dur <= 0.0 {
        return None;
    }
    Some((size as f64) * 8.0 / dur)
}

fn parse_pts_lines(text: &str) -> Option<f64> {
    let mut last = None;
    for line in text.lines() {
        let token = line.trim().trim_end_matches(',');
        if token.is_empty() {
            continue;
        }
        if let Ok(v) = token.parse::<f64>() {
            last = Some(v);
        }
    }
    last
}

/// Probe last video `pts_time` (and whether ffprobe said the file ended early).
fn probe_last_video_pts(path: &Path, read_intervals: Option<&str>) -> (Option<f64>, bool) {
    let path_s = path.to_string_lossy();
    let mut args: Vec<&str> = vec!["-v", "error", "-i", path_s.as_ref(), "-select_streams", "v:0"];
    if let Some(iv) = read_intervals {
        args.extend(["-read_intervals", iv]);
    }
    args.extend(["-show_entries", "packet=pts_time", "-of", "csv=p=0"]);
    match Command::new("ffprobe").args(&args).output() {
        Ok(out) => {
            let premature = String::from_utf8_lossy(&out.stderr)
                .to_lowercase()
                .contains("ended prematurely");
            (
                parse_pts_lines(&String::from_utf8_lossy(&out.stdout)),
                premature,
            )
        }
        Err(_) => (None, false),
    }
}

/// True end of the video bitstream (last packet pts), not container/tag duration.
///
/// Fast path seeks near the end, but HEVC/NVENC long GOPs make `-read_intervals` land on a prior keyframe — a tiny packet window then looks "truncated". If that sample is short of the declared duration, fall back to a full packet walk.
fn written_media_seconds(path: &Path, declared_duration: Option<f64>) -> (Option<f64>, bool) {
    let slack = |dur: f64| STREAM_END_SLACK_S.max(dur * STREAM_END_SLACK_FRAC);

    if let Some(dur) = declared_duration {
        if dur > 1.0 {
            // Seek well before the end so a long GOP still leaves enough packets to walk past the declared duration (60fps × 30s ≈ 1800).
            let seek = (dur - 30.0).max(0.0);
            let (last, premature) =
                probe_last_video_pts(path, Some(&format!("{seek}%+#2400")));
            match last {
                Some(w) if premature || w + slack(dur) >= dur => return (Some(w), premature),
                Some(_) | None => {
                    // Sample inconclusive (keyframe seek still short) — full scan.
                }
            }
        }
    }

    probe_last_video_pts(path, None)
}

fn check_pair_inner(
    output_path: &Path,
    input_info: &Value,
    output_info: &Value,
) -> Option<String> {
    let input_duration = match format_duration(input_info) {
        Some(d) => d,
        None => return Some("Input missing duration".into()),
    };
    let output_duration = match format_duration(output_info) {
        Some(d) => d,
        None => return Some("Output missing duration".into()),
    };

    // WebM often copies full source duration into tags while clusters stop early.
    if output_duration > 5.0 {
        let (written, premature) = written_media_seconds(output_path, Some(output_duration));
        let slack = STREAM_END_SLACK_S.max(output_duration * STREAM_END_SLACK_FRAC);
        match written {
            None => {
                return Some(format!(
                    "Incomplete output: declared {output_duration:.2}s but no video packets readable"
                ));
            }
            Some(w) if premature || w + slack < output_duration => {
                let bps_note = implied_bitrate_bps(output_info)
                    .map(|b| format!(", size implies {:.1} kbps", b / 1000.0))
                    .unwrap_or_default();
                return Some(format!(
                    "Incomplete output: declared {output_duration:.2}s but video stream ends at ~{w:.2}s{bps_note}"
                ));
            }
            _ => {}
        }
    }

    let duration_diff = (input_duration - output_duration).abs();
    if duration_diff > 0.1 {
        return Some(format!(
            "Duration mismatch: Input={input_duration:.2}s, Output={output_duration:.2}s, Diff={duration_diff:.2}s"
        ));
    }

    let (Some(input_video), Some(output_video)) =
        (first_video(input_info), first_video(output_info))
    else {
        return Some("Missing video stream in input or output".into());
    };

    let in_w = input_video.get("width").and_then(|v| v.as_u64()).unwrap_or(0);
    let in_h = input_video.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
    let out_w = output_video.get("width").and_then(|v| v.as_u64()).unwrap_or(0);
    let out_h = output_video.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
    if in_w != out_w || in_h != out_h {
        return Some(format!(
            "Resolution mismatch: Input={in_w}x{in_h}, Output={out_w}x{out_h}"
        ));
    }

    let in_alpha = video_has_alpha(input_info);
    let out_alpha = video_has_alpha(output_info);
    let out_codec = output_video
        .get("codec_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let out_ext = output_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if in_alpha {
        if out_ext != "webm" {
            return Some(format!(
                "Container mismatch: alpha input expects .webm, got .{out_ext}"
            ));
        }
        if out_codec != "vp9" {
            return Some(format!(
                "Codec mismatch: alpha input expects vp9, got {out_codec}"
            ));
        }
        if !out_alpha {
            return Some("Alpha mismatch: input has alpha but output does not".into());
        }
    } else {
        if out_ext != "mp4" {
            return Some(format!(
                "Container mismatch: opaque input expects .mp4, got .{out_ext}"
            ));
        }
        if out_codec != "hevc" && out_codec != "h265" {
            return Some(format!(
                "Codec mismatch: opaque input expects hevc, got {out_codec}"
            ));
        }
        if out_alpha {
            return Some("Alpha mismatch: opaque input but output has alpha".into());
        }
    }

    let in_audio: Vec<_> = input_info
        .get("streams")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter(|s| s.get("codec_type").and_then(|v| v.as_str()) == Some("audio"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let out_audio: Vec<_> = output_info
        .get("streams")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter(|s| s.get("codec_type").and_then(|v| v.as_str()) == Some("audio"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if in_audio.len() != out_audio.len() {
        return Some(format!(
            "Audio stream count mismatch: Input={}, Output={}",
            in_audio.len(),
            out_audio.len()
        ));
    }
    for (i, (ina, outa)) in in_audio.iter().zip(out_audio.iter()).enumerate() {
        if ina.get("channels") != outa.get("channels") {
            return Some(format!(
                "Audio channel mismatch in stream {i}: Input={:?}, Output={:?}",
                ina.get("channels"),
                outa.get("channels")
            ));
        }
    }

    None
}

/// Check all input/output pairs under the two folders.
pub fn run_check(input_dir: &Path, output_dir: &Path) -> Result<CheckReport, String> {
    if !input_dir.is_dir() || !output_dir.is_dir() {
        return Err("input and output directories must both exist".into());
    }

    let input_files = list_named(input_dir, INPUT_EXTS)?;
    let output_files = list_named(output_dir, OUTPUT_EXTS)?;
    let output_set: HashSet<_> = output_files.iter().cloned().collect();
    let input_stems: HashSet<_> = input_files
        .iter()
        .filter_map(|f| Path::new(f).file_stem()?.to_str().map(|s| s.to_string()))
        .collect();

    let mut report = CheckReport::default();
    let mut input_meta: HashMap<String, Value> = HashMap::new();

    for inf in &input_files {
        match file_info(&input_dir.join(inf)) {
            Ok(info) => {
                input_meta.insert(inf.clone(), info);
            }
            Err(e) => {
                report.issues.push(CheckIssue {
                    input: Some(inf.clone()),
                    output: None,
                    message: format!("Input probe failed: {e}"),
                });
            }
        }
    }

    let mut pairs: Vec<(String, String, Value)> = Vec::new();
    for inf in &input_files {
        let Some(info) = input_meta.get(inf) else {
            continue;
        };
        let has_alpha = video_has_alpha(info);
        let outp = expected_output_name(inf, has_alpha);
        let sibling = expected_output_name(inf, !has_alpha);
        if output_set.contains(&outp) {
            if output_set.contains(&sibling) {
                report.issues.push(CheckIssue {
                    input: Some(inf.clone()),
                    output: Some(sibling.clone()),
                    message: format!(
                        "Container mismatch: unexpected leftover {sibling} (expected {outp})"
                    ),
                });
            }
            pairs.push((inf.clone(), outp, info.clone()));
        } else if output_set.contains(&sibling) {
            report.issues.push(CheckIssue {
                input: Some(inf.clone()),
                output: Some(sibling.clone()),
                message: format!("Container mismatch: found {sibling}, expected {outp}"),
            });
        } else {
            let kind = if has_alpha {
                "VP9+α WebM"
            } else {
                "nvHEVC MP4"
            };
            report.issues.push(CheckIssue {
                input: Some(inf.clone()),
                output: Some(outp.clone()),
                message: format!("Missing from output (expected {outp}, {kind})"),
            });
        }
    }

    for outf in &output_files {
        let stem = Path::new(outf)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if !input_stems.contains(stem) && !is_chunk_filename(outf) {
            report.issues.push(CheckIssue {
                input: None,
                output: Some(outf.clone()),
                message: "Missing from input".into(),
            });
        }
    }

    for (inf, outp, info) in pairs {
        let out_path = output_dir.join(&outp);
        match file_info(&out_path) {
            Ok(out_info) => match check_pair_inner(&out_path, &info, &out_info) {
                None => report.ok_pairs += 1,
                Some(msg) => report.issues.push(CheckIssue {
                    input: Some(inf),
                    output: Some(outp),
                    message: msg,
                }),
            },
            Err(e) => report.issues.push(CheckIssue {
                input: Some(inf),
                output: Some(outp),
                message: format!("Output file error: {e}"),
            }),
        }
    }

    Ok(report)
}

/// True when this issue points at an on-disk output that delete would remove.
/// Skips “Missing from output” (no file exists) and input-only probe failures.
pub fn issue_has_deletable_output(output_dir: &Path, issue: &CheckIssue) -> bool {
    if issue.message.starts_with("Missing from output") {
        return false;
    }
    let Some(name) = &issue.output else {
        return false;
    };
    output_dir.join(name).is_file()
}

/// Issues whose outputs would actually be deleted (for prompting).
pub fn deletable_issues<'a>(output_dir: &Path, issues: &'a [CheckIssue]) -> Vec<&'a CheckIssue> {
    issues
        .iter()
        .filter(|i| issue_has_deletable_output(output_dir, i))
        .collect()
}

/// Delete output files listed on issues (skips “Missing from output”).
pub fn delete_issue_outputs(output_dir: &Path, issues: &[CheckIssue]) -> (usize, usize) {
    let mut deleted = 0usize;
    let mut failed = 0usize;
    for issue in issues {
        if !issue_has_deletable_output(output_dir, issue) {
            continue;
        }
        let name = issue.output.as_ref().expect("deletable implies output");
        match std::fs::remove_file(output_dir.join(name)) {
            Ok(()) => deleted += 1,
            Err(_) => failed += 1,
        }
    }
    (deleted, failed)
}

/// List video filenames in a folder (non-recursive).
pub fn list_media(dir: &Path, input_side: bool) -> Vec<PathBuf> {
    let exts = if input_side { INPUT_EXTS } else { OUTPUT_EXTS };
    list_named(dir, exts)
        .unwrap_or_default()
        .into_iter()
        .map(|n| dir.join(n))
        .collect()
}
