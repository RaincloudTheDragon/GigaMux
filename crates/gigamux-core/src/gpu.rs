//! Local NVENC/QSV capability probe: device list, SM → generation, AV1/QSV smokes.

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

/// NVENC hardware generation (Wikipedia NVENC#Versions numbering).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NvencGeneration {
    /// Unmapped SM / name — still usable for encode when listed.
    Unknown,
    /// 1st gen (Kepler).
    Kepler,
    /// 2nd gen (1st Maxwell).
    Maxwell1,
    /// 3rd gen (Maxwell 2 / GM20x).
    Maxwell2,
    /// 4th gen (Pascal).
    Pascal,
    /// 5th gen (Volta).
    Volta,
    /// 6th gen (Turing).
    Turing,
    /// 7th gen (Ampere).
    Ampere,
    /// 8th gen (Ada Lovelace).
    Ada,
    /// 9th gen (Blackwell).
    Blackwell,
}

impl NvencGeneration {
    /// Short UI label, e.g. `NVENC 7 (Ampere)`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "NVENC (?)",
            Self::Kepler => "NVENC 1 (Kepler)",
            Self::Maxwell1 => "NVENC 2 (Maxwell)",
            Self::Maxwell2 => "NVENC 3 (Maxwell 2)",
            Self::Pascal => "NVENC 4 (Pascal)",
            Self::Volta => "NVENC 5 (Volta)",
            Self::Turing => "NVENC 6 (Turing)",
            Self::Ampere => "NVENC 7 (Ampere)",
            Self::Ada => "NVENC 8 (Ada)",
            Self::Blackwell => "NVENC 9 (Blackwell)",
        }
    }

    /// ASCII token for container metadata (e.g. `NVENC7_Ampere`).
    pub fn meta_token(self) -> &'static str {
        match self {
            Self::Unknown => "NVENC_Unknown",
            Self::Kepler => "NVENC1_Kepler",
            Self::Maxwell1 => "NVENC2_Maxwell",
            Self::Maxwell2 => "NVENC3_Maxwell2",
            Self::Pascal => "NVENC4_Pascal",
            Self::Volta => "NVENC5_Volta",
            Self::Turing => "NVENC6_Turing",
            Self::Ampere => "NVENC7_Ampere",
            Self::Ada => "NVENC8_Ada",
            Self::Blackwell => "NVENC9_Blackwell",
        }
    }

    /// Flamenco worker tag for this generation (`nvenc-ada`), or `None` for Unknown.
    pub fn farm_tag_slug(self) -> Option<&'static str> {
        match self {
            Self::Unknown => None,
            Self::Kepler => Some("nvenc-kepler"),
            Self::Maxwell1 => Some("nvenc-maxwell"),
            Self::Maxwell2 => Some("nvenc-maxwell2"),
            Self::Pascal => Some("nvenc-pascal"),
            Self::Volta => Some("nvenc-volta"),
            Self::Turing => Some("nvenc-turing"),
            Self::Ampere => Some("nvenc-ampere"),
            Self::Ada => Some("nvenc-ada"),
            Self::Blackwell => Some("nvenc-blackwell"),
        }
    }

    /// Parse a farm tag slug (`nvenc-ada` or bare `ada`) back to a generation.
    pub fn from_farm_tag_slug(s: &str) -> Option<Self> {
        let t = s.trim().to_ascii_lowercase();
        let t = t.strip_prefix("nvenc-").unwrap_or(&t);
        match t {
            "kepler" => Some(Self::Kepler),
            "maxwell" | "maxwell1" => Some(Self::Maxwell1),
            "maxwell2" => Some(Self::Maxwell2),
            "pascal" => Some(Self::Pascal),
            "volta" => Some(Self::Volta),
            "turing" => Some(Self::Turing),
            "ampere" => Some(Self::Ampere),
            "ada" => Some(Self::Ada),
            "blackwell" => Some(Self::Blackwell),
            _ => None,
        }
    }

    /// Consumer NVENC AV1 encode (Ada+). Used for farm HW mute when gen is pinned without a local smoke.
    pub fn supports_av1_nvenc(self) -> bool {
        matches!(self, Self::Ada | Self::Blackwell)
    }
}

