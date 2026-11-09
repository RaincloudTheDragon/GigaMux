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

#[cfg(windows)]
mod taskbar_progress;
mod ffmpeg_idle_pause;

use gigamux_core::check::{
    check_issue_category, check_issue_severity, delete_issue_outputs, integrity_blocked_outputs,
    issue_has_deletable_output, list_media, permanent_delete_file, restore_trashed_file, run_check,
    trash_file, CheckIssue, CheckReport, CheckSeverity,
};
use gigamux_core::chunk::chunk_time_range;
use gigamux_core::encode::{
    build_farm_chunked_job, build_farm_ffmpeg_argv, farm_integrity_for_chunked,
    farm_integrity_for_single, run_batch, write_farm_ffconcat, EncodeEvent, EncodeOptions,
    FarmEncodeRequest,
};
use gigamux_core::flamenco::{
    cancel_exec_mode_for_manager_version, deploy_gigamux_manager, discover_manager,
    ensure_worker_tags, flamenco_script_hint, infer_twoway_local_prefix, local_gigamux_js_present,
    make_gigamux_chunked_job, make_gigamux_encode_job, normalize_check_exe_path, ping_manager,
    resolve_flamenco_check_exe, submit_job, CancelExecMode, FlamencoTwoway, REQUIRED_WORKER_TAGS,
};
use gigamux_core::ffmpeg_active::{active_ffmpeg_pid, kill_process_tree};
use gigamux_core::vmaf::{libvmaf_available, run_vmaf, VmafEvent, VmafReport};
use gigamux_core::gpu::{probe_local_encoders, GpuChoice, HwCatalog};
use gigamux_core::job_log;
use gigamux_core::prefs::{self, EncodeSlot, Prefs};
use gigamux_core::preview_cache::{self, PersistedPreviewCache, PersistedPreviewEntry};
use gigamux_core::probe::{
    codec_heat, codec_tag, container_heat, enrich_summary_bitrates, file_info, format_bytes,
    format_length, media_summary_fast, pix_fmt_chip_tag, pix_fmt_heat, rate_mode_chip_tag,
    rate_mode_heat, size_delta, thumbnail_rgba, truncate_cli_err, video_has_alpha, FrameTiming,
    MediaSummary, RgbaThumb, SizeDelta, THUMB_W,
};
use gigamux_core::targets::{
    audio_encode_for, audio_must_recode, cycle_alpha_container, cycle_opaque_container, pair_stem,
    pix_fmt_convert_warning, strip_test_prefix, AlphaVideo, AudioEncode, OpaqueVideo, OutContainer,
    TestBatch,
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
    /// Live source frame at current out-time (center preview during encode).
    live_texture: Option<egui::TextureHandle>,
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
    // Adobe-style: Alt held while launching → ask to purge prefs (before the window eats focus).
    let ask_purge_prefs = alt_held_at_launch();
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
        Box::new(move |_cc| Ok(Box::new(GigaMuxApp::new(ask_purge_prefs)))),
    )
}

/// True when Alt/Option is down at process start (prefs-purge offer).
fn alt_held_at_launch() -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_MENU};
        // High bit set ⇒ key currently down.
        unsafe { GetAsyncKeyState(VK_MENU.0 as i32) as u16 & 0x8000 != 0 }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

enum JobKind {
    Encode,
    Check,
    Vmaf,
}

/// Left-rail category in the Settings second window.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    Encode,
    Files,
}

fn settings_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("gigamux_settings")
}

fn check_results_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("gigamux_check_results")
}

/// Color for a Check severity row / header.
fn check_severity_color(sev: CheckSeverity) -> egui::Color32 {
    match sev {
        CheckSeverity::High => egui::Color32::from_rgb(220, 90, 90),
        CheckSeverity::Medium => egui::Color32::from_rgb(220, 160, 70),
        CheckSeverity::Low => egui::Color32::from_rgb(200, 180, 70),
    }
}

enum AppEvent {
    Encode(EncodeEvent),
    CheckDone(Result<CheckReport, String>),
    Vmaf(VmafEvent),
    VmafDone(Result<VmafReport, String>),
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
    /// libvmaf vs encode pair; kept across re-probes while the file fingerprint matches.
    vmaf_score: Option<f64>,
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
            vmaf_score: None,
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
    /// Draft maxrate text per slot (index 0..=3 = Test1…Test4).
    maxrate_override_edit: [String; 4],
    /// Draft CQ/CRF text per slot (empty = default 0).
    cq_override_edit: [String; 4],
    /// Pair-jump prefer batch in test mode (cycled by Shift+←/→).
    test_nav_prefer: TestBatch,
    /// After Shift sibling/pair jump: scroll this Output test-slot header into view once.
    focus_out_test_slot: Option<TestBatch>,
    /// Local NVENC/QSV probe (generation, AV1 smoke, QSV name).
    hw: HwCatalog,
    /// This ffmpeg build exposes the `libvmaf` filter (probed once at launch).
    vmaf_available: bool,
    gpu_warn: Option<String>,
    input_files: Vec<PathBuf>,
    output_files: Vec<PathBuf>,
    /// Sum of listed Input file sizes (updated in `refresh_lists`).
    input_listed_bytes: u64,
    /// Sum of listed Output file sizes (updated in `refresh_lists`).
    output_listed_bytes: u64,
    /// Primary selection (preview / log focus): side + basename.
    active: Option<(FileSide, String)>,
    /// Multi-select set on the same side as `active` (always includes the primary name).
    selected: HashSet<String>,
    /// Shift-click range anchor (basename on the active side).
    selection_anchor: Option<String>,
    /// Click-drag marquee over Input/Output rows (None when idle).
    box_select: Option<BoxSelect>,
    /// Per-input-file log lines (encode progress keyed by input basename).
    file_logs: HashMap<String, Vec<String>>,
    /// Session / batch log when no file is active.
    session_log: Vec<String>,
    log_open: bool,
    /// When true, log panel shows the session log even if a file is active.
    log_show_session: bool,
    show_settings: bool,
    /// Active left-rail tab inside the Settings viewport.
    settings_tab: SettingsTab,
    /// Raise/focus the Settings viewport once after opening via the cog.
    settings_raise: bool,
    busy: bool,
    job: Option<JobKind>,
    cancel: Arc<AtomicBool>,
    event_rx: Option<Receiver<AppEvent>>,
    /// Check results viewport (severity tree + selective purge).
    pending_check: Option<CheckResultsWindow>,
    /// Pref-armed: when idle and due, silent Check then VMAF unscored intact pairs.
    auto_vmaf_armed: bool,
    /// Earliest Instant to start the next auto VMAF pass (None while disarmed or in-flight).
    auto_vmaf_due_at: Option<Instant>,
    /// True while the in-flight VMAF job is the auto (Check→score) path; drives reschedule on done.
    auto_vmaf_pass: bool,
    /// Shutter-style: ffmpeg process tree is NtSuspended until 1 s system idle.
    ffmpeg_suspended: bool,
    /// Last observed system idle ms (detect fresh input via decrease).
    last_system_idle_ms: u64,
    /// Confirm before Recycle Bin (unless the user opted out).
    pending_recycle_delete: Option<RecycleDeletePrompt>,
    /// Recycle Bin failed — confirm permanent delete (or proceed if warnings disabled).
    pending_permanent_delete: Option<PermanentDeletePrompt>,
    /// Flamenco sender options (Submit posts jobs; Cancel dismisses).
    pending_flamenco: Option<FlamencoSubmitPrompt>,
    /// Important error modal (session log still gets the line; panel stays collapsed).
    pending_alert: Option<AlertPrompt>,
    /// Alt held at launch — choose which prefs / logs to purge.
    pending_purge: Option<PurgePrompt>,
    /// One-by-one delete cascade (list shrinks between each file).
    delete_queue: Option<DeleteQueue>,
    /// Blender-style waypoint history (delete steps + cursor); jump undoes/redoes across Recycle Bin.
    undo_history: UndoHistory,
    /// Active history jump (restore and/or re-trash cascades toward a waypoint).
    history_nav: Option<HistoryNav>,
    /// Last-frame hit rects for folder drag-and-drop targeting.
    input_rect: Option<egui::Rect>,
    output_rect: Option<egui::Rect>,
    /// Pane under the cursor during an OS file drag (Windows often has no reliable pointer on the drop frame).
    dnd_hover_side: Option<FileSide>,
    /// Visible row count for PgUp/PgDn (updated while painting the active folder list).
    folder_page_rows: usize,
    /// After keyboard nav, scroll the primary row into view once.
    scroll_sel_into_view: bool,
    /// Invalidates in-flight pre-cache workers when the input folder changes.
    cache_gen: Arc<AtomicU64>,
    preview_cache: HashMap<String, PreviewCacheEntry>,
    cache_rx: Option<Receiver<PreviewMsg>>,
    /// Dual progress bars while an encode batch is running.
    encode_progress: Option<EncodeProgress>,
    /// Taskbar overlay is currently showing (Windows); used to clear once when the job ends.
    #[cfg(windows)]
    taskbar_showing: bool,
    /// Inline F2 rename of the active list row.
    rename_edit: Option<RenameEdit>,
    /// Last Input/Output `list_media` pass (idle auto-refresh cadence).
    lists_refreshed_at: Instant,
}

/// How to commit an inline F2 rename.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RenameCommitMode {
    /// Enter / click-away: one file, or multi numbered `stem (n).ext`.
    Default,
    /// Alt+Enter: spread baseline→draft affixes across the selection.
    Pattern,
}

/// Prefix/suffix strip + add inferred from editing one name (Alt+Enter multi).
#[derive(Clone, Debug)]
struct RenameAffixEdit {
    rem_pre: String,
    rem_suf: String,
    add_pre: String,
    add_suf: String,
}

/// In-list rename draft (F2 on the active file).
struct RenameEdit {
    side: FileSide,
    old_name: String,
    draft: String,
    /// Draft when rename opened — Alt+Enter pattern is relative to this.
    baseline: String,
    /// Multi-select F2 edits the stem only; each file keeps its own extension.
    stem_only: bool,
    /// Request TextEdit focus once after opening.
    request_focus: bool,
}

/// Pace between one-by-one delete / restore steps so the list visibly moves.
const DELETE_STEP_MS: u64 = 90;
/// Auto VMAF: retry while unscored outputs remain (other workers / integrity / score fail).
const AUTO_VMAF_RETRY_BUSY: Duration = Duration::from_secs(5 * 60);
/// Auto VMAF: watch interval once every output has a score.
const AUTO_VMAF_RETRY_CAUGHT_UP: Duration = Duration::from_secs(60 * 60);
/// Cap Ctrl+Z undo gestures (not individual files) so the stack cannot grow without bound.
const DELETE_UNDO_CAP: usize = 50;
/// Pointer travel before a press becomes a marquee instead of a click.
const BOX_SELECT_SLOP: f32 = 5.0;

/// In-progress drag rectangle for multi-select in a folder list.
struct BoxSelect {
    side: FileSide,
    /// Which scroll list owns this drag (`id_salt`) — Test1/Test2 both use Output.
    list_id: String,
    origin: egui::Pos2,
    current: egui::Pos2,
    /// Ctrl/Cmd held at press — union with selection at press time.
    additive: bool,
    baseline: HashSet<String>,
    /// True once drag exceeded [`BOX_SELECT_SLOP`].
    active: bool,
    /// Press started on a file row (vs blank padding).
    started_on_row: bool,
}

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

/// One F2 / context-rename gesture — pairs are `(before, after)` basenames on `side`.
#[derive(Clone)]
struct RenameUndoBatch {
    side: FileSide,
    pairs: Vec<(String, String)>,
}

/// History waypoint: delete (Recycle Bin) or rename (in-place basename swap).
#[derive(Clone)]
enum UndoStep {
    Delete(DeleteUndoBatch),
    Rename(RenameUndoBatch),
}

/// Waypoint history: `cursor == 0` is Original; `cursor == steps.len()` is the tip.
struct UndoHistory {
    /// Steps oldest → newest. Truncated when a new action happens while not at the tip.
    steps: Vec<UndoStep>,
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
    /// Restore / reverse toward Original (move cursor down).
    Undo,
    /// Re-apply toward the tip (move cursor up / redo).
    Redo,
}

/// One history jump: pending batches, then a per-file cascade for delete batches.
struct HistoryNav {
    target: usize,
    pending: VecDeque<(HistoryDir, UndoStep)>,
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

impl RenameUndoBatch {
    /// Menu / session label (e.g. `Rename "a.mp4" → "b.mp4"`).
    fn action_label(&self) -> String {
        match self.pairs.as_slice() {
            [] => "Rename".into(),
            [(o, n)] => format!("Rename \"{o}\" → \"{n}\""),
            _ => format!("Rename {} files", self.pairs.len()),
        }
    }
}

impl UndoStep {
    fn action_label(&self) -> String {
        match self {
            Self::Delete(b) => b.action_label(),
            Self::Rename(b) => b.action_label(),
        }
    }

    fn side(&self) -> FileSide {
        match self {
            Self::Delete(b) => b.side,
            Self::Rename(b) => b.side,
        }
    }
}

/// Dedicated Check results window: severity-ordered tree + selective purge.
struct CheckResultsWindow {
    issues: Vec<CheckIssue>,
    /// Indices into `issues` checked for Recycle Bin purge (must have on-disk output).
    selected: HashSet<usize>,
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

/// Flamenco sender dialog (opened by Flamenco button / context menu before POST).
#[derive(Clone)]
struct FlamencoSubmitPrompt {
    /// Input basenames to submit (frozen when the dialog opens).
    names: Vec<String>,
    /// Draft worker tag (seeded from prefs; written back on Submit).
    worker_tag: String,
    /// Draft ffmpeg exe on workers (seeded from prefs; written back on Submit).
    ffmpeg_exe: String,
    /// Enable frame chunking (N encode + concat/mux).
    chunking: bool,
    /// Chunk count when `chunking` (2..=32).
    chunks: u32,
    /// Only chunk software/alpha paths; HW opaque stays single-task.
    only_chunk_software: bool,
    /// Duration (s) of the first selected input for the chunk estimate label.
    estimate_duration_s: f64,
    /// Basename used for the estimate label.
    estimate_name: String,
}

/// What to do after the user confirms an alert with OK (not Esc / Open log).
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum AlertAfterOk {
    #[default]
    None,
    /// Open Settings → Files (e.g. Flamenco URL / Deploy GigaMux).
    OpenSettingsFiles,
}

/// Modal for important errors (does not expand the log panel).
#[derive(Clone)]
struct AlertPrompt {
    title: String,
    message: String,
    after_ok: AlertAfterOk,
}

/// Alt-at-launch purge chooser (which prefs slices + optional logs).
#[derive(Clone)]
struct PurgePrompt {
    folders: bool,
    targets: bool,
    hardware: bool,
    bitrate: bool,
    /// OBS metadata + delete-confirm toggles.
    misc: bool,
    session_log: bool,
    file_logs: bool,
    job_logs: bool,
}

impl Default for PurgePrompt {
    fn default() -> Self {
        // Prefs on, logs off — matches “reset settings, keep history” default.
        Self {
            folders: true,
            targets: true,
            hardware: true,
            bitrate: true,
            misc: true,
            session_log: false,
            file_logs: false,
            job_logs: false,
        }
    }
}

impl PurgePrompt {
    fn any_selected(&self) -> bool {
        self.folders
            || self.targets
            || self.hardware
            || self.bitrate
            || self.misc
            || self.session_log
            || self.file_logs
            || self.job_logs
    }
}

impl GigaMuxApp {
    fn new(ask_purge_prefs: bool) -> Self {
        let mut prefs = prefs::load();
        let hw = probe_local_encoders();
        // ffmpeg `-filters` probe — avoid spawning every frame for greyed VMAF controls.
        let vmaf_available = libvmaf_available();
        let warn = prefs.sanitize_slot_gpus(&hw.nvenc);
        // Opaque VP8 has no HW path: UI greys the checkbox but keeps the prefs flag.
        let (maxrate_override_edit, cq_override_edit) = Self::draft_edits_from_prefs(&prefs);
        let mut app = Self {
            prefs,
            maxrate_override_edit,
            cq_override_edit,
            test_nav_prefer: TestBatch::One,
            focus_out_test_slot: None,
            hw,
            vmaf_available,
            gpu_warn: warn,
            input_files: Vec::new(),
            output_files: Vec::new(),
            input_listed_bytes: 0,
            output_listed_bytes: 0,
            active: None,
            selected: HashSet::new(),
            selection_anchor: None,
            box_select: None,
            file_logs: HashMap::new(),
            // Restore prior session lines from AppData (same cap as the live buffer).
            session_log: prefs::load_session_log(500),
            log_open: false,
            log_show_session: true,
            show_settings: false,
            settings_tab: SettingsTab::Encode,
            settings_raise: false,
            busy: false,
            job: None,
            cancel: Arc::new(AtomicBool::new(false)),
            event_rx: None,
            pending_check: None,
            auto_vmaf_armed: false,
            auto_vmaf_due_at: None,
            auto_vmaf_pass: false,
            ffmpeg_suspended: false,
            last_system_idle_ms: u64::MAX,
            pending_recycle_delete: None,
            pending_permanent_delete: None,
            pending_flamenco: None,
            pending_alert: None,
            pending_purge: ask_purge_prefs.then(PurgePrompt::default),
            delete_queue: None,
            undo_history: UndoHistory::new(),
            history_nav: None,
            input_rect: None,
            output_rect: None,
            dnd_hover_side: None,
            folder_page_rows: 10,
            scroll_sel_into_view: false,
            cache_gen: Arc::new(AtomicU64::new(0)),
            preview_cache: HashMap::new(),
            cache_rx: None,
            encode_progress: None,
            #[cfg(windows)]
            taskbar_showing: false,
            rename_edit: None,
            lists_refreshed_at: Instant::now(),
        };
        app.load_preview_cache_from_disk();
        app.refresh_lists();
        let started = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        app.push_session(format!("—— session start {started} ——"));
        // Probe is RAM-only (no disk cache); log what we saw at launch.
        app.push_session(format!("HW probe (launch): {}", app.hw.summary_line()));
        // Pref on at launch: one immediate idle pass, then 5m/1h cadence after each finish.
        if app.prefs.auto_vmaf_when_idle {
            app.schedule_auto_vmaf(Duration::ZERO);
        }
        app
    }

    /// Live re-probe NVENC/QSV into RAM + session log. Never trust a prior snapshot.
    fn refresh_hw_probe(&mut self, why: &str) {
        self.hw = probe_local_encoders();
        self.gpu_warn = self.prefs.sanitize_slot_gpus(&self.hw.nvenc);
        self.push_session(format!("HW probe ({why}): {}", self.hw.summary_line()));
    }

    fn clear_active(&mut self) {
        self.rename_edit = None;
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

    /// ↑/↓ / range nav list: in test mode on Output, stay inside the active test batch.
    fn names_for_nav(&self, side: FileSide) -> Vec<String> {
        let names = self.names_for(side);
        if side != FileSide::Output || !self.prefs.test_mode {
            return names;
        }
        let Some((_, cur)) = self.active.as_ref() else {
            return names;
        };
        let (Some(batch), _) = strip_test_prefix(cur) else {
            return names;
        };
        names
            .into_iter()
            .filter(|n| strip_test_prefix(n).0 == Some(batch))
            .collect()
    }

    /// Draft maxrate/CQ text boxes from persisted slot overrides.
    fn draft_edits_from_prefs(prefs: &Prefs) -> ([String; 4], [String; 4]) {
        let maxrate = TestBatch::ALL.map(|b| {
            prefs
                .slot(b)
                .maxrate_override_k
                .map(|k| k.to_string())
                .unwrap_or_default()
        });
        let cq = TestBatch::ALL.map(|b| {
            prefs
                .slot(b)
                .cq_override
                .map(|q| q.to_string())
                .unwrap_or_default()
        });
        (maxrate, cq)
    }

    /// Commit maxrate + CQ drafts for every slot.
    fn commit_all_slot_edits(&mut self) {
        for &b in &TestBatch::ALL {
            self.commit_maxrate_override_edit(b);
            self.commit_cq_override_edit(b);
        }
    }

    /// Output files belonging to one test batch (`test1_*`…`test4_*`).
    fn output_files_for_batch(files: &[PathBuf], batch: TestBatch) -> Vec<PathBuf> {
        files
            .iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| strip_test_prefix(n).0 == Some(batch))
            })
            .cloned()
            .collect()
    }

    /// Pair stem for input↔output jumps (strips `testN_` on outputs).
    fn pair_stem_of(name: &str) -> String {
        pair_stem(name)
    }

    /// Same-stem counterpart on the other side.
    /// In test mode, `prefer` picks which test batch when jumping toward Output.
    fn counterpart_name(&self, side: FileSide, name: &str) -> Option<String> {
        self.counterpart_name_prefer(side, name, TestBatch::One)
    }

    fn counterpart_name_prefer(
        &self,
        side: FileSide,
        name: &str,
        prefer: TestBatch,
    ) -> Option<String> {
        let stem = Self::pair_stem_of(name);
        let other = Self::other_side(side);
        let names = self.names_for(other);
        match (side, other) {
            // Input → Output: prefer the requested test batch (or plain stem when not in test mode).
            (FileSide::Input, FileSide::Output) => {
                if self.prefs.test_mode {
                    names
                        .iter()
                        .find(|n| {
                            let (batch, rest) = strip_test_prefix(n);
                            batch == Some(prefer) && rest == stem
                        })
                        .cloned()
                        .or_else(|| {
                            // Fallback: any prefixed or plain match for this stem.
                            names.into_iter().find(|n| Self::pair_stem_of(n) == stem)
                        })
                } else {
                    names
                        .into_iter()
                        .find(|n| Self::file_stem_of(n) == stem.as_str())
                }
            }
            // Output → Input: strip test prefix then match input stem.
            (FileSide::Output, FileSide::Input) => names
                .into_iter()
                .find(|n| Self::file_stem_of(n) == stem.as_str()),
            _ => None,
        }
    }

    /// Next/prev test output for the same source stem among active batches.
    fn sibling_test_name(&self, name: &str, forward: bool) -> Option<String> {
        let (batch, stem) = strip_test_prefix(name);
        let batch = batch?;
        let active = self.prefs.active_batches();
        let want = if forward {
            batch.next_among(active)
        } else {
            batch.prev_among(active)
        };
        if want == batch {
            return None;
        }
        self.names_for(FileSide::Output).into_iter().find(|n| {
            let (b, rest) = strip_test_prefix(n);
            b == Some(want) && rest == stem
        })
    }

    /// Cycle `test_nav_prefer` through active batches; returns the new prefer.
    fn cycle_test_nav_prefer(&mut self, forward: bool) -> TestBatch {
        let active = self.prefs.active_batches();
        if active.is_empty() {
            return TestBatch::One;
        }
        let cur = if active.contains(&self.test_nav_prefer) {
            self.test_nav_prefer
        } else {
            active[0]
        };
        let next = if forward {
            cur.next_among(active)
        } else {
            cur.prev_among(active)
        };
        self.test_nav_prefer = next;
        next
    }

    fn other_side(side: FileSide) -> FileSide {
        match side {
            FileSide::Input => FileSide::Output,
            FileSide::Output => FileSide::Input,
        }
    }

