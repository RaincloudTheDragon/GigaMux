//! Flamenco connector: UPnP/SSDP Manager discovery, `GigaMux.js` deploy, worker tags, job submit.

use std::collections::HashSet;
use std::fs;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::targets::{AlphaVideo, OutContainer, OpaqueVideo, OutputTargets};

/// Bundled job compiler (deploy into Manager `scripts/`).
pub const GIGAMUX_JS: &str = include_str!("../../../flamenco/GigaMux.js");

/// Same ST / device id Flamenco workers use (`internal/upnp_ssdp`).
const SSDP_ST: &str = "urn:flamenco:manager:0";
const SSDP_UUID: &str = "aa80bc5f-d0af-46b8-8630-23bd7e80ec4d";
const SSDP_ADDR: &str = "239.255.255.250:1900";

/// Tags GigaMux expects on the Manager (create-if-missing). Workers are assigned separately.
pub const REQUIRED_WORKER_TAGS: &[(&str, &str)] = &[
    ("nvenc", "NVENC-capable ffmpeg (opaque hardware encode)"),
    ("qsv", "QSV-capable ffmpeg (VP9 QSV opaque)"),
    ("vmaf", "ffmpeg build with libvmaf"),
];

/// Body for `POST /api/v3/jobs` (Flamenco `SubmittedJob`).
#[derive(Debug, Clone, Serialize)]
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
}

impl Default for FlamencoIntegrity {
    fn default() -> Self {
        Self {
            check_exe: DEFAULT_CHECK_EXE.into(),
            chunk_checks: Vec::new(),
            final_check: None,
            cleanup: None,
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

fn integrity_settings_json(integ: &FlamencoIntegrity) -> Result<(String, String, String, String), String> {
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
    Ok((check_exe, chunk_checks_json, final_check_json, cleanup_json))
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
    let (check_exe, chunk_checks_json, final_check_json, cleanup_json) =
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
    let (check_exe, chunk_checks_json, final_check_json, cleanup_json) =
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

/// POST a job to Flamenco Manager. Returns the job id from the response when present.
pub fn submit_job(manager_url: &str, job: &SubmittedJob) -> Result<String, String> {
    let base = manager_base(manager_url)?;
    let url = format!("{base}/api/v3/jobs");
    let body = serde_json::to_vec(job).map_err(|e| format!("serialize job: {e}"))?;
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
    {
        Some(
            "Manager has an old/incompatible GigaMux.js. Settings → Files → Deploy GigaMux… then retry",
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

/// Create any missing tags from [`REQUIRED_WORKER_TAGS`]. Returns names that were created.
pub fn ensure_worker_tags(manager_url: &str) -> Result<Vec<String>, String> {
    let existing = fetch_worker_tags(manager_url)?;
    let have: HashSet<String> = existing.into_iter().map(|(_, n)| n).collect();
    let base = manager_base(manager_url)?;
    let url = format!("{base}/api/v3/worker-mgt/tags");
    let mut created = Vec::new();
    for &(name, description) in REQUIRED_WORKER_TAGS {
        if have.contains(name) {
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
        created.push(name.to_string());
    }
    Ok(created)
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
}
