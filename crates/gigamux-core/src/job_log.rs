//! Per-job gzipped logs under `%APPDATA%\GigaMux\config\logs\` (encode + VMAF).

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::Local;
use flate2::write::GzEncoder;
use flate2::Compression;

use crate::encode::EncodeEvent;
use crate::prefs::prefs_dir;
use crate::vmaf::VmafEvent;

/// Which pipeline owns this durable log file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobLogKind {
    Encode,
    Vmaf,
}

impl JobLogKind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Encode => "encode",
            Self::Vmaf => "vmaf",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Encode => "encode",
            Self::Vmaf => "VMAF",
        }
    }
}

/// One gzipped log for a single encode or VMAF batch.
pub struct JobLog {
    path: PathBuf,
    encoder: Option<GzEncoder<BufWriter<File>>>,
}

/// Backward-compatible alias used by encode.
pub type EncodeJobLog = JobLog;

impl JobLog {
    /// Create `logs/{encode|vmaf}_YYYYMMDD_HHMMSS.log.gz` (suffix if the stamp collides).
    pub fn start(kind: JobLogKind) -> Result<Self, String> {
        let root = prefs_dir().ok_or_else(|| "could not resolve AppData config dir".to_string())?;
        let dir = root.join("logs");
        fs::create_dir_all(&dir).map_err(|e| format!("create logs dir: {e}"))?;
        let stamp = Local::now().format("%Y%m%d_%H%M%S");
        let prefix = kind.prefix();
        let mut path = dir.join(format!("{prefix}_{stamp}.log.gz"));
        let mut n = 2u32;
        while path.exists() {
            path = dir.join(format!("{prefix}_{stamp}_{n}.log.gz"));
            n += 1;
        }
        let file = File::create(&path).map_err(|e| format!("create {}: {e}", path.display()))?;
        let mut encoder = GzEncoder::new(BufWriter::new(file), Compression::default());
        writeln!(
            encoder,
            "# GigaMux {} job log — {}",
            kind.title(),
            Local::now().to_rfc3339()
        )
        .map_err(|e| format!("write log header: {e}"))?;
        Ok(Self {
            path,
            encoder: Some(encoder),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append a human line for durable encode events (skips progress thumbs / out-time ticks).
    pub fn record_encode(&mut self, ev: &EncodeEvent) {
        let Some(line) = encode_event_line(ev) else {
            return;
        };
        self.write_line(&line);
    }

    /// Append a human line for durable VMAF events (skips out-time ticks).
    pub fn record_vmaf(&mut self, ev: &VmafEvent) {
        let Some(line) = vmaf_event_line(ev) else {
            return;
        };
        self.write_line(&line);
    }

    fn write_line(&mut self, line: &str) {
        let Some(enc) = self.encoder.as_mut() else {
            return;
        };
        let _ = writeln!(enc, "{line}");
        let _ = enc.flush();
    }
}

impl Drop for JobLog {
    fn drop(&mut self) {
        if let Some(enc) = self.encoder.take() {
            let _ = enc.finish();
        }
    }
}

fn encode_event_line(ev: &EncodeEvent) -> Option<String> {
    match ev {
        EncodeEvent::Log(msg) => Some(msg.clone()),
        EncodeEvent::BatchStarted {
            total_files,
            total_duration_s,
        } => Some(format!(
            "batch started — {total_files} file(s), {total_duration_s:.3}s total duration"
        )),
        EncodeEvent::FileStarted {
            name,
            index,
            total,
            duration_s,
        } => Some(format!(
            "=== file {index}/{total}: {name} ({duration_s:.3}s) ==="
        )),
        EncodeEvent::FileSkipped { name } => Some(format!("skip (exists): {name}")),
        EncodeEvent::FileDone { name } => Some(format!("done: {name}")),
        EncodeEvent::FileFailed { name, error } => Some(format!("error: {name}: {error}")),
        EncodeEvent::Finished {
            ok,
            skipped,
            failed,
        } => Some(format!(
            "finished — ok={ok} skipped={skipped} failed={failed}"
        )),
        EncodeEvent::FileProgress { .. } | EncodeEvent::LiveThumb { .. } => None,
    }
}

fn vmaf_event_line(ev: &VmafEvent) -> Option<String> {
    match ev {
        VmafEvent::Log(msg) => Some(msg.clone()),
        VmafEvent::BatchStarted {
            total_files,
            total_duration_s,
        } => Some(format!(
            "batch started — {total_files} pair(s), {total_duration_s:.3}s total duration"
        )),
        VmafEvent::FileStarted {
            name,
            index,
            total,
            duration_s,
        } => Some(format!(
            "=== pair {index}/{total}: {name} ({duration_s:.3}s) ==="
        )),
        VmafEvent::FileDone { name, score } => Some(format!("done: {name} — VMAF {score:.4}")),
        VmafEvent::FileFailed { name, error } => Some(format!("error: {name}: {error}")),
        VmafEvent::Finished { ok, failed } => {
            Some(format!("finished — scored={ok} failed={failed}"))
        }
        VmafEvent::FileProgress { .. } => None,
    }
}

/// `%APPDATA%\GigaMux\config\logs` (or platform equivalent).
pub fn encode_logs_dir() -> Option<PathBuf> {
    prefs_dir().map(|d| d.join("logs"))
}

/// Alias — same directory holds encode + VMAF job logs.
pub fn job_logs_dir() -> Option<PathBuf> {
    encode_logs_dir()
}

/// Delete every file under `config/logs` (encode/VMAF `.log.gz` etc.). Returns how many removed.
pub fn clear_encode_logs() -> Result<usize, String> {
    let Some(dir) = encode_logs_dir() else {
        return Ok(0);
    };
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut n = 0usize;
    for entry in fs::read_dir(&dir).map_err(|e| format!("read logs dir: {e}"))? {
        let entry = entry.map_err(|e| format!("logs entry: {e}"))?;
        let path = entry.path();
        if path.is_file() {
            fs::remove_file(&path).map_err(|e| format!("remove {}: {e}", path.display()))?;
            n += 1;
        }
    }
    Ok(n)
}
