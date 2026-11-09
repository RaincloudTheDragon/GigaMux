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
    check_chunk_file, check_issue_category, check_issue_severity, check_output_pair,
    cleanup_farm_intermediates, deletable_issues, delete_issue_outputs, ensure_chunk_file,
    ensure_out_file, integrity_blocked_outputs, is_codec_mismatch_issue, is_orphan_output_issue,
    issue_has_deletable_output, list_media, permanent_delete_file, restore_trashed_file, run_check,
    trash_file, CheckIssue, CheckReport, CheckSeverity,
};
pub use chunk::{chunk_time_range, estimate_total_frames, parse_fps};
pub use encode::{
    build_farm_chunked_job, build_farm_ffmpeg_argv, farm_integrity_for_chunked,
    farm_integrity_for_single, run_batch, write_farm_ffconcat, EncodeEvent, EncodeOptions,
    FarmChunkedJob, FarmEncodeRequest, FarmFfmpegCommand,
};
pub use ffmpeg_active::{active_ffmpeg_pid, kill_process_tree, ActiveFfmpegGuard};
pub use flamenco::{
    cancel_exec_mode_for_manager_version, check_exe_from_deploy_scripts_dir, deploy_gigamux_js,
    deploy_gigamux_manager, discover_manager, ensure_worker_tags, fetch_worker_tags,
    flamenco_script_hint, infer_twoway_local_prefix, local_gigamux_js_present,
    make_gigamux_chunked_job, make_gigamux_encode_job, normalize_check_exe_path, ping_manager,
    resolve_flamenco_check_exe, rewrite_flamenco_twoway_path, scripts_dir_from_manager_install,
    submit_job, CancelExecMode, DeployReport, FarmChunkCheck, FarmCleanup, FarmFinalCheck,
    FlamencoIntegrity, FlamencoTwoway, SubmittedJob, DEFAULT_CHECK_EXE, GIGAMUX_JS,
    REQUIRED_WORKER_TAGS,
};
pub use job_log::{
    encode_event_line, encode_logs_dir, job_logs_dir, vmaf_event_line, EncodeJobLog, JobLog,
    JobLogKind,
};
pub use gpu::{
    av1_nvenc_available, encoder_metadata_tags, list_av1_nvenc_devices, list_nvenc_devices,
    list_nvenc_devices_for, nvenc_encoder_works, nvenc_generation_from_sm_name, probe_local_encoders,
    qsv_vp9_available, sanitize_meta_token, GpuChoice, HwCatalog, NvencDevice, NvencGeneration,
    QsvDevice,
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
