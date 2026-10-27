//! GigaMux native desktop shell (egui/eframe — no webview).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use eframe::egui;
use gigamux_core::check::{
    deletable_issues, delete_issue_outputs, list_media, run_check, CheckIssue, CheckReport,
};
use gigamux_core::encode::{run_batch, EncodeEvent, EncodeOptions};
use gigamux_core::gpu::{list_nvenc_devices, GpuChoice, NvencDevice};
use gigamux_core::prefs::{self, Prefs};
use gigamux_core::probe::{
    format_bytes, format_length, media_summary, thumbnail_rgba, FrameTiming, MediaSummary,
    RgbaThumb, THUMB_W,
};

/// Live dual-bar encode progress (current file + whole batch).
struct EncodeProgress {
    total_files: usize,
    total_duration_s: f64,
    file_index: usize,
    file_total: usize,
    file_name: String,
    file_duration_s: f64,
    file_out_time_s: f64,
    /// Sum of durations for files that finished (ok or failed) this batch.
    completed_duration_s: f64,
    batch_started: Instant,
    file_started: Instant,
}

impl EncodeProgress {
    fn file_frac(&self) -> f32 {
        if self.file_duration_s > 0.05 {
            (self.file_out_time_s / self.file_duration_s).clamp(0.0, 1.0) as f32
        } else {
            0.0
        }
    }

    fn batch_frac(&self) -> f32 {
        if self.total_duration_s > 0.05 {
            let done = self.completed_duration_s + self.file_out_time_s.min(self.file_duration_s);
            (done / self.total_duration_s).clamp(0.0, 1.0) as f32
        } else if self.total_files > 0 {
            let done = (self.file_index.saturating_sub(1)) as f64 + self.file_frac() as f64;
            (done / self.total_files as f64).clamp(0.0, 1.0) as f32
        } else {
            0.0
        }
    }

    /// Rufus-style `elapsed / ~remaining` (or `elapsed / —` until enough progress).
    fn time_label(elapsed: f64, frac: f32) -> String {
        let rem = if frac > 0.02 {
            let eta = elapsed * ((1.0 - frac as f64) / frac as f64);
            format!("~{}", format_length(eta))
        } else {
            "—".into()
        };
        format!("{} / {rem}", format_length(elapsed))
    }

    fn file_label(&self) -> String {
        Self::time_label(self.file_started.elapsed().as_secs_f64(), self.file_frac())
    }

    fn batch_label(&self) -> String {
        Self::time_label(self.batch_started.elapsed().as_secs_f64(), self.batch_frac())
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 700.0])
            .with_title("GigaMux")
            // Required on Windows — otherwise dropped_files stays empty.
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "GigaMux",
        options,
        Box::new(|_cc| Ok(Box::new(GigaMuxApp::new()))),
    )
}

enum JobKind {
    Encode,
    Check,
}

enum AppEvent {
    Encode(EncodeEvent),
    CheckDone(Result<CheckReport, String>),
}

struct PreviewReady {
    gen: u64,
    name: String,
    summary: Result<MediaSummary, String>,
    thumb: Result<RgbaThumb, String>,
}

/// Cached thumb + metadata for one input file (filled by background pre-cache).
struct PreviewCacheEntry {
    summary: Option<Result<MediaSummary, String>>,
    thumb: Option<Result<RgbaThumb, String>>,
    texture: Option<egui::TextureHandle>,
    /// Still waiting on the pre-cache worker.
    pending: bool,
    /// Matches [`MediaSummary::SCHEMA`] when filled so stale shapes re-fetch.
    schema: u32,
}

impl PreviewCacheEntry {
    fn pending() -> Self {
        Self {
            summary: None,
            thumb: None,
            texture: None,
            pending: true,
            schema: 0,
        }
    }

    fn ready(&self) -> bool {
        !self.pending && self.summary.is_some() && self.schema == MediaSummary::SCHEMA
    }
}

/// Center column: collapsed when nothing selected; expands to thumb width when active.
const CENTER_MIN_W: f32 = 160.0;
const SIDE_MIN_W: f32 = 180.0;

/// Which folder list currently owns the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileSide {
    Input,
    Output,
}

struct GigaMuxApp {
    prefs: Prefs,
    devices: Vec<NvencDevice>,
    gpu_warn: Option<String>,
    input_files: Vec<PathBuf>,
    output_files: Vec<PathBuf>,
    /// Selected file (basename) and which pane it lives in.
    active: Option<(FileSide, String)>,
    /// Per-input-file log lines (encode progress keyed by input basename).
    file_logs: HashMap<String, Vec<String>>,
    /// Session / batch log when no file is active.
    session_log: Vec<String>,
    log_open: bool,
    /// When true, log panel shows the session log even if a file is active.
    log_show_session: bool,
    show_settings: bool,
    busy: bool,
    job: Option<JobKind>,
    cancel: Arc<AtomicBool>,
    event_rx: Option<Receiver<AppEvent>>,
    /// Check result dialog (always shown on issues; delete only if outputs exist).
    pending_check: Option<CheckPrompt>,
    /// Last-frame hit rects for folder drag-and-drop targeting.
    input_rect: Option<egui::Rect>,
    output_rect: Option<egui::Rect>,
    /// Invalidates in-flight pre-cache workers when the input folder changes.
    cache_gen: Arc<AtomicU64>,
    preview_cache: HashMap<String, PreviewCacheEntry>,
    cache_rx: Option<Receiver<PreviewReady>>,
    /// Dual progress bars while an encode batch is running.
    encode_progress: Option<EncodeProgress>,
}

/// Post-check dialog: warn about non-intact files; optional delete of bad outputs.
#[derive(Clone)]
struct CheckPrompt {
    issue_count: usize,
    deletable: Vec<CheckIssue>,
}

