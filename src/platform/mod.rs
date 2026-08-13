//! Operating-system integration used by the application UI.

pub mod associations;
pub mod drop_target;
pub mod settings;
pub mod shell;
#[cfg(windows)]
pub mod shell_ext;
pub mod titlebar;
#[cfg(not(windows))]
pub mod shell_ext {
    //! Fallback for platforms without Explorer shell integration.
    use std::path::Path;

    pub fn register_context_menu(_dll_path: &Path) -> Result<(), String> {
        Err("The Explorer right-click menu is only available on Windows".to_owned())
    }

    pub fn unregister_context_menu() -> Result<(), String> {
        Err("The Explorer right-click menu is only available on Windows".to_owned())
    }

    pub fn is_context_menu_registered() -> bool {
        false
    }
}
pub mod windows;

pub use associations::{register_file_associations, unregister_file_associations};
pub use drop_target::install_file_drop_handler;
pub use settings::{
    ColumnBoundaries, WindowGeometry, load_column_boundaries, load_font_preference,
    load_header_encryption_preference, load_language_preference, load_theme_preference,
    load_thread_preference, load_window_geometry, resolve_font_family, save_column_boundaries,
    save_font_preference, save_header_encryption_preference, save_language_preference,
    save_theme_preference, save_thread_preference, save_window_geometry,
};
pub use shell::reveal_in_explorer;
pub use titlebar::apply_window_theme;
pub use windows::{
    center_window, center_window_with_logical_size, pick_archive, pick_files, pick_folder,
    run_elevated, save_archive, show_error, show_info,
};
