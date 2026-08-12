use std::path::{Component, Path, PathBuf};

use super::{ArchiveError, ArchiveResult};

pub(crate) fn safe_relative_path(path: &Path) -> ArchiveResult<PathBuf> {
    let display = path.to_string_lossy();
    if display.is_empty() {
        return Err(ArchiveError::UnsafeEntryPath(display.into_owned()));
    }

    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                validate_windows_component(value.to_string_lossy().as_ref())?;
                result.push(value);
            }
            Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                return Err(ArchiveError::UnsafeEntryPath(display.into_owned()));
            }
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
        return Err(ArchiveError::UnsafeEntryPath(component.to_owned()));
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
