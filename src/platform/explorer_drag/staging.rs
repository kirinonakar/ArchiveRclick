use std::{fs, path::Path, time::Duration};

pub const STAGING_PREFIX: &str = "ArchiveRclick-drag-";

/// TempDir cleanup, including short-lived antivirus/file-indexer sharing locks.
/// Always called by the extraction/drag worker, never by the UI thread.
pub fn cleanup_staging(directory: tempfile::TempDir) {
    let path = directory.path().to_owned();
    if directory.close().is_ok() {
        return;
    }
    for delay in [25, 50, 100, 200, 400] {
        std::thread::sleep(Duration::from_millis(delay));
        match fs::remove_dir_all(&path) {
            Ok(()) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => {}
        }
    }
    crate::diagnostics::log(&format!(
        "Could not remove drag staging directory {}",
        path.display()
    ));
}

pub fn cleanup_stale_staging() {
    let _ = std::thread::Builder::new()
        .name("archive-drag-cleanup".into())
        .spawn(|| {
            cleanup_at(&std::env::temp_dir(), process_may_be_alive);
        });
}

#[cfg(windows)]
fn process_may_be_alive(id: u32) -> bool {
    use windows::Win32::{
        Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, STILL_ACTIVE},
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, id) } {
        Ok(handle) => {
            let mut code = STILL_ACTIVE.0 as u32;
            let result = unsafe { GetExitCodeProcess(handle, &mut code) };
            let _ = unsafe { CloseHandle(handle) };
            result.is_err() || code == STILL_ACTIVE.0 as u32
        }
        Err(error) => error.code() != windows::core::HRESULT::from_win32(ERROR_INVALID_PARAMETER.0),
    }
}
#[cfg(not(windows))]
fn process_may_be_alive(_: u32) -> bool {
    true
}

fn owner(name: &str) -> Option<u32> {
    let (process, suffix) = name.strip_prefix(STAGING_PREFIX)?.split_once('-')?;
    if suffix.is_empty()
        || !suffix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    process.parse().ok()
}

fn cleanup_at(root: &Path, alive: impl Fn(u32) -> bool) -> usize {
    let Ok(root) = fs::canonicalize(root) else {
        return 0;
    };
    let Ok(entries) = fs::read_dir(&root) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        // Restrict recursive removal to direct, plain directories in this
        // resolved temp root. Never follow junctions or another app's names.
        if path.parent() != Some(root.as_path()) {
            continue;
        }
        let Some(process) = entry.file_name().to_str().and_then(owner) else {
            continue;
        };
        if alive(process) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                continue;
            }
        }
        match fs::remove_dir_all(&path) {
            Ok(()) => removed += 1,
            Err(error) => crate::diagnostics::log(&format!(
                "Could not remove abandoned drag folder {}: {error}",
                path.display()
            )),
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn removes_dead_owners_without_touching_live_or_unrelated_folders() {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "ArchiveRclick-drag-7656-36nvr9",
            "ArchiveRclick-drag-42-live",
            "ArchiveRclick-drag-unknown",
            "other-folder",
        ] {
            fs::create_dir(root.path().join(name)).unwrap();
            fs::write(
                root.path().join(name).join("payload.txt"),
                b"selected content",
            )
            .unwrap();
        }
        assert_eq!(cleanup_at(root.path(), |id| id == 42), 1);
        assert!(!root.path().join("ArchiveRclick-drag-7656-36nvr9").exists());
        for name in [
            "ArchiveRclick-drag-42-live",
            "ArchiveRclick-drag-unknown",
            "other-folder",
        ] {
            assert!(root.path().join(name).join("payload.txt").is_file());
        }
    }
    #[test]
    fn cleanup_removes_all_files_and_nested_directories() {
        let directory = tempfile::Builder::new()
            .prefix(STAGING_PREFIX)
            .tempdir()
            .unwrap();
        let path = directory.path().to_owned();
        fs::create_dir_all(path.join("folder/deep")).unwrap();
        fs::write(path.join("folder/deep/selected.txt"), b"payload").unwrap();
        cleanup_staging(directory);
        assert!(!path.exists());
    }
}
