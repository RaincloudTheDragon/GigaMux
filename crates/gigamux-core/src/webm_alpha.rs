//! Remux paired VP9 IVF (color + alpha) into a WebM with AlphaMode + BlockAdditional.
//! Alpha plane is BlockAddID 1 (codec-defined), matching libvpx/WebM alpha packaging.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use crate::probe::VideoColorMeta;

/// One coded frame from an IVF file.
#[derive(Debug, Clone)]
pub struct IvfFrame {
    pub pts: u64,
    pub data: Vec<u8>,
}

/// IVF stream header (DKIF).
#[derive(Debug, Clone)]
pub struct IvfHeader {
    pub width: u16,
    pub height: u16,
    /// Timebase denominator (ticks per second).
    pub timebase_den: u32,
    /// Timebase numerator.
    pub timebase_num: u32,
    pub frame_count: u32,
}

/// Read IVF header + all frames.
pub fn read_ivf(path: &Path) -> Result<(IvfHeader, Vec<IvfFrame>), String> {
    let mut f = File::open(path).map_err(|e| format!("open IVF {}: {e}", path.display()))?;
    let mut hdr = [0u8; 32];
    f.read_exact(&mut hdr)
        .map_err(|e| format!("read IVF header {}: {e}", path.display()))?;
    if &hdr[0..4] != b"DKIF" {
        return Err(format!("not an IVF file: {}", path.display()));
    }
    let header = IvfHeader {
        width: u16::from_le_bytes([hdr[12], hdr[13]]),
        height: u16::from_le_bytes([hdr[14], hdr[15]]),
        timebase_den: u32::from_le_bytes([hdr[16], hdr[17], hdr[18], hdr[19]]),
        timebase_num: u32::from_le_bytes([hdr[20], hdr[21], hdr[22], hdr[23]]),
        frame_count: u32::from_le_bytes([hdr[24], hdr[25], hdr[26], hdr[27]]),
    };
    if header.width == 0 || header.height == 0 {
        return Err(format!("IVF has zero dimensions: {}", path.display()));
    }
    if header.timebase_den == 0 || header.timebase_num == 0 {
        return Err(format!("IVF has invalid timebase: {}", path.display()));
    }

    let mut frames = Vec::new();
    loop {
        let mut fh = [0u8; 12];
        match f.read_exact(&mut fh) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(format!("read IVF frame header {}: {e}", path.display())),
        }
        let size = u32::from_le_bytes([fh[0], fh[1], fh[2], fh[3]]) as usize;
        let pts = u64::from_le_bytes([
            fh[4], fh[5], fh[6], fh[7], fh[8], fh[9], fh[10], fh[11],
        ]);
        let mut data = vec![0u8; size];
        f.read_exact(&mut data)
            .map_err(|e| format!("read IVF frame data {}: {e}", path.display()))?;
        frames.push(IvfFrame { pts, data });
    }
    Ok((header, frames))
}

/// Pair color + alpha IVFs into a video-only WebM with AlphaMode=1 and BlockAdditional alpha.
/// `color_meta` is written into Matroska `Colour` so ffprobe doesn't default the stream to `tv`.
pub fn mux_vp9_alpha_webm(
    color_ivf: &Path,
    alpha_ivf: &Path,
    out_webm: &Path,
    color_meta: &VideoColorMeta,
) -> Result<(), String> {
    let (color_hdr, color_frames) = read_ivf(color_ivf)?;
    let (alpha_hdr, alpha_frames) = read_ivf(alpha_ivf)?;

    if color_frames.is_empty() {
        return Err("color IVF has no frames".into());
    }
    if color_frames.len() != alpha_frames.len() {
        return Err(format!(
            "IVF frame count mismatch: color={} alpha={}",
            color_frames.len(),
            alpha_frames.len()
        ));
    }
    if color_hdr.width != alpha_hdr.width || color_hdr.height != alpha_hdr.height {
        return Err(format!(
            "IVF dimension mismatch: color={}x{} alpha={}x{}",
            color_hdr.width, color_hdr.height, alpha_hdr.width, alpha_hdr.height
        ));
    }

    // Prefer color timebase; alpha should match after a shared filter graph.
    let tb_num = color_hdr.timebase_num as u64;
    let tb_den = color_hdr.timebase_den as u64;

    let mut out = File::create(out_webm).map_err(|e| format!("create {}: {e}", out_webm.display()))?;
    write_webm_vp9_alpha(
        &mut out,
        color_hdr.width,
        color_hdr.height,
        tb_num,
        tb_den,
        &color_frames,
        &alpha_frames,
        color_meta,
    )
    .map_err(|e| format!("write WebM {}: {e}", out_webm.display()))?;
    Ok(())
}

