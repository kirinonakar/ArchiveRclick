use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("libarchive could not be loaded: {0}")]
    LibraryUnavailable(String),

    #[error("the installed libarchive is missing the required symbol {0}")]
    MissingSymbol(&'static str),

    #[error("7-Zip (7z.dll) error: {0}")]
    SevenZip(String),

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

    #[error("File is not a supported archive, or the archive is damaged: {0}")]
    InvalidArchive(PathBuf),

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

    /// Returns true for failures that are commonly fixed by running the
    /// operation with a Windows administrator token. Archive libraries often
    /// surface access-denied as text instead of preserving the I/O error kind.
    pub fn requires_elevation(&self) -> bool {
        match self {
            Self::Io { source, .. } => {
                source.kind() == std::io::ErrorKind::PermissionDenied
                    || matches!(source.raw_os_error(), Some(5 | 1314))
            }
            Self::LibArchive { message, .. } | Self::SevenZip(message) => {
                let message = message.to_ascii_lowercase();
                [
                    "access is denied",
                    "access denied",
                    "permission denied",
                    "eacces",
                    "eperm",
                    "operation not permitted",
                    "0x80070005",
                    "error 5",
                    "error 13",
                    "errno 13",
                ]
                .iter()
                .any(|marker| message.contains(marker))
            }
            _ => false,
        }
    }
}

pub type ArchiveResult<T> = Result<T, ArchiveError>;

#[cfg(test)]
mod tests {
    use super::ArchiveError;

    #[test]
    fn detects_windows_permission_errors() {
        assert!(ArchiveError::io(
            "C:\\Windows",
            std::io::Error::from_raw_os_error(5),
        )
        .requires_elevation());
        assert!(ArchiveError::io(
            "C:\\Windows",
            std::io::Error::from_raw_os_error(1314),
        )
        .requires_elevation());
        assert!(!ArchiveError::io(
            "C:\\missing",
            std::io::Error::from_raw_os_error(2),
        )
        .requires_elevation());
    }

    #[test]
    fn detects_permission_text_from_archive_backends() {
        let libarchive = ArchiveError::LibArchive {
            operation: "opening archive",
            message: "error 13".to_owned(),
        };
        assert!(libarchive.requires_elevation());

        let seven_zip = ArchiveError::SevenZip("0x80070005: access denied".to_owned());
        assert!(seven_zip.requires_elevation());
    }
}
