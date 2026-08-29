//! Shared path validation, filesystem safety, and temporary-output handling.

use super::*;

pub(super) fn build_path(display: &str) -> ArchiveResult<PathBuf> {
    let mut path = PathBuf::new();
    for component in display.trim_end_matches(['/', '\\']).split(['/', '\\']) {
        if !component.is_empty() {
            path.push(component);
        }
    }
    if path.as_os_str().is_empty() {
        return Err(ArchiveError::UnsafeEntryPath(display.to_owned()));
    }
    Ok(path)
}

pub(super) fn check_cancel(cancel: &CancellationToken) -> ArchiveResult<()> {
    if cancel.is_cancelled() {
        Err(ArchiveError::Cancelled)
    } else {
        Ok(())
    }
}

pub(super) fn checked_add_with_limit(
    total: u64,
    add: u64,
    limit: u64,
    subject: &str,
) -> ArchiveResult<u64> {
    let next = total
        .checked_add(add)
        .ok_or_else(|| ArchiveError::LimitExceeded(format!("{subject} overflow")))?;
    if next > limit {
        return Err(ArchiveError::LimitExceeded(format!(
            "{subject} exceeds the configured limit"
        )));
    }
    Ok(next)
}

pub(super) fn file_length(path: &Path) -> ArchiveResult<u64> {
    if let Some(paths) = split_volume_paths(path) {
        let mut total = 0u64;
        for part in paths {
            let length = fs::metadata(&part)
                .map(|metadata| metadata.len())
                .map_err(|error| ArchiveError::io(&part, error))?;
            total = total.checked_add(length).ok_or_else(|| {
                ArchiveError::LimitExceeded("split archive size overflow".to_owned())
            })?;
        }
        Ok(total)
    } else {
        fs::metadata(path)
            .map(|metadata| metadata.len())
            .map_err(|error| ArchiveError::io(path, error))
    }
}

pub(super) fn is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

pub(super) fn same_windows_path(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

pub(super) fn stream_buffer_size(file_size: u64) -> usize {
    usize::try_from(file_size.min(STREAM_BUFFER_SIZE as u64))
        .unwrap_or(STREAM_BUFFER_SIZE)
        .max(MIN_STREAM_BUFFER_SIZE)
}

pub(super) fn temporary_path(parent: &Path, target: &Path) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("output");
    parent.join(format!(
        ".{name}.archive-rclick-{}-{nonce}.tmp",
        std::process::id()
    ))
}

/// Moves `temp` onto `target`, replacing an existing file when present.
pub(super) fn install_temporary(root: &Path, temp: &Path, target: &Path) -> ArchiveResult<()> {
    ensure_no_reparse_ancestors(root, target)?;
    match fs::rename(temp, target) {
        Ok(()) => Ok(()),
        Err(error)
            if error.kind() == std::io::ErrorKind::AlreadyExists
                || fs::symlink_metadata(target).is_ok() =>
        {
            fs::remove_file(target).map_err(|error| ArchiveError::io(target, error))?;
            fs::rename(temp, target).map_err(|error| ArchiveError::io(target, error))
        }
        Err(error) => Err(ArchiveError::io(target, error)),
    }
}

pub(super) fn install_temporary_volumes(
    root: &Path,
    temporary_paths: &[PathBuf],
    target: &Path,
) -> ArchiveResult<()> {
    let cleanup_installed = |paths: &[PathBuf]| {
        for path in paths {
            let _ = fs::remove_file(path);
        }
    };
    let mut installed = Vec::with_capacity(temporary_paths.len());
    for (index, temporary) in temporary_paths.iter().enumerate() {
        let index = u32::try_from(index + 1)
            .map_err(|_| ArchiveError::LimitExceeded("too many output volumes".to_owned()))?;
        let destination = volume_part_path(target, index);
        if let Err(error) = install_temporary(root, temporary, &destination) {
            cleanup_installed(&installed);
            return Err(error);
        }
        installed.push(destination);
    }

    // A previous archive may have had more parts. Remove only the
    // contiguous, conventionally named suffix after the newly installed
    // set so stale volumes cannot be mistaken for part of this archive.
    let mut index = u32::try_from(temporary_paths.len() + 1)
        .map_err(|_| ArchiveError::LimitExceeded("too many output volumes".to_owned()))?;
    while index <= u32::from(u16::MAX) {
        let stale = volume_part_path(target, index);
        match fs::symlink_metadata(&stale) {
            Ok(metadata) => {
                if is_reparse(&metadata) {
                    cleanup_installed(&installed);
                    return Err(ArchiveError::ReparsePoint(stale));
                }
                if let Err(error) = fs::remove_file(&stale) {
                    cleanup_installed(&installed);
                    return Err(ArchiveError::io(&stale, error));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                cleanup_installed(&installed);
                return Err(ArchiveError::io(&stale, error));
            }
        }
        index = index.saturating_add(1);
    }

    // A previous unsplit archive can have the same base name.  A split
    // result owns that name's volume set, so remove the obsolete base
    // file after all new parts have been installed successfully.
    match fs::symlink_metadata(target) {
        Ok(metadata) => {
            if is_reparse(&metadata) {
                cleanup_installed(&installed);
                return Err(ArchiveError::ReparsePoint(target.to_owned()));
            }
            if let Err(error) = fs::remove_file(target) {
                cleanup_installed(&installed);
                return Err(ArchiveError::io(target, error));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            cleanup_installed(&installed);
            return Err(ArchiveError::io(target, error));
        }
    }
    Ok(())
}
