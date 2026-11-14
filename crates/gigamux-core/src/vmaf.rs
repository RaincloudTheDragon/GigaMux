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
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::check::{check_chunk_file, is_chunk_filename, list_media};
use crate::chunk::{
    chunk_deliver_stem, chunk_frame_range, estimate_total_frames, parse_chunk_filename, parse_fps,
    CHUNK_SEEK_PAD_S,
};
use crate::job_log::{JobLog, JobLogKind};
use crate::probe::{file_info, video_codec_name, video_has_alpha};
use crate::targets::{OutputTargets, TestBatch};

/// Optional reference trim for chunk VMAF (distorted is already the chunk, PTS0).
#[derive(Debug, Clone, Copy)]
pub struct VmafRefTrim {
    pub start_frame: u64,
    pub frame_count: u64,
    pub fps: f64,
}

/// Symmetric trim for farm temporal VMAF parts (same frame range on reference and distorted).
#[derive(Debug, Clone, Copy)]
pub struct VmafBothTrim {
    pub start_frame: u64,
    pub frame_count: u64,
    pub fps: f64,
}

impl VmafBothTrim {
    fn as_ref_trim(self) -> VmafRefTrim {
        VmafRefTrim {
            start_frame: self.start_frame,
            frame_count: self.frame_count,
            fps: self.fps,
        }
    }
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
    libvmaf_available_exe("ffmpeg")
}

/// True when `ffmpeg_exe` lists a `libvmaf` filter (`-filters`).
pub fn libvmaf_available_exe(ffmpeg_exe: &str) -> bool {
    let exe = {
        let t = ffmpeg_exe.trim();
        if t.is_empty() {
            "ffmpeg"
        } else {
            t
        }
    };
    let Ok(out) = Command::new(exe)
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

/// Prefer the requested exe when it has libvmaf; else Manager `tools/` siblings of this helper.
///
/// Linux PATH ffmpeg often lacks libvmaf while Deploy ships `ffmpeg-linux-amd64` next to
/// `gigamux-check` — farm VMAF must use that build.
pub fn resolve_ffmpeg_for_vmaf(ffmpeg_exe: &str) -> Result<String, String> {
    let requested = {
        let t = ffmpeg_exe.trim();
        if t.is_empty() {
            "ffmpeg".to_string()
        } else {
            t.to_string()
        }
    };
    if libvmaf_available_exe(&requested) {
        return Ok(requested);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            // Deploy layout: Manager tools/ next to gigamux-check.
            #[cfg(windows)]
            {
                candidates.push(dir.join("ffmpeg-windows-amd64.exe"));
                candidates.push(dir.join("ffmpeg.exe"));
            }
            #[cfg(not(windows))]
            {
                candidates.push(dir.join("ffmpeg-linux-amd64"));
                candidates.push(dir.join("ffmpeg"));
            }
        }
    }
    for c in &candidates {
        if !c.is_file() {
            continue;
        }
        let s = c.to_string_lossy().into_owned();
        if libvmaf_available_exe(&s) {
            println!(
                "vmaf: {requested} has no libvmaf; using {} instead",
                c.display()
            );
            return Ok(s);
        }
    }
    Err(format!(
        "ffmpeg has no libvmaf filter (tried {requested}{}); install a full build or place ffmpeg-linux-amd64 / ffmpeg-windows-amd64.exe next to gigamux-check",
        if candidates.is_empty() {
            String::new()
        } else {
            format!(
                " + siblings {}",
                candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    ))
}

/// On-disk farm/local VMAF score next to a deliver (`clip.mp4.vmaf.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmafSidecar {
    pub version: u32,
    pub score: f64,
    pub input: String,
    pub output: String,
}

/// Sidecar path: `{output_path}.vmaf.json`.
pub fn vmaf_sidecar_path(output: &Path) -> PathBuf {
    let mut s = output.as_os_str().to_os_string();
    s.push(".vmaf.json");
    PathBuf::from(s)
}

/// Temporal part sidecar: `{output_path}.vmaf.pIIIofNNN.json`.
pub fn vmaf_part_sidecar_path(output: &Path, part: u32, parts: u32) -> PathBuf {
    let mut s = output.as_os_str().to_os_string();
    s.push(format!(".vmaf.p{part:03}of{parts:03}.json"));
    PathBuf::from(s)
}

/// One farm temporal slice’s score (merged into the final deliver sidecar).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmafPartSidecar {
    pub version: u32,
    pub score: f64,
    pub frames: u64,
    pub input: String,
    pub output: String,
    pub part: u32,
    pub parts: u32,
}

