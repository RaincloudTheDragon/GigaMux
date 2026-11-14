//! Flamenco worker helper for GigaMux hybrid integrity.
//!
//! Commands (exit 0 = ok, non-zero = fail task):
//! - `gigamux-check ensure-chunk <path> <expect_dur> <ffmpeg_exe> -- <ffmpeg argv…>` (argv includes leading program name; do **not** nest paths in JSON: Flamenco two-way expands `{var}` to `W:\…` and breaks JSON escapes)
//! - `gigamux-check ensure-out <path> <expect_dur> <ffmpeg_exe> -- <ffmpeg argv…>` (same for mux stage vmux / remux final; rebuilds incomplete leftovers)
//! - `gigamux-check ensure-vmaf <ref> <dist> <sidecar> [ffmpeg_exe]` (reuse sidecar score; else libvmaf + write)
//! - `gigamux-check ensure-vmaf-part <ref> <dist> <part_sidecar> <start_frame> <frame_count> <fps> [ffmpeg_exe]` (temporal slice; sidecar name `*.vmaf.pIIIofNNN.json`)
//! - `gigamux-check merge-vmaf <final_sidecar> <part_sidecar…>` (frame-weighted mean → final; deletes parts)
//! - `gigamux-check chunk <path> <expect_dur_secs>`
//! - `gigamux-check final <input> <output> <opaque_video> <opaque_container> <alpha_video> <alpha_container>`
//! - `gigamux-check cleanup <dir> <stem> <ext> <chunks>`
//! - `gigamux-check mux-alpha-webm <color.ivf> <alpha.ivf> <out.webm> [range] [space] [primaries] [transfer]`
//! - `gigamux-check farm-probe <out_dir>` (write `{host}.json` GPU/QSV/VMAF inventory)

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use gigamux_core::check::{
    check_chunk_file, check_output_pair, cleanup_farm_intermediates, ensure_chunk_file,
    ensure_out_file,
};
use gigamux_core::flamenco::{write_farm_probe_report, FarmFinalCheck};
use gigamux_core::probe::VideoColorMeta;
use gigamux_core::targets::{AlphaVideo, OutContainer, OpaqueVideo};
use gigamux_core::vmaf::{ensure_vmaf, ensure_vmaf_part, merge_vmaf_parts};
use gigamux_core::webm_alpha::mux_vp9_alpha_webm;

fn parse_opaque_video(s: &str) -> Result<OpaqueVideo, String> {
    match s.to_ascii_lowercase().as_str() {
        "hevc" => Ok(OpaqueVideo::Hevc),
        "h264" => Ok(OpaqueVideo::H264),
        "av1" => Ok(OpaqueVideo::Av1),
        "vp9" => Ok(OpaqueVideo::Vp9),
        "vp8" => Ok(OpaqueVideo::Vp8),
        _ => Err(format!("unknown opaque_video {s:?}")),
    }
}

fn parse_alpha_video(s: &str) -> Result<AlphaVideo, String> {
    match s.to_ascii_lowercase().as_str() {
        "vp9" => Ok(AlphaVideo::Vp9),
        "vp8" => Ok(AlphaVideo::Vp8),
        _ => Err(format!("unknown alpha_video {s:?}")),
    }
}