/// Map Compute SM + marketing name → NVENC generation.
pub fn nvenc_generation_from_sm_name(sm_major: u32, sm_minor: u32, name: &str) -> NvencGeneration {
    let sm = sm_major * 10 + sm_minor;
    let lower = name.to_ascii_lowercase();
    // Prefer SM when present; name only breaks SM ties / fills Unknown.
    match sm_major {
        0 if sm == 0 => generation_from_name(&lower),
        // Kepler GK10x/GK110 — first NVENC generation.
        3 => NvencGeneration::Kepler,
        5 => {
            // SM 5.0/5.2 Maxwell1; GM20x (960/970/980/Titan X) → Maxwell2.
            if sm >= 52
                || lower.contains("960")
                || lower.contains("970")
                || lower.contains("980")
                || lower.contains("titan x")
            {
                NvencGeneration::Maxwell2
            } else {
                NvencGeneration::Maxwell1
            }
        }
        6 => NvencGeneration::Pascal,
        7 if sm_minor >= 5 => NvencGeneration::Turing,
        7 => NvencGeneration::Volta,
        8 if sm_minor >= 9 => NvencGeneration::Ada,
        8 => NvencGeneration::Ampere,
        9 => {
            // Hopper SM 9.0 is datacenter; consumer Blackwell often SM 10+/12.
            if generation_from_name(&lower) == NvencGeneration::Blackwell {
                NvencGeneration::Blackwell
            } else {
                NvencGeneration::Unknown
            }
        }
        10 | 12 => NvencGeneration::Blackwell,
        _ => generation_from_name(&lower),
    }
}

fn generation_from_name(lower: &str) -> NvencGeneration {
    if lower.contains("rtx 50")
        || lower.contains("rtx50")
        || lower.contains("5090")
        || lower.contains("5080")
        || lower.contains("5070")
        || lower.contains("5060")
        || lower.contains("blackwell")
    {
        NvencGeneration::Blackwell
    } else if lower.contains("rtx 40")
        || lower.contains("rtx40")
        || lower.contains("4090")
        || lower.contains("4080")
        || lower.contains("4070")
        || lower.contains("4060")
        || lower.contains("ada")
    {
        NvencGeneration::Ada
    } else if lower.contains("rtx 30")
        || lower.contains("rtx30")
        || lower.contains("3090")
        || lower.contains("3080")
        || lower.contains("3070")
        || lower.contains("3060")
        || lower.contains("3050")
        || lower.contains("a4000")
        || lower.contains("a5000")
        || lower.contains("a6000")
        || lower.contains("ampere")
    {
        NvencGeneration::Ampere
    } else if lower.contains("rtx 20")
        || lower.contains("rtx20")
        || lower.contains("gtx 16")
        || lower.contains("1660")
        || lower.contains("1650")
        || lower.contains("turing")
        || lower.contains("quadro rtx")
    {
        NvencGeneration::Turing
    } else if lower.contains("titan v") || lower.contains("volta") {
        NvencGeneration::Volta
    } else if lower.contains("gtx 10")
        || lower.contains("1080")
        || lower.contains("1070")
        || lower.contains("1060")
        || lower.contains("1050")
        || lower.contains("pascal")
    {
        NvencGeneration::Pascal
    } else if lower.contains("980")
        || lower.contains("970")
        || lower.contains("960")
        || lower.contains("maxwell")
    {
        NvencGeneration::Maxwell2
    } else if lower.contains("750 ti") || lower.contains("750ti") || lower.contains("gtx 750") {
        // GM107 — 2nd-gen NVENC (Maxwell), not Kepler.
        NvencGeneration::Maxwell1
    } else if lower.contains("kepler")
        || lower.contains("gtx 780")
        || lower.contains("gtx 770")
        || lower.contains("gtx 760")
        || lower.contains("gtx 680")
        || lower.contains("gtx 670")
        || lower.contains("gtx 660")
        || lower.contains("gtx 650")
        || lower.contains("gtx titan")
        || lower.contains("quadro k")
    {
        NvencGeneration::Kepler
    } else {
        NvencGeneration::Unknown
    }
}

