//! GigaMux core — prefs, GPU probe, encode batch, check (no UI).

pub mod check;
pub mod encode;
pub mod gpu;
pub mod job_log;
pub mod prefs;
pub mod preview_cache;
pub mod probe;
pub mod targets;
pub mod vmaf;

pub use check::{
    deletable_issues, delete_issue_outputs, list_media, permanent_delete_file, restore_trashed_file,
    run_check, trash_file, CheckIssue, CheckReport,
};
pub use encode::{run_batch, EncodeEvent, EncodeOptions};
pub use job_log::{encode_logs_dir, job_logs_dir, EncodeJobLog, JobLog, JobLogKind};
pub use gpu::{
    av1_nvenc_available, list_av1_nvenc_devices, list_nvenc_devices, list_nvenc_devices_for,
    nvenc_encoder_works, GpuChoice, NvencDevice,
};
pub use prefs::Prefs;
pub use probe::{
    codec_heat, codec_tag, container_heat, enrich_summary_bitrates, format_bytes, format_length,
    media_summary, media_summary_fast, pix_fmt_chip_tag, pix_fmt_heat, rate_mode_chip_tag,
    rate_mode_heat, size_delta, thumbnail_rgba, FrameTiming, MediaSummary, RgbaThumb, SizeDelta,
    THUMB_W,
};
pub use targets::{
    alpha_pair_ok, audio_encode_for, audio_must_recode, cycle_alpha_container,
    cycle_opaque_container, delivery_pix_fmt, nvenc_pix_fmt, opaque_pair_ok,
    pix_fmt_convert_warning, pix_fmt_label, pix_fmt_will_convert, software_pix_fmt, AlphaVideo,
    AudioEncode, OpaqueVideo, OutContainer,
    OutputTargets,
};
pub use vmaf::{libvmaf_available, run_vmaf, VmafEvent, VmafReport, VmafResult};