/// Parse `….vmaf.pIIIofNNN.json` → `(part, parts)`.
pub fn parse_vmaf_part_sidecar_name(path: &Path) -> Option<(u32, u32)> {
    let name = path.file_name()?.to_str()?;
    // Stem may be `clip.mp4.vmaf.p000of004` — match trailing `.vmaf.pIIIofNNN.json`.
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\.vmaf\.p(\d+)of(\d+)\.json$").expect("vmaf part re")
    });
    let caps = re.captures(name)?;
    let part: u32 = caps.get(1)?.as_str().parse().ok()?;
    let parts: u32 = caps.get(2)?.as_str().parse().ok()?;
    if parts == 0 || part >= parts {
        return None;
    }
    Some((part, parts))
}

/// Read a finite-score part sidecar, or `None` if missing/invalid.
pub fn read_vmaf_part_sidecar(path: &Path) -> Option<VmafPartSidecar> {
    let raw = std::fs::read_to_string(path).ok()?;
    let sc: VmafPartSidecar = serde_json::from_str(&raw).ok()?;
    if sc.version == 0 || !sc.score.is_finite() || sc.frames == 0 || sc.parts == 0 {
        return None;
    }
    Some(sc)
}

/// Atomic write of a VMAF part sidecar (temp + rename).
pub fn write_vmaf_part_sidecar(path: &Path, sc: &VmafPartSidecar) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("vmaf part sidecar mkdir: {e}"))?;
    }
    let tmp = {
        let mut s = path.as_os_str().to_os_string();
        s.push(".tmp");
        PathBuf::from(s)
    };
    let body =
        serde_json::to_string_pretty(sc).map_err(|e| format!("vmaf part sidecar json: {e}"))?;
    std::fs::write(&tmp, body).map_err(|e| format!("vmaf part sidecar write: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("vmaf part sidecar rename: {e}")
    })
}

/// Read a finite-score sidecar, or `None` if missing/invalid.
pub fn read_vmaf_sidecar(path: &Path) -> Option<VmafSidecar> {
    let raw = std::fs::read_to_string(path).ok()?;
    let sc: VmafSidecar = serde_json::from_str(&raw).ok()?;
    if sc.version == 0 || !sc.score.is_finite() {
        return None;
    }
    Some(sc)
}

/// Atomic write of a VMAF sidecar (temp + rename).
pub fn write_vmaf_sidecar(path: &Path, sc: &VmafSidecar) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("vmaf sidecar mkdir: {e}"))?;
    }
    // Append `.tmp` — do not use `with_extension` (would clobber `.vmaf.json`).
    let tmp = {
        let mut s = path.as_os_str().to_os_string();
        s.push(".tmp");
        PathBuf::from(s)
    };
    let body = serde_json::to_string_pretty(sc).map_err(|e| format!("vmaf sidecar json: {e}"))?;
    std::fs::write(&tmp, body).map_err(|e| format!("vmaf sidecar write: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("vmaf sidecar rename: {e}")
    })
}

/// Farm/worker path: skip if sidecar has a score, else run libvmaf and write sidecar.
/// `ffmpeg_exe` empty → `"ffmpeg"`. Prints `VMAF score: …` / skip note on stdout for task logs.
pub fn ensure_vmaf(
    reference: &Path,
    distorted: &Path,
    sidecar: &Path,
    ffmpeg_exe: &str,
) -> Result<f64, String> {
    if let Some(sc) = read_vmaf_sidecar(sidecar) {
        println!(
            "ensure-vmaf: reuse sidecar {} (score {:.6})",
            sidecar.display(),
            sc.score
        );
        return Ok(sc.score);
    }
    if !reference.is_file() {
        return Err(format!("ensure-vmaf: reference missing: {}", reference.display()));
    }
    if !distorted.is_file() {
        return Err(format!("ensure-vmaf: distorted missing: {}", distorted.display()));
    }
    let exe = resolve_ffmpeg_for_vmaf(ffmpeg_exe)?;
    let score = run_vmaf_pair_exe(&exe, reference, distorted, None)?;
    let in_name = reference
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| reference.display().to_string());
    let out_name = distorted
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| distorted.display().to_string());
    write_vmaf_sidecar(
        sidecar,
        &VmafSidecar {
            version: 1,
            score,
            input: in_name,
            output: out_name,
        },
    )?;
    println!("ensure-vmaf: wrote {} (score {:.6})", sidecar.display(), score);
    Ok(score)
}

