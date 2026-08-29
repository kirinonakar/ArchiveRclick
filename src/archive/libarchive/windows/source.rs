//! Filesystem source discovery and archive-entry metadata collection.

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SourceKind {
    File,
    Directory,
}

pub(super) struct SourceItem {
    pub(super) source: PathBuf,
    pub(super) archive_name: String,
    pub(super) kind: SourceKind,
    pub(super) size: u64,
    pub(super) modified_unix_seconds: Option<i64>,
}

pub(super) fn collect_sources(
    files: &[PathBuf],
    destination: &Path,
    cancel: &CancellationToken,
) -> ArchiveResult<(Vec<SourceItem>, u64)> {
    let mut items = Vec::new();
    let mut names = HashSet::new();
    let mut total = 0u64;
    for source in files {
        check_cancel(cancel)?;
        let selected_metadata =
            fs::symlink_metadata(source).map_err(|error| ArchiveError::io(source, error))?;
        if is_reparse(&selected_metadata) {
            return Err(ArchiveError::UnsafeEntryType(format!(
                "input is a link or reparse point: {}",
                source.display()
            )));
        }
        let source = fs::canonicalize(source).map_err(|error| ArchiveError::io(source, error))?;
        if destination.starts_with(&source) {
            return Err(ArchiveError::InvalidInput(format!(
                "archive destination is inside selected input {}",
                source.display()
            )));
        }
        if source == destination {
            return Err(ArchiveError::InvalidInput(
                "archive cannot contain its own output".to_owned(),
            ));
        }
        let root_name = source.file_name().ok_or_else(|| {
            ArchiveError::InvalidInput(format!(
                "cannot derive an archive name for {}",
                source.display()
            ))
        })?;
        let root_name = root_name.to_str().ok_or_else(|| {
            ArchiveError::InvalidInput(format!(
                "input name is not valid Unicode: {}",
                source.display()
            ))
        })?;
        if selected_metadata.is_file() && is_thumbs_db_name(root_name) {
            continue;
        }
        let archive_name = if files.len() == 1 && selected_metadata.is_dir() {
            String::new()
        } else {
            root_name.to_owned()
        };
        collect_source(
            &source,
            archive_name,
            destination,
            cancel,
            &mut names,
            &mut items,
            &mut total,
        )?;
    }
    Ok((items, total))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn collect_source(
    source: &Path,
    archive_name: String,
    destination: &Path,
    cancel: &CancellationToken,
    names: &mut HashSet<String>,
    items: &mut Vec<SourceItem>,
    total: &mut u64,
) -> ArchiveResult<()> {
    check_cancel(cancel)?;
    if source == destination {
        return Err(ArchiveError::InvalidInput(
            "archive cannot contain its own output".to_owned(),
        ));
    }
    let metadata = fs::symlink_metadata(source).map_err(|error| ArchiveError::io(source, error))?;
    if is_reparse(&metadata) {
        return Err(ArchiveError::UnsafeEntryType(format!(
            "input is a link or reparse point: {}",
            source.display()
        )));
    }
    if !archive_name.is_empty() {
        let key = archive_name.to_lowercase();
        if !names.insert(key) {
            return Err(ArchiveError::InvalidInput(format!(
                "duplicate archive pathname {archive_name}"
            )));
        }
    }
    let modified_unix_seconds = metadata.modified().ok().and_then(system_time_seconds);
    if metadata.is_file() {
        let size = metadata.len();
        if size > i64::MAX as u64 {
            return Err(ArchiveError::LimitExceeded(format!(
                "{} is too large for libarchive",
                source.display()
            )));
        }
        *total = total
            .checked_add(size)
            .ok_or_else(|| ArchiveError::LimitExceeded("input byte count overflow".to_owned()))?;
        items.push(SourceItem {
            source: source.to_path_buf(),
            archive_name,
            kind: SourceKind::File,
            size,
            modified_unix_seconds,
        });
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(ArchiveError::UnsafeEntryType(format!(
            "unsupported input type: {}",
            source.display()
        )));
    }
    if !archive_name.is_empty() {
        items.push(SourceItem {
            source: source.to_path_buf(),
            archive_name: archive_name.clone(),
            kind: SourceKind::Directory,
            size: 0,
            modified_unix_seconds,
        });
    }
    let mut children = fs::read_dir(source)
        .map_err(|error| ArchiveError::io(source, error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ArchiveError::io(source, error))?;
    // `sort_by_key` rebuilds the lowercase allocation for every
    // comparison.  Directory enumeration can contain thousands of
    // siblings, so cache the key once per entry instead.
    children.sort_by_cached_key(|entry| entry.file_name().to_string_lossy().to_lowercase());
    for child in children {
        let child_name = child.file_name();
        let child_name = child_name.to_str().ok_or_else(|| {
            ArchiveError::InvalidInput(format!(
                "input name is not valid Unicode: {}",
                child.path().display()
            ))
        })?;
        if child_name.contains(['/', '\\', '\0']) {
            return Err(ArchiveError::InvalidInput(format!(
                "invalid input name {child_name:?}"
            )));
        }
        if !child.path().is_dir() && is_thumbs_db_name(child_name) {
            continue;
        }
        let child_archive_name = if archive_name.is_empty() {
            child_name.to_owned()
        } else {
            format!("{archive_name}/{child_name}")
        };
        collect_source(
            &child.path(),
            child_archive_name,
            destination,
            cancel,
            names,
            items,
            total,
        )?;
    }
    Ok(())
}

pub(super) fn is_thumbs_db_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("Thumbs.db")
}

pub(super) fn system_time_seconds(time: SystemTime) -> Option<i64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).ok(),
        Err(error) => i64::try_from(error.duration().as_secs())
            .ok()
            .and_then(|seconds| seconds.checked_neg()),
    }
}

pub(super) fn is_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}
