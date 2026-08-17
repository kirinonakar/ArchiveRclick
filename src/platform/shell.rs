//! Shell integration shared by archive operations.

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR};

    fn path_string(value: &OsStr) -> Result<HSTRING, String> {
        let wide: Vec<u16> = value.encode_wide().collect();
        if wide.contains(&0) {
            return Err("The path cannot contain a null character".to_owned());
        }
        Ok(HSTRING::from_wide(&wide))
    }

    fn shell_execute(
        target: impl windows::core::Param<PCWSTR>,
        parameters: impl windows::core::Param<PCWSTR>,
    ) -> Result<(), String> {
        let result = unsafe {
            ShellExecuteW(
                None,
                windows::core::w!("open"),
                target,
                parameters,
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        let code = result.0 as isize;
        if code <= 32 {
            Err(format!(
                "Windows could not open the target (ShellExecuteW returned {code})"
            ))
        } else {
            Ok(())
        }
    }

    pub fn open_file(path: &Path) -> Result<(), String> {
        let absolute: PathBuf = std::path::absolute(path)
            .map_err(|error| format!("Could not resolve {}: {error}", path.display()))?;
        let metadata = absolute
            .metadata()
            .map_err(|error| format!("Could not inspect {}: {error}", absolute.display()))?;
        if !metadata.is_file() {
            return Err(format!("The target is not a file: {}", absolute.display()));
        }

        let target = path_string(absolute.as_os_str())?;
        shell_execute(&target, PCWSTR::null())
    }

    pub fn open_url(url: &str) -> Result<(), String> {
        if url.contains('\0') {
            return Err("The URL cannot contain a null character".to_owned());
        }
        let target = HSTRING::from(url);
        shell_execute(&target, PCWSTR::null())
    }

    pub fn reveal_in_explorer(path: &Path) -> Result<(), String> {
        let absolute: PathBuf = std::path::absolute(path)
            .map_err(|error| format!("Could not resolve {}: {error}", path.display()))?;
        let metadata = absolute
            .metadata()
            .map_err(|error| format!("Could not inspect {}: {error}", absolute.display()))?;

        if metadata.is_dir() {
            let target = path_string(absolute.as_os_str())?;
            return shell_execute(&target, PCWSTR::null());
        }

        let mut parameters: Vec<u16> = "/select,\"".encode_utf16().collect();
        parameters.extend(absolute.as_os_str().encode_wide());
        parameters.push('"' as u16);
        if parameters.contains(&0) {
            return Err("The path cannot contain a null character".to_owned());
        }
        let parameters = HSTRING::from_wide(&parameters);

        shell_execute(windows::core::w!("explorer.exe"), &parameters)
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;

    pub fn open_file(_path: &Path) -> Result<(), String> {
        Err("Opening files is only available on Windows".to_owned())
    }

    pub fn open_url(_url: &str) -> Result<(), String> {
        Err("Opening links is only available on Windows".to_owned())
    }

    pub fn reveal_in_explorer(_path: &Path) -> Result<(), String> {
        Err("Reveal in Explorer is only available on Windows".to_owned())
    }
}

pub use imp::{open_file, open_url, reveal_in_explorer};
