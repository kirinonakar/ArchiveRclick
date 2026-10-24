use std::path::{Path, PathBuf};

use super::{ArchiveError, ArchiveResult};

/// Conservative Windows case-insensitive collision check. Unicode uppercase
/// also folds sigma variants; expansions may serialize extra paths, which is
/// safe. Filesystem create/replace checks remain authoritative.
pub(crate) fn has_output_path_conflict<'a>(
    paths: impl IntoIterator<Item = (&'a Path, bool)>,
) -> bool {
    use std::collections::HashSet;
    let key = |path: &Path| path.to_string_lossy().replace('\\', "/").to_uppercase();
    let paths: Vec<_> = paths.into_iter().collect();
    let files: HashSet<_> = paths
        .iter()
        .filter(|(_, dir)| !dir)
        .map(|(p, _)| key(p))
        .collect();
    let mut seen = HashSet::new();
    paths.iter().any(|(path, _)| {
        !seen.insert(key(path))
            || path
                .ancestors()
                .skip(1)
                .any(|parent| files.contains(&key(parent)))
    })
}

pub(crate) fn safe_relative_path(path: &Path) -> ArchiveResult<PathBuf> {
    let relative = traversal_checked_path(path)?;
    for component in relative.to_string_lossy().split(['/', '\\']) {
        validate_windows_component(component)?;
    }
    Ok(relative)
}

pub(crate) fn traversal_checked_path(path: &Path) -> ArchiveResult<PathBuf> {
    let display = path.to_string_lossy();
    let bytes = display.as_bytes();
    if display.is_empty()
        || display.starts_with(['/', '\\'])
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
        || display
            .split(['/', '\\'])
            .any(|component| component == "..")
    {
        return Err(ArchiveError::UnsafeEntryPath(display.into_owned()));
    }

    let mut result = PathBuf::new();
    // Parse archive separators consistently on all hosts. A colon in a relative
    // name is incompatible (including drive-relative forms); it is never used
    // as an output path until safe_relative_path has rejected it.
    for component in display.split(['/', '\\']) {
        if !component.is_empty() && component != "." {
            result.push(component);
        }
    }

    if result.as_os_str().is_empty() {
        return Err(ArchiveError::UnsafeEntryPath(display.into_owned()));
    }
    Ok(result)
}

fn validate_windows_component(component: &str) -> ArchiveResult<()> {
    let invalid = component.is_empty()
        || component.ends_with([' ', '.'])
        || component
            .chars()
            .any(|ch| ch < '\u{20}' || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*'));

    let stem = component
        .trim_end_matches([' ', '.'])
        .split('.')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem.strip_prefix("COM").is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
        || stem.strip_prefix("LPT").is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });

    if invalid || reserved {
        return Err(ArchiveError::IncompatibleEntryPath(component.to_owned()));
    }
    Ok(())
}

pub(crate) fn ensure_no_reparse_ancestors(root: &Path, target: &Path) -> ArchiveResult<()> {
    match std::fs::symlink_metadata(root) {
        Ok(metadata) if is_reparse_point(&metadata) => {
            return Err(ArchiveError::ReparsePoint(root.to_path_buf()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ArchiveError::io(root, error)),
    }

    let relative = target
        .strip_prefix(root)
        .map_err(|_| ArchiveError::UnsafeEntryPath(target.display().to_string()))?;

    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(ArchiveError::io(&current, error)),
        };

        if is_reparse_point(&metadata) {
            return Err(ArchiveError::ReparsePoint(current));
        }
    }
    Ok(())
}

/// A regular file blocking a directory (including any parent), or a directory
/// blocking a file, is skipped without deleting the existing object.
pub(crate) fn has_type_conflict(
    root: &Path,
    target: &Path,
    directory: bool,
) -> ArchiveResult<bool> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| ArchiveError::UnsafeEntryPath(target.display().to_string()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if is_reparse_point(&metadata) {
                    return Err(ArchiveError::ReparsePoint(current));
                }
                let want_directory = current != target || directory;
                if metadata.is_dir() != want_directory {
                    return Ok(true);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => return Ok(true),
            Err(error) => return Err(ArchiveError::io(&current, error)),
        }
    }
    Ok(false)
}

#[cfg(windows)]
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use super::safe_relative_path;
    use std::path::{Path, PathBuf};

    #[test]
    fn parallel_paths_detect_unicode_duplicates_and_nonadjacent_parents() {
        let conflict = |paths: &[(&str, bool)]| {
            super::has_output_path_conflict(
                paths
                    .iter()
                    .map(|(path, directory)| (Path::new(path), *directory)),
            )
        };
        assert!(conflict(&[("Éa.txt", false), ("éa.txt", false)]));
        assert!(conflict(&[("Σ.txt", false), ("ς.txt", false)]));
        assert!(conflict(&[
            ("É", false),
            ("other", false),
            ("é/child", false)
        ]));
        assert!(conflict(&[("a", false), ("a-", false), ("a/b", false)]));
        assert!(!conflict(&[("É", true), ("é/child", false)]));
        assert!(!conflict(&[("Éa.txt", false), ("éb.txt", false)]));
    }

    #[test]
    fn accepts_normal_relative_paths() {
        assert_eq!(
            safe_relative_path(Path::new("folder/file.txt")).unwrap(),
            PathBuf::from("folder/file.txt")
        );
    }

    #[test]
    fn rejects_parent_and_absolute_paths() {
        assert!(safe_relative_path(Path::new("../escape.txt")).is_err());
        assert!(safe_relative_path(Path::new("C:\\Windows\\escape.txt")).is_err());
        assert!(safe_relative_path(Path::new("\\\\server\\share\\escape.txt")).is_err());
    }

    #[test]
    fn rejects_reserved_and_ambiguous_windows_names() {
        for path in [
            "CON",
            "aux.txt",
            "dir/NUL.bin",
            "name. ",
            "a:b",
            "star*",
            "COM¹",
            "com².txt",
            "dir/COM³.bin",
            "LPT¹",
            "lpt².txt",
            "dir/LPT³.bin",
        ] {
            assert!(safe_relative_path(Path::new(path)).is_err(), "{path}");
        }
    }
}