fn parse_container(s: &str) -> Result<OutContainer, String> {
    match s.to_ascii_lowercase().as_str() {
        "mp4" => Ok(OutContainer::Mp4),
        "mkv" => Ok(OutContainer::Mkv),
        "webm" => Ok(OutContainer::Webm),
        _ => Err(format!("unknown container {s:?}")),
    }
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(cmd) = args.next() else {
        eprintln!(
            "usage: gigamux-check ensure-chunk|ensure-out|ensure-vmaf|ensure-vmaf-part|merge-vmaf|chunk|final|cleanup|mux-alpha-webm|farm-probe …"
        );
        return ExitCode::from(2);
    };
    /// Shared `--` / legacy JSON argv parsing for ensure-* commands.
    fn parse_ensure_argv(args: impl Iterator<Item = String>, label: &str) -> Result<Vec<String>, String> {
        let mut argv: Vec<String> = args.collect();
        if argv.first().map(String::as_str) == Some("--") {
            argv.remove(0);
        } else if argv.len() == 1 {
            argv = serde_json::from_str::<Vec<String>>(&argv[0])
                .map_err(|e| format!("{label}: argv JSON: {e}"))?;
        }
        if argv.is_empty() {
            return Err(format!("{label}: missing ffmpeg argv after --"));
        }
        Ok(argv)
    }
    let result = match cmd.as_str() {
        "ensure-chunk" => {
            let path = PathBuf::from(args.next().unwrap_or_default());
            let dur_s = args.next().unwrap_or_default();
            let expect: f64 = match dur_s.parse() {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("ensure-chunk: bad expect_dur {dur_s:?}");
                    return ExitCode::from(2);
                }
            };
            let ffmpeg_exe = args.next().unwrap_or_else(|| "ffmpeg".into());
            match parse_ensure_argv(args, "ensure-chunk") {
                Ok(argv) => ensure_chunk_file(&path, expect, &ffmpeg_exe, &argv),
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::from(2);
                }
            }
        }
        "ensure-out" => {
            let path = PathBuf::from(args.next().unwrap_or_default());
            let dur_s = args.next().unwrap_or_default();
            let expect: f64 = match dur_s.parse() {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("ensure-out: bad expect_dur {dur_s:?}");
                    return ExitCode::from(2);
                }
            };
            let ffmpeg_exe = args.next().unwrap_or_else(|| "ffmpeg".into());
            match parse_ensure_argv(args, "ensure-out") {
                Ok(argv) => ensure_out_file(&path, expect, &ffmpeg_exe, &argv),
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::from(2);
                }
            }
        }
        "ensure-vmaf" => {
            let reference = PathBuf::from(args.next().unwrap_or_default());
            let distorted = PathBuf::from(args.next().unwrap_or_default());
            let sidecar = PathBuf::from(args.next().unwrap_or_default());
            let ffmpeg_exe = args.next().unwrap_or_else(|| "ffmpeg".into());
            if reference.as_os_str().is_empty()
                || distorted.as_os_str().is_empty()
                || sidecar.as_os_str().is_empty()
            {
                eprintln!("ensure-vmaf: need <ref> <dist> <sidecar> [ffmpeg_exe]");
                return ExitCode::from(2);
            }
            match ensure_vmaf(&reference, &distorted, &sidecar, &ffmpeg_exe) {
                Ok(score) => {
                    println!("gigamux-check ensure-vmaf: ok score={score:.6}");
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("gigamux-check ensure-vmaf: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        "ensure-vmaf-part" => {
            let reference = PathBuf::from(args.next().unwrap_or_default());
            let distorted = PathBuf::from(args.next().unwrap_or_default());
            let part_sidecar = PathBuf::from(args.next().unwrap_or_default());
            let start_s = args.next().unwrap_or_default();
            let count_s = args.next().unwrap_or_default();
            let fps_s = args.next().unwrap_or_default();
            let ffmpeg_exe = args.next().unwrap_or_else(|| "ffmpeg".into());
            if reference.as_os_str().is_empty()
                || distorted.as_os_str().is_empty()
                || part_sidecar.as_os_str().is_empty()
                || start_s.is_empty()
                || count_s.is_empty()
                || fps_s.is_empty()
            {
                eprintln!(
                    "ensure-vmaf-part: need <ref> <dist> <part_sidecar> <start_frame> <frame_count> <fps> [ffmpeg_exe]"
                );
                return ExitCode::from(2);
            }
            let start_frame: u64 = match start_s.parse() {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("ensure-vmaf-part: bad start_frame {start_s:?}");
                    return ExitCode::from(2);
                }
            };
            let frame_count: u64 = match count_s.parse() {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("ensure-vmaf-part: bad frame_count {count_s:?}");
                    return ExitCode::from(2);
                }
            };
            let fps: f64 = match fps_s.parse() {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("ensure-vmaf-part: bad fps {fps_s:?}");
                    return ExitCode::from(2);
                }
            };
            match ensure_vmaf_part(
                &reference,
                &distorted,
                &part_sidecar,
                start_frame,
                frame_count,
                fps,
                &ffmpeg_exe,
            ) {
                Ok(score) => {
                    println!("gigamux-check ensure-vmaf-part: ok score={score:.6}");
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("gigamux-check ensure-vmaf-part: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        "merge-vmaf" => {
            let final_sidecar = PathBuf::from(args.next().unwrap_or_default());
            if final_sidecar.as_os_str().is_empty() {
                eprintln!("merge-vmaf: need <final_sidecar> <part_sidecar…>");
                return ExitCode::from(2);
            }
            let mut parts: Vec<PathBuf> = Vec::new();
            for p in args {
                if !p.is_empty() {
                    parts.push(PathBuf::from(p));
                }
            }
            if parts.is_empty() {
                eprintln!("merge-vmaf: need at least one <part_sidecar>");
                return ExitCode::from(2);
            }
            match merge_vmaf_parts(&final_sidecar, &parts) {
                Ok(score) => {
                    println!("gigamux-check merge-vmaf: ok score={score:.6}");
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("gigamux-check merge-vmaf: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        "chunk" => {
            let path = PathBuf::from(args.next().unwrap_or_default());
            let dur_s = args.next().unwrap_or_default();
            let expect: f64 = match dur_s.parse() {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("chunk: bad expect_dur {dur_s:?}");
                    return ExitCode::from(2);
                }
            };
            check_chunk_file(&path, expect)
        }
        "final" => {
            // Discrete args; avoid JSON (same two-way backslash issue).
            let input = args.next().unwrap_or_default();
            let output = args.next().unwrap_or_default();
            let ov = args.next().unwrap_or_default();
            let oc = args.next().unwrap_or_default();
            let av = args.next().unwrap_or_default();
            let ac = args.next().unwrap_or_default();
            if input.is_empty() || output.is_empty() {
                eprintln!("final: need input output opaque_video opaque_container alpha_video alpha_container");
                return ExitCode::from(2);
            }
            let spec = match (|| -> Result<FarmFinalCheck, String> {
                Ok(FarmFinalCheck {
                    input,
                    output,
                    opaque_video: parse_opaque_video(&ov)?,
                    opaque_container: parse_container(&oc)?,
                    alpha_video: parse_alpha_video(&av)?,
                    alpha_container: parse_container(&ac)?,
                })
            })() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("final: {e}");
                    return ExitCode::from(2);
                }
            };
            check_output_pair(
                PathBuf::from(&spec.input).as_path(),
                PathBuf::from(&spec.output).as_path(),
                spec.targets(),
            )
        }
        "cleanup" => {
            let dir = args.next().unwrap_or_default();
            let stem = args.next().unwrap_or_default();
            let ext = args.next().unwrap_or_default();
            let chunks_s = args.next().unwrap_or_default();
            let chunks: u32 = match chunks_s.parse() {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("cleanup: bad chunks {chunks_s:?}");
                    return ExitCode::from(2);
                }
            };
            if dir.is_empty() || stem.is_empty() || ext.is_empty() {
                eprintln!("cleanup: need dir stem ext chunks");
                return ExitCode::from(2);
            }
            cleanup_farm_intermediates(PathBuf::from(dir).as_path(), &stem, &ext, chunks)
        }
        "mux-alpha-webm" => {
            let color = PathBuf::from(args.next().unwrap_or_default());
            let alpha = PathBuf::from(args.next().unwrap_or_default());
            let out = PathBuf::from(args.next().unwrap_or_default());
            if color.as_os_str().is_empty()
                || alpha.as_os_str().is_empty()
                || out.as_os_str().is_empty()
            {
                eprintln!(
                    "mux-alpha-webm: need <color.ivf> <alpha.ivf> <out.webm> [range] [space] [primaries] [transfer]"
                );
                return ExitCode::from(2);
            }
            // Optional colour tags (empty / "-" = omit); written into Matroska Colour.
            fn opt_tag(s: Option<String>) -> Option<String> {
                let t = s.unwrap_or_default().trim().to_string();
                if t.is_empty() || t == "-" {
                    None
                } else {
                    Some(t.to_ascii_lowercase())
                }
            }
            let meta = VideoColorMeta {
                range: opt_tag(args.next()),
                space: opt_tag(args.next()),
                primaries: opt_tag(args.next()),
                transfer: opt_tag(args.next()),
            };
            match mux_vp9_alpha_webm(&color, &alpha, &out, &meta) {
                Ok(()) => {
                    println!("gigamux-check mux-alpha-webm: ok {}", out.display());
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("gigamux-check mux-alpha-webm: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        "farm-probe" => {
            let dir = args.next().unwrap_or_default();
            if dir.is_empty() {
                eprintln!("farm-probe: need <out_dir>");
                return ExitCode::from(2);
            }
            match write_farm_probe_report(PathBuf::from(dir).as_path()) {
                Ok(path) => {
                    println!("gigamux-check farm-probe: ok {}", path.display());
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("gigamux-check farm-probe: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        other => {
            eprintln!("unknown command {other:?}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => {
            println!("gigamux-check {cmd}: ok");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("gigamux-check {cmd}: {e}");
            ExitCode::FAILURE
        }
    }
}