/// Matroska `Colour.Range` (RFC 9559): 1 = broadcast/tv, 2 = full/pc.
fn matroska_range(name: &str) -> Option<u64> {
    match name.to_ascii_lowercase().as_str() {
        "tv" | "mpeg" => Some(1),
        "pc" | "jpeg" => Some(2),
        _ => None,
    }
}

/// ISO/IEC 23001-8 / H.273 matrix coefficients commonly emitted by ffprobe.
fn matroska_matrix(name: &str) -> Option<u64> {
    match name.to_ascii_lowercase().as_str() {
        "rgb" | "gbr" => Some(0),
        "bt709" => Some(1),
        "fcc" => Some(4),
        "bt470bg" | "gamma28" => Some(5),
        "smpte170m" => Some(6),
        "smpte240m" => Some(7),
        "ycgco" => Some(8),
        "bt2020nc" | "bt2020_ncl" => Some(9),
        "bt2020c" | "bt2020_cl" => Some(10),
        "smpte2085" => Some(11),
        "chroma-derived-nc" => Some(12),
        "chroma-derived-c" => Some(13),
        "ictcp" => Some(14),
        _ => None,
    }
}

fn matroska_primaries(name: &str) -> Option<u64> {
    match name.to_ascii_lowercase().as_str() {
        "bt709" => Some(1),
        "bt470m" => Some(4),
        "bt470bg" => Some(5),
        "smpte170m" => Some(6),
        "smpte240m" => Some(7),
        "film" => Some(8),
        "bt2020" => Some(9),
        "smpte428" | "smpte428_1" => Some(10),
        "smpte431" => Some(11),
        "smpte432" => Some(12),
        "jedec-p22" | "ebu3213" => Some(22),
        _ => None,
    }
}

fn matroska_transfer(name: &str) -> Option<u64> {
    match name.to_ascii_lowercase().as_str() {
        "bt709" => Some(1),
        "gamma22" | "bt470m" => Some(4),
        "gamma28" | "bt470bg" => Some(5),
        "smpte170m" => Some(6),
        "smpte240m" => Some(7),
        "linear" => Some(8),
        "log" | "log100" => Some(9),
        "log_sqrt" | "log316" => Some(10),
        "iec61966-2-4" => Some(11),
        "bt1361e" | "bt1361" => Some(12),
        "iec61966-2-1" => Some(13), // sRGB / scRGB — common on full-range WebM
        "bt2020-10" | "bt2020_10" => Some(14),
        "bt2020-12" | "bt2020_12" => Some(15),
        "smpte2084" => Some(16),
        "smpte428" | "smpte428_1" => Some(17),
        "arib-std-b67" => Some(18),
        _ => None,
    }
}

/// True when a VP9 bitstream frame is a keyframe (uncompressed header `frame_type == 0`).
fn vp9_is_keyframe(data: &[u8]) -> bool {
    if data.len() < 2 {
        return true;
    }
    let mut bit: u32 = 0;
    let read = |bit: &mut u32, n: u32| -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *bit / 8;
            let b = *data.get(byte as usize)?;
            let shift = 7 - (*bit % 8);
            v = (v << 1) | (((b >> shift) & 1) as u32);
            *bit += 1;
        }
        Some(v)
    };
    // frame_marker must be 2
    if read(&mut bit, 2) != Some(2) {
        return false;
    }
    let profile_low = read(&mut bit, 1).unwrap_or(0);
    let profile_high = read(&mut bit, 1).unwrap_or(0);
    let profile = (profile_high << 1) | profile_low;
    if profile == 3 {
        let _ = read(&mut bit, 1);
    }
    if read(&mut bit, 1) == Some(1) {
        // show_existing_frame — not a coded keyframe
        return false;
    }
    // frame_type: 0 = KEY_FRAME
    read(&mut bit, 1) == Some(0)
}

/// Median positive pts delta (ticks); falls back to 1.
fn median_pts_delta(frames: &[IvfFrame]) -> u64 {
    let mut deltas: Vec<u64> = frames
        .windows(2)
        .filter_map(|w| w[1].pts.checked_sub(w[0].pts))
        .filter(|d| *d > 0)
        .collect();
    if deltas.is_empty() {
        return 1;
    }
    deltas.sort_unstable();
    deltas[deltas.len() / 2]
}

