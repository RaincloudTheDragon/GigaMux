//! Flamenco connector: UPnP/SSDP Manager discovery, `GigaMux.js` deploy, worker tags, job submit.

use std::collections::HashSet;
use std::fs;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::prefs::EncodeSlot;
use crate::targets::{AlphaVideo, OutContainer, OpaqueVideo, OutputTargets};

/// Bundled job compiler (deploy into Manager `scripts/`).
pub const GIGAMUX_JS: &str = include_str!("../../../flamenco/GigaMux.js");

/// Same ST / device id Flamenco workers use (`internal/upnp_ssdp`).
const SSDP_ST: &str = "urn:flamenco:manager:0";
const SSDP_UUID: &str = "aa80bc5f-d0af-46b8-8630-23bd7e80ec4d";
const SSDP_ADDR: &str = "239.255.255.250:1900";

/// Coarse Flamenco pools (always Ensure'd). Gen tags (`nvenc-ada`, …) are created only after farm probe.
pub const REQUIRED_WORKER_TAGS: &[(&str, &str)] = &[
    ("nvenc", "NVENC-capable ffmpeg (opaque hardware encode; any generation)"),
    ("qsv", "QSV-capable ffmpeg (VP9 QSV opaque / hybrid color)"),
    ("vmaf", "ffmpeg build with libvmaf"),
    ("cpu", "Software / libvpx alpha chunks and remux tooling"),
];

/// Description for a probe-gated `nvenc-{gen}` tag.
pub fn nvenc_gen_tag_description(slug: &str) -> String {
    use crate::gpu::NvencGeneration;
    match NvencGeneration::from_farm_tag_slug(slug) {
        Some(g) => format!("{} — pin opaque NVENC jobs to this generation", g.label()),
        None => format!("NVENC generation pool ({slug})"),
    }
}

/// Flamenco worker tag name derived from a slot (not the legacy global prefs tag).
///
/// Alpha / software → `cpu`; HW opaque NVENC → `nvenc` or `nvenc-{gen}`; HW opaque VP9 → `qsv`.
pub fn farm_worker_tag_for_slot(slot: &EncodeSlot, has_alpha: bool) -> String {
    if has_alpha || !slot.use_hardware {
        return "cpu".into();
    }
    if slot.opaque_video.is_nvenc() {
        return match slot.nvenc_gen {
            Some(g) => g
                .farm_tag_slug()
                .unwrap_or("nvenc")
                .to_string(),
            None => "nvenc".into(),
        };
    }
    if slot.opaque_video.is_qsv() {
        return "qsv".into();
    }
    "cpu".into()
}

/// Whether farm encode argv should use HW for this slot (gen-aware; ignores sender-local probe).
pub fn farm_slot_use_hardware(slot: &EncodeSlot) -> bool {
    if !slot.use_hardware {
        return false;
    }
    match slot.opaque_video {
        OpaqueVideo::Vp8 => false,
        OpaqueVideo::Av1 => match slot.nvenc_gen {
            // Pinned gen without AV1 NVENC → CPU on farm.
            Some(g) => g.supports_av1_nvenc(),
            // Coarse `nvenc` pool may include Ada+; keep HW.
            None => true,
        },
        OpaqueVideo::Vp9 => true,
        OpaqueVideo::Hevc | OpaqueVideo::H264 => true,
    }
}

/// Body for `POST /api/v3/jobs` (Flamenco `SubmittedJob`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmittedJob {
    pub name: String,
    #[serde(rename = "type")]
    pub job_type: String,
    pub submitter_platform: String,
    pub priority: i32,
    pub settings: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_tag: Option<String>,
}

fn flamenco_exe_and_tag(
    ffmpeg_exe: Option<&str>,
    worker_tag: Option<&str>,
) -> (String, Option<String>) {
    let exe = ffmpeg_exe
        .filter(|s| !s.is_empty())
        .unwrap_or("ffmpeg")
        .to_string();
    let tag = worker_tag
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    (exe, tag)
}

/// How `GigaMux.js` should author `exec` for Cancel (Windows orphan workaround).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelExecMode {
    /// Direct `ffmpeg` exec — needs Worker kill PR (3.10-beta+ / matched Workers).
    Direct,
    /// Manager ≤3.9: helper wrap not shipped yet; same exec + submitter/JS warn.
    LegacyStub,
}

impl CancelExecMode {
    /// Setting value written into the Flamenco job (`cancel_exec`).
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::LegacyStub => "legacy_stub",
        }
    }
}

/// Pick cancel_exec from Manager `/api/v3/version` (or shortversion) text.
/// `3.10-beta*` and ≥3.10 → Direct; ≤3.9 → LegacyStub; unparseable → LegacyStub (warn path).
pub fn cancel_exec_mode_for_manager_version(version: &str) -> CancelExecMode {
    let lower = version.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return CancelExecMode::LegacyStub;
    }
    // Explicit beta builds ship (or expect) the Worker process-tree kill PR.
    if lower.contains("beta") {
        return CancelExecMode::Direct;
    }
    let parts: Vec<u32> = lower
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .take(2)
        .filter_map(|s| s.parse().ok())
        .collect();
    match parts.as_slice() {
        [maj, min, ..] if *maj > 3 || (*maj == 3 && *min >= 10) => CancelExecMode::Direct,
        [maj, min, ..] if *maj < 3 || (*maj == 3 && *min <= 9) => CancelExecMode::LegacyStub,
        [maj] if *maj > 3 => CancelExecMode::Direct,
        [maj] if *maj <= 3 => CancelExecMode::LegacyStub,
        _ => CancelExecMode::LegacyStub,
    }
}

/// Default worker binary name for hybrid integrity (`PATH` / Flamenco `tools/`).
pub const DEFAULT_CHECK_EXE: &str = "gigamux-check";

/// One chunk path + expected duration for same-task trailing healthcheck.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FarmChunkCheck {
    pub path: String,
    pub expect_dur: f64,
}

/// Final input→output integrity payload for `gigamux-check final`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FarmFinalCheck {
    pub input: String,
    pub output: String,
    pub opaque_video: OpaqueVideo,
    pub opaque_container: OutContainer,
    pub alpha_video: AlphaVideo,
    pub alpha_container: OutContainer,
}

impl FarmFinalCheck {
    pub fn targets(&self) -> OutputTargets {
        OutputTargets {
            opaque_video: self.opaque_video,
            opaque_container: self.opaque_container,
            alpha_video: self.alpha_video,
            alpha_container: self.alpha_container,
        }
    }
}

/// Delete farm intermediates after a successful final check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FarmCleanup {
    pub dir: String,
    pub stem: String,
    pub ext: String,
    /// Chunk count (`0` = no `*.cNNNofMMM.*` files).
    pub chunks: u32,
}

/// Farm VMAF mode on the Flamenco send dialog (not a persisted pref). Default off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FarmVmafMode {
    /// No farm VMAF tasks / follow-up jobs.
    #[default]
    Off,
    /// Trailing `vmaf` task on the encode job (same worker tag; those workers need libvmaf).
    Attached,
    /// After encode completes, submit a separate VMAF-only job tagged `vmaf`.
    Separate,
}

/// One farm VMAF pair for `gigamux-check ensure-vmaf` (attached or VMAF-only job).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FarmVmafSpec {
    pub input: String,
    pub output: String,
    pub sidecar: String,
}

/// Integrity settings embedded in a GigaMux Flamenco job (hybrid model).
#[derive(Debug, Clone)]
pub struct FlamencoIntegrity {
    /// Worker exe name/path (`gigamux-check` by default).
    pub check_exe: String,
    /// Parallel to encode tasks (empty when not chunking).
    pub chunk_checks: Vec<FarmChunkCheck>,
    pub final_check: Option<FarmFinalCheck>,
    /// Only run after successful final check (chunked jobs).
    pub cleanup: Option<FarmCleanup>,
    /// Trailing VMAF on the encode job (after final-check).
    pub vmaf: Option<FarmVmafSpec>,
}

impl Default for FlamencoIntegrity {
    fn default() -> Self {
        Self {
            check_exe: DEFAULT_CHECK_EXE.into(),
            chunk_checks: Vec::new(),
            final_check: None,
            cleanup: None,
            vmaf: None,
        }
    }
}

fn rewrite_integrity_paths(mut integ: FlamencoIntegrity, tw: Option<FlamencoTwoway<'_>>) -> FlamencoIntegrity {
    // Absolute check_exe (Manager tools/ on shared storage) must brace like argv paths.
    integ.check_exe = rewrite_path_arg(&integ.check_exe, tw);
    for c in &mut integ.chunk_checks {
        c.path = rewrite_path_arg(&c.path, tw);
    }
    if let Some(f) = integ.final_check.as_mut() {
        f.input = rewrite_path_arg(&f.input, tw);
        f.output = rewrite_path_arg(&f.output, tw);
    }
    if let Some(c) = integ.cleanup.as_mut() {
        c.dir = rewrite_path_arg(&c.dir, tw);
    }
    if let Some(v) = integ.vmaf.as_mut() {
        v.input = rewrite_path_arg(&v.input, tw);
        v.output = rewrite_path_arg(&v.output, tw);
        v.sidecar = rewrite_path_arg(&v.sidecar, tw);
    }
    integ
}