/// Farm temporal part: skip if part sidecar exists; else score the shared frame range on both inputs.
pub fn ensure_vmaf_part(
    reference: &Path,
    distorted: &Path,
    part_sidecar: &Path,
    start_frame: u64,
    frame_count: u64,
    fps: f64,
    ffmpeg_exe: &str,
) -> Result<f64, String> {
    if let Some(sc) = read_vmaf_part_sidecar(part_sidecar) {
        println!(
            "ensure-vmaf-part: reuse {} (score {:.6} frames={})",
            part_sidecar.display(),
            sc.score,
            sc.frames
        );
        return Ok(sc.score);
    }
    if frame_count == 0 {
        return Err("ensure-vmaf-part: frame_count must be > 0".into());
    }
    if !(fps.is_finite() && fps > 0.0) {
        return Err(format!("ensure-vmaf-part: bad fps {fps}"));
    }
    let (part, parts) = parse_vmaf_part_sidecar_name(part_sidecar).ok_or_else(|| {
        format!(
            "ensure-vmaf-part: sidecar name must end with .vmaf.pIIIofNNN.json: {}",
            part_sidecar.display()
        )
    })?;
    if !reference.is_file() {
        return Err(format!(
            "ensure-vmaf-part: reference missing: {}",
            reference.display()
        ));
    }
    if !distorted.is_file() {
        return Err(format!(
            "ensure-vmaf-part: distorted missing: {}",
            distorted.display()
        ));
    }
    let exe = resolve_ffmpeg_for_vmaf(ffmpeg_exe)?;
    let trim = VmafBothTrim {
        start_frame,
        frame_count,
        fps,
    };
    let score = run_vmaf_pair_exe(&exe, reference, distorted, Some(&trim))?;
    let in_name = reference
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| reference.display().to_string());
    let out_name = distorted
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| distorted.display().to_string());
    write_vmaf_part_sidecar(
        part_sidecar,
        &VmafPartSidecar {
            version: 1,
            score,
            frames: frame_count,
            input: in_name,
            output: out_name,
            part,
            parts,
        },
    )?;
    println!(
        "ensure-vmaf-part: wrote {} (score {:.6} frames={frame_count} part {part}/{parts})",
        part_sidecar.display(),
        score
    );
    Ok(score)
}

/// Frame-weighted mean of part scores → final `{deliver}.vmaf.json`; deletes part sidecars on success.
pub fn merge_vmaf_parts(final_sidecar: &Path, part_sidecars: &[PathBuf]) -> Result<f64, String> {
    if part_sidecars.is_empty() {
        return Err("merge-vmaf: no part sidecars".into());
    }
    let mut weighted = 0.0_f64;
    let mut total_frames = 0_u64;
    let mut input = String::new();
    let mut output = String::new();
    for path in part_sidecars {
        let sc = read_vmaf_part_sidecar(path).ok_or_else(|| {
            format!("merge-vmaf: missing/invalid part sidecar: {}", path.display())
        })?;
        weighted += sc.score * sc.frames as f64;
        total_frames = total_frames.saturating_add(sc.frames);
        if input.is_empty() {
            input = sc.input;
        }
        if output.is_empty() {
            output = sc.output;
        }
    }
    if total_frames == 0 {
        return Err("merge-vmaf: total frames is 0".into());
    }
    let score = weighted / total_frames as f64;
    write_vmaf_sidecar(
        final_sidecar,
        &VmafSidecar {
            version: 1,
            score,
            input,
            output,
        },
    )?;
    for path in part_sidecars {
        let _ = std::fs::remove_file(path);
    }
    println!(
        "merge-vmaf: wrote {} (score {:.6} from {} parts, {total_frames} frames)",
        final_sidecar.display(),
        score,
        part_sidecars.len()
    );
    Ok(score)
}