    /// Parse `i:name` / `o:name` cache keys.
    fn split_preview_key(key: &str) -> Option<(FileSide, &str)> {
        key.strip_prefix("i:")
            .map(|n| (FileSide::Input, n))
            .or_else(|| key.strip_prefix("o:").map(|n| (FileSide::Output, n)))
    }

    /// VMAF only while the paired file is still listed.
    /// Test-mode outputs keep their own score (input is not overwritten per-pass).
    fn live_vmaf_score(&self, side: FileSide, name: &str) -> Option<f64> {
        let score = self.preview_cache.get(&Self::preview_key(side, name))?.vmaf_score?;
        let other_name = self.counterpart_name(side, name)?;
        // Prefixed test outputs: show score whenever the input counterpart still exists.
        if side == FileSide::Output && strip_test_prefix(name).0.is_some() {
            return Some(score);
        }
        let other = self
            .preview_cache
            .get(&Self::preview_key(Self::other_side(side), &other_name))?;
        match other.vmaf_score {
            Some(s) if (s - score).abs() < 1e-6 => Some(score),
            _ => None,
        }
    }

    /// Drop cached VMAF when the pair is gone or only one side still has a score.
    /// Skips undo-parked cache rows that aren't in the live lists.
    fn scrub_stale_vmaf(&mut self) -> bool {
        let live_keys: HashSet<String> = self
            .names_for(FileSide::Input)
            .into_iter()
            .map(|n| Self::preview_key(FileSide::Input, &n))
            .chain(
                self.names_for(FileSide::Output)
                    .into_iter()
                    .map(|n| Self::preview_key(FileSide::Output, &n)),
            )
            .collect();
        let mut clear = Vec::new();
        for key in self.preview_cache.keys() {
            if !live_keys.contains(key) {
                continue;
            }
            let Some((side, name)) = Self::split_preview_key(key) else {
                continue;
            };
            let Some(score) = self.preview_cache.get(key).and_then(|e| e.vmaf_score) else {
                continue;
            };
            let stale = match self.counterpart_name(side, name) {
                None => true,
                Some(other_name) => {
                    // Test-prefixed outputs keep their own score without mirroring onto input.
                    if side == FileSide::Output && strip_test_prefix(name).0.is_some() {
                        false
                    } else {
                        let other_key = Self::preview_key(Self::other_side(side), &other_name);
                        match self.preview_cache.get(&other_key).and_then(|e| e.vmaf_score) {
                            Some(s) if (s - score).abs() < 1e-6 => false,
                            _ => true,
                        }
                    }
                }
            };
            if stale {
                clear.push(key.clone());
            }
        }
        if clear.is_empty() {
            return false;
        }
        for key in clear {
            if let Some(e) = self.preview_cache.get_mut(&key) {
                e.vmaf_score = None;
            }
        }
        true
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
        let names = self.names_for_nav(side);
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

    /// Replace selection with `names` (list order); primary = last selected row.
    fn select_set(&mut self, side: FileSide, names: HashSet<String>, anchor: Option<String>) {
        if names.is_empty() {
            self.clear_active();
            return;
        }
        let ordered: Vec<String> = self
            .names_for(side)
            .into_iter()
            .filter(|n| names.contains(n))
            .collect();
        if ordered.is_empty() {
            self.clear_active();
            return;
        }
        self.selected = ordered.iter().cloned().collect();
        self.selection_anchor = anchor.or_else(|| ordered.first().cloned());
        if let Some(primary) = ordered.last().cloned() {
            self.set_primary(side, primary);
        }
    }

    /// Select every file on `side` (Ctrl+A while hovering over that pane).
    fn select_all_on_side(&mut self, side: FileSide) {
        let names = self.names_for(side);
        let Some(first) = names.first().cloned() else {
            return;
        };
        let set = names.into_iter().collect();
        self.select_set(side, set, Some(first.clone()));
        // Keep primary at the top of the list (not the last row).
        self.set_primary(side, first);
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
            if let UndoStep::Delete(d) = batch {
                for path in &d.paths {
                    park(d.side, path);
                }
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
                if let UndoStep::Delete(del) = batch {
                    for path in &del.paths {
                        park(del.side, path);
                    }
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
                    vmaf_score: pe.vmaf_score,
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
                    Err(err) => (None, 0, 0, Some(truncate_cli_err(&err, 800))),
                },
                // Don't persist multi‑k‑line ffmpeg dumps into preview_cache.json.
                Some(Err(err)) => (None, 0, 0, Some(truncate_cli_err(err, 800))),
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
                    vmaf_score: e.vmaf_score,
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

    /// True when an encode job is using heavy software libvpx (opaque VP8/9 CPU or non-hybrid α).
    fn cpu_encode_active(&self) -> bool {
        if !(self.busy && matches!(self.job, Some(JobKind::Encode))) {
            return false;
        }
        // Opaque VP9 + hardware uses QSV — don't throttle previews as a CPU encode.
        if self.prefs.slot1.opaque_video.is_libvpx()
            && !(self.prefs.slot1.use_hardware && self.prefs.slot1.opaque_video.is_qsv())
        {
            return true;
        }
        // α VP9/.webm + hardware uses QSV+libvpx hybrid — keep previews responsive.
        let hybrid_alpha = self.prefs.slot1.use_hardware
            && self.prefs.slot1.alpha_video == AlphaVideo::Vp9
            && self.prefs.slot1.alpha_container == OutContainer::Webm;
        if hybrid_alpha {
            return false;
        }
        // Other α delivers use full libvpx — throttle if any known-α input is in the batch.
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
                    .entry(name.clone())
                    .or_insert_with(PreviewCacheEntry::pending);
                // Keep VMAF only while the on-disk file is unchanged.
                let same_file = entry.file_size == file_size
                    && entry.mtime_secs == mtime_secs
                    && entry.source_path == source_path;
                let keep_vmaf = if same_file { entry.vmaf_score } else { None };
                let drop_pair = entry.vmaf_score.is_some() && keep_vmaf.is_none();
                entry.pending = false;
                entry.bitrates_ready = false;
                entry.schema = MediaSummary::SCHEMA;
                entry.summary = Some(summary);
                entry.source_path = source_path;
                entry.file_size = file_size;
                entry.mtime_secs = mtime_secs;
                entry.vmaf_score = keep_vmaf;
                // Replaced encode → clear the other side so input meta doesn't keep a dead score.
                if drop_pair {
                    if let Some((side, bare)) = Self::split_preview_key(&name) {
                        if let Some(other_name) = self.counterpart_name(side, bare) {
                            let other_key =
                                Self::preview_key(Self::other_side(side), &other_name);
                            if let Some(e) = self.preview_cache.get_mut(&other_key) {
                                e.vmaf_score = None;
                            }
                        }
                    }
                }
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
            self.alert_error(
                "Drop",
                format!("drop ignored (not a folder): {}", path.display()),
            );
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

    /// Pointer for folder DnD: egui often has none during an OS drag on Windows.
    fn dnd_pointer(ctx: &egui::Context) -> Option<egui::Pos2> {
        ctx.pointer_interact_pos()
            .or_else(|| ctx.pointer_latest_pos())
            .or_else(|| win32_cursor_egui_pos(ctx))
    }

    /// Which pane contains `p`, if either.
    fn pane_at(&self, p: egui::Pos2) -> Option<FileSide> {
        if self.input_rect.is_some_and(|r| r.contains(p)) {
            Some(FileSide::Input)
        } else if self.output_rect.is_some_and(|r| r.contains(p)) {
            Some(FileSide::Output)
        } else {
            None
        }
    }

    /// Apply OS file drops using the pane under the cursor (Win32 fallback when egui has no pos).
    fn handle_folder_dnd(&mut self, ctx: &egui::Context) {
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });

        // Live hit-test every drag frame (Win32 cursor) so outline works even if egui pointer is dead.
        if hovering {
            if let Some(p) = Self::dnd_pointer(ctx) {
                self.dnd_hover_side = self.pane_at(p);
            }
            let stroke = egui::Stroke::new(2.0, egui::Color32::from_rgb(80, 160, 255));
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::unique("dnd_overlay"),
            ));
            match self.dnd_hover_side {
                Some(FileSide::Input) => {
                    if let Some(r) = self.input_rect {
                        painter.rect_stroke(r, 4.0, stroke, egui::StrokeKind::Inside);
                    }
                }
                Some(FileSide::Output) => {
                    if let Some(r) = self.output_rect {
                        painter.rect_stroke(r, 4.0, stroke, egui::StrokeKind::Inside);
                    }
                }
                None => {}
            }
            ctx.request_repaint();
        }

        if dropped.is_empty() {
            return;
        }

        let Some(path) = dropped.into_iter().next() else {
            return;
        };
        let pointer = Self::dnd_pointer(ctx);
        let side = self
            .dnd_hover_side
            .or_else(|| pointer.and_then(|p| self.pane_at(p)));
        self.dnd_hover_side = None;
        match side {
            Some(FileSide::Input) => self.set_folder_from_path(true, path),
            Some(FileSide::Output) => self.set_folder_from_path(false, path),
            None => {
                self.alert_error(
                    "Drop",
                    "drop a folder onto the Input or Output pane to set it",
                );
            }
        }
    }

    /// Idle re-scan interval so farm/external outputs appear without a manual refresh.
    const LISTS_IDLE_REFRESH: Duration = Duration::from_secs(60);

    fn refresh_lists(&mut self) {
        self.lists_refreshed_at = Instant::now();
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
        self.input_listed_bytes = Self::sum_file_bytes(&self.input_files);
        self.output_listed_bytes = Self::sum_file_bytes(&self.output_files);
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
        // Output deleted/renamed → drop orphaned input (and vice versa) VMAF so meta stays live.
        if self.scrub_stale_vmaf() {
            self.save_preview_cache_to_disk();
        }
    }

    fn sum_file_bytes(files: &[PathBuf]) -> u64 {
        files
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum()
    }

    /// Pane footer: listed count + total size (e.g. `15 items · 129.83 GB`).
    fn folder_size_footer(count: usize, bytes: u64) -> String {
        let noun = if count == 1 { "item" } else { "items" };
        format!("{count} {noun} · {}", format_bytes(bytes))
    }

    /// ↓/% green or ↑/% red — same markers as center Size row / test compare strip.
    /// `small` matches pane footer text size when true.
    fn ui_size_delta_labels(ui: &mut egui::Ui, delta: SizeDelta, small: bool) {
        let green = egui::Color32::from_rgb(80, 180, 100);
        let red = egui::Color32::from_rgb(220, 90, 80);
        let label = |ui: &mut egui::Ui, color: egui::Color32, text: String| {
            let mut rt = egui::RichText::new(text).color(color);
            if small {
                rt = rt.small();
            }
            ui.label(rt);
        };
        match delta {
            SizeDelta::Smaller { pct, bytes } => {
                label(ui, green, format!("↓{pct}%"));
                ui.add_space(2.0);
                label(ui, green, format!("↓{}", format_bytes(bytes)));
            }
            SizeDelta::Larger { pct, bytes } => {
                label(ui, red, format!("↑{pct}%"));
                ui.add_space(2.0);
                label(ui, red, format!("↑{}", format_bytes(bytes)));
            }
            SizeDelta::Same => {}
        }
    }

    /// Once a minute while idle: re-list folders and top up preview cache (e.g. farm delivers).
    fn tick_idle_list_refresh(&mut self, ctx: &egui::Context) {
        if !(self.history_idle() && self.rename_edit.is_none()) {
            return;
        }
        let due = self.lists_refreshed_at + Self::LISTS_IDLE_REFRESH;
        let now = Instant::now();
        if now >= due {
            self.refresh_lists();
        }
        let next = self.lists_refreshed_at + Self::LISTS_IDLE_REFRESH;
        ctx.request_repaint_after(next.saturating_duration_since(Instant::now()));
    }

    /// UI session pane + durable `session.log` (UI-only actions; not job-mirrored lines).
    fn push_session(&mut self, line: impl Into<String>) {
        let line = line.into();
        let _ = prefs::append_session_log(&line);
        self.session_log.push(line);
        trim_log(&mut self.session_log);
        // Do not rewrite disk from the UI tail — job lines live on disk beyond the pane.
    }

    /// Important error: session log + modal. Never auto-expands the log panel.
    fn alert_error(&mut self, title: impl Into<String>, message: impl Into<String>) {
        self.alert_error_with(title, message, AlertAfterOk::None);
    }

    /// Like [`Self::alert_error`], but run `after_ok` only if the user clicks OK (not Esc / Open log).
    fn alert_error_with(
        &mut self,
        title: impl Into<String>,
        message: impl Into<String>,
        after_ok: AlertAfterOk,
    ) {
        let title = title.into();
        let message = message.into();
        self.push_session(message.clone());
        self.pending_alert = Some(AlertPrompt {
            title,
            message,
            after_ok,
        });
    }

    /// Modal only when the session line was already written another way.
    fn show_alert(&mut self, title: impl Into<String>, message: impl Into<String>) {
        self.pending_alert = Some(AlertPrompt {
            title: title.into(),
            message: message.into(),
            after_ok: AlertAfterOk::None,
        });
    }

