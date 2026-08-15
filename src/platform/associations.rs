//! Per-user registration for Windows' Default Apps UI.
//!
//! Modern Windows protects the user's chosen defaults. ArchiveRclick registers
//! itself as an available handler without overwriting `UserChoice`; the user can
//! then select it in Settings.

#[cfg(windows)]
mod imp {
    use std::{path::Path, ptr};

    use windows::{
        Win32::{
            Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS},
            System::Registry::{
                HKEY, HKEY_CURRENT_USER, REG_SZ, RegCloseKey, RegCreateKeyW, RegDeleteKeyValueW,
                RegDeleteTreeW, RegSetValueExW,
            },
            UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify},
        },
        core::{HSTRING, PCWSTR},
    };

    const PROG_ID: &str = "ArchiveRclick.Archive";
    const EXTENSIONS: &[&str] = &[
        ".zip", ".zipx", ".7z", ".rar", ".tar", ".gz", ".bz2", ".xz", ".zst", ".cab", ".lha",
        ".lzh", ".tgz", ".tbz2", ".txz", ".iso",
    ];

    struct OwnedKey(HKEY);

    impl Drop for OwnedKey {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: the handle came from a successful RegCreateKeyW.
                let _ = unsafe { RegCloseKey(self.0) };
            }
        }
    }

    pub fn register_file_associations(executable: &Path) -> Result<(), String> {
        let executable = std::path::absolute(executable)
            .map_err(|error| format!("Could not resolve {}: {error}", executable.display()))?;
        if !executable.is_file() {
            return Err(format!(
                "Executable does not exist: {}",
                executable.display()
            ));
        }
        let quoted_executable = quote_windows_argument(&executable.to_string_lossy());

        set_default(
            &format!(r"Software\Classes\{PROG_ID}"),
            "ArchiveRclick archive",
        )?;
        set_default(
            &format!(r"Software\Classes\{PROG_ID}\DefaultIcon"),
            &format!("{quoted_executable},0"),
        )?;
        set_default(
            &format!(r"Software\Classes\{PROG_ID}\shell\open\command"),
            &format!("{quoted_executable} \"%1\""),
        )?;

        for extension in EXTENSIONS {
            set_named(
                &format!(r"Software\Classes\{extension}\OpenWithProgids"),
                PROG_ID,
                "",
            )?;
        }

        let capabilities = r"Software\ArchiveRclick\Capabilities";
        set_named(capabilities, "ApplicationName", "ArchiveRclick")?;
        set_named(
            capabilities,
            "ApplicationDescription",
            "Fast, lightweight archive browsing and extraction",
        )?;
        for extension in EXTENSIONS {
            set_named(
                &format!(r"{capabilities}\FileAssociations"),
                extension,
                PROG_ID,
            )?;
        }
        set_named(
            r"Software\RegisteredApplications",
            "ArchiveRclick",
            capabilities,
        )?;
        notify_association_change();
        Ok(())
    }

    pub fn unregister_file_associations() -> Result<(), String> {
        for extension in EXTENSIONS {
            delete_value_if_present(
                &format!(r"Software\Classes\{extension}\OpenWithProgids"),
                PROG_ID,
            )?;
        }
        delete_tree_if_present(&format!(r"Software\Classes\{PROG_ID}"))?;
        delete_tree_if_present(r"Software\ArchiveRclick")?;
        delete_value_if_present(r"Software\RegisteredApplications", "ArchiveRclick")?;
        notify_association_change();
        Ok(())
    }

    fn set_default(key: &str, value: &str) -> Result<(), String> {
        set_value(key, PCWSTR::null(), value)
    }

    fn set_named(key: &str, name: &str, value: &str) -> Result<(), String> {
        let name = HSTRING::from(name);
        set_value(key, PCWSTR(name.as_ptr()), value)
    }

    fn set_value(key: &str, name: PCWSTR, value: &str) -> Result<(), String> {
        let key_name = HSTRING::from(key);
        let mut raw = HKEY(ptr::null_mut());
        // SAFETY: strings remain live for the calls and `raw` is an out-parameter.
        let status = unsafe { RegCreateKeyW(HKEY_CURRENT_USER, &key_name, &mut raw) };
        require_success(status, "create registry key", key)?;
        let key = OwnedKey(raw);
        let data = utf16_bytes(value);
        // SAFETY: key is live and data is valid UTF-16 including its terminator.
        let status = unsafe { RegSetValueExW(key.0, name, None, REG_SZ, Some(&data)) };
        require_success(status, "write registry value", value)
    }

    fn delete_tree_if_present(key: &str) -> Result<(), String> {
        let key_name = HSTRING::from(key);
        // SAFETY: key name remains live for the call.
        let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &key_name) };
        require_deleted(status, "delete registry tree", key)
    }

    fn delete_value_if_present(key: &str, value: &str) -> Result<(), String> {
        let key = HSTRING::from(key);
        let value = HSTRING::from(value);
        // SAFETY: both strings remain live for the call.
        let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &key, &value) };
        require_deleted(status, "delete registry value", value.to_string_lossy())
    }

    fn require_success(
        status: windows::Win32::Foundation::WIN32_ERROR,
        operation: &str,
        subject: &str,
    ) -> Result<(), String> {
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!(
                "Could not {operation} {subject:?} (Windows error {})",
                status.0
            ))
        }
    }

    fn require_deleted(
        status: windows::Win32::Foundation::WIN32_ERROR,
        operation: &str,
        subject: impl AsRef<str>,
    ) -> Result<(), String> {
        if status == ERROR_SUCCESS
            || status == ERROR_FILE_NOT_FOUND
            || status == ERROR_PATH_NOT_FOUND
        {
            Ok(())
        } else {
            Err(format!(
                "Could not {operation} {:?} (Windows error {})",
                subject.as_ref(),
                status.0
            ))
        }
    }

    fn utf16_bytes(value: &str) -> Vec<u8> {
        value
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    fn quote_windows_argument(value: &str) -> String {
        format!("\"{}\"", value.replace('"', "\\\""))
    }

    fn notify_association_change() {
        // SAFETY: SHCNE_ASSOCCHANGED with SHCNF_IDLIST requires no item pointers.
        unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
    }

    #[cfg(test)]
    mod tests {
        use super::{quote_windows_argument, utf16_bytes};

        #[test]
        fn registry_strings_are_terminated_utf16() {
            assert_eq!(utf16_bytes("A"), vec![65, 0, 0, 0]);
        }

        #[test]
        fn executable_paths_are_quoted() {
            assert_eq!(
                quote_windows_argument(r"C:\Program Files\app.exe"),
                r#""C:\Program Files\app.exe""#
            );
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;

    pub fn register_file_associations(_executable: &Path) -> Result<(), String> {
        Err("File-association registration is only available on Windows".to_owned())
    }

    pub fn unregister_file_associations() -> Result<(), String> {
        Err("File-association registration is only available on Windows".to_owned())
    }
}

pub use imp::{register_file_associations, unregister_file_associations};