impl GigaMuxApp {
    fn new() -> Self {
        let mut prefs = prefs::load();
        let devices = match list_nvenc_devices() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("NVENC probe: {e}");
                Vec::new()
            }
        };
        let (gpu, warn) = prefs.gpu.sanitize(&devices);
        prefs.gpu = gpu;
        let mut app = Self {
            prefs,
            devices,
            gpu_warn: warn,
            input_files: Vec::new(),
            output_files: Vec::new(),
            active: None,
            file_logs: HashMap::new(),
            // Restore prior session lines from AppData (same cap as the live buffer).
            session_log: prefs::load_session_log(500),
            log_open: false,
            log_show_session: true,
            show_settings: false,
            busy: false,
            job: None,
            cancel: Arc::new(AtomicBool::new(false)),
            event_rx: None,
            pending_check: None,
            input_rect: None,
            output_rect: None,
            cache_gen: Arc::new(AtomicU64::new(0)),
            preview_cache: HashMap::new(),
            cache_rx: None,
            encode_progress: None,
        };
        app.refresh_lists();
        let started = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        app.push_session(format!("—— session start {started} ——"));
        app
    }

    fn clear_active(&mut self) {
        self.active = None;
        self.log_show_session = true;
    }

    fn preview_key(side: FileSide, name: &str) -> String {
        match side {
            FileSide::Input => format!("i:{name}"),
            FileSide::Output => format!("o:{name}"),
        }
    }

    fn file_stem_of(name: &str) -> &str {
        Path::new(name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(name)
    }

    /// Basename list for one side.
    fn names_for(&self, side: FileSide) -> Vec<String> {
        let files = match side {
            FileSide::Input => &self.input_files,
            FileSide::Output => &self.output_files,
        };
        files
            .iter()
            .filter_map(|p| p.file_name()?.to_str().map(|s| s.to_string()))
            .collect()
    }

    /// Same-stem counterpart on the other side (first match in that list).
    fn counterpart_name(&self, side: FileSide, name: &str) -> Option<String> {
        let stem = Self::file_stem_of(name);
        let other = match side {
            FileSide::Input => FileSide::Output,
            FileSide::Output => FileSide::Input,
        };
        self.names_for(other)
            .into_iter()
            .find(|n| Self::file_stem_of(n) == stem)
    }

    /// Input basename used for per-file encode logs (maps output → paired input).
    fn log_file_name(&self) -> Option<String> {
        let (side, name) = self.active.as_ref()?;
        match side {
            FileSide::Input => Some(name.clone()),
            FileSide::Output => self.counterpart_name(FileSide::Output, name),
        }
    }

    /// Select a file — preview comes from the pre-cache (no re-probe on click).
    fn select_file(&mut self, side: FileSide, name: String) {
        if self
            .active
            .as_ref()
            .is_some_and(|(s, n)| *s == side && n == &name)
        {
            return;
        }
        self.active = Some((side, name.clone()));
        self.log_show_session = false;
        let key = Self::preview_key(side, &name);
        self.preview_cache
            .entry(key)
            .or_insert_with(PreviewCacheEntry::pending);
    }

    /// Drop stale cache entries and pre-cache thumbs/metadata for input + output files.
    fn start_preview_cache(&mut self) {
        let mut jobs: Vec<(String, PathBuf)> = Vec::new();
        for (side, files, dir) in [
            (FileSide::Input, &self.input_files, &self.prefs.input_dir),
            (FileSide::Output, &self.output_files, &self.prefs.output_dir),
        ] {
            for path in files {
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let key = Self::preview_key(side, name);
                jobs.push((key, dir.join(name)));
            }
        }
        let key_set: std::collections::HashSet<_> = jobs.iter().map(|(k, _)| k.clone()).collect();
        self.preview_cache.retain(|k, _| key_set.contains(k));

        let mut to_fetch = Vec::new();
        for (key, path) in jobs {
            if self.preview_cache.get(&key).is_some_and(|e| e.ready()) {
                continue;
            }
            self.preview_cache
                .insert(key.clone(), PreviewCacheEntry::pending());
            to_fetch.push((key, path));
        }
        if to_fetch.is_empty() {
            return;
        }

        // Invalidate any prior worker, then start a new one for this folder snapshot.
        let gen = self.cache_gen.fetch_add(1, Ordering::Relaxed) + 1;
        let gen_flag = Arc::clone(&self.cache_gen);
        let (tx, rx) = mpsc::channel();
        self.cache_rx = Some(rx);
        thread::spawn(move || {
            for (name, path) in to_fetch {
                if gen_flag.load(Ordering::Relaxed) != gen {
                    break;
                }
                let summary = media_summary(&path);
                let dur = summary.as_ref().map(|s| s.duration_s).unwrap_or(0.0);
                let thumb = thumbnail_rgba(&path, dur);
                if tx
                    .send(PreviewReady {
                        gen,
                        name,
                        summary,
                        thumb,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    fn apply_preview_ready(&mut self, ctx: &egui::Context, msg: PreviewReady) {
        if msg.gen != self.cache_gen.load(Ordering::Relaxed) {
            return;
        }
        let mut texture = None;
        if let Ok(ref thumb) = msg.thumb {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [thumb.width as usize, thumb.height as usize],
                &thumb.rgba,
            );
            texture = Some(ctx.load_texture(
                format!("thumb-{}", msg.name),
                image,
                egui::TextureOptions::LINEAR,
            ));
        }
        let entry = self
            .preview_cache
            .entry(msg.name)
            .or_insert_with(PreviewCacheEntry::pending);
        entry.pending = false;
        entry.schema = MediaSummary::SCHEMA;
        entry.summary = Some(msg.summary);
        entry.thumb = Some(msg.thumb);
        entry.texture = texture;
    }

    fn poll_preview_cache(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.cache_rx.take() else {
            return;
        };
        let mut got_any = false;
        let mut keep = true;
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    got_any = true;
                    self.apply_preview_ready(ctx, msg);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    keep = false;
                    break;
                }
            }
        }
        if keep {
            self.cache_rx = Some(rx);
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }
        if got_any {
            ctx.request_repaint();
        }
    }

    /// Set input or output folder from a dropped path (folder, or parent of a file).
    fn set_folder_from_path(&mut self, is_input: bool, path: PathBuf) {
        let dir = if path.is_dir() {
            path
        } else if let Some(parent) = path.parent().filter(|p| p.is_dir()) {
            parent.to_path_buf()
        } else {
            self.push_session(format!("drop ignored (not a folder): {}", path.display()));
            self.log_open = true;
            return;
        };
        if is_input {
            self.prefs.input_dir = dir;
        } else {
            self.prefs.output_dir = dir;
        }
        let _ = prefs::save(&self.prefs);
        self.refresh_lists();
    }

    /// Route OS file drops onto the input/output pane under the pointer.
    fn handle_folder_dnd(&mut self, ctx: &egui::Context) {
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });

        let pointer = ctx.pointer_interact_pos().or_else(|| ctx.pointer_latest_pos());
        let target_input = pointer
            .and_then(|p| self.input_rect.map(|r| (r, p)))
            .is_some_and(|(r, p)| r.contains(p));
        let target_output = pointer
            .and_then(|p| self.output_rect.map(|r| (r, p)))
            .is_some_and(|(r, p)| r.contains(p));

        // Highlight the pane under a folder drag.
        if hovering {
            let stroke = egui::Stroke::new(2.0, egui::Color32::from_rgb(80, 160, 255));
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::unique("dnd_overlay"),
            ));
            if target_input {
                if let Some(r) = self.input_rect {
                    painter.rect_stroke(r, 4.0, stroke, egui::StrokeKind::Inside);
                }
            } else if target_output {
                if let Some(r) = self.output_rect {
                    painter.rect_stroke(r, 4.0, stroke, egui::StrokeKind::Inside);
                }
            }
        }

        if dropped.is_empty() {
            return;
        }
        let Some(path) = dropped.into_iter().next() else {
            return;
        };
        if target_input {
            self.set_folder_from_path(true, path);
        } else if target_output {
            self.set_folder_from_path(false, path);
        } else {
            self.push_session("drop a folder onto the Input or Output pane to set it");
            self.log_open = true;
        }
    }

    fn refresh_lists(&mut self) {
        self.input_files = if prefs::dir_ok(&self.prefs.input_dir) {
            list_media(&self.prefs.input_dir, true)
        } else {
            Vec::new()
        };
        self.output_files = if prefs::dir_ok(&self.prefs.output_dir) {
            list_media(&self.prefs.output_dir, false)
        } else {
            Vec::new()
        };
        // Drop selection if the file vanished.
        if let Some((side, name)) = self.active.clone() {
            let still = self.names_for(side).iter().any(|n| n == &name);
            if !still {
                self.clear_active();
            }
        }
        if self.input_files.is_empty() && self.output_files.is_empty() {
            self.cache_gen.fetch_add(1, Ordering::Relaxed);
            self.cache_rx = None;
            self.preview_cache.clear();
        } else {
            self.start_preview_cache();
        }
    }

    fn push_session(&mut self, line: impl Into<String>) {
        let line = line.into();
        let _ = prefs::append_session_log(&line);
        self.session_log.push(line);
        let before = self.session_log.len();
        trim_log(&mut self.session_log);
        // Keep the on-disk file capped to the same tail as the UI buffer.
        if self.session_log.len() < before {
            let _ = prefs::rewrite_session_log(&self.session_log);
        }
    }

    fn push_file_log(&mut self, file: &str, line: impl Into<String>) {
        let lines = self.file_logs.entry(file.to_string()).or_default();
        lines.push(line.into());
        trim_log(lines);
    }

    fn pick_input_folder(&mut self) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            self.prefs.input_dir = path;
            let _ = prefs::save(&self.prefs);
            self.refresh_lists();
        }
    }

    fn pick_output_folder(&mut self) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            self.prefs.output_dir = path;
            let _ = prefs::save(&self.prefs);
            self.refresh_lists();
        }
    }

    fn export_profile(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("GigaMux profile", &["json"])
            .set_file_name("gigamux-profile.json")
            .save_file()
        {
            match prefs::export_profile(&self.prefs, &path) {
                Ok(()) => self.push_session(format!("exported profile → {}", path.display())),
                Err(e) => self.push_session(format!("export failed: {e}")),
            }
            self.log_open = true;
        }
    }

    fn import_profile(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("GigaMux profile", &["json"])
            .pick_file()
        {
            match prefs::import_profile(&path) {
                Ok(p) => {
                    let (gpu, warn) = p.gpu.sanitize(&self.devices);
                    self.prefs = Prefs { gpu, ..p };
                    self.gpu_warn = warn;
                    let _ = prefs::save(&self.prefs);
                    self.refresh_lists();
                    self.push_session(format!("imported profile ← {}", path.display()));
                }
                Err(e) => self.push_session(format!("import failed: {e}")),
            }
            self.log_open = true;
        }
    }

    /// Encode all inputs, or only `only_name` when set (context-menu selection).
    fn start_encode(&mut self, only_name: Option<String>) {
        if self.busy {
            return;
        }
        if !prefs::dir_ok(&self.prefs.input_dir) || !prefs::dir_ok(&self.prefs.output_dir) {
            self.push_session("set both input and output folders before Encode");
            self.log_open = true;
            return;
        }
        let _ = prefs::save(&self.prefs);
        self.cancel.store(false, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        let opts = EncodeOptions {
            input_dir: self.prefs.input_dir.clone(),
            output_dir: self.prefs.output_dir.clone(),
            gpu: self.prefs.gpu,
            preserve_obs_metadata: self.prefs.preserve_obs_metadata,
            use_peak_bitrate: self.prefs.use_peak_bitrate,
            only_names: only_name.clone().map(|n| vec![n]),
        };
        let cancel = Arc::clone(&self.cancel);
        self.busy = true;
        self.job = Some(JobKind::Encode);
        self.encode_progress = None;
        self.event_rx = Some(rx);
        if let Some(n) = &only_name {
            self.push_session(format!("Encode started (selection: {n})…"));
        } else {
            self.push_session("Encode started (all)…");
        }
        self.log_open = true;
        thread::spawn(move || {
            run_batch(opts, cancel, |ev| {
                let _ = tx.send(AppEvent::Encode(ev));
            });
        });
    }

    /// Request cancel of the running encode batch (honored between files).
    fn cancel_encode(&mut self) {
        if matches!(self.job, Some(JobKind::Encode)) && self.busy {
            self.cancel.store(true, Ordering::Relaxed);
            self.push_session("cancel requested…");
            self.log_open = true;
        }
    }

    /// Move selection by `delta` (±1) within the active side's list.
    fn navigate_list(&mut self, delta: isize) {
        let Some((side, cur)) = self.active.clone() else {
            return;
        };
        let names = self.names_for(side);
        if names.is_empty() {
            return;
        }
        let Some(idx) = names.iter().position(|n| n == &cur) else {
            return;
        };
        let next = (idx as isize + delta).clamp(0, names.len() as isize - 1) as usize;
        if next != idx {
            self.select_file(side, names[next].clone());
        }
    }

    /// ← input / → output: jump to the same-stem file on that side.
    fn jump_to_side(&mut self, target: FileSide) {
        let Some((side, name)) = self.active.clone() else {
            return;
        };
        if side == target {
            return;
        }
        if let Some(other) = self.counterpart_name(side, &name) {
            self.select_file(target, other);
        }
    }

    /// Delete the active file from disk (Del). Refuses while a job is running.
    fn delete_active(&mut self) {
        let Some((side, name)) = self.active.clone() else {
            return;
        };
        if self.busy {
            self.push_session("can't delete while a job is running");
            self.log_open = true;
            return;
        }
        let dir = match side {
            FileSide::Input => &self.prefs.input_dir,
            FileSide::Output => &self.prefs.output_dir,
        };
        let path = dir.join(&name);
        // Prefer selecting the next neighbor in the same list after delete.
        let names = self.names_for(side);
        let idx = names.iter().position(|n| n == &name);
        let next_name = idx.and_then(|i| {
            names
                .get(i + 1)
                .or_else(|| (i > 0).then(|| &names[i - 1]))
                .cloned()
        });

        match std::fs::remove_file(&path) {
            Ok(()) => {
                self.push_session(format!("deleted {name}"));
                self.refresh_lists();
                if let Some(n) = next_name.filter(|n| self.names_for(side).iter().any(|x| x == n)) {
                    self.select_file(side, n);
                } else {
                    self.clear_active();
                }
            }
            Err(e) => {
                self.push_session(format!("delete failed ({name}): {e}"));
                self.log_open = true;
            }
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let (ctrl_j, f5, f12, esc, arrow_up, arrow_down, arrow_left, arrow_right, delete) =
            ctx.input(|i| {
                (
                    i.modifiers.command && i.key_pressed(egui::Key::J),
                    i.key_pressed(egui::Key::F5),
                    i.key_pressed(egui::Key::F12),
                    i.key_pressed(egui::Key::Escape),
                    i.key_pressed(egui::Key::ArrowUp),
                    i.key_pressed(egui::Key::ArrowDown),
                    i.key_pressed(egui::Key::ArrowLeft),
                    i.key_pressed(egui::Key::ArrowRight),
                    i.key_pressed(egui::Key::Delete),
                )
            });
        // Ctrl+J toggles log even while typing in a field.
        if ctrl_j {
            self.log_open = !self.log_open;
        }
        // Skip remaining keys when a text field has focus.
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        if f5 {
            self.refresh_lists();
            self.push_session("reloaded file lists");
        }
        if f12 {
            self.start_encode(None);
        }
        if esc {
            if self.show_settings {
                let _ = prefs::save(&self.prefs);
                self.show_settings = false;
            } else {
                self.cancel_encode();
            }
        }
        if arrow_up {
            self.navigate_list(-1);
        }
        if arrow_down {
            self.navigate_list(1);
        }
        if arrow_left {
            self.jump_to_side(FileSide::Input);
        }
        if arrow_right {
            self.jump_to_side(FileSide::Output);
        }
        if delete {
            self.delete_active();
        }
    }

    fn start_check(&mut self) {
        if self.busy {
            return;
        }
        if !prefs::dir_ok(&self.prefs.input_dir) || !prefs::dir_ok(&self.prefs.output_dir) {
            self.push_session("set both input and output folders before Check");
            self.log_open = true;
            return;
        }
        let input = self.prefs.input_dir.clone();
        let output = self.prefs.output_dir.clone();
        let (tx, rx) = mpsc::channel();
        self.busy = true;
        self.job = Some(JobKind::Check);
        self.event_rx = Some(rx);
        self.push_session("Check started…");
        self.log_open = true;
        thread::spawn(move || {
            let result = run_check(&input, &output);
            let _ = tx.send(AppEvent::CheckDone(result));
        });
    }

    fn poll_events(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.event_rx.take() else {
            return;
        };
        let mut finished = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AppEvent::Encode(EncodeEvent::Log(msg)) => {
                    if let Some(name) = self.log_file_name() {
                        self.push_file_log(&name, msg);
                    } else {
                        self.push_session(msg);
                    }
                }
                AppEvent::Encode(EncodeEvent::BatchStarted {
                    total_files,
                    total_duration_s,
                }) => {
                    let now = Instant::now();
                    self.encode_progress = Some(EncodeProgress {
                        total_files,
                        total_duration_s,
                        file_index: 0,
                        file_total: total_files,
                        file_name: String::new(),
                        file_duration_s: 0.0,
                        file_out_time_s: 0.0,
                        completed_duration_s: 0.0,
                        batch_started: now,
                        file_started: now,
                    });
                }
                AppEvent::Encode(EncodeEvent::FileStarted {
                    name,
                    index,
                    total,
                    duration_s,
                }) => {
                    self.select_file(FileSide::Input, name.clone());
                    self.push_file_log(&name, format!("encoding {name} ({index}/{total})"));
                    if let Some(p) = &mut self.encode_progress {
                        p.file_index = index;
                        p.file_total = total;
                        p.file_name = name;
                        p.file_duration_s = duration_s;
                        p.file_out_time_s = 0.0;
                        p.file_started = Instant::now();
                    }
                }
                AppEvent::Encode(EncodeEvent::FileProgress { out_time_s }) => {
                    if let Some(p) = &mut self.encode_progress {
                        p.file_out_time_s = out_time_s.max(p.file_out_time_s);
                    }
                }
                AppEvent::Encode(EncodeEvent::FileSkipped { name }) => {
                    self.push_file_log(&name, format!("skip (exists): {name}"));
                    self.push_session(format!("skip (exists): {name}"));
                }
                AppEvent::Encode(EncodeEvent::FileDone { name }) => {
                    self.push_file_log(&name, format!("done: {name}"));
                    self.push_session(format!("done: {name}"));
                    if let Some(p) = &mut self.encode_progress {
                        p.completed_duration_s += p.file_duration_s;
                        p.file_out_time_s = p.file_duration_s;
                    }
                }
                AppEvent::Encode(EncodeEvent::FileFailed { name, error }) => {
                    self.push_file_log(&name, format!("error: {error}"));
                    self.push_session(format!("error {name}: {error}"));
                    if let Some(p) = &mut self.encode_progress {
                        p.completed_duration_s += p.file_duration_s;
                        p.file_out_time_s = 0.0;
                    }
                }
                AppEvent::Encode(EncodeEvent::Finished {
                    ok,
                    skipped,
                    failed,
                }) => {
                    self.push_session(format!(
                        "finished — ok={ok} skipped={skipped} failed={failed}"
                    ));
                    finished = true;
                    self.refresh_lists();
                }
                AppEvent::CheckDone(result) => {
                    finished = true;
                    match result {
                        Ok(report) => self.apply_check_report(report),
                        Err(e) => self.push_session(format!("check failed: {e}")),
                    }
                    self.refresh_lists();
                }
            }
        }
        if finished {
            self.busy = false;
            self.job = None;
            self.encode_progress = None;
            self.event_rx = None;
        } else {
            self.event_rx = Some(rx);
            // Snappy refresh while ffmpeg progress lines stream in.
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    fn apply_check_report(&mut self, report: CheckReport) {
        self.log_open = true;
        self.log_show_session = true;
        if report.ok() {
            self.push_session(format!(
                "check ok — {} pair(s), no issues",
                report.ok_pairs
            ));
            self.pending_check = None;
            return;
        }
        for issue in &report.issues {
            let label = match (&issue.input, &issue.output) {
                (Some(i), Some(o)) => format!("{i} → {o}"),
                (Some(i), None) => i.clone(),
                (None, Some(o)) => o.clone(),
                (None, None) => "?".into(),
            };
            let line = format!("ISSUE {label}: {}", issue.message);
            if let Some(inp) = &issue.input {
                self.push_file_log(inp, &line);
            }
            self.push_session(line);
        }
        self.push_session(format!(
            "check done — ok_pairs={} issues={}",
            report.ok_pairs,
            report.issues.len()
        ));
        let deletable: Vec<CheckIssue> = deletable_issues(&self.prefs.output_dir, &report.issues)
            .into_iter()
            .cloned()
            .collect();
        // Always warn that files are not intact; delete only offered when outputs exist.
        self.pending_check = Some(CheckPrompt {
            issue_count: report.issues.len(),
            deletable,
        });
    }

    fn ui_menu(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("menu").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Import profile…").clicked() {
                        ui.close();
                        self.import_profile();
                    }
                    if ui.button("Export profile…").clicked() {
                        ui.close();
                        self.export_profile();
                    }
                });
            });
        });
    }

    fn ui_folder_pane(
        &mut self,
        ui: &mut egui::Ui,
        title: &str,
        dir: &Path,
        is_input: bool,
        files: &[PathBuf],
    ) {
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.strong(title);
                if prefs::dir_ok(dir) {
                    ui.label(egui::RichText::new(short_path(dir)).weak().small());
                    if ui.small_button("Change…").clicked() {
                        if is_input {
                            self.pick_input_folder();
                        } else {
                            self.pick_output_folder();
                        }
                    }
                }
            });
            ui.separator();

            if !prefs::dir_ok(dir) {
                let hovering = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
                let label = if hovering {
                    if is_input {
                        "Drop to set input folder"
                    } else {
                        "Drop to set output folder"
                    }
                } else if is_input {
                    "Select input folder\n(or drop a folder here)"
                } else {
                    "Select output folder\n(or drop a folder here)"
                };
                let size = ui.available_size();
                let btn = egui::Button::new(egui::RichText::new(label).heading()).min_size(size);
                if ui.add(btn).clicked() {
                    if is_input {
                        self.pick_input_folder();
                    } else {
                        self.pick_output_folder();
                    }
                }
                return;
            }

            let side = if is_input {
                FileSide::Input
            } else {
                FileSide::Output
            };
            egui::ScrollArea::vertical()
                .id_salt(if is_input { "in_list" } else { "out_list" })
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if files.is_empty() {
                        ui.label(egui::RichText::new("(empty)").weak());
                    } else {
                        for path in files {
                            let name = path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| path.display().to_string());
                            let selected = self
                                .active
                                .as_ref()
                                .is_some_and(|(s, n)| *s == side && n == &name);
                            let resp = ui.selectable_label(selected, &name);
                            if resp.clicked() {
                                self.select_file(side, name.clone());
                            }
                            if resp.double_clicked() {
                                self.select_file(side, name.clone());
                                if let Err(e) = open_path_in_os(path) {
                                    self.push_session(format!("open failed: {e}"));
                                }
                            }
                            resp.context_menu(|ui| {
                                if is_input
                                    && ui
                                        .add_enabled(
                                            !self.busy,
                                            egui::Button::new("Encode this file"),
                                        )
                                        .clicked()
                                {
                                    self.select_file(FileSide::Input, name.clone());
                                        self.start_encode(Some(name.clone()));
                                        ui.close();
                                    }
                                if ui.button("Open in player").clicked() {
                                    self.select_file(side, name.clone());
                                    if let Err(e) = open_path_in_os(path) {
                                        self.push_session(format!("open failed: {e}"));
                                    }
                                    ui.close();
                                }
                                if ui
                                    .add_enabled(!self.busy, egui::Button::new("Delete"))
                                    .clicked()
                                {
                                    self.select_file(side, name.clone());
                                    self.delete_active();
                                    ui.close();
                                }
                            });
                        }
                    }
                    // Click or click-drag blank space below the list to clear selection.
                    let fill = ui.available_size().max(egui::vec2(1.0, 24.0));
                    let blank = ui.allocate_response(fill, egui::Sense::click_and_drag());
                    if blank.clicked() || blank.drag_started() {
                        self.clear_active();
                    }
                });
        });
    }

    fn ui_center(&mut self, ui: &mut egui::Ui) {
        // Split preview vs actions into separate bands so log expand/collapse never lets Settings / Encode / Check overlap the preview (or each other).
        let full = ui.available_size();
        let actions_h = 130.0;
        let preview_h = (full.y - actions_h).max(48.0);

        ui.allocate_ui_with_layout(
            egui::vec2(full.x, preview_h),
            egui::Layout::top_down(egui::Align::Center),
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("center_preview")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(8.0);
                        self.ui_preview(ui);
                    });
            },
        );

        ui.allocate_ui_with_layout(
            egui::vec2(full.x, actions_h.min(ui.available_height().max(actions_h * 0.5))),
            egui::Layout::top_down(egui::Align::Center),
            |ui| {
                ui.add_space(8.0);
                if ui
                    .add_sized([72.0, 28.0], egui::Button::new("Settings"))
                    .clicked()
                {
                    self.show_settings = true;
                }
                ui.add_space(10.0);
                let encode = ui
                    .add_enabled(
                        !self.busy,
                        egui::Button::new("Encode all").min_size(egui::vec2(140.0, 36.0)),
                    )
                    .on_hover_text(
                        "Encode every file in the input folder (F12). Right-click a file to encode only that one.",
                    );
                if encode.clicked() {
                    self.start_encode(None);
                }
                ui.add_space(6.0);
                let check = ui.add_enabled(
                    !self.busy,
                    egui::Button::new("Check").min_size(egui::vec2(120.0, 28.0)),
                );
                if check.clicked() {
                    self.start_check();
                }
                if self.busy && self.encode_progress.is_none() {
                    let label = match self.job {
                        Some(JobKind::Encode) => "Encoding…",
                        Some(JobKind::Check) => "Checking…",
                        None => "Busy…",
                    };
                    ui.label(label);
                }
            },
        );
    }

    /// Dual encode bars sit in their own bottom panel (above the log), so the log strip never clips them.
    fn ui_progress_panel(&mut self, ui: &mut egui::Ui) {
        let Some(prog) = self.encode_progress.as_ref() else {
            return;
        };
        let file_caption = if prog.file_name.is_empty() {
            "Current".into()
        } else {
            format!(
                "{} ({}/{})",
                prog.file_name, prog.file_index, prog.file_total
            )
        };
        let file_frac = prog.file_frac();
        let file_label = prog.file_label();
        let batch_frac = prog.batch_frac();
        let batch_label = prog.batch_label();
        let batch_caption = format!(
            "All tasks ({} file{})",
            prog.total_files,
            if prog.total_files == 1 { "" } else { "s" }
        );

        egui::Panel::bottom("encode_progress")
            .exact_size(78.0)
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    let bar_w = (ui.available_width() - 24.0).clamp(160.0, 640.0);
                    ui.label(egui::RichText::new(file_caption).small());
                    Self::progress_bar_with_center_label(ui, bar_w, file_frac, &file_label);
                    ui.add_space(2.0);
                    ui.label(egui::RichText::new(batch_caption).small());
                    Self::progress_bar_with_center_label(ui, bar_w, batch_frac, &batch_label);
                });
            });
    }

    /// Progress fill with Rufus-style centered `elapsed / ~remaining` text.
    fn progress_bar_with_center_label(ui: &mut egui::Ui, width: f32, frac: f32, label: &str) {
        let height = 22.0;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
        let visuals = ui.style().visuals.clone();
        let rounding = 3.0;
        ui.painter()
            .rect_filled(rect, rounding, visuals.extreme_bg_color);
        let fill_w = (rect.width() * frac.clamp(0.0, 1.0)).max(if frac > 0.0 { 2.0 } else { 0.0 });
        if fill_w > 0.0 {
            let mut fill = rect;
            fill.set_width(fill_w);
            ui.painter()
                .rect_filled(fill, rounding, visuals.selection.bg_fill);
        }
        ui.painter().rect_stroke(
            rect,
            rounding,
            visuals.widgets.noninteractive.bg_stroke,
            egui::StrokeKind::Inside,
        );
        // Centered elapsed / ETA — pick contrast against the bar mid-point.
        let mid_x = rect.center().x;
        let filled_to = rect.left() + fill_w;
        let on_fill = mid_x <= filled_to;
        let color = if on_fill {
            visuals.strong_text_color()
        } else {
            visuals.text_color()
        };
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(13.0),
            color,
        );
    }

    fn ui_preview(&mut self, ui: &mut egui::Ui) {
        let Some((side, name)) = self.active.clone() else {
            ui.label(egui::RichText::new("Select a file").weak());
            return;
        };
        let key = Self::preview_key(side, &name);
        let Some(entry) = self.preview_cache.get(&key) else {
            ui.label("Loading preview…");
            return;
        };
        if entry.pending {
            ui.label("Loading preview…");
        }
        if let Some(tex) = &entry.texture {
            // Fit inside the center column; never spill into side panes.
            let max_w = ui.available_width().max(1.0);
            let tex_size = tex.size_vec2();
            let scale = (max_w / tex_size.x).min(1.0);
            let size = egui::vec2(tex_size.x * scale, tex_size.y * scale);
            ui.add(egui::Image::new((tex.id(), size)).maintain_aspect_ratio(true));
        }
        if let Some(Err(e)) = &entry.thumb {
            ui.colored_label(egui::Color32::from_rgb(200, 100, 80), format!("thumb: {e}"));
        }
        match &entry.summary {
            Some(Ok(s)) => {
                ui.add_space(8.0);
                Self::ui_meta_table(ui, s);
            }
            Some(Err(e)) => {
                ui.colored_label(egui::Color32::from_rgb(200, 100, 80), e);
            }
            None => {}
        }
    }

    fn ui_meta_table(ui: &mut egui::Ui, s: &MediaSummary) {
        let yes_no = |b: bool| if b { "yes" } else { "no" };
        let codec_v = match &s.video_profile {
            Some(p) => format!("{} ({p})", s.video_codec),
            None => s.video_codec.clone(),
        };
        let timing = match s.timing {
            FrameTiming::Cfr => "CFR",
            FrameTiming::Vfr => "VFR",
            FrameTiming::Unknown => "—",
        };
        let fps = if s.fps > 0.0 {
            if (s.fps - s.fps.round()).abs() < 0.05 {
                format!("{}", s.fps.round() as u32)
            } else {
                format!("{:.2}", s.fps)
            }
        } else {
            "—".into()
        };
        let res = if s.width > 0 && s.height > 0 {
            format!("{}×{} @ {fps} {timing}", s.width, s.height)
        } else {
            "—".into()
        };
        let codec_a = match &s.audio_profile {
            Some(p) => format!("{} ({p})", s.audio_codec),
            None => s.audio_codec.clone(),
        };
        let layout = if s.audio_sample_rate > 0 {
            format!(
                "{} · {} kHz",
                s.audio_layout,
                s.audio_sample_rate as f64 / 1000.0
            )
        } else {
            s.audio_layout.clone()
        };

        let row = |ui: &mut egui::Ui, label: &str, value: &str| {
            ui.label(egui::RichText::new(label).weak().small());
            ui.monospace(value);
            ui.end_row();
        };

        egui::Grid::new("meta_table")
            .num_columns(2)
            .spacing([10.0, 3.0])
            .min_col_width(72.0)
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Video").strong());
                ui.label("");
                ui.end_row();
                row(ui, "Codec", &codec_v);
                row(ui, "Container", &s.container.to_ascii_uppercase());
                row(ui, "Res", &res);
                row(ui, "Size", &format_bytes(s.file_size_bytes));
                row(ui, "Peakrate", &format!("{} kbps", s.peak_video_kbps));
                row(
                    ui,
                    "Avg rate",
                    &if s.avg_video_kbps > 0 {
                        format!("{} kbps", s.avg_video_kbps)
                    } else {
                        "—".into()
                    },
                );
                row(ui, "Length", &format_length(s.duration_s));
                row(
                    ui,
                    "Encoder",
                    s.encoder.as_deref().unwrap_or("—"),
                );
                row(ui, "OBS", yes_no(s.from_obs));
                row(
                    ui,
                    "OBS chapt",
                    &if s.has_obs_chapters {
                        format!("yes ({})", s.chapter_count)
                    } else {
                        "no".into()
                    },
                );
                if s.has_alpha {
                    row(ui, "Alpha", "yes");
                }

                // Blank row between sections (no add_space — illegal inside Grid).
                ui.label("");
                ui.label("");
                ui.end_row();

                ui.label(egui::RichText::new("Audio").strong());
                ui.label("");
                ui.end_row();
                row(ui, "Codec", &codec_a);
                if s.audio_track_count > 0 {
                    row(ui, "Tracks", &s.audio_track_count.to_string());
                }
                row(ui, "Layout", &layout);
                row(ui, "Peakrate", &format!("{} kbps", s.peak_audio_kbps));
            });
    }

    /// Center is thumb-width when a src is active, otherwise minimum; sides split the rest.
    fn column_widths(&self, viewport_w: f32) -> (f32, f32, f32, f32) {
        let sep = 8.0;
        let center = if self.active.is_some() {
            THUMB_W as f32
        } else {
            CENTER_MIN_W
        };
        let rem = (viewport_w - center - sep).max(0.0);
        let side = (rem / 2.0).max(SIDE_MIN_W);
        let content = side * 2.0 + center + sep;
        (side, center, side, content)
    }

    fn ui_settings(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        egui::Window::new("Settings")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("GPU");
                    let mut selected = match self.prefs.gpu {
                        GpuChoice::Any => None,
                        GpuChoice::Index(i) => Some(i),
                    };
                    egui::ComboBox::from_id_salt("gpu_settings")
                        .selected_text(match self.prefs.gpu {
                            GpuChoice::Any => "Any".to_string(),
                            GpuChoice::Index(i) => self
                                .devices
                                .iter()
                                .find(|d| d.index == i)
                                .map(|d| format!("#{i} {}", d.name))
                                .unwrap_or_else(|| format!("#{i}")),
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut selected, None, "Any");
                            for d in &self.devices {
                                ui.selectable_value(
                                    &mut selected,
                                    Some(d.index),
                                    format!("#{} {}", d.index, d.name),
                                );
                            }
                        });
                    self.prefs.gpu = match selected {
                        None => GpuChoice::Any,
                        Some(i) => GpuChoice::Index(i),
                    };
                    if ui.button("Refresh").clicked() {
                        match list_nvenc_devices() {
                            Ok(d) => {
                                let (gpu, warn) = self.prefs.gpu.sanitize(&d);
                                self.prefs.gpu = gpu;
                                self.gpu_warn = warn;
                                self.devices = d;
                            }
                            Err(e) => self.push_session(format!("GPU probe failed: {e}")),
                        }
                    }
                });
                if let Some(w) = &self.gpu_warn {
                    ui.colored_label(egui::Color32::from_rgb(200, 140, 40), w);
                }
                ui.checkbox(
                    &mut self.prefs.preserve_obs_metadata,
                    "Preserve OBS metadata",
                )
                .on_hover_text(
                    "Keep creation time / chapters / tags from OBS Hybrid MP4 across re-encode",
                );
                ui.checkbox(
                    &mut self.prefs.use_peak_bitrate,
                    "Ceiling from peak rate",
                )
                .on_hover_text(
                    "Off (default): use average video bitrate. On: use 1s video peak.",
                );
                ui.separator();
                if ui.button("Close").clicked() {
                    let _ = prefs::save(&self.prefs);
                    self.show_settings = false;
                }
            });
    }

    fn ui_log_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("log_bar")
            .exact_size(if self.log_open { 220.0 } else { 28.0 })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let log_name = self.log_file_name();
                    let showing_session = self.log_show_session || log_name.is_none();
                    let title = if showing_session {
                        "Log — session".to_string()
                    } else if let Some(n) = &log_name {
                        format!("Log — {n}")
                    } else {
                        "Log".to_string()
                    };
                    ui.label(title);
                    if log_name.is_some() {
                        // Button names the view you switch *to*.
                        let label = if self.log_show_session {
                            "File log"
                        } else {
                            "Session log"
                        };
                        if ui
                            .small_button(label)
                            .on_hover_text("Toggle session log vs active file log")
                            .clicked()
                        {
                            self.log_show_session = !self.log_show_session;
                            self.log_open = true;
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // ASCII caret — emoji triangles often missing from egui's default font.
                        let caret = if self.log_open { "v" } else { "^" };
                        if ui
                            .button(caret)
                            .on_hover_text(if self.log_open {
                                "Hide log (Ctrl+J)"
                            } else {
                                "Show log (Ctrl+J)"
                            })
                            .clicked()
                        {
                            self.log_open = !self.log_open;
                        }
                    });
                });
                if !self.log_open {
                    return;
                }
                ui.separator();
                let log_name = self.log_file_name();
                let showing_session = self.log_show_session || log_name.is_none();
                let lines: Vec<String> = if showing_session {
                    self.session_log.clone()
                } else if let Some(name) = log_name {
                    self.file_logs.get(&name).cloned().unwrap_or_default()
                } else {
                    self.session_log.clone()
                };
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if lines.is_empty() {
                            ui.label(egui::RichText::new("(no log yet)").weak());
                        } else {
                            for line in &lines {
                                let color = log_line_color(line);
                                ui.label(egui::RichText::new(line).monospace().color(color));
                            }
                        }
                    });
            });
    }

    fn ui_check_prompt(&mut self, ctx: &egui::Context) {
        let Some(prompt) = self.pending_check.clone() else {
            return;
        };
        egui::Window::new("Check found issues")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 40.0])
            .show(ctx, |ui| {
                ui.label(format!(
                    "{} file(s) are not intact (missing, mismatched, or incomplete).",
                    prompt.issue_count
                ));
                if prompt.deletable.is_empty() {
                    ui.label("No problematic output files to delete.");
                    ui.horizontal(|ui| {
                        if ui.button("Open session log").clicked() {
                            self.log_open = true;
                            self.log_show_session = true;
                            self.pending_check = None;
                        }
                        if ui.button("OK").clicked() {
                            self.pending_check = None;
                        }
                    });
                } else {
                    ui.label(format!(
                        "Delete {} problematic output file(s)?",
                        prompt.deletable.len()
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("Delete outputs").clicked() {
                            let (deleted, failed) =
                                delete_issue_outputs(&self.prefs.output_dir, &prompt.deletable);
                            self.push_session(format!(
                                "deleted {deleted} output(s); failed {failed}"
                            ));
                            self.pending_check = None;
                            self.refresh_lists();
                        }
                        if ui.button("Keep").clicked() {
                            self.pending_check = None;
                        }
                        if ui.button("Open session log").clicked() {
                            self.log_open = true;
                            self.log_show_session = true;
                            self.pending_check = None;
                        }
                    });
                }
            });
    }
}

