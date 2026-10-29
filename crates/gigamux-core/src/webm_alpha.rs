//! Remux paired VP9 IVF (color + alpha) into a WebM with AlphaMode + BlockAdditional.
//! Alpha plane is BlockAddID 1 (codec-defined), matching libvpx/WebM alpha packaging.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

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
pub fn mux_vp9_alpha_webm(color_ivf: &Path, alpha_ivf: &Path, out_webm: &Path) -> Result<(), String> {
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
    )
    .map_err(|e| format!("write WebM {}: {e}", out_webm.display()))?;
    Ok(())
}

fn write_webm_vp9_alpha(
    w: &mut impl Write,
    width: u16,
    height: u16,
    tb_num: u64,
    tb_den: u64,
    color: &[IvfFrame],
    alpha: &[IvfFrame],
) -> std::io::Result<()> {
    // Timecode scale: 1 ms. Convert IVF pts → ms.
    const TIMECODE_SCALE: u64 = 1_000_000; // ns per ms unit in Matroska Info
    let pts_to_ms = |pts: u64| -> u64 {
        // pts * (tb_num/tb_den) seconds → ms
        pts.saturating_mul(tb_num).saturating_mul(1000) / tb_den.max(1)
    };

    // EBML header
    write_master(w, ID_EBML, &|w| {
        write_uint(w, ID_EBML_VERSION, 1)?;
        write_uint(w, ID_EBML_READ_VERSION, 1)?;
        write_uint(w, ID_EBML_MAX_ID_LENGTH, 4)?;
        write_uint(w, ID_EBML_MAX_SIZE_LENGTH, 8)?;
        write_string(w, ID_DOC_TYPE, "webm")?;
        write_uint(w, ID_DOC_TYPE_VERSION, 4)?;
        write_uint(w, ID_DOC_TYPE_READ_VERSION, 2)?;
        Ok(())
    })?;

    // Segment with unknown size (streaming-friendly).
    write_id(w, ID_SEGMENT)?;
    write_unknown_size(w)?;

    write_master(w, ID_INFO, &|w| {
        write_uint(w, ID_TIMECODE_SCALE, TIMECODE_SCALE)?;
        write_string(w, ID_MUXING_APP, "GigaMux")?;
        write_string(w, ID_WRITING_APP, "GigaMux vp9_qsv+libvpx alpha hybrid")?;
        Ok(())
    })?;

    write_master(w, ID_TRACKS, &|w| {
        write_master(w, ID_TRACK_ENTRY, &|w| {
            write_uint(w, ID_TRACK_NUMBER, 1)?;
            write_uint(w, ID_TRACK_UID, 1)?;
            write_uint(w, ID_TRACK_TYPE, 1)?; // video
            write_uint(w, ID_FLAG_LACING, 0)?;
            write_uint(w, ID_MAX_BLOCK_ADDITION_ID, 1)?;
            write_string(w, ID_CODEC_ID, "V_VP9")?;
            write_master(w, ID_VIDEO, &|w| {
                write_uint(w, ID_PIXEL_WIDTH, width as u64)?;
                write_uint(w, ID_PIXEL_HEIGHT, height as u64)?;
                write_uint(w, ID_ALPHA_MODE, 1)?;
                Ok(())
            })?;
            Ok(())
        })?;
        Ok(())
    })?;

    // One Cluster per ~1s of media (or all frames if shorter).
    let cluster_span_ms: u64 = 1000;
    let mut i = 0;
    while i < color.len() {
        let cluster_tc = pts_to_ms(color[i].pts);
        let mut end = i + 1;
        while end < color.len() {
            let t = pts_to_ms(color[end].pts);
            if t.saturating_sub(cluster_tc) > cluster_span_ms {
                break;
            }
            // Block relative timecode is i16 — stay within range.
            if t.saturating_sub(cluster_tc) > i16::MAX as u64 {
                break;
            }
            end += 1;
        }

        write_master(w, ID_CLUSTER, &|w| {
            write_uint(w, ID_CLUSTER_TIMECODE, cluster_tc)?;
            for idx in i..end {
                let abs = pts_to_ms(color[idx].pts);
                let rel = (abs.saturating_sub(cluster_tc)) as i16;
                // IVF has no keyframe flag; omit ReferenceBlock (all key) so alpha sidecars always decode.
                write_block_group(w, 1, rel, true, &color[idx].data, &alpha[idx].data)?;
            }
            Ok(())
        })?;
        i = end;
    }

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
    write_master(w, ID_BLOCK_GROUP, &|w| {
        write_binary(w, ID_BLOCK, &encode_block(track, rel_tc, color))?;
        write_master(w, ID_BLOCK_ADDITIONS, &|w| {
            write_master(w, ID_BLOCK_MORE, &|w| {
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
const ID_MUXING_APP: &[u8] = &[0x4D, 0x80];
const ID_WRITING_APP: &[u8] = &[0x57, 0x41];
const ID_TRACKS: &[u8] = &[0x16, 0x54, 0xAE, 0x6B];
const ID_TRACK_ENTRY: &[u8] = &[0xAE];
const ID_TRACK_NUMBER: &[u8] = &[0xD7];
const ID_TRACK_UID: &[u8] = &[0x73, 0xC5];
const ID_TRACK_TYPE: &[u8] = &[0x83];
const ID_FLAG_LACING: &[u8] = &[0x9C];
const ID_MAX_BLOCK_ADDITION_ID: &[u8] = &[0x55, 0xEE];
const ID_CODEC_ID: &[u8] = &[0x86];
const ID_VIDEO: &[u8] = &[0xE0];
const ID_PIXEL_WIDTH: &[u8] = &[0xB0];
const ID_PIXEL_HEIGHT: &[u8] = &[0xBA];
const ID_ALPHA_MODE: &[u8] = &[0x53, 0xC0];
const ID_CLUSTER: &[u8] = &[0x1F, 0x43, 0xB6, 0x75];
const ID_CLUSTER_TIMECODE: &[u8] = &[0xE7];
const ID_BLOCK_GROUP: &[u8] = &[0xA0];
const ID_BLOCK: &[u8] = &[0xA1];
const ID_BLOCK_ADDITIONS: &[u8] = &[0x75, 0xA1];
const ID_BLOCK_MORE: &[u8] = &[0xA6];
const ID_BLOCK_ADD_ID: &[u8] = &[0xEE];
const ID_BLOCK_ADDITIONAL: &[u8] = &[0xA5];
const ID_REFERENCE_BLOCK: &[u8] = &[0xFB];

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
    body: &dyn Fn(&mut Vec<u8>) -> std::io::Result<()>,
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
        let mut cur = Cursor::new(Vec::new());
        write_webm_vp9_alpha(&mut cur, 16, 16, 1, 30, &color, &alpha).unwrap();
        let bytes = cur.into_inner();
        assert!(bytes.windows(4).any(|w| w == b"webm"));
        assert!(bytes.len() > 64);
    }

}
