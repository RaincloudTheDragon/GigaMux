//! Shutter-style ffmpeg pause: suspend on user input; resume after 1 second idle.

use std::collections::HashMap;
use std::mem::size_of;
use std::time::Duration;

/// Quiet time before a suspended ffmpeg is resumed.
pub const IDLE_RESUME: Duration = Duration::from_secs(1);

/// System-wide idle time since the last mouse/keyboard input (Windows `GetLastInputInfo`).
#[cfg(windows)]
pub fn system_idle() -> Duration {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    let mut info = LASTINPUTINFO {
        cbSize: size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: LASTINPUTINFO is fully initialized; GetLastInputInfo writes dwTime.
    let ok = unsafe { GetLastInputInfo(&mut info) };
    if !ok.as_bool() {
        return Duration::ZERO;
    }
    let now = unsafe { GetTickCount() };
    Duration::from_millis(u64::from(now.wrapping_sub(info.dwTime)))
}

#[cfg(not(windows))]
pub fn system_idle() -> Duration {
    Duration::from_secs(u64::MAX / 4)
}

/// Suspend every process in the tree rooted at `root_pid` (shim + real ffmpeg).
#[cfg(windows)]
pub fn suspend_process_tree(root_pid: u32) -> bool {
    set_tree_suspended(root_pid, true)
}

/// Resume a tree previously suspended by [`suspend_process_tree`].
#[cfg(windows)]
pub fn resume_process_tree(root_pid: u32) -> bool {
    set_tree_suspended(root_pid, false)
}

#[cfg(not(windows))]
pub fn suspend_process_tree(_root_pid: u32) -> bool {
    false
}

#[cfg(not(windows))]
pub fn resume_process_tree(_root_pid: u32) -> bool {
    false
}

#[cfg(windows)]
fn set_tree_suspended(root_pid: u32, suspend: bool) -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SUSPEND_RESUME};

    let snap = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(h) => h,
        Err(_) => return false,
    };

    let mut parent_of: HashMap<u32, u32> = HashMap::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: snapshot + PROCESSENTRY32W sized correctly for the ToolHelp APIs.
    let mut ok = unsafe { Process32FirstW(snap, &mut entry) };
    while ok.is_ok() {
        parent_of.insert(entry.th32ProcessID, entry.th32ParentProcessID);
        ok = unsafe { Process32NextW(snap, &mut entry) };
    }
    let _ = unsafe { CloseHandle(snap) };

    // Collect root + descendants (parent chain points at an included pid).
    let mut tree = vec![root_pid];
    let mut changed = true;
    while changed {
        changed = false;
        for (&pid, &ppid) in &parent_of {
            if tree.contains(&ppid) && !tree.contains(&pid) {
                tree.push(pid);
                changed = true;
            }
        }
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn NtSuspendProcess(process_handle: HANDLE) -> i32;
        fn NtResumeProcess(process_handle: HANDLE) -> i32;
    }

    let mut any = false;
    for pid in tree {
        let handle = match unsafe { OpenProcess(PROCESS_SUSPEND_RESUME, false, pid) } {
            Ok(h) => h,
            Err(_) => continue,
        };
        // SAFETY: handle opened with PROCESS_SUSPEND_RESUME.
        let status = unsafe {
            if suspend {
                NtSuspendProcess(handle)
            } else {
                NtResumeProcess(handle)
            }
        };
        let _ = unsafe { CloseHandle(handle) };
        if status >= 0 {
            any = true;
        }
    }
    any
}
