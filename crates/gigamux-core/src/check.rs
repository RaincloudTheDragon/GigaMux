//! Verify input/output encode pairs (port of check_files.py).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use regex::Regex;
use serde_json::Value;

use crate::probe::{file_info, video_has_alpha};
use crate::targets::{pair_stem, OutputTargets, TestBatch};

/// Containers accepted as encode *inputs* (probe + list). Keep broad — disk wins over allowlist nostalgia.
const INPUT_EXTS: &[&str] = &[
    "mp4", "mkv", "mov", "webm", "m4v", "avi", "mpg", "mpeg", "m2v", "m2ts", "mts", "ts", "mxf",
    "ogv", "ogg", "wmv", "flv", "3gp", "3g2",
];
const OUTPUT_EXTS: &[&str] = &["mp4", "webm", "mkv"];
const STREAM_END_SLACK_S: f64 = 1.0;
const STREAM_END_SLACK_FRAC: f64 = 0.002;
/// Allowed |input − output| `nb_frames` difference when both sides report counts.
const NB_FRAMES_SLACK: u64 = 2;

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
        Regex::new(r"(?i)^.+\.c\d{3}of\d{3}\.(mp4|webm|mkv)$").expect("chunk name regex")
    });
    re.is_match(name)
}

/// Absolute duration slack for farm chunk expects (frame-split rounding).
const CHUNK_DUR_SLACK_S: f64 = 0.25;
const CHUNK_DUR_SLACK_FRAC: f64 = 0.02;

/// Force ffmpeg overwrite (`-y`); replaces a leading `-n` if present.
fn argv_force_overwrite(argv: &[String]) -> Vec<String> {
    let mut out = argv.to_vec();
    // Skip argv[0] program name when swapping flags.
    let start = if out.len() > 1 { 1 } else { 0 };
    for a in out.iter_mut().skip(start) {
        if a == "-n" {
            *a = "-y".into();
            return out;
        }
        if a == "-y" {
            return out;
        }
    }
    if out.len() > start {
        out.insert(start, "-y".into());
    }
    out
}

/// Reuse a good existing chunk, otherwise encode with `-y` then healthcheck.
///
/// `ffmpeg_argv` is a full argv (leading program name ignored); `ffmpeg_exe` is what actually runs.
pub fn ensure_chunk_file(
    path: &Path,
    expect_dur_s: f64,
    ffmpeg_exe: &str,
    ffmpeg_argv: &[String],
) -> Result<(), String> {
    if path.is_file() {
        match check_chunk_file(path, expect_dur_s) {
            Ok(()) => return Ok(()), // reuse existing good chunk
            Err(e) => {
                // Bad leftover — remove so `-y` encode is unambiguous on odd filesystems.
                let _ = std::fs::remove_file(path);
                // Surface why we're re-encoding (worker task log).
                eprintln!("ensure-chunk: re-encode after failed check: {e}");
            }
        }
    }
    let exe = {
        let t = ffmpeg_exe.trim();
        if t.is_empty() {
            "ffmpeg"
        } else {
            t
        }
    };
    let argv = argv_force_overwrite(ffmpeg_argv);
    let args: Vec<&str> = if argv.is_empty() {
        Vec::new()
    } else {
        argv[1..].iter().map(String::as_str).collect()
    };
    // Inherit stdio so Flamenco captures live `-stats` (`.output()` buffers until exit → silent task log).
    let status = Command::new(exe)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| format!("ffmpeg start ({exe}): {e}"))?;
    if !status.success() {
        return Err(format!(
            "ffmpeg encode failed (status {:?}); see task log for ffmpeg stderr",
            status.code()
        ));
    }
    check_chunk_file(path, expect_dur_s)
}