/// Strip a trailing `.exe` so one path works on Windows (CreateProcess) and Linux.
pub fn normalize_check_exe_path(path: &str) -> String {
    let t = path.trim();
    if t.is_empty() {
        return String::new();
    }
    t.strip_suffix(".exe")
        .or_else(|| t.strip_suffix(".EXE"))
        .unwrap_or(t)
        .to_string()
}

/// `scripts/` or Manager install from last Deploy → `{manager}/tools/gigamux-check` (no `.exe`; Windows CreateProcess adds it).
///
/// Empty / unknown → bare `gigamux-check` (PATH fallback).
pub fn check_exe_from_deploy_scripts_dir(scripts_or_manager: &str) -> String {
    let t = scripts_or_manager.trim();
    if t.is_empty() {
        return DEFAULT_CHECK_EXE.to_string();
    }
    let p = Path::new(t);
    let manager = if p.file_name().and_then(|n| n.to_str()) == Some("scripts") {
        p.parent().unwrap_or(p)
    } else if p.join("flamenco-manager.yaml").is_file() || p.join("tools").is_dir() {
        p
    } else if p.join("GigaMux.js").is_file() {
        // Bare scripts dir without being named "scripts".
        p.parent().unwrap_or(p)
    } else {
        p
    };
    manager
        .join("tools")
        .join(DEFAULT_CHECK_EXE)
        .to_string_lossy()
        .into_owned()
}

/// Prefer explicit worker-reachable helper path; else Deploy-derived Manager `tools/`; else PATH name.
pub fn resolve_flamenco_check_exe(override_path: &str, scripts_or_manager: &str) -> String {
    let over = normalize_check_exe_path(override_path);
    if !over.is_empty() {
        return over;
    }
    check_exe_from_deploy_scripts_dir(scripts_or_manager)
}

fn rewrite_path_arg(arg: &str, tw: Option<FlamencoTwoway<'_>>) -> String {
    match tw {
        Some(tw) => rewrite_flamenco_twoway_path(arg, tw),
        None => arg.to_string(),
    }
}

fn integrity_settings_json(
    integ: &FlamencoIntegrity,
) -> Result<(String, String, String, String, String), String> {
    let check_exe = {
        let t = integ.check_exe.trim();
        if t.is_empty() {
            DEFAULT_CHECK_EXE.to_string()
        } else {
            t.to_string()
        }
    };
    let chunk_checks_json = if integ.chunk_checks.is_empty() {
        String::new()
    } else {
        serde_json::to_string(&integ.chunk_checks)
            .map_err(|e| format!("serialize chunk_checks: {e}"))?
    };
    let final_check_json = match &integ.final_check {
        Some(f) => serde_json::to_string(f).map_err(|e| format!("serialize final_check: {e}"))?,
        None => String::new(),
    };
    let cleanup_json = match &integ.cleanup {
        Some(c) => serde_json::to_string(c).map_err(|e| format!("serialize cleanup: {e}"))?,
        None => String::new(),
    };
    let vmaf_json = match &integ.vmaf {
        Some(v) => serde_json::to_string(v).map_err(|e| format!("serialize vmaf: {e}"))?,
        None => String::new(),
    };
    Ok((
        check_exe,
        chunk_checks_json,
        final_check_json,
        cleanup_json,
        vmaf_json,
    ))
}

/// Optional two-way path rewrite for argv (Flamenco only HasPrefix-replaces whole setting strings).
#[derive(Debug, Clone, Copy)]
pub struct FlamencoTwoway<'a> {
    /// Local shared root matching Manager's Windows/Linux two-way value (e.g. `Z:\` or `/mnt/shared`).
    pub local_prefix: &'a str,
    /// Variable name on the Manager (usually `shared_storage_path`).
    pub var_name: &'a str,
}

/// Infer a Windows drive-root prefix (`Z:\`) from a path when prefs left the shared prefix empty.
pub fn infer_twoway_local_prefix(path: &Path) -> Option<String> {
    let s = path.to_string_lossy();
    let b = s.as_bytes();
    // `X:\` or `X:/…`
    if b.len() >= 3 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') {
        let drive = (b[0] as char).to_ascii_uppercase();
        if drive.is_ascii_alphabetic() {
            return Some(format!("{drive}:\\"));
        }
    }
    None
}

/// Rewrite an absolute path under `local_prefix` to `{var_name}/rest` (forward slashes).
/// Flamenco Manager only auto-rewrites settings whose *entire* value is a path prefix; GigaMux argv JSON must brace paths itself so workers get OS-native roots on expand.
pub fn rewrite_flamenco_twoway_path(arg: &str, tw: FlamencoTwoway<'_>) -> String {
    let prefix = tw.local_prefix.trim();
    let var = tw.var_name.trim();
    if prefix.is_empty() || var.is_empty() || arg.is_empty() {
        return arg.to_string();
    }
    let norm_arg = arg.replace('\\', "/");
    let norm_prefix = prefix.replace('\\', "/");
    let norm_prefix = norm_prefix.trim_end_matches('/');
    if norm_prefix.is_empty() {
        return arg.to_string();
    }
    let (arg_cmp, pref_cmp) = if cfg!(windows) {
        (norm_arg.to_ascii_lowercase(), norm_prefix.to_ascii_lowercase())
    } else {
        (norm_arg.clone(), norm_prefix.to_string())
    };
    if !arg_cmp.starts_with(&pref_cmp) {
        return arg.to_string();
    }
    let rest = &norm_arg[norm_prefix.len()..];
    // Require a path boundary so `W:` does not match `Weird\file`.
    if !rest.is_empty() && !rest.starts_with('/') {
        return arg.to_string();
    }
    format!("{{{var}}}{rest}")
}

fn rewrite_argv_twoway(argv: &[String], tw: Option<FlamencoTwoway<'_>>) -> Vec<String> {
    let Some(tw) = tw else {
        return argv.to_vec();
    };
    argv.iter()
        .map(|a| rewrite_flamenco_twoway_path(a, tw))
        .collect()
}

/// One-file encode job for Flamenco (`GigaMux.js` → worker `exec` ffmpeg).
pub fn make_gigamux_encode_job(
    name: impl Into<String>,
    ffmpeg_exe: Option<&str>,
    ffmpeg_argv: &[String],
    worker_tag: Option<&str>,
    twoway: Option<FlamencoTwoway<'_>>,
    cancel_exec: CancelExecMode,
    integrity: Option<FlamencoIntegrity>,
) -> Result<SubmittedJob, String> {
    if ffmpeg_argv.is_empty() {
        return Err("ffmpeg argv is empty".into());
    }
    // argv[0] is the logical program name; worker uses settings.ffmpeg_exe (or "ffmpeg").
    let (exe, tag) = flamenco_exe_and_tag(ffmpeg_exe, worker_tag);
    let argv = rewrite_argv_twoway(ffmpeg_argv, twoway);
    let argv_json =
        serde_json::to_string(&argv).map_err(|e| format!("serialize ffmpeg argv: {e}"))?;
    // Also set encode_argvs_json (length 1) so newer GigaMux.js prefers the array form.
    let encode_argvs_json =
        serde_json::to_string(&vec![&argv]).map_err(|e| format!("serialize encode_argvs: {e}"))?;
    let integ = rewrite_integrity_paths(integrity.unwrap_or_default(), twoway);
    let (check_exe, chunk_checks_json, final_check_json, cleanup_json, vmaf_json) =
        integrity_settings_json(&integ)?;
    let settings = json!({
        "ffmpeg_exe": exe,
        "ffmpeg_argv_json": argv_json,
        "encode_argvs_json": encode_argvs_json,
        "concat_argv_json": "",
        "mux_argv_json": "",
        "cancel_exec": cancel_exec.as_setting(),
        "check_exe": check_exe,
        "chunk_checks_json": chunk_checks_json,
        "final_check_json": final_check_json,
        "cleanup_json": cleanup_json,
        "vmaf_json": vmaf_json,
        "vmaf_pairs_json": "",
    });
    Ok(SubmittedJob {
        name: name.into(),
        job_type: "GigaMux".into(),
        submitter_platform: "windows".into(),
        priority: 50,
        settings,
        worker_tag: tag,
    })
}

