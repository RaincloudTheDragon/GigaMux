//! NVENC GPU selection for ffmpeg `hevc_nvenc -gpu`.

use std::process::Command;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Persistable GPU choice: Any (−1) or explicit device index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuChoice {
    Any,
    Index(u32),
}

impl Default for GpuChoice {
    fn default() -> Self {
        Self::Any
    }
}

impl GpuChoice {
    /// Value passed to ffmpeg `-gpu`.
    pub fn ffmpeg_arg(self) -> String {
        match self {
            Self::Any => "any".to_string(),
            Self::Index(i) => i.to_string(),
        }
    }

    /// Fall back to Any when a saved index is not in the live device list.
    pub fn sanitize(self, devices: &[NvencDevice]) -> (Self, Option<String>) {
        match self {
            Self::Any => (Self::Any, None),
            Self::Index(i) => {
                if devices.iter().any(|d| d.index == i) {
                    (Self::Index(i), None)
                } else {
                    (
                        Self::Any,
                        Some(format!(
                            "saved GPU index {i} not found; falling back to Any"
                        )),
                    )
                }
            }
        }
    }
}

/// One NVENC-capable device from `ffmpeg -gpu list`.
#[derive(Debug, Clone)]
pub struct NvencDevice {
    pub index: u32,
    pub name: String,
}

/// Probe NVENC devices via a short hevc_nvenc encode with `-gpu list`.
pub fn list_nvenc_devices() -> Result<Vec<NvencDevice>, String> {
    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-f",
            "lavfi",
            "-i",
            "nullsrc=s=64x64:d=0.1",
            "-c:v",
            "hevc_nvenc",
            "-gpu",
            "list",
            "-f",
            "null",
            "-",
        ])
        .output()
        .map_err(|e| format!("ffmpeg not found or failed to start: {e}"))?;

    // Device lines are printed to stderr.
    let text = String::from_utf8_lossy(&output.stderr);
    parse_gpu_list(&text)
}

fn parse_gpu_list(stderr: &str) -> Result<Vec<NvencDevice>, String> {
    // e.g. [h264_nvenc @ ...] [ GPU #0 - < NVIDIA GeForce RTX 3060 > has Compute SM 8.6 ]
    let re = Regex::new(r"GPU #(\d+)\s*-\s*<\s*([^>]+?)\s*>")
        .map_err(|e| format!("gpu list regex: {e}"))?;
    let mut devices = Vec::new();
    for cap in re.captures_iter(stderr) {
        let index: u32 = cap[1].parse().map_err(|e| format!("gpu index parse: {e}"))?;
        let name = cap[2].trim().to_string();
        if !devices.iter().any(|d: &NvencDevice| d.index == index) {
            devices.push(NvencDevice { index, name });
        }
    }
    devices.sort_by_key(|d| d.index);
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sample_line() {
        let sample = r#"[hevc_nvenc @ 0x0] [ GPU #0 - < NVIDIA GeForce RTX 3060 > has Compute SM 8.6 ]
[hevc_nvenc @ 0x0] [ GPU #1 - < NVIDIA RTX A4000 > has Compute SM 8.6 ]"#;
        let d = parse_gpu_list(sample).unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].index, 0);
        assert!(d[0].name.contains("3060"));
        assert_eq!(d[1].index, 1);
    }
}