/// One NVENC-capable device from `ffmpeg -gpu list` (+ SM / generation / AV1).
#[derive(Debug, Clone)]
pub struct NvencDevice {
    pub index: u32,
    pub name: String,
    /// Compute capability major (0 if unparsed).
    pub sm_major: u32,
    /// Compute capability minor (0 if unparsed).
    pub sm_minor: u32,
    pub generation: NvencGeneration,
    /// True when a one-frame `av1_nvenc` smoke on this index succeeds.
    pub av1: bool,
}

impl NvencDevice {
    /// Combo / footer label: `#0 · NVENC 7 (Ampere) · RTX 3060`.
    pub fn ui_label(&self) -> String {
        let short = short_gpu_name(&self.name);
        format!("#{} · {} · {short}", self.index, self.generation.label())
    }

    /// Metadata token: `NVENC7_Ampere_GPU0_RTX_3060`.
    pub fn meta_hw_tag(&self) -> String {
        let name = sanitize_meta_token(&short_gpu_name(&self.name));
        format!("{}_GPU{}_{}", self.generation.meta_token(), self.index, name)
    }
}

/// Strip common vendor prefixes for compact UI.
fn short_gpu_name(name: &str) -> String {
    let t = name
        .trim()
        .trim_start_matches("NVIDIA ")
        .trim_start_matches("GeForce ")
        .trim()
        .to_string();
    if t.is_empty() {
        name.trim().to_string()
    } else {
        t
    }
}

/// ASCII `[A-Za-z0-9]+` with underscores for metadata values.
pub fn sanitize_meta_token(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if c == ' ' || c == '-' || c == '_' || c == '.' {
            if !out.ends_with('_') {
                out.push('_');
            }
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        "GPU".into()
    } else {
        out
    }
}

/// Detected Intel QSV path (VP9 smoke + optional adapter name).
#[derive(Debug, Clone)]
pub struct QsvDevice {
    pub name: String,
    /// Coarse label for UI (e.g. `QSV (Intel UHD Graphics 770)`).
    pub label: String,
}

impl QsvDevice {
    pub fn meta_hw_tag(&self) -> String {
        format!("QSV_{}", sanitize_meta_token(&self.name))
    }
}

/// Snapshot of local hardware encoders after probe.
#[derive(Debug, Clone, Default)]
pub struct HwCatalog {
    pub nvenc: Vec<NvencDevice>,
    pub qsv: Option<QsvDevice>,
}

impl HwCatalog {
    /// One-line inventory for the session log (never persisted as truth).
    pub fn summary_line(&self) -> String {
        let mut parts = Vec::new();
        for d in &self.nvenc {
            let av1 = if d.av1 { "+AV1" } else { "" };
            parts.push(format!("{}{av1}", d.ui_label()));
        }
        if let Some(q) = &self.qsv {
            parts.push(q.label.clone());
        }
        if parts.is_empty() {
            "none (CPU only)".into()
        } else {
            parts.join(" · ")
        }
    }

    pub fn nvenc_av1(&self) -> Vec<&NvencDevice> {
        self.nvenc.iter().filter(|d| d.av1).collect()
    }

    /// Resolve which NVENC device `choice` maps to (Any → first, or None).
    pub fn resolve_nvenc(&self, choice: GpuChoice) -> Option<&NvencDevice> {
        match choice {
            GpuChoice::Any => self.nvenc.first(),
            GpuChoice::Index(i) => self.nvenc.iter().find(|d| d.index == i),
        }
    }

