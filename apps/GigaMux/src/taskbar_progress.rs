//! Windows taskbar progress overlay (`ITaskbarList3` — green fill on Win10).

use std::cell::RefCell;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::core::Interface;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::{
    ITaskbarList, ITaskbarList3, TaskbarList, TBPF_NOPROGRESS, TBPF_NORMAL,
};

// UI thread only (eframe `logic` / `ui`); COM interfaces are not Sync.
thread_local! {
    static TASKBAR: RefCell<Option<ITaskbarList3>> = const { RefCell::new(None) };
}

/// Extract the Win32 HWND from an eframe frame (best-effort).
pub fn hwnd_from_frame(frame: &eframe::Frame) -> Option<HWND> {
    let handle = frame.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut _)),
        _ => None,
    }
}

fn with_taskbar(f: impl FnOnce(&ITaskbarList3)) {
    TASKBAR.with(|cell| {
        if cell.borrow().is_none() {
            let created = unsafe {
                let list: ITaskbarList3 =
                    match CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER) {
                        Ok(l) => l,
                        Err(_) => return,
                    };
                let base: ITaskbarList = match list.cast() {
                    Ok(b) => b,
                    Err(_) => return,
                };
                if base.HrInit().is_err() {
                    return;
                }
                Some(list)
            };
            *cell.borrow_mut() = created;
        }
        if let Some(tb) = cell.borrow().as_ref() {
            f(tb);
        }
    });
}

/// Show determinate progress `0.0..=1.0` on the taskbar button for `hwnd`.
pub fn set_progress(hwnd: HWND, frac: f32) {
    let frac = frac.clamp(0.0, 1.0);
    let completed = (frac * 1000.0).round() as u64;
    with_taskbar(|tb| unsafe {
        let _ = tb.SetProgressState(hwnd, TBPF_NORMAL);
        let _ = tb.SetProgressValue(hwnd, completed, 1000);
    });
}

/// Dismiss the taskbar progress overlay.
pub fn clear(hwnd: HWND) {
    with_taskbar(|tb| unsafe {
        let _ = tb.SetProgressState(hwnd, TBPF_NOPROGRESS);
    });
}

/// Drive the overlay from batch progress (`None` clears).
pub fn sync(frame: &eframe::Frame, batch_frac: Option<f32>) {
    let Some(hwnd) = hwnd_from_frame(frame) else {
        return;
    };
    match batch_frac {
        Some(f) => set_progress(hwnd, f),
        None => clear(hwnd),
    }
}
