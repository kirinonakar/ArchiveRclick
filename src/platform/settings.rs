//! User preferences, persisted in the per-user registry.

#[cfg(windows)]
mod imp {
    use std::ptr;

    use windows::{
        Win32::{
            Foundation::{ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS},
            System::Registry::{
                HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ, REG_VALUE_TYPE,
                RegCloseKey, RegCreateKeyW, RegEnumValueW, RegOpenKeyExW, RegQueryValueExW,
                RegSetValueExW,
            },
        },
        core::{HSTRING, PCWSTR, PWSTR},
    };

    const SETTINGS_KEY: &str = r"Software\ArchiveRclick\Settings";
    const FONT_VALUE: &str = "FontFamily";
    const THREAD_VALUE: &str = "CpuThreads";
    const FONTS_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts";

    const AUTO: &str = "auto";
    const PREFERRED_FONT: &str = "Noto Sans JP";
    const FALLBACK_FONT: &str = "Yu Gothic";

    struct OwnedKey(HKEY);

    impl Drop for OwnedKey {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: the handle came from a successful registry call.
                let _ = unsafe { RegCloseKey(self.0) };
            }
        }
    }

    /// Loads the stored font preference; returns "auto" when unset.
    pub fn load_font_preference() -> String {
        let Some(key) = open_key(HKEY_CURRENT_USER, SETTINGS_KEY) else {
            return AUTO.to_owned();
        };
        match read_string_value(key.0, FONT_VALUE) {
            Some(value) if !value.is_empty() => value,
            _ => AUTO.to_owned(),
        }
    }

    /// Persists the font preference ("auto" or a concrete family name).
    pub fn save_font_preference(family: &str) -> Result<(), String> {
        let key_name = HSTRING::from(SETTINGS_KEY);
        let mut raw = HKEY(ptr::null_mut());
        // SAFETY: the key name stays live and `raw` is an out-parameter.
        let status = unsafe { RegCreateKeyW(HKEY_CURRENT_USER, &key_name, &mut raw) };
        if status != ERROR_SUCCESS {
            return Err(format!(
                "Could not open the settings registry key (Windows error {})",
                status.0
            ));
        }
        let key = OwnedKey(raw);
        let value = HSTRING::from(family);
        let data = utf16_bytes(family);
        // SAFETY: key is live and data is valid UTF-16 including its terminator.
        let status = unsafe { RegSetValueExW(key.0, PCWSTR(value.as_ptr()), None, REG_SZ, Some(&data)) };
        if status != ERROR_SUCCESS {
            return Err(format!(
                "Could not write the settings registry value (Windows error {})",
                status.0
            ));
        }
        Ok(())
    }

    /// Loads the stored CPU-thread preference for 7z compression; returns
    /// "auto" when unset.
    pub fn load_thread_preference() -> String {
        let Some(key) = open_key(HKEY_CURRENT_USER, SETTINGS_KEY) else {
            return AUTO.to_owned();
        };
        match read_string_value(key.0, THREAD_VALUE) {
            Some(value) if !value.is_empty() => value,
            _ => AUTO.to_owned(),
        }
    }

    /// Persists the CPU-thread preference ("auto", "4", "8", "16", "all").
    pub fn save_thread_preference(preference: &str) -> Result<(), String> {
        let key_name = HSTRING::from(SETTINGS_KEY);
        let mut raw = HKEY(ptr::null_mut());
        // SAFETY: the key name stays live and `raw` is an out-parameter.
        let status = unsafe { RegCreateKeyW(HKEY_CURRENT_USER, &key_name, &mut raw) };
        if status != ERROR_SUCCESS {
            return Err(format!(
                "Could not open the settings registry key (Windows error {})",
                status.0
            ));
        }
        let key = OwnedKey(raw);
        let value = HSTRING::from(preference);
        let data = utf16_bytes(preference);
        // SAFETY: key is live and data is valid UTF-16 including its terminator.
        let status = unsafe {
            RegSetValueExW(key.0, PCWSTR(value.as_ptr()), None, REG_SZ, Some(&data))
        };
        if status != ERROR_SUCCESS {
            return Err(format!(
                "Could not write the settings registry value (Windows error {})",
                status.0
            ));
        }
        Ok(())
    }

    /// Resolves a stored preference to a concrete font family.
    ///
    /// "auto" picks Noto Sans JP when it is installed on this system and
    /// falls back to Yu Gothic (shipped with every Windows 8.1+) otherwise.
    pub fn resolve_font_family(preference: &str) -> String {
        if preference.is_empty() || preference == AUTO {
            if font_family_installed(PREFERRED_FONT) {
                PREFERRED_FONT.to_owned()
            } else {
                FALLBACK_FONT.to_owned()
            }
        } else {
            preference.to_owned()
        }
    }

    /// Reports whether a font family is registered for the machine or the
    /// current user (e.g. "Noto Sans JP" matches "Noto Sans JP (TrueType)").
    fn font_family_installed(family: &str) -> bool {
        if family.is_empty() {
            return false;
        }
        font_hive_has_family(HKEY_LOCAL_MACHINE, family)
            || font_hive_has_family(HKEY_CURRENT_USER, family)
    }

    fn font_hive_has_family(hive: HKEY, family: &str) -> bool {
        let Some(key) = open_key(hive, FONTS_KEY) else {
            return false;
        };
        let wanted = family.to_lowercase();
        let mut index = 0u32;
        loop {
            let mut buffer = [0u16; 512];
            let mut length = buffer.len() as u32;
            // SAFETY: the buffers are valid for the documented sizes and the
            // key is live for the call.
            let status = unsafe {
                RegEnumValueW(
                    key.0,
                    index,
                    Some(PWSTR(buffer.as_mut_ptr())),
                    &mut length,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if status == ERROR_NO_MORE_ITEMS {
                return false;
            }
            if status == ERROR_MORE_DATA {
                // Abnormally long value name; skip it.
                index += 1;
                continue;
            }
            if status != ERROR_SUCCESS {
                return false;
            }
            let name = String::from_utf16_lossy(&buffer[..length as usize]);
            if value_name_matches(&name, &wanted) {
                return true;
            }
            index += 1;
        }
    }

    /// True when a font registry value name starts with the family, e.g.
    /// "Noto Sans JP (TrueType)" starts with "noto sans jp".
    fn value_name_matches(name: &str, wanted_lowercase: &str) -> bool {
        name.to_lowercase().starts_with(wanted_lowercase)
    }

    fn open_key(hive: HKEY, path: &str) -> Option<OwnedKey> {
        let key_name = HSTRING::from(path);
        let mut raw = HKEY(ptr::null_mut());
        // SAFETY: the key name stays live and `raw` is an out-parameter.
        let status = unsafe { RegOpenKeyExW(hive, &key_name, None, KEY_READ, &mut raw) };
        (status == ERROR_SUCCESS).then_some(OwnedKey(raw))
    }

    fn read_string_value(key: HKEY, value: &str) -> Option<String> {
        let value = HSTRING::from(value);
        let mut size = 0u32;
        // SAFETY: `size` is a valid out-parameter.
        let status = unsafe { RegQueryValueExW(key, PCWSTR(value.as_ptr()), None, None, None, Some(&mut size)) };
        if status != ERROR_SUCCESS || size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        let mut value_type = REG_VALUE_TYPE(0);
        // SAFETY: `data` has the exact size the API requested and
        // `value_type` is a valid out-parameter.
        let status = unsafe {
            RegQueryValueExW(
                key,
                PCWSTR(value.as_ptr()),
                None,
                Some(&mut value_type),
                Some(data.as_mut_ptr()),
                Some(&mut size),
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let bytes = &data[..size as usize];
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        Some(
            String::from_utf16_lossy(&units)
                .trim_end_matches('\0')
                .to_owned(),
        )
    }

    fn utf16_bytes(value: &str) -> Vec<u8> {
        value
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::value_name_matches;

        #[test]
        fn font_value_names_match_by_prefix() {
            assert!(value_name_matches("Noto Sans JP (TrueType)", "noto sans jp"));
            assert!(value_name_matches(
                "Yu Gothic Regular & Yu Gothic UI Semilight (TrueType)",
                "yu gothic"
            ));
            assert!(!value_name_matches("Meiryo (TrueType)", "noto sans jp"));
            assert!(!value_name_matches("Noto Sans KR (TrueType)", "noto sans jp"));
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn load_font_preference() -> String {
        "auto".to_owned()
    }

    pub fn save_font_preference(_family: &str) -> Result<(), String> {
        Err("Settings persistence is only available on Windows".to_owned())
    }

    pub fn load_thread_preference() -> String {
        "auto".to_owned()
    }

    pub fn save_thread_preference(_preference: &str) -> Result<(), String> {
        Err("Settings persistence is only available on Windows".to_owned())
    }

    pub fn resolve_font_family(preference: &str) -> String {
        if preference.is_empty() || preference == "auto" {
            "Yu Gothic".to_owned()
        } else {
            preference.to_owned()
        }
    }
}

pub use imp::{
    load_font_preference, load_thread_preference, resolve_font_family, save_font_preference,
    save_thread_preference,
};
