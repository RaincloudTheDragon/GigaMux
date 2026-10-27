//! GigaMux core — prefs, GPU probe, encode batch, check (no UI).

pub mod check;
pub mod encode;
pub mod gpu;
pub mod prefs;
pub mod probe;

pub use check::{
    deletable_issues, delete_issue_outputs, list_media, run_check, CheckIssue, CheckReport,
};
pub use encode::{run_batch, EncodeEvent, EncodeOptions};
pub use gpu::{list_nvenc_devices, GpuChoice, NvencDevice};
pub use prefs::Prefs;
pub use probe::{
    format_bytes, format_length, media_summary, thumbnail_rgba, FrameTiming, MediaSummary,
    RgbaThumb, THUMB_W,
};
