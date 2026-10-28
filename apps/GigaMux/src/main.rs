//! GigaMux native desktop shell (egui/eframe — no webview).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use gigamux_core::check::{
    deletable_issues, delete_issue_outputs, list_media, permanent_delete_file, restore_trashed_file,
    run_check, trash_file, CheckIssue, CheckReport,
};
use gigamux_core::encode::{run_batch, EncodeEvent, EncodeOptions};
use gigamux_core::gpu::{list_nvenc_devices, GpuChoice, NvencDevice};
use gigamux_core::prefs::{self, Prefs};
use gigamux_core::preview_cache::{self, PersistedPreviewCache, PersistedPreviewEntry};
use gigamux_core::probe::{
    codec_heat, codec_tag, container_heat, enrich_summary_bitrates, format_bytes, format_length,
    media_summary_fast, thumbnail_rgba, FrameTiming, MediaSummary, RgbaThumb, THUMB_W,
};
use gigamux_core::targets::{
    audio_encode_for, audio_must_recode, cycle_alpha_container, cycle_opaque_container,
    AudioEncode, OutContainer, OutputTargets,
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

/// Incremental preview worker messages (fast chips first, then peeks + thumb).
enum PreviewMsg {
    Fast {
        gen: u64,
        name: String,
        summary: Result<MediaSummary, String>,
    },
    Bitrates {
        gen: u64,
        name: String,
        peak_video_kbps: u32,
        avg_video_kbps: u32,
        peak_audio_kbps: u32,
    },
    Thumb {
        gen: u64,
        name: String,
        thumb: Result<RgbaThumb, String>,
    },
}

/// Cached thumb + metadata for one file (RAM + optional AppData persistence).
struct PreviewCacheEntry {
    summary: Option<Result<MediaSummary, String>>,
    thumb: Option<Result<RgbaThumb, String>>,
    texture: Option<egui::TextureHandle>,
    /// Still waiting on the fast probe.
    pending: bool,
    /// Peak/avg packet sampling finished (meta table rates are final).
    bitrates_ready: bool,
    /// Matches [`MediaSummary::SCHEMA`] when filled so stale shapes re-fetch.
    schema: u32,
    /// Absolute path + fingerprint for freshness checks across sessions.
    source_path: Option<PathBuf>,
    file_size: u64,
    mtime_secs: i64,
}

impl PreviewCacheEntry {
    fn pending() -> Self {
        Self {
            summary: None,
            thumb: None,
            texture: None,
            pending: true,
            bitrates_ready: false,
            schema: 0,
            source_path: None,
            file_size: 0,
            mtime_secs: 0,
        }
    }

    /// Chips / basic meta are usable (bitrates + thumb may still be loading).
    fn ready(&self) -> bool {
        !self.pending && self.summary.is_some() && self.schema == MediaSummary::SCHEMA
    }

    /// Ready and still matches the file on disk at `path`.
    fn fresh_for(&self, path: &Path) -> bool {
        if !self.ready() {
            return false;
        }
        let Some((size, mtime)) = preview_cache::file_fingerprint(path) else {
            return false;
        };
        if self.file_size != size || self.mtime_secs != mtime {
            return false;
        }
        match &self.source_path {
            Some(src) if src == path => true,
            Some(src) => match (std::fs::canonicalize(src), std::fs::canonicalize(path)) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            },
            None => false,
        }
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
    /// Primary selection (preview / log focus): side + basename.
    active: Option<(FileSide, String)>,
    /// Multi-select set on the same side as `active` (always includes the primary name).
    selected: HashSet<String>,
    /// Shift-click range anchor (basename on the active side).
    selection_anchor: Option<String>,
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
    /// Confirm before Recycle Bin (unless the user opted out).
    pending_recycle_delete: Option<RecycleDeletePrompt>,
    /// Recycle Bin failed — confirm permanent delete (or proceed if warnings disabled).
    pending_permanent_delete: Option<PermanentDeletePrompt>,
    /// One-by-one delete cascade (list shrinks between each file).
    delete_queue: Option<DeleteQueue>,
    /// Blender-style waypoint history (delete steps + cursor); jump undoes/redoes across Recycle Bin.
    undo_history: UndoHistory,
    /// Active history jump (restore and/or re-trash cascades toward a waypoint).
    history_nav: Option<HistoryNav>,
    /// Last-frame hit rects for folder drag-and-drop targeting.
    input_rect: Option<egui::Rect>,
    output_rect: Option<egui::Rect>,
    /// Invalidates in-flight pre-cache workers when the input folder changes.
    cache_gen: Arc<AtomicU64>,
    preview_cache: HashMap<String, PreviewCacheEntry>,
    cache_rx: Option<Receiver<PreviewMsg>>,
    /// Dual progress bars while an encode batch is running.
    encode_progress: Option<EncodeProgress>,
}

/// Pace between one-by-one delete / restore steps so the list visibly moves.
const DELETE_STEP_MS: u64 = 90;
/// Cap Ctrl+Z undo gestures (not individual files) so the stack cannot grow without bound.
const DELETE_UNDO_CAP: usize = 50;

/// Sequential Recycle Bin job — pops one basename per step so rows vanish one-by-one.
struct DeleteQueue {
    side: FileSide,
    remaining: VecDeque<String>,
    next_name: Option<String>,
    next_at: Instant,
    /// Paths successfully recycled this cascade — committed as one Ctrl+Z batch when done.
    recycled: Vec<PathBuf>,
}

/// One Del / check-delete gesture registered as a history waypoint.
#[derive(Clone)]
struct DeleteUndoBatch {
    side: FileSide,
    /// Absolute paths as they were before trash (Recycle Bin original_path match).
    paths: Vec<PathBuf>,
}

/// Waypoint history: `cursor == 0` is Original; `cursor == steps.len()` is the tip.
struct UndoHistory {
    /// Delete steps oldest → newest. Truncated when a new delete happens while not at the tip.
    steps: Vec<DeleteUndoBatch>,
    cursor: usize,
}

impl UndoHistory {
    fn new() -> Self {
        Self {
            steps: Vec::new(),
            cursor: 0,
        }
    }

    fn label_at(&self, pos: usize) -> String {
        if pos == 0 {
            "Original".into()
        } else {
            self.steps[pos - 1].action_label()
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HistoryDir {
    /// Restore files from the Recycle Bin (move cursor down).
    Undo,
    /// Re-trash files (move cursor up / redo).
    Redo,
}

/// One history jump: pending batches, then a per-file cascade for the active batch.
struct HistoryNav {
    target: usize,
    pending: VecDeque<(HistoryDir, DeleteUndoBatch)>,
    dir: HistoryDir,
    side: FileSide,
    remaining: VecDeque<PathBuf>,
    next_at: Instant,
    touched: Vec<String>,
}

impl DeleteUndoBatch {
    /// Menu / session label for this undo step (e.g. `Delete "a.mp4"`).
    fn action_label(&self) -> String {
        let names: Vec<String> = self
            .paths
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        match names.as_slice() {
            [] => "Delete".into(),
            [one] => format!("Delete \"{one}\""),
            [a, b] => format!("Delete \"{a}\", \"{b}\""),
            _ => format!(
                "Delete \"{}\", \"{}\", … ({} files)",
                names[0],
                names[1],
                names.len()
            ),
        }
    }
}

/// Post-check dialog: warn about non-intact files; optional delete of bad outputs.
#[derive(Clone)]
struct CheckPrompt {
    issue_count: usize,
    deletable: Vec<CheckIssue>,
}

/// Shared fields for delete confirmation dialogs (one or many basenames).
#[derive(Clone)]
struct DeletePromptBase {
    side: FileSide,
    names: Vec<String>,
    next_name: Option<String>,
    dont_ask_again: bool,
}

/// Confirm moving a file to the Recycle Bin.
#[derive(Clone)]
struct RecycleDeletePrompt {
    base: DeletePromptBase,
}

/// Confirm a permanent delete after Recycle Bin failed.
#[derive(Clone)]
struct PermanentDeletePrompt {
    base: DeletePromptBase,
    trash_error: String,
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
            selected: HashSet::new(),
            selection_anchor: None,
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
            pending_recycle_delete: None,
            pending_permanent_delete: None,
            delete_queue: None,
            undo_history: UndoHistory::new(),
            history_nav: None,
            input_rect: None,
            output_rect: None,
            cache_gen: Arc::new(AtomicU64::new(0)),
            preview_cache: HashMap::new(),
            cache_rx: None,
            encode_progress: None,
        };
        app.load_preview_cache_from_disk();
        app.refresh_lists();
        let started = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        app.push_session(format!("—— session start {started} ——"));
        app
    }

    fn clear_active(&mut self) {
        self.active = None;
        self.selected.clear();
        self.selection_anchor = None;
        self.log_show_session = true;
    }

    fn is_selected(&self, side: FileSide, name: &str) -> bool {
        self.active
            .as_ref()
            .is_some_and(|(s, _)| *s == side)
            && self.selected.contains(name)
    }

    /// Ordered selected basenames on the active side (list order).
    fn selected_names_ordered(&self) -> Vec<String> {
        let Some((side, _)) = self.active else {
            return Vec::new();
        };
        self.names_for(side)
            .into_iter()
            .filter(|n| self.selected.contains(n))
            .collect()
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

    /// Plain click: single-select `name` as primary (clears multi-select).
    fn select_file(&mut self, side: FileSide, name: String) {
        self.selected.clear();
        self.selected.insert(name.clone());
        self.selection_anchor = Some(name.clone());
        self.set_primary(side, name);
    }

    /// Ctrl/Cmd-click: toggle membership; primary follows the clicked row.
    fn select_toggle(&mut self, side: FileSide, name: String) {
        if self.active.as_ref().is_some_and(|(s, _)| *s != side) {
            self.select_file(side, name);
            return;
        }
        if self.selected.contains(&name) {
            self.selected.remove(&name);
            if self.selected.is_empty() {
                self.clear_active();
                return;
            }
            if self.active.as_ref().is_some_and(|(_, n)| n == &name) {
                // Primary was removed — pick another selected name (stable list order).
                if let Some(next) = self.names_for(side).into_iter().find(|n| self.selected.contains(n))
                {
                    self.set_primary(side, next);
                }
            }
        } else {
            self.selected.insert(name.clone());
            self.selection_anchor = Some(name.clone());
            self.set_primary(side, name);
        }
    }

    /// Shift-click: select inclusive range from anchor → `name` on the same side.
    fn select_range(&mut self, side: FileSide, name: String) {
        let names = self.names_for(side);
        let Some(anchor) = self
            .selection_anchor
            .clone()
            .or_else(|| self.active.as_ref().filter(|(s, _)| *s == side).map(|(_, n)| n.clone()))
        else {
            self.select_file(side, name);
            return;
        };
        if self.active.as_ref().is_some_and(|(s, _)| *s != side) {
            self.select_file(side, name);
            return;
        }
        let Some(i0) = names.iter().position(|n| n == &anchor) else {
            self.select_file(side, name);
            return;
        };
        let Some(i1) = names.iter().position(|n| n == &name) else {
            return;
        };
        let (lo, hi) = if i0 <= i1 { (i0, i1) } else { (i1, i0) };
        self.selected = names[lo..=hi].iter().cloned().collect();
        self.set_primary(side, name);
    }

    fn set_primary(&mut self, side: FileSide, name: String) {
        self.active = Some((side, name.clone()));
        self.selected.insert(name.clone());
        self.log_show_session = false;
        let key = Self::preview_key(side, &name);
        self.preview_cache
            .entry(key)
            .or_insert_with(PreviewCacheEntry::pending);
    }

    /// Basename after a multi-delete (first survivor below the block, else above).
    fn next_name_after_delete(side_names: &[String], deleting: &HashSet<String>) -> Option<String> {
        let indices: Vec<usize> = side_names
            .iter()
            .enumerate()
            .filter(|(_, n)| deleting.contains(*n))
            .map(|(i, _)| i)
            .collect();
        let Some(&last) = indices.last() else {
            return None;
        };
        side_names
            .iter()
            .skip(last + 1)
            .find(|n| !deleting.contains(*n))
            .cloned()
            .or_else(|| {
                indices
                    .first()
                    .and_then(|&first| {
                        (0..first)
                            .rev()
                            .map(|i| &side_names[i])
                            .find(|n| !deleting.contains(*n))
                            .cloned()
                    })
            })
    }

    /// Preview keys for recycled files still on the undo / cascade stacks (keep chips + metadata).
    fn parked_preview_keys(&self) -> HashSet<String> {
        let mut keys = HashSet::new();
        let mut park = |side: FileSide, path: &Path| {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                keys.insert(Self::preview_key(side, name));
            }
        };
        for batch in &self.undo_history.steps {
            for path in &batch.paths {
                park(batch.side, path);
            }
        }
        if let Some(q) = &self.delete_queue {
            for path in &q.recycled {
                park(q.side, path);
            }
        }
        if let Some(nav) = &self.history_nav {
            for path in &nav.remaining {
                park(nav.side, path);
            }
            for (d, batch) in &nav.pending {
                let _ = d;
                for path in &batch.paths {
                    park(batch.side, path);
                }
            }
        }
        keys
    }

    /// Hydrate RAM cache from AppData (textures filled later via [`Self::hydrate_preview_textures`]).
    fn load_preview_cache_from_disk(&mut self) {
        let disk = preview_cache::load(MediaSummary::SCHEMA);
        for (key, pe) in disk.entries {
            let thumb = if pe.thumb_err.is_some() || pe.thumb_file.is_some() {
                Some(preview_cache::load_thumb(&pe))
            } else {
                None
            };
            self.preview_cache.insert(
                key,
                PreviewCacheEntry {
                    summary: Some(pe.summary),
                    thumb,
                    texture: None,
                    pending: false,
                    bitrates_ready: pe.bitrates_ready,
                    schema: MediaSummary::SCHEMA,
                    source_path: Some(pe.path),
                    file_size: pe.size,
                    mtime_secs: pe.mtime_secs,
                },
            );
        }
    }

    /// Write RAM cache to AppData (meta JSON + thumb files).
    fn save_preview_cache_to_disk(&self) {
        let mut entries = HashMap::new();
        for (key, e) in &self.preview_cache {
            if !e.ready() {
                continue;
            }
            let Some(path) = e.source_path.clone() else {
                continue;
            };
            let Some(summary) = e.summary.clone() else {
                continue;
            };
            let (thumb_file, thumb_width, thumb_height, thumb_err) = match &e.thumb {
                Some(Ok(thumb)) => match preview_cache::write_thumb(key, thumb) {
                    Ok(name) => (Some(name), thumb.width, thumb.height, None),
                    Err(err) => (None, 0, 0, Some(err)),
                },
                Some(Err(err)) => (None, 0, 0, Some(err.clone())),
                None => (None, 0, 0, None),
            };
            entries.insert(
                key.clone(),
                PersistedPreviewEntry {
                    path,
                    size: e.file_size,
                    mtime_secs: e.mtime_secs,
                    summary,
                    thumb_file,
                    thumb_width,
                    thumb_height,
                    thumb_err,
                    bitrates_ready: e.bitrates_ready,
                },
            );
        }
        let disk = PersistedPreviewCache {
            schema: MediaSummary::SCHEMA,
            entries,
        };
        let _ = preview_cache::save(&disk);
    }

    /// Wipe RAM + AppData preview cache (Ctrl+F5).
    fn clear_preview_cache(&mut self) {
        self.cache_gen.fetch_add(1, Ordering::Relaxed);
        self.cache_rx = None;
        self.preview_cache.clear();
        let _ = preview_cache::clear();
    }

    /// Build egui textures for disk-loaded thumbs that have none yet.
    fn hydrate_preview_textures(&mut self, ctx: &egui::Context) {
        for (key, entry) in self.preview_cache.iter_mut() {
            if entry.texture.is_some() {
                continue;
            }
            let Some(Ok(thumb)) = entry.thumb.as_ref() else {
                continue;
            };
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [thumb.width as usize, thumb.height as usize],
                &thumb.rgba,
            );
            entry.texture = Some(ctx.load_texture(
                format!("thumb-{key}"),
                image,
                egui::TextureOptions::LINEAR,
            ));
        }
    }

    fn preview_path_for_key(&self, key: &str) -> Option<PathBuf> {
        if let Some(name) = key.strip_prefix("i:") {
            Some(self.prefs.input_dir.join(name))
        } else if let Some(name) = key.strip_prefix("o:") {
            Some(self.prefs.output_dir.join(name))
        } else {
            None
        }
    }

    /// True when an encode job is using software libvpx (opaque VP8/9 or any α file).
    fn cpu_encode_active(&self) -> bool {
        if !(self.busy && matches!(self.job, Some(JobKind::Encode))) {
            return false;
        }
        if self.prefs.opaque_video.is_libvpx() {
            return true;
        }
        // Alpha delivers always use libvpx — throttle if any known-α input is in the batch.
        self.input_files.iter().any(|p| {
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                return false;
            };
            self.preview_cache
                .get(&Self::preview_key(FileSide::Input, name))
                .and_then(|e| e.summary.as_ref())
                .and_then(|r| r.as_ref().ok())
                .is_some_and(|s| s.has_alpha)
        })
    }

    /// All logical CPUs for preview workers; leave most free while CPU encode runs.
    fn preview_worker_count(&self) -> usize {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .max(1);
        if self.cpu_encode_active() {
            (cores / 4).max(1)
        } else {
            cores
        }
    }

    /// Drop stale cache entries; fast-probe chips in parallel, then enrich peeks + thumbs.
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
        let key_set: HashSet<_> = jobs.iter().map(|(k, _)| k.clone()).collect();
        let parked = self.parked_preview_keys();
        self.preview_cache
            .retain(|k, _| key_set.contains(k) || parked.contains(k));

        // Full probe vs enrich-only (disk hit with chips but missing peeks/thumb).
        let mut to_fast = VecDeque::new();
        let mut to_enrich = VecDeque::new();
        for (key, path) in jobs {
            if let Some(e) = self.preview_cache.get(&key) {
                if e.fresh_for(&path) {
                    let need_rates = !e.bitrates_ready;
                    let need_thumb = e.thumb.is_none();
                    if need_rates || need_thumb {
                        to_enrich.push_back((key, path, need_rates, need_thumb));
                    }
                    continue;
                }
            }
            self.preview_cache
                .insert(key.clone(), PreviewCacheEntry::pending());
            to_fast.push_back((key, path));
        }
        if to_fast.is_empty() && to_enrich.is_empty() {
            return;
        }

        let gen = self.cache_gen.fetch_add(1, Ordering::Relaxed) + 1;
        let gen_flag = Arc::clone(&self.cache_gen);
        let (tx, rx) = mpsc::channel();
        self.cache_rx = Some(rx);

        let fast_q = Arc::new(Mutex::new(to_fast));
        let enrich_q = Arc::new(Mutex::new(to_enrich));
        let workers = self.preview_worker_count();
        for _ in 0..workers {
            let fast_q = Arc::clone(&fast_q);
            let enrich_q = Arc::clone(&enrich_q);
            let gen_flag = Arc::clone(&gen_flag);
            let tx = tx.clone();
            thread::spawn(move || {
                loop {
                    if gen_flag.load(Ordering::Relaxed) != gen {
                        break;
                    }
                    // Prefer fast chip probes so the list lights up before peeks/thumbs.
                    let next_fast = fast_q.lock().ok().and_then(|mut q| q.pop_front());
                    if let Some((name, path)) = next_fast {
                        let summary = media_summary_fast(&path);
                        if tx
                            .send(PreviewMsg::Fast {
                                gen,
                                name: name.clone(),
                                summary: summary.clone(),
                            })
                            .is_err()
                        {
                            break;
                        }
                        if let Ok(mut s) = summary {
                            enrich_summary_bitrates(&path, &mut s);
                            let _ = tx.send(PreviewMsg::Bitrates {
                                gen,
                                name: name.clone(),
                                peak_video_kbps: s.peak_video_kbps,
                                avg_video_kbps: s.avg_video_kbps,
                                peak_audio_kbps: s.peak_audio_kbps,
                            });
                            let thumb = thumbnail_rgba(&path, s.duration_s);
                            let _ = tx.send(PreviewMsg::Thumb { gen, name, thumb });
                        }
                        continue;
                    }
                    let next_enrich = enrich_q.lock().ok().and_then(|mut q| q.pop_front());
                    let Some((name, path, need_rates, need_thumb)) = next_enrich else {
                        break;
                    };
                    // Disk/RAM already has chips — only finish missing peeks / thumb.
                    let Ok(mut s) = media_summary_fast(&path) else {
                        continue;
                    };
                    if need_rates {
                        enrich_summary_bitrates(&path, &mut s);
                        if tx
                            .send(PreviewMsg::Bitrates {
                                gen,
                                name: name.clone(),
                                peak_video_kbps: s.peak_video_kbps,
                                avg_video_kbps: s.avg_video_kbps,
                                peak_audio_kbps: s.peak_audio_kbps,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    if need_thumb {
                        let thumb = thumbnail_rgba(&path, s.duration_s);
                        let _ = tx.send(PreviewMsg::Thumb { gen, name, thumb });
                    }
                }
            });
        }
    }

    fn apply_preview_msg(&mut self, ctx: &egui::Context, msg: PreviewMsg) {
        match msg {
            PreviewMsg::Fast { gen, name, summary } => {
                if gen != self.cache_gen.load(Ordering::Relaxed) {
                    return;
                }
                let (source_path, file_size, mtime_secs) =
                    match self.preview_path_for_key(&name) {
                        Some(p) => {
                            let (sz, mt) = preview_cache::file_fingerprint(&p).unwrap_or((0, 0));
                            (Some(p), sz, mt)
                        }
                        None => (None, 0, 0),
                    };
                let entry = self
                    .preview_cache
                    .entry(name)
                    .or_insert_with(PreviewCacheEntry::pending);
                entry.pending = false;
                entry.bitrates_ready = false;
                entry.schema = MediaSummary::SCHEMA;
                entry.summary = Some(summary);
                entry.source_path = source_path;
                entry.file_size = file_size;
                entry.mtime_secs = mtime_secs;
            }
            PreviewMsg::Bitrates {
                gen,
                name,
                peak_video_kbps,
                avg_video_kbps,
                peak_audio_kbps,
            } => {
                if gen != self.cache_gen.load(Ordering::Relaxed) {
                    return;
                }
                let Some(entry) = self.preview_cache.get_mut(&name) else {
                    return;
                };
                if let Some(Ok(s)) = entry.summary.as_mut() {
                    s.peak_video_kbps = peak_video_kbps;
                    s.avg_video_kbps = avg_video_kbps;
                    s.peak_audio_kbps = peak_audio_kbps;
                }
                entry.bitrates_ready = true;
            }
            PreviewMsg::Thumb { gen, name, thumb } => {
                if gen != self.cache_gen.load(Ordering::Relaxed) {
                    return;
                }
                let mut texture = None;
                if let Ok(ref t) = thumb {
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [t.width as usize, t.height as usize],
                        &t.rgba,
                    );
                    texture = Some(ctx.load_texture(
                        format!("thumb-{name}"),
                        image,
                        egui::TextureOptions::LINEAR,
                    ));
                }
                let entry = self
                    .preview_cache
                    .entry(name)
                    .or_insert_with(PreviewCacheEntry::pending);
                entry.thumb = Some(thumb);
                entry.texture = texture;
            }
        }
    }

    fn poll_preview_cache(&mut self, ctx: &egui::Context) {
        self.hydrate_preview_textures(ctx);
        let Some(rx) = self.cache_rx.take() else {
            return;
        };
        let mut got_any = false;
        let mut keep = true;
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    got_any = true;
                    self.apply_preview_msg(ctx, msg);
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
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
        } else {
            // All workers done — persist whatever we have (chips + peeks + thumbs).
            self.save_preview_cache_to_disk();
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
        // Prune multi-select / primary if files vanished from disk.
        if let Some((side, name)) = self.active.clone() {
            let alive: HashSet<String> = self.names_for(side).into_iter().collect();
            self.selected.retain(|n| alive.contains(n));
            if let Some(a) = self.selection_anchor.clone() {
                if !alive.contains(&a) {
                    self.selection_anchor = self.selected.iter().next().cloned();
                }
            }
            if !alive.contains(&name) {
                if let Some(next) = self.selected.iter().next().cloned() {
                    self.set_primary(side, next);
                } else {
                    self.clear_active();
                }
            } else if self.selected.is_empty() {
                self.selected.insert(name);
            }
        }
        if self.input_files.is_empty() && self.output_files.is_empty() {
            self.cache_gen.fetch_add(1, Ordering::Relaxed);
            self.cache_rx = None;
            // Preserve undo-parked thumbs/metadata even when both panes are empty.
            let parked = self.parked_preview_keys();
            self.preview_cache.retain(|k, _| parked.contains(k));
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
                Ok(mut p) => {
                    p.sanitize_targets();
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
        if self.busy || self.delete_queue.is_some() || self.history_nav.is_some() {
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
            opaque_video: self.prefs.opaque_video,
            opaque_container: self.prefs.opaque_container,
            alpha_video: self.prefs.alpha_video,
            alpha_container: self.prefs.alpha_container,
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

    /// Delete all selected files (Del). Refuses while a job / delete cascade is running.
    /// Warns before Recycle Bin / permanent delete unless the user opted out.
    fn delete_active(&mut self) {
        let Some((side, _)) = self.active else {
            return;
        };
        if self.busy {
            self.push_session("can't delete while a job is running");
            self.log_open = true;
            return;
        }
        if self.pending_recycle_delete.is_some()
            || self.pending_permanent_delete.is_some()
            || self.delete_queue.is_some()
            || self.history_nav.is_some()
        {
            return;
        }
        let names = self.selected_names_ordered();
        if names.is_empty() {
            return;
        }
        let side_names = self.names_for(side);
        let deleting: HashSet<String> = names.iter().cloned().collect();
        let next_name = Self::next_name_after_delete(&side_names, &deleting);
        let base = DeletePromptBase {
            side,
            names,
            next_name,
            dont_ask_again: false,
        };
        if self.prefs.warn_recycle_delete {
            self.pending_recycle_delete = Some(RecycleDeletePrompt { base });
        } else {
            self.start_delete_queue(base);
        }
    }

    /// Begin a paced one-by-one Recycle Bin cascade.
    fn start_delete_queue(&mut self, base: DeletePromptBase) {
        let DeletePromptBase {
            side,
            names,
            next_name,
            ..
        } = base;
        if names.is_empty() {
            return;
        }
        self.delete_queue = Some(DeleteQueue {
            side,
            remaining: names.into(),
            next_name,
            next_at: Instant::now(),
            recycled: Vec::new(),
        });
    }

    /// Commit the cascade's recycled paths as a single Ctrl+Z batch, then clear the queue.
    fn finish_delete_queue(&mut self) {
        let Some(q) = self.delete_queue.take() else {
            return;
        };
        let side = q.side;
        let next_name = q.next_name;
        if !q.recycled.is_empty() {
            self.push_delete_undo_batch(DeleteUndoBatch {
                side,
                paths: q.recycled,
            });
        }
        self.finish_delete_selection(side, next_name);
    }

    /// Pop one file from the delete queue, refresh the list, then wait briefly before the next.
    fn tick_delete_queue(&mut self, ctx: &egui::Context) {
        if self.pending_permanent_delete.is_some() {
            return;
        }
        let Some(q) = self.delete_queue.as_ref() else {
            return;
        };
        let now = Instant::now();
        if now < q.next_at {
            ctx.request_repaint_after(q.next_at.saturating_duration_since(now));
            return;
        }
        let side = q.side;
        let name = {
            let q = self.delete_queue.as_mut().unwrap();
            q.remaining.pop_front()
        };
        let Some(name) = name else {
            self.finish_delete_queue();
            return;
        };

        let dir = match side {
            FileSide::Input => self.prefs.input_dir.clone(),
            FileSide::Output => self.prefs.output_dir.clone(),
        };
        let path = dir.join(&name);
        // Capture before trash — canonicalize fails once the file is gone.
        let undo_path = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());

        match trash_file(&path) {
            Ok(()) => {
                if let Some(q) = self.delete_queue.as_mut() {
                    q.recycled.push(undo_path);
                }
                self.push_session(format!("moved to Recycle Bin: {name}"));
                self.after_one_deleted(side, &name);
            }
            Err(e) if e == "not present on disk" => {
                self.push_session(format!("delete failed ({name}): {e}"));
                self.log_open = true;
                self.after_one_deleted(side, &name);
            }
            Err(e) => {
                // Pause the cascade until permanent delete is confirmed or skipped.
                if self.prefs.warn_permanent_delete {
                    let next_name = self.delete_queue.as_ref().and_then(|q| q.next_name.clone());
                    self.pending_permanent_delete = Some(PermanentDeletePrompt {
                        base: DeletePromptBase {
                            side,
                            names: vec![name],
                            next_name,
                            dont_ask_again: false,
                        },
                        trash_error: e,
                    });
                    return;
                }
                match permanent_delete_file(&path) {
                    Ok(()) => {
                        self.push_session(format!("permanently deleted: {name}"));
                        self.after_one_deleted(side, &name);
                    }
                    Err(e2) => {
                        self.push_session(format!("permanent delete failed ({name}): {e2}"));
                        self.log_open = true;
                        self.after_one_deleted(side, &name);
                    }
                }
            }
        }

        if let Some(q) = self.delete_queue.as_mut() {
            q.next_at = Instant::now() + Duration::from_millis(DELETE_STEP_MS);
        }
        ctx.request_repaint();
    }

    /// Drop one name from selection and re-list so the row vanishes this frame.
    /// Preview cache stays until the undo stack releases it (chips survive Del→restore).
    fn after_one_deleted(&mut self, side: FileSide, name: &str) {
        self.selected.remove(name);
        if self.selection_anchor.as_deref() == Some(name) {
            self.selection_anchor = self.selected.iter().next().cloned();
        }
        self.refresh_lists();
        if self.active.as_ref().is_some_and(|(s, n)| *s == side && n == name) {
            if let Some(n) = self
                .names_for(side)
                .into_iter()
                .find(|n| self.selected.contains(n))
            {
                self.set_primary(side, n);
            } else {
                // Keep focus empty while the cascade continues; finish_delete_selection restores.
                self.active = None;
                self.log_show_session = true;
            }
        }
    }

    /// Re-list from disk and restore selection after the cascade finishes.
    fn finish_delete_selection(&mut self, side: FileSide, next_name: Option<String>) {
        self.refresh_lists();
        if let Some(n) = next_name.filter(|n| self.names_for(side).iter().any(|x| x == n)) {
            self.select_file(side, n);
        } else {
            self.clear_active();
        }
    }

    /// Register a completed delete at the tip (truncates any redo branch first).
    fn push_delete_undo_batch(&mut self, batch: DeleteUndoBatch) {
        if batch.paths.is_empty() {
            return;
        }
        let h = &mut self.undo_history;
        h.steps.truncate(h.cursor);
        h.steps.push(batch);
        h.cursor = h.steps.len();
        while h.steps.len() > DELETE_UNDO_CAP {
            h.steps.remove(0);
            h.cursor = h.cursor.saturating_sub(1);
        }
    }

    /// Select every touched basename on `side` (primary = last in list order).
    fn select_restored(&mut self, side: FileSide, names: &[String]) {
        let present: Vec<String> = self
            .names_for(side)
            .into_iter()
            .filter(|n| names.iter().any(|x| x == n))
            .collect();
        if present.is_empty() {
            self.clear_active();
            return;
        }
        self.selected = present.iter().cloned().collect();
        self.selection_anchor = present.first().cloned();
        if let Some(primary) = present.last().cloned() {
            self.set_primary(side, primary);
        }
    }

    fn history_idle(&self) -> bool {
        !self.busy && self.delete_queue.is_none() && self.history_nav.is_none()
    }

    fn can_undo_delete(&self) -> bool {
        self.history_idle() && self.undo_history.cursor > 0
    }

    fn can_redo_delete(&self) -> bool {
        self.history_idle() && self.undo_history.cursor < self.undo_history.steps.len()
    }

    /// Ctrl+Z: move one waypoint toward Original.
    fn undo_last_delete(&mut self) {
        if self.undo_history.cursor == 0 {
            self.push_session("nothing to undo");
            return;
        }
        self.history_jump(self.undo_history.cursor - 1);
    }

    /// Shift+Ctrl+Z: move one waypoint toward the tip.
    fn redo_last_delete(&mut self) {
        if self.undo_history.cursor >= self.undo_history.steps.len() {
            self.push_session("nothing to redo");
            return;
        }
        self.history_jump(self.undo_history.cursor + 1);
    }

    /// Jump to a waypoint index (`0` = Original, `steps.len()` = tip). Blender Undo History.
    fn history_jump(&mut self, target: usize) {
        if !self.history_idle() {
            self.push_session("can't jump history while a job is running");
            self.log_open = true;
            return;
        }
        let target = target.min(self.undo_history.steps.len());
        let cursor = self.undo_history.cursor;
        if target == cursor {
            return;
        }
        let mut pending = VecDeque::new();
        if target < cursor {
            // Undo: restore steps[target..cursor] newest-first.
            for i in (target..cursor).rev() {
                pending.push_back((HistoryDir::Undo, self.undo_history.steps[i].clone()));
            }
        } else {
            // Redo: re-trash steps[cursor..target] oldest-first.
            for i in cursor..target {
                pending.push_back((HistoryDir::Redo, self.undo_history.steps[i].clone()));
            }
        }
        self.history_nav = Some(HistoryNav {
            target,
            pending,
            dir: HistoryDir::Undo,
            side: FileSide::Input,
            remaining: VecDeque::new(),
            next_at: Instant::now(),
            touched: Vec::new(),
        });
        self.history_nav_start_next_batch();
    }

    /// Pull the next pending batch into the per-file cascade (or finish the jump).
    fn history_nav_start_next_batch(&mut self) {
        let Some(nav) = self.history_nav.as_mut() else {
            return;
        };
        let Some((dir, batch)) = nav.pending.pop_front() else {
            let target = nav.target;
            let touched = nav.touched.clone();
            let side = nav.side;
            self.history_nav = None;
            self.undo_history.cursor = target;
            self.refresh_lists();
            if !touched.is_empty() {
                self.select_restored(side, &touched);
            }
            return;
        };
        // Advance cursor as each batch completes directionally at start of batch.
        match dir {
            HistoryDir::Undo => {
                self.undo_history.cursor = self.undo_history.cursor.saturating_sub(1);
            }
            HistoryDir::Redo => {
                self.undo_history.cursor =
                    (self.undo_history.cursor + 1).min(self.undo_history.steps.len());
            }
        }
        let remaining: VecDeque<PathBuf> = match dir {
            // Last-trashed file reappears first.
            HistoryDir::Undo => batch.paths.into_iter().rev().collect(),
            // Same order as the original delete cascade.
            HistoryDir::Redo => batch.paths.into_iter().collect(),
        };
        let side = batch.side;
        if let Some(nav) = self.history_nav.as_mut() {
            nav.dir = dir;
            nav.side = side;
            nav.remaining = remaining;
            nav.next_at = Instant::now();
            nav.touched.clear();
        }
        if self
            .history_nav
            .as_ref()
            .is_some_and(|n| n.remaining.is_empty())
        {
            self.history_nav_start_next_batch();
        }
    }

    /// Esc: abort the jump; cursor already reflects completed batches.
    fn cancel_history_nav(&mut self) {
        let Some(nav) = self.history_nav.take() else {
            return;
        };
        let side = nav.side;
        let touched = nav.touched;
        self.refresh_lists();
        if !touched.is_empty() {
            self.select_restored(side, &touched);
        }
        self.push_session("history jump cancelled");
    }

    /// Drive one file step of the active history jump.
    fn tick_history_nav(&mut self, ctx: &egui::Context) {
        let Some(nav) = self.history_nav.as_ref() else {
            return;
        };
        let now = Instant::now();
        if now < nav.next_at {
            ctx.request_repaint_after(nav.next_at.saturating_duration_since(now));
            return;
        }
        if nav.remaining.is_empty() && nav.pending.is_empty() {
            self.history_nav_start_next_batch();
            return;
        }
        if nav.remaining.is_empty() {
            // Batch done — select what this batch touched, then continue.
            let side = nav.side;
            let touched = nav.touched.clone();
            self.refresh_lists();
            if !touched.is_empty() {
                self.select_restored(side, &touched);
            }
            self.history_nav_start_next_batch();
            ctx.request_repaint();
            return;
        }

        let dir = nav.dir;
        let side = nav.side;
        let path = {
            let nav = self.history_nav.as_mut().unwrap();
            nav.remaining.pop_front().unwrap()
        };
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());

        match dir {
            HistoryDir::Undo => match restore_trashed_file(&path) {
                Ok(()) => {
                    self.push_session(format!("restored from Recycle Bin: {name}"));
                    if let Some(nav) = self.history_nav.as_mut() {
                        nav.touched.push(name.clone());
                    }
                    self.refresh_lists();
                    if let Some(nav) = self.history_nav.as_ref() {
                        let names = nav.touched.clone();
                        self.select_restored(side, &names);
                    }
                }
                Err(e) => {
                    self.push_session(format!("undo failed ({name}): {e}"));
                    self.log_open = true;
                }
            },
            HistoryDir::Redo => match trash_file(&path) {
                Ok(()) => {
                    self.push_session(format!("moved to Recycle Bin: {name}"));
                    if let Some(nav) = self.history_nav.as_mut() {
                        nav.touched.push(name.clone());
                    }
                    self.after_one_deleted(side, &name);
                }
                Err(e) => {
                    self.push_session(format!("redo delete failed ({name}): {e}"));
                    self.log_open = true;
                }
            },
        }

        if let Some(nav) = self.history_nav.as_mut() {
            nav.next_at = Instant::now() + Duration::from_millis(DELETE_STEP_MS);
        }
        ctx.request_repaint();
    }

    /// Permanent-delete the prompted name(s), then resume the recycle cascade if any.
    fn execute_permanent_delete(
        &mut self,
        side: FileSide,
        names: Vec<String>,
        next_name: Option<String>,
    ) {
        let dir = match side {
            FileSide::Input => self.prefs.input_dir.clone(),
            FileSide::Output => self.prefs.output_dir.clone(),
        };
        for name in &names {
            let path = dir.join(name);
            match permanent_delete_file(&path) {
                Ok(()) => {
                    self.push_session(format!("permanently deleted: {name}"));
                    self.after_one_deleted(side, name);
                }
                Err(e) => {
                    self.push_session(format!("permanent delete failed ({name}): {e}"));
                    self.log_open = true;
                    self.after_one_deleted(side, name);
                }
            }
        }
        if self.delete_queue.is_some() {
            if let Some(q) = self.delete_queue.as_mut() {
                q.next_at = Instant::now() + Duration::from_millis(DELETE_STEP_MS);
            }
        } else {
            self.finish_delete_selection(side, next_name);
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let (ctrl_j, ctrl_z, ctrl_shift_z, f5, ctrl_f5, f12, esc, arrow_up, arrow_down, arrow_left, arrow_right, delete) =
            ctx.input(|i| {
                let z = i.key_pressed(egui::Key::Z);
                let f5_pressed = i.key_pressed(egui::Key::F5);
                (
                    i.modifiers.command && i.key_pressed(egui::Key::J),
                    i.modifiers.command && !i.modifiers.shift && z,
                    i.modifiers.command && i.modifiers.shift && z,
                    f5_pressed && !i.modifiers.command,
                    f5_pressed && i.modifiers.command,
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
        if ctrl_shift_z {
            self.redo_last_delete();
        } else if ctrl_z {
            self.undo_last_delete();
        }
        // F5 = re-index files (keep meta/thumb cache); Ctrl+F5 = wipe cache then re-index.
        if ctrl_f5 {
            self.clear_preview_cache();
            self.refresh_lists();
            self.push_session("cleared preview cache; reloaded file lists");
        } else if f5 {
            self.refresh_lists();
            self.push_session("reloaded file lists");
        }
        if f12 {
            self.start_encode(None);
        }
        if esc {
            if self.pending_permanent_delete.is_some() {
                self.pending_permanent_delete = None;
                if let Some(q) = self.delete_queue.as_mut() {
                    q.next_at = Instant::now() + Duration::from_millis(DELETE_STEP_MS);
                }
            } else if self.pending_recycle_delete.is_some() {
                self.pending_recycle_delete = None;
            } else if self.delete_queue.is_some() {
                self.finish_delete_queue();
                self.push_session("delete cancelled");
            } else if self.history_nav.is_some() {
                self.cancel_history_nav();
            } else if self.show_settings {
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
        let targets = OutputTargets {
            opaque_video: self.prefs.opaque_video,
            opaque_container: self.prefs.opaque_container,
            alpha_video: self.prefs.alpha_video,
            alpha_container: self.prefs.alpha_container,
        };
        let (tx, rx) = mpsc::channel();
        self.busy = true;
        self.job = Some(JobKind::Check);
        self.event_rx = Some(rx);
        self.push_session("Check started…");
        self.log_open = true;
        thread::spawn(move || {
            let result = run_check(&input, &output, targets);
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
                ui.menu_button("Edit", |ui| {
                    let can_undo = self.can_undo_delete();
                    let can_redo = self.can_redo_delete();
                    let undo_sc = ui.ctx().format_shortcut(&egui::KeyboardShortcut::new(
                        egui::Modifiers::COMMAND,
                        egui::Key::Z,
                    ));
                    let redo_sc = ui.ctx().format_shortcut(&egui::KeyboardShortcut::new(
                        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                        egui::Key::Z,
                    ));
                    // Undo names the current waypoint action; Redo names the next tipward step.
                    let undo_label = if self.undo_history.cursor > 0 {
                        format!(
                            "Undo {}",
                            self.undo_history.label_at(self.undo_history.cursor)
                        )
                    } else {
                        "Undo".into()
                    };
                    let redo_label = if self.undo_history.cursor < self.undo_history.steps.len() {
                        format!(
                            "Redo {}",
                            self.undo_history
                                .label_at(self.undo_history.cursor + 1)
                        )
                    } else {
                        "Redo".into()
                    };
                    if ui
                        .add_enabled(can_undo, egui::Button::new(undo_label).shortcut_text(undo_sc))
                        .clicked()
                    {
                        ui.close();
                        self.undo_last_delete();
                    }
                    if ui
                        .add_enabled(can_redo, egui::Button::new(redo_label).shortcut_text(redo_sc))
                        .clicked()
                    {
                        ui.close();
                        self.redo_last_delete();
                    }
                    // Hover submenu: waypoints newest → Original; ● = current; click jumps.
                    ui.menu_button("Undo History", |ui| {
                        let idle = self.history_idle();
                        let cursor = self.undo_history.cursor;
                        let tip = self.undo_history.steps.len();
                        for pos in (0..=tip).rev() {
                            let label = self.undo_history.label_at(pos);
                            let is_here = pos == cursor;
                            ui.horizontal(|ui| {
                                if is_here {
                                    ui.label("●");
                                } else {
                                    ui.add_space(14.0);
                                }
                                let enabled = idle && !is_here;
                                if ui
                                    .add_enabled(enabled, egui::Button::new(label))
                                    .clicked()
                                {
                                    ui.close();
                                    self.history_jump(pos);
                                }
                            });
                        }
                    });
                });
            });
        });
    }

    /// Persist output-target toggles after a chip click.
    fn save_targets(&mut self) {
        self.prefs.sanitize_targets();
        let _ = prefs::save(&self.prefs);
    }

    /// Amber line when any input would need audio re-encode for current targets.
    /// Opaque WebM (typically Opus) → MP4 always warns once the file is listed.
    fn audio_recode_warning(&self) -> Option<String> {
        for path in &self.input_files {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let key = Self::preview_key(FileSide::Input, &name);
            let entry = self.preview_cache.get(&key);
            let Some(summary) = entry.and_then(|e| e.summary.as_ref()).and_then(|r| r.as_ref().ok())
            else {
                continue;
            };
            let cont = self.prefs.container_for(summary.has_alpha);
            let src_webm = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("webm"))
                || summary.container.eq_ignore_ascii_case("webm");
            let raw = summary.audio_codec.to_ascii_lowercase();
            let raw = if raw == "—" { String::new() } else { raw };

            // Opaque WebM → MP4: Opus (usual WebM audio) must become AAC.
            if !summary.has_alpha && src_webm && matches!(cont, OutContainer::Mp4) {
                return Some(format!("Opus will be recoded to AAC for {}", cont.label()));
            }

            if raw.is_empty() || !audio_must_recode(&raw, cont) {
                continue;
            }
            let src = match raw.as_str() {
                "opus" | "libopus" => "Opus",
                "vorbis" => "Vorbis",
                "aac" => "AAC",
                "mp3" => "MP3",
                "flac" => "FLAC",
                other => other,
            };
            let dest = match audio_encode_for(&raw, cont) {
                AudioEncode::Aac => "AAC",
                AudioEncode::Opus => "Opus",
                AudioEncode::Copy => continue,
            };
            return Some(format!("{src} will be recoded to {dest} for {}", cont.label()));
        }
        None
    }

    fn ui_output_targets(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Targets").strong().small());
        // One cycling codec chip + one cycling container chip per path.
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Opaque").weak().small());
            let v = self.prefs.opaque_video;
            if select_codec_chip(ui, v.label(), true, true) {
                let next = v.next();
                self.prefs.opaque_video = next;
                self.prefs.opaque_container =
                    gigamux_core::targets::clamp_opaque_container(next, self.prefs.opaque_container);
                self.save_targets();
            }
            let c = self.prefs.opaque_container;
            if select_ext_chip(ui, c.label(), true, true) {
                self.prefs.opaque_container = cycle_opaque_container(self.prefs.opaque_video, c);
                self.save_targets();
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("α").weak().small());
            let v = self.prefs.alpha_video;
            if select_codec_chip(ui, v.label(), true, true) {
                let next = v.next();
                self.prefs.alpha_video = next;
                self.prefs.alpha_container =
                    gigamux_core::targets::clamp_alpha_container(next, self.prefs.alpha_container);
                self.save_targets();
            }
            let c = self.prefs.alpha_container;
            if select_ext_chip(ui, c.label(), true, true) {
                self.prefs.alpha_container = cycle_alpha_container(self.prefs.alpha_video, c);
                self.save_targets();
            }
        });
        if let Some(warn) = self.audio_recode_warning() {
            ui.colored_label(egui::Color32::from_rgb(220, 160, 60), warn);
        }
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
            // Output targets sit above the list (and above the empty-folder CTA).
            if !is_input {
                self.ui_output_targets(ui);
            }
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
                            let selected = self.is_selected(side, &name);
                            let ext = path
                                .extension()
                                .and_then(|e| e.to_str())
                                .unwrap_or("")
                                .to_ascii_lowercase();
                            // Codec (+ optional α) chips from preview cache; muted … while probing.
                            let cache_key = Self::preview_key(side, &name);
                            let tags = self.preview_cache.get(&cache_key).and_then(|e| {
                                if e.pending {
                                    return Some((None, false));
                                }
                                e.summary.as_ref().and_then(|r| r.as_ref().ok()).map(|s| {
                                    (Some(codec_tag(s)), s.has_alpha)
                                })
                            });
                            let resp = ui
                                .horizontal(|ui| {
                                    let resp = ui.selectable_label(selected, &name);
                                    match &tags {
                                        Some((None, _)) => codec_chip(ui, "…"),
                                        Some((Some(tag), has_alpha)) => {
                                            codec_chip(ui, tag);
                                            if *has_alpha {
                                                alpha_chip(ui);
                                            }
                                        }
                                        None => {}
                                    }
                                    if !ext.is_empty() {
                                        ext_chip(ui, &format!(".{ext}"), true);
                                    }
                                    resp
                                })
                                .inner;
                            if resp.clicked() {
                                let (shift, ctrl) = ui.ctx().input(|i| {
                                    (i.modifiers.shift, i.modifiers.command)
                                });
                                if shift {
                                    self.select_range(side, name.clone());
                                } else if ctrl {
                                    self.select_toggle(side, name.clone());
                                } else {
                                    self.select_file(side, name.clone());
                                }
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
                                let del_label = if self.is_selected(side, &name)
                                    && self.selected.len() > 1
                                {
                                    format!("Delete {} selected", self.selected.len())
                                } else {
                                    "Delete".into()
                                };
                                if ui
                                    .add_enabled(!self.busy, egui::Button::new(del_label))
                                    .clicked()
                                {
                                    if !self.is_selected(side, &name) {
                                        self.select_file(side, name.clone());
                                    }
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
        let bitrates_ready = entry.bitrates_ready;
        match &entry.summary {
            Some(Ok(s)) => {
                ui.add_space(8.0);
                Self::ui_meta_table(ui, s, bitrates_ready);
            }
            Some(Err(e)) => {
                ui.colored_label(egui::Color32::from_rgb(200, 100, 80), e);
            }
            None => {}
        }
    }

    fn ui_meta_table(ui: &mut egui::Ui, s: &MediaSummary, bitrates_ready: bool) {
        let yes_no = |b: bool| if b { "yes" } else { "no" };
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
                ui.label(egui::RichText::new("Codec").weak().small());
                ui.horizontal(|ui| {
                    codec_chip(ui, &codec_tag(s));
                    if s.has_alpha {
                        alpha_chip(ui);
                    }
                    if let Some(p) = &s.video_profile {
                        ui.monospace(format!("({p})"));
                    }
                });
                ui.end_row();
                row(ui, "Container", &s.container.to_ascii_uppercase());
                row(ui, "Res", &res);
                row(ui, "Size", &format_bytes(s.file_size_bytes));
                row(
                    ui,
                    "Peakrate",
                    &if bitrates_ready {
                        format!("{} kbps", s.peak_video_kbps)
                    } else {
                        "…".into()
                    },
                );
                row(
                    ui,
                    "Avg rate",
                    &if !bitrates_ready {
                        "…".into()
                    } else if s.avg_video_kbps > 0 {
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
                row(
                    ui,
                    "Peakrate",
                    &if bitrates_ready {
                        format!("{} kbps", s.peak_audio_kbps)
                    } else {
                        "…".into()
                    },
                );
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
                ui.checkbox(
                    &mut self.prefs.warn_recycle_delete,
                    "Warn before Recycle Bin",
                )
                .on_hover_text(
                    "Confirm before moving a file to the Recycle Bin. Uncheck via the dialog’s “Don’t ask again.”",
                );
                ui.checkbox(
                    &mut self.prefs.warn_permanent_delete,
                    "Warn before permanent delete",
                )
                .on_hover_text(
                    "When Recycle Bin fails, confirm before permanently deleting. Uncheck via the dialog’s “Don’t ask again.”",
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
                            // Read-only TextEdit: click-drag select, Ctrl+A, copy; keeps line colors via layouter.
                            let text = lines.join("\n");
                            let mut text_ref: &str = text.as_str();
                            let salt = if showing_session {
                                "session_log_text"
                            } else {
                                "file_log_text"
                            };
                            ui.add(
                                egui::TextEdit::multiline(&mut text_ref)
                                    .id_salt(salt)
                                    .desired_width(ui.available_width())
                                    .frame(egui::Frame::NONE)
                                    .margin(egui::Margin::ZERO)
                                    .layouter(&mut |ui, buf, wrap_width| {
                                        layout_log_text(ui, buf.as_str(), wrap_width)
                                    }),
                            );
                        }
                    });
            });
    }

    fn ui_recycle_delete_prompt(&mut self, ctx: &egui::Context) {
        let Some(mut prompt) = self.pending_recycle_delete.clone() else {
            return;
        };
        let n = prompt.base.names.len();
        egui::Window::new("Move to Recycle Bin")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                if n == 1 {
                    ui.label("Move this file to the Recycle Bin?");
                    ui.label(egui::RichText::new(&prompt.base.names[0]).strong());
                } else {
                    ui.label(format!("Move {n} files to the Recycle Bin?"));
                    for name in prompt.base.names.iter().take(8) {
                        ui.label(egui::RichText::new(name).strong());
                    }
                    if n > 8 {
                        ui.label(egui::RichText::new(format!("…and {} more", n - 8)).weak());
                    }
                }
                ui.add_space(6.0);
                ui.checkbox(&mut prompt.base.dont_ask_again, "Don't ask again");
                if let Some(p) = self.pending_recycle_delete.as_mut() {
                    p.base.dont_ask_again = prompt.base.dont_ask_again;
                }
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.pending_recycle_delete = None;
                    }
                    if ui.button("Move to Recycle Bin").clicked() {
                        if prompt.base.dont_ask_again {
                            self.prefs.warn_recycle_delete = false;
                            let _ = prefs::save(&self.prefs);
                        }
                        self.pending_recycle_delete = None;
                        self.start_delete_queue(prompt.base);
                    }
                });
            });
    }

    fn ui_permanent_delete_prompt(&mut self, ctx: &egui::Context) {
        let Some(mut prompt) = self.pending_permanent_delete.clone() else {
            return;
        };
        let n = prompt.base.names.len();
        egui::Window::new("Permanent delete")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 140, 60),
                    if n == 1 {
                        "Recycle Bin failed — this will permanently delete the file."
                    } else {
                        "Recycle Bin failed — this will permanently delete these files."
                    },
                );
                for name in prompt.base.names.iter().take(8) {
                    ui.label(egui::RichText::new(name).strong());
                }
                if n > 8 {
                    ui.label(egui::RichText::new(format!("…and {} more", n - 8)).weak());
                }
                ui.label(
                    egui::RichText::new(format!("({})", prompt.trash_error))
                        .weak()
                        .small(),
                );
                ui.add_space(6.0);
                ui.checkbox(&mut prompt.base.dont_ask_again, "Don't ask again");
                if let Some(p) = self.pending_permanent_delete.as_mut() {
                    p.base.dont_ask_again = prompt.base.dont_ask_again;
                }
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.pending_permanent_delete = None;
                        // Skip this file and continue the one-by-one cascade.
                        if let Some(q) = self.delete_queue.as_mut() {
                            q.next_at = Instant::now() + Duration::from_millis(DELETE_STEP_MS);
                        }
                    }
                    let del = ui.add(
                        egui::Button::new("Delete permanently")
                            .fill(egui::Color32::from_rgb(160, 50, 40)),
                    );
                    if del.clicked() {
                        if prompt.base.dont_ask_again {
                            self.prefs.warn_permanent_delete = false;
                            let _ = prefs::save(&self.prefs);
                        }
                        self.pending_permanent_delete = None;
                        self.execute_permanent_delete(
                            prompt.base.side,
                            prompt.base.names,
                            prompt.base.next_name,
                        );
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
                            let (deleted, failed, trashed) =
                                delete_issue_outputs(&self.prefs.output_dir, &prompt.deletable);
                            self.push_delete_undo_batch(DeleteUndoBatch {
                                side: FileSide::Output,
                                paths: trashed,
                            });
                            self.push_session(format!(
                                "Recycle Bin: {deleted} output(s); failed {failed}"
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

/// Markdown-backtick-style chip: monospace label on a filled rounded rect.
fn tag_chip(ui: &mut egui::Ui, label: &str, bg: egui::Color32, fg: egui::Color32) {
    egui::Frame::new()
        .fill(bg)
        .corner_radius(3.0)
        .inner_margin(egui::Margin::symmetric(5, 1))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(label)
                    .monospace()
                    .size(12.0)
                    .color(fg),
            );
        });
}