    /// Session pane only — durable line already written by [`job_log::JobLog`].
    fn note_session_ui(&mut self, line: impl Into<String>) {
        self.session_log.push(line.into());
        trim_log(&mut self.session_log);
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
                Err(e) => self.alert_error("Export", format!("export failed: {e}")),
            }
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
                    self.gpu_warn = p.sanitize_slot_gpus(&self.hw.nvenc);
                    self.prefs = p;
                    let (maxrate, cq) = Self::draft_edits_from_prefs(&self.prefs);
                    self.maxrate_override_edit = maxrate;
                    self.cq_override_edit = cq;
                    let _ = prefs::save(&self.prefs);
                    self.refresh_lists();
                    self.push_session(format!("imported profile ← {}", path.display()));
                }
                Err(e) => self.alert_error("Import", format!("import failed: {e}")),
            }
        }
    }

    /// Apply selected purge slices from the Alt-at-launch dialog.
    fn apply_purge(&mut self, p: &PurgePrompt) {
        let d = Prefs::default();
        let mut parts: Vec<String> = Vec::new();
        let mut touched_prefs = false;

        if p.folders {
            self.prefs.input_dir = d.input_dir.clone();
            self.prefs.output_dir = d.output_dir.clone();
            touched_prefs = true;
            parts.push("folders".into());
        }
        if p.targets {
            self.prefs.slot1.opaque_video = d.slot1.opaque_video;
            self.prefs.slot1.opaque_container = d.slot1.opaque_container;
            self.prefs.slot1.alpha_video = d.slot1.alpha_video;
            self.prefs.slot1.alpha_container = d.slot1.alpha_container;
            self.prefs.slot2 = EncodeSlot::default_slot2();
            self.prefs.slot3 = EncodeSlot::default();
            self.prefs.slot4 = EncodeSlot::default();
            self.prefs.test_mode = false;
            self.prefs.test_slot_count = 2;
            touched_prefs = true;
            parts.push("targets".into());
        }
        if p.hardware {
            self.prefs.slot1.use_hardware = d.slot1.use_hardware;
            self.prefs.slot2.use_hardware = d.slot2.use_hardware;
            self.prefs.slot3.use_hardware = d.slot3.use_hardware;
            self.prefs.slot4.use_hardware = d.slot4.use_hardware;
            self.prefs.slot1.gpu = d.slot1.gpu;
            self.prefs.slot2.gpu = d.slot2.gpu;
            self.prefs.slot3.gpu = d.slot3.gpu;
            self.prefs.slot4.gpu = d.slot4.gpu;
            touched_prefs = true;
            parts.push("hardware".into());
        }
        if p.bitrate {
            self.prefs.use_peak_bitrate = d.use_peak_bitrate;
            for &b in &TestBatch::ALL {
                self.prefs.slot_mut(b).maxrate_override_k = d.slot(b).maxrate_override_k;
                self.prefs.slot_mut(b).cq_override = d.slot(b).cq_override;
            }
            self.maxrate_override_edit = Default::default();
            self.cq_override_edit = Default::default();
            touched_prefs = true;
            parts.push("bitrate".into());
        }
        if p.misc {
            self.prefs.preserve_obs_metadata = d.preserve_obs_metadata;
            self.prefs.write_encoder_metadata = d.write_encoder_metadata;
            self.prefs.warn_recycle_delete = d.warn_recycle_delete;
            self.prefs.warn_permanent_delete = d.warn_permanent_delete;
            self.prefs.job_done_chime = d.job_done_chime;
            self.prefs.check_ignore_orphan_outputs = d.check_ignore_orphan_outputs;
            self.prefs.auto_vmaf_when_idle = d.auto_vmaf_when_idle;
            self.prefs.work_during_inactivity = d.work_during_inactivity;
            self.prefs.flamenco_manager_url = d.flamenco_manager_url;
            self.prefs.flamenco_scripts_dir = d.flamenco_scripts_dir;
            self.prefs.flamenco_check_exe = d.flamenco_check_exe;
            self.prefs.flamenco_worker_tag = d.flamenco_worker_tag;
            self.prefs.flamenco_ffmpeg_exe = d.flamenco_ffmpeg_exe;
            self.prefs.flamenco_chunking = d.flamenco_chunking;
            self.prefs.flamenco_chunks = d.flamenco_chunks;
            self.prefs.flamenco_only_chunk_software = d.flamenco_only_chunk_software;
            self.prefs.flamenco_twoway_local = d.flamenco_twoway_local;
            self.prefs.flamenco_twoway_var = d.flamenco_twoway_var;
            touched_prefs = true;
            parts.push("misc".into());
        }
        if touched_prefs {
            self.prefs.sanitize_targets();
            self.gpu_warn = self.prefs.sanitize_slot_gpus(&self.hw.nvenc);
            let _ = prefs::save(&self.prefs);
            if p.folders {
                self.clear_active();
                self.refresh_lists();
            }
        }

        if p.session_log {
            self.session_log.clear();
            let _ = prefs::clear_session_log();
            parts.push("session log".into());
            let started = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
            self.push_session(format!("—— session start {started} ——"));
        }
        if p.file_logs {
            self.file_logs.clear();
            parts.push("file logs".into());
        }
        if p.job_logs {
            match job_log::clear_encode_logs() {
                Ok(n) => parts.push(format!("job logs ({n})")),
                Err(e) => self.alert_error("Purge", format!("job log purge failed: {e}")),
            }
        }

        if parts.is_empty() {
            self.push_session("purge: nothing selected");
        } else {
            self.push_session(format!("purged: {}", parts.join(", ")));
        }
    }

    /// Open the Flamenco sender dialog for the current Input selection (or `only_name`).
    fn open_flamenco_submit(&mut self, only_name: Option<String>) {
        let url = self.prefs.flamenco_manager_url.trim();
        if url.is_empty() {
            self.alert_error_with(
                "Flamenco",
                "set Flamenco Manager URL in Settings → Files",
                AlertAfterOk::OpenSettingsFiles,
            );
            return;
        }
        if !prefs::dir_ok(&self.prefs.input_dir) || !prefs::dir_ok(&self.prefs.output_dir) {
            self.alert_error(
                "Flamenco",
                "set both input and output folders before Flamenco submit",
            );
            return;
        }
        let names: Vec<String> = if let Some(n) = only_name {
            vec![n]
        } else if self.active.as_ref().is_some_and(|(s, _)| *s == FileSide::Input) {
            self.selected_names_ordered()
        } else {
            Vec::new()
        };
        if names.is_empty() {
            self.alert_error(
                "Flamenco",
                "select input file(s) (or use context menu) for Flamenco submit",
            );
            return;
        }
        let (estimate_name, estimate_duration_s) = names
            .first()
            .map(|n| {
                let p = self.prefs.input_dir.join(n);
                let dur = file_info(&p)
                    .ok()
                    .map(|info| {
                        info.pointer("/format/duration")
                            .and_then(|v| {
                                v.as_f64()
                                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                            })
                            .unwrap_or(0.0)
                    })
                    .unwrap_or(0.0);
                (n.clone(), dur)
            })
            .unwrap_or_default();
        self.pending_flamenco = Some(FlamencoSubmitPrompt {
            names,
            worker_tag: self.prefs.flamenco_worker_tag.clone(),
            ffmpeg_exe: self.prefs.flamenco_ffmpeg_exe.clone(),
            chunking: self.prefs.flamenco_chunking,
            chunks: self.prefs.flamenco_chunks.clamp(2, 32),
            only_chunk_software: self.prefs.flamenco_only_chunk_software,
            estimate_duration_s,
            estimate_name,
        });
    }

    /// True when farm encode for this input would use HW opaque (not alpha / not CPU).
    fn farm_path_is_hw_opaque(&self, input: &Path, slot: &EncodeSlot) -> bool {
        let Ok(info) = file_info(input) else {
            return slot.use_hardware
                && (slot.opaque_video.is_nvenc() || slot.opaque_video.is_qsv());
        };
        if video_has_alpha(&info) {
            return false;
        }
        slot.use_hardware && (slot.opaque_video.is_nvenc() || slot.opaque_video.is_qsv())
    }

    /// Submit the given inputs to Flamenco (one job each; stock worker `exec` via `GigaMux.js`).
    fn submit_flamenco_encode(&mut self, names: Vec<String>) {
        let url = self.prefs.flamenco_manager_url.trim().to_string();
        if url.is_empty() {
            self.alert_error_with(
                "Flamenco",
                "set Flamenco Manager URL in Settings → Files",
                AlertAfterOk::OpenSettingsFiles,
            );
            return;
        }
        if !prefs::dir_ok(&self.prefs.input_dir) || !prefs::dir_ok(&self.prefs.output_dir) {
            self.alert_error(
                "Flamenco",
                "set both input and output folders before Flamenco submit",
            );
            return;
        }
        self.commit_maxrate_override_edit(TestBatch::One);
        self.commit_cq_override_edit(TestBatch::One);
        if names.is_empty() {
            self.alert_error(
                "Flamenco",
                "select input file(s) (or use context menu) for Flamenco submit",
            );
            return;
        }
        // Local Deploy path remembered: catch missing GigaMux.js before Manager returns "job type unknown".
        let scripts_dir = self.prefs.flamenco_scripts_dir.trim();
        if !scripts_dir.is_empty() && !local_gigamux_js_present(scripts_dir) {
            self.alert_error_with(
                "Flamenco",
                format!(
                    "GigaMux.js is missing from:\n{scripts_dir}\n\nSettings → Files → Deploy GigaMux… then retry."
                ),
                AlertAfterOk::OpenSettingsFiles,
            );
            return;
        }
        // Local HW catalog is only for argv metadata / path checks — still re-probe.
        self.refresh_hw_probe("flamenco");
        // Effective HW for this machine (same as local encode mute); shared across the batch.
        let mut slot = self.prefs.slot1.clone();
        slot.use_hardware = self.effective_use_hardware_for(&slot);
        let exe_owned = {
            let t = self.prefs.flamenco_ffmpeg_exe.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        };
        let tag_owned = {
            let t = self.prefs.flamenco_worker_tag.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        };
        let exe = exe_owned.as_deref();
        let tag = tag_owned.as_deref();
        let want_chunk = self.prefs.flamenco_chunking;
        let chunk_n = self.prefs.flamenco_chunks.clamp(2, 32);
        let only_soft = self.prefs.flamenco_only_chunk_software;
        // Flamenco only HasPrefix-rewrites whole setting strings; brace paths inside argv JSON ourselves.
        let mut twoway_local = self.prefs.flamenco_twoway_local.trim().to_string();
        if twoway_local.is_empty() {
            // Match Manager drive-root two-way (e.g. `Z:\`) without requiring the Settings field every time.
            if let Some(inferred) = infer_twoway_local_prefix(&self.prefs.input_dir) {
                self.push_session(format!(
                    "Flamenco: using inferred shared path prefix {inferred} (set Settings → Files to override)"
                ));
                twoway_local = inferred;
            }
        }
        let twoway_var = {
            let t = self.prefs.flamenco_twoway_var.trim();
            if t.is_empty() {
                "shared_storage_path".to_string()
            } else {
                t.to_string()
            }
        };
        let twoway = if twoway_local.is_empty() {
            None
        } else {
            Some(FlamencoTwoway {
                local_prefix: twoway_local.as_str(),
                var_name: twoway_var.as_str(),
            })
        };
        if twoway.is_none() {
            self.push_session(
                "Flamenco: shared path prefix empty — Linux workers will see Windows paths; set Settings → Files → Shared path prefix",
            );
        }
        // ≤3.9: stub cancel helper (warn). 3.10-beta+: direct ffmpeg exec (Worker kill PR).
        let manager_ver = ping_manager(&url).unwrap_or_else(|_| "unknown".into());
        let cancel_exec = cancel_exec_mode_for_manager_version(&manager_ver);
        if cancel_exec == CancelExecMode::LegacyStub {
            self.push_session(format!(
                "Flamenco: Manager {manager_ver} ≤3.9 — Windows Cancel may leave ffmpeg running until a cancel helper ships; prefer 3.10-beta Workers with the kill PR"
            ));
        }
        let mut ok = 0usize;
        let mut skipped = 0usize;
        let mut failed = 0usize;
        // Pop once for missing/old GigaMux.js; further submits would hit the same Manager error.
        let mut script_alerted = false;
        for name in &names {
            let input = self.prefs.input_dir.join(name);
            if !input.is_file() {
                self.push_session(format!("Flamenco: input not found: {name}"));
                failed += 1;
                continue;
            }
            let hw_opaque = self.farm_path_is_hw_opaque(&input, &slot);
            let use_chunks = want_chunk && !(only_soft && hw_opaque);
            let farm = FarmEncodeRequest {
                input: input.clone(),
                output_dir: self.prefs.output_dir.clone(),
                gpu: slot.gpu,
                hw: self.hw.clone(),
                preserve_obs_metadata: self.prefs.preserve_obs_metadata,
                write_encoder_metadata: self.prefs.write_encoder_metadata,
                use_peak_bitrate: self.prefs.use_peak_bitrate,
                slot: slot.clone(),
                // Farm: one full deliver (not testN_ multi-slot).
                test_batch: None,
                chunks: if use_chunks { chunk_n } else { 1 },
            };

            if use_chunks {
                let job_built = match build_farm_chunked_job(&farm) {
                    Ok(j) => j,
                    Err(e) => {
                        self.push_session(format!("Flamenco chunk argv: {e}"));
                        failed += 1;
                        continue;
                    }
                };
                let out_name = job_built
                    .final_output
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| job_built.final_output.display().to_string());
                if job_built.final_output.is_file() {
                    self.push_session(format!("Flamenco skip (exists): {out_name}"));
                    skipped += 1;
                    continue;
                }
                if let Err(e) = write_farm_ffconcat(&job_built) {
                    self.push_session(format!("Flamenco ffconcat: {e}"));
                    failed += 1;
                    continue;
                }
                let mut integrity = farm_integrity_for_chunked(&farm, &job_built);
                integrity.check_exe = resolve_flamenco_check_exe(
                    &self.prefs.flamenco_check_exe,
                    &self.prefs.flamenco_scripts_dir,
                );
                let job = match make_gigamux_chunked_job(
                    format!("GigaMux {name}"),
                    exe,
                    &job_built.encode_argvs,
                    &job_built.mux_argv,
                    &job_built.mux_remux_argv,
                    job_built.out_expect_dur_s,
                    tag,
                    twoway,
                    cancel_exec,
                    Some(integrity),
                ) {
                    Ok(j) => j,
                    Err(e) => {
                        self.push_session(format!("Flamenco job: {e}"));
                        failed += 1;
                        continue;
                    }
                };
                match submit_job(&url, &job) {
                    Ok(id) => {
                        self.push_session(format!(
                            "Flamenco submitted: job {id}; {name} → {out_name} ({} chunks, {}k / {})",
                            chunk_n, job_built.bitrate_k, job_built.bitrate_label
                        ));
                        ok += 1;
                    }
                    Err(e) => {
                        self.push_session(format!("Flamenco submit failed ({name}): {e}"));
                        failed += 1;
                        if !script_alerted {
                            if let Some(hint) = flamenco_script_hint(&e) {
                                self.alert_error_with(
                                    "Flamenco",
                                    hint,
                                    AlertAfterOk::OpenSettingsFiles,
                                );
                                script_alerted = true;
                                break;
                            }
                        }
                    }
                }
                continue;
            }

            if want_chunk && only_soft && hw_opaque {
                self.push_session(format!(
                    "Flamenco: {name} HW opaque — single encode (only-chunk-software)"
                ));
            }
            let cmd = match build_farm_ffmpeg_argv(&farm) {
                Ok(c) => c,
                Err(e) => {
                    self.push_session(format!("Flamenco argv: {e}"));
                    failed += 1;
                    continue;
                }
            };
            let out_name = cmd
                .output
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| cmd.output.display().to_string());
            // Same as local encode: never replace an existing deliver (do not submit).
            if cmd.output.is_file() {
                self.push_session(format!("Flamenco skip (exists): {out_name}"));
                skipped += 1;
                continue;
            }
            let mut integrity = farm_integrity_for_single(&farm, &cmd);
            integrity.check_exe = resolve_flamenco_check_exe(
                &self.prefs.flamenco_check_exe,
                &self.prefs.flamenco_scripts_dir,
            );
            let job = match make_gigamux_encode_job(
                format!("GigaMux {name}"),
                exe,
                &cmd.argv,
                tag,
                twoway,
                cancel_exec,
                Some(integrity),
            ) {
                Ok(j) => j,
                Err(e) => {
                    self.push_session(format!("Flamenco job: {e}"));
                    failed += 1;
                    continue;
                }
            };
            match submit_job(&url, &job) {
                Ok(id) => {
                    self.push_session(format!(
                        "Flamenco submitted: job {id}; {name} → {out_name} ({}k / {})",
                        cmd.bitrate_k, cmd.bitrate_label
                    ));
                    ok += 1;
                }
                Err(e) => {
                    self.push_session(format!("Flamenco submit failed ({name}): {e}"));
                    failed += 1;
                    if !script_alerted {
                        if let Some(hint) = flamenco_script_hint(&e) {
                            self.alert_error_with(
                                "Flamenco",
                                hint,
                                AlertAfterOk::OpenSettingsFiles,
                            );
                            script_alerted = true;
                            break;
                        }
                    }
                }
            }
        }
        if failed > 0 && !script_alerted {
            self.alert_error(
                "Flamenco",
                format!(
                    "Flamenco batch: submitted={ok} skipped={skipped} failed={failed} (of {})",
                    names.len()
                ),
            );
        } else if names.len() > 1 || skipped > 0 {
            self.push_session(format!(
                "Flamenco batch: submitted={ok} skipped={skipped} failed={failed} (of {})",
                names.len()
            ));
        }
        if ok > 0 {
            let _ = prefs::save(&self.prefs);
        }
    }

    /// Encode all inputs, or only `only_name` when set (context-menu selection).
    fn start_encode(&mut self, only_name: Option<String>) {
        if self.busy || self.delete_queue.is_some() || self.history_nav.is_some() {
            return;
        }
        if !prefs::dir_ok(&self.prefs.input_dir) || !prefs::dir_ok(&self.prefs.output_dir) {
            self.alert_error(
                "Encode",
                "set both input and output folders before Encode",
            );
            return;
        }
        self.commit_all_slot_edits();
        // Missing AV1 NVENC / QSV: checkbox is greyed; encode falls back to CPU without clearing prefs.
        self.start_encode_now(only_name);
    }

    /// Spawn the encode worker (caller already cleared folder guards).
    fn start_encode_now(&mut self, only_name: Option<String>) {
        // Hardware can change mid-session — re-probe; do not reuse a stale catalog.
        self.refresh_hw_probe("encode");
        let _ = prefs::save(&self.prefs);
        self.cancel.store(false, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        // Prefer prefs HW flags, but only when each slot’s opaque target has a HW path here.
        let mut slot1 = self.prefs.slot1.clone();
        slot1.use_hardware = self.effective_use_hardware_for(&slot1);
        let mut slot2 = self.prefs.slot2.clone();
        slot2.use_hardware = self.effective_use_hardware_for(&slot2);
        let mut slot3 = self.prefs.slot3.clone();
        slot3.use_hardware = self.effective_use_hardware_for(&slot3);
        let mut slot4 = self.prefs.slot4.clone();
        slot4.use_hardware = self.effective_use_hardware_for(&slot4);
        let opts = EncodeOptions {
            input_dir: self.prefs.input_dir.clone(),
            output_dir: self.prefs.output_dir.clone(),
            gpu: GpuChoice::Any,
            hw: self.hw.clone(),
            preserve_obs_metadata: self.prefs.preserve_obs_metadata,
            write_encoder_metadata: self.prefs.write_encoder_metadata,
            use_peak_bitrate: self.prefs.use_peak_bitrate,
            test_mode: self.prefs.test_mode,
            test_slot_count: self.prefs.test_slot_count,
            slot1,
            slot2,
            slot3,
            slot4,
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
        thread::spawn(move || {
            run_batch(opts, cancel, |ev| {
                let _ = tx.send(AppEvent::Encode(ev));
            });
        });
    }

    /// Request cancel of the running encode/VMAF (kills in-flight ffmpeg tree, then stops the batch).
    fn cancel_encode(&mut self) {
        // Shared cancel flag for encode + VMAF (both drive ffmpeg).
        if matches!(self.job, Some(JobKind::Encode | JobKind::Vmaf)) && self.busy {
            // Resume a Shutter-paused tree first, then tree-kill immediately (don't wait for the worker's 50ms poll — Esc spam was orphaning Chocolatey-shim grandchildren).
            self.resume_ffmpeg_if_suspended();
            if let Some(pid) = active_ffmpeg_pid() {
                kill_process_tree(pid);
            }
            self.cancel.store(true, Ordering::Relaxed);
            self.push_session("cancel requested…");
        }
    }

    /// Move selection by `delta` within the active side's list (clamped).
    fn navigate_list(&mut self, delta: isize) {
        let Some((side, cur)) = self.active.clone() else {
            return;
        };
        let names = self.names_for_nav(side);
        if names.is_empty() {
            return;
        }
        let Some(idx) = names.iter().position(|n| n == &cur) else {
            return;
        };
        let next = (idx as isize + delta).clamp(0, names.len() as isize - 1) as usize;
        if next != idx {
            self.select_file(side, names[next].clone());
            self.scroll_sel_into_view = true;
        }
    }

    /// Shift+↑/↓/PgUp/PgDn: inclusive range from selection anchor to the landed row.
    fn navigate_list_extend(&mut self, delta: isize) {
        let Some((side, cur)) = self.active.clone() else {
            return;
        };
        let names = self.names_for_nav(side);
        if names.is_empty() {
            return;
        }
        let Some(idx) = names.iter().position(|n| n == &cur) else {
            return;
        };
        let next = (idx as isize + delta).clamp(0, names.len() as isize - 1) as usize;
        self.select_range(side, names[next].clone());
        self.scroll_sel_into_view = true;
    }

    /// Jump selection to the first (`to_start`) or last file on the active side.
    fn navigate_list_edge(&mut self, to_start: bool) {
        let Some((side, _)) = self.active else {
            return;
        };
        let names = self.names_for_nav(side);
        let Some(name) = (if to_start {
            names.first()
        } else {
            names.last()
        })
        .cloned() else {
            return;
        };
        if self.active.as_ref().is_some_and(|(_, n)| *n == name) {
            self.scroll_sel_into_view = true;
            return;
        }
        self.select_file(side, name);
        self.scroll_sel_into_view = true;
    }

    /// Shift+Home/End: inclusive range from selection anchor to list edge; primary lands on the edge.
    fn navigate_list_edge_extend(&mut self, to_start: bool) {
        let Some((side, _)) = self.active.clone() else {
            return;
        };
        let names = self.names_for_nav(side);
        let Some(name) = (if to_start {
            names.first()
        } else {
            names.last()
        })
        .cloned() else {
            return;
        };
        self.select_range(side, name);
        self.scroll_sel_into_view = true;
    }

    /// Stable id for Output-column test slot collapsing headers (keyboard can force-open).
    fn out_test_slot_id(batch: TestBatch) -> egui::Id {
        egui::Id::unique(("gigamux_out_test_slot", batch.index() as u8))
    }

    fn set_out_test_slot_open(ctx: &egui::Context, batch: TestBatch, open: bool) {
        let mut st = egui::collapsing_header::CollapsingState::load_with_default_open(
            ctx,
            Self::out_test_slot_id(batch),
            open,
        );
        st.set_open(open);
        st.store(ctx);
    }

    /// Expand the Output test-slot panel that owns `name` and request its header into view.
    fn focus_out_test_slot_for(&mut self, ctx: &egui::Context, name: &str) {
        if !self.prefs.test_mode {
            return;
        }
        let (Some(batch), _) = strip_test_prefix(name) else {
            return;
        };
        if self.prefs.active_batches().contains(&batch) {
            Self::set_out_test_slot_open(ctx, batch, true);
            self.focus_out_test_slot = Some(batch);
            ctx.request_repaint();
        }
    }

    /// ← input / → output: jump to the same-stem file on that side.
    /// `prefer` selects which test batch when landing on Output in test mode.
    fn jump_to_side_prefer(&mut self, ctx: &egui::Context, target: FileSide, prefer: TestBatch) {
        let Some((side, name)) = self.active.clone() else {
            return;
        };
        if side == target {
            return;
        }
        if let Some(other) = self.counterpart_name_prefer(side, &name, prefer) {
            if target == FileSide::Output {
                self.focus_out_test_slot_for(ctx, &other);
            }
            self.select_file(target, other);
            // Keep the landed row on-screen (same path as ↑/↓ / Home/End).
            self.scroll_sel_into_view = true;
        }
    }

    /// Shift+↑/↓ on Output in test mode: cycle among active test encodes for this stem.
    fn jump_sibling_test(&mut self, ctx: &egui::Context, forward: bool) {
        let Some((side, name)) = self.active.clone() else {
            return;
        };
        if side != FileSide::Output || !self.prefs.test_mode {
            return;
        }
        if let Some(sib) = self.sibling_test_name(&name, forward) {
            self.focus_out_test_slot_for(ctx, &sib);
            self.select_file(FileSide::Output, sib);
            self.scroll_sel_into_view = true;
        }
    }

    /// Stem + extension (extension includes the leading `.`, or empty).
    fn split_file_name(name: &str) -> (String, String) {
        let p = Path::new(name);
        let stem = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(name)
            .to_string();
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        (stem, ext)
    }

    /// True when `name` is illegal as a basename.
    fn rename_name_invalid(name: &str) -> bool {
        name.is_empty()
            || name.contains(['/', '\\', '\0'])
            || name == "."
            || name == ".."
    }

    /// F2: start inline rename on the active row (bulk applies to the whole selection on commit).
    fn begin_rename_active(&mut self) {
        let Some((side, name)) = self.active.clone() else {
            return;
        };
        if self.busy
            || self.delete_queue.is_some()
            || self.history_nav.is_some()
            || self.pending_recycle_delete.is_some()
            || self.pending_permanent_delete.is_some()
        {
            return;
        }
        // Multi-select: draft the stem only (Explorer-style); each file keeps its own extension.
        let stem_only = self.selected.len() > 1;
        let draft = if stem_only {
            Self::split_file_name(&name).0
        } else {
            name.clone()
        };
        self.rename_edit = Some(RenameEdit {
            side,
            old_name: name,
            baseline: draft.clone(),
            draft,
            stem_only,
            request_focus: true,
        });
    }

    /// Longest contiguous shared run of `a`/`b` as byte offsets `(a0, b0, len)`.
    fn longest_common_substring(a: &str, b: &str) -> Option<(usize, usize, usize)> {
        let ac: Vec<char> = a.chars().collect();
        let bc: Vec<char> = b.chars().collect();
        let mut best_i = 0usize;
        let mut best_j = 0usize;
        let mut best_len = 0usize;
        for i in 0..ac.len() {
            for j in 0..bc.len() {
                let mut k = 0;
                while i + k < ac.len() && j + k < bc.len() && ac[i + k] == bc[j + k] {
                    k += 1;
                }
                if k > best_len {
                    best_i = i;
                    best_j = j;
                    best_len = k;
                }
            }
        }
        if best_len == 0 {
            return None;
        }
        let byte_len = |chars: &[char], start: usize, n: usize| -> usize {
            chars[start..start + n].iter().map(|c| c.len_utf8()).sum()
        };
        let a0 = byte_len(&ac, 0, best_i);
        let b0 = byte_len(&bc, 0, best_j);
        let n = byte_len(&ac, best_i, best_len);
        Some((a0, b0, n))
    }

    /// Infer strip/add affixes from editing `baseline` into `draft` (shared core via LCS).
    fn infer_rename_affix_edit(baseline: &str, draft: &str) -> Option<RenameAffixEdit> {
        if baseline.is_empty() {
            return None;
        }
        if baseline == draft {
            return Some(RenameAffixEdit {
                rem_pre: String::new(),
                rem_suf: String::new(),
                add_pre: String::new(),
                add_suf: String::new(),
            });
        }
        let (a0, b0, n) = Self::longest_common_substring(baseline, draft)?;
        Some(RenameAffixEdit {
            rem_pre: baseline[..a0].to_string(),
            rem_suf: baseline[a0 + n..].to_string(),
            add_pre: draft[..b0].to_string(),
            add_suf: draft[b0 + n..].to_string(),
        })
    }

    /// Apply a multi-select affix edit to one stem/basename unit.
    fn apply_rename_affix_edit(unit: &str, edit: &RenameAffixEdit) -> Result<String, String> {
        let mut s = unit;
        if !edit.rem_pre.is_empty() {
            s = s.strip_prefix(&edit.rem_pre).ok_or_else(|| {
                format!("\"{unit}\" is missing prefix \"{}\"", edit.rem_pre)
            })?;
        }
        if !edit.rem_suf.is_empty() {
            s = s.strip_suffix(&edit.rem_suf).ok_or_else(|| {
                format!("\"{unit}\" is missing suffix \"{}\"", edit.rem_suf)
            })?;
        }
        Ok(format!("{}{s}{}", edit.add_pre, edit.add_suf))
    }

    /// Remap caches/selection after one basename changes on `side`.
    fn remap_after_rename(&mut self, side: FileSide, old_name: &str, new_name: &str, new_path: &Path) {
        let old_key = Self::preview_key(side, old_name);
        let new_key = Self::preview_key(side, new_name);
        if let Some(mut entry) = self.preview_cache.remove(&old_key) {
            entry.source_path = Some(new_path.to_path_buf());
            self.preview_cache.insert(new_key, entry);
        }
        if side == FileSide::Input {
            if let Some(lines) = self.file_logs.remove(old_name) {
                self.file_logs.insert(new_name.to_string(), lines);
            }
        }
        if self.selected.remove(old_name) {
            self.selected.insert(new_name.to_string());
        }
        if self.selection_anchor.as_deref() == Some(old_name) {
            self.selection_anchor = Some(new_name.to_string());
        }
    }

    /// Two-phase basename swap on `side`. `pairs` are `(from, to)`; skips identity. Returns applied pairs.
    fn apply_rename_pairs(
        &mut self,
        side: FileSide,
        pairs: &[(String, String)],
    ) -> Result<Vec<(String, String)>, String> {
        let dir = match side {
            FileSide::Input => self.prefs.input_dir.clone(),
            FileSide::Output => self.prefs.output_dir.clone(),
        };
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .filter(|(a, b)| a != b)
            .cloned()
            .collect();
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let from_set: HashSet<String> = pairs.iter().map(|(o, _)| o.clone()).collect();
        for (_, new_name) in &pairs {
            if Self::rename_name_invalid(new_name) {
                return Err(format!("invalid name ({new_name})"));
            }
            let dest = dir.join(new_name);
            if dest.exists() && !from_set.contains(new_name) {
                return Err(format!("{new_name} already exists"));
            }
        }
        {
            let mut seen = HashSet::new();
            for (_, new_name) in &pairs {
                if !seen.insert(new_name.clone()) {
                    return Err(format!("duplicate target {new_name}"));
                }
            }
        }
        // Two-phase so A→B and B→C can't clobber mid-batch.
        let mut temps: Vec<(String, PathBuf, String)> = Vec::new();
        for (i, (old_name, new_name)) in pairs.iter().enumerate() {
            let from = dir.join(old_name);
            let temp = dir.join(format!(".gigamux_renaming_{i}_{}", std::process::id()));
            std::fs::rename(&from, &temp).map_err(|e| format!("{old_name}: {e}"))?;
            temps.push((old_name.clone(), temp, new_name.clone()));
        }
        let mut done: Vec<(String, String)> = Vec::new();
        for (old_name, temp, new_name) in temps {
            let to = dir.join(&new_name);
            std::fs::rename(&temp, &to).map_err(|e| format!("{old_name} → {new_name}: {e}"))?;
            self.remap_after_rename(side, &old_name, &new_name, &to);
            done.push((old_name, new_name));
        }
        Ok(done)
    }

    /// Apply the inline rename draft (Enter = single / numbered multi; Alt+Enter = affix pattern).
    fn commit_rename(&mut self, mode: RenameCommitMode) {
        let Some(edit) = self.rename_edit.take() else {
            return;
        };
        let mut draft = edit.draft.trim().to_string();
        // Stem-only edit: ignore a typed active extension so numbering/pattern stay on stems.
        if edit.stem_only {
            let (_, ext) = Self::split_file_name(&edit.old_name);
            if !ext.is_empty() && draft.ends_with(&ext) {
                draft.truncate(draft.len() - ext.len());
            }
        }
        if Self::rename_name_invalid(&draft) {
            self.alert_error("Rename", "rename failed: invalid name");
            return;
        }
        // Selection on this side, list order; always includes the edited active when multi.
        let mut names = self.selected_names_ordered();
        if names.is_empty() {
            names.push(edit.old_name.clone());
        }
        if !names.iter().any(|n| n == &edit.old_name) {
            names.push(edit.old_name.clone());
        }

        let bulk = names.len() > 1;
        let pairs: Vec<(String, String)> = match mode {
            RenameCommitMode::Default if !bulk => {
                // Single: draft is the new basename (stem + kept ext when stem_only).
                let new_name = if edit.stem_only {
                    let (_, ext) = Self::split_file_name(&edit.old_name);
                    format!("{draft}{ext}")
                } else {
                    draft.clone()
                };
                if new_name == edit.old_name {
                    return;
                }
                vec![(edit.old_name.clone(), new_name)]
            }
            RenameCommitMode::Default => {
                // Multi Enter: `stem (1).ext` … keeping each file's original extension.
                let (stem, _) = Self::split_file_name(&draft);
                if Self::rename_name_invalid(&stem) {
                    self.alert_error("Rename", "rename failed: invalid name");
                    return;
                }
                names
                    .iter()
                    .enumerate()
                    .map(|(i, old)| {
                        let (_, ext) = Self::split_file_name(old);
                        let new_name = format!("{stem} ({}){ext}", i + 1);
                        (old.clone(), new_name)
                    })
                    .collect()
            }
            RenameCommitMode::Pattern => {
                // Alt+Enter: same strip/add that turned baseline → draft across selection.
                let Some(affix) = Self::infer_rename_affix_edit(&edit.baseline, &draft) else {
                    self.alert_error(
                        "Rename",
                        "rename pattern failed: could not match edit to the original name",
                    );
                    return;
                };
                let mut pairs = Vec::with_capacity(names.len());
                for old in &names {
                    let (unit, ext) = if edit.stem_only {
                        let (stem, ext) = Self::split_file_name(old);
                        (stem, ext)
                    } else {
                        (old.clone(), String::new())
                    };
                    match Self::apply_rename_affix_edit(&unit, &affix) {
                        Ok(new_unit) => {
                            let new_name = format!("{new_unit}{ext}");
                            if Self::rename_name_invalid(&new_name) {
                                self.alert_error(
                                    "Rename",
                                    format!("rename failed: invalid name ({new_name})"),
                                );
                                return;
                            }
                            pairs.push((old.clone(), new_name));
                        }
                        Err(e) => {
                            self.alert_error(
                                "Rename",
                                format!("rename pattern failed: {e}"),
                            );
                            return;
                        }
                    }
                }
                pairs
            }
        };
        // Full target set before identity filter (needed so multi-select survives no-op members).
        let bulk_targets: Vec<String> = pairs.iter().map(|(_, n)| n.clone()).collect();

        let done = match self.apply_rename_pairs(edit.side, &pairs) {
            Ok(d) => d,
            Err(e) => {
                self.alert_error("Rename", format!("rename failed: {e}"));
                return;
            }
        };
        if done.is_empty() {
            return;
        }

        self.push_undo_step(UndoStep::Rename(RenameUndoBatch {
            side: edit.side,
            pairs: done.clone(),
        }));

        self.refresh_lists();
        // Keep focus on the renamed active (its new name).
        let focus = done
            .iter()
            .find(|(o, _)| o == &edit.old_name)
            .map(|(_, n)| n.clone())
            .or_else(|| done.last().map(|(_, n)| n.clone()));
        if let Some(n) = focus {
            if bulk {
                // Restore the whole batch so a follow-up F2 still multi-selects.
                let set: HashSet<String> = bulk_targets.iter().cloned().collect();
                self.selection_anchor = self
                    .names_for(edit.side)
                    .into_iter()
                    .find(|x| set.contains(x));
                self.selected = set;
                self.set_primary(edit.side, n);
            } else {
                self.select_file(edit.side, n);
            }
        }
        if done.len() == 1 {
            let (o, n) = &done[0];
            self.push_session(format!("renamed: {o} → {n}"));
        } else {
            self.push_session(format!("renamed {} files", done.len()));
        }
        self.save_preview_cache_to_disk();
    }

    /// Delete all selected files (Del). Refuses while a job / delete cascade is running.
    /// Warns before Recycle Bin / permanent delete unless the user opted out.
    fn delete_active(&mut self) {
        let Some((side, _)) = self.active else {
            return;
        };
        if self.busy {
            self.alert_error("Delete", "can't delete while a job is running");
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
            self.push_undo_step(UndoStep::Delete(DeleteUndoBatch {
                side,
                paths: q.recycled,
            }));
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
                self.alert_error("Delete", format!("delete failed ({name}): {e}"));
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
                        self.alert_error(
                            "Delete",
                            format!("permanent delete failed ({name}): {e2}"),
                        );
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

    /// Register a completed action at the tip (truncates any redo branch first).
    fn push_undo_step(&mut self, step: UndoStep) {
        let empty = match &step {
            UndoStep::Delete(b) => b.paths.is_empty(),
            UndoStep::Rename(b) => b.pairs.is_empty(),
        };
        if empty {
            return;
        }
        let h = &mut self.undo_history;
        h.steps.truncate(h.cursor);
        h.steps.push(step);
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
            self.alert_error("History", "can't jump history while a job is running");
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
        let side = batch.side();
        match batch {
            // Rename is instantaneous (no Recycle Bin pacing).
            UndoStep::Rename(ren) => {
                let pairs: Vec<(String, String)> = match dir {
                    HistoryDir::Undo => ren.pairs.into_iter().map(|(o, n)| (n, o)).collect(),
                    HistoryDir::Redo => ren.pairs,
                };
                match self.apply_rename_pairs(side, &pairs) {
                    Ok(done) => {
                        let names: Vec<String> = done.iter().map(|(_, n)| n.clone()).collect();
                        if let Some(nav) = self.history_nav.as_mut() {
                            nav.side = side;
                            nav.dir = dir;
                            nav.remaining.clear();
                            nav.touched = names.clone();
                        }
                        self.refresh_lists();
                        if !names.is_empty() {
                            self.select_restored(side, &names);
                        }
                        self.save_preview_cache_to_disk();
                        if done.len() == 1 {
                            let (o, n) = &done[0];
                            self.push_session(format!("renamed: {o} → {n}"));
                        } else if !done.is_empty() {
                            self.push_session(format!("renamed {} files", done.len()));
                        }
                    }
                    Err(e) => {
                        self.alert_error(
                            "History",
                            format!("rename undo/redo failed: {e}"),
                        );
                    }
                }
                self.history_nav_start_next_batch();
            }
            UndoStep::Delete(del) => {
                let remaining: VecDeque<PathBuf> = match dir {
                    // Last-trashed file reappears first.
                    HistoryDir::Undo => del.paths.into_iter().rev().collect(),
                    // Same order as the original delete cascade.
                    HistoryDir::Redo => del.paths.into_iter().collect(),
                };
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
                    self.alert_error("History", format!("undo failed ({name}): {e}"));
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
                    self.alert_error(
                        "History",
                        format!("redo delete failed ({name}): {e}"),
                    );
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
                    self.alert_error(
                        "Delete",
                        format!("permanent delete failed ({name}): {e}"),
                    );
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
        let (
            ctrl_j,
            ctrl_z,
            ctrl_shift_z,
            f5,
            ctrl_f5,
            f12,
            esc,
            arrow_up,
            arrow_down,
            arrow_left,
            arrow_right,
            home,
            end,
            page_up,
            page_down,
            delete,
            f2,
            ctrl,
            shift,
            ctrl_a,
        ) = ctx.input(|i| {
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
                i.key_pressed(egui::Key::Home),
                i.key_pressed(egui::Key::End),
                i.key_pressed(egui::Key::PageUp),
                i.key_pressed(egui::Key::PageDown),
                i.key_pressed(egui::Key::Delete),
                i.key_pressed(egui::Key::F2),
                i.modifiers.command,
                i.modifiers.shift,
                i.modifiers.command && i.key_pressed(egui::Key::A),
            )
        });
        // Ctrl+J toggles log even while typing in a field.
        if ctrl_j {
            self.log_open = !self.log_open;
        }
        // Esc cancels an in-list rename even while the TextEdit holds focus.
        if esc && self.rename_edit.is_some() {
            self.rename_edit = None;
            return;
        }
        // Skip remaining keys when a text field has focus.
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        if f2 {
            self.begin_rename_active();
        }
        // Ctrl+A: select all in the Input/Output pane while hovering over one or the other.
        if ctrl_a {
            if let Some(p) = Self::dnd_pointer(ctx).or_else(|| ctx.pointer_latest_pos()) {
                if let Some(side) = self.pane_at(p) {
                    self.select_all_on_side(side);
                }
            }
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
            if self.pending_alert.is_some() {
                self.pending_alert = None;
            } else if self.pending_purge.is_some() {
                self.pending_purge = None;
            } else if self.pending_permanent_delete.is_some() {
                self.pending_permanent_delete = None;
                if let Some(q) = self.delete_queue.as_mut() {
                    q.next_at = Instant::now() + Duration::from_millis(DELETE_STEP_MS);
                }
            } else if self.pending_recycle_delete.is_some() {
                self.pending_recycle_delete = None;
            } else if self.pending_flamenco.is_some() {
                self.pending_flamenco = None;
            } else if self.delete_queue.is_some() {
                self.finish_delete_queue();
                self.push_session("delete cancelled");
            } else if self.history_nav.is_some() {
                self.cancel_history_nav();
            } else if self.pending_check.is_some() {
                self.pending_check = None;
            } else if self.show_settings {
                self.commit_all_slot_edits();
                let _ = prefs::save(&self.prefs);
                self.show_settings = false;
            } else {
                self.cancel_encode();
            }
        }
        // List nav: ↑/↓ step; Ctrl+↑/↓ + Home/End jump edges; Shift+Home/End/PgUp/PgDn range-select.
        if home {
            if shift {
                self.navigate_list_edge_extend(true);
            } else {
                self.navigate_list_edge(true);
            }
        } else if end {
            if shift {
                self.navigate_list_edge_extend(false);
            } else {
                self.navigate_list_edge(false);
            }
        } else if ctrl && arrow_up {
            self.navigate_list_edge(true);
        } else if ctrl && arrow_down {
            self.navigate_list_edge(false);
        } else if page_up {
            let page = self.folder_page_rows.max(1) as isize;
            if shift {
                self.navigate_list_extend(-page);
            } else {
                self.navigate_list(-page);
            }
        } else if page_down {
            let page = self.folder_page_rows.max(1) as isize;
            if shift {
                self.navigate_list_extend(page);
            } else {
                self.navigate_list(page);
            }
        } else if arrow_up {
            // Test mode + Output: Shift+↑/↓ cycles active test siblings; else normal step.
            if shift
                && self.prefs.test_mode
                && self
                    .active
                    .as_ref()
                    .is_some_and(|(s, _)| *s == FileSide::Output)
            {
                self.jump_sibling_test(ctx, false);
            } else {
                self.navigate_list(-1);
            }
        } else if arrow_down {
            if shift
                && self.prefs.test_mode
                && self
                    .active
                    .as_ref()
                    .is_some_and(|(s, _)| *s == FileSide::Output)
            {
                self.jump_sibling_test(ctx, true);
            } else {
                self.navigate_list(1);
            }
        }
        if arrow_left {
            let prefer = if shift && self.prefs.test_mode {
                self.cycle_test_nav_prefer(false)
            } else {
                TestBatch::One
            };
            self.jump_to_side_prefer(ctx, FileSide::Input, prefer);
        }
        if arrow_right {
            let prefer = if shift && self.prefs.test_mode {
                self.cycle_test_nav_prefer(true)
            } else {
                TestBatch::One
            };
            self.jump_to_side_prefer(ctx, FileSide::Output, prefer);
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
            self.alert_error(
                "Check",
                "set both input and output folders before Check",
            );
            return;
        }
        let input = self.prefs.input_dir.clone();
        let output = self.prefs.output_dir.clone();
        let slots = self.prefs.check_vmaf_slots();
        let (tx, rx) = mpsc::channel();
        self.busy = true;
        self.job = Some(JobKind::Check);
        self.event_rx = Some(rx);
        self.push_session("Check started…");
        thread::spawn(move || {
            let result = run_check(&input, &output, &slots);
            let _ = tx.send(AppEvent::CheckDone(result));
        });
    }

    /// VMAF input→output for one basename, or every matched pair when `None`.
    fn start_vmaf(&mut self, only_name: Option<String>) {
        if self.busy || self.delete_queue.is_some() || self.history_nav.is_some() {
            return;
        }
        if !self.vmaf_available {
            self.alert_error(
                "VMAF",
                "VMAF unavailable — ffmpeg has no libvmaf filter (install a full build)",
            );
            return;
        }
        if !prefs::dir_ok(&self.prefs.input_dir) || !prefs::dir_ok(&self.prefs.output_dir) {
            self.alert_error(
                "VMAF",
                "set both input and output folders before VMAF",
            );
            return;
        }
        let input = self.prefs.input_dir.clone();
        let output = self.prefs.output_dir.clone();
        let slots = self.prefs.check_vmaf_slots();
        self.cancel.store(false, Ordering::Relaxed);
        let cancel = Arc::clone(&self.cancel);
        let (tx, rx) = mpsc::channel();
        self.busy = true;
        self.job = Some(JobKind::Vmaf);
        self.event_rx = Some(rx);
        let label = only_name
            .as_deref()
            .map(|n| format!("VMAF started — {n}"))
            .unwrap_or_else(|| "VMAF started — all pairs".into());
        self.push_session(label);
        self.encode_progress = None;
        thread::spawn(move || {
            let tx_ev = tx.clone();
            let result = run_vmaf(
                &input,
                &output,
                &slots,
                only_name,
                None,
                &cancel,
                |ev| {
                    let _ = tx_ev.send(AppEvent::Vmaf(ev));
                },
            );
            let _ = tx.send(AppEvent::VmafDone(result));
        });
    }

    /// Output basenames that exist on disk but have no cached VMAF score yet.
    fn unscored_output_names(&self) -> HashSet<String> {
        self.names_for(FileSide::Output)
            .into_iter()
            .filter(|n| self.live_vmaf_score(FileSide::Output, n).is_none())
            .collect()
    }

    /// Arm auto VMAF to fire once app-idle after `delay` (ZERO = ASAP).
    fn schedule_auto_vmaf(&mut self, delay: Duration) {
        self.auto_vmaf_armed = true;
        self.auto_vmaf_due_at = Some(Instant::now() + delay);
    }

    /// Drop the pending auto VMAF schedule (pref off or unrecoverable skip).
    fn clear_auto_vmaf_schedule(&mut self) {
        self.auto_vmaf_armed = false;
        self.auto_vmaf_due_at = None;
    }

    /// Resume a suspended ffmpeg tree (toggle off, or job ended while paused).
    fn resume_ffmpeg_if_suspended(&mut self) {
        if !self.ffmpeg_suspended {
            return;
        }
        if let Some(pid) = active_ffmpeg_pid() {
            let _ = ffmpeg_idle_pause::resume_process_tree(pid);
        }
        self.ffmpeg_suspended = false;
        self.last_system_idle_ms = u64::MAX;
    }

    /// Shutter-style: suspend active ffmpeg on fresh input; resume after [`ffmpeg_idle_pause::IDLE_RESUME`].
    fn tick_ffmpeg_idle_pause(&mut self) {
        if !self.prefs.work_during_inactivity {
            self.resume_ffmpeg_if_suspended();
            return;
        }
        let Some(pid) = active_ffmpeg_pid() else {
            self.ffmpeg_suspended = false;
            self.last_system_idle_ms = u64::MAX;
            return;
        };
        let idle = ffmpeg_idle_pause::system_idle();
        let idle_ms = idle.as_millis() as u64;
        // Fresh input while running → pause immediately (do not treat the Start click as a pause).
        if idle_ms < self.last_system_idle_ms && !self.ffmpeg_suspended {
            if ffmpeg_idle_pause::suspend_process_tree(pid) {
                self.ffmpeg_suspended = true;
                self.note_session_ui(
                    "ffmpeg paused (user activity) — resumes after 1 s idle",
                );
            }
        }
        self.last_system_idle_ms = idle_ms;
        if self.ffmpeg_suspended && idle >= ffmpeg_idle_pause::IDLE_RESUME {
            if ffmpeg_idle_pause::resume_process_tree(pid) {
                self.ffmpeg_suspended = false;
                self.note_session_ui("ffmpeg resumed");
            }
        }
    }

    /// When armed, due, and idle: silent integrity Check, then VMAF only unscored intact outputs.
    fn try_start_auto_vmaf(&mut self) {
        if !self.auto_vmaf_armed || !self.prefs.auto_vmaf_when_idle {
            return;
        }
        let Some(due) = self.auto_vmaf_due_at else {
            return;
        };
        if Instant::now() < due {
            return;
        }
        if !self.history_idle() || self.pending_check.is_some() {
            return;
        }
        if !prefs::dir_ok(&self.prefs.input_dir) || !prefs::dir_ok(&self.prefs.output_dir) {
            self.clear_auto_vmaf_schedule();
            self.push_session("auto VMAF: skipped — set input and output folders");
            return;
        }
        if !self.vmaf_available {
            self.clear_auto_vmaf_schedule();
            self.push_session("auto VMAF: skipped — libvmaf unavailable");
            return;
        }
        let total_out = self.names_for(FileSide::Output).len();
        let unscored = self.unscored_output_names();
        let already = total_out.saturating_sub(unscored.len());
        if unscored.is_empty() {
            // Caught up: keep watching hourly for new / still-writing outputs.
            if total_out == 0 {
                self.push_session("auto VMAF: nothing to score — next check in 1 h");
            } else {
                self.push_session(format!(
                    "auto VMAF: skipped — {already} already scored, 0 unscored — next check in 1 h"
                ));
            }
            self.schedule_auto_vmaf(AUTO_VMAF_RETRY_CAUGHT_UP);
            return;
        }
        self.clear_auto_vmaf_schedule();
        self.start_auto_vmaf(unscored, already);
    }

    /// Background: run_check (no purge UI) → filter High/Medium → run_vmaf allowlist.
    fn start_auto_vmaf(&mut self, unscored: HashSet<String>, already_scored: usize) {
        let input = self.prefs.input_dir.clone();
        let output = self.prefs.output_dir.clone();
        let slots = self.prefs.check_vmaf_slots();
        self.cancel.store(false, Ordering::Relaxed);
        let cancel = Arc::clone(&self.cancel);
        let (tx, rx) = mpsc::channel();
        self.busy = true;
        self.job = Some(JobKind::Vmaf);
        self.auto_vmaf_pass = true;
        self.event_rx = Some(rx);
        self.push_session(format!(
            "auto VMAF: Check then score ({} unscored, {already_scored} already scored)…",
            unscored.len()
        ));
        self.encode_progress = None;
        thread::spawn(move || {
            let tx_ev = tx.clone();
            let send_log = |msg: String| {
                let _ = tx_ev.send(AppEvent::Vmaf(VmafEvent::Log(msg)));
            };
            let check = match run_check(&input, &output, &slots) {
                Ok(r) => r,
                Err(e) => {
                    send_log(format!("auto VMAF: check failed — {e}"));
                    let _ = tx.send(AppEvent::VmafDone(Err(format!("auto VMAF check: {e}"))));
                    return;
                }
            };
            let blocked = integrity_blocked_outputs(&check.issues);
            let integrity_n = unscored.iter().filter(|n| blocked.contains(*n)).count();
            let allow: HashSet<String> = unscored
                .into_iter()
                .filter(|n| !blocked.contains(n))
                .collect();
            if integrity_n > 0 {
                send_log(format!(
                    "auto VMAF: skipped {integrity_n} — integrity (no auto-delete)"
                ));
            }
            if allow.is_empty() {
                send_log("auto VMAF: nothing left to score after integrity filter".into());
                let _ = tx_ev.send(AppEvent::Vmaf(VmafEvent::Finished { ok: 0, failed: 0 }));
                let _ = tx.send(AppEvent::VmafDone(Ok(VmafReport::default())));
                return;
            }
            send_log(format!(
                "auto VMAF: scoring {} pair(s)…",
                allow.len()
            ));
            let result = run_vmaf(
                &input,
                &output,
                &slots,
                None,
                Some(allow),
                &cancel,
                |ev| {
                    let _ = tx_ev.send(AppEvent::Vmaf(ev));
                },
            );
            let _ = tx.send(AppEvent::VmafDone(result));
        });
    }

    fn poll_events(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.event_rx.take() else {
            return;
        };
        let mut finished = false;
        // Capture before the job clears so only auto Check→VMAF passes reschedule.
        let auto_pass = self.auto_vmaf_pass;
        let mut auto_vmaf_err = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AppEvent::Encode(EncodeEvent::Log(msg)) => {
                    // Always pin to the in-flight encode file — not UI selection (deselect must not leak into session / other files).
                    // Durable session.log already got this via JobLog; pane/file logs only here.
                    let encode_name = self
                        .encode_progress
                        .as_ref()
                        .map(|p| p.file_name.clone())
                        .filter(|n| !n.is_empty());
                    if let Some(name) = encode_name {
                        self.push_file_log(&name, msg);
                    } else {
                        self.note_session_ui(msg);
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
                        live_texture: None,
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
                        p.live_texture = None;
                    }
                }
                AppEvent::Encode(EncodeEvent::FileProgress { out_time_s }) => {
                    if let Some(p) = &mut self.encode_progress {
                        p.file_out_time_s = out_time_s.max(p.file_out_time_s);
                    }
                }
                AppEvent::Encode(EncodeEvent::LiveThumb { name, thumb }) => {
                    // Ignore stale frames from a previous file's side-thread grab.
                    let matches = self
                        .encode_progress
                        .as_ref()
                        .is_some_and(|p| p.file_name == name);
                    if matches {
                        let image = egui::ColorImage::from_rgba_unmultiplied(
                            [thumb.width as usize, thumb.height as usize],
                            &thumb.rgba,
                        );
                        let tex = ctx.load_texture(
                            format!("live-encode-{name}"),
                            image,
                            egui::TextureOptions::LINEAR,
                        );
                        if let Some(p) = &mut self.encode_progress {
                            p.live_texture = Some(tex);
                        }
                    }
                }
                AppEvent::Encode(EncodeEvent::FileSkipped { name }) => {
                    self.push_file_log(&name, format!("skip (exists): {name}"));
                    self.note_session_ui(format!("skip (exists): {name}"));
                }
                AppEvent::Encode(EncodeEvent::FileDone { name }) => {
                    self.push_file_log(&name, format!("done: {name}"));
                    self.note_session_ui(format!("done: {name}"));
                    if let Some(p) = &mut self.encode_progress {
                        p.completed_duration_s += p.file_duration_s;
                        p.file_out_time_s = p.file_duration_s;
                    }
                }
                AppEvent::Encode(EncodeEvent::FileFailed { name, error }) => {
                    self.push_file_log(&name, format!("error: {error}"));
                    self.note_session_ui(format!("error {name}: {error}"));
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
                    let summary =
                        format!("finished — ok={ok} skipped={skipped} failed={failed}");
                    self.note_session_ui(summary.clone());
                    if failed > 0 {
                        self.show_alert("Encode", summary);
                    }
                    finished = true;
                    self.refresh_lists();
                    // Immediate auto-VMAF once the batch produced at least one ok output.
                    if ok > 0 && self.prefs.auto_vmaf_when_idle {
                        self.schedule_auto_vmaf(Duration::ZERO);
                    }
                }
                AppEvent::CheckDone(result) => {
                    finished = true;
                    match result {
                        Ok(report) => self.apply_check_report(report),
                        Err(e) => self.alert_error("Check", format!("check failed: {e}")),
                    }
                    self.refresh_lists();
                }
                AppEvent::Vmaf(ev) => match ev {
                    VmafEvent::Log(msg) => {
                        // JobLog already mirrored to session.log.
                        self.note_session_ui(msg);
                    }
                    VmafEvent::BatchStarted {
                        total_files,
                        total_duration_s,
                    } => {
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
                            live_texture: None,
                        });
                    }
                    VmafEvent::FileStarted {
                        name,
                        index,
                        total,
                        duration_s,
                    } => {
                        self.select_file(FileSide::Input, name.clone());
                        if let Some(p) = &mut self.encode_progress {
                            p.file_index = index;
                            p.file_total = total;
                            p.file_name = name;
                            p.file_duration_s = duration_s;
                            p.file_out_time_s = 0.0;
                            p.file_started = Instant::now();
                            p.live_texture = None;
                        }
                    }
                    VmafEvent::FileProgress { out_time_s } => {
                        if let Some(p) = &mut self.encode_progress {
                            p.file_out_time_s = out_time_s.max(p.file_out_time_s);
                        }
                    }
                    VmafEvent::FileDone {
                        name,
                        output,
                        score,
                    } => {
                        if let Some(p) = &mut self.encode_progress {
                            p.completed_duration_s += p.file_duration_s;
                            p.file_out_time_s = p.file_duration_s;
                        }
                        // Persist immediately so cancel keeps finished pairs and the UI updates live.
                        self.persist_vmaf_score(&name, &output, score);
                    }
                    VmafEvent::FileFailed { name: _, error: _ } => {
                        if let Some(p) = &mut self.encode_progress {
                            p.completed_duration_s += p.file_duration_s;
                            p.file_out_time_s = 0.0;
                        }
                    }
                    VmafEvent::Finished { ok, failed } => {
                        let summary =
                            format!("VMAF finished — scored={ok} failed={failed}");
                        self.note_session_ui(summary.clone());
                        if failed > 0 && !auto_pass {
                            self.show_alert("VMAF", summary);
                        }
                    }
                },
                AppEvent::VmafDone(result) => {
                    finished = true;
                    match result {
                        // Scores were already stored on each FileDone; this is a safety net + log panel.
                        Ok(report) => self.apply_vmaf_report(report),
                        Err(e) => {
                            let msg = format!("VMAF failed: {e}");
                            if auto_pass {
                                auto_vmaf_err = true;
                                self.push_session(msg);
                            } else {
                                self.alert_error("VMAF", msg);
                            }
                        }
                    }
                }
            }
        }
        if finished {
            self.busy = false;
            self.job = None;
            self.encode_progress = None;
            self.event_rx = None;
            if auto_pass {
                self.auto_vmaf_pass = false;
                // 5m while unscored remain (or the pass errored); 1h when caught up.
                if self.prefs.auto_vmaf_when_idle {
                    self.refresh_lists();
                    if auto_vmaf_err || !self.unscored_output_names().is_empty() {
                        self.schedule_auto_vmaf(AUTO_VMAF_RETRY_BUSY);
                        self.push_session("auto VMAF: next check in 5 min");
                    } else {
                        self.schedule_auto_vmaf(AUTO_VMAF_RETRY_CAUGHT_UP);
                        self.push_session("auto VMAF: next check in 1 h");
                    }
                }
            }
            if self.prefs.job_done_chime {
                play_job_done_chime();
            }
        } else {
            self.event_rx = Some(rx);
            // Snappy refresh while ffmpeg progress lines stream in.
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    /// Write one VMAF score into preview cache (+ disk) as soon as a pair finishes.
    fn persist_vmaf_score(&mut self, input: &str, output: &str, score: f64) {
        let out_key = Self::preview_key(FileSide::Output, output);
        let already = self
            .preview_cache
            .get(&out_key)
            .and_then(|e| e.vmaf_score)
            .is_some();
        {
            let e = self
                .preview_cache
                .entry(out_key)
                .or_insert_with(PreviewCacheEntry::pending);
            e.vmaf_score = Some(score);
            if e.source_path.is_none() {
                e.source_path = Some(self.prefs.output_dir.join(output));
            }
        }
        // Mirror onto input only for non-test pairs so multi testN_ scores do not clobber each other.
        if strip_test_prefix(output).0.is_none() {
            let in_key = Self::preview_key(FileSide::Input, input);
            let e = self
                .preview_cache
                .entry(in_key)
                .or_insert_with(PreviewCacheEntry::pending);
            e.vmaf_score = Some(score);
            if e.source_path.is_none() {
                e.source_path = Some(self.prefs.input_dir.join(input));
            }
        }
        if !already {
            self.push_file_log(input, &format!("VMAF {input} → {output}: {score:.4}"));
        }
        self.save_preview_cache_to_disk();
    }

    /// End-of-batch safety net: any scored pairs not yet cached (should be rare after live FileDone).
    fn apply_vmaf_report(&mut self, report: VmafReport) {
        self.log_show_session = true;
        for r in &report.results {
            if let Some(s) = r.score {
                let out_key = Self::preview_key(FileSide::Output, &r.output);
                let missing = self
                    .preview_cache
                    .get(&out_key)
                    .and_then(|e| e.vmaf_score)
                    .is_none();
                if missing {
                    self.persist_vmaf_score(&r.input, &r.output, s);
                }
            }
        }
    }

    fn apply_check_report(&mut self, report: CheckReport) {
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
            // Low severity → yellow WARN; High/Medium → red ISSUE.
            let line = if check_issue_severity(issue) == CheckSeverity::Low {
                format!("WARN {label}: {}", issue.message)
            } else {
                format!("ISSUE {label}: {}", issue.message)
            };
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
        // Default purge selection: High-severity deletable outputs (Low stays unchecked).
        let out_dir = &self.prefs.output_dir;
        let mut selected = HashSet::new();
        for (idx, issue) in report.issues.iter().enumerate() {
            if !issue_has_deletable_output(out_dir, issue) {
                continue;
            }
            if check_issue_severity(issue) == CheckSeverity::High {
                selected.insert(idx);
            }
        }
        self.pending_check = Some(CheckResultsWindow {
            issues: report.issues,
            selected,
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

    /// True when this slot's GPU choice can run `av1_nvenc`.
    fn av1_nvenc_ok_for_slot(&self, slot: &EncodeSlot) -> bool {
        self.hw.av1_ok_for(slot.gpu)
    }

    fn batch_edit_idx(batch: TestBatch) -> usize {
        batch.index()
    }

    /// Whether opaque Use hardware can apply on this machine for `slot`'s codec.
    fn opaque_hardware_path_available_for(&self, slot: &EncodeSlot) -> bool {
        match slot.opaque_video {
            OpaqueVideo::Vp8 => false,
            OpaqueVideo::Av1 => self.av1_nvenc_ok_for_slot(slot),
            OpaqueVideo::Vp9 => self.hw.qsv.is_some(),
            OpaqueVideo::Hevc | OpaqueVideo::H264 => !self.hw.nvenc.is_empty(),
        }
    }

    /// Prefs HW flag and a usable opaque HW path for this slot.
    fn effective_use_hardware_for(&self, slot: &EncodeSlot) -> bool {
        slot.use_hardware && self.opaque_hardware_path_available_for(slot)
    }

    /// Hover for Use hardware: default help, or why the box is greyed (preference kept).
    fn hardware_checkbox_hover_for(&self, slot: &EncodeSlot) -> String {
        if self.opaque_hardware_path_available_for(slot) {
            return "On: NVENC for HEVC/H.264/AV1; Intel QSV for opaque VP9; QSV+libvpx hybrid for α VP9/.webm (falls back to full libvpx). Off: CPU.".into();
        }
        match slot.opaque_video {
            OpaqueVideo::Av1 if !self.hw.nvenc.iter().any(|d| d.av1) => {
                "AV1 NVENC not available on this machine. Checkbox greyed (preference kept). Encode uses libaom (CPU) for AV1, or pick HEVC/H.264."
                    .into()
            }
            OpaqueVideo::Av1 => {
                "Selected GPU has no AV1 NVENC. Checkbox greyed (preference kept). Pick Any/another GPU or HEVC/H.264; AV1 encode uses libaom while greyed."
                    .into()
            }
            OpaqueVideo::Vp9 => {
                "Intel QSV VP9 not available. Checkbox greyed (preference kept). Encode uses libvpx (CPU) for VP9, or pick HEVC/H.264/AV1."
                    .into()
            }
            OpaqueVideo::Vp8 => {
                "Opaque VP8 has no HW path. Checkbox greyed (preference kept)."
                    .into()
            }
            OpaqueVideo::Hevc | OpaqueVideo::H264 => {
                "No NVENC GPU detected. Checkbox greyed (preference kept). Encode uses CPU."
                    .into()
            }
        }
    }

    /// Use hardware checkbox for one slot; muted when that slot has no HW path here.
    fn ui_use_hardware_checkbox(
        &mut self,
        ui: &mut egui::Ui,
        batch: TestBatch,
        persist: impl FnOnce(&mut Self),
    ) {
        let hw_ok = self.opaque_hardware_path_available_for(self.prefs.slot(batch));
        let hover = self.hardware_checkbox_hover_for(self.prefs.slot(batch));
        let resp = ui
            .scope(|ui| {
                if !hw_ok {
                    ui.multiply_opacity(0.45);
                }
                ui.checkbox(&mut self.prefs.slot_mut(batch).use_hardware, "Use hardware")
            })
            .inner;
        let changed = resp.changed();
        if !hw_ok && (resp.clicked() || resp.secondary_clicked()) {
            resp.show_tooltip_text(&hover);
        }
        resp.on_hover_text(hover);
        if changed {
            persist(self);
        }
    }

    /// Amber line when any input’s chroma/bit-depth can’t be matched for a slot.
    fn chroma_convert_warning_for(&self, batch: TestBatch) -> Option<String> {
        let slot = self.prefs.slot(batch);
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
            if let Some(msg) = pix_fmt_convert_warning(
                &summary.pix_fmt,
                summary.has_alpha,
                slot.use_hardware,
                slot.opaque_video,
            ) {
                return Some(msg);
            }
        }
        None
    }

    /// Amber line when any input would need audio re-encode for a slot’s containers.
    fn audio_recode_warning_for(&self, batch: TestBatch) -> Option<String> {
        let slot = self.prefs.slot(batch);
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
            let cont = slot.container_for(summary.has_alpha);
            let src_webm = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("webm"))
                || summary.container.eq_ignore_ascii_case("webm");
            let raw = summary.audio_codec.to_ascii_lowercase();
            let raw = if raw == "—" { String::new() } else { raw };

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

    /// Apply the maxrate draft for `batch` into prefs (empty → auto; invalid → keep previous).
    fn commit_maxrate_override_edit(&mut self, batch: TestBatch) {
        let idx = Self::batch_edit_idx(batch);
        let trimmed = self.maxrate_override_edit[idx].trim().to_string();
        let next = if trimmed.is_empty() {
            None
        } else {
            trimmed.parse::<u32>().ok().filter(|&k| k > 0)
        };
        if next.is_none() && !trimmed.is_empty() {
            self.maxrate_override_edit[idx] = self
                .prefs
                .slot(batch)
                .maxrate_override_k
                .map(|k| k.to_string())
                .unwrap_or_default();
            return;
        }
        if next != self.prefs.slot(batch).maxrate_override_k {
            self.prefs.slot_mut(batch).maxrate_override_k = next;
            self.maxrate_override_edit[idx] = next.map(|k| k.to_string()).unwrap_or_default();
            self.save_targets();
        }
    }

    /// Optional maxrate kbps next to Use hardware; empty = automatic ceiling.
    fn ui_maxrate_override(&mut self, ui: &mut egui::Ui, batch: TestBatch) {
        let idx = Self::batch_edit_idx(batch);
        ui.label(egui::RichText::new("Maxrate").weak().small());
        let edit = ui
            .add(
                egui::TextEdit::singleline(&mut self.maxrate_override_edit[idx])
                    .desired_width(64.0)
                    .hint_text("auto")
                    .font(egui::TextStyle::Monospace)
                    .id_salt(format!("maxrate_{idx}")),
            )
            .on_hover_text(
                "Override automatic encode maxrate (kbps). Leave empty for auto from source avg/peak.",
            );
        if edit.lost_focus() || (edit.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
        {
            self.commit_maxrate_override_edit(batch);
        }
        ui.label(egui::RichText::new("kbps").weak().small());
    }

    /// Apply the CQ draft for `batch` into prefs (empty → default 0; invalid → keep previous).
    fn commit_cq_override_edit(&mut self, batch: TestBatch) {
        let idx = Self::batch_edit_idx(batch);
        let trimmed = self.cq_override_edit[idx].trim().to_string();
        let next = if trimmed.is_empty() {
            None
        } else {
            trimmed.parse::<u32>().ok().filter(|&q| q <= 63)
        };
        if next.is_none() && !trimmed.is_empty() {
            self.cq_override_edit[idx] = self
                .prefs
                .slot(batch)
                .cq_override
                .map(|q| q.to_string())
                .unwrap_or_default();
            return;
        }
        // Persist Some(0) as None so the file stays clean for the default.
        let next = next.filter(|&q| q > 0);
        if next != self.prefs.slot(batch).cq_override {
            self.prefs.slot_mut(batch).cq_override = next;
            self.cq_override_edit[idx] = next.map(|q| q.to_string()).unwrap_or_default();
            self.save_targets();
        }
    }

    /// Codec/container chips + HW/maxrate for one encode slot.
    fn ui_slot_targets(&mut self, ui: &mut egui::Ui, batch: TestBatch, heading: &str) {
        ui.label(egui::RichText::new(heading).strong().small());
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Opaque").weak().small());
            let v = self.prefs.slot(batch).opaque_video;
            if select_codec_chip(ui, v.label(), true, true) {
                let next = v.next();
                self.prefs.slot_mut(batch).opaque_video = next;
                let cur = self.prefs.slot(batch).opaque_container;
                self.prefs.slot_mut(batch).opaque_container =
                    gigamux_core::targets::clamp_opaque_container(next, cur);
                self.save_targets();
            }
            let c = self.prefs.slot(batch).opaque_container;
            let ov = self.prefs.slot(batch).opaque_video;
            if select_ext_chip(ui, c.label(), true, true) {
                self.prefs.slot_mut(batch).opaque_container = cycle_opaque_container(ov, c);
                self.save_targets();
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("α").weak().small());
            let v = self.prefs.slot(batch).alpha_video;
            if select_codec_chip(ui, v.label(), true, true) {
                let next = v.next();
                self.prefs.slot_mut(batch).alpha_video = next;
                let cur = self.prefs.slot(batch).alpha_container;
                self.prefs.slot_mut(batch).alpha_container =
                    gigamux_core::targets::clamp_alpha_container(next, cur);
                self.save_targets();
            }
            let c = self.prefs.slot(batch).alpha_container;
            let av = self.prefs.slot(batch).alpha_video;
            if select_ext_chip(ui, c.label(), true, true) {
                self.prefs.slot_mut(batch).alpha_container = cycle_alpha_container(av, c);
                self.save_targets();
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            self.ui_use_hardware_checkbox(ui, batch, |s| s.save_targets());
            self.ui_maxrate_override(ui, batch);
            // CQ stays prefs-only (advanced); not exposed in the shell UI.
        });
        self.ui_slot_gpu_picker(ui, batch);
        if let Some(warn) = self.chroma_convert_warning_for(batch) {
            ui.colored_label(egui::Color32::from_rgb(220, 160, 60), warn);
        }
        if let Some(warn) = self.audio_recode_warning_for(batch) {
            ui.colored_label(egui::Color32::from_rgb(220, 160, 60), warn);
        }
    }

    /// Device/gen under Use hardware (same place on QSV-only machines as on NVENC).
    fn ui_slot_gpu_picker(&mut self, ui: &mut egui::Ui, batch: TestBatch) {
        let slot = self.prefs.slot(batch).clone();
        if !slot.use_hardware {
            return;
        }
        match slot.opaque_video {
            OpaqueVideo::Vp9 => {
                if let Some(q) = &self.hw.qsv {
                    ui.label(egui::RichText::new(q.label.clone()).weak().small());
                }
            }
            OpaqueVideo::Hevc | OpaqueVideo::H264 | OpaqueVideo::Av1 => {
                if self.hw.nvenc.is_empty() {
                    // QSV-only machine: still show the probe line under Use hardware.
                    if let Some(q) = &self.hw.qsv {
                        ui.label(
                            egui::RichText::new(format!("{} — Opaque VP9 uses this", q.label))
                                .weak()
                                .small(),
                        );
                    }
                    return;
                }
                if self.hw.nvenc.len() == 1 {
                    ui.label(
                        egui::RichText::new(self.hw.nvenc[0].ui_label())
                            .weak()
                            .small(),
                    );
                    return;
                }
                // Multi-GPU: combo so each test target can pick a different gen/device.
                let mut selected = match slot.gpu {
                    GpuChoice::Any => None,
                    GpuChoice::Index(i) => Some(i),
                };
                let id = format!("slot_gpu_{}", batch.index());
                let preview = match slot.gpu {
                    GpuChoice::Any => self
                        .hw
                        .nvenc
                        .first()
                        .map(|d| format!("Any ({})", d.ui_label()))
                        .unwrap_or_else(|| "Any".into()),
                    GpuChoice::Index(i) => self
                        .hw
                        .nvenc
                        .iter()
                        .find(|d| d.index == i)
                        .map(|d| d.ui_label())
                        .unwrap_or_else(|| format!("#{i}")),
                };
                egui::ComboBox::from_id_salt(id)
                    .selected_text(preview)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut selected, None, "Any");
                        for d in &self.hw.nvenc {
                            ui.selectable_value(&mut selected, Some(d.index), d.ui_label());
                        }
                    });
                let next = match selected {
                    None => GpuChoice::Any,
                    Some(i) => GpuChoice::Index(i),
                };
                if next != self.prefs.slot(batch).gpu {
                    self.prefs.slot_mut(batch).gpu = next;
                    self.save_targets();
                }
            }
            OpaqueVideo::Vp8 => {}
        }
    }

    fn ui_test_mode_checkbox(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui
                .checkbox(&mut self.prefs.test_mode, "Test mode")
                .on_hover_text(
                    "Encode each input as test1_*…testN_* with independent targets for size + VMAF comparison. ←/→ = Test1, Shift+←/→ cycles prefer, Shift+↑/↓ cycles siblings.",
                )
                .changed()
            {
                self.prefs.test_slot_count = self.prefs.test_slot_count.clamp(2, 4);
                self.save_targets();
            }
            if self.prefs.test_mode {
                ui.add_space(6.0);
                ui.label("Target Slots");
                let mut count = self.prefs.test_slot_count.clamp(2, 4);
                egui::ComboBox::from_id_salt("test_slot_count")
                    .selected_text(count.to_string())
                    .width(40.0)
                    .show_ui(ui, |ui| {
                        for n in 2u8..=4 {
                            ui.selectable_value(&mut count, n, n.to_string());
                        }
                    });
                if count != self.prefs.test_slot_count {
                    self.prefs.test_slot_count = count;
                    self.save_targets();
                }
            }
        });
    }

    fn ui_output_targets(&mut self, ui: &mut egui::Ui) {
        self.ui_slot_targets(ui, TestBatch::One, "Targets");
    }

    /// Mid-strip: size Δ vs input; VMAF score (vs input) with ↑/↓ vs Test1 quality baseline.
    fn ui_test_compare_strip(&mut self, ui: &mut egui::Ui) {
        let stem = self.active.as_ref().map(|(_, n)| Self::pair_stem_of(n));
        let Some(stem) = stem else {
            ui.label(
                egui::RichText::new("Select a file to compare target slots")
                    .weak()
                    .small(),
            );
            return;
        };
        let active = self.prefs.active_batches();
        let out_names = self.names_for(FileSide::Output);
        let present: Vec<(TestBatch, String)> = active
            .iter()
            .filter_map(|&b| {
                out_names
                    .iter()
                    .find(|n| {
                        let (batch, rest) = strip_test_prefix(n);
                        batch == Some(b) && rest == stem
                    })
                    .cloned()
                    .map(|n| (b, n))
            })
            .collect();
        if present.is_empty() {
            let expect = active
                .iter()
                .map(|b| b.prefix().trim_end_matches('_'))
                .collect::<Vec<_>>()
                .join("/");
            ui.label(
                egui::RichText::new(format!("Waiting for {expect}_ outputs for \"{stem}\""))
                    .weak()
                    .small(),
            );
            return;
        }

        let bytes_for = |side: FileSide, name: &str| -> Option<u64> {
            let key = Self::preview_key(side, name);
            let e = self.preview_cache.get(&key)?;
            if e.file_size > 0 {
                return Some(e.file_size);
            }
            e.summary
                .as_ref()
                .and_then(|r| r.as_ref().ok())
                .map(|s| s.file_size_bytes)
                .filter(|&b| b > 0)
        };

        // Same greens/reds as the center Size row (VMAF Δ uses these too).
        let green = egui::Color32::from_rgb(80, 180, 100);
        let red = egui::Color32::from_rgb(220, 90, 80);
        let input_name = self
            .names_for(FileSide::Input)
            .into_iter()
            .find(|n| Self::file_stem_of(n) == stem.as_str());
        let in_bytes = input_name
            .as_deref()
            .and_then(|n| bytes_for(FileSide::Input, n));

        // Collect VMAF first so every row can Δ against Test1 (quality A/B heuristic).
        let vmafs: Vec<(TestBatch, Option<f64>)> = present
            .iter()
            .map(|(b, n)| (*b, self.live_vmaf_score(FileSide::Output, n)))
            .collect();
        let vmaf_baseline = vmafs
            .iter()
            .find(|(b, v)| *b == TestBatch::One && v.is_some())
            .and_then(|(_, v)| *v);

        egui::Grid::new("test_compare")
            .num_columns(3)
            .spacing([10.0, 3.0])
            .min_col_width(40.0)
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Compare").strong().small());
                ui.label(
                    egui::RichText::new(format!("{stem}  ·  size vs input"))
                        .weak()
                        .small(),
                );
                ui.label(egui::RichText::new("VMAF vs Test1").weak().small());
                ui.end_row();

                // Input baseline size (VMAF is N/A on the source).
                ui.label(egui::RichText::new("Input").weak().small());
                ui.monospace(match in_bytes {
                    Some(n) => format_bytes(n),
                    None => "-".into(),
                });
                ui.label(egui::RichText::new("-").weak().small());
                ui.end_row();

                for ((batch, name), (_, v)) in present.iter().zip(vmafs.iter()) {
                    let b = bytes_for(FileSide::Output, name);
                    ui.label(egui::RichText::new(batch.label()).weak().small());
                    ui.horizontal(|ui| {
                        ui.monospace(match b {
                            Some(n) => format_bytes(n),
                            None => "-".into(),
                        });
                        if let (Some(a), Some(c)) = (in_bytes, b) {
                            if let Some(d) = size_delta(a, c) {
                                Self::ui_size_delta_labels(ui, d, false);
                            }
                        }
                    });
                    // Absolute VMAF is vs the input; ↑/↓ vs Test1 is the slot A/B heuristic.
                    ui.horizontal(|ui| {
                        ui.monospace(match v {
                            Some(score) => format!("{score:.2}"),
                            None => "-".into(),
                        });
                        if *batch != TestBatch::One {
                            if let (Some(base), Some(score)) = (vmaf_baseline, *v) {
                                let d = score - base;
                                if d.abs() >= 1e-4 {
                                    if d > 0.0 {
                                        ui.colored_label(green, format!("↑{d:.2}"));
                                    } else {
                                        ui.colored_label(red, format!("↓{:.2}", -d));
                                    }
                                }
                            }
                        }
                    });
                    ui.end_row();
                }
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
                // Test mode sits on the output path row, immediately after Change….
                if !is_input {
                    self.ui_test_mode_checkbox(ui);
                }
            });
            if !prefs::dir_ok(dir) {
                // Targets above the empty-folder CTA (non-test only; test needs a real dir for lists).
                if !is_input && !self.prefs.test_mode {
                    self.ui_output_targets(ui);
                    ui.separator();
                }
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

            // Test mode: compare strip + collapsing panels; list height capped so other slot headers stay reachable.
            if !is_input && self.prefs.test_mode {
                let batches: Vec<TestBatch> = self.prefs.active_batches().to_vec();
                let multi = batches.len() > 2;
                self.ui_test_compare_strip(ui);
                ui.separator();
                let n = batches.len().max(1) as f32;
                let header_reserve = 24.0 * n;
                let open_n = batches
                    .iter()
                    .filter(|b| {
                        egui::collapsing_header::CollapsingState::load_with_default_open(
                            ui.ctx(),
                            Self::out_test_slot_id(**b),
                            !multi || b.index() == 0,
                        )
                        .is_open()
                    })
                    .count()
                    .max(1) as f32;
                // Cap each open body so later slot headers are not pushed fully off-screen.
                let list_max =
                    ((ui.available_height() - header_reserve) / open_n - 90.0).clamp(96.0, 280.0);
                egui::ScrollArea::vertical()
                    .id_salt("out_test_slots_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (i, batch) in batches.iter().enumerate() {
                            let files_b = Self::output_files_for_batch(files, *batch);
                            let salt = format!("out_list_t{}", batch.index() + 1);
                            let default_open = !multi || i == 0;
                            let id = Self::out_test_slot_id(*batch);
                            let state = egui::collapsing_header::CollapsingState::load_with_default_open(
                                ui.ctx(),
                                id,
                                default_open,
                            );
                            let (toggle, header_ir, _body) = state
                                .show_header(ui, |ui| {
                                    ui.strong(batch.label())
                                        .on_hover_text("Shift+click caret: expand/collapse all target slots");
                                })
                                .body(|ui| {
                                    self.ui_slot_targets(ui, *batch, batch.label());
                                    ui.separator();
                                    // Constrain the inner file ScrollArea so sibling headers remain visible.
                                    ui.allocate_ui(
                                        egui::vec2(ui.available_width(), list_max),
                                        |ui| {
                                            self.ui_file_scroll_list(
                                                ui, side, false, &files_b, &salt,
                                            );
                                        },
                                    );
                                });
                            // Shift+caret: apply this slot's new open state to every active test slot.
                            if toggle.clicked() && ui.input(|i| i.modifiers.shift) {
                                let open = egui::collapsing_header::CollapsingState::load(
                                    ui.ctx(),
                                    id,
                                )
                                .map(|s| s.is_open())
                                .unwrap_or(true);
                                for b in &batches {
                                    set_collapsing_open(
                                        ui.ctx(),
                                        Self::out_test_slot_id(*b),
                                        open,
                                    );
                                }
                                ui.ctx().request_repaint();
                            }
                            // Keyboard jump opens a collapsed slot: bring its header into the outer scroll.
                            if self.focus_out_test_slot == Some(*batch) {
                                header_ir.response.scroll_to_me(Some(egui::Align::TOP));
                                self.focus_out_test_slot = None;
                            }
                        }
                    });
                return;
            }

            if !is_input {
                self.ui_output_targets(ui);
            }
            ui.separator();
            let salt = if is_input { "in_list" } else { "out_list" };
            self.ui_file_scroll_list(ui, side, is_input, files, salt);
        });
    }

    /// Scrollable file rows for one folder list (or one test-batch half).
    fn ui_file_scroll_list(
        &mut self,
        ui: &mut egui::Ui,
        side: FileSide,
        is_input: bool,
        files: &[PathBuf],
        id_salt: &str,
    ) {
        // Wheel scrolls; primary-drag is marquee (ScrollSource drag defaults to touch-only).
        egui::ScrollArea::vertical()
            .id_salt(id_salt)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut row_rects: Vec<(String, egui::Rect)> = Vec::new();
                // Defer row clicks until after marquee handling (drag wins over click).
                let mut pending_click: Option<(String, bool, bool)> = None;
                let mut pending_dbl: Option<(String, PathBuf)> = None;

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
                        // Test-mode slot lists already name the batch; hide testN_ there only.
                        // Non-test mode always shows the real basename (prefixes included).
                        let row_label = if self.prefs.test_mode {
                            let (batch, stem) = strip_test_prefix(&name);
                            if batch.is_some() {
                                if ext.is_empty() {
                                    stem
                                } else {
                                    format!("{stem}.{ext}")
                                }
                            } else {
                                name.clone()
                            }
                        } else {
                            name.clone()
                        };
                        // Codec (+ α) + chroma + CBR/VBR from preview cache; muted … while probing.
                        let cache_key = Self::preview_key(side, &name);
                        let tags = self.preview_cache.get(&cache_key).and_then(|e| {
                            if e.pending {
                                return Some((None, false, None, None));
                            }
                            e.summary.as_ref().and_then(|r| r.as_ref().ok()).map(|s| {
                                let rate = if e.bitrates_ready {
                                    rate_mode_chip_tag(
                                        s.avg_video_kbps,
                                        s.peak_video_kbps,
                                        s.duration_s,
                                    )
                                    .map(|t| t.to_string())
                                } else {
                                    None
                                };
                                (
                                    Some(codec_tag(s)),
                                    s.has_alpha,
                                    pix_fmt_chip_tag(&s.pix_fmt),
                                    rate,
                                )
                            })
                        });
                        let renaming = self.rename_edit.as_ref().is_some_and(|e| {
                            e.side == side && e.old_name == name
                        });
                        let mut commit_rename: Option<RenameCommitMode> = None;
                        let row = ui.horizontal(|ui| {
                            let resp = if renaming {
                                let edit = self.rename_edit.as_mut().unwrap();
                                let te = egui::TextEdit::singleline(&mut edit.draft)
                                    .desired_width(ui.available_width().min(280.0).max(120.0))
                                    .font(egui::TextStyle::Monospace)
                                    .id_salt(("rename", side as u8, &edit.old_name));
                                let resp = ui
                                    .add(te)
                                    .on_hover_text(
                                        "Enter: rename (multi → stem (n).ext). Alt+Enter: apply prefix/suffix edit to all selected.",
                                    );
                                if edit.request_focus {
                                    resp.request_focus();
                                    edit.request_focus = false;
                                }
                                // Enter = default (single or numbered multi); Alt+Enter = affix pattern. Skip focus-loss commit while Alt is held — Alt alone can blur the TextEdit before Enter arrives.
                                let (enter, alt) = ui.input(|i| {
                                    (i.key_pressed(egui::Key::Enter), i.modifiers.alt)
                                });
                                if enter {
                                    commit_rename = Some(if alt {
                                        RenameCommitMode::Pattern
                                    } else {
                                        RenameCommitMode::Default
                                    });
                                } else if resp.lost_focus() && !alt {
                                    commit_rename = Some(RenameCommitMode::Default);
                                }
                                resp
                            } else {
                                ui.selectable_label(selected, &row_label)
                                    .on_hover_text(&name)
                            };
                            match &tags {
                                Some((None, _, _, _)) => codec_chip(ui, "…"),
                                Some((Some(tag), has_alpha, chroma, rate)) => {
                                    codec_chip(ui, tag);
                                    if *has_alpha {
                                        alpha_chip(ui);
                                    }
                                    if let Some(c) = chroma {
                                        pix_fmt_chip(ui, c);
                                    }
                                    if let Some(r) = rate {
                                        rate_mode_chip(ui, r);
                                    }
                                }
                                None => {}
                            }
                            if !ext.is_empty() {
                                ext_chip(ui, &format!(".{ext}"), true);
                            }
                            resp
                        });
                        if let Some(mode) = commit_rename {
                            self.commit_rename(mode);
                        }
                        let resp = row.inner;
                        // Full row (name + chips) for marquee hit-testing.
                        row_rects.push((name.clone(), row.response.rect));
                        // Keyboard nav asks once; snap only if the row is off-screen.
                        if self.scroll_sel_into_view
                            && self.active.as_ref().is_some_and(|(s, n)| {
                                *s == side && n == &name
                            })
                        {
                            let clip = ui.clip_rect();
                            let row = resp.rect;
                            let on_screen = clip.y_range().contains(row.top())
                                && clip.y_range().contains(row.bottom());
                            if !on_screen {
                                resp.scroll_to_me_animation(
                                    Some(egui::Align::Center),
                                    egui::style::ScrollAnimation::none(),
                                );
                            }
                            self.scroll_sel_into_view = false;
                        }
                        if resp.clicked() && !renaming {
                            let (shift, ctrl) = ui.ctx().input(|i| {
                                (i.modifiers.shift, i.modifiers.command)
                            });
                            pending_click = Some((name.clone(), shift, ctrl));
                        }
                        if resp.double_clicked() {
                            pending_dbl = Some((name.clone(), path.clone()));
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
                            if is_input {
                                let farm_ok = !self.prefs.flamenco_manager_url.trim().is_empty();
                                let farm_sel_n = if self.is_selected(FileSide::Input, &name) {
                                    self.selected.len().max(1)
                                } else {
                                    1
                                };
                                let farm_label = if farm_sel_n > 1 {
                                    format!("Send {farm_sel_n} to Flamenco")
                                } else {
                                    "Send to Flamenco".into()
                                };
                                if ui
                                    .add_enabled(farm_ok, egui::Button::new(farm_label))
                                    .on_hover_text(
                                        "Submit selected input(s) to Flamenco Manager (one job each via GigaMux.js)",
                                    )
                                    .on_disabled_hover_text(
                                        "Set Flamenco Manager URL in Settings → Files",
                                    )
                                    .clicked()
                                {
                                    // Keep multi-select: only collapse when the row was not already selected.
                                    if !self.is_selected(FileSide::Input, &name) {
                                        self.select_file(FileSide::Input, name.clone());
                                    }
                                    self.open_flamenco_submit(None);
                                    ui.close();
                                }
                            }
                            {
                                let vmaf_ok = self.vmaf_available;
                                let disabled_hint = if !vmaf_ok {
                                    "ffmpeg has no libvmaf filter — install a full build (e.g. gyan.dev full)"
                                } else {
                                    "Wait for the current job to finish"
                                };
                                if ui
                                    .add_enabled(
                                        !self.busy && vmaf_ok,
                                        egui::Button::new("VMAF this file"),
                                    )
                                    .on_hover_text(
                                        "libvmaf: this file’s input vs matching output (same stem)",
                                    )
                                    .on_disabled_hover_text(disabled_hint)
                                    .clicked()
                                {
                                    self.select_file(side, name.clone());
                                    self.start_vmaf(Some(name.clone()));
                                    ui.close();
                                }
                            }
                            if ui.button("Open in player").clicked() {
                                self.select_file(side, name.clone());
                                if let Err(e) = open_path_in_os(path) {
                                    self.alert_error("Open", format!("open failed: {e}"));
                                }
                                ui.close();
                            }
                            if ui
                                .button("Reveal in file browser")
                                .on_hover_text("Select this file in Explorer / Finder / file manager")
                                .clicked()
                            {
                                self.select_file(side, name.clone());
                                if let Err(e) = reveal_path_in_os(path) {
                                    self.alert_error("Reveal", format!("reveal failed: {e}"));
                                }
                                ui.close();
                            }
                            let rename_label = if self.is_selected(side, &name)
                                && self.selected.len() > 1
                            {
                                format!("Rename {} selected", self.selected.len())
                            } else {
                                "Rename".into()
                            };
                            if ui
                                .add_enabled(!self.busy, egui::Button::new(rename_label))
                                .on_hover_text(
                                    "F2 — Enter renames (multi → stem (n).ext); Alt+Enter applies the edit to all selected",
                                )
                                .clicked()
                            {
                                if !self.is_selected(side, &name) {
                                    self.select_file(side, name.clone());
                                } else {
                                    self.set_primary(side, name.clone());
                                }
                                self.begin_rename_active();
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
                // Trailing blank absorbs presses for marquee / clear.
                let fill = ui.available_size().max(egui::vec2(1.0, 24.0));
                let blank = ui.allocate_response(fill, egui::Sense::click_and_drag());

                let list_rect = ui.min_rect().union(blank.rect);
                let (pressed, down, released, additive, pointer) = ui.ctx().input(|i| {
                    (
                        i.pointer.primary_pressed(),
                        i.pointer.primary_down(),
                        i.pointer.primary_released(),
                        i.modifiers.command,
                        i.pointer.interact_pos(),
                    )
                });

                // Begin marquee on primary press inside this list only.
                if pressed {
                    if let Some(pos) = pointer.filter(|p| list_rect.contains(*p)) {
                        let on_row = row_rects.iter().any(|(_, r)| r.contains(pos));
                        let baseline = if additive {
                            self.selected.clone()
                        } else {
                            HashSet::new()
                        };
                        self.box_select = Some(BoxSelect {
                            side,
                            list_id: id_salt.to_string(),
                            origin: pos,
                            current: pos,
                            additive,
                            baseline,
                            active: false,
                            started_on_row: on_row,
                        });
                    }
                }

                // Only the list that started the drag may update/finish it (Test1/Test2 share Output).
                let owns_marquee = self
                    .box_select
                    .as_ref()
                    .is_some_and(|b| b.side == side && b.list_id == id_salt);

                let mut marquee_consumed = false;
                if owns_marquee {
                    if down {
                        if let Some(pos) = pointer {
                            if let Some(box_sel) = self.box_select.as_mut() {
                                box_sel.current = pos;
                                if !box_sel.active
                                    && (box_sel.current - box_sel.origin).length()
                                        >= BOX_SELECT_SLOP
                                {
                                    box_sel.active = true;
                                }
                            }
                        }
                    }
                    let active = self.box_select.as_ref().is_some_and(|b| b.active);
                    if active {
                        marquee_consumed = true;
                        let (origin, current, additive, baseline) = {
                            let b = self.box_select.as_ref().unwrap();
                            (b.origin, b.current, b.additive, b.baseline.clone())
                        };
                        let rect =
                            egui::Rect::from_two_pos(origin, current).intersect(list_rect);
                        let fill_c = egui::Color32::from_rgba_unmultiplied(90, 160, 230, 45);
                        let stroke =
                            egui::Stroke::new(1.0, egui::Color32::from_rgb(90, 160, 230));
                        ui.painter().rect_filled(rect, 0.0, fill_c);
                        ui.painter().rect_stroke(
                            rect,
                            0.0,
                            stroke,
                            egui::StrokeKind::Outside,
                        );

                        let mut hits: HashSet<String> = if additive {
                            baseline.clone()
                        } else {
                            HashSet::new()
                        };
                        for (name, row) in &row_rects {
                            if rect.intersects(*row) {
                                hits.insert(name.clone());
                            }
                        }
                        let anchor = baseline.iter().next().cloned().or_else(|| {
                            row_rects
                                .iter()
                                .find(|(_, r)| rect.intersects(*r))
                                .map(|(n, _)| n.clone())
                        });
                        self.select_set(side, hits, anchor);
                    }

                    if released {
                        if let Some(box_sel) = self.box_select.take() {
                            if box_sel.active {
                                marquee_consumed = true;
                            } else if !box_sel.started_on_row {
                                // Click empty space without dragging — clear (unless Ctrl).
                                if !box_sel.additive {
                                    self.clear_active();
                                }
                                marquee_consumed = true;
                            }
                        }
                    }
                }

                if !marquee_consumed {
                    if let Some((name, path)) = pending_dbl {
                        self.select_file(side, name);
                        if let Err(e) = open_path_in_os(&path) {
                            self.alert_error("Open", format!("open failed: {e}"));
                        }
                    } else if let Some((name, shift, ctrl)) = pending_click {
                        if shift {
                            self.select_range(side, name);
                        } else if ctrl {
                            self.select_toggle(side, name);
                        } else {
                            self.select_file(side, name);
                        }
                    }
                }

                // PgUp/PgDn step ≈ one viewport of rows (overlap by one).
                if self.active.as_ref().is_some_and(|(s, _)| *s == side) {
                    let clip_h = ui.clip_rect().height();
                    let row_h = row_rects
                        .first()
                        .map(|(_, r)| r.height())
                        .unwrap_or(22.0)
                        .max(1.0);
                    let visible = (clip_h / row_h).floor() as usize;
                    self.folder_page_rows = visible.saturating_sub(1).max(1);
                }
            });
    }


    fn ui_center(&mut self, ui: &mut egui::Ui) {
        // Bottom-up: action stack hugs a fixed gap above the log; preview fills whatever remains.
        // (Central panel already ends at the log / progress strip, so this tracks log height for free.)
        const GAP_ABOVE_LOG: f32 = 8.0;
        let full = ui.available_size();
        ui.allocate_ui_with_layout(
            full,
            egui::Layout::bottom_up(egui::Align::Center),
            |ui| {
                ui.add_space(GAP_ABOVE_LOG);
                // First widgets sit nearest the log (bottom_up draws upward).
                if self.busy && self.encode_progress.is_none() {
                    let label = match self.job {
                        Some(JobKind::Encode) => "Encoding…",
                        Some(JobKind::Check) => "Checking…",
                        Some(JobKind::Vmaf) => "VMAF…",
                        None => "Busy…",
                    };
                    ui.label(label);
                    ui.add_space(6.0);
                }
                {
                    let vmaf_ok = self.vmaf_available;
                    let disabled_hint = if !vmaf_ok {
                        "ffmpeg has no libvmaf filter — install a full build (e.g. gyan.dev full)"
                    } else {
                        "Wait for the current job to finish"
                    };
                    let vmaf = ui
                        .add_enabled(
                            !self.busy && vmaf_ok,
                            egui::Button::new("VMAF").min_size(egui::vec2(120.0, 28.0)),
                        )
                        .on_hover_text(
                            "libvmaf all input→output pairs. Right-click a file for one pair. Esc cancels.",
                        )
                        .on_disabled_hover_text(disabled_hint);
                    if vmaf.clicked() {
                        self.start_vmaf(None);
                    }
                }
                ui.add_space(6.0);
                let check = ui.add_enabled(
                    !self.busy,
                    egui::Button::new("Check").min_size(egui::vec2(120.0, 28.0)),
                );
                if check.clicked() {
                    self.start_check();
                }
                ui.add_space(6.0);
                {
                    let farm_ok = !self.prefs.flamenco_manager_url.trim().is_empty();
                    let farm = ui
                        .add_enabled(
                            farm_ok,
                            egui::Button::new("Flamenco").min_size(egui::vec2(120.0, 28.0)),
                        )
                        .on_hover_text(
                            "Open Flamenco sender for every selected input. Use Settings → Files to Find Manager, Deploy JS, Ensure tags.",
                        )
                        .on_disabled_hover_text(
                            "Set Flamenco Manager URL (or Find) in Settings → Files",
                        );
                    if farm.clicked() {
                        self.open_flamenco_submit(None);
                    }
                }
                ui.add_space(6.0);
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
                ui.add_space(10.0);
                if ui
                    .add_sized(
                        [40.0, 40.0],
                        egui::Button::new(egui::RichText::new("\u{2699}").size(22.0)),
                    )
                    .on_hover_text("Settings")
                    .clicked()
                {
                    self.show_settings = true;
                    self.settings_raise = true;
                }
                // Above the cog (bottom_up: later = higher): Shutter-style encode/VMAF pause.
                ui.add_space(6.0);
                {
                    let was = self.prefs.work_during_inactivity;
                    let resp = ui
                        .checkbox(
                            &mut self.prefs.work_during_inactivity,
                            "Work during inactivity",
                        )
                        .on_hover_text(
                            "Pause encode/VMAF ffmpeg when you use the mouse or keyboard; resume after 1 s idle.",
                        );
                    if resp.changed() {
                        let _ = prefs::save(&self.prefs);
                        if was && !self.prefs.work_during_inactivity {
                            self.resume_ffmpeg_if_suspended();
                        }
                    }
                }
                ui.add_space(8.0);

                // Remaining height is the preview band (above the action stack).
                let preview_h = ui.available_height().max(48.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), preview_h),
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
            },
        );
    }

    /// Dual encode/VMAF bars sit in their own bottom panel (above the log), so the log strip never clips them.
    fn ui_progress_panel(&mut self, ui: &mut egui::Ui) {
        let Some(prog) = self.encode_progress.as_ref() else {
            return;
        };
        let is_vmaf = matches!(self.job, Some(JobKind::Vmaf));
        let file_caption = if prog.file_name.is_empty() {
            if is_vmaf {
                "Current pair".into()
            } else {
                "Current".into()
            }
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
        let batch_caption = if is_vmaf {
            format!(
                "All pairs ({} file{})",
                prog.total_files,
                if prog.total_files == 1 { "" } else { "s" }
            )
        } else {
            format!(
                "All tasks ({} file{})",
                prog.total_files,
                if prog.total_files == 1 { "" } else { "s" }
            )
        };

        let paused = self.ffmpeg_suspended;
        egui::Panel::bottom("encode_progress")
            .exact_size(if paused { 94.0 } else { 78.0 })
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    if paused {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 160, 60),
                            "Paused — resumes after 1 s idle",
                        );
                    }
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
        // During encode, prefer the live out-time frame over the static mid-clip thumb.
        let live_tex = self.encode_progress.as_ref().and_then(|p| {
            if p.file_name == name {
                p.live_texture.as_ref()
            } else {
                None
            }
        });
        let tex = live_tex.or(entry.texture.as_ref());
        if let Some(tex) = tex {
            // Fit inside the center column; never spill into side panes.
            let max_w = ui.available_width().max(1.0);
            let tex_size = tex.size_vec2();
            let scale = (max_w / tex_size.x).min(1.0);
            let size = egui::vec2(tex_size.x * scale, tex_size.y * scale);
            ui.add(egui::Image::new((tex.id(), size)).maintain_aspect_ratio(true));
        }
        if let Some(Err(e)) = &entry.thumb {
            // Never paint raw multi‑k‑line ffmpeg dumps into egui (freezes the window).
            let shown = truncate_cli_err(e, 400);
            let truncated = shown.contains("… (stderr truncated)") || e.len() > 400;
            ui.horizontal(|ui| {
                ui.colored_label(
                    egui::Color32::from_rgb(200, 100, 80),
                    format!("thumb: {shown}"),
                );
                if truncated
                    && ui
                        .small_button("Copy")
                        .on_hover_text("Copy full ffmpeg stderr")
                        .clicked()
                {
                    ui.ctx().copy_text(e.clone());
                }
            });
        }
        let bitrates_ready = entry.bitrates_ready;
        // Hide score unless the live pair still exists with a matching score on both sides.
        let vmaf_score = self.live_vmaf_score(side, &name);
        // Output meta: ↓/% green or ↑/% red vs same-stem input when both sizes are known.
        let size_vs_input = if side == FileSide::Output {
            self.counterpart_name(side, &name).and_then(|in_name| {
                let in_key = Self::preview_key(FileSide::Input, &in_name);
                let in_bytes = self.preview_cache.get(&in_key).and_then(|e| {
                    if e.file_size > 0 {
                        Some(e.file_size)
                    } else {
                        e.summary
                            .as_ref()
                            .and_then(|r| r.as_ref().ok())
                            .map(|s| s.file_size_bytes)
                            .filter(|&b| b > 0)
                    }
                })?;
                let out_bytes = entry
                    .summary
                    .as_ref()
                    .and_then(|r| r.as_ref().ok())
                    .map(|s| s.file_size_bytes)
                    .filter(|&b| b > 0)
                    .or_else(|| (entry.file_size > 0).then_some(entry.file_size))?;
                size_delta(in_bytes, out_bytes)
            })
        } else {
            None
        };
        match &entry.summary {
            Some(Ok(s)) => {
                ui.add_space(8.0);
                Self::ui_meta_table(ui, s, bitrates_ready, size_vs_input, vmaf_score);
            }
            Some(Err(e)) => {
                ui.colored_label(egui::Color32::from_rgb(200, 100, 80), e);
            }
            None => {}
        }
    }

    fn ui_meta_table(
        ui: &mut egui::Ui,
        s: &MediaSummary,
        bitrates_ready: bool,
        size_vs_input: Option<SizeDelta>,
        vmaf_score: Option<f64>,
    ) {
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
                    if let Some(c) = pix_fmt_chip_tag(&s.pix_fmt) {
                        pix_fmt_chip(ui, &c);
                    }
                    if bitrates_ready {
                        if let Some(r) = rate_mode_chip_tag(
                            s.avg_video_kbps,
                            s.peak_video_kbps,
                            s.duration_s,
                        ) {
                            rate_mode_chip(ui, r);
                        }
                    }
                    if let Some(p) = &s.video_profile {
                        ui.monospace(format!("({p})"));
                    }
                });
                ui.end_row();
                row(ui, "Container", &s.container.to_ascii_uppercase());
                row(ui, "Res", &res);
                ui.label(egui::RichText::new("Size").weak().small());
                ui.horizontal(|ui| {
                    ui.monospace(format_bytes(s.file_size_bytes));
                    if let Some(d) = size_vs_input {
                        Self::ui_size_delta_labels(ui, d, false);
                    }
                });
                ui.end_row();
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
                row(
                    ui,
                    "VMAF",
                    &match vmaf_score {
                        Some(v) => format!("{v:.4}"),
                        None => "—".into(),
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

    /// Second OS window (Adobe-style left tabs). Immediate viewport so panes can mutate prefs live.
    fn ui_settings(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let vp_id = settings_viewport_id();
        if self.settings_raise {
            ctx.send_viewport_cmd_to(vp_id, egui::ViewportCommand::Focus);
            self.settings_raise = false;
        }
        let mut close = false;
        ctx.show_viewport_immediate(
            vp_id,
            egui::ViewportBuilder::default()
                .with_title("GigaMux Settings")
                .with_inner_size([560.0, 420.0])
                .with_min_inner_size([480.0, 360.0]),
            |ui, _class| {
                // Esc or title-bar X dismisses and persists.
                if ui.input(|i| i.key_pressed(egui::Key::Escape) || i.viewport().close_requested())
                {
                    close = true;
                }
                egui::Panel::bottom("settings_bottom")
                    .resizable(false)
                    .show_separator_line(true)
                    .show(ui, |ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Close").clicked() {
                                close = true;
                            }
                        });
                    });
                egui::Panel::left("settings_tabs")
                    .exact_size(140.0)
                    .resizable(false)
                    .show_separator_line(true)
                    .show(ui, |ui| {
                        ui.add_space(8.0);
                        ui.selectable_value(&mut self.settings_tab, SettingsTab::Encode, "Encode");
                        ui.selectable_value(&mut self.settings_tab, SettingsTab::Files, "Files");
                    });
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.add_space(6.0);
                    match self.settings_tab {
                        SettingsTab::Encode => self.ui_settings_encode(ui),
                        SettingsTab::Files => self.ui_settings_files(ui),
                    }
                });
            },
        );
        if close {
            self.commit_all_slot_edits();
            let _ = prefs::save(&self.prefs);
            self.show_settings = false;
        }
    }

    /// Encode tab: hardware, GPU, maxrate, peak ceiling (active test slots when test mode).
    /// CQ is prefs-only (advanced); not shown here.
    fn ui_settings_encode(&mut self, ui: &mut egui::Ui) {
        if self.prefs.test_mode {
            let batches: Vec<TestBatch> = self.prefs.active_batches().to_vec();
            let multi = batches.len() > 2;
            egui::ScrollArea::vertical()
                .id_salt("settings_test_slots_scroll")
                .auto_shrink([false, false])
                .max_height(ui.available_height() * 0.55)
                .show(ui, |ui| {
                    for (i, batch) in batches.iter().enumerate() {
                        let default_open = !multi || i == 0;
                        let id = egui::Id::unique(("gigamux_settings_test_slot", batch.index() as u8));
                        let state = egui::collapsing_header::CollapsingState::load_with_default_open(
                            ui.ctx(),
                            id,
                            default_open,
                        );
                        let (toggle, _, _) = state
                            .show_header(ui, |ui| {
                                ui.strong(batch.label()).on_hover_text(
                                    "Shift+click caret: expand/collapse all target slots",
                                );
                            })
                            .body(|ui| {
                                self.ui_slot_targets(ui, *batch, batch.label());
                            });
                        if toggle.clicked() && ui.input(|i| i.modifiers.shift) {
                            let open = egui::collapsing_header::CollapsingState::load(ui.ctx(), id)
                                .map(|s| s.is_open())
                                .unwrap_or(true);
                            for b in &batches {
                                set_collapsing_open(
                                    ui.ctx(),
                                    egui::Id::unique((
                                        "gigamux_settings_test_slot",
                                        b.index() as u8,
                                    )),
                                    open,
                                );
                            }
                            ui.ctx().request_repaint();
                        }
                    }
                });
        } else {
            self.ui_slot_targets(ui, TestBatch::One, "Targets");
        }
        ui.add_space(8.0);
        // Detected encoders summary (GPU pick lives on each Targets row when multi-GPU).
        ui.horizontal(|ui| {
            let detected = {
                let mut parts = Vec::new();
                for d in &self.hw.nvenc {
                    parts.push(d.ui_label());
                }
                if let Some(q) = &self.hw.qsv {
                    parts.push(q.label.clone());
                }
                if parts.is_empty() {
                    "none (CPU only)".into()
                } else {
                    parts.join(" · ")
                }
            };
            ui.label(egui::RichText::new(format!("Detected: {detected}")).weak().small());
            if ui
                .small_button("Refresh")
                .on_hover_text("Re-probe NVENC / QSV (not cached to disk)")
                .clicked()
            {
                self.refresh_hw_probe("manual");
                self.vmaf_available = libvmaf_available();
                let _ = prefs::save(&self.prefs);
            }
        });
        if let Some(w) = &self.gpu_warn {
            ui.colored_label(egui::Color32::from_rgb(200, 140, 40), w.clone());
        }
        if ui
            .checkbox(
                &mut self.prefs.write_encoder_metadata,
                "Write encoder metadata",
            )
            .on_hover_text(
                "Stamp GigaMux_encoder / GigaMux_hw (codec + NVENC gen / QSV) into output files.",
            )
            .changed()
        {
            let _ = prefs::save(&self.prefs);
        }
        if ui
            .checkbox(
                &mut self.prefs.use_peak_bitrate,
                "Ceiling from peak rate",
            )
            .on_hover_text(
                "Off (default): use average video bitrate. On: use 1s video peak.",
            )
            .changed()
        {
            let _ = prefs::save(&self.prefs);
        }
    }

    /// Files tab: metadata + delete confirms.
    fn ui_settings_files(&mut self, ui: &mut egui::Ui) {
        if ui
            .checkbox(
                &mut self.prefs.preserve_obs_metadata,
                "Preserve OBS metadata",
            )
            .on_hover_text(
                "Keep creation time / chapters / tags from OBS Hybrid MP4 across re-encode",
            )
            .changed()
        {
            let _ = prefs::save(&self.prefs);
        }
        if ui
            .checkbox(
                &mut self.prefs.write_encoder_metadata,
                "Write encoder metadata",
            )
            .on_hover_text(
                "Stamp GigaMux_encoder / GigaMux_hw into outputs (also on Settings → Encode).",
            )
            .changed()
        {
            let _ = prefs::save(&self.prefs);
        }
        if ui
            .checkbox(
                &mut self.prefs.warn_recycle_delete,
                "Warn before Recycle Bin",
            )
            .on_hover_text(
                "Confirm before moving a file to the Recycle Bin. Uncheck via the dialog’s “Don’t ask again.”",
            )
            .changed()
        {
            let _ = prefs::save(&self.prefs);
        }
        if ui
            .checkbox(
                &mut self.prefs.warn_permanent_delete,
                "Warn before permanent delete",
            )
            .on_hover_text(
                "When Recycle Bin fails, confirm before permanently deleting. Uncheck via the dialog’s “Don’t ask again.”",
            )
            .changed()
        {
            let _ = prefs::save(&self.prefs);
        }
        if ui
            .checkbox(&mut self.prefs.job_done_chime, "Chime when jobs finish")
            .on_hover_text(
                "Play a short system sound when an encode, VMAF, or check batch completes.",
            )
            .changed()
        {
            let _ = prefs::save(&self.prefs);
        }
        {
            let was = self.prefs.auto_vmaf_when_idle;
            let vmaf_ok = self.vmaf_available;
            let resp = ui
                .add_enabled_ui(vmaf_ok, |ui| {
                    ui.checkbox(
                        &mut self.prefs.auto_vmaf_when_idle,
                        "Auto VMAF when idle",
                    )
                })
                .inner
                .on_hover_text(
                    "On startup, after encode, or when enabled while idle: integrity-Check then VMAF unscored intact pairs. Retries every 5 min while work remains, else every 1 h.",
                )
                .on_disabled_hover_text(
                    "ffmpeg has no libvmaf filter — install a full build (e.g. gyan.dev full)",
                );
            if resp.changed() {
                let _ = prefs::save(&self.prefs);
                // Turning on while idle arms an immediate pass over current unscored outputs.
                if self.prefs.auto_vmaf_when_idle && !was && self.history_idle() {
                    self.schedule_auto_vmaf(Duration::ZERO);
                }
                if !self.prefs.auto_vmaf_when_idle {
                    self.clear_auto_vmaf_schedule();
                }
            }
        }
        if ui
            .checkbox(
                &mut self.prefs.check_ignore_orphan_outputs,
                "Check: ignore outputs without inputs",
            )
            .on_hover_text(
                "Legacy pref — orphans are selected in the Check results window via category checkboxes.",
            )
            .changed()
        {
            let _ = prefs::save(&self.prefs);
        }
        ui.add_space(10.0);
        ui.label(egui::RichText::new("Flamenco").strong());
        ui.horizontal(|ui| {
            ui.label("Manager URL");
            if ui
                .add(
                    egui::TextEdit::singleline(&mut self.prefs.flamenco_manager_url)
                        .desired_width(240.0)
                        .hint_text("http://127.0.0.1:8080"),
                )
                .changed()
            {
                let _ = prefs::save(&self.prefs);
            }
            if ui
                .button("Find")
                .on_hover_text(
                    "UPnP/SSDP discovery (same as Flamenco workers). Manager must have autodiscoverable enabled.",
                )
                .clicked()
            {
                match discover_manager(std::time::Duration::from_secs(4)) {
                    Ok(url) => {
                        self.prefs.flamenco_manager_url = url.trim_end_matches('/').to_string();
                        let _ = prefs::save(&self.prefs);
                        self.push_session(format!(
                            "Flamenco discovered Manager {}",
                            self.prefs.flamenco_manager_url
                        ));
                    }
                    Err(e) => self.alert_error("Flamenco", format!("Flamenco discover: {e}")),
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Manager install");
            ui.label(
                egui::RichText::new(if self.prefs.flamenco_scripts_dir.is_empty() {
                    "(not set)".into()
                } else {
                    // Prefer showing Manager root when scripts/ was stored.
                    let p = std::path::Path::new(&self.prefs.flamenco_scripts_dir);
                    if p.file_name().is_some_and(|n| n == "scripts") {
                        p.parent()
                            .map(|x| x.display().to_string())
                            .unwrap_or_else(|| self.prefs.flamenco_scripts_dir.clone())
                    } else {
                        self.prefs.flamenco_scripts_dir.clone()
                    }
                })
                .weak()
                .small(),
            );
        });
        ui.horizontal(|ui| {
            if ui
                .button("Deploy GigaMux…")
                .on_hover_text(
                    "Pick the Manager install folder (must contain flamenco-manager.yaml). Writes scripts/GigaMux.js plus tools/gigamux-check.exe and tools/gigamux-check (Linux). Jobs point check_exe at that shared tools path.",
                )
                .clicked()
            {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("Flamenco Manager install folder")
                    .pick_folder()
                {
                    match deploy_gigamux_manager(&path) {
                        Ok(rep) => {
                            self.prefs.flamenco_scripts_dir = rep
                                .scripts_js
                                .parent()
                                .unwrap_or(rep.manager_dir.as_path())
                                .to_string_lossy()
                                .into_owned();
                            // Default helper path = Deployed tools/ (assumes Manager install is on shared storage).
                            self.prefs.flamenco_check_exe = normalize_check_exe_path(
                                &rep.manager_dir
                                    .join("tools")
                                    .join("gigamux-check")
                                    .to_string_lossy(),
                            );
                            let _ = prefs::save(&self.prefs);
                            self.push_session(format!(
                                "Flamenco deployed {} ; {} ; {} ; check_exe={}",
                                rep.scripts_js.display(),
                                rep.tools_win.display(),
                                rep.tools_linux.display(),
                                self.prefs.flamenco_check_exe
                            ));
                            self.show_alert(
                                "Flamenco deploy",
                                format!(
                                    "Wrote:\n• {}\n• {}\n• {}\n\nWorker check path set to:\n{}\n\nAssumes the Manager install is on shared storage. If not, set Worker check helper to a path workers can open.",
                                    rep.scripts_js.display(),
                                    rep.tools_win.display(),
                                    rep.tools_linux.display(),
                                    self.prefs.flamenco_check_exe
                                ),
                            );
                        }
                        Err(e) => self.alert_error("Flamenco", format!("Flamenco deploy: {e}")),
                    }
                }
            }
            let can_tags = !self.prefs.flamenco_manager_url.trim().is_empty();
            if ui
                .add_enabled(can_tags, egui::Button::new("Ensure tags"))
                .on_hover_text(format!(
                    "Create missing worker tags via REST: {}",
                    REQUIRED_WORKER_TAGS
                        .iter()
                        .map(|(n, _)| *n)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .on_disabled_hover_text("Set Manager URL (or Find) first")
                .clicked()
            {
                let url = self.prefs.flamenco_manager_url.clone();
                match ensure_worker_tags(&url) {
                    Ok(created) if created.is_empty() => {
                        self.push_session("Flamenco tags: already present");
                    }
                    Ok(created) => {
                        self.push_session(format!(
                            "Flamenco tags created: {}",
                            created.join(", ")
                        ));
                    }
                    Err(e) => self.alert_error("Flamenco", format!("Flamenco tags: {e}")),
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Worker check helper");
            if ui
                .add(
                    egui::TextEdit::singleline(&mut self.prefs.flamenco_check_exe)
                        .desired_width(280.0)
                        .hint_text(r"Z:\Flamenco\tools\gigamux-check"),
                )
                .on_hover_text(
                    "Absolute path to gigamux-check that workers can open (shared storage). No .exe suffix. Deploy fills this from Manager tools/ when the Manager install is on shared storage; override if helpers live elsewhere.",
                )
                .changed()
            {
                self.prefs.flamenco_check_exe =
                    normalize_check_exe_path(&self.prefs.flamenco_check_exe);
                let _ = prefs::save(&self.prefs);
            }
            if ui
                .button("Browse…")
                .on_hover_text("Pick gigamux-check or gigamux-check.exe on shared storage.")
                .clicked()
            {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("gigamux-check (worker-reachable)")
                    .add_filter("gigamux-check", &["exe", ""])
                    .pick_file()
                {
                    self.prefs.flamenco_check_exe =
                        normalize_check_exe_path(&path.to_string_lossy());
                    let _ = prefs::save(&self.prefs);
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Worker tag");
            if ui
                .add(
                    egui::TextEdit::singleline(&mut self.prefs.flamenco_worker_tag)
                        .desired_width(180.0)
                        .hint_text("(optional)"),
                )
                .on_hover_text(
                    "Tag name or UUID for submitted jobs. Empty = tagless. Use Ensure tags first for nvenc/qsv/vmaf.",
                )
                .changed()
            {
                let _ = prefs::save(&self.prefs);
            }
        });
        ui.horizontal(|ui| {
            ui.label("Worker ffmpeg");
            if ui
                .add(
                    egui::TextEdit::singleline(&mut self.prefs.flamenco_ffmpeg_exe)
                        .desired_width(280.0)
                        .hint_text("ffmpeg"),
                )
                .on_hover_text(
                    "Absolute path on workers, or leave empty for `ffmpeg` (PATH / Manager tools/).",
                )
                .changed()
            {
                let _ = prefs::save(&self.prefs);
            }
        });
        ui.horizontal(|ui| {
            ui.label("Shared path prefix");
            if ui
                .add(
                    egui::TextEdit::singleline(&mut self.prefs.flamenco_twoway_local)
                        .desired_width(180.0)
                        .hint_text(r"Z:\  or  /mnt/shared"),
                )
                .on_hover_text(
                    "Must match the Manager two-way Windows/Linux value for shared storage (e.g. Z:\\ ↔ /mnt/shared). GigaMux braces argv paths to {var} because Flamenco cannot rewrite inside encode_argvs_json.",
                )
                .changed()
            {
                let _ = prefs::save(&self.prefs);
            }
            ui.label("var");
            if ui
                .add(
                    egui::TextEdit::singleline(&mut self.prefs.flamenco_twoway_var)
                        .desired_width(160.0)
                        .hint_text("shared_storage_path"),
                )
                .on_hover_text("Flamenco two-way variable name (default shared_storage_path).")
                .changed()
            {
                let _ = prefs::save(&self.prefs);
            }
        });
    }

    /// Important-error modal (OK / Esc). Does not expand the log panel.
    fn ui_alert_prompt(&mut self, ctx: &egui::Context) {
        let Some(prompt) = self.pending_alert.clone() else {
            return;
        };
        let mut confirmed_ok = false;
        let mut dismiss = false;
        egui::Window::new(&prompt.title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(&prompt.message);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        confirmed_ok = true;
                        dismiss = true;
                    }
                    if ui.button("Open log").clicked() {
                        self.log_open = true;
                        self.log_show_session = true;
                        dismiss = true;
                    }
                });
            });
        // Esc (here or global handler): dismiss only — do not run after_ok.
        if dismiss || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            let after_ok = self
                .pending_alert
                .take()
                .map(|a| a.after_ok)
                .unwrap_or(AlertAfterOk::None);
            if confirmed_ok {
                match after_ok {
                    AlertAfterOk::None => {}
                    AlertAfterOk::OpenSettingsFiles => {
                        self.show_settings = true;
                        self.settings_tab = SettingsTab::Files;
                        self.settings_raise = true;
                    }
                }
            }
        }
    }

    /// Alt-at-launch: pick which prefs slices / logs to purge.
    fn ui_purge_prefs_prompt(&mut self, ctx: &egui::Context) {
        let Some(mut prompt) = self.pending_purge.clone() else {
            return;
        };
        let mut apply = false;
        let mut cancel = false;
        egui::Window::new("Purge on launch")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("Alt was held at launch. Choose what to reset:");
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Preferences").strong().small());
                ui.checkbox(&mut prompt.folders, "Folders (input / output)");
                ui.checkbox(&mut prompt.targets, "Encode targets (codec / container)");
                ui.checkbox(&mut prompt.hardware, "Hardware / GPU");
                ui.checkbox(&mut prompt.bitrate, "Bitrate ceiling options");
                ui.checkbox(&mut prompt.misc, "OBS metadata + delete warnings");
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Logs").strong().small());
                ui.checkbox(&mut prompt.session_log, "Session log");
                ui.checkbox(&mut prompt.file_logs, "Per-file logs (this session)");
                ui.checkbox(&mut prompt.job_logs, "Job logs (encode + VMAF .log.gz)");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let can = prompt.any_selected();
                    if ui
                        .add_enabled(can, egui::Button::new("Purge selected"))
                        .clicked()
                    {
                        apply = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if apply {
            self.apply_purge(&prompt);
            self.pending_purge = None;
        } else if cancel {
            self.pending_purge = None;
        } else {
            self.pending_purge = Some(prompt);
        }
    }

    /// Lines for the log view currently shown (session vs active file).
    fn visible_log_lines(&self) -> Vec<String> {
        let log_name = self.log_file_name();
        let showing_session = self.log_show_session || log_name.is_none();
        if showing_session {
            self.session_log.clone()
        } else if let Some(name) = log_name {
            self.file_logs.get(&name).cloned().unwrap_or_default()
        } else {
            self.session_log.clone()
        }
    }

    /// Clipboard text for the copy control: file log as-is; session log from latest `—— session start`.
    fn visible_log_text(&self) -> String {
        let log_name = self.log_file_name();
        let showing_session = self.log_show_session || log_name.is_none();
        if showing_session {
            let lines = &self.session_log;
            let start = lines
                .iter()
                .rposition(|l| l.starts_with("—— session start"))
                .unwrap_or(0);
            lines[start..].join("\n")
        } else {
            self.visible_log_lines().join("\n")
        }
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
                        // Two-squares copy control (painted — no emoji font required).
                        if copy_icon_button(ui)
                            .on_hover_text("Copy log to clipboard")
                            .clicked()
                        {
                            let text = self.visible_log_text();
                            ui.ctx().copy_text(text);
                        }
                    });
                });
                if !self.log_open {
                    return;
                }
                ui.separator();
                let lines = self.visible_log_lines();
                let showing_session = self.log_show_session || self.log_file_name().is_none();
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

    /// Flamenco sender scaffold: review files + tag/exe, then Submit posts jobs.
    fn ui_flamenco_submit_prompt(&mut self, ctx: &egui::Context) {
        let Some(mut prompt) = self.pending_flamenco.clone() else {
            return;
        };
        let n = prompt.names.len();
        let manager = self.prefs.flamenco_manager_url.trim().to_string();
        egui::Window::new("Send to Flamenco")
            .collapsible(false)
            .resizable(true)
            .default_size([420.0, 320.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("Manager").strong());
                ui.label(egui::RichText::new(&manager).weak().small());
                ui.add_space(8.0);

                ui.label(egui::RichText::new(format!(
                    "Jobs ({n} file{})",
                    if n == 1 { "" } else { "s" }
                )).strong());
                for name in prompt.names.iter().take(8) {
                    ui.label(egui::RichText::new(name).strong());
                }
                if n > 8 {
                    ui.label(egui::RichText::new(format!("…and {} more", n - 8)).weak());
                }
                ui.add_space(8.0);

                ui.label("Worker tag (optional)");
                ui.add(
                    egui::TextEdit::singleline(&mut prompt.worker_tag)
                        .desired_width(280.0)
                        .hint_text("nvenc / qsv / …"),
                );
                ui.label("ffmpeg on worker (optional)");
                ui.add(
                    egui::TextEdit::singleline(&mut prompt.ffmpeg_exe)
                        .desired_width(280.0)
                        .hint_text("ffmpeg or absolute path"),
                );
                ui.add_space(8.0);

                ui.checkbox(&mut prompt.chunking, "Chunk encode (N workers → concat → mux)");
                if prompt.chunking {
                    ui.horizontal(|ui| {
                        ui.label("Chunks");
                        ui.add(egui::Slider::new(&mut prompt.chunks, 2..=32));
                    });
                    if prompt.estimate_duration_s > 0.0 {
                        // Equal time slices for the label; last chunk absorbs remainder frames.
                        let (_, per) =
                            chunk_time_range(prompt.estimate_duration_s, 0, prompt.chunks)
                                .unwrap_or((0.0, prompt.estimate_duration_s / prompt.chunks as f64));
                        ui.label(
                            egui::RichText::new(format!(
                                "≈ {} per chunk from {} (last may be longer)",
                                format_length(per),
                                prompt.estimate_name
                            ))
                            .weak()
                            .small(),
                        );
                    } else {
                        ui.label(
                            egui::RichText::new("duration unknown for estimate")
                                .weak()
                                .small(),
                        );
                    }
                    ui.checkbox(
                        &mut prompt.only_chunk_software,
                        "Only chunk software (HW opaque stays single)",
                    );
                }

                if let Some(p) = self.pending_flamenco.as_mut() {
                    p.worker_tag = prompt.worker_tag.clone();
                    p.ffmpeg_exe = prompt.ffmpeg_exe.clone();
                    p.chunking = prompt.chunking;
                    p.chunks = prompt.chunks.clamp(2, 32);
                    p.only_chunk_software = prompt.only_chunk_software;
                }

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.pending_flamenco = None;
                    }
                    let submit_label = if n == 1 {
                        "Submit 1 job".into()
                    } else {
                        format!("Submit {n} jobs")
                    };
                    if ui
                        .add(egui::Button::new(submit_label).min_size(egui::vec2(120.0, 28.0)))
                        .clicked()
                    {
                        self.prefs.flamenco_worker_tag = prompt.worker_tag.trim().to_string();
                        self.prefs.flamenco_ffmpeg_exe = prompt.ffmpeg_exe.trim().to_string();
                        self.prefs.flamenco_chunking = prompt.chunking;
                        self.prefs.flamenco_chunks = prompt.chunks.clamp(2, 32);
                        self.prefs.flamenco_only_chunk_software = prompt.only_chunk_software;
                        let _ = prefs::save(&self.prefs);
                        let names = prompt.names;
                        self.pending_flamenco = None;
                        self.submit_flamenco_encode(names);
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

    /// Dedicated Check results viewport: severity tree, selective purge.
    fn ui_check_prompt(&mut self, ctx: &egui::Context) {
        if self.pending_check.is_none() {
            return;
        }
        let vp_id = check_results_viewport_id();
        let mut close = false;
        let mut purge = false;
        let mut open_log = false;
        let out_dir = self.prefs.output_dir.clone();

        ctx.show_viewport_immediate(
            vp_id,
            egui::ViewportBuilder::default()
                .with_title("Check results")
                .with_inner_size([640.0, 480.0])
                .with_min_inner_size([480.0, 320.0]),
            |ui, _class| {
                if ui.input(|i| i.key_pressed(egui::Key::Escape) || i.viewport().close_requested()) {
                    close = true;
                }

                let (issue_n, selected_n) = self
                    .pending_check
                    .as_ref()
                    .map(|w| (w.issues.len(), w.selected.len()))
                    .unwrap_or((0, 0));

                egui::Panel::bottom("check_results_bottom")
                    .resizable(false)
                    .show_separator_line(true)
                    .show(ui, |ui| {
                        ui.add_space(4.0);
                        let delete_label = if selected_n > 0 {
                            format!("Delete outputs of all selected ({selected_n})")
                        } else {
                            "Delete outputs of all selected".into()
                        };
                        let delete_btn = egui::Button::new(
                            egui::RichText::new(delete_label).size(16.0).strong(),
                        )
                        .min_size(egui::vec2(ui.available_width(), 36.0));
                        if ui
                            .add_enabled(selected_n > 0, delete_btn)
                            .on_hover_text("Move checked outputs to the Recycle Bin")
                            .clicked()
                        {
                            purge = true;
                        }
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.button("Close").clicked() {
                                close = true;
                            }
                            if ui.button("Open session log").clicked() {
                                open_log = true;
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(format!("{issue_n} issue(s)"));
                            });
                        });
                        ui.add_space(2.0);
                    });

                egui::CentralPanel::default().show(ui, |ui| {
                    ui.add_space(4.0);
                    ui.label(
                        "Check a severity or category to select its deletable outputs, or toggle rows.",
                    );
                    ui.add_space(6.0);

                    // Build (severity → category → indices) without holding a long borrow.
                    let groups: Vec<(CheckSeverity, Vec<(&'static str, Vec<usize>)>)> = {
                        let Some(w) = self.pending_check.as_ref() else {
                            return;
                        };
                        let sevs = [
                            CheckSeverity::High,
                            CheckSeverity::Medium,
                            CheckSeverity::Low,
                        ];
                        sevs.into_iter()
                            .filter_map(|sev| {
                                let mut by_cat: Vec<(&'static str, Vec<usize>)> = Vec::new();
                                for (idx, issue) in w.issues.iter().enumerate() {
                                    if check_issue_severity(issue) != sev {
                                        continue;
                                    }
                                    let cat = check_issue_category(issue);
                                    if let Some((_, idxs)) =
                                        by_cat.iter_mut().find(|(c, _)| *c == cat)
                                    {
                                        idxs.push(idx);
                                    } else {
                                        by_cat.push((cat, vec![idx]));
                                    }
                                }
                                if by_cat.is_empty() {
                                    None
                                } else {
                                    Some((sev, by_cat))
                                }
                            })
                            .collect()
                    };

                    // Apply a group checkbox: select/clear every deletable index in `idxs`.
                    let apply_group = |app: &mut Self, idxs: &[usize], on: bool| {
                        let Some(w) = app.pending_check.as_mut() else {
                            return;
                        };
                        for &idx in idxs {
                            if issue_has_deletable_output(&out_dir, &w.issues[idx]) {
                                if on {
                                    w.selected.insert(idx);
                                } else {
                                    w.selected.remove(&idx);
                                }
                            }
                        }
                    };

                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for (sev, cats) in &groups {
                                let sev_idxs: Vec<usize> = cats
                                    .iter()
                                    .flat_map(|(_, idxs)| idxs.iter().copied())
                                    .collect();
                                let n = sev_idxs.len();
                                let color = check_severity_color(*sev);
                                let header = format!("{} — {n}", sev.label());

                                // Absolute ids so Shift+caret can sync children without Ui nesting.
                                let mut sev_toggle: Option<bool> = None;
                                let sev_id = egui::Id::unique(("gigamux_check_sev", *sev as u8));
                                let cat_ids: Vec<egui::Id> = cats
                                    .iter()
                                    .map(|(cat, _)| {
                                        egui::Id::unique(("gigamux_check_cat", *sev as u8, *cat))
                                    })
                                    .collect();
                                let (sev_caret, _, _) =
                                    egui::collapsing_header::CollapsingState::load_with_default_open(
                                        ui.ctx(),
                                        sev_id,
                                        *sev != CheckSeverity::Low,
                                    )
                                    .show_header(ui, |ui| {
                                        sev_toggle = check_group_checkbox(
                                            ui,
                                            self.pending_check.as_ref(),
                                            &sev_idxs,
                                            &out_dir,
                                        );
                                        ui.label(
                                            egui::RichText::new(header).color(color).strong(),
                                        );
                                    })
                                    .body(|ui| {
                                        for ((cat, idxs), &cat_id) in cats.iter().zip(cat_ids.iter())
                                        {
                                            let mut cat_toggle: Option<bool> = None;
                                            egui::collapsing_header::CollapsingState::load_with_default_open(
                                                ui.ctx(),
                                                cat_id,
                                                true,
                                            )
                                            .show_header(ui, |ui| {
                                                cat_toggle = check_group_checkbox(
                                                    ui,
                                                    self.pending_check.as_ref(),
                                                    idxs,
                                                    &out_dir,
                                                );
                                                ui.label(format!("{cat} ({})", idxs.len()));
                                            })
                                            .body(|ui| {
                                                for &idx in idxs {
                                                    let (row_label, deletable) = {
                                                        let Some(w) = self.pending_check.as_ref()
                                                        else {
                                                            return;
                                                        };
                                                        let issue = &w.issues[idx];
                                                        let pair = match (
                                                            &issue.input,
                                                            &issue.output,
                                                        ) {
                                                            (Some(i), Some(o)) => {
                                                                format!("{i} → {o}")
                                                            }
                                                            (Some(i), None) => i.clone(),
                                                            (None, Some(o)) => o.clone(),
                                                            (None, None) => "?".into(),
                                                        };
                                                        let label = format!(
                                                            "{pair}: {}",
                                                            issue.message
                                                        );
                                                        let deletable =
                                                            issue_has_deletable_output(
                                                                &out_dir, issue,
                                                            );
                                                        (label, deletable)
                                                    };
                                                    ui.horizontal(|ui| {
                                                        if deletable {
                                                            let mut on = self
                                                                .pending_check
                                                                .as_ref()
                                                                .map(|w| {
                                                                    w.selected.contains(&idx)
                                                                })
                                                                .unwrap_or(false);
                                                            if ui.checkbox(&mut on, "").changed()
                                                            {
                                                                if let Some(w) =
                                                                    self.pending_check.as_mut()
                                                                {
                                                                    if on {
                                                                        w.selected.insert(idx);
                                                                    } else {
                                                                        w.selected.remove(&idx);
                                                                    }
                                                                }
                                                            }
                                                        } else {
                                                            ui.add_space(16.0);
                                                        }
                                                        ui.label(
                                                            egui::RichText::new(row_label)
                                                                .color(color)
                                                                .small(),
                                                        );
                                                    });
                                                }
                                            });
                                            if let Some(on) = cat_toggle {
                                                apply_group(self, idxs, on);
                                            }
                                        }
                                    });
                                // Blender outliner: Shift+caret recursively opens/closes children.
                                if sev_caret.clicked() && ui.input(|i| i.modifiers.shift) {
                                    let open =
                                        egui::collapsing_header::CollapsingState::load(
                                            ui.ctx(),
                                            sev_id,
                                        )
                                        .map(|s| s.is_open())
                                        .unwrap_or(true);
                                    for &cat_id in &cat_ids {
                                        set_collapsing_open(ui.ctx(), cat_id, open);
                                    }
                                    // Children may have drawn with stale open state this frame.
                                    ui.ctx().request_repaint();
                                }
                                if let Some(on) = sev_toggle {
                                    apply_group(self, &sev_idxs, on);
                                }
                            }
                        });
                });
            },
        );

        if purge {
            if let Some(w) = self.pending_check.take() {
                let to_delete: Vec<CheckIssue> = w
                    .selected
                    .into_iter()
                    .filter_map(|idx| w.issues.get(idx).cloned())
                    .collect();
                if !to_delete.is_empty() {
                    let (deleted, failed, trashed) =
                        delete_issue_outputs(&self.prefs.output_dir, &to_delete);
                    self.push_undo_step(UndoStep::Delete(DeleteUndoBatch {
                        side: FileSide::Output,
                        paths: trashed,
                    }));
                    self.push_session(format!(
                        "Recycle Bin: {deleted} output(s); failed {failed}"
                    ));
                    self.refresh_lists();
                }
            }
        } else if open_log {
            self.log_open = true;
            self.log_show_session = true;
            self.pending_check = None;
        } else if close {
            self.pending_check = None;
        }
    }
}

