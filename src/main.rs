#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(error) = archive_rclick_core::app::run() {
        eprintln!("ArchiveRclick failed: {error:#}");
        #[cfg(windows)]
        archive_rclick_core::platform::windows::show_error("ArchiveRclick", &error.to_string());
        std::process::exit(1);
    }
}
