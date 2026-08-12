//! Operating-system integration used by the application UI.

pub mod associations;
pub mod drop_target;
pub mod settings;
pub mod shell;
pub mod shell_ext;
pub mod windows;

pub use associations::{register_file_associations, unregister_file_associations};
pub use drop_target::install_file_drop_handler;
pub use settings::{load_font_preference, resolve_font_family, save_font_preference};
pub use shell::reveal_in_explorer;
pub use windows::{pick_archive, pick_files, pick_folder, save_archive, show_error, show_info};
