//! Flamenco worker helper for GigaMux hybrid integrity.
//!
//! Commands (exit 0 = ok, non-zero = fail task):
//! - `gigamux-check ensure-chunk <path> <expect_dur> <ffmpeg_exe> -- <ffmpeg argv…>` (argv includes leading program name; do **not** nest paths in JSON: Flamenco two-way expands `{var}` to `W:\…` and breaks JSON escapes)
//! - `gigamux-check ensure-out <path> <expect_dur> <ffmpeg_exe> -- <ffmpeg argv…>` (same for mux stage vmux / remux final; rebuilds incomplete leftovers)
//! - `gigamux-check ensure-vmaf <ref> <dist> <sidecar> [ffmpeg_exe]` (reuse sidecar score; else libvmaf + write)
//! - `gigamux-check chunk <path> <expect_dur_secs>`
//! - `gigamux-check final <input> <output> <opaque_video> <opaque_container> <alpha_video> <alpha_container>`
//! - `gigamux-check cleanup <dir> <stem> <ext> <chunks>`

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use gigamux_core::check::{
    check_chunk_file, check_output_pair, cleanup_farm_intermediates, ensure_chunk_file,
    ensure_out_file,
};
use gigamux_core::flamenco::FarmFinalCheck;
use gigamux_core::targets::{AlphaVideo, OutContainer, OpaqueVideo};
use gigamux_core::vmaf::ensure_vmaf;

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
            "usage: gigamux-check ensure-chunk|ensure-out|ensure-vmaf|chunk|final|cleanup …"
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
            // Discrete args — avoid JSON (same two-way backslash issue).
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
