#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    archive_rclick_core::diagnostics::install_panic_hook();
    if let Err(error) = archive_rclick_core::app::run() {
        eprintln!("ArchiveRclick failed: {error:#}");
        archive_rclick_core::diagnostics::log(&format!("ArchiveRclick failed: {error:#}"));
        #[cfg(windows)]
        archive_rclick_core::platform::windows::show_error("ArchiveRclick", &error.to_string());
        std::process::exit(1);
    }
}