/// How RGB relates to alpha when flattening onto black for VMAF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AlphaAssociation {
    /// RGB is independent of A — must `premultiply` before dropping alpha.
    Straight,
    /// RGB is already ×A (or zero under transparent) — dropping A is the black composite.
    Premultiplied,
}

/// VP8/VP9 with alpha need libvpx; native decode drops the A plane and poisons VMAF.
fn vmaf_alpha_decoder_prefix(info: &Value) -> Vec<String> {
    if !video_has_alpha(info) {
        return Vec::new();
    }
    match video_codec_name(info).as_str() {
        "vp9" => vec!["-c:v".into(), "libvpx-vp9".into()],
        "vp8" => vec!["-c:v".into(), "libvpx".into()],
        _ => Vec::new(),
    }
}

/// Mean `signalstats` YAVG over a short head of the file (association probe).
fn mean_yavg_sample(ffmpeg_exe: &str, path: &Path, info: &Value, vf: &str) -> Result<f64, String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-nostats".into(),
        "-t".into(),
        "0.5".into(),
    ];
    args.extend(vmaf_alpha_decoder_prefix(info));
    args.extend([
        "-i".into(),
        path.to_string_lossy().into_owned(),
        "-vf".into(),
        format!("{vf},signalstats,metadata=print:key=lavfi.signalstats.YAVG"),
        "-f".into(),
        "null".into(),
        "-".into(),
    ]);
    let out = Command::new(ffmpeg_exe)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("alpha assoc probe spawn: {e}"))?;
    let err = String::from_utf8_lossy(&out.stderr);
    let mut vals = Vec::new();
    for line in err.lines() {
        if let Some(i) = line.find("YAVG=") {
            let rest = &line[i + 5..];
            let num: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == 'e' || *c == 'E' || *c == '+')
                .collect();
            if let Ok(v) = num.parse::<f64>() {
                vals.push(v);
            }
        }
    }
    if vals.is_empty() {
        return Err(format!(
            "alpha assoc probe: no YAVG from {} ({})",
            path.display(),
            if out.status.success() {
                "empty"
            } else {
                "ffmpeg failed"
            }
        ));
    }
    Ok(vals.iter().sum::<f64>() / vals.len() as f64)
}

/// Detect straight vs premultiplied by whether `premultiply` removes substantial luminance.
/// OBS α WebMs are often straight (bright RGB under low A); hybrid delivers are often already premul-like.
fn probe_alpha_association(
    ffmpeg_exe: &str,
    path: &Path,
    info: &Value,
) -> Result<AlphaAssociation, String> {
    if !video_has_alpha(info) {
        return Ok(AlphaAssociation::Premultiplied);
    }
    let raw = mean_yavg_sample(ffmpeg_exe, path, info, "format=yuv420p")?;
    let premul = mean_yavg_sample(
        ffmpeg_exe,
        path,
        info,
        "format=yuva420p,premultiply=inplace=1,format=yuv420p",
    )?;
    // Premultiply killing ≥15% of mean Y ⇒ RGB lived under low alpha ⇒ straight.
    if raw > 1.0 && premul < raw * 0.85 {
        Ok(AlphaAssociation::Straight)
    } else {
        Ok(AlphaAssociation::Premultiplied)
    }
}

/// Flatten one α plane onto black for libvmaf — premul only when that side is straight.
fn vmaf_plane_prep(stream: &str, trim: Option<&VmafRefTrim>, assoc: AlphaAssociation) -> String {
    let body = match assoc {
        AlphaAssociation::Straight => {
            "format=yuva420p,premultiply=inplace=1,format=yuv420p,setpts=PTS-STARTPTS"
        }
        // Already ×A (or opaque): drop A; do not premul again (would double-darken soft edges).
        AlphaAssociation::Premultiplied => "format=yuva420p,format=yuv420p,setpts=PTS-STARTPTS",
    };
    if let Some(tr) = trim {
        let (_, rel_start, rel_end) = trim_seek_and_rel(tr);
        format!("{stream}trim=start_frame={rel_start}:end_frame={rel_end},{body}")
    } else {
        format!("{stream}{body}")
    }
}