    /// First local device matching `gen`, if any.
    pub fn first_nvenc_of_gen(&self, gen: NvencGeneration) -> Option<&NvencDevice> {
        self.nvenc.iter().find(|d| d.generation == gen)
    }

    /// Prefer a local device of `nvenc_gen` when set; otherwise keep `gpu`.
    pub fn resolve_gpu_for_slot(
        &self,
        nvenc_gen: Option<NvencGeneration>,
        gpu: GpuChoice,
    ) -> GpuChoice {
        if let Some(gen) = nvenc_gen {
            if let Some(d) = self.first_nvenc_of_gen(gen) {
                return GpuChoice::Index(d.index);
            }
        }
        gpu
    }

    /// Unique NVENC generations present on this machine (stable order by device index).
    pub fn local_nvenc_gens(&self) -> Vec<NvencGeneration> {
        let mut out = Vec::new();
        for d in &self.nvenc {
            if d.generation != NvencGeneration::Unknown && !out.contains(&d.generation) {
                out.push(d.generation);
            }
        }
        out
    }

    /// True when `choice` can open `av1_nvenc`.
    pub fn av1_ok_for(&self, choice: GpuChoice) -> bool {
        match choice {
            GpuChoice::Any => self.nvenc.iter().any(|d| d.av1),
            GpuChoice::Index(i) => self.nvenc.iter().any(|d| d.index == i && d.av1),
        }
    }

    /// `GigaMux_hw` value for an NVENC encode with this choice (or software-ish fallback).
    pub fn nvenc_meta_hw(&self, choice: GpuChoice) -> String {
        match self.resolve_nvenc(choice) {
            Some(d) => d.meta_hw_tag(),
            None => "NVENC_Any".into(),
        }
    }

    /// Farm metadata when a generation is pinned (no local device required).
    pub fn nvenc_meta_hw_for_gen(gen: NvencGeneration) -> String {
        format!("{}_Farm", gen.meta_token())
    }
}

/// Full local probe: NVENC list + SM/gen + AV1 smoke + QSV.
pub fn probe_local_encoders() -> HwCatalog {
    let mut nvenc = match list_nvenc_devices() {
        Ok(d) => d,
        Err(_) => Vec::new(),
    };
    for d in &mut nvenc {
        d.av1 = nvenc_encoder_works("av1_nvenc", Some(d.index));
    }
    let qsv = if qsv_vp9_available() {
        let name = probe_qsv_adapter_name().unwrap_or_else(|| "Intel Quick Sync".into());
        Some(QsvDevice {
            label: format!("QSV ({name})"),
            name,
        })
    } else {
        None
    };
    HwCatalog { nvenc, qsv }
}

/// Probe NVENC devices via a short `hevc_nvenc` encode with `-gpu list`.
pub fn list_nvenc_devices() -> Result<Vec<NvencDevice>, String> {
    list_nvenc_devices_for("hevc_nvenc")
}

/// Devices reported by ffmpeg `-gpu list` for an NVENC encoder (inventory + SM/generation).
/// Note: `av1_nvenc -gpu list` still lists pre-Ada cards — set [`NvencDevice::av1`] via smoke.
pub fn list_nvenc_devices_for(encoder: &str) -> Result<Vec<NvencDevice>, String> {
    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-f",
            "lavfi",
            "-i",
            "nullsrc=s=64x64:d=0.1",
            "-c:v",
            encoder,
            "-gpu",
            "list",
            "-f",
            "null",
            "-",
        ])
        .output()
        .map_err(|e| format!("ffmpeg not found or failed to start: {e}"))?;

    // Device lines land on stderr (list mode often exits non-zero; parse anyway).
    let text = String::from_utf8_lossy(&output.stderr);
    parse_gpu_list(&text)
}