fn write_webm_vp9_alpha(
    w: &mut impl Write,
    width: u16,
    height: u16,
    tb_num: u64,
    tb_den: u64,
    color: &[IvfFrame],
    alpha: &[IvfFrame],
    color_meta: &VideoColorMeta,
) -> std::io::Result<()> {
    // One Matroska tick = one IVF pts unit (avoids 16/17ms VFR jitter from 1ms scale @ 60fps).
    let timecode_scale = (1_000_000_000u64.saturating_mul(tb_num.max(1))) / tb_den.max(1);
    let timecode_scale = timecode_scale.max(1);
    let pts0 = color.first().map(|f| f.pts).unwrap_or(0);
    let delta = median_pts_delta(color);
    let default_duration_ns = timecode_scale.saturating_mul(delta);
    let last_pts = color.last().map(|f| f.pts).unwrap_or(pts0);
    // Duration in TimecodeScale ticks (Matroska Info Duration is a float).
    let duration_ticks = last_pts.saturating_sub(pts0).saturating_add(delta) as f64;

    // EBML header
    write_master(w, ID_EBML, |w| {
        write_uint(w, ID_EBML_VERSION, 1)?;
        write_uint(w, ID_EBML_READ_VERSION, 1)?;
        write_uint(w, ID_EBML_MAX_ID_LENGTH, 4)?;
        write_uint(w, ID_EBML_MAX_SIZE_LENGTH, 8)?;
        write_string(w, ID_DOC_TYPE, "webm")?;
        write_uint(w, ID_DOC_TYPE_VERSION, 4)?;
        write_uint(w, ID_DOC_TYPE_READ_VERSION, 2)?;
        Ok(())
    })?;

    // Segment with unknown size; track cluster offsets for Cues (Premiere/Influx seek).
    write_id(w, ID_SEGMENT)?;
    write_unknown_size(w)?;
    let mut segment = Vec::new();
    let mut cue_entries: Vec<(u64, u64)> = Vec::new(); // (time_ticks, cluster_pos)

    {
        let w = &mut segment;
        write_master(w, ID_INFO, |w| {
            write_uint(w, ID_TIMECODE_SCALE, timecode_scale)?;
            write_float(w, ID_DURATION, duration_ticks)?;
            write_string(w, ID_MUXING_APP, "GigaMux")?;
            write_string(w, ID_WRITING_APP, "GigaMux vp9_qsv+libvpx alpha hybrid")?;
            Ok(())
        })?;

        write_master(w, ID_TRACKS, |w| {
            write_master(w, ID_TRACK_ENTRY, |w| {
                write_uint(w, ID_TRACK_NUMBER, 1)?;
                write_uint(w, ID_TRACK_UID, 1)?;
                write_uint(w, ID_TRACK_TYPE, 1)?; // video
                write_uint(w, ID_FLAG_LACING, 0)?;
                write_uint(w, ID_MAX_BLOCK_ADDITION_ID, 1)?;
                write_uint(w, ID_DEFAULT_DURATION, default_duration_ns)?;
                write_string(w, ID_CODEC_ID, "V_VP9")?;
                write_master(w, ID_VIDEO, |w| {
                    write_uint(w, ID_PIXEL_WIDTH, width as u64)?;
                    write_uint(w, ID_PIXEL_HEIGHT, height as u64)?;
                    write_uint(w, ID_ALPHA_MODE, 1)?;
                    let range = color_meta.range.as_deref().and_then(matroska_range);
                    let matrix = color_meta.space.as_deref().and_then(matroska_matrix);
                    let primaries = color_meta.primaries.as_deref().and_then(matroska_primaries);
                    let transfer = color_meta.transfer.as_deref().and_then(matroska_transfer);
                    if range.is_some()
                        || matrix.is_some()
                        || primaries.is_some()
                        || transfer.is_some()
                    {
                        write_master(w, ID_COLOUR, |w| {
                            if let Some(v) = matrix {
                                write_uint(w, ID_MATRIX_COEFFICIENTS, v)?;
                            }
                            if let Some(v) = range {
                                write_uint(w, ID_COLOUR_RANGE, v)?;
                            }
                            if let Some(v) = transfer {
                                write_uint(w, ID_TRANSFER_CHARACTERISTICS, v)?;
                            }
                            if let Some(v) = primaries {
                                write_uint(w, ID_PRIMARIES, v)?;
                            }
                            Ok(())
                        })?;
                    }
                    Ok(())
                })?;
                Ok(())
            })?;
            Ok(())
        })?;

        // ~1s clusters in tick units (or 60 frames if tb is 1/fps).
        let cluster_span = delta.saturating_mul(60).max(1);
        let mut i = 0;
        while i < color.len() {
            let cluster_tc = color[i].pts.saturating_sub(pts0);
            let mut end = i + 1;
            while end < color.len() {
                let t = color[end].pts.saturating_sub(pts0);
                if t.saturating_sub(cluster_tc) > cluster_span {
                    break;
                }
                if t.saturating_sub(cluster_tc) > i16::MAX as u64 {
                    break;
                }
                end += 1;
            }

            let cluster_pos = w.len() as u64;
            write_master(w, ID_CLUSTER, |w| {
                write_uint(w, ID_CLUSTER_TIMECODE, cluster_tc)?;
                for idx in i..end {
                    let abs = color[idx].pts.saturating_sub(pts0);
                    let rel = (abs.saturating_sub(cluster_tc)) as i16;
                    let is_key = vp9_is_keyframe(&color[idx].data);
                    if is_key {
                        cue_entries.push((abs, cluster_pos));
                    }
                    write_block_group(
                        w,
                        1,
                        rel,
                        is_key,
                        &color[idx].data,
                        &alpha[idx].data,
                    )?;
                }
                Ok(())
            })?;
            i = end;
        }

        // Deduplicate cues that share a cluster (keep first key time per cluster).
        cue_entries.sort_by_key(|e| e.1);
        cue_entries.dedup_by_key(|e| e.1);
        cue_entries.sort_by_key(|e| e.0);

        write_master(w, ID_CUES, |w| {
            for (time, pos) in &cue_entries {
                write_master(w, ID_CUE_POINT, |w| {
                    write_uint(w, ID_CUE_TIME, *time)?;
                    write_master(w, ID_CUE_TRACK_POSITIONS, |w| {
                        write_uint(w, ID_CUE_TRACK, 1)?;
                        write_uint(w, ID_CUE_CLUSTER_POSITION, *pos)?;
                        Ok(())
                    })?;
                    Ok(())
                })?;
            }
            Ok(())
        })?;
    }

    w.write_all(&segment)?;
    w.flush()?;
    Ok(())
}

