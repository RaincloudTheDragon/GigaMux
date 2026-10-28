//! GigaMux core — prefs, GPU probe, encode batch, check (no UI).

pub mod check;
pub mod encode;
pub mod gpu;
pub mod prefs;
pub mod probe;
pub mod targets;

pub use check::{
    deletable_issues, delete_issue_outputs, list_media, permanent_delete_file, restore_trashed_file,
    run_check, trash_file, CheckIssue, CheckReport,
};
pub use encode::{run_batch, EncodeEvent, EncodeOptions};
pub use gpu::{list_nvenc_devices, GpuChoice, NvencDevice};
pub use prefs::Prefs;
pub use probe::{
    codec_heat, codec_tag, container_heat, format_bytes, format_length, media_summary,
    thumbnail_rgba, FrameTiming, MediaSummary, RgbaThumb, THUMB_W,
};
pub use targets::{
    alpha_pair_ok, audio_encode_for, audio_must_recode, cycle_alpha_container,
    cycle_opaque_container, opaque_pair_ok, AlphaVideo, AudioEncode, OpaqueVideo, OutContainer,
    OutputTargets,
};
