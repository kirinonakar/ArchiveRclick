#[cfg(windows)]
mod disk_space;
#[cfg(windows)]
pub mod encoding;
mod engine;
mod entry;
mod error;
mod extraction_policy;
pub mod libarchive;
mod options;
mod path_safety;
pub mod sevenzip;
#[cfg(windows)]
mod zone_identifier;

pub use engine::{ArchiveEngine, ConflictResolver, ProgressSink};
pub use entry::{ArchiveEntry, ArchiveEntryKind, ArchiveListing, OperationSummary};
pub use error::{ArchiveError, ArchiveResult};
pub use options::{
    ConflictChoice, CreateFormat, CreateOptions, ExtractOptions, ExtractSelection,
    InitialConflictPolicy, ThreadCount, UnsupportedEntryPolicy, VOLUME_CUSTOM_UI_INDEX,
    VolumeSizePreset, ZipBackend, parse_volume_size,
};
pub(crate) use path_safety::{ensure_no_reparse_ancestors, safe_relative_path};
pub use sevenzip::{CompositeEngine, SevenZipEngine};

pub(crate) fn spanned_zip_volume_number(extension: &str) -> Option<u32> {
    let digits = extension.strip_prefix(['z', 'Z'])?;
    if digits.len() < 2
        || (digits.len() > 2 && digits.starts_with('0'))
        || !digits.bytes().all(|digit| digit.is_ascii_digit())
    {
        return None;
    }
    digits.parse().ok().filter(|&number| number > 0)
}