/// Chunked encode job: N video-only encodes + stage mux + remux (GigaMux.js task graph).
pub fn make_gigamux_chunked_job(
    name: impl Into<String>,
    ffmpeg_exe: Option<&str>,
    encode_argvs: &[Vec<String>],
    mux_argv: &[String],
    mux_remux_argv: &[String],
    out_expect_dur_s: f64,
    worker_tag: Option<&str>,
    twoway: Option<FlamencoTwoway<'_>>,
    cancel_exec: CancelExecMode,
    integrity: Option<FlamencoIntegrity>,
) -> Result<SubmittedJob, String> {
    if encode_argvs.is_empty() {
        return Err("encode_argvs is empty".into());
    }
    if mux_argv.is_empty() || mux_remux_argv.is_empty() {
        return Err("chunked job requires mux_argv and mux_remux_argv".into());
    }
    if out_expect_dur_s <= 0.0 {
        return Err(format!("out_expect_dur_s must be > 0 (got {out_expect_dur_s})"));
    }
    let (exe, tag) = flamenco_exe_and_tag(ffmpeg_exe, worker_tag);
    let encode_rewritten: Vec<Vec<String>> = encode_argvs
        .iter()
        .map(|a| rewrite_argv_twoway(a, twoway))
        .collect();
    let mux_rewritten = rewrite_argv_twoway(mux_argv, twoway);
    let mux_remux_rewritten = rewrite_argv_twoway(mux_remux_argv, twoway);
    let encode_argvs_json = serde_json::to_string(&encode_rewritten)
        .map_err(|e| format!("serialize encode_argvs: {e}"))?;
    let mux_argv_json =
        serde_json::to_string(&mux_rewritten).map_err(|e| format!("serialize mux_argv: {e}"))?;
    let mux_remux_argv_json = serde_json::to_string(&mux_remux_rewritten)
        .map_err(|e| format!("serialize mux_remux_argv: {e}"))?;
    let integ = rewrite_integrity_paths(integrity.unwrap_or_default(), twoway);
    if !integ.chunk_checks.is_empty() && integ.chunk_checks.len() != encode_argvs.len() {
        return Err(format!(
            "chunk_checks length {} != encode_argvs {}",
            integ.chunk_checks.len(),
            encode_argvs.len()
        ));
    }
    let (check_exe, chunk_checks_json, final_check_json, cleanup_json, vmaf_json) =
        integrity_settings_json(&integ)?;
    // concat_argv_json stays empty — stage mux reads ffconcat directly (no `.vcat`).
    let settings = json!({
        "ffmpeg_exe": exe,
        "encode_argvs_json": encode_argvs_json,
        "concat_argv_json": "",
        "mux_argv_json": mux_argv_json,
        "mux_remux_argv_json": mux_remux_argv_json,
        "out_expect_dur": out_expect_dur_s,
        "ffmpeg_argv_json": "",
        "cancel_exec": cancel_exec.as_setting(),
        "check_exe": check_exe,
        "chunk_checks_json": chunk_checks_json,
        "final_check_json": final_check_json,
        "cleanup_json": cleanup_json,
        "vmaf_json": vmaf_json,
        "vmaf_pairs_json": "",
    });
    Ok(SubmittedJob {
        name: name.into(),
        job_type: "GigaMux".into(),
        submitter_platform: "windows".into(),
        priority: 50,
        settings,
        worker_tag: tag,
    })
}

/// VMAF-only Flamenco job (`vmaf` tag): one `ensure-vmaf` task per pair.
pub fn make_gigamux_vmaf_job(
    name: impl Into<String>,
    ffmpeg_exe: Option<&str>,
    pairs: &[FarmVmafSpec],
    worker_tag: Option<&str>,
    twoway: Option<FlamencoTwoway<'_>>,
    check_exe: &str,
) -> Result<SubmittedJob, String> {
    if pairs.is_empty() {
        return Err("vmaf pairs is empty".into());
    }
    let (exe, tag) = flamenco_exe_and_tag(ffmpeg_exe, worker_tag);
    let mut rewritten = Vec::with_capacity(pairs.len());
    for p in pairs {
        rewritten.push(FarmVmafSpec {
            input: rewrite_path_arg(&p.input, twoway),
            output: rewrite_path_arg(&p.output, twoway),
            sidecar: rewrite_path_arg(&p.sidecar, twoway),
        });
    }
    let pairs_json =
        serde_json::to_string(&rewritten).map_err(|e| format!("serialize vmaf_pairs: {e}"))?;
    let check = {
        let t = check_exe.trim();
        let raw = if t.is_empty() {
            DEFAULT_CHECK_EXE
        } else {
            t
        };
        rewrite_path_arg(raw, twoway)
    };
    let settings = json!({
        "ffmpeg_exe": exe,
        "ffmpeg_argv_json": "",
        "encode_argvs_json": "",
        "concat_argv_json": "",
        "mux_argv_json": "",
        "mux_remux_argv_json": "",
        "cancel_exec": CancelExecMode::Direct.as_setting(),
        "check_exe": check,
        "chunk_checks_json": "",
        "final_check_json": "",
        "cleanup_json": "",
        "vmaf_json": "",
        "vmaf_pairs_json": pairs_json,
    });
    Ok(SubmittedJob {
        name: name.into(),
        job_type: "GigaMux".into(),
        submitter_platform: "windows".into(),
        priority: 50,
        settings,
        worker_tag: tag,
    })
}

fn manager_base(url: &str) -> Result<String, String> {
    let base = url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("Flamenco Manager URL is empty".into());
    }
    Ok(base.to_string())
}

/// Read ureq response text; on HTTP error statuses ureq returns `Error::Status` with the body.
fn flamenco_http_text(result: Result<ureq::Response, ureq::Error>) -> Result<(u16, String), String> {
    match result {
        Ok(resp) => {
            let status = resp.status();
            let text = resp
                .into_string()
                .map_err(|e| format!("Flamenco response read: {e}"))?;
            Ok((status, text))
        }
        // ureq 2: non-2xx becomes Err(Status) — still extract the Manager message.
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            Ok((code, text))
        }
        Err(e) => Err(format!("Flamenco submit: {e}")),
    }
}

/// Resolve a worker tag **name or UUID** to the Manager tag UUID (required on submit).
pub fn resolve_worker_tag_uuid(manager_url: &str, tag: &str) -> Result<Option<String>, String> {
    let t = tag.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let tags = fetch_worker_tags(manager_url)?;
    if let Some((id, _)) = tags.iter().find(|(id, _)| id.eq_ignore_ascii_case(t)) {
        return Ok(Some(id.clone()));
    }
    if let Some((id, _)) = tags.iter().find(|(_, name)| name.eq_ignore_ascii_case(t)) {
        return Ok(Some(id.clone()));
    }
    Err(format!(
        "unknown worker tag {t:?} — run Ensure tags (and assign workers in Manager) first"
    ))
}

/// POST a job to Flamenco Manager. Returns the job id from the response when present.
///
/// Flamenco persists `worker_tag` as a **UUID** (`FetchWorkerTagByUUID`); names like `nvenc` must be resolved first or StoreAuthoredJob fails with HTTP 500.
pub fn submit_job(manager_url: &str, job: &SubmittedJob) -> Result<String, String> {
    let mut job = job.clone();
    if let Some(tag) = job.worker_tag.take() {
        job.worker_tag = resolve_worker_tag_uuid(manager_url, &tag)?;
    }
    let base = manager_base(manager_url)?;
    let url = format!("{base}/api/v3/jobs");
    let body = serde_json::to_vec(&job).map_err(|e| format!("serialize job: {e}"))?;
    let (status, text) = flamenco_http_text(
        ureq::post(&url)
            .set("Content-Type", "application/json")
            .send_bytes(&body),
    )?;
    if !(200..300).contains(&status) {
        // Truncate huge bodies but keep compileJob errors readable.
        let snippet: String = text.chars().take(500).collect();
        let hint = flamenco_script_hint(&snippet)
            .map(|h| format!(" — {h}"))
            .unwrap_or_default();
        return Err(format!("Flamenco HTTP {status}: {snippet}{hint}"));
    }
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let id = v
        .get("id")
        .or_else(|| v.get("uuid"))
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| text.chars().take(120).collect());
    Ok(id)
}

