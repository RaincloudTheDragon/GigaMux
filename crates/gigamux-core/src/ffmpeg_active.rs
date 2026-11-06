//! Publish the live ffmpeg root PID so the UI can Shutter-style suspend/resume it.

use std::sync::atomic::{AtomicU32, Ordering};

/// Root PID of the ffmpeg currently owned by encode/VMAF (`0` = none).
static ACTIVE_ROOT_PID: AtomicU32 = AtomicU32::new(0);

/// Register `pid` as the active ffmpeg until [`ActiveFfmpegGuard`] drops.
pub fn set_active_ffmpeg_pid(pid: u32) {
    ACTIVE_ROOT_PID.store(pid, Ordering::SeqCst);
}

/// Clear only if `pid` is still the registered root (ignore stale clears).
pub fn clear_active_ffmpeg_pid(pid: u32) {
    let _ = ACTIVE_ROOT_PID.compare_exchange(pid, 0, Ordering::SeqCst, Ordering::SeqCst);
}

/// Current encode/VMAF ffmpeg root PID, if any.
pub fn active_ffmpeg_pid() -> Option<u32> {
    match ACTIVE_ROOT_PID.load(Ordering::SeqCst) {
        0 => None,
        p => Some(p),
    }
}

/// RAII: publish PID while the Child lives; clear on drop (cancel, exit, or panic).
pub struct ActiveFfmpegGuard {
    pid: u32,
}

impl ActiveFfmpegGuard {
    pub fn new(pid: u32) -> Self {
        set_active_ffmpeg_pid(pid);
        Self { pid }
    }
}

impl Drop for ActiveFfmpegGuard {
    fn drop(&mut self) {
        clear_active_ffmpeg_pid(self.pid);
    }
}