/// Reuse a good existing concat/mux output, otherwise run ffmpeg with `-y` then healthcheck.
///
/// Same contract as [`ensure_chunk_file`], but **surfaces** remove failures (locked incomplete leftovers must not block `-y`).
pub fn ensure_out_file(
    path: &Path,
    expect_dur_s: f64,
    ffmpeg_exe: &str,
    ffmpeg_argv: &[String],
) -> Result<(), String> {
    if path.is_file() {
        match check_chunk_file(path, expect_dur_s) {
            Ok(()) => return Ok(()),
            Err(e) => {
                std::fs::remove_file(path).map_err(|re| {
                    format!(
                        "ensure-out: cannot remove bad {}: {re} (check was: {e})",
                        path.display()
                    )
                })?;
                eprintln!("ensure-out: re-run after failed check: {e}");
            }
        }
    }
    let exe = {
        let t = ffmpeg_exe.trim();
        if t.is_empty() {
            "ffmpeg"
        } else {
            t
        }
    };
    let argv = argv_force_overwrite(ffmpeg_argv);
    let args: Vec<&str> = if argv.is_empty() {
        Vec::new()
    } else {
        argv[1..].iter().map(String::as_str).collect()
    };
    // Inherit stdio so Flamenco captures live `-stats`.
    let status = Command::new(exe)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| format!("ffmpeg start ({exe}): {e}"))?;
    if !status.success() {
        return Err(format!(
            "ffmpeg ensure-out failed (status {:?}); see task log for ffmpeg stderr",
            status.code()
        ));
    }
    check_chunk_file(path, expect_dur_s)
}

