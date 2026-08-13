//! Native Windows dialogs and message boxes.

#[cfg(windows)]
mod imp {
    use std::ffi::{OsStr, OsString};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;

    use slint::winit_030::WinitWindowAccessor;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
        CoInitializeEx, CoTaskMemFree, CoUninitialize,
    };
    use windows::Win32::UI::Shell::{
        FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR,
        FOS_OVERWRITEPROMPT, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog, FileSaveDialog,
        IFileOpenDialog, IFileSaveDialog, IShellItem, SIGDN_FILESYSPATH, ShellExecuteW,
    };
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW, SW_SHOWNORMAL,
    };
    use windows::Win32::{
        Foundation::{ERROR_CANCELLED, POINT},
        Graphics::Gdi::{
            GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
        },
    };
    use windows::core::{Error, HRESULT, HSTRING, PWSTR};

    struct ComApartment;

    impl ComApartment {
        fn initialize() -> Result<Self, String> {
            // A successful S_OK or S_FALSE must both be balanced with CoUninitialize.
            let result =
                unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };

            if result.is_err() {
                let error: Error = result.into();
                Err(format!("Could not initialize COM: {error}"))
            } else {
                Ok(Self)
            }
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    struct CoTaskMemString(PWSTR);

    impl Drop for CoTaskMemString {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CoTaskMemFree(Some(self.0.as_ptr().cast())) };
            }
        }
    }

    fn dialog_text(value: &str, field: &str) -> Result<HSTRING, String> {
        if value.contains('\0') {
            return Err(format!("{field} cannot contain a null character"));
        }

        Ok(HSTRING::from(value))
    }

    fn is_cancelled(error: &Error) -> bool {
        error.code() == HRESULT::from_win32(ERROR_CANCELLED.0)
    }

    fn show_dialog(result: windows::core::Result<()>, operation: &str) -> Result<bool, String> {
        match result {
            Ok(()) => Ok(true),
            Err(error) if is_cancelled(&error) => Ok(false),
            Err(error) => Err(format!("Could not {operation}: {error}")),
        }
    }

    fn item_path(item: &IShellItem) -> Result<PathBuf, String> {
        let raw = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }
            .map_err(|error| format!("Could not read the selected path: {error}"))?;
        let allocated = CoTaskMemString(raw);

        if allocated.0.is_null() {
            return Err("The selected item did not provide a file-system path".to_owned());
        }

        // SIGDN_FILESYSPATH returns a null-terminated UTF-16 string allocated by COM.
        let value = unsafe { OsString::from_wide(allocated.0.as_wide()) };
        Ok(PathBuf::from(value))
    }

    fn configure_open_dialog(
        dialog: &IFileOpenDialog,
        extra_options: windows::Win32::UI::Shell::FILEOPENDIALOGOPTIONS,
    ) -> Result<(), String> {
        let options = unsafe { dialog.GetOptions() }
            .map_err(|error| format!("Could not read the file-dialog options: {error}"))?;
        unsafe {
            dialog.SetOptions(
                options | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST | FOS_NOCHANGEDIR | extra_options,
            )
        }
        .map_err(|error| format!("Could not configure the file dialog: {error}"))
    }

    pub fn pick_archive() -> Result<Option<PathBuf>, String> {
        let _com = ComApartment::initialize()?;
        let dialog: IFileOpenDialog =
            unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }
                .map_err(|error| format!("Could not create the archive picker: {error}"))?;

        configure_open_dialog(&dialog, FOS_FILEMUSTEXIST)?;
        unsafe { dialog.SetTitle(windows::core::w!("Open archive")) }
            .map_err(|error| format!("Could not set the archive-picker title: {error}"))?;

        if !show_dialog(unsafe { dialog.Show(None) }, "show the archive picker")? {
            return Ok(None);
        }

        let item = unsafe { dialog.GetResult() }
            .map_err(|error| format!("Could not read the selected archive: {error}"))?;
        item_path(&item).map(Some)
    }

    pub fn pick_files() -> Result<Option<Vec<PathBuf>>, String> {
        let _com = ComApartment::initialize()?;
        let dialog: IFileOpenDialog =
            unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }
                .map_err(|error| format!("Could not create the file picker: {error}"))?;

        configure_open_dialog(&dialog, FOS_FILEMUSTEXIST | FOS_ALLOWMULTISELECT)?;
        unsafe { dialog.SetTitle(windows::core::w!("Choose files to archive")) }
            .map_err(|error| format!("Could not set the file-picker title: {error}"))?;

        if !show_dialog(unsafe { dialog.Show(None) }, "show the file picker")? {
            return Ok(None);
        }

        let items = unsafe { dialog.GetResults() }
            .map_err(|error| format!("Could not read the selected files: {error}"))?;
        let count = unsafe { items.GetCount() }
            .map_err(|error| format!("Could not count the selected files: {error}"))?;
        let mut paths = Vec::with_capacity(count as usize);

        for index in 0..count {
            let item = unsafe { items.GetItemAt(index) }
                .map_err(|error| format!("Could not read selected item {index}: {error}"))?;
            paths.push(item_path(&item)?);
        }

        Ok(Some(paths))
    }

    pub fn pick_folder(title: &str) -> Result<Option<PathBuf>, String> {
        let title = dialog_text(title, "Dialog title")?;
        let _com = ComApartment::initialize()?;
        let dialog: IFileOpenDialog =
            unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }
                .map_err(|error| format!("Could not create the folder picker: {error}"))?;

        configure_open_dialog(&dialog, FOS_PICKFOLDERS)?;
        unsafe { dialog.SetTitle(&title) }
            .map_err(|error| format!("Could not set the folder-picker title: {error}"))?;

        if !show_dialog(unsafe { dialog.Show(None) }, "show the folder picker")? {
            return Ok(None);
        }

        let item = unsafe { dialog.GetResult() }
            .map_err(|error| format!("Could not read the selected folder: {error}"))?;
        item_path(&item).map(Some)
    }

    pub fn save_archive(default_name: &str, extension: &str) -> Result<Option<PathBuf>, String> {
        let default_name = dialog_text(default_name, "Default file name")?;
        let extension = extension.trim_start_matches('.');
        if extension.is_empty() || extension.contains(['/', '\\']) {
            return Err("Archive extension must be a non-empty file extension".to_owned());
        }
        let extension = dialog_text(extension, "Archive extension")?;

        let _com = ComApartment::initialize()?;
        let dialog: IFileSaveDialog =
            unsafe { CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER) }
                .map_err(|error| format!("Could not create the save dialog: {error}"))?;

        let options = unsafe { dialog.GetOptions() }
            .map_err(|error| format!("Could not read the save-dialog options: {error}"))?;
        unsafe {
            dialog.SetOptions(
                options
                    | FOS_FORCEFILESYSTEM
                    | FOS_PATHMUSTEXIST
                    | FOS_NOCHANGEDIR
                    | FOS_OVERWRITEPROMPT,
            )
        }
        .map_err(|error| format!("Could not configure the save dialog: {error}"))?;
        unsafe { dialog.SetTitle(windows::core::w!("Save archive")) }
            .map_err(|error| format!("Could not set the save-dialog title: {error}"))?;
        unsafe { dialog.SetFileName(&default_name) }
            .map_err(|error| format!("Could not set the default archive name: {error}"))?;
        unsafe { dialog.SetDefaultExtension(&extension) }
            .map_err(|error| format!("Could not set the default archive extension: {error}"))?;

        if !show_dialog(unsafe { dialog.Show(None) }, "show the save dialog")? {
            return Ok(None);
        }

        let item = unsafe { dialog.GetResult() }
            .map_err(|error| format!("Could not read the archive destination: {error}"))?;
        item_path(&item).map(Some)
    }

    pub fn show_error(title: &str, message: &str) {
        // Embedded nulls would silently truncate Win32 strings, so replace them for display.
        let title = HSTRING::from(title.replace('\0', "�"));
        let message = HSTRING::from(message.replace('\0', "�"));
        unsafe {
            let _ = MessageBoxW(None, &message, &title, MB_OK | MB_ICONERROR);
        }
    }

    pub fn show_info(title: &str, message: &str) {
        let title = HSTRING::from(title.replace('\0', "�"));
        let message = HSTRING::from(message.replace('\0', "�"));
        unsafe {
            let _ = MessageBoxW(None, &message, &title, MB_OK | MB_ICONINFORMATION);
        }
    }

    /// Places a newly created window in the work area of the monitor holding
    /// the pointer. The optional logical size is used when Slint has not
    /// created the native window yet and therefore reports a zero size.
    pub fn center_window(window: &slint::Window) {
        center_window_impl(window, None);
    }

    /// Centers a window before its native handle exists. `logical_size` is
    /// converted with the target monitor's DPI so the position is correct on
    /// high-DPI displays as well.
    pub fn center_window_with_logical_size(
        window: &slint::Window,
        logical_size: slint::LogicalSize,
    ) {
        center_window_impl(window, Some(logical_size));
    }

    fn center_window_impl(window: &slint::Window, fallback_size: Option<slint::LogicalSize>) {
        let mut cursor = POINT::default();
        // SAFETY: cursor is a valid writable point supplied by the caller.
        let _ = unsafe { GetCursorPos(&mut cursor) };
        // SAFETY: the point is initialized even when GetCursorPos fails.
        let monitor = unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST) };
        if monitor.0.is_null() {
            return;
        }
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: info is initialized with the required cbSize and the
        // monitor handle came from MonitorFromPoint.
        if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            return;
        }

        let (width, height) =
            <slint::Window as WinitWindowAccessor>::with_winit_window(window, |winit_window| {
                let size = winit_window.outer_size();
                (size.width, size.height)
            })
            .unwrap_or_else(|| {
                let size = window.size();
                if size.width > 0 && size.height > 0 {
                    (size.width, size.height)
                } else {
                    let scale_factor = monitor_scale_factor(monitor)
                        .unwrap_or_else(|| f64::from(window.scale_factor()).max(0.01));
                    (
                        logical_to_physical(
                            fallback_size.map_or(0.0, |size| size.width),
                            scale_factor,
                        ),
                        logical_to_physical(
                            fallback_size.map_or(0.0, |size| size.height),
                            scale_factor,
                        ),
                    )
                }
            });
        if width == 0 || height == 0 {
            return;
        }
        let width = i32::try_from(width).unwrap_or(i32::MAX);
        let height = i32::try_from(height).unwrap_or(i32::MAX);
        let work = info.rcWork;
        let x = work.left + (work.right - work.left - width).max(0) / 2;
        let y = work.top + (work.bottom - work.top - height).max(0) / 2;
        window.set_position(slint::PhysicalPosition::new(x, y));
    }

    fn monitor_scale_factor(monitor: HMONITOR) -> Option<f64> {
        let mut dpi_x = 0;
        let mut dpi_y = 0;
        // SAFETY: the monitor handle came from MonitorFromPoint and both DPI
        // outputs point to writable local values.
        unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.ok()?;
        (dpi_x > 0).then(|| f64::from(dpi_x) / 96.0)
    }

    fn logical_to_physical(value: f32, scale_factor: f64) -> u32 {
        if !value.is_finite() || value <= 0.0 {
            return 0;
        }
        (f64::from(value) * scale_factor)
            .round()
            .clamp(1.0, f64::from(u32::MAX)) as u32
    }

    /// Starts a second copy of the app with the Windows UAC runas verb.
    /// false means the shell rejected the request (most commonly the user
    /// pressed No in the consent dialog).
    pub fn run_elevated(args: &[OsString]) -> Result<bool, String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("Could not locate ArchiveRclick: {error}"))?;
        let executable = wide_os(executable.as_os_str())?;
        let command_line = args
            .iter()
            .map(|argument| quote_windows_arg(argument.as_os_str()))
            .collect::<Vec<_>>()
            .join(" ");
        let command_line = wide_text(&command_line)?;
        // SAFETY: all strings are NUL-terminated and remain alive through the
        // synchronous ShellExecuteW call.
        let result = unsafe {
            ShellExecuteW(
                None,
                windows::core::w!("runas"),
                windows::core::PCWSTR(executable.as_ptr()),
                windows::core::PCWSTR(command_line.as_ptr()),
                windows::core::PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        Ok(result.0 as usize > 32)
    }

    fn wide_text(value: &str) -> Result<Vec<u16>, String> {
        if value.contains('\0') {
            return Err("The elevation command contains a null character".to_owned());
        }
        let mut wide: Vec<u16> = value.encode_utf16().collect();
        wide.push(0);
        Ok(wide)
    }

    fn wide_os(value: &OsStr) -> Result<Vec<u16>, String> {
        if value.encode_wide().any(|unit| unit == 0) {
            return Err("The executable path contains a null character".to_owned());
        }
        let mut wide: Vec<u16> = value.encode_wide().collect();
        wide.push(0);
        Ok(wide)
    }

    pub(crate) fn quote_windows_arg(value: &OsStr) -> String {
        let value = value.to_string_lossy();
        let mut quoted = String::with_capacity(value.len() + 2);
        quoted.push('"');
        let mut backslashes = 0usize;
        for character in value.chars() {
            match character {
                '\\' => backslashes += 1,
                '"' => {
                    quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                    quoted.push('"');
                    backslashes = 0;
                }
                _ => {
                    quoted.extend(std::iter::repeat_n('\\', backslashes));
                    backslashes = 0;
                    quoted.push(character);
                }
            }
        }
        quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
        quoted.push('"');
        quoted
    }
}

