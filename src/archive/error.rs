use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("libarchive could not be loaded: {0}")]
    LibraryUnavailable(String),

    #[error("the installed libarchive is missing the required symbol {0}")]
    MissingSymbol(&'static str),

    #[error("libarchive error during {operation}: {message}")]
    LibArchive {
        operation: &'static str,
        message: String,
    },

    #[error("archive entry has an unsafe path: {0}")]
    UnsafeEntryPath(String),

    #[error("archive entry type is unsafe to extract: {0}")]
    UnsafeEntryType(String),

    #[error("extraction would cross a reparse point: {0}")]
    ReparsePoint(PathBuf),

    #[error("operation cancelled")]
    Cancelled,

    #[error("archive exceeds the configured safety limit: {0}")]
    LimitExceeded(String),

    #[error("password required or incorrect")]
    PasswordRequired,

    #[error("unsupported archive creation option: {0}")]
    UnsupportedOption(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("worker thread failed: {0}")]
    Worker(String),
}

impl ArchiveError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

pub type ArchiveResult<T> = Result<T, ArchiveError>;
