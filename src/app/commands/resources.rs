//! Discovery of packaged shell and third-party resources.

use super::*;

pub(super) fn context_menu_state_text() -> &'static str {
    if platform::shell_ext::is_context_menu_managed_by_package() {
        if platform::shell_ext::is_context_menu_registered() {
            "Registered (MSIX)"
        } else {
            "Not registered (MSIX)"
        }
    } else if platform::shell_ext::is_context_menu_registered() {
        "Registered"
    } else {
        "Not registered"
    }
}

/// Locates the shell extension DLL that ships next to the app executable.
pub(super) fn context_menu_dll_path() -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not locate ArchiveRclick: {error}"))?;
    let dll = executable
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("archive_rclick_core.dll");
    if dll.is_file() {
        Ok(dll)
    } else {
        Err(format!(
            "The shell extension DLL was not found next to the app ({}).\nBuild or package ArchiveRclick and try again.",
            dll.display()
        ))
    }
}

/// Locates a bundled third-party document in portable, packaged, and Cargo
/// build output layouts.
fn third_party_file_path(file_name: &str, description: &str) -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not locate ArchiveRclick: {error}"))?;
    let directory = executable.parent().unwrap_or_else(|| Path::new("."));
    let candidates = [
        directory.join(file_name),
        directory.join("runtime").join(file_name),
    ];
    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .ok_or_else(|| {
            format!(
                "The bundled third-party {description} was not found next to the app. Expected {}.",
                candidates[0].display()
            )
        })
}

pub(super) fn third_party_notices_path() -> Result<PathBuf, String> {
    third_party_file_path("THIRD-PARTY-NOTICES.md", "notice")
}

pub(super) fn third_party_runtime_licenses_path() -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not locate ArchiveRclick: {error}"))?;
    let directory = executable.parent().unwrap_or_else(|| Path::new("."));
    let candidates = [
        directory.join("licenses"),
        directory.join("runtime").join("licenses"),
    ];
    candidates
        .iter()
        .find(|path| path.is_dir())
        .cloned()
        .ok_or_else(|| {
            format!(
                "The bundled runtime licenses were not found next to the app. Expected {}.",
                candidates[0].display()
            )
        })
}