/// Clickable chip; returns true when clicked while enabled.
fn select_tag_chip(
    ui: &mut egui::Ui,
    label: &str,
    bg: egui::Color32,
    fg: egui::Color32,
    enabled: bool,
) -> bool {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let inner = egui::Frame::new()
        .fill(bg)
        .corner_radius(3.0)
        .inner_margin(egui::Margin::symmetric(5, 1))
        .show(ui, |ui| {
            ui.add_enabled(
                enabled,
                egui::Label::new(
                    egui::RichText::new(label)
                        .monospace()
                        .size(12.0)
                        .color(fg),
                )
                .sense(sense),
            )
        });
    enabled && inner.inner.clicked()
}

/// Codec chip: warm→cool spectrum tint from [`codec_heat`].
fn codec_chip(ui: &mut egui::Ui, tag: &str) {
    let pending = tag == "…";
    let (bg, fg) = if pending {
        chip_neutral(ui.visuals().dark_mode)
    } else {
        heat_chip_colors(codec_heat(tag), ui.visuals().dark_mode)
    };
    tag_chip(ui, tag, bg, fg);
}

/// Extension chip (file row or target toggle).
fn ext_chip(ui: &mut egui::Ui, label: &str, selected_or_plain: bool) {
    let dark = ui.visuals().dark_mode;
    let (bg, fg) = if selected_or_plain {
        heat_chip_colors(container_heat(label), dark)
    } else {
        chip_neutral(dark)
    };
    tag_chip(ui, label, bg, fg);
}