/// One-frame encode smoke test. Needed for AV1: `-gpu list` prints GPUs that cannot encode.
pub fn nvenc_encoder_works(encoder: &str, gpu: Option<u32>) -> bool {
    let gpu_arg = match gpu {
        Some(i) => i.to_string(),
        None => "any".to_string(),
    };
    Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "nullsrc=s=256x256:d=0.1",
            "-frames:v",
            "1",
            "-c:v",
            encoder,
            "-gpu",
            &gpu_arg,
            "-f",
            "null",
            "-",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// GPUs from `hevc_devices` that actually open `av1_nvenc` (Ada / 40-series+).
pub fn list_av1_nvenc_devices(hevc_devices: &[NvencDevice]) -> Vec<NvencDevice> {
    hevc_devices
        .iter()
        .filter(|d| {
            if d.av1 {
                true
            } else {
                nvenc_encoder_works("av1_nvenc", Some(d.index))
            }
        })
        .map(|d| {
            let mut c = d.clone();
            c.av1 = true;
            c
        })
        .collect()
}

/// True when `av1_nvenc` can encode on any GPU.
pub fn av1_nvenc_available() -> bool {
    nvenc_encoder_works("av1_nvenc", None)
}

/// True when Intel QSV can open `vp9_qsv` (false on AMD / Intel F / no iGPU / missing ffmpeg QSV).
pub fn qsv_vp9_available() -> bool {
    Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-init_hw_device",
            "qsv=hw",
            "-f",
            "lavfi",
            "-i",
            "nullsrc=s=256x256:d=0.1",
            "-frames:v",
            "1",
            "-c:v",
            "vp9_qsv",
            "-pix_fmt",
            "nv12",
            "-f",
            "null",
            "-",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Best-effort Intel display adapter name (Windows WMI); `None` elsewhere / on failure.
fn probe_qsv_adapter_name() -> Option<String> {
    #[cfg(windows)]
    {
        let output = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "(Get-CimInstance Win32_VideoController | Where-Object { $_.Name -match 'Intel' } | Select-Object -First 1 -ExpandProperty Name)",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if name.is_empty() {
            None
        } else {
            Some(name)
        }
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn parse_gpu_list(stderr: &str) -> Result<Vec<NvencDevice>, String> {
    // e.g. [h264_nvenc @ ...] [ GPU #0 - < NVIDIA GeForce RTX 3060 > has Compute SM 8.6 ]
    let re = Regex::new(
        r"GPU #(\d+)\s*-\s*<\s*([^>]+?)\s*>(?:\s*has Compute SM\s+(\d+)\.(\d+))?",
    )
    .map_err(|e| format!("gpu list regex: {e}"))?;
    let mut devices = Vec::new();
    for cap in re.captures_iter(stderr) {
        let index: u32 = cap[1].parse().map_err(|e| format!("gpu index parse: {e}"))?;
        let name = cap[2].trim().to_string();
        let sm_major: u32 = cap
            .get(3)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let sm_minor: u32 = cap
            .get(4)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let generation = nvenc_generation_from_sm_name(sm_major, sm_minor, &name);
        if !devices.iter().any(|d: &NvencDevice| d.index == index) {
            devices.push(NvencDevice {
                index,
                name,
                sm_major,
                sm_minor,
                generation,
                av1: false,
            });
        }
    }
    devices.sort_by_key(|d| d.index);
    Ok(devices)
}

/// Workspace crate version (`0.1.0`) plus optional `+` short git SHA from [`build.rs`](../../build.rs).
pub fn gigamux_version_payload() -> String {
    let ver = env!("CARGO_PKG_VERSION");
    let sha = env!("GIGAMUX_GIT_SHA");
    if sha.is_empty() {
        ver.to_string()
    } else {
        format!("{ver}+{sha}")
    }
}

/// OBS-style label, e.g. `GigaMux (0.1.0+abcdef12)`.
pub fn gigamux_version_label() -> String {
    format!("GigaMux ({})", gigamux_version_payload())
}

/// Build metadata pairs stamped into outputs when Write encoder metadata is on.
pub fn encoder_metadata_tags(
    encoder_name: &str,
    hw: Option<&str>,
) -> Vec<(String, String)> {
    let payload = gigamux_version_payload();
    let label = gigamux_version_label();
    let mut tags = vec![
        ("GigaMux_encoder".into(), sanitize_meta_token(encoder_name)),
        // Keep `0.1.0+abcdef12` literal (do not run through sanitize_meta_token — it mangles `+` / `.`).
        ("GigaMux_version".into(), payload),
        // OBS-style; some muxers still overwrite with Lavf — UI prefers GigaMux_* on probe.
        ("encoder".into(), label),
    ];
    if let Some(h) = hw {
        // `hw` is already built from sanitize_meta_token pieces.
        tags.push(("GigaMux_hw".into(), h.to_string()));
    }
    tags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sample_line_with_sm() {
        let sample = r#"[hevc_nvenc @ 0x0] [ GPU #0 - < NVIDIA GeForce RTX 3060 > has Compute SM 8.6 ]
[hevc_nvenc @ 0x0] [ GPU #1 - < NVIDIA GeForce RTX 4080 > has Compute SM 8.9 ]"#;
        let d = parse_gpu_list(sample).unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].index, 0);
        assert!(d[0].name.contains("3060"));
        assert_eq!(d[0].sm_major, 8);
        assert_eq!(d[0].sm_minor, 6);
        assert_eq!(d[0].generation, NvencGeneration::Ampere);
        assert_eq!(d[1].generation, NvencGeneration::Ada);
    }

    #[test]
    fn generation_from_sm_and_name() {
        assert_eq!(
            nvenc_generation_from_sm_name(8, 6, "NVIDIA GeForce RTX 3060"),
            NvencGeneration::Ampere
        );
        assert_eq!(
            nvenc_generation_from_sm_name(8, 9, "RTX 4080"),
            NvencGeneration::Ada
        );
        assert_eq!(
            nvenc_generation_from_sm_name(7, 5, "RTX 2080"),
            NvencGeneration::Turing
        );
        assert_eq!(
            nvenc_generation_from_sm_name(0, 0, "GeForce RTX 4090"),
            NvencGeneration::Ada
        );
        assert_eq!(
            nvenc_generation_from_sm_name(3, 5, "NVIDIA GeForce GTX 780"),
            NvencGeneration::Kepler
        );
        assert_eq!(
            nvenc_generation_from_sm_name(0, 0, "GeForce GTX 680"),
            NvencGeneration::Kepler
        );
    }

    #[test]
    fn version_label_shape() {
        let payload = gigamux_version_payload();
        let label = gigamux_version_label();
        assert!(label.starts_with("GigaMux ("));
        assert!(label.ends_with(')'));
        assert!(label.contains(env!("CARGO_PKG_VERSION")));
        // Pre-release builds usually embed SHA; empty only if git was unavailable at compile.
        if !env!("GIGAMUX_GIT_SHA").is_empty() {
            assert!(payload.contains('+'));
            assert_eq!(env!("GIGAMUX_GIT_SHA").len(), 8);
        }
        assert_eq!(label, format!("GigaMux ({payload})"));
    }

    #[test]
    fn sanitize_meta() {
        assert_eq!(sanitize_meta_token("RTX 3060"), "RTX_3060");
        assert_eq!(sanitize_meta_token("hevc_nvenc"), "hevc_nvenc");
    }

    #[test]
    fn meta_hw_tag_shape() {
        let d = NvencDevice {
            index: 0,
            name: "NVIDIA GeForce RTX 3060".into(),
            sm_major: 8,
            sm_minor: 6,
            generation: NvencGeneration::Ampere,
            av1: false,
        };
        let t = d.meta_hw_tag();
        assert!(t.contains("NVENC7_Ampere"));
        assert!(t.contains("GPU0"));
        assert!(t.contains("RTX_3060") || t.contains("3060"));
    }
}