/// `GET /api/v3/version`: confirms a Manager URL is reachable.
pub fn ping_manager(manager_url: &str) -> Result<String, String> {
    let base = manager_base(manager_url)?;
    let url = format!("{base}/api/v3/version");
    let resp = ureq::get(&url)
        .timeout(Duration::from_secs(3))
        .call()
        .map_err(|e| format!("Flamenco ping: {e}"))?;
    let status = resp.status();
    let text = resp
        .into_string()
        .map_err(|e| format!("Flamenco ping read: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(format!("Flamenco ping HTTP {status}: {text}"));
    }
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let ver = v
        .get("version")
        .or_else(|| v.get("shortversion"))
        .and_then(|x| x.as_str())
        .unwrap_or("ok");
    Ok(ver.to_string())
}

/// Job status snapshot from `GET /api/v3/jobs/{id}` (minimal fields for the farm panel).
#[derive(Debug, Clone)]
pub struct FlamencoJobStatus {
    pub id: String,
    pub name: String,
    pub status: String,
}

/// One task from `GET /api/v3/jobs/{id}/tasks`.
#[derive(Debug, Clone)]
pub struct FlamencoTaskSummary {
    pub id: String,
    pub name: String,
    pub status: String,
    pub worker: String,
}

/// True when Manager considers the job finished (no more status polling needed).
pub fn flamenco_job_terminal(status: &str) -> bool {
    matches!(
        status,
        "completed" | "canceled" | "cancelled" | "failed" | "paused"
    )
}

/// `GET /api/v3/jobs/{id}` → id / name / status.
pub fn fetch_job_status(manager_url: &str, job_id: &str) -> Result<FlamencoJobStatus, String> {
    let base = manager_base(manager_url)?;
    let url = format!("{base}/api/v3/jobs/{job_id}");
    let (status, text) = flamenco_http_text(
        ureq::get(&url)
            .timeout(Duration::from_secs(5))
            .call(),
    )?;
    if !(200..300).contains(&status) {
        let snippet: String = text.chars().take(200).collect();
        return Err(format!("Flamenco job HTTP {status}: {snippet}"));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("Flamenco job JSON: {e}"))?;
    Ok(FlamencoJobStatus {
        id: v
            .get("id")
            .and_then(|x| x.as_str())
            .unwrap_or(job_id)
            .to_string(),
        name: v
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        status: v
            .get("status")
            .and_then(|x| x.as_str())
            .unwrap_or("?")
            .to_string(),
    })
}

/// `GET /api/v3/jobs/{id}/tasks` → task summaries (worker name when present).
pub fn fetch_job_tasks(
    manager_url: &str,
    job_id: &str,
) -> Result<Vec<FlamencoTaskSummary>, String> {
    let base = manager_base(manager_url)?;
    let url = format!("{base}/api/v3/jobs/{job_id}/tasks");
    let (status, text) = flamenco_http_text(
        ureq::get(&url)
            .timeout(Duration::from_secs(5))
            .call(),
    )?;
    if !(200..300).contains(&status) {
        let snippet: String = text.chars().take(200).collect();
        return Err(format!("Flamenco tasks HTTP {status}: {snippet}"));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("Flamenco tasks JSON: {e}"))?;
    let arr = if let Some(a) = v.as_array() {
        a.clone()
    } else if let Some(a) = v.get("tasks").and_then(|x| x.as_array()) {
        a.clone()
    } else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(arr.len());
    for t in arr {
        let worker = t
            .get("worker")
            .and_then(|w| {
                if let Some(s) = w.as_str() {
                    Some(s.to_string())
                } else {
                    w.get("name")
                        .and_then(|n| n.as_str())
                        .map(|s| s.to_string())
                }
            })
            .unwrap_or_default();
        out.push(FlamencoTaskSummary {
            id: t
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            name: t
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            status: t
                .get("status")
                .and_then(|x| x.as_str())
                .unwrap_or("?")
                .to_string(),
            worker,
        });
    }
    Ok(out)
}

/// Prefer an active task for log tail; else last task whose name contains `encode`; else last task.
pub fn pick_log_task(tasks: &[FlamencoTaskSummary]) -> Option<&FlamencoTaskSummary> {
    if let Some(t) = tasks.iter().find(|t| t.status == "active") {
        return Some(t);
    }
    if let Some(t) = tasks
        .iter()
        .rev()
        .find(|t| t.name.to_ascii_lowercase().contains("encode"))
    {
        return Some(t);
    }
    tasks.last()
}

/// `GET /api/v3/tasks/{task_id}/logtail` → plain-text tail (~2048 bytes on Manager).
pub fn fetch_task_logtail(manager_url: &str, task_id: &str) -> Result<String, String> {
    let base = manager_base(manager_url)?;
    let url = format!("{base}/api/v3/tasks/{task_id}/logtail");
    let (status, text) = flamenco_http_text(
        ureq::get(&url)
            .timeout(Duration::from_secs(5))
            .call(),
    )?;
    if status == 204 || (200..300).contains(&status) && text.is_empty() {
        return Ok(String::new());
    }
    if !(200..300).contains(&status) {
        let snippet: String = text.chars().take(200).collect();
        return Err(format!("Flamenco logtail HTTP {status}: {snippet}"));
    }
    Ok(text)
}

/// Full task log via `GET /api/v3/tasks/{id}/log` → download `url` under `/job-files/…`.
/// Prefer this over [`fetch_task_logtail`] — logtail is only ~2 KiB and drops earlier stderr.
pub fn fetch_task_log(manager_url: &str, task_id: &str) -> Result<String, String> {
    let base = manager_base(manager_url)?;
    let info_url = format!("{base}/api/v3/tasks/{task_id}/log");
    let (status, text) = flamenco_http_text(
        ureq::get(&info_url)
            .timeout(Duration::from_secs(5))
            .call(),
    )?;
    if status == 204 {
        return Ok(String::new());
    }
    if !(200..300).contains(&status) {
        let snippet: String = text.chars().take(200).collect();
        return Err(format!("Flamenco log info HTTP {status}: {snippet}"));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("Flamenco log info JSON: {e}"))?;
    let rel = v
        .get("url")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "Flamenco log info missing url".to_string())?;
    let file_url = if rel.starts_with("http://") || rel.starts_with("https://") {
        rel.to_string()
    } else if rel.starts_with('/') {
        format!("{base}{rel}")
    } else {
        format!("{base}/{rel}")
    };
    let (status, body) = flamenco_http_text(
        ureq::get(&file_url)
            .timeout(Duration::from_secs(30))
            .call(),
    )?;
    if !(200..300).contains(&status) {
        let snippet: String = body.chars().take(200).collect();
        return Err(format!("Flamenco log file HTTP {status}: {snippet}"));
    }
    Ok(body)
}

/// Manager web UI root (SPA has no stable deep-link per job).
pub fn manager_web_url(manager_url: &str) -> Result<String, String> {
    let base = manager_base(manager_url)?;
    Ok(format!("{base}/"))
}

/// UPnP/SSDP M-SEARCH for Flamenco Manager description URLs (same ST as workers).
pub fn discover_manager_locations(timeout: Duration) -> Result<Vec<String>, String> {
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("SSDP bind: {e}"))?;
    sock.set_read_timeout(Some(Duration::from_millis(400)))
        .map_err(|e| format!("SSDP timeout: {e}"))?;
    let mx = timeout.as_secs().clamp(1, 5);
    let msearch = format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: {mx}\r\nST: {SSDP_ST}\r\n\r\n"
    );
    sock.send_to(msearch.as_bytes(), SSDP_ADDR)
        .map_err(|e| format!("SSDP send: {e}"))?;

    let deadline = Instant::now() + timeout;
    let mut seen = HashSet::new();
    let mut locations = Vec::new();
    let mut buf = [0u8; 8192];
    let mut last_send = Instant::now();

    while Instant::now() < deadline {
        if last_send.elapsed() >= Duration::from_secs(1) {
            let _ = sock.send_to(msearch.as_bytes(), SSDP_ADDR);
            last_send = Instant::now();
        }
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                if !text.contains(SSDP_UUID) {
                    continue;
                }
                if let Some(loc) = http_header_value(&text, "LOCATION") {
                    if seen.insert(loc.clone()) {
                        locations.push(loc);
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("SSDP recv: {e}")),
        }
        if !locations.is_empty() && Instant::now() + Duration::from_millis(500) >= deadline {
            break;
        }
    }
    Ok(locations)
}

/// Description LOCATION → Manager root URL (`http://host:port/`), first that answers `/api/v3/version`.
pub fn discover_manager(timeout: Duration) -> Result<String, String> {
    let locations = discover_manager_locations(timeout)?;
    if locations.is_empty() {
        return Err("no Flamenco Manager found via UPnP/SSDP (is autodiscoverable enabled?)".into());
    }
    let mut tried = Vec::new();
    for loc in &locations {
        let Some(root) = location_to_manager_root(loc) else {
            continue;
        };
        match ping_manager(&root) {
            Ok(_) => return Ok(root),
            Err(e) => tried.push(format!("{root} ({e})")),
        }
    }
    Err(format!(
        "found SSDP locations but none answered /api/v3/version: {}",
        tried.join("; ")
    ))
}

fn location_to_manager_root(location: &str) -> Option<String> {
    // Flamenco workers strip path to `/` (description is under /upnp/…).
    let url = location.trim();
    let after_scheme = url.split("://").nth(1)?;
    let hostport = after_scheme.split('/').next()?;
    let scheme = url.split("://").next()?;
    Some(format!("{scheme}://{hostport}/"))
}

fn http_header_value(message: &str, name: &str) -> Option<String> {
    let want = name.to_ascii_lowercase();
    for line in message.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if k.trim().eq_ignore_ascii_case(&want) {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Resolve Manager install dir → `scripts/` (expects `flamenco-manager.yaml` beside the exe).
pub fn scripts_dir_from_manager_install(manager_dir: &Path) -> Result<PathBuf, String> {
    let yaml = manager_dir.join("flamenco-manager.yaml");
    if !yaml.is_file() {
        return Err(format!(
            "{} not found; pick the Manager install folder (contains flamenco-manager.yaml)",
            yaml.display()
        ));
    }
    let scripts = manager_dir.join("scripts");
    if !scripts.is_dir() {
        fs::create_dir_all(&scripts).map_err(|e| format!("create scripts/: {e}"))?;
    }
    Ok(scripts)
}

/// Paths written by [`deploy_gigamux_manager`].
#[derive(Debug, Clone)]
pub struct DeployReport {
    pub manager_dir: PathBuf,
    pub scripts_js: PathBuf,
    pub tools_win: PathBuf,
    pub tools_linux: PathBuf,
}

/// Candidate dirs that may contain prebuilt `gigamux-check` helpers.
fn helper_search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.push(dir.join("helpers"));
            roots.push(dir.join("flamenco").join("helpers"));
            // Dev: GigaMux.exe under target/debug → repo flamenco/helpers + target/release.
            roots.push(dir.join("..").join("..").join("flamenco").join("helpers"));
            roots.push(dir.join("..").join("release"));
            roots.push(dir.to_path_buf());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.join("flamenco").join("helpers"));
        roots.push(cwd.join("helpers"));
        roots.push(cwd.join("target").join("release"));
    }
    // Compile-time workspace path (dev builds of gigamux-core).
    roots.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../flamenco/helpers"));
    roots.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release"));
    roots
}

