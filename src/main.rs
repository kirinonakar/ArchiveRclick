#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(error) = archive_rclick::app::run() {
        eprintln!("ArchiveRclick failed: {error:#}");
        #[cfg(windows)]
        archive_rclick::platform::windows::show_error("ArchiveRclick", &error.to_string());
        std::process::exit(1);
    }
}
