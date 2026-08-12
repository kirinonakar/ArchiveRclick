//! Native Windows dialogs and message boxes.

#[cfg(windows)]
mod imp {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    use windows::Win32::Foundation::ERROR_CANCELLED;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
        CoInitializeEx, CoTaskMemFree, CoUninitialize,
    };
    use windows::Win32::UI::Shell::{
        FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR,
        FOS_OVERWRITEPROMPT, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog, FileSaveDialog,
        IFileOpenDialog, IFileSaveDialog, IShellItem, SIGDN_FILESYSPATH,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW,
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
}

#[cfg(not(windows))]
mod imp {
    use std::path::PathBuf;

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
}

pub use super::shell::reveal_in_explorer;
pub use imp::{pick_archive, pick_files, pick_folder, save_archive, show_error, show_info};
