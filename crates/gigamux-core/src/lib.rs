//! GigaMux core — prefs, GPU probe, encode batch, check (no UI).

pub mod check;
pub mod chunk;
pub mod encode;
pub mod ffmpeg_active;
pub mod flamenco;
pub mod gpu;
pub mod job_log;
pub mod prefs;
pub mod preview_cache;
pub mod probe;
pub mod targets;
pub mod vmaf;
pub mod webm_alpha;

pub use check::{
    check_issue_category, check_issue_severity, deletable_issues, delete_issue_outputs,
    integrity_blocked_outputs, is_codec_mismatch_issue, is_orphan_output_issue,
    issue_has_deletable_output, list_media, permanent_delete_file, restore_trashed_file, run_check,
    trash_file, CheckIssue, CheckReport, CheckSeverity,
};
pub use chunk::{chunk_time_range, estimate_total_frames, parse_fps};
pub use encode::{
    build_farm_chunked_job, build_farm_ffmpeg_argv, run_batch, write_farm_ffconcat, EncodeEvent,
    EncodeOptions, FarmChunkedJob, FarmEncodeRequest, FarmFfmpegCommand,
};
pub use ffmpeg_active::{active_ffmpeg_pid, ActiveFfmpegGuard};
pub use flamenco::{
    deploy_gigamux_js, discover_manager, ensure_worker_tags, fetch_worker_tags,
    infer_twoway_local_prefix, make_gigamux_chunked_job, make_gigamux_encode_job, ping_manager,
    rewrite_flamenco_twoway_path, scripts_dir_from_manager_install, submit_job, FlamencoTwoway,
    SubmittedJob, GIGAMUX_JS, REQUIRED_WORKER_TAGS,
};
pub use job_log::{
    encode_event_line, encode_logs_dir, job_logs_dir, vmaf_event_line, EncodeJobLog, JobLog,
    JobLogKind,
};
pub use gpu::{
    av1_nvenc_available, list_av1_nvenc_devices, list_nvenc_devices, list_nvenc_devices_for,
    nvenc_encoder_works, qsv_vp9_available, GpuChoice, NvencDevice,
};
pub use prefs::{logs_dir, EncodeSlot, Prefs};
pub use probe::{
    codec_heat, codec_tag, container_heat, enrich_summary_bitrates, format_bytes, format_length,
    media_summary, media_summary_fast, pix_fmt_chip_tag, pix_fmt_heat, rate_mode_chip_tag,
    rate_mode_heat, size_delta, thumbnail_rgba, truncate_cli_err, video_color_meta, FrameTiming,
    MediaSummary, RgbaThumb, SizeDelta, VideoColorMeta, THUMB_W,
};
pub use targets::{
    alpha_pair_ok, audio_encode_for, audio_must_recode, cycle_alpha_container,
    cycle_opaque_container, delivery_pix_fmt, nvenc_pix_fmt, opaque_pair_ok, pair_stem,
    qsv_vp9_pix_fmt, pix_fmt_convert_warning, pix_fmt_label, pix_fmt_will_convert, software_pix_fmt,
    strip_test_prefix, AlphaVideo, AudioEncode, OpaqueVideo, OutContainer, OutputTargets,
    TestBatch,
};
pub use vmaf::{libvmaf_available, run_vmaf, VmafEvent, VmafReport, VmafResult};
