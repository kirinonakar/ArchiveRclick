//! Shared selection and compatibility policy for every extraction backend.
use super::{
    ArchiveEntryKind, ArchiveError, ArchiveResult, ExtractOptions, OperationSummary,
    UnsupportedEntryPolicy,
    path_safety::{safe_relative_path, traversal_checked_path},
};
use std::path::{Path, PathBuf};

pub(crate) fn selected_path(
    path: &Path,
    kind: ArchiveEntryKind,
    options: &ExtractOptions,
    summary: &mut OperationSummary,
) -> ArchiveResult<Option<PathBuf>> {
    // Traversal is fatal even in an unselected or incompatible entry. Check the
    // entire raw path before looking at any Windows component restrictions.
    let relative = traversal_checked_path(path)?;
    if !options.selection.includes(&relative) {
        return Ok(None);
    }
    let relative = match safe_relative_path(path) {
        Ok(path) => path,
        Err(ArchiveError::IncompatibleEntryPath(_))
            if options.incompatible_names == UnsupportedEntryPolicy::Skip =>
        {
            summary.skip_entry("name is incompatible with Windows", path);
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if !matches!(kind, ArchiveEntryKind::File | ArchiveEntryKind::Directory) {
        if options.unsupported_entries == UnsupportedEntryPolicy::Fail {
            return Err(ArchiveError::UnsafeEntryType(path.display().to_string()));
        }
        summary.skip_entry("link or special file", path);
        return Ok(None);
    }
    Ok(Some(relative))
}
