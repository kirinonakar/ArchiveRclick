//! Synchronizes the native Windows title bar with the selected app theme.

#[cfg(windows)]
mod windows_titlebar {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::winit_030::{WinitWindowAccessor, winit};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
        DwmSetWindowAttribute,
    };

    fn rgb(red: u8, green: u8, blue: u8) -> u32 {
        u32::from(red) | (u32::from(green) << 8) | (u32::from(blue) << 16)
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

    /// Applies both the Win32 dark-mode flag and explicit caption colors.
    ///
    /// The explicit colors keep the title bar in sync on Windows builds where
    /// the immersive flag alone changes button glyphs but leaves the caption
    /// background untouched.
    pub fn apply(window: &slint::Window, theme_selection: i32) {
        let _ = window.with_winit_window(|winit_window| {
            let system_dark = matches!(winit_window.theme(), Some(winit::window::Theme::Dark));
            let dark = match theme_selection {
                2 => true,
                1 => false,
                _ => system_dark,
            };

            winit_window.set_theme(Some(if dark {
                winit::window::Theme::Dark
            } else {
                winit::window::Theme::Light
            }));

            let Some(hwnd) = hwnd_from_window(winit_window) else {
                return;
            };

            let immersive_dark = i32::from(dark);
            let caption_color = if dark {
                rgb(31, 33, 36)
            } else {
                rgb(248, 249, 250)
            };
            let text_color = if dark {
                rgb(245, 247, 250)
            } else {
                rgb(31, 35, 40)
            };
            let border_color = if dark {
                rgb(58, 61, 66)
            } else {
                rgb(218, 222, 227)
            };

            // These attributes are supported on current Windows 10/11. Older
            // builds simply return an error, which is intentionally ignored.
            unsafe {
                let _ = DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_USE_IMMERSIVE_DARK_MODE,
                    (&immersive_dark as *const i32).cast(),
                    std::mem::size_of_val(&immersive_dark) as u32,
                );
                let _ = DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_CAPTION_COLOR,
                    (&caption_color as *const u32).cast(),
                    std::mem::size_of_val(&caption_color) as u32,
                );
                let _ = DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_TEXT_COLOR,
                    (&text_color as *const u32).cast(),
                    std::mem::size_of_val(&text_color) as u32,
                );
                let _ = DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_BORDER_COLOR,
                    (&border_color as *const u32).cast(),
                    std::mem::size_of_val(&border_color) as u32,
                );
            }
        });
    }
}

#[cfg(windows)]
pub fn apply_window_theme(window: &slint::Window, theme_selection: i32) {
    windows_titlebar::apply(window, theme_selection);
}

#[cfg(not(windows))]
pub fn apply_window_theme(_window: &slint::Window, _theme_selection: i32) {}