fn find_helper_file(file_name: &str) -> Result<PathBuf, String> {
    for root in helper_search_roots() {
        let p = root.join(file_name);
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(format!(
        "missing helper `{file_name}` — expected next to the app under helpers/ (release) or flamenco/helpers/ (dev; see flamenco/helpers/README.md)"
    ))
}

/// Copy bundled `GigaMux.js` into `scripts_dir` (legacy; prefer [`deploy_gigamux_manager`]).
pub fn deploy_gigamux_js(scripts_dir: &Path) -> Result<PathBuf, String> {
    if !scripts_dir.is_dir() {
        return Err(format!("not a directory: {}", scripts_dir.display()));
    }
    let dest = scripts_dir.join("GigaMux.js");
    fs::write(&dest, GIGAMUX_JS).map_err(|e| format!("write {}: {e}", dest.display()))?;
    Ok(dest)
}

/// Deploy job script + Win/Linux `gigamux-check` into a Manager install folder.
///
/// Requires `flamenco-manager.yaml` (not a bare `scripts/` pick). Writes:
/// - `scripts/GigaMux.js`
/// - `tools/gigamux-check.exe` (Windows)
/// - `tools/gigamux-check` (Linux)
///
/// Workers must still have `gigamux-check` on `PATH` (`misc`/`exec` does not search worker `tools/`).
pub fn deploy_gigamux_manager(manager_dir: &Path) -> Result<DeployReport, String> {
    let scripts = scripts_dir_from_manager_install(manager_dir)?;
    let tools = manager_dir.join("tools");
    fs::create_dir_all(&tools).map_err(|e| format!("create tools/: {e}"))?;

    let win_src = find_helper_file("gigamux-check.exe")?;
    let linux_src = find_helper_file("gigamux-check")?;
    // Refuse accidentally deploying the Windows PE as the Linux name.
    if win_src.canonicalize().ok() == linux_src.canonicalize().ok() {
        return Err(
            "Windows and Linux helpers resolved to the same file — place a Linux ELF at flamenco/helpers/gigamux-check"
                .into(),
        );
    }

    let scripts_js = deploy_gigamux_js(&scripts)?;
    let tools_win = tools.join("gigamux-check.exe");
    let tools_linux = tools.join("gigamux-check");
    fs::copy(&win_src, &tools_win)
        .map_err(|e| format!("copy {}: {e}", tools_win.display()))?;
    fs::copy(&linux_src, &tools_linux)
        .map_err(|e| format!("copy {}: {e}", tools_linux.display()))?;
    // Windows→SMB copies usually drop the Unix execute bit; set it when the OS supports it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&tools_linux)
            .map_err(|e| format!("stat {}: {e}", tools_linux.display()))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&tools_linux, perms)
            .map_err(|e| format!("chmod {}: {e}", tools_linux.display()))?;
    }

    Ok(DeployReport {
        manager_dir: manager_dir.to_path_buf(),
        scripts_js,
        tools_win,
        tools_linux,
    })
}

/// True when `scripts_dir/GigaMux.js` exists (local path from last Deploy).
pub fn local_gigamux_js_present(scripts_dir: &str) -> bool {
    let t = scripts_dir.trim();
    if t.is_empty() {
        return false;
    }
    let p = Path::new(t);
    // Accept either scripts/ path or Manager install path.
    if p.join("GigaMux.js").is_file() {
        return true;
    }
    p.join("scripts").join("GigaMux.js").is_file()
}

/// User-facing hint when Manager rejects a GigaMux job due to missing/old script.
pub fn flamenco_script_hint(err_or_body: &str) -> Option<&'static str> {
    let lower = err_or_body.to_ascii_lowercase();
    if lower.contains("job type unknown") {
        Some(
            "GigaMux.js is missing from the Manager scripts/ folder (or Manager has not loaded it). Settings → Files → Deploy GigaMux… then retry",
        )
    } else if lower.contains("ffmpeg_argv_json is required")
        || (lower.contains("encode_argvs_json") && lower.contains("required"))
        || lower.contains("vmaf_pairs_json")
        || lower.contains("vmaf_json")
        || lower.contains("ensure-vmaf")
    {
        Some(
            "Manager has an old/incompatible GigaMux.js (or outdated gigamux-check). Settings → Files → Deploy GigaMux… then retry",
        )
    } else {
        None
    }
}

