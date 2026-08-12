use std::path::PathBuf;

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
    pub entries: Vec<ArchiveEntry>,
    pub total_uncompressed_size: u64,
}

#[derive(Debug, Clone, Default)]
pub struct OperationSummary {
    pub entries_processed: u64,
    pub bytes_processed: u64,
    pub entries_skipped: u64,
}