fn log_line_color(line: &str) -> egui::Color32 {
    let lower = line.to_ascii_lowercase();
    if lower.starts_with("progress:") {
        egui::Color32::from_rgb(180, 120, 220)
    } else if lower.starts_with("ffmpeg:") {
        egui::Color32::from_rgb(90, 200, 120)
    } else if lower.contains("issue")
        || lower.contains("error")
        || lower.contains("fail")
        || lower.contains("incomplete")
        || lower.contains("missing")
    {
        egui::Color32::from_rgb(230, 90, 90)
    } else if lower.contains("skip") || lower.contains("cancel") || lower.contains("warn") {
        egui::Color32::from_rgb(220, 180, 60)
    } else if lower.contains("done")
        || lower.contains("check ok")
        || lower.starts_with("finished")
        || lower.contains("deleted")
    {
        egui::Color32::from_rgb(90, 200, 120)
    } else if lower.contains("encoding")
        || lower.contains("encode started")
        || lower.contains("check started")
    {
        egui::Color32::from_rgb(100, 170, 230)
    } else {
        egui::Color32::from_rgb(200, 200, 200)
    }
}

fn trim_log(lines: &mut Vec<String>) {
    if lines.len() > 500 {
        let drain = lines.len() - 500;
        lines.drain(0..drain);
    }
}