/// List existing worker tags on the Manager (`GET /api/v3/worker-mgt/tags`).
pub fn fetch_worker_tags(manager_url: &str) -> Result<Vec<(String, String)>, String> {
    let base = manager_base(manager_url)?;
    let url = format!("{base}/api/v3/worker-mgt/tags");
    let resp = ureq::get(&url)
        .timeout(Duration::from_secs(5))
        .call()
        .map_err(|e| format!("fetch tags: {e}"))?;
    let status = resp.status();
    let text = resp
        .into_string()
        .map_err(|e| format!("fetch tags read: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(format!("fetch tags HTTP {status}: {text}"));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("tags JSON: {e}"))?;
    let arr = v
        .get("tags")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for t in arr {
        let id = t.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let name = t
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if !name.is_empty() {
            out.push((id, name));
        }
    }
    Ok(out)
}

/// Manager worker list row (`GET /api/v3/worker-mgt/workers`).
#[derive(Debug, Clone)]
pub struct FlamencoWorkerSummary {
    pub id: String,
    pub name: String,
    pub status: String,
}

/// Temporary tag pin used so one farm-probe job targets exactly one worker; restore prior tags and delete the pin when the job ends.
#[derive(Debug, Clone)]
pub struct ProbeWorkerPin {
    pub worker_id: String,
    pub worker_name: String,
    pub tag_id: String,
    pub tag_name: String,
    pub previous_tag_ids: Vec<String>,
}

/// Whether Probe Farm should submit a job for this worker status (skip offline/error).
pub fn worker_status_probeable(status: &str) -> bool {
    match status.trim().to_ascii_lowercase().as_str() {
        "offline" | "error" => false,
        _ => true,
    }
}

/// Short unique tag name for pinning a single worker during farm probe (`gm-` + first 8 hex of id).
pub fn probe_pin_tag_name(worker_id: &str) -> String {
    let compact: String = worker_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(8)
        .collect();
    format!("gm-{compact}")
}

/// List workers known to the Manager.
pub fn fetch_workers(manager_url: &str) -> Result<Vec<FlamencoWorkerSummary>, String> {
    let base = manager_base(manager_url)?;
    let url = format!("{base}/api/v3/worker-mgt/workers");
    let resp = ureq::get(&url)
        .timeout(Duration::from_secs(5))
        .call()
        .map_err(|e| format!("fetch workers: {e}"))?;
    let status = resp.status();
    let text = resp
        .into_string()
        .map_err(|e| format!("fetch workers read: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(format!("fetch workers HTTP {status}: {text}"));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("workers JSON: {e}"))?;
    let arr = v
        .get("workers")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for w in arr {
        let id = w.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let name = w
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let st = w
            .get("status")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if !id.is_empty() {
            out.push(FlamencoWorkerSummary {
                id,
                name,
                status: st,
            });
        }
    }
    Ok(out)
}

/// Tag UUIDs currently assigned to a worker (`GET …/workers/{id}` → `tags[].id`).
pub fn fetch_worker_tag_ids(manager_url: &str, worker_id: &str) -> Result<Vec<String>, String> {
    let base = manager_base(manager_url)?;
    let url = format!("{base}/api/v3/worker-mgt/workers/{worker_id}");
    let resp = ureq::get(&url)
        .timeout(Duration::from_secs(5))
        .call()
        .map_err(|e| format!("fetch worker: {e}"))?;
    let status = resp.status();
    let text = resp
        .into_string()
        .map_err(|e| format!("fetch worker read: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(format!("fetch worker HTTP {status}: {text}"));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("worker JSON: {e}"))?;
    let mut out = Vec::new();
    if let Some(arr) = v.get("tags").and_then(|t| t.as_array()) {
        for t in arr {
            if let Some(id) = t.get("id").and_then(|x| x.as_str()) {
                if !id.is_empty() {
                    out.push(id.to_string());
                }
            }
        }
    }
    Ok(out)
}

/// Replace a worker's tag membership (`POST …/workers/{id}/settags`).
pub fn set_worker_tags(
    manager_url: &str,
    worker_id: &str,
    tag_ids: &[String],
) -> Result<(), String> {
    let base = manager_base(manager_url)?;
    let url = format!("{base}/api/v3/worker-mgt/workers/{worker_id}/settags");
    let body = json!({ "tag_ids": tag_ids });
    let (status, text) = flamenco_http_text(
        ureq::post(&url)
            .set("Content-Type", "application/json")
            .send_json(&body),
    )?;
    if !(200..300).contains(&status) {
        return Err(format!("settags HTTP {status}: {text}"));
    }
    Ok(())
}

/// Ensure a named tag exists; returns its UUID (create response or existing list).
pub fn ensure_named_worker_tag(
    manager_url: &str,
    name: &str,
    description: &str,
) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("empty tag name".into());
    }
    let existing = fetch_worker_tags(manager_url)?;
    if let Some((id, _)) = existing.iter().find(|(_, n)| n.eq_ignore_ascii_case(name)) {
        return Ok(id.clone());
    }
    let base = manager_base(manager_url)?;
    let url = format!("{base}/api/v3/worker-mgt/tags");
    let body = json!({ "name": name, "description": description });
    let (status, text) = flamenco_http_text(
        ureq::post(&url)
            .set("Content-Type", "application/json")
            .send_json(&body),
    )?;
    if !(200..300).contains(&status) {
        return Err(format!("create tag {name} HTTP {status}: {text}"));
    }
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    // Fallback: re-list (create may have succeeded without an id field).
    let existing = fetch_worker_tags(manager_url)?;
    existing
        .into_iter()
        .find(|(_, n)| n.eq_ignore_ascii_case(name))
        .map(|(id, _)| id)
        .ok_or_else(|| format!("create tag {name}: no id in response"))
}

/// `DELETE /api/v3/worker-mgt/tag/{tag_id}` — removes the tag and any worker assignments.
pub fn delete_worker_tag(manager_url: &str, tag_id: &str) -> Result<(), String> {
    let id = tag_id.trim();
    if id.is_empty() {
        return Err("empty tag id".into());
    }
    let base = manager_base(manager_url)?;
    let url = format!("{base}/api/v3/worker-mgt/tag/{id}");
    let (status, text) = flamenco_http_text(ureq::delete(&url).call())?;
    if !(200..300).contains(&status) {
        return Err(format!("delete tag HTTP {status}: {text}"));
    }
    Ok(())
}

/// Delete leftover GigaMux farm-probe pin tags (`gm-…`). Safe when no probe job is using them.
pub fn delete_probe_pin_tags(manager_url: &str) -> Result<Vec<String>, String> {
    let tags = fetch_worker_tags(manager_url)?;
    let mut deleted = Vec::new();
    for (id, name) in tags {
        if !name.to_ascii_lowercase().starts_with("gm-") {
            continue;
        }
        delete_worker_tag(manager_url, &id)?;
        deleted.push(name);
    }
    Ok(deleted)
}

/// Append a unique `gm-…` tag to one worker so a probe job can target only that host; caller must restore+delete.
pub fn pin_worker_for_probe(
    manager_url: &str,
    worker: &FlamencoWorkerSummary,
) -> Result<ProbeWorkerPin, String> {
    let tag_name = probe_pin_tag_name(&worker.id);
    let tag_id = ensure_named_worker_tag(
        manager_url,
        &tag_name,
        "GigaMux farm-probe pin (temporary; deleted after probe)",
    )?;
    let previous_tag_ids = fetch_worker_tag_ids(manager_url, &worker.id)?;
    let mut next = previous_tag_ids.clone();
    if !next.iter().any(|id| id.eq_ignore_ascii_case(&tag_id)) {
        next.push(tag_id.clone());
    }
    set_worker_tags(manager_url, &worker.id, &next)?;
    Ok(ProbeWorkerPin {
        worker_id: worker.id.clone(),
        worker_name: worker.name.clone(),
        tag_id,
        tag_name,
        previous_tag_ids,
    })
}

/// Restore the worker's prior tags, then delete the temporary `gm-…` pin tag.
pub fn restore_worker_tags_after_probe(
    manager_url: &str,
    pin: &ProbeWorkerPin,
) -> Result<(), String> {
    set_worker_tags(manager_url, &pin.worker_id, &pin.previous_tag_ids)?;
    delete_worker_tag(manager_url, &pin.tag_id)
}

/// Create missing coarse tags plus any `nvenc-{gen}` slugs in `inventory`. Returns names created.
pub fn ensure_worker_tags(
    manager_url: &str,
    inventory: Option<&FarmProbeInventory>,
) -> Result<Vec<String>, String> {
    let existing = fetch_worker_tags(manager_url)?;
    let mut have: HashSet<String> = existing.into_iter().map(|(_, n)| n).collect();
    let base = manager_base(manager_url)?;
    let url = format!("{base}/api/v3/worker-mgt/tags");
    let mut created = Vec::new();
    let mut wanted: Vec<(String, String)> = REQUIRED_WORKER_TAGS
        .iter()
        .map(|(n, d)| ((*n).to_string(), (*d).to_string()))
        .collect();
    if let Some(inv) = inventory {
        for slug in &inv.nvenc_gens {
            if crate::gpu::NvencGeneration::from_farm_tag_slug(slug).is_none() {
                continue;
            }
            let name = if slug.starts_with("nvenc-") {
                slug.clone()
            } else {
                format!("nvenc-{slug}")
            };
            let desc = nvenc_gen_tag_description(&name);
            wanted.push((name, desc));
        }
    }
    for (name, description) in wanted {
        if have.contains(&name) {
            continue;
        }
        let body = json!({ "name": name, "description": description });
        let resp = ureq::post(&url)
            .set("Content-Type", "application/json")
            .send_json(&body)
            .map_err(|e| format!("create tag {name}: {e}"))?;
        let status = resp.status();
        let text = resp
            .into_string()
            .map_err(|e| format!("create tag {name} read: {e}"))?;
        if !(200..300).contains(&status) {
            return Err(format!("create tag {name} HTTP {status}: {text}"));
        }
        have.insert(name.clone());
        created.push(name);
    }
    Ok(created)
}

/// Persisted union of farm probe host reports (prefs).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FarmProbeInventory {
    /// Flamenco tag slugs seen on the farm (`nvenc-ada`, …).
    #[serde(default)]
    pub nvenc_gens: Vec<String>,
    /// Hostnames that contributed probe JSON (until Clear).
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub has_qsv: bool,
    #[serde(default)]
    pub has_vmaf: bool,
    /// True once any host probe completed (soft/cpu pool usable).
    #[serde(default)]
    pub has_cpu: bool,
}

impl FarmProbeInventory {
    /// Merge another host/dir aggregate into this inventory (union; never drops gens).
    pub fn merge_from(&mut self, other: &FarmProbeInventory) {
        for g in &other.nvenc_gens {
            let slug = normalize_nvenc_gen_slug(g);
            if !slug.is_empty() && !self.nvenc_gens.iter().any(|x| x == &slug) {
                self.nvenc_gens.push(slug);
            }
        }
        for h in &other.hosts {
            if !h.is_empty() && !self.hosts.iter().any(|x| x == h) {
                self.hosts.push(h.clone());
            }
        }
        self.has_qsv |= other.has_qsv;
        self.has_vmaf |= other.has_vmaf;
        self.has_cpu |= other.has_cpu;
        self.nvenc_gens.sort();
        self.hosts.sort();
    }

    /// Tags the Send dialog may offer (empty string = tagless).
    pub fn allowed_submit_tags(&self) -> Vec<String> {
        let mut out = vec![String::new(), "nvenc".into()];
        for g in &self.nvenc_gens {
            let slug = normalize_nvenc_gen_slug(g);
            if !slug.is_empty() && !out.iter().any(|x| x == &slug) {
                out.push(slug);
            }
        }
        out.push("qsv".into());
        out.push("cpu".into());
        // vmaf is hardcoded for VMAF-only jobs; still list for prefs default clarity.
        out.push("vmaf".into());
        out
    }
}

fn normalize_nvenc_gen_slug(s: &str) -> String {
    let t = s.trim().to_ascii_lowercase();
    if t.is_empty() {
        return String::new();
    }
    if let Some(g) = crate::gpu::NvencGeneration::from_farm_tag_slug(&t) {
        return g.farm_tag_slug().unwrap_or("").to_string();
    }
    String::new()
}

/// One worker's probe JSON on shared storage (`{out_dir}/{host}.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FarmProbeHostReport {
    pub host: String,
    /// Full tag slugs (`nvenc-ada`).
    pub nvenc_gens: Vec<String>,
    pub has_qsv: bool,
    pub has_vmaf: bool,
    pub has_cpu: bool,
}

impl FarmProbeHostReport {
    fn into_inventory(self) -> FarmProbeInventory {
        FarmProbeInventory {
            nvenc_gens: self.nvenc_gens,
            hosts: if self.host.is_empty() {
                Vec::new()
            } else {
                vec![self.host]
            },
            has_qsv: self.has_qsv,
            has_vmaf: self.has_vmaf,
            has_cpu: self.has_cpu,
        }
    }
}

/// Probe this machine and write `{out_dir}/{hostname}.json` (Flamenco worker / local).
pub fn write_farm_probe_report(out_dir: &Path) -> Result<PathBuf, String> {
    use crate::gpu::probe_local_encoders;
    use crate::vmaf::libvmaf_available;

    let host = farm_probe_hostname();
    let catalog = probe_local_encoders();
    let mut gens: Vec<String> = catalog
        .nvenc
        .iter()
        .filter_map(|d| d.generation.farm_tag_slug().map(|s| s.to_string()))
        .collect();
    gens.sort();
    gens.dedup();
    let report = FarmProbeHostReport {
        host: host.clone(),
        nvenc_gens: gens,
        has_qsv: catalog.qsv.is_some(),
        has_vmaf: libvmaf_available(),
        has_cpu: true,
    };
    fs::create_dir_all(out_dir).map_err(|e| format!("farm-probe mkdir: {e}"))?;
    let safe_host: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = out_dir.join(format!("{safe_host}.json"));
    let text = serde_json::to_string_pretty(&report).map_err(|e| format!("farm-probe json: {e}"))?;
    fs::write(&path, text).map_err(|e| format!("farm-probe write {}: {e}", path.display()))?;
    // Also print a parseable line for task logs.
    println!(
        "GIGAMUX_FARM_PROBE host={} gens={} qsv={} vmaf={}",
        report.host,
        report.nvenc_gens.join(","),
        report.has_qsv,
        report.has_vmaf
    );
    Ok(path)
}

fn farm_probe_hostname() -> String {
    // Windows workers set COMPUTERNAME. Linux often has HOSTNAME only as a bash local (not exported), so Flamenco `exec` children never see it — fall through to `hostname(1)` / gethostname.
    for key in ["COMPUTERNAME", "HOSTNAME", "HOST"] {
        if let Ok(v) = std::env::var(key) {
            let t = v.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    if let Ok(out) = std::process::Command::new("hostname").output() {
        if out.status.success() {
            let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !t.is_empty() {
                return t;
            }
        }
    }
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: gethostname writes a NUL-terminated name into `buf`.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut _, buf.len()) };
        if rc == 0 {
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(s) = std::str::from_utf8(&buf[..len]) {
                let t = s.trim();
                if !t.is_empty() {
                    return t.to_string();
                }
            }
        }
    }
    "unknown-host".into()
}

/// Load per-host probe JSON files from `out_dir` (skips unreadable / invalid).
pub fn load_farm_probe_reports(out_dir: &Path) -> Result<Vec<FarmProbeHostReport>, String> {
    let mut out = Vec::new();
    if !out_dir.is_dir() {
        return Ok(out);
    }
    let entries = fs::read_dir(out_dir).map_err(|e| format!("farm-probe read dir: {e}"))?;
    for ent in entries.flatten() {
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let report: FarmProbeHostReport = match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(_) => continue,
        };
        out.push(report);
    }
    Ok(out)
}

