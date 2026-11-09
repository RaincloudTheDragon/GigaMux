//! Publish the live ffmpeg root PID so the UI can Shutter-style suspend/resume/kill it.
//!
//! Chocolatey ShimGen `ffmpeg.exe` spawns a real encoder as a child. `Child::kill` alone often terminates only the shim and orphans the worker — always prefer [`kill_process_tree`].

use std::process::{Child, Command, Stdio};
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

/// Force-kill `pid` and its descendants (Windows: `taskkill /T /F`).
pub fn kill_process_tree(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        // Best-effort process-group kill when we spawned with a new session (encode/VMAF do not today).
        let _ = Command::new("kill")
            .args(["-TERM", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Tree-kill then `Child::kill`/`wait` (cancel paths).
pub fn force_kill_child(child: &mut Child) {
    let pid = child.id();
    kill_process_tree(pid);
    let _ = child.kill();
    let _ = child.wait();
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

/// `Child` that tree-kills on drop if still running (shim orphans / panic / forgotten cancel).
pub struct TrackedFfmpegChild {
    child: Child,
}

impl TrackedFfmpegChild {
    pub fn new(child: Child) -> Self {
        Self { child }
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn stderr(&mut self) -> &mut Option<std::process::ChildStderr> {
        &mut self.child.stderr
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    pub fn force_kill(&mut self) {
        force_kill_child(&mut self.child);
    }
}

impl Drop for TrackedFfmpegChild {
    fn drop(&mut self) {
        // If we already waited, try_wait returns Some and we leave the corpse alone.
        match self.child.try_wait() {
            Ok(None) => force_kill_child(&mut self.child),
            _ => {}
        }
    }
}
