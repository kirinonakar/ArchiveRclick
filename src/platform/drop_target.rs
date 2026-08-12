//! Native file-drop bridge for Explorer-to-application drops.

use std::path::PathBuf;

pub type FileDropHandler = Box<dyn Fn(Vec<PathBuf>)>;

#[cfg(windows)]
mod imp {
    use std::{
        ffi::OsString,
        os::windows::ffi::OsStringExt,
        panic::{AssertUnwindSafe, catch_unwind},
        path::PathBuf,
        ptr,
    };

    use windows::{
        Win32::{
            Foundation::{HWND, LPARAM, LRESULT, WPARAM},
            UI::{
                Shell::{
                    DefSubclassProc, DragAcceptFiles, DragFinish, DragQueryFileW, HDROP,
                    RemoveWindowSubclass, SetWindowSubclass,
                },
                WindowsAndMessaging::{
                    EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
                    IsWindowVisible, WM_DROPFILES,
                },
            },
        },
        core::BOOL,
    };

    use super::FileDropHandler;

    const DROP_SUBCLASS_ID: usize = 0x4152_434B;

    pub struct FileDropTarget {
        hwnd: HWND,
        handler: *mut FileDropHandler,
    }

    impl Drop for FileDropTarget {
        fn drop(&mut self) {
            // SAFETY: this object owns the subclass registration and boxed
            // callback. The UI event loop is no longer dispatching the callback
            // when the registration is dropped on the UI thread.
            unsafe {
                DragAcceptFiles(self.hwnd, false);
                let _ = RemoveWindowSubclass(self.hwnd, Some(drop_subclass_proc), DROP_SUBCLASS_ID);
                drop(Box::from_raw(self.handler));
            }
        }
    }

    struct WindowSearch {
        process_id: u32,
        hwnd: HWND,
    }

    unsafe extern "system" fn find_window(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: EnumWindows passes back the WindowSearch pointer supplied by
        // find_main_window for the duration of the synchronous enumeration.
        let search = unsafe { &mut *(data.0 as *mut WindowSearch) };
        let mut process_id = 0u32;
        // SAFETY: hwnd is supplied by EnumWindows and process_id is writable.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
        if process_id != search.process_id || !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            return true.into();
        }

        // Match the Slint window title as well as the process id so hidden
        // renderer/helper windows cannot accidentally receive the subclass.
        let length = unsafe { GetWindowTextLengthW(hwnd) };
        if length <= 0 {
            return true.into();
        }
        let mut title = vec![0u16; length as usize + 1];
        let copied = unsafe { GetWindowTextW(hwnd, &mut title) };
        let title = String::from_utf16_lossy(&title[..copied.max(0) as usize]);
        if title == "ArchiveRclick" || title.ends_with(" — ArchiveRclick") {
            search.hwnd = hwnd;
            return false.into();
        }
        true.into()
    }

    fn find_main_window() -> Result<HWND, String> {
        let mut search = WindowSearch {
            process_id: std::process::id(),
            hwnd: HWND(ptr::null_mut()),
        };
        // EnumWindows reports failure when the callback deliberately stops the
        // enumeration. The populated handle is the authoritative result.
        let _ = unsafe {
            EnumWindows(
                Some(find_window),
                LPARAM((&mut search as *mut WindowSearch) as isize),
            )
        };
        if search.hwnd.0.is_null() {
            Err("Could not locate the ArchiveRclick window for file drop".to_owned())
        } else {
            Ok(search.hwnd)
        }
    }

    fn dropped_paths(drop: HDROP) -> Vec<PathBuf> {
        // SAFETY: drop is valid until DragFinish is called by the window proc.
        let count = unsafe { DragQueryFileW(drop, u32::MAX, None) };
        let mut paths = Vec::with_capacity(count as usize);
        for index in 0..count {
            let length = unsafe { DragQueryFileW(drop, index, None) };
            if length == 0 {
                continue;
            }
            let mut buffer = vec![0u16; length as usize + 1];
            let copied = unsafe { DragQueryFileW(drop, index, Some(&mut buffer)) };
            if copied > 0 {
                paths.push(PathBuf::from(OsString::from_wide(
                    &buffer[..copied as usize],
                )));
            }
        }
        paths
    }

    unsafe extern "system" fn drop_subclass_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _subclass_id: usize,
        handler: usize,
    ) -> LRESULT {
        if message == WM_DROPFILES {
            let drop = HDROP(wparam.0 as *mut _);
            let paths = dropped_paths(drop);
            // SAFETY: WM_DROPFILES transfers ownership of this handle to the
            // receiver and requires exactly one DragFinish call.
            unsafe { DragFinish(drop) };
            if !paths.is_empty() && handler != 0 {
                // SAFETY: FileDropTarget keeps the boxed callback alive while
                // the subclass is installed. Prevent a Rust panic crossing the
                // system callback boundary.
                let callback = unsafe { &*(handler as *const FileDropHandler) };
                let _ = catch_unwind(AssertUnwindSafe(|| callback(paths)));
            }
            return LRESULT(0);
        }

        // SAFETY: unhandled messages must continue through the comctl32
        // subclass chain.
        unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
    }

    pub fn install_file_drop_handler(handler: FileDropHandler) -> Result<FileDropTarget, String> {
        let hwnd = find_main_window()?;
        let handler = Box::into_raw(Box::new(handler));
        // SAFETY: hwnd belongs to the current UI thread and handler remains
        // allocated until FileDropTarget removes the subclass.
        let installed = unsafe {
            SetWindowSubclass(
                hwnd,
                Some(drop_subclass_proc),
                DROP_SUBCLASS_ID,
                handler as usize,
            )
        };
        if !installed.as_bool() {
            // SAFETY: installation failed, so no system callback can retain it.
            unsafe { drop(Box::from_raw(handler)) };
            return Err(format!(
                "Could not enable Explorer file drop: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: the legacy shell drop contract is enabled only after the
        // subclass is ready to receive and release HDROP handles.
        unsafe { DragAcceptFiles(hwnd, true) };
        Ok(FileDropTarget { hwnd, handler })
    }
}

#[cfg(not(windows))]
mod imp {
    use super::FileDropHandler;

    pub struct FileDropTarget {
        _handler: FileDropHandler,
    }

    pub fn install_file_drop_handler(handler: FileDropHandler) -> Result<FileDropTarget, String> {
        Ok(FileDropTarget { _handler: handler })
    }
}

pub use imp::{FileDropTarget, install_file_drop_handler};
