//! Mirrors archive-operation progress on the Windows taskbar button.

#[cfg(windows)]
mod imp {
    use std::cell::RefCell;

    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::winit_030::{WinitWindowAccessor, winit};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    use windows::Win32::UI::Shell::{
        ITaskbarList3, TaskbarList, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_PAUSED,
    };

    thread_local! {
        // Created lazily on the UI thread. A failed creation is intentionally
        // not cached: COM may not be initialized yet before the event loop
        // starts, and a later call should retry instead of staying disabled.
        static TASKBAR_LIST: RefCell<Option<ITaskbarList3>> = const { RefCell::new(None) };
    }

    fn hwnd_from_window(window: &winit::window::Window) -> Option<HWND> {
        let handle = window.window_handle().ok()?.as_raw();
        match handle {
            RawWindowHandle::Win32(handle) => {
                Some(HWND(handle.hwnd.get() as *mut std::ffi::c_void))
            }
            _ => None,
        }
    }

    fn create_taskbar_list() -> windows::core::Result<ITaskbarList3> {
        // Runs on the UI thread, where the winit event loop has initialized
        // the COM apartment. HrInit must succeed before the progress calls.
        let taskbar_list: ITaskbarList3 =
            unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER)? };
        unsafe { taskbar_list.HrInit()? };
        Ok(taskbar_list)
    }

    fn with_taskbar_list(callback: impl FnOnce(&ITaskbarList3)) {
        TASKBAR_LIST.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_none()
                && let Ok(taskbar_list) = create_taskbar_list()
            {
                *slot = Some(taskbar_list);
            }
            if let Some(taskbar_list) = slot.as_ref() {
                callback(taskbar_list);
            }
        });
    }

    fn with_hwnd(window: &slint::Window, callback: impl FnOnce(&ITaskbarList3, HWND)) {
        // The native window does not exist before the first show; skipping is
        // expected then, and the next progress update applies the state.
        let _ = window.with_winit_window(|winit_window| {
            let Some(hwnd) = hwnd_from_window(winit_window) else {
                return;
            };
            with_taskbar_list(|taskbar_list| callback(taskbar_list, hwnd));
        });
    }

    /// Shows a marquee (indeterminate) progress on the taskbar button.
    pub fn show_indeterminate(window: &slint::Window) {
        with_hwnd(window, |taskbar_list, hwnd| unsafe {
            let _ = taskbar_list.SetProgressState(hwnd, TBPF_INDETERMINATE);
        });
    }

    /// Shows determinate progress; `fraction` is clamped to 0.0..=1.0.
    pub fn show_fraction(window: &slint::Window, fraction: f32) {
        const TOTAL: u64 = 10_000;
        let completed = (fraction.clamp(0.0, 1.0) * TOTAL as f32) as u64;
        with_hwnd(window, |taskbar_list, hwnd| unsafe {
            let _ = taskbar_list.SetProgressValue(hwnd, completed, TOTAL);
            let _ = taskbar_list.SetProgressState(hwnd, TBPF_NORMAL);
        });
    }

    /// Marks the running operation as paused; used while cancelling.
    pub fn pause(window: &slint::Window) {
        with_hwnd(window, |taskbar_list, hwnd| unsafe {
            let _ = taskbar_list.SetProgressState(hwnd, TBPF_PAUSED);
        });
    }

    /// Removes any progress overlay from the taskbar button.
    pub fn clear(window: &slint::Window) {
        with_hwnd(window, |taskbar_list, hwnd| unsafe {
            let _ = taskbar_list.SetProgressState(hwnd, TBPF_NOPROGRESS);
        });
    }
}

#[cfg(windows)]
pub use imp::{clear, pause, show_fraction, show_indeterminate};

#[cfg(not(windows))]
mod imp {
    //! Taskbar progress is a Windows-only feature; other platforms no-op.

    pub fn show_indeterminate(_window: &slint::Window) {}
    pub fn show_fraction(_window: &slint::Window, _fraction: f32) {}
    pub fn pause(_window: &slint::Window) {}
    pub fn clear(_window: &slint::Window) {}
}

#[cfg(not(windows))]
pub use imp::{clear, pause, show_fraction, show_indeterminate};
