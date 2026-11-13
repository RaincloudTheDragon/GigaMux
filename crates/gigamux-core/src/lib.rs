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
    farm_integrity_for_single, farm_vmaf_spec, run_batch, write_farm_ffconcat, EncodeEvent,
    EncodeOptions, FarmChunkedJob, FarmEncodeRequest, FarmFfmpegCommand,
};
pub use ffmpeg_active::{active_ffmpeg_pid, kill_process_tree, ActiveFfmpegGuard};
pub use flamenco::{
    cancel_exec_mode_for_manager_version, check_exe_from_deploy_scripts_dir, delete_probe_pin_tags,
    delete_worker_tag, deploy_gigamux_js, deploy_gigamux_manager, discover_manager,
    ensure_named_worker_tag, export_worker_yamls, farm_probe_out_dir, farm_slot_use_hardware,
    farm_task_type_for_slot, fetch_job_status, fetch_job_tasks, fetch_task_log, fetch_task_logtail,
    fetch_worker_tag_ids, fetch_worker_tags, fetch_workers, flamenco_job_terminal,
    flamenco_script_hint, flamenco_worker_yaml, infer_twoway_local_prefix, is_gigamux_managed_tag,
    load_farm_probe_reports, local_gigamux_js_present, make_gigamux_chunked_job,
    make_gigamux_encode_job, make_gigamux_probe_job, make_gigamux_vmaf_job, manager_web_url,
    merge_farm_probe_dir, normalize_check_exe_path, nvenc_gen_tag_description, pick_log_task,
    pin_worker_for_probe, ping_manager, probe_pin_tag_name, recommend_task_types_from_farm_probe,
    recommended_task_types, resolve_flamenco_check_exe, resolve_worker_tag_uuid,
    restore_worker_tags_after_probe, rewrite_flamenco_twoway_path, safe_worker_yaml_filename,
    scripts_dir_from_manager_install, set_worker_tags, submit_job, worker_status_probeable,
    write_farm_probe_report, CancelExecMode, DeployReport, FarmChunkCheck, FarmCleanup,
    FarmFinalCheck, FarmProbeHostReport, FarmProbeInventory, FarmProbeTaskTypeReport, FarmVmafMode,
    FarmVmafSpec, FlamencoIntegrity, FlamencoJobStatus, FlamencoTaskSummary, FlamencoTwoway,
    FlamencoWorkerSummary, ProbeWorkerPin, SubmittedJob, WorkerTaskTypeRecommendation,
    CAPABILITY_TASK_TYPES, DEFAULT_CHECK_EXE, GIGAMUX_JS,
};
pub use job_log::{
    encode_event_line, encode_logs_dir, job_logs_dir, vmaf_event_line, EncodeJobLog, JobLog,
    JobLogKind,
};
pub use gpu::{
    av1_nvenc_available, encoder_metadata_tags, gigamux_version_label, gigamux_version_payload,
    list_av1_nvenc_devices, list_nvenc_devices, list_nvenc_devices_for, nvenc_encoder_works,
    nvenc_generation_from_sm_name, probe_local_encoders, qsv_vp9_available, sanitize_meta_token,
    GpuChoice, HwCatalog, NvencDevice, NvencGeneration, QsvDevice,
}; // NvencGeneration used by EncodeSlot.nvenc_gen + farm tags
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
pub use vmaf::{
    ensure_vmaf, libvmaf_available, read_vmaf_sidecar, run_vmaf, vmaf_sidecar_path,
    write_vmaf_sidecar, VmafEvent, VmafReport, VmafResult, VmafSidecar,
};