/// Read all `*.json` probe reports under `out_dir` into one inventory (union).
pub fn merge_farm_probe_dir(out_dir: &Path) -> Result<FarmProbeInventory, String> {
    let mut inv = FarmProbeInventory::default();
    for report in load_farm_probe_reports(out_dir)? {
        inv.merge_from(&report.into_inventory());
    }
    Ok(inv)
}

/// Coarse pools + `nvenc-{gen}` (+ leftover `gm-` pins) that GigaMux owns for auto-assign.
pub fn is_gigamux_managed_tag(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    if n.starts_with("gm-") {
        return true;
    }
    if n == "nvenc" || n == "qsv" || n == "vmaf" || n == "cpu" {
        return true;
    }
    crate::gpu::NvencGeneration::from_farm_tag_slug(&n).is_some()
}

/// Capability tags implied by one host probe (names, not UUIDs).
pub fn desired_capability_tags(report: &FarmProbeHostReport) -> Vec<String> {
    let mut tags = Vec::new();
    if report.has_cpu {
        tags.push("cpu".into());
    }
    if report.has_qsv {
        tags.push("qsv".into());
    }
    if report.has_vmaf {
        tags.push("vmaf".into());
    }
    let mut gens = Vec::new();
    for g in &report.nvenc_gens {
        let slug = normalize_nvenc_gen_slug(g);
        if !slug.is_empty() && !gens.iter().any(|x| x == &slug) {
            gens.push(slug);
        }
    }
    if !gens.is_empty() {
        tags.push("nvenc".into());
        tags.extend(gens);
    }
    tags.sort();
    tags.dedup();
    tags
}

/// Result of matching probe JSON hosts to Manager workers and applying capability tags.
#[derive(Debug, Clone, Default)]
pub struct FarmProbeAssignReport {
    /// `worker: tag,tag,…`
    pub assigned: Vec<String>,
    /// Probe hosts with no matching Manager worker name.
    pub unmatched: Vec<String>,
}

