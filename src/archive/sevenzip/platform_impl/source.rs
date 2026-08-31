//! Filesystem traversal and source manifest construction for archive creation.

use super::*;

// ------------------------------------------------------------------
// Source collection for creation
// ------------------------------------------------------------------
fn is_thumbs_db_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("Thumbs.db")
}

pub(super) fn collect_sources(
    files: &[PathBuf],
    destination: &Path,
    cancel: &CancellationToken,
) -> ArchiveResult<(Vec<SourceItem>, u64)> {
    let mut items = Vec::new();
    let mut total_bytes = 0u64;
    for root in files {
        check_cancel(cancel)?;
        let canonical = fs::canonicalize(root).map_err(|error| ArchiveError::io(root, error))?;
        if same_windows_path(&canonical, destination) {
            continue;
        }
        let Some(base_name) = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            return Err(ArchiveError::InvalidInput(format!(
                "input has no file name: {}",
                root.display()
            )));
        };
        let metadata = fs::symlink_metadata(root).map_err(|error| ArchiveError::io(root, error))?;
        // Skip reparse-point inputs (symlinks/junctions).
        if is_reparse(&metadata) {
            continue;
        }
        if !metadata.is_dir() && is_thumbs_db_name(&base_name) {
            continue;
        }
        if metadata.is_dir() {
            let prefix = if files.len() == 1 { "" } else { &base_name };
            walk_directory(
                &canonical,
                prefix,
                destination,
                &mut items,
                &mut total_bytes,
                cancel,
            )?;
        } else {
            total_bytes = checked_add_with_limit(
                total_bytes,
                metadata.len(),
                MAX_LIST_DECLARED_BYTES,
                "7z creation input size",
            )?;
            items.push(SourceItem {
                source: root.clone(),
                archive_name: base_name,
                kind: SourceKind::File,
                size: metadata.len(),
                modified_unix_seconds: metadata_modified_seconds(&metadata),
            });
        }
    }
    // Deterministic order regardless of filesystem enumeration order.
    items.sort_by(|left, right| left.archive_name.cmp(&right.archive_name));
    Ok((items, total_bytes))
}

fn walk_directory(
    canonical_root: &Path,
    prefix: &str,
    destination: &Path,
    items: &mut Vec<SourceItem>,
    total_bytes: &mut u64,
    cancel: &CancellationToken,
) -> ArchiveResult<()> {
    let mut pending = vec![(prefix.to_owned(), canonical_root.to_path_buf())];
    while let Some((archive_prefix, directory)) = pending.pop() {
        check_cancel(cancel)?;
        let mut children: Vec<(String, PathBuf, fs::Metadata)> = Vec::new();
        let entries =
            fs::read_dir(&directory).map_err(|error| ArchiveError::io(&directory, error))?;
        for entry in entries {
            check_cancel(cancel)?;
            let entry = entry.map_err(|error| ArchiveError::io(&directory, error))?;
            let path = entry.path();
            // Windows directory enumeration already supplies this metadata.
            // DirEntry::metadata avoids an extra filesystem call per item and,
            // like symlink_metadata, does not follow links/reparse points.
            let metadata = entry
                .metadata()
                .map_err(|error| ArchiveError::io(&path, error))?;
            // Skip reparse points (symlinks/junctions): archiving them
            // through 7z.dll would dereference them on extraction.
            if is_reparse(&metadata) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            children.push((name, path, metadata));
        }
        children.sort_by(|left, right| left.0.cmp(&right.0));
        if !archive_prefix.is_empty() {
            items.push(SourceItem {
                source: directory.clone(),
                // The archive handler adds the directory separator based
                // on KPID_IS_DIR. Keeping it out of the callback property
                // also matches the path form used for child entries.
                archive_name: archive_prefix.clone(),
                kind: SourceKind::Directory,
                size: 0,
                modified_unix_seconds: None,
            });
        }
        for (name, path, metadata) in children {
            check_cancel(cancel)?;
            if same_windows_path(&path, destination) {
                continue;
            }
            if !metadata.is_dir() && is_thumbs_db_name(&name) {
                continue;
            }
            let child_archive_name = if archive_prefix.is_empty() {
                name.clone()
            } else {
                format!("{archive_prefix}/{name}")
            };
            if metadata.is_dir() {
                pending.push((child_archive_name, path));
            } else {
                *total_bytes = checked_add_with_limit(
                    *total_bytes,
                    metadata.len(),
                    MAX_LIST_DECLARED_BYTES,
                    "7z creation input size",
                )?;
                items.push(SourceItem {
                    source: path,
                    archive_name: child_archive_name,
                    kind: SourceKind::File,
                    size: metadata.len(),
                    modified_unix_seconds: metadata_modified_seconds(&metadata),
                });
            }
        }
    }
    Ok(())
}

fn metadata_modified_seconds(metadata: &fs::Metadata) -> Option<i64> {
    use std::os::windows::fs::MetadataExt;
    let filetime = i64::try_from(metadata.last_write_time()).ok()?;
    Some((filetime / 10_000_000).saturating_sub(FILETIME_EPOCH_SECONDS))
}