/// Persist open/closed for a collapsing header (used by Shift+caret recursive expand).
fn set_collapsing_open(ctx: &egui::Context, id: egui::Id, open: bool) {
    let mut st =
        egui::collapsing_header::CollapsingState::load_with_default_open(ctx, id, open);
    st.set_open(open);
    st.store(ctx);
}

/// Group checkbox for a severity/category: toggles all deletable indices; indeterminate when partial.
/// Returns `Some(new_on)` when the user clicked; `None` if no deletable rows or unchanged.
fn check_group_checkbox(
    ui: &mut egui::Ui,
    window: Option<&CheckResultsWindow>,
    idxs: &[usize],
    out_dir: &Path,
) -> Option<bool> {
    let Some(w) = window else {
        return None;
    };
    let deletable: Vec<usize> = idxs
        .iter()
        .copied()
        .filter(|&idx| issue_has_deletable_output(out_dir, &w.issues[idx]))
        .collect();
    // Medium (“Missing from output”) has nothing on disk to purge; still show a disabled checkbox so High/Medium/Low headers stay aligned.
    if deletable.is_empty() {
        let mut on = false;
        ui.add_enabled(false, egui::Checkbox::without_text(&mut on))
            .on_hover_text("Nothing to delete in this group");
        return None;
    }
    let n_on = deletable.iter().filter(|i| w.selected.contains(i)).count();
    let mut on = n_on == deletable.len();
    let indeterminate = n_on > 0 && n_on < deletable.len();
    let resp = ui.add(egui::Checkbox::without_text(&mut on).indeterminate(indeterminate));
    if resp.changed() {
        Some(on)
    } else {
        None
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

/// Standard “two overlapping squares” copy control (stroke only — no emoji glyph).
fn copy_icon_button(ui: &mut egui::Ui) -> egui::Response {
    let size = egui::vec2(18.0, 18.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let visuals = ui.style().interact(&response);
    let stroke = egui::Stroke::new(1.2, visuals.fg_stroke.color);
    let painter = ui.painter();
    // Back square (offset up-right) then front square (down-left) — classic copy glyph.
    let back = egui::Rect::from_min_size(
        rect.min + egui::vec2(5.0, 2.5),
        egui::vec2(9.0, 9.0),
    );
    let front = egui::Rect::from_min_size(
        rect.min + egui::vec2(2.5, 5.5),
        egui::vec2(9.0, 9.0),
    );
    if response.hovered() || response.highlighted() {
        painter.rect_filled(rect, 3.0, visuals.weak_bg_fill);
    }
    painter.rect_stroke(back, 1.0, stroke, egui::StrokeKind::Outside);
    painter.rect_filled(front, 1.0, visuals.weak_bg_fill);
    painter.rect_stroke(front, 1.0, stroke, egui::StrokeKind::Outside);
    response
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

/// Chroma / bit-depth chip from ffprobe `pix_fmt` (e.g. `4:2:2`, `4:2:0 10`).
fn pix_fmt_chip(ui: &mut egui::Ui, tag: &str) {
    let (bg, fg) = heat_chip_colors(pix_fmt_heat(tag), ui.visuals().dark_mode);
    tag_chip(ui, tag, bg, fg);
}

/// Detected source rate mode (CBR / VBR) from peak÷avg once peeks are ready.
fn rate_mode_chip(ui: &mut egui::Ui, tag: &str) {
    let (bg, fg) = heat_chip_colors(rate_mode_heat(tag), ui.visuals().dark_mode);
    tag_chip(ui, tag, bg, fg);
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
    } else if lower.starts_with("warn")
        || lower.contains("skip")
        || lower.contains("cancel")
    {
        // WARN (codec mismatch / orphan output) before the generic "missing" → red path.
        egui::Color32::from_rgb(220, 180, 60)
    } else if lower.contains("issue")
        || lower.contains("error")
        || lower.contains("fail")
        || lower.contains("incomplete")
        || lower.contains("missing")
    {
        egui::Color32::from_rgb(230, 90, 90)
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

/// Short system sound when a batch job finishes (encode / VMAF / check).
fn play_job_done_chime() {
    #[cfg(windows)]
    {
        use windows::core::w;
        use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_NODEFAULT};
        // SystemAsterisk alias, async so the UI thread is not blocked.
        let _ = unsafe {
            PlaySoundW(
                w!("SystemAsterisk"),
                None,
                SND_ALIAS | SND_ASYNC | SND_NODEFAULT,
            )
        };
    }
    #[cfg(not(windows))]
    {
        // Terminal bell as a light fallback elsewhere.
        eprint!("\x07");
    }
}

/// Screen cursor → egui viewport coordinates. Used when egui's pointer is dead during OS DnD.
#[cfg(windows)]
fn win32_cursor_egui_pos(ctx: &egui::Context) -> Option<egui::Pos2> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut pt = POINT::default();
    unsafe { GetCursorPos(&mut pt) }.ok()?;
    let ppp = ctx.pixels_per_point();
    let inner = ctx.input(|i| i.viewport().inner_rect)?;
    let screen = egui::pos2(pt.x as f32 / ppp, pt.y as f32 / ppp);
    Some(egui::pos2(screen.x - inner.min.x, screen.y - inner.min.y))
}

#[cfg(not(windows))]
fn win32_cursor_egui_pos(_ctx: &egui::Context) -> Option<egui::Pos2> {
    None
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

/// Reveal a file/folder in the OS file browser (select in Explorer / Finder; open parent on Linux).
fn reveal_path_in_os(path: &Path) -> Result<(), String> {
    let path = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf());
    #[cfg(target_os = "windows")]
    {
        return reveal_path_in_os_windows(&path);
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .args(["-R", &path.to_string_lossy()])
            .spawn()
            .map_err(|e| format!("open -R: {e}"))?;
        return Ok(());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = if path.is_dir() {
            path.clone()
        } else {
            path.parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| path.clone())
        };
        Command::new("xdg-open")
            .arg(&dir)
            .spawn()
            .map_err(|e| format!("xdg-open: {e}"))?;
        return Ok(());
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    {
        let _ = path;
        Err("reveal not supported on this OS".into())
    }
}

/// Windows: Shell `SHOpenFolderAndSelectItems` (explorer `/select,` mishandles spaces and `\\?\`).
#[cfg(target_os = "windows")]
fn reveal_path_in_os_windows(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{ILCreateFromPathW, ILFree, SHOpenFolderAndSelectItems};

    // canonicalize() often yields `\\?\C:\…` / `\\?\UNC\…`, which ILCreateFromPathW rejects.
    let s = path.to_string_lossy();
    let s = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.into_owned()
    };
    let wide: Vec<u16> = std::ffi::OsStr::new(&s)
        .encode_wide()
        .chain(Some(0))
        .collect();

    unsafe {
        // Ignore already-initialized / mode mismatch; egui may have COM up already.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let item = ILCreateFromPathW(PCWSTR(wide.as_ptr()));
        if item.is_null() {
            return Err(format!("reveal: could not resolve {s}"));
        }
        let hr = SHOpenFolderAndSelectItems(item, None, 0);
        ILFree(Some(item));
        hr.map_err(|e| format!("reveal: {e}"))
    }
}

impl eframe::App for GigaMuxApp {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.poll_events(ctx);
        // Keep elapsed / ETA labels ticking while encode bars are visible.
        if self.encode_progress.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        #[cfg(windows)]
        {
            // Green Win10 taskbar fill tracks the same overall batch fraction as the UI bar.
            let frac = self.encode_progress.as_ref().map(|p| p.batch_frac());
            if frac.is_some() {
                taskbar_progress::sync(frame, frac);
                self.taskbar_showing = true;
            } else if self.taskbar_showing {
                taskbar_progress::sync(frame, None);
                self.taskbar_showing = false;
            }
        }
        let _ = frame;
        self.tick_delete_queue(ctx);
        self.tick_history_nav(ctx);
        self.poll_preview_cache(ctx);
        // Pref-armed idle pass: silent Check → filtered VMAF (no purge window).
        // Wake when the next due Instant arrives even if the UI is otherwise idle.
        if self.prefs.auto_vmaf_when_idle && self.auto_vmaf_armed {
            if let Some(due) = self.auto_vmaf_due_at {
                let now = Instant::now();
                if now < due {
                    ctx.request_repaint_after(due.saturating_duration_since(now));
                }
            }
        }
        self.try_start_auto_vmaf();
        self.tick_idle_list_refresh(ctx);
        // Suspend live ffmpeg on user input; resume after 1 s system idle.
        self.tick_ffmpeg_idle_pause();
        self.handle_shortcuts(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.ui_menu(ui);
        // Bottom panels stack upward: log first (edge), then progress above it.
        self.ui_log_panel(ui);
        self.ui_progress_panel(ui);

        egui::CentralPanel::default().show(ui, |ui| {
            let total = ui.available_size();
            let (in_w, center_w, out_w, content_w) = self.column_widths(total.x);
            let scroll = content_w > total.x + 0.5;

            let mut show_cols = |ui: &mut egui::Ui| {
                let in_size = egui::vec2(in_w, total.y);
                let in_resp = ui.allocate_ui_with_layout(
                    in_size,
                    egui::Layout::bottom_up(egui::Align::Min),
                    |ui| {
                        // Size footer stays under the Input list (full side width).
                        ui.add(
                            egui::Label::new(egui::RichText::new(Self::folder_size_footer(
                                self.input_files.len(),
                                self.input_listed_bytes,
                            )).small())
                            .truncate(),
                        );
                        let files = self.input_files.clone();
                        let dir = self.prefs.input_dir.clone();
                        self.ui_folder_pane(ui, "Input", &dir, true, &files);
                        // allocate_ui shrink-wraps; claim the full column so DnD hits empty space too.
                        ui.expand_to_include_rect(ui.max_rect());
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
                let out_size = egui::vec2(out_w, total.y);
                let out_resp = ui.allocate_ui_with_layout(
                    out_size,
                    egui::Layout::bottom_up(egui::Align::Min),
                    |ui| {
                        // Folder total vs Input: same ↓/% green · ↑/% red as center Size.
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::Label::new(egui::RichText::new(Self::folder_size_footer(
                                    self.output_files.len(),
                                    self.output_listed_bytes,
                                )).small())
                                .truncate(),
                            );
                            if let Some(d) =
                                size_delta(self.input_listed_bytes, self.output_listed_bytes)
                            {
                                Self::ui_size_delta_labels(ui, d, true);
                            }
                        });
                        let files = self.output_files.clone();
                        let dir = self.prefs.output_dir.clone();
                        self.ui_folder_pane(ui, "Output", &dir, false, &files);
                        ui.expand_to_include_rect(ui.max_rect());
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

        // After pane rects are set — DnD needs them, and paints a foreground overlay.
        self.handle_folder_dnd(&ctx);

        self.ui_settings(&ctx);
        self.ui_alert_prompt(&ctx);
        self.ui_purge_prefs_prompt(&ctx);
        self.ui_check_prompt(&ctx);
        self.ui_flamenco_submit_prompt(&ctx);
        self.ui_recycle_delete_prompt(&ctx);
        self.ui_permanent_delete_prompt(&ctx);
    }

    fn on_exit(&mut self) {
        let _ = prefs::save(&self.prefs);
        self.save_preview_cache_to_disk();
    }
}