/// Healthcheck one farm chunk file against its expected duration (video-only).
pub fn check_chunk_file(path: &Path, expect_dur_s: f64) -> Result<(), String> {
    if !path.is_file() {
        return Err(format!("chunk missing: {}", path.display()));
    }
    if expect_dur_s <= 0.0 {
        return Err(format!("invalid expect_dur {expect_dur_s}"));
    }
    let info = file_info(path)?;
    let Some(video) = first_video(&info) else {
        return Err(format!("chunk missing video stream: {}", path.display()));
    };
    let declared = format_duration(&info).ok_or_else(|| {
        format!("chunk missing duration: {}", path.display())
    })?;
    let slack = CHUNK_DUR_SLACK_S.max(expect_dur_s * CHUNK_DUR_SLACK_FRAC);
    if (declared - expect_dur_s).abs() > slack {
        return Err(format!(
            "chunk duration mismatch: expect {expect_dur_s:.3}s, got {declared:.3}s ({})",
            path.display()
        ));
    }
    // Truncated containers: declared tags can lie while packets stop early.
    if declared > 5.0 {
        let codec = video
            .get("codec_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let (written, premature) = written_media_seconds(path, Some(declared), codec);
        let end_slack = STREAM_END_SLACK_S.max(declared * STREAM_END_SLACK_FRAC);
        match written {
            None if premature && codec.eq_ignore_ascii_case("av1") => {
                // Same soft AV1 probe caveat as final check — do not fail the chunk.
            }
            None => {
                return Err(format!(
                    "Incomplete chunk: declared {declared:.2}s but no video packets readable ({})",
                    path.display()
                ));
            }
            Some(w) if premature || w + end_slack < declared => {
                return Err(format!(
                    "Incomplete chunk: declared {declared:.2}s but video ends at ~{w:.2}s ({})",
                    path.display()
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// One input/output pair integrity check (same rules as directory Check).
pub fn check_output_pair(
    input_path: &Path,
    output_path: &Path,
    targets: OutputTargets,
) -> Result<(), String> {
    if !input_path.is_file() {
        return Err(format!("input missing: {}", input_path.display()));
    }
    if !output_path.is_file() {
        return Err(format!("output missing: {}", output_path.display()));
    }
    let input_info = file_info(input_path)?;
    let output_info = file_info(output_path)?;
    match check_pair_inner(output_path, &input_info, &output_info, targets) {
        None => Ok(()),
        Some(msg) => Err(msg),
    }
}

/// Delete farm intermediates for a stem (`*.cNNNofMMM.*`, `.ffconcat`, `.vcat.*`, `.vmux.mkv`). Permanent remove (shared storage).
pub fn cleanup_farm_intermediates(
    output_dir: &Path,
    stem: &str,
    ext: &str,
    chunks: u32,
) -> Result<(), String> {
    let ext = ext.trim_start_matches('.');
    let mut errors = Vec::new();
    let mut remove = |path: PathBuf| {
        if path.is_file() {
            if let Err(e) = std::fs::remove_file(&path) {
                errors.push(format!("{}: {e}", path.display()));
            }
        }
    };
    if chunks > 0 {
        for i in 0..chunks {
            remove(output_dir.join(crate::chunk::chunk_output_name(stem, i, chunks, ext)));
        }
    }
    remove(output_dir.join(format!("{stem}.ffconcat")));
    remove(output_dir.join(format!("{stem}.vcat.{ext}")));
    // Multi-audio MP4 staging (see farm mux MKV→final remux).
    remove(output_dir.join(format!("{stem}.vmux.mkv")));
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
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

fn stream_nb_frames(stream: &Value) -> Option<u64> {
    stream
        .get("nb_frames")
        .and_then(|x| {
            x.as_u64()
                .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
        })
        .filter(|&n| n > 0)
}

fn streams_of_type<'a>(info: &'a Value, codec_type: &str) -> Vec<&'a Value> {
    info.get("streams")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter(|s| s.get("codec_type").and_then(|v| v.as_str()) == Some(codec_type))
                .collect()
        })
        .unwrap_or_default()
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
///
/// `decoder` forces an input codec (e.g. `av1`) so NVENC AV1 isn't walked through ancient libaom.
fn probe_last_video_pts(
    path: &Path,
    read_intervals: Option<&str>,
    decoder: Option<&str>,
) -> (Option<f64>, bool) {
    let path_s = path.to_string_lossy();
    let mut args: Vec<&str> = vec!["-v", "error"];
    if let Some(d) = decoder {
        args.extend(["-c:v", d]);
    }
    args.extend(["-i", path_s.as_ref(), "-select_streams", "v:0"]);
    if let Some(iv) = read_intervals {
        args.extend(["-read_intervals", iv]);
    }
    args.extend(["-show_entries", "packet=pts_time", "-of", "csv=p=0"]);
    match Command::new("ffprobe").args(&args).output() {
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
            // libaom choking on NVENC level indices floods stderr and stalls — treat as unreadable.
            if err.contains("bitstream not supported")
                || err.contains("seq_level_idx")
                || err.contains("decode error rate")
            {
                return (None, true);
            }
            let premature = err.contains("ended prematurely");
            (
                parse_pts_lines(&String::from_utf8_lossy(&out.stdout)),
                premature,
            )
        }
        Err(_) => (None, false),
    }
}

fn packet_probe_decoder(codec_name: &str) -> Option<&'static str> {
    match codec_name.to_ascii_lowercase().as_str() {
        // Prefer generic `av1` over libaom (handles NVENC seq_level values libaom rejects).
        "av1" => Some("av1"),
        _ => None,
    }
}

/// True end of the video bitstream (last packet pts), not container/tag duration.
///
/// Fast path seeks near the end, but HEVC/NVENC long GOPs make `-read_intervals` land on a prior keyframe — a tiny packet window then looks "truncated". If that sample is short of the declared duration, fall back to a full packet walk.
fn written_media_seconds(
    path: &Path,
    declared_duration: Option<f64>,
    codec_name: &str,
) -> (Option<f64>, bool) {
    let slack = |dur: f64| STREAM_END_SLACK_S.max(dur * STREAM_END_SLACK_FRAC);
    let decoder = packet_probe_decoder(codec_name);

    if let Some(dur) = declared_duration {
        if dur > 1.0 {
            // Seek well before the end so a long GOP still leaves enough packets to walk past the declared duration (60fps × 30s ≈ 1800).
            let seek = (dur - 30.0).max(0.0);
            let (last, premature) =
                probe_last_video_pts(path, Some(&format!("{seek}%+#2400")), decoder);
            match last {
                Some(w) if premature || w + slack(dur) >= dur => return (Some(w), premature),
                // Hard decode rejection — don't grind a full-file libaom walk.
                None if premature => return (None, true),
                Some(_) | None => {
                    // Sample inconclusive (keyframe seek still short) — full scan.
                }
            }
        }
    }

    probe_last_video_pts(path, None, decoder)
}

fn check_pair_inner(
    output_path: &Path,
    input_info: &Value,
    output_info: &Value,
    targets: OutputTargets,
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
        let out_codec_early = output_info
            .get("streams")
            .and_then(|s| s.as_array())
            .and_then(|a| {
                a.iter()
                    .find(|s| s.get("codec_type").and_then(|v| v.as_str()) == Some("video"))
            })
            .and_then(|v| v.get("codec_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let (written, premature) =
            written_media_seconds(output_path, Some(output_duration), out_codec_early);
        let slack = STREAM_END_SLACK_S.max(output_duration * STREAM_END_SLACK_FRAC);
        match written {
            None if premature && out_codec_early.eq_ignore_ascii_case("av1") => {
                return Some(format!(
                    "AV1 output unreadable by probe decoder (declared {output_duration:.2}s) — often NVENC seq_level vs libaom; file may still play in NVDEC/dav1d players"
                ));
            }
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
    let expect_ext = targets.container_for(in_alpha).ext();
    let expect_names = if in_alpha {
        targets.alpha_video.probe_names()
    } else {
        targets.opaque_video.probe_names()
    };
    let expect_label = if in_alpha {
        targets.alpha_video.label()
    } else {
        targets.opaque_video.label()
    };

    if out_ext != expect_ext {
        return Some(format!(
            "Container mismatch: {} input expects .{expect_ext}, got .{out_ext}",
            if in_alpha { "alpha" } else { "opaque" }
        ));
    }
    if !expect_names.iter().any(|n| *n == out_codec) {
        return Some(format!(
            "Codec mismatch: {} input expects {expect_label}, got {out_codec}",
            if in_alpha { "alpha" } else { "opaque" }
        ));
    }
    if in_alpha && !out_alpha {
        return Some("Alpha mismatch: input has alpha but output does not".into());
    }
    if !in_alpha && out_alpha {
        return Some("Alpha mismatch: opaque input but output has alpha".into());
    }

    let in_audio = streams_of_type(input_info, "audio");
    let out_audio = streams_of_type(output_info, "audio");

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

    let in_subs = streams_of_type(input_info, "subtitle");
    let out_subs = streams_of_type(output_info, "subtitle");
    if in_subs.len() != out_subs.len() {
        return Some(format!(
            "Subtitle stream count mismatch: Input={}, Output={}",
            in_subs.len(),
            out_subs.len()
        ));
    }

    if let (Some(in_n), Some(out_n)) = (
        stream_nb_frames(input_video),
        stream_nb_frames(output_video),
    ) {
        let diff = in_n.abs_diff(out_n);
        if diff > NB_FRAMES_SLACK {
            return Some(format!(
                "Frame count mismatch: Input={in_n}, Output={out_n}, Diff={diff} (slack {NB_FRAMES_SLACK})"
            ));
        }
    }

    None
}


/// Check all input/output pairs under the two folders against `slots`.
/// Non-test: one `(None, targets)` entry. Test mode: `(Some(TestN), targets)` per active batch.
pub fn run_check(
    input_dir: &Path,
    output_dir: &Path,
    slots: &[(Option<TestBatch>, OutputTargets)],
) -> Result<CheckReport, String> {
    if !input_dir.is_dir() || !output_dir.is_dir() {
        return Err("input and output directories must both exist".into());
    }
    if slots.is_empty() {
        return Err("check slots must not be empty".into());
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

    // (input, output, input_info, targets for this pair)
    let mut pairs: Vec<(String, String, Value, OutputTargets)> = Vec::new();

    for inf in &input_files {
        let Some(info) = input_meta.get(inf) else {
            continue;
        };
        let has_alpha = video_has_alpha(info);
        for &(batch, slot) in slots {
            let outp = slot.expected_name_for(inf, has_alpha, batch);
            let sibling = slot.expected_name_for(inf, !has_alpha, batch);
            if output_set.contains(&outp) {
                if sibling != outp && output_set.contains(&sibling) {
                    report.issues.push(CheckIssue {
                        input: Some(inf.clone()),
                        output: Some(sibling.clone()),
                        message: format!(
                            "Container mismatch: unexpected leftover {sibling} (expected {outp})"
                        ),
                    });
                }
                pairs.push((inf.clone(), outp, info.clone(), slot));
            } else if sibling != outp && output_set.contains(&sibling) {
                report.issues.push(CheckIssue {
                    input: Some(inf.clone()),
                    output: Some(sibling.clone()),
                    message: format!("Container mismatch: found {sibling}, expected {outp}"),
                });
            } else {
                let kind = slot.kind_label(has_alpha);
                report.issues.push(CheckIssue {
                    input: Some(inf.clone()),
                    output: Some(outp.clone()),
                    message: format!("Missing from output (expected {outp}, {kind})"),
                });
            }
        }
    }

    for outf in &output_files {
        // Strip testN_ so multi-slot outputs still pair to input stems.
        let stem = pair_stem(outf);
        if !input_stems.contains(&stem) && !is_chunk_filename(outf) {
            report.issues.push(CheckIssue {
                input: None,
                output: Some(outf.clone()),
                message: "Missing from input".into(),
            });
        }
    }

    for (inf, outp, info, slot) in pairs {
        let out_path = output_dir.join(&outp);
        match file_info(&out_path) {
            Ok(out_info) => match check_pair_inner(&out_path, &info, &out_info, slot) {
                None => {
                    report.ok_pairs += 1;
                }
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

/// Output exists on disk with no matching input stem (`Missing from input`).
pub fn is_orphan_output_issue(issue: &CheckIssue) -> bool {
    issue.input.is_none() && issue.output.is_some()
}

/// Target codec differs from current prefs (file is intact; not a delete candidate).
pub fn is_codec_mismatch_issue(issue: &CheckIssue) -> bool {
    issue.message.starts_with("Codec mismatch:")
}

/// How serious a Check finding is (drives result-window order / color / purge defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CheckSeverity {
    /// Broken / incomplete encode — purge candidates when an output exists.
    High = 0,
    /// Expected output missing (nothing on disk to purge).
    Medium = 1,
    /// Informational (wrong target codec, orphan output, AV1 probe quirk).
    Low = 2,
}

impl CheckSeverity {
    pub fn label(self) -> &'static str {
        match self {
            Self::High => "High",
            Self::Medium => "Medium",
            Self::Low => "Low",
        }
    }
}

/// Classify a Check issue for the results UI.
pub fn check_issue_severity(issue: &CheckIssue) -> CheckSeverity {
    if is_codec_mismatch_issue(issue)
        || is_orphan_output_issue(issue)
        || issue.message.starts_with("AV1 output unreadable")
    {
        CheckSeverity::Low
    } else if issue.message.starts_with("Missing from output") {
        CheckSeverity::Medium
    } else {
        CheckSeverity::High
    }
}

/// Short group title for the Check results tree.
pub fn check_issue_category(issue: &CheckIssue) -> &'static str {
    let m = issue.message.as_str();
    if m.starts_with("Codec mismatch") {
        "Codec mismatch"
    } else if m.starts_with("Missing from input") {
        "Outputs without inputs"
    } else if m.starts_with("Missing from output") {
        "Missing from output"
    } else if m.starts_with("Container mismatch") {
        "Container mismatch"
    } else if m.starts_with("Alpha mismatch") {
        "Alpha mismatch"
    } else if m.starts_with("Audio") {
        "Audio mismatch"
    } else if m.starts_with("Subtitle") {
        "Subtitle mismatch"
    } else if m.starts_with("Frame count") {
        "Frame count mismatch"
    } else if m.starts_with("Input probe") {
        "Input probe failed"
    } else if m.starts_with("Output file error") {
        "Output file error"
    } else if m.starts_with("Incomplete") || m.starts_with("AV1 output unreadable") {
        "Incomplete / unreadable"
    } else if m.starts_with("Duration mismatch") {
        "Duration mismatch"
    } else if m.starts_with("Resolution mismatch") {
        "Resolution mismatch"
    } else if m.starts_with("Missing video") {
        "Missing video stream"
    } else if m.contains("duration") || m.contains("Duration") {
        "Duration"
    } else {
        "Other"
    }
}

/// Output basenames that must not be auto-VMAF’d (High/Medium integrity failures).
/// Low severity (codec mismatch, orphans, AV1 quirk) does not block scoring.
pub fn integrity_blocked_outputs(issues: &[CheckIssue]) -> HashSet<String> {
    let mut blocked = HashSet::new();
    for issue in issues {
        if check_issue_severity(issue) == CheckSeverity::Low {
            continue;
        }
        if let Some(name) = &issue.output {
            blocked.insert(name.clone());
        }
    }
    blocked
}

/// True when this issue points at an on-disk output that delete would remove.
/// Skips “Missing from output” and input-only probe failures; codec mismatch / orphans are deletable when selected.
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
/// When `ignore_orphans`, skips outputs that have no matching input stem.
pub fn deletable_issues<'a>(
    output_dir: &Path,
    issues: &'a [CheckIssue],
    ignore_orphans: bool,
) -> Vec<&'a CheckIssue> {
    issues
        .iter()
        .filter(|i| issue_has_deletable_output(output_dir, i))
        .filter(|i| !(ignore_orphans && is_orphan_output_issue(i)))
        .collect()
}

/// Move a path to the OS Recycle Bin / Trash. Errors if missing or trash fails.
pub fn trash_file(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err("not present on disk".into());
    }
    trash::delete(path).map_err(|e| format!("{e}"))
}

/// Permanently remove a file (no Recycle Bin). Errors if missing or remove fails.
pub fn permanent_delete_file(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err("not present on disk".into());
    }
    std::fs::remove_file(path).map_err(|e| format!("{e}"))
}

/// Normalize for Recycle Bin path compares (slash + `\\?\` + case on Windows).
fn norm_trash_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    #[cfg(windows)]
    {
        s.replace('/', "\\").to_lowercase()
    }
    #[cfg(not(windows))]
    {
        s.replace('\\', "/").to_string()
    }
}

/// Restore a previously recycled file to `original` (Windows / Linux Trash only).
pub fn restore_trashed_file(original: &Path) -> Result<(), String> {
    #[cfg(any(
        windows,
        all(
            unix,
            not(target_os = "macos"),
            not(target_os = "ios"),
            not(target_os = "android")
        )
    ))]
    {
        let want = norm_trash_path(original);
        let items = trash::os_limited::list().map_err(|e| format!("{e}"))?;
        let mut matches: Vec<_> = items
            .into_iter()
            .filter(|i| norm_trash_path(&i.original_path()) == want)
            .collect();
        if matches.is_empty() {
            return Err("not in Recycle Bin".into());
        }
        // Prefer the most recently deleted twin if the same path was trashed more than once.
        matches.sort_by_key(|i| i.time_deleted);
        let item = matches.pop().expect("non-empty matches");
        trash::os_limited::restore_all([item]).map_err(|e| format!("{e}"))
    }
    #[cfg(not(any(
        windows,
        all(
            unix,
            not(target_os = "macos"),
            not(target_os = "ios"),
            not(target_os = "android")
        )
    )))]
    {
        let _ = original;
        Err("Recycle Bin restore is not supported on this OS".into())
    }
}