/// Pad seek + relative trim window for a VmafRefTrim (same math as encode ChunkTrim).
fn trim_seek_and_rel(tr: &VmafRefTrim) -> (Option<f64>, u64, u64) {
    let pad_frames = ((tr.fps * CHUNK_SEEK_PAD_S).round() as u64).max(1);
    let pre_frame = tr.start_frame.saturating_sub(pad_frames);
    let seek_s = if pre_frame > 0 {
        Some(pre_frame as f64 / tr.fps)
    } else {
        None
    };
    let rel_start = tr.start_frame - pre_frame;
    let rel_end = rel_start + tr.frame_count;
    (seek_s, rel_start, rel_end)
}

/// Opaque filter leg: optional frame trim + PTS0.
fn opaque_vmaf_leg(stream: &str, trim: Option<&VmafRefTrim>, label: &str) -> String {
    if let Some(tr) = trim {
        let (_, rel_start, rel_end) = trim_seek_and_rel(tr);
        format!(
            "{stream}trim=start_frame={rel_start}:end_frame={rel_end},setpts=PTS-STARTPTS[{label}]"
        )
    } else {
        format!("{stream}setpts=PTS-STARTPTS[{label}]")
    }
}

/// Opaque path: PTS align only. Alpha path: per-side association flatten onto black before libvmaf.
/// `dist_trim` is set for farm temporal parts (symmetric); encode leftover chunks leave it `None`.
fn vmaf_filter_complex(
    threads: &str,
    alpha: bool,
    ref_trim: Option<&VmafRefTrim>,
    dist_trim: Option<&VmafRefTrim>,
    ref_assoc: AlphaAssociation,
    dist_assoc: AlphaAssociation,
) -> String {
    if alpha {
        let ref_prep = vmaf_plane_prep("[0:v]", ref_trim, ref_assoc);
        let dist_prep = vmaf_plane_prep("[1:v]", dist_trim, dist_assoc);
        format!("{ref_prep}[ref];{dist_prep}[dist];[ref][dist]libvmaf=n_threads={threads}")
    } else {
        let ref_leg = opaque_vmaf_leg("[0:v]", ref_trim, "ref");
        let dist_leg = opaque_vmaf_leg("[1:v]", dist_trim, "dist");
        format!("{ref_leg};{dist_leg};[ref][dist]libvmaf=n_threads={threads}")
    }
}