fn write_block_group(
    w: &mut impl Write,
    track: u64,
    rel_tc: i16,
    is_key: bool,
    color: &[u8],
    alpha: &[u8],
) -> std::io::Result<()> {
    write_master(w, ID_BLOCK_GROUP, |w| {
        write_binary(w, ID_BLOCK, &encode_block(track, rel_tc, color))?;
        write_master(w, ID_BLOCK_ADDITIONS, |w| {
            write_master(w, ID_BLOCK_MORE, |w| {
                // BlockAddID 1 is default; write explicitly for clarity.
                write_uint(w, ID_BLOCK_ADD_ID, 1)?;
                write_binary(w, ID_BLOCK_ADDITIONAL, alpha)?;
                Ok(())
            })?;
            Ok(())
        })?;
        if !is_key {
            // Self-reference to prior frame (0 = previous block in display order).
            write_int(w, ID_REFERENCE_BLOCK, 0)?;
        }
        Ok(())
    })
}

fn encode_block(track: u64, rel_tc: i16, frame: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + frame.len());
    write_vint_number(&mut buf, track);
    buf.extend_from_slice(&rel_tc.to_be_bytes());
    // Flags: no lacing; keyframe bit is not in Block (Matroska uses ReferenceBlock).
    buf.push(0x00);
    buf.extend_from_slice(frame);
    buf
}

// --- EBML helpers ---