/// Recycle output files listed on issues (skips “Missing from output”).
/// Returns `(ok_count, fail_count, successfully trashed absolute paths, per-file error lines)` for undo / UI.
pub fn delete_issue_outputs(
    output_dir: &Path,
    issues: &[CheckIssue],
) -> (usize, usize, Vec<PathBuf>) {
    let mut deleted = 0usize;
    let mut failed = 0usize;
    let mut trashed = Vec::new();
    for issue in issues {
        if !issue_has_deletable_output(output_dir, issue) {
            continue;
        }
        let name = issue.output.as_ref().expect("deletable implies output");
        let path = output_dir.join(name);
        let recorded = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        match trash_file(&path) {
            Ok(()) => {
                deleted += 1;
                trashed.push(recorded);
            }
            Err(_) => failed += 1,
        }
    }
    (deleted, failed, trashed)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn chunk_check_missing_file() {
        let err = check_chunk_file(Path::new("definitely-missing-chunk.webm"), 1.0).unwrap_err();
        assert!(err.contains("chunk missing"), "{err}");
    }

    #[test]
    fn cleanup_removes_known_intermediates() {
        let dir = std::env::temp_dir().join(format!(
            "gigamux_cleanup_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let stem = "clip";
        let ext = "mp4";
        fs::write(dir.join("clip.c000of002.mp4"), b"x").unwrap();
        fs::write(dir.join("clip.c001of002.mp4"), b"x").unwrap();
        fs::write(dir.join("clip.ffconcat"), b"x").unwrap();
        fs::write(dir.join("clip.vcat.mp4"), b"x").unwrap();
        fs::write(dir.join("clip.vmux.mkv"), b"x").unwrap();
        fs::write(dir.join("clip.mp4"), b"keep").unwrap();
        cleanup_farm_intermediates(&dir, stem, ext, 2).unwrap();
        assert!(!dir.join("clip.c000of002.mp4").exists());
        assert!(!dir.join("clip.ffconcat").exists());
        assert!(!dir.join("clip.vcat.mp4").exists());
        assert!(!dir.join("clip.vmux.mkv").exists());
        assert!(dir.join("clip.mp4").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn chunk_filename_accepts_mkv() {
        assert!(is_chunk_filename("a.c000of003.mkv"));
        assert!(is_chunk_filename("a.c000of003.webm"));
        assert!(!is_chunk_filename("a.mp4"));
    }

    #[test]
    fn argv_force_overwrite_swaps_n_for_y() {
        let v = argv_force_overwrite(&[
            "ffmpeg".into(),
            "-n".into(),
            "-i".into(),
            "in.mp4".into(),
        ]);
        assert_eq!(v[1], "-y");
        let already = argv_force_overwrite(&["ffmpeg".into(), "-y".into(), "out".into()]);
        assert_eq!(already[1], "-y");
    }
}