/// Tag ids currently on a worker, with names (`GET …/workers/{id}`).
pub fn fetch_worker_tag_membership(
    manager_url: &str,
    worker_id: &str,
) -> Result<Vec<(String, String)>, String> {
    let base = manager_base(manager_url)?;
    let url = format!("{base}/api/v3/worker-mgt/workers/{worker_id}");
    let resp = ureq::get(&url)
        .timeout(Duration::from_secs(5))
        .call()
        .map_err(|e| format!("fetch worker: {e}"))?;
    let status = resp.status();
    let text = resp
        .into_string()
        .map_err(|e| format!("fetch worker read: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(format!("fetch worker HTTP {status}: {text}"));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("worker JSON: {e}"))?;
    let mut out = Vec::new();
    if let Some(arr) = v.get("tags").and_then(|t| t.as_array()) {
        for t in arr {
            let id = t.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let name = t
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if !id.is_empty() {
                out.push((id, name));
            }
        }
    }
    Ok(out)
}

/// Match each probe host report to a Manager worker (case-insensitive name) and set capability tags.
///
/// Replaces GigaMux-managed tags (`nvenc`, `nvenc-*`, `qsv`, `vmaf`, `cpu`, `gm-*`) with the probe set; leaves any other tags alone.
pub fn assign_workers_from_farm_probe(
    manager_url: &str,
    out_dir: &Path,
) -> Result<FarmProbeAssignReport, String> {
    let reports = load_farm_probe_reports(out_dir)?;
    let workers = fetch_workers(manager_url)?;
    let all_tags = fetch_worker_tags(manager_url)?;
    let resolve = |name: &str| -> Result<String, String> {
        all_tags
            .iter()
            .find(|(_, n)| n.eq_ignore_ascii_case(name))
            .map(|(id, _)| id.clone())
            .ok_or_else(|| format!("tag {name:?} missing — Ensure tags first"))
    };

    let mut report = FarmProbeAssignReport::default();
    for host_report in &reports {
        let host = host_report.host.trim();
        if host.is_empty() || host.eq_ignore_ascii_case("unknown-host") {
            report.unmatched.push(host_report.host.clone());
            continue;
        }
        let Some(worker) = workers
            .iter()
            .find(|w| w.name.eq_ignore_ascii_case(host))
        else {
            report.unmatched.push(host.to_string());
            continue;
        };
        let desired_names = desired_capability_tags(host_report);
        let mut desired_ids = Vec::new();
        for name in &desired_names {
            desired_ids.push(resolve(name)?);
        }
        let current = fetch_worker_tag_membership(manager_url, &worker.id)?;
        let mut next: Vec<String> = current
            .iter()
            .filter(|(_, name)| !is_gigamux_managed_tag(name))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &desired_ids {
            if !next.iter().any(|x| x.eq_ignore_ascii_case(id)) {
                next.push(id.clone());
            }
        }
        // Skip no-op settags when membership already matches.
        let mut cur_ids: Vec<String> = current.iter().map(|(id, _)| id.clone()).collect();
        cur_ids.sort();
        let mut next_sorted = next.clone();
        next_sorted.sort();
        if cur_ids == next_sorted {
            report.assigned.push(format!(
                "{}: {} (unchanged)",
                worker.name,
                if desired_names.is_empty() {
                    "(none)".into()
                } else {
                    desired_names.join(",")
                }
            ));
            continue;
        }
        set_worker_tags(manager_url, &worker.id, &next)?;
        report.assigned.push(format!(
            "{}: {}",
            worker.name,
            if desired_names.is_empty() {
                "(none)".into()
            } else {
                desired_names.join(",")
            }
        ));
    }
    Ok(report)
}

/// Default shared folder for probe JSON (under output dir).
pub fn farm_probe_out_dir(output_dir: &Path) -> PathBuf {
    output_dir.join("gigamux-farm-probe")
}

/// Submit a short farm-probe job (`gigamux-check farm-probe <dir>`).
///
/// Shaped like a single-task encode so Manager accepts it without requiring a new settings key / job-type reload: `ffmpeg_exe` is the check binary, argv is `farm-probe <out_dir>`.
pub fn make_gigamux_probe_job(
    name: impl Into<String>,
    check_exe: &str,
    probe_out_dir: &Path,
    worker_tag: Option<&str>,
    twoway: Option<FlamencoTwoway<'_>>,
) -> Result<SubmittedJob, String> {
    let exe_check = if check_exe.trim().is_empty() {
        DEFAULT_CHECK_EXE.to_string()
    } else {
        check_exe.trim().to_string()
    };
    let out = probe_out_dir.to_string_lossy().into_owned();
    let out = match twoway {
        Some(tw) => rewrite_flamenco_twoway_path(&out, tw),
        None => out,
    };
    let check_rewritten = match twoway {
        Some(tw) => rewrite_flamenco_twoway_path(&exe_check, tw),
        None => exe_check,
    };
    // Logical argv[0] is ignored by GigaMux.js; worker exec uses ffmpeg_exe (= check binary).
    let argv = vec![
        "gigamux-check".to_string(),
        "farm-probe".to_string(),
        out.clone(),
    ];
    // Prefer dedicated probe_out_dir when Manager has the redeployed script; encode_argvs always present so compile/persist never hits the empty-argv 400 (and works on older JS too).
    make_gigamux_encode_job(
        name,
        Some(check_rewritten.as_str()),
        &argv,
        worker_tag,
        None, // paths already braced above; avoid double-rewrite of check/out
        CancelExecMode::Direct,
        Some(FlamencoIntegrity {
            check_exe: check_rewritten.clone(),
            ..FlamencoIntegrity::default()
        }),
    )
    .map(|mut job| {
        // New JS short-circuits on probe_out_dir; older JS runs the encode argv (same command).
        if let Some(obj) = job.settings.as_object_mut() {
            obj.insert("probe_out_dir".into(), json!(out));
        }
        job
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrity_paths_rewrite_with_twoway() {
        let integ = FlamencoIntegrity {
            check_exe: r"Z:\Flamenco\tools\gigamux-check".into(),
            final_check: Some(FarmFinalCheck {
                input: r"Z:\GigaMux\in\a.mp4".into(),
                output: r"Z:\GigaMux\out\a.mp4".into(),
                opaque_video: OpaqueVideo::Av1,
                opaque_container: OutContainer::Mp4,
                alpha_video: AlphaVideo::Vp9,
                alpha_container: OutContainer::Webm,
            }),
            ..FlamencoIntegrity::default()
        };
        let argv = vec!["ffmpeg".into(), "-i".into(), r"Z:\GigaMux\in\a.mp4".into()];
        let job = make_gigamux_encode_job(
            "t",
            None,
            &argv,
            None,
            Some(FlamencoTwoway {
                local_prefix: r"Z:\",
                var_name: "shared_storage_path",
            }),
            CancelExecMode::Direct,
            Some(integ),
        )
        .unwrap();
        let final_json = job.settings["final_check_json"].as_str().unwrap();
        assert!(final_json.contains("{shared_storage_path}/GigaMux/in/a.mp4"));
        assert!(final_json.contains("{shared_storage_path}/GigaMux/out/a.mp4"));
        assert_eq!(
            job.settings["check_exe"],
            "{shared_storage_path}/Flamenco/tools/gigamux-check"
        );
    }

    #[test]
    fn check_exe_from_scripts_dir() {
        assert_eq!(
            check_exe_from_deploy_scripts_dir(r"Z:\Flamenco\scripts"),
            r"Z:\Flamenco\tools\gigamux-check"
        );
        assert_eq!(check_exe_from_deploy_scripts_dir(""), DEFAULT_CHECK_EXE);
        assert_eq!(
            resolve_flamenco_check_exe(r"W:\shared\tools\gigamux-check.exe", ""),
            r"W:\shared\tools\gigamux-check"
        );
        assert_eq!(
            resolve_flamenco_check_exe("", r"Z:\Flamenco\scripts"),
            r"Z:\Flamenco\tools\gigamux-check"
        );
    }

    #[test]
    fn script_hint_maps_manager_errors() {
        assert!(flamenco_script_hint(r#"{"code":400,"message":"job type unknown"}"#).is_some());
        assert!(flamenco_script_hint("ffmpeg_argv_json is required").is_some());
        assert!(flamenco_script_hint("connection refused").is_none());
    }

    #[test]
    fn infer_drive_root_prefix() {
        assert_eq!(
            infer_twoway_local_prefix(Path::new(r"Z:\GigaMux\input")),
            Some(r"Z:\".into())
        );
        assert_eq!(infer_twoway_local_prefix(Path::new("/mnt/shared/media")), None);
    }

    #[test]
    fn twoway_rewrites_windows_prefix() {
        let tw = FlamencoTwoway {
            local_prefix: r"Z:\",
            var_name: "shared_storage_path",
        };
        assert_eq!(
            rewrite_flamenco_twoway_path(r"Z:\GigaMux\input\a.mp4", tw),
            "{shared_storage_path}/GigaMux/input/a.mp4"
        );
        assert_eq!(
            rewrite_flamenco_twoway_path(r"Z:/GigaMux/x.mp4", tw),
            "{shared_storage_path}/GigaMux/x.mp4"
        );
        // Non-matching path unchanged.
        assert_eq!(
            rewrite_flamenco_twoway_path(r"D:\other\a.mp4", tw),
            r"D:\other\a.mp4"
        );
    }

    #[test]
    fn twoway_encode_job_braces_paths_inside_json() {
        let argv = vec![
            "ffmpeg".into(),
            "-i".into(),
            r"Z:\GigaMux\in.mp4".into(),
            r"Z:\GigaMux\out.mp4".into(),
        ];
        let job = make_gigamux_encode_job(
            "t",
            None,
            &argv,
            None,
            Some(FlamencoTwoway {
                local_prefix: r"Z:\",
                var_name: "shared_storage_path",
            }),
            CancelExecMode::Direct,
            None,
        )
        .unwrap();
        let enc = job.settings["encode_argvs_json"].as_str().unwrap();
        assert!(enc.contains("{shared_storage_path}/GigaMux/in.mp4"));
        assert!(enc.contains("{shared_storage_path}/GigaMux/out.mp4"));
        assert!(!enc.contains(r"Z:\\GigaMux"));
        assert_eq!(job.settings["cancel_exec"], "direct");
    }

    #[test]
    fn cancel_exec_mode_from_manager_version() {
        assert_eq!(
            cancel_exec_mode_for_manager_version("3.10-beta2 (aa2f2039)"),
            CancelExecMode::Direct
        );
        assert_eq!(
            cancel_exec_mode_for_manager_version("3.10-beta2"),
            CancelExecMode::Direct
        );
        assert_eq!(
            cancel_exec_mode_for_manager_version("3.9.3"),
            CancelExecMode::LegacyStub
        );
        assert_eq!(
            cancel_exec_mode_for_manager_version("3.9"),
            CancelExecMode::LegacyStub
        );
        assert_eq!(
            cancel_exec_mode_for_manager_version("3.10.0"),
            CancelExecMode::Direct
        );
        assert_eq!(
            cancel_exec_mode_for_manager_version(""),
            CancelExecMode::LegacyStub
        );
    }

    #[test]
    fn probe_pin_helpers() {
        assert_eq!(
            probe_pin_tag_name("4312d68c-ea6d-4566-9bf6-e9f09be48ceb"),
            "gm-4312d68c"
        );
        assert!(worker_status_probeable("awake"));
        assert!(worker_status_probeable("asleep"));
        assert!(!worker_status_probeable("offline"));
        assert!(!worker_status_probeable("error"));
        assert!(is_gigamux_managed_tag("nvenc-turing"));
        assert!(is_gigamux_managed_tag("gm-abc"));
        assert!(!is_gigamux_managed_tag("GPU-EEVEE"));
        let tags = desired_capability_tags(&FarmProbeHostReport {
            host: "godzilla".into(),
            nvenc_gens: vec!["nvenc-turing".into()],
            has_qsv: false,
            has_vmaf: false,
            has_cpu: true,
        });
        assert_eq!(tags, vec!["cpu", "nvenc", "nvenc-turing"]);
    }

    #[test]
    fn farm_tag_from_slot() {
        use crate::gpu::NvencGeneration;
        use crate::prefs::EncodeSlot;
        use crate::targets::OpaqueVideo;

        let mut slot = EncodeSlot::default();
        slot.opaque_video = OpaqueVideo::Hevc;
        slot.use_hardware = true;
        slot.nvenc_gen = None;
        assert_eq!(farm_worker_tag_for_slot(&slot, false), "nvenc");
        slot.nvenc_gen = Some(NvencGeneration::Turing);
        assert_eq!(farm_worker_tag_for_slot(&slot, false), "nvenc-turing");
        assert_eq!(farm_worker_tag_for_slot(&slot, true), "cpu");
        slot.opaque_video = OpaqueVideo::Av1;
        assert!(!farm_slot_use_hardware(&slot)); // Turing has no AV1 NVENC
        slot.nvenc_gen = Some(NvencGeneration::Ada);
        assert!(farm_slot_use_hardware(&slot));
        slot.opaque_video = OpaqueVideo::Vp9;
        slot.nvenc_gen = None;
        assert_eq!(farm_worker_tag_for_slot(&slot, false), "qsv");
    }

    #[test]
    fn job_terminal_and_pick_log_task() {
        assert!(flamenco_job_terminal("completed"));
        assert!(flamenco_job_terminal("canceled"));
        assert!(!flamenco_job_terminal("active"));
        assert!(!flamenco_job_terminal("queued"));

        let tasks = vec![
            FlamencoTaskSummary {
                id: "a".into(),
                name: "encode".into(),
                status: "completed".into(),
                worker: "W1".into(),
            },
            FlamencoTaskSummary {
                id: "b".into(),
                name: "mux".into(),
                status: "active".into(),
                worker: "W2".into(),
            },
        ];
        assert_eq!(pick_log_task(&tasks).unwrap().id, "b");

        let done = vec![
            FlamencoTaskSummary {
                id: "c0".into(),
                name: "encode-c0".into(),
                status: "completed".into(),
                worker: "W1".into(),
            },
            FlamencoTaskSummary {
                id: "mux".into(),
                name: "mux".into(),
                status: "completed".into(),
                worker: "W2".into(),
            },
        ];
        // Prefer last encode* when nothing is active.
        assert_eq!(pick_log_task(&done).unwrap().id, "c0");
    }
}
