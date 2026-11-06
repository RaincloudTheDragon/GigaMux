//! Flamenco connector: UPnP/SSDP Manager discovery, `GigaMux.js` deploy, worker tags, job submit.

use std::collections::HashSet;
use std::fs;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

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

/// One-file encode job for Flamenco (`GigaMux.js` → worker `exec` ffmpeg).
pub fn make_gigamux_encode_job(
    name: impl Into<String>,
    ffmpeg_exe: Option<&str>,
    ffmpeg_argv: &[String],
    worker_tag: Option<&str>,
) -> Result<SubmittedJob, String> {
    if ffmpeg_argv.is_empty() {
        return Err("ffmpeg argv is empty".into());
    }
    // argv[0] is the logical program name; worker uses settings.ffmpeg_exe (or "ffmpeg").
    let (exe, tag) = flamenco_exe_and_tag(ffmpeg_exe, worker_tag);
    let argv_json =
        serde_json::to_string(&ffmpeg_argv).map_err(|e| format!("serialize ffmpeg argv: {e}"))?;
    // Also set encode_argvs_json (length 1) so newer GigaMux.js prefers the array form.
    let encode_argvs_json = serde_json::to_string(&vec![ffmpeg_argv])
        .map_err(|e| format!("serialize encode_argvs: {e}"))?;
    let settings = json!({
        "ffmpeg_exe": exe,
        "ffmpeg_argv_json": argv_json,
        "encode_argvs_json": encode_argvs_json,
        "concat_argv_json": "",
        "mux_argv_json": "",
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

/// Chunked encode job: N video-only encodes + concat + mux (GigaMux.js task graph).
pub fn make_gigamux_chunked_job(
    name: impl Into<String>,
    ffmpeg_exe: Option<&str>,
    encode_argvs: &[Vec<String>],
    concat_argv: &[String],
    mux_argv: &[String],
    worker_tag: Option<&str>,
) -> Result<SubmittedJob, String> {
    if encode_argvs.is_empty() {
        return Err("encode_argvs is empty".into());
    }
    if concat_argv.is_empty() || mux_argv.is_empty() {
        return Err("chunked job requires concat_argv and mux_argv".into());
    }
    let (exe, tag) = flamenco_exe_and_tag(ffmpeg_exe, worker_tag);
    let encode_argvs_json = serde_json::to_string(encode_argvs)
        .map_err(|e| format!("serialize encode_argvs: {e}"))?;
    let concat_argv_json =
        serde_json::to_string(concat_argv).map_err(|e| format!("serialize concat_argv: {e}"))?;
    let mux_argv_json =
        serde_json::to_string(mux_argv).map_err(|e| format!("serialize mux_argv: {e}"))?;
    let settings = json!({
        "ffmpeg_exe": exe,
        "encode_argvs_json": encode_argvs_json,
        "concat_argv_json": concat_argv_json,
        "mux_argv_json": mux_argv_json,
        "ffmpeg_argv_json": "",
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
        let hint = if snippet.contains("ffmpeg_argv_json is required") {
            " (Manager still has old GigaMux.js — Settings → Files → Deploy GigaMux.js…)"
        } else {
            ""
        };
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
            "{} not found; pick the folder that contains the Flamenco Manager executable",
            yaml.display()
        ));
    }
    let scripts = manager_dir.join("scripts");
    if !scripts.is_dir() {
        fs::create_dir_all(&scripts).map_err(|e| format!("create scripts/: {e}"))?;
    }
    Ok(scripts)
}

/// Copy bundled `GigaMux.js` into `scripts_dir`.
pub fn deploy_gigamux_js(scripts_dir: &Path) -> Result<PathBuf, String> {
    if !scripts_dir.is_dir() {
        return Err(format!("not a directory: {}", scripts_dir.display()));
    }
    let dest = scripts_dir.join("GigaMux.js");
    fs::write(&dest, GIGAMUX_JS).map_err(|e| format!("write {}: {e}", dest.display()))?;
    Ok(dest)
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