/// Blocking libvmaf with a specific ffmpeg binary (no cancel / UI progress).
/// `both_trim` applies the same frame range to reference and distorted (farm temporal parts).
fn run_vmaf_pair_exe(
    ffmpeg_exe: &str,
    reference: &Path,
    distorted: &Path,
    both_trim: Option<&VmafBothTrim>,
) -> Result<f64, String> {
    let ref_info = file_info(reference)?;
    let dist_info = file_info(distorted)?;
    let alpha = video_has_alpha(&ref_info) || video_has_alpha(&dist_info);
    let threads = n_threads().to_string();
    let (ref_assoc, dist_assoc) = if alpha {
        (
            probe_alpha_association(ffmpeg_exe, reference, &ref_info)?,
            probe_alpha_association(ffmpeg_exe, distorted, &dist_info)?,
        )
    } else {
        (
            AlphaAssociation::Premultiplied,
            AlphaAssociation::Premultiplied,
        )
    };
    let side_trim = both_trim.map(|t| t.as_ref_trim());
    let filter = vmaf_filter_complex(
        &threads,
        alpha,
        side_trim.as_ref(),
        side_trim.as_ref(),
        ref_assoc,
        dist_assoc,
    );
    let (seek_s, frames_v) = if let Some(tr) = side_trim.as_ref() {
        let (ss, _, _) = trim_seek_and_rel(tr);
        (ss, Some(tr.frame_count))
    } else {
        (None, None)
    };
    let mut args: Vec<String> = vec!["-hide_banner".into(), "-nostats".into()];
    // Symmetric -ss before each input when temporal-chunking both sides.
    if let Some(ss) = seek_s {
        args.extend(["-ss".into(), format!("{ss:.6}")]);
    }
    args.extend(vmaf_alpha_decoder_prefix(&ref_info));
    args.extend(["-i".into(), reference.to_string_lossy().into_owned()]);
    if let Some(ss) = seek_s {
        args.extend(["-ss".into(), format!("{ss:.6}")]);
    }
    args.extend(vmaf_alpha_decoder_prefix(&dist_info));
    args.extend([
        "-i".into(),
        distorted.to_string_lossy().into_owned(),
        "-filter_complex".into(),
        filter,
    ]);
    if let Some(n) = frames_v {
        args.extend(["-frames:v".into(), n.to_string()]);
    }
    args.extend(["-f".into(), "null".into(), "-".into()]);
    let out = Command::new(ffmpeg_exe)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("ffmpeg spawn: {e}"))?;
    let err = String::from_utf8_lossy(&out.stderr);
    let mut score = None;
    let mut tail: VecDeque<String> = VecDeque::new();
    for line in err.lines() {
        push_stderr_tail(&mut tail, line);
        if let Some(s) = parse_vmaf_score(line) {
            score = Some(s);
        }
    }
    if let Some(s) = score {
        return Ok(s);
    }
    if !out.status.success() {
        return Err(format_ffmpeg_fail(&out.status, &tail));
    }
    Err(if tail.is_empty() {
        "VMAF score not found in ffmpeg output (is libvmaf enabled?)".into()
    } else {
        format!(
            "VMAF score not found in ffmpeg output (is libvmaf enabled?): {}",
            tail.iter().cloned().collect::<Vec<_>>().join(" | ")
        )
    })
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
    let ref_info = file_info(reference)?;
    let dist_info = file_info(distorted)?;
    let alpha = video_has_alpha(&ref_info) || video_has_alpha(&dist_info);
    let threads = n_threads().to_string();
    // Chunk trim: same pad + frame trim as encode `ChunkTrim` (reference only; distorted already PTS0).
    let (seek_s, frames_v) = if let Some(tr) = ref_trim {
        let pad_frames = ((tr.fps * CHUNK_SEEK_PAD_S).round() as u64).max(1);
        let pre_frame = tr.start_frame.saturating_sub(pad_frames);
        let seek_s = if pre_frame > 0 {
            Some(pre_frame as f64 / tr.fps)
        } else {
            None
        };
        (seek_s, Some(tr.frame_count))
    } else {
        (None, None)
    };
    // Probe each α side so OBS straight vs hybrid premul both flatten to the same black composite.
    let (ref_assoc, dist_assoc) = if alpha {
        on_update(
            Some("VMAF: probing alpha association (straight vs premultiplied)…".into()),
            None,
        );
        (
            probe_alpha_association("ffmpeg", reference, &ref_info)?,
            probe_alpha_association("ffmpeg", distorted, &dist_info)?,
        )
    } else {
        (
            AlphaAssociation::Premultiplied,
            AlphaAssociation::Premultiplied,
        )
    };
    if alpha {
        on_update(
            Some(format!(
                "VMAF: alpha assoc ref={ref_assoc:?} dist={dist_assoc:?}"
            )),
            None,
        );
    }
    // Encode leftover chunks: trim reference only (distorted is already the short clip).
    let filter = vmaf_filter_complex(&threads, alpha, ref_trim, None, ref_assoc, dist_assoc);

    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-stats".into(),
        "-stats_period".into(),
        "0.5".into(),
    ];
    if let Some(ss) = seek_s {
        args.extend(["-ss".into(), format!("{ss:.6}")]);
    }
    args.extend(vmaf_alpha_decoder_prefix(&ref_info));
    args.extend(["-i".into(), reference.to_string_lossy().into_owned()]);
    args.extend(vmaf_alpha_decoder_prefix(&dist_info));
    args.extend([
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
        // Hybrid `{stem}.color.cNNN…` pairs to the deliver stem for ref trim / ownership.
        let deliver = chunk_deliver_stem(&stem).to_string();
        if let Some(only) = only_chunk {
            if only != out_name.as_str() {
                continue;
            }
        } else if only_basename.is_some() {
            // Input/final selection: metas are already filtered; require deliver-stem ownership.
            if !metas.iter().any(|m| m.deliver_stems.contains(&deliver)) {
                continue;
            }
        }
        if let Some(allow) = only_outputs {
            if !allow.contains(out_name) {
                continue;
            }
        }
        let Some(meta) = metas.iter().find(|m| m.deliver_stems.contains(&deliver)) else {
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
            deliver,
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
/// When `force`, ignore existing `{out}.vmaf.json` and re-run libvmaf (overwrite sidecar).
pub fn run_vmaf(
    input_dir: &Path,
    output_dir: &Path,
    slots: &[(Option<TestBatch>, OutputTargets)],
    only_basename: Option<String>,
    only_outputs: Option<HashSet<String>>,
    force: bool,
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
        // Durable skip: same `{out}.vmaf.json` as farm `ensure-vmaf` (no re-libvmaf) unless `force`.
        let sidecar = vmaf_sidecar_path(out_path);
        if !force {
            if let Some(sc) = read_vmaf_sidecar(&sidecar) {
                ok += 1;
                on_event(VmafEvent::Log(format!(
                    "VMAF {}/{}: {in_name} → {out_name} — skip (sidecar {:.4})",
                    i + 1,
                    total_files,
                    sc.score
                )));
                on_event(VmafEvent::FileDone {
                    name: in_name.clone(),
                    output: out_name.clone(),
                    score: sc.score,
                });
                report.results.push(VmafResult {
                    input: in_name.clone(),
                    output: out_name.clone(),
                    score: Some(sc.score),
                    error: None,
                });
                continue;
            }
        } else if sidecar.is_file() {
            on_event(VmafEvent::Log(format!(
                "VMAF {}/{}: {in_name} → {out_name}: re-score (ignoring sidecar)",
                i + 1,
                total_files
            )));
        }
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
                // Persist so local + farm agree on "already scored".
                if let Err(e) = write_vmaf_sidecar(
                    &sidecar,
                    &VmafSidecar {
                        version: 1,
                        score,
                        input: in_name.clone(),
                        output: out_name.clone(),
                    },
                ) {
                    on_event(VmafEvent::Log(format!(
                        "VMAF {in_name}: scored {score:.4} but sidecar write failed: {e}"
                    )));
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_path_and_round_trip() {
        let out = PathBuf::from("clip.mp4");
        let side = vmaf_sidecar_path(&out);
        assert_eq!(side, PathBuf::from("clip.mp4.vmaf.json"));
        let dir = std::env::temp_dir().join(format!("gigamux_vmaf_side_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.mp4.vmaf.json");
        write_vmaf_sidecar(
            &path,
            &VmafSidecar {
                version: 1,
                score: 97.5,
                input: "a.mkv".into(),
                output: "a.mp4".into(),
            },
        )
        .unwrap();
        let sc = read_vmaf_sidecar(&path).expect("read");
        assert!((sc.score - 97.5).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn alpha_vmaf_filter_premultiplies_straight_only() {
        // Straight → premul flatten; already-premul → drop A only (no second premul).
        let both_straight = vmaf_filter_complex(
            "4",
            true,
            None,
            AlphaAssociation::Straight,
            AlphaAssociation::Straight,
        );
        assert!(both_straight.contains("premultiply=inplace=1"), "{both_straight}");
        let mixed = vmaf_filter_complex(
            "4",
            true,
            None,
            AlphaAssociation::Straight,
            AlphaAssociation::Premultiplied,
        );
        // Ref (straight) premuls; dist (premul) only drops A.
        assert!(mixed.starts_with("[0:v]format=yuva420p,premultiply=inplace=1"), "{mixed}");
        assert!(
            mixed.contains("[1:v]format=yuva420p,format=yuv420p,setpts=PTS-STARTPTS[dist]"),
            "{mixed}"
        );
        assert!(mixed.contains("libvmaf=n_threads=4"), "{mixed}");
        let opaque = vmaf_filter_complex(
            "4",
            false,
            None,
            AlphaAssociation::Premultiplied,
            AlphaAssociation::Premultiplied,
        );
        assert!(!opaque.contains("premultiply"), "{opaque}");
    }

    #[test]
    fn vp9_alpha_uses_libvpx_decoder_prefix() {
        let info = serde_json::json!({
            "streams": [{
                "codec_type": "video",
                "codec_name": "vp9",
                "pix_fmt": "yuva420p",
            }]
        });
        let prefix = vmaf_alpha_decoder_prefix(&info);
        assert_eq!(prefix, vec!["-c:v".to_string(), "libvpx-vp9".to_string()]);
    }
}