const ID_EBML: &[u8] = &[0x1A, 0x45, 0xDF, 0xA3];
const ID_EBML_VERSION: &[u8] = &[0x42, 0x86];
const ID_EBML_READ_VERSION: &[u8] = &[0x42, 0xF7];
const ID_EBML_MAX_ID_LENGTH: &[u8] = &[0x42, 0xF2];
const ID_EBML_MAX_SIZE_LENGTH: &[u8] = &[0x42, 0xF3];
const ID_DOC_TYPE: &[u8] = &[0x42, 0x82];
const ID_DOC_TYPE_VERSION: &[u8] = &[0x42, 0x87];
const ID_DOC_TYPE_READ_VERSION: &[u8] = &[0x42, 0x85];
const ID_SEGMENT: &[u8] = &[0x18, 0x53, 0x80, 0x67];
const ID_INFO: &[u8] = &[0x15, 0x49, 0xA9, 0x66];
const ID_TIMECODE_SCALE: &[u8] = &[0x2A, 0xD7, 0xB1];
const ID_DURATION: &[u8] = &[0x44, 0x89];
const ID_MUXING_APP: &[u8] = &[0x4D, 0x80];
const ID_WRITING_APP: &[u8] = &[0x57, 0x41];
const ID_TRACKS: &[u8] = &[0x16, 0x54, 0xAE, 0x6B];
const ID_TRACK_ENTRY: &[u8] = &[0xAE];
const ID_TRACK_NUMBER: &[u8] = &[0xD7];
const ID_TRACK_UID: &[u8] = &[0x73, 0xC5];
const ID_TRACK_TYPE: &[u8] = &[0x83];
const ID_FLAG_LACING: &[u8] = &[0x9C];
const ID_DEFAULT_DURATION: &[u8] = &[0x23, 0xE3, 0x83];
const ID_MAX_BLOCK_ADDITION_ID: &[u8] = &[0x55, 0xEE];
const ID_CODEC_ID: &[u8] = &[0x86];
const ID_VIDEO: &[u8] = &[0xE0];
const ID_PIXEL_WIDTH: &[u8] = &[0xB0];
const ID_PIXEL_HEIGHT: &[u8] = &[0xBA];
const ID_ALPHA_MODE: &[u8] = &[0x53, 0xC0];
const ID_COLOUR: &[u8] = &[0x55, 0xB0];
const ID_MATRIX_COEFFICIENTS: &[u8] = &[0x55, 0xB1];
const ID_COLOUR_RANGE: &[u8] = &[0x55, 0xB9];
const ID_TRANSFER_CHARACTERISTICS: &[u8] = &[0x55, 0xBA];
const ID_PRIMARIES: &[u8] = &[0x55, 0xBB];
const ID_CLUSTER: &[u8] = &[0x1F, 0x43, 0xB6, 0x75];
const ID_CLUSTER_TIMECODE: &[u8] = &[0xE7];
const ID_BLOCK_GROUP: &[u8] = &[0xA0];
const ID_BLOCK: &[u8] = &[0xA1];
const ID_BLOCK_ADDITIONS: &[u8] = &[0x75, 0xA1];
const ID_BLOCK_MORE: &[u8] = &[0xA6];
const ID_BLOCK_ADD_ID: &[u8] = &[0xEE];
const ID_BLOCK_ADDITIONAL: &[u8] = &[0xA5];
const ID_REFERENCE_BLOCK: &[u8] = &[0xFB];
const ID_CUES: &[u8] = &[0x1C, 0x53, 0xBB, 0x6B];
const ID_CUE_POINT: &[u8] = &[0xBB];
const ID_CUE_TIME: &[u8] = &[0xB3];
const ID_CUE_TRACK_POSITIONS: &[u8] = &[0xB7];
const ID_CUE_TRACK: &[u8] = &[0xF7];
const ID_CUE_CLUSTER_POSITION: &[u8] = &[0xF1];

fn write_id(w: &mut impl Write, id: &[u8]) -> std::io::Result<()> {
    w.write_all(id)
}

fn write_unknown_size(w: &mut impl Write) -> std::io::Result<()> {
    // 1-byte unknown size: 0xFF
    w.write_all(&[0xFF])
}

fn write_size(w: &mut impl Write, size: u64) -> std::io::Result<()> {
    write_vint(w, size)
}

fn write_vint(w: &mut impl Write, value: u64) -> std::io::Result<()> {
    let mut buf = [0u8; 8];
    let len = encode_vint(value, &mut buf);
    w.write_all(&buf[..len])
}

fn write_vint_number(buf: &mut Vec<u8>, value: u64) {
    let mut tmp = [0u8; 8];
    let len = encode_vint(value, &mut tmp);
    buf.extend_from_slice(&tmp[..len]);
}

