#[cfg(windows)]
pub mod encoding;
mod engine;
mod entry;
mod error;
pub mod libarchive;
mod options;
mod path_safety;
pub mod sevenzip;

pub use engine::{ArchiveEngine, ConflictResolver, ProgressSink};
pub use entry::{ArchiveEntry, ArchiveEntryKind, ArchiveListing, OperationSummary};
pub use error::{ArchiveError, ArchiveResult};
pub use options::{
    ConflictChoice, CreateFormat, CreateOptions, ExtractOptions, ExtractSelection,
    InitialConflictPolicy, ThreadCount, VOLUME_CUSTOM_UI_INDEX, VolumeSizePreset,
    parse_volume_size,
};
pub(crate) use path_safety::{ensure_no_reparse_ancestors, safe_relative_path};
pub use sevenzip::{CompositeEngine, SevenZipEngine};