fn short_path(path: &Path) -> String {
    let s = path.display().to_string();
    if s.len() <= 48 {
        s
    } else {
        format!("…{}", &s[s.len() - 46..])
    }
}

/// Open a path with the OS default handler (same as double-click in Explorer).
fn open_path_in_os(path: &Path) -> Result<(), String> {
    let path = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf());
    #[cfg(target_os = "windows")]
    {
        // `start` needs an empty title arg when the path may be quoted/spaced.
        Command::new("cmd")
            .args(["/C", "start", "", &path.to_string_lossy()])
            .spawn()
            .map_err(|e| format!("start: {e}"))?;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("open: {e}"))?;
        return Ok(());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("xdg-open: {e}"))?;
        return Ok(());
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    {
        let _ = path;
        Err("open not supported on this OS".into())
    }
}

impl eframe::App for GigaMuxApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_events(ctx);
        // Keep elapsed / ETA labels ticking while encode bars are visible.
        if self.encode_progress.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        self.poll_preview_cache(ctx);
        self.handle_shortcuts(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // DnD paints an overlay, so it belongs in ui (not logic).
        self.handle_folder_dnd(&ctx);
        self.ui_menu(ui);
        // Bottom panels stack upward: log first (edge), then progress above it.
        self.ui_log_panel(ui);
        self.ui_progress_panel(ui);

        egui::CentralPanel::default().show(ui, |ui| {
            let total = ui.available_size();
            let (in_w, center_w, out_w, content_w) = self.column_widths(total.x);
            let scroll = content_w > total.x + 0.5;

            let mut show_cols = |ui: &mut egui::Ui| {
                let in_resp = ui.allocate_ui_with_layout(
                    egui::vec2(in_w, total.y),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        let files = self.input_files.clone();
                        let dir = self.prefs.input_dir.clone();
                        self.ui_folder_pane(ui, "Input", &dir, true, &files);
                    },
                );
                self.input_rect = Some(in_resp.response.rect);
                ui.separator();
                ui.allocate_ui_with_layout(
                    egui::vec2(center_w, total.y),
                    egui::Layout::top_down(egui::Align::Center),
                    |ui| self.ui_center(ui),
                );
                ui.separator();
                let out_resp = ui.allocate_ui_with_layout(
                    egui::vec2(out_w, total.y),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        let files = self.output_files.clone();
                        let dir = self.prefs.output_dir.clone();
                        self.ui_folder_pane(ui, "Output", &dir, false, &files);
                    },
                );
                self.output_rect = Some(out_resp.response.rect);
            };

            if scroll {
                egui::ScrollArea::horizontal()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_min_width(content_w);
                        ui.horizontal(|ui| show_cols(ui));
                    });
            } else {
                ui.horizontal(|ui| show_cols(ui));
            }
        });

        self.ui_settings(&ctx);
        self.ui_check_prompt(&ctx);
    }

    fn on_exit(&mut self) {
        let _ = prefs::save(&self.prefs);
    }
}