/// Selectable codec chip for Targets row.
fn select_codec_chip(ui: &mut egui::Ui, label: &str, selected: bool, enabled: bool) -> bool {
    let dark = ui.visuals().dark_mode;
    let (bg, fg) = if !enabled {
        let (b, f) = chip_neutral(dark);
        (b.gamma_multiply(0.7), f.gamma_multiply(0.55))
    } else if selected {
        heat_chip_colors(codec_heat(label), dark)
    } else {
        chip_neutral(dark)
    };
    select_tag_chip(ui, label, bg, fg, enabled)
}

/// Selectable extension chip for Targets row.
fn select_ext_chip(ui: &mut egui::Ui, label: &str, selected: bool, enabled: bool) -> bool {
    let dark = ui.visuals().dark_mode;
    let (bg, fg) = if !enabled {
        let (b, f) = chip_neutral(dark);
        (b.gamma_multiply(0.7), f.gamma_multiply(0.55))
    } else if selected {
        heat_chip_colors(container_heat(label), dark)
    } else {
        chip_neutral(dark)
    };
    select_tag_chip(ui, label, bg, fg, enabled)
}

/// Separate α chip (only drawn when the stream has alpha).
fn alpha_chip(ui: &mut egui::Ui) {
    let (bg, fg) = if ui.visuals().dark_mode {
        (
            egui::Color32::from_rgb(72, 48, 96),
            egui::Color32::from_rgb(220, 190, 255),
        )
    } else {
        (
            egui::Color32::from_rgb(232, 220, 248),
            egui::Color32::from_rgb(88, 48, 140),
        )
    };
    tag_chip(ui, "α", bg, fg);
}

