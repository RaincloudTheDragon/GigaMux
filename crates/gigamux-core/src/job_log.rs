//! Per-encode-job gzipped logs under `%APPDATA%\GigaMux\config\logs\`.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::Local;
use flate2::write::GzEncoder;
use flate2::Compression;

use crate::encode::EncodeEvent;
use crate::prefs::prefs_dir;

/// One gzipped log for a single `run_batch` invocation.
pub struct EncodeJobLog {
    path: PathBuf,
    encoder: Option<GzEncoder<BufWriter<File>>>,
}

impl EncodeJobLog {
    /// Create `logs/encode_YYYYMMDD_HHMMSS.log.gz` (suffix if the stamp collides).
    pub fn start() -> Result<Self, String> {
        let root = prefs_dir().ok_or_else(|| "could not resolve AppData config dir".to_string())?;
        let dir = root.join("logs");
        fs::create_dir_all(&dir).map_err(|e| format!("create logs dir: {e}"))?;
        let stamp = Local::now().format("%Y%m%d_%H%M%S");
        let mut path = dir.join(format!("encode_{stamp}.log.gz"));
        let mut n = 2u32;
        while path.exists() {
            path = dir.join(format!("encode_{stamp}_{n}.log.gz"));
            n += 1;
        }
        let file = File::create(&path).map_err(|e| format!("create {}: {e}", path.display()))?;
        let mut encoder = GzEncoder::new(BufWriter::new(file), Compression::default());
        writeln!(
            encoder,
            "# GigaMux encode job log — {}",
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

    /// Append a human line for durable events (skips progress thumbs / out-time ticks).
    pub fn record(&mut self, ev: &EncodeEvent) {
        let Some(line) = event_line(ev) else {
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

impl Drop for EncodeJobLog {
    fn drop(&mut self) {
        if let Some(enc) = self.encoder.take() {
            let _ = enc.finish();
        }
    }
}

fn event_line(ev: &EncodeEvent) -> Option<String> {
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

/// `%APPDATA%\GigaMux\config\logs` (or platform equivalent).
pub fn encode_logs_dir() -> Option<PathBuf> {
    prefs_dir().map(|d| d.join("logs"))
}
