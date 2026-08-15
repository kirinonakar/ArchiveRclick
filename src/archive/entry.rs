use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveEntryKind {
    File,
    Directory,
    Symlink,
    Hardlink,
    Other,
}

#[derive(Debug, Clone)]
pub struct ArchiveEntry {
    pub index: u64,
    pub path: PathBuf,
    pub display_path: String,
    pub size: Option<u64>,
    pub compressed_size: Option<u64>,
    pub modified_unix_seconds: Option<i64>,
    pub kind: ArchiveEntryKind,
    pub encrypted: bool,
}

#[derive(Debug, Clone)]
pub struct ArchiveListing {
    pub archive_path: PathBuf,
    pub format_name: String,
    pub filter_name: Option<String>,
    /// Non-fatal parser warning. A value means the listing contains the
    /// entries recovered before the archive became unreadable.
    pub warning: Option<String>,
    pub entries: Vec<ArchiveEntry>,
    pub total_uncompressed_size: u64,
}

impl ArchiveListing {
    /// Keeps the entries below one archive directory.  The archive reader is
    /// still responsible for opening and validating the source; this helper
    /// only narrows a complete listing for engines that do not support
    /// metadata-only directory scans.
    pub(crate) fn restrict_to_directory(mut self, directory: &Path) -> Self {
        let prefix = archive_path_components(directory);
        if prefix.is_empty() {
            return self;
        }

        let mut total_uncompressed_size = 0u64;
        let mut entries = Vec::new();
        for mut entry in self.entries {
            let components = archive_path_components(&entry.path);
            if components.len() < prefix.len() || components[..prefix.len()] != prefix[..] {
                continue;
            }
            total_uncompressed_size =
                total_uncompressed_size.saturating_add(entry.size.unwrap_or(0));
            entry.index = entries.len() as u64;
            entries.push(entry);
        }
        self.entries = entries;
        self.total_uncompressed_size = total_uncompressed_size;
        self
    }
}

fn archive_path_components(path: &Path) -> Vec<String> {
    path.to_string_lossy()
        .replace('\\', "/")
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .map(str::to_owned)
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct OperationSummary {
    pub entries_processed: u64,
    pub bytes_processed: u64,
    pub entries_skipped: u64,
    /// Non-fatal extraction warning. A value means the operation completed
    /// with the entries that could be recovered from a damaged archive.
    pub warning: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{ArchiveEntry, ArchiveEntryKind, ArchiveListing};
    use std::path::{Path, PathBuf};

    fn entry(index: u64, path: &str, size: u64) -> ArchiveEntry {
        ArchiveEntry {
            index,
            path: PathBuf::from(path),
            display_path: path.to_owned(),
            size: Some(size),
            compressed_size: None,
            modified_unix_seconds: None,
            kind: ArchiveEntryKind::File,
            encrypted: false,
        }
    }

    #[test]
    fn restrict_to_directory_keeps_descendants_and_reindexes_entries() {
        let listing = ArchiveListing {
            archive_path: PathBuf::from("sample.iso"),
            format_name: "ISO 9660".to_owned(),
            filter_name: None,
            warning: None,
            entries: vec![
                entry(0, "root.txt", 1),
                entry(1, "dir/a.txt", 2),
                entry(2, "dir/sub/b.txt", 4),
                entry(3, "other.txt", 8),
            ],
            total_uncompressed_size: 15,
        };

        let listing = listing.restrict_to_directory(Path::new("dir"));
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|entry| entry.display_path.as_str())
                .collect::<Vec<_>>(),
            vec!["dir/a.txt", "dir/sub/b.txt"]
        );
        assert_eq!(listing.entries[0].index, 0);
        assert_eq!(listing.entries[1].index, 1);
        assert_eq!(listing.total_uncompressed_size, 6);
    }
}