fn chip_neutral(dark: bool) -> (egui::Color32, egui::Color32) {
    if dark {
        (
            egui::Color32::from_rgb(48, 50, 56),
            egui::Color32::from_rgb(170, 174, 184),
        )
    } else {
        (
            egui::Color32::from_rgb(228, 230, 236),
            egui::Color32::from_rgb(90, 94, 104),
        )
    }
}

/// HSV → sRGB (`h` in degrees, `s`/`v` in 0..=1).
fn hsv_rgb(h: f32, s: f32, v: f32) -> egui::Color32 {
    let h = h.rem_euclid(360.0) / 60.0;
    let c = v * s;
    let x = c * (1.0 - (h.rem_euclid(2.0) - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    egui::Color32::from_rgb(
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    )
}

/// Warm→cool spectrum: heat 1 = red/orange … heat 0 = violet (full hue sweep).
fn heat_chip_colors(heat: f32, dark: bool) -> (egui::Color32, egui::Color32) {
    let t = heat.clamp(0.0, 1.0).powf(0.9);
    // Hue sweep: 12° (warm red-orange) → 275° (cool violet).
    let hue = 12.0 + (1.0 - t) * 263.0;
    if dark {
        (hsv_rgb(hue, 0.62, 0.42), hsv_rgb(hue, 0.40, 0.96))
    } else {
        (hsv_rgb(hue, 0.38, 0.93), hsv_rgb(hue, 0.82, 0.42))
    }
}

/// Colored monospace galley for the log TextEdit layouter.
fn layout_log_text(
    ui: &egui::Ui,
    text: &str,
    wrap_width: f32,
) -> std::sync::Arc<egui::Galley> {
    use egui::text::{LayoutJob, TextFormat};
    let font_id = egui::TextStyle::Monospace.resolve(ui.style());
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            job.append(
                "\n",
                0.0,
                TextFormat {
                    font_id: font_id.clone(),
                    color: ui.visuals().text_color(),
                    ..Default::default()
                },
            );
        }
        job.append(
            line,
            0.0,
            TextFormat {
                font_id: font_id.clone(),
                color: log_line_color(line),
                ..Default::default()
            },
        );
    }
    ui.fonts_mut(|f| f.layout_job(job))
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
        self.tick_delete_queue(ctx);
        self.tick_history_nav(ctx);
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
        self.ui_recycle_delete_prompt(&ctx);
        self.ui_permanent_delete_prompt(&ctx);
    }

    fn on_exit(&mut self) {
        let _ = prefs::save(&self.prefs);
        self.save_preview_cache_to_disk();
    }
}