/// Encode an EBML variable-size integer (element size / track number).
fn encode_vint(value: u64, out: &mut [u8; 8]) -> usize {
    if value < (1 << 7) - 1 {
        out[0] = 0x80 | (value as u8);
        1
    } else if value < (1 << 14) - 1 {
        out[0] = 0x40 | ((value >> 8) as u8);
        out[1] = value as u8;
        2
    } else if value < (1 << 21) - 1 {
        out[0] = 0x20 | ((value >> 16) as u8);
        out[1] = (value >> 8) as u8;
        out[2] = value as u8;
        3
    } else if value < (1 << 28) - 1 {
        out[0] = 0x10 | ((value >> 24) as u8);
        out[1] = (value >> 16) as u8;
        out[2] = (value >> 8) as u8;
        out[3] = value as u8;
        4
    } else {
        out[0] = 0x01;
        out[1] = (value >> 48) as u8;
        out[2] = (value >> 40) as u8;
        out[3] = (value >> 32) as u8;
        out[4] = (value >> 24) as u8;
        out[5] = (value >> 16) as u8;
        out[6] = (value >> 8) as u8;
        out[7] = value as u8;
        8
    }
}

fn write_uint(w: &mut impl Write, id: &[u8], value: u64) -> std::io::Result<()> {
    let mut payload = Vec::new();
    // Minimal big-endian encoding (drop leading zeros, keep at least one byte).
    let be = value.to_be_bytes();
    let start = be.iter().position(|&b| b != 0).unwrap_or(7);
    payload.extend_from_slice(&be[start..]);
    write_id(w, id)?;
    write_size(w, payload.len() as u64)?;
    w.write_all(&payload)
}

fn write_float(w: &mut impl Write, id: &[u8], value: f64) -> std::io::Result<()> {
    write_id(w, id)?;
    write_size(w, 8)?;
    w.write_all(&value.to_be_bytes())
}

fn write_int(w: &mut impl Write, id: &[u8], value: i64) -> std::io::Result<()> {
    // Signed EBML integer — use 2 bytes for ReferenceBlock 0.
    let payload = (value as i16).to_be_bytes();
    write_id(w, id)?;
    write_size(w, payload.len() as u64)?;
    w.write_all(&payload)
}

fn write_string(w: &mut impl Write, id: &[u8], s: &str) -> std::io::Result<()> {
    write_id(w, id)?;
    write_size(w, s.len() as u64)?;
    w.write_all(s.as_bytes())
}

fn write_binary(w: &mut impl Write, id: &[u8], data: &[u8]) -> std::io::Result<()> {
    write_id(w, id)?;
    write_size(w, data.len() as u64)?;
    w.write_all(data)
}

fn write_master(
    w: &mut impl Write,
    id: &[u8],
    mut body: impl FnMut(&mut Vec<u8>) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    body(&mut buf)?;
    write_id(w, id)?;
    write_size(w, buf.len() as u64)?;
    w.write_all(&buf)
}

/// True when hybrid QSV color + libvpx alpha WebM remux should be attempted.
pub fn wants_qsv_alpha_hybrid(
    use_hardware: bool,
    has_alpha: bool,
    alpha_video_is_vp9: bool,
    alpha_container_is_webm: bool,
) -> bool {
    use_hardware && has_alpha && alpha_video_is_vp9 && alpha_container_is_webm
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn vint_roundtrip_sizes() {
        let mut buf = [0u8; 8];
        assert_eq!(encode_vint(1, &mut buf), 1);
        assert_eq!(buf[0], 0x81);
        // 1-byte VINT max payload is 126 (0x7F = unknown-size sentinel).
        assert_eq!(encode_vint(126, &mut buf), 1);
        assert_eq!(encode_vint(127, &mut buf), 2);
    }

    #[test]
    fn write_emptyish_webm_smoke() {
        // One tiny fake VP9-ish payload each (not a real bitstream — just mux structure).
        let color = vec![IvfFrame {
            pts: 0,
            data: vec![0x82, 0x49, 0x83],
        }];
        let alpha = vec![IvfFrame {
            pts: 0,
            data: vec![0x82, 0x49, 0x84],
        }];
        let meta = VideoColorMeta {
            range: Some("pc".into()),
            space: Some("bt709".into()),
            primaries: Some("bt709".into()),
            transfer: Some("iec61966-2-1".into()),
        };
        let mut cur = Cursor::new(Vec::new());
        write_webm_vp9_alpha(&mut cur, 16, 16, 1, 30, &color, &alpha, &meta).unwrap();
        let bytes = cur.into_inner();
        assert!(bytes.windows(4).any(|w| w == b"webm"));
        assert!(bytes.len() > 64);
        // Colour master id 0x55B0 should appear when meta is present.
        assert!(bytes.windows(2).any(|w| w == [0x55, 0xB0]));
    }
}