#[cfg(not(windows))]
mod imp {
    use std::{ffi::OsString, path::PathBuf};

    fn unsupported(operation: &str) -> String {
        format!("{operation} is only available on Windows")
    }

    pub fn pick_archive() -> Result<Option<PathBuf>, String> {
        Err(unsupported("The native archive picker"))
    }

    pub fn pick_files() -> Result<Option<Vec<PathBuf>>, String> {
        Err(unsupported("The native file picker"))
    }

    pub fn pick_folder(_title: &str) -> Result<Option<PathBuf>, String> {
        Err(unsupported("The native folder picker"))
    }

    pub fn save_archive(_default_name: &str, _extension: &str) -> Result<Option<PathBuf>, String> {
        Err(unsupported("The native save dialog"))
    }

    pub fn show_error(title: &str, message: &str) {
        eprintln!("{title}: {message}");
    }

    pub fn show_info(title: &str, message: &str) {
        println!("{title}: {message}");
    }

    pub fn center_window(_window: &slint::Window) {}

    pub fn center_window_with_logical_size(
        _window: &slint::Window,
        _logical_size: slint::LogicalSize,
    ) {
    }

    pub fn run_elevated(_args: &[OsString]) -> Result<bool, String> {
        Err(unsupported("Elevation"))
    }
}

pub use super::shell::reveal_in_explorer;
pub use imp::{
    center_window, center_window_with_logical_size, pick_archive, pick_files, pick_folder,
    run_elevated, save_archive, show_error, show_info,
};
pub(crate) use imp::quote_windows_arg;
