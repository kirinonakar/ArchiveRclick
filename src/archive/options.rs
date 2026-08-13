use std::{
    fmt,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitialConflictPolicy {
    Ask,
    OverwriteAll,
    SkipAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictChoice {
    Overwrite,
    Skip,
    OverwriteAll,
    SkipAll,
    Cancel,
}

#[derive(Debug, Clone, Default)]
pub enum ExtractSelection {
    #[default]
    All,
    Paths(Vec<PathBuf>),
}

impl ExtractSelection {
    pub fn includes(&self, candidate: &Path) -> bool {
        match self {
            Self::All => true,
            Self::Paths(paths) => paths
                .iter()
                .any(|selected| candidate == selected || candidate.starts_with(selected)),
        }
    }
}

#[derive(Clone)]
pub struct ExtractOptions {
    pub selection: ExtractSelection,
    pub password: Option<String>,
    pub conflict_policy: InitialConflictPolicy,
    /// Optional selected-entry total from an already-loaded listing.
    pub total_entries_hint: Option<u64>,
    /// Optional selected uncompressed-byte total from an already-loaded listing.
    pub total_bytes_hint: Option<u64>,
    pub max_entries: u64,
    pub max_total_bytes: u64,
    pub max_file_bytes: u64,
}

impl fmt::Debug for ExtractOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExtractOptions")
            .field("selection", &self.selection)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("conflict_policy", &self.conflict_policy)
            .field("total_entries_hint", &self.total_entries_hint)
            .field("total_bytes_hint", &self.total_bytes_hint)
            .field("max_entries", &self.max_entries)
            .field("max_total_bytes", &self.max_total_bytes)
            .field("max_file_bytes", &self.max_file_bytes)
            .finish()
    }
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            selection: ExtractSelection::All,
            password: None,
            conflict_policy: InitialConflictPolicy::Ask,
            total_entries_hint: None,
            total_bytes_hint: None,
            max_entries: 1_000_000,
            max_total_bytes: 512 * 1024 * 1024 * 1024,
            max_file_bytes: 128 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateFormat {
    Zip,
    SevenZip,
    Tar,
    TarGzip,
    TarXz,
    TarZstd,
}

impl CreateFormat {
    pub const ALL: [Self; 6] = [
        Self::Zip,
        Self::SevenZip,
        Self::Tar,
        Self::TarGzip,
        Self::TarXz,
        Self::TarZstd,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Zip => "ZIP",
            Self::SevenZip => "7z",
            Self::Tar => "TAR",
            Self::TarGzip => "TAR.GZ",
            Self::TarXz => "TAR.XZ",
            Self::TarZstd => "TAR.ZST",
        }
    }

    pub fn default_extension(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::SevenZip => "7z",
            Self::Tar => "tar",
            Self::TarGzip => "tar.gz",
            Self::TarXz => "tar.xz",
            Self::TarZstd => "tar.zst",
        }
    }

    pub fn from_ui_index(index: i32) -> Self {
        Self::ALL
            .get(index.max(0) as usize)
            .copied()
            .unwrap_or(Self::Zip)
    }
}

/// CPU thread count used by the 7z backend when it compresses LZMA2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadCount {
    Auto,
    Four,
    Eight,
    Sixteen,
    All,
}

impl ThreadCount {
    pub const ALL: [Self; 5] = [
        Self::Auto,
        Self::Four,
        Self::Eight,
        Self::Sixteen,
        Self::All,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "자동",
            Self::Four => "4",
            Self::Eight => "8",
            Self::Sixteen => "16",
            Self::All => "전체",
        }
    }

    pub fn from_ui_index(index: i32) -> Self {
        Self::ALL
            .get(index.max(0) as usize)
            .copied()
            .unwrap_or(Self::Auto)
    }

    pub fn ui_index(self) -> i32 {
        Self::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(0) as i32
    }

    pub fn registry_key(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Four => "4",
            Self::Eight => "8",
            Self::Sixteen => "16",
            Self::All => "all",
        }
    }

    pub fn from_registry_key(key: &str) -> Self {
        match key {
            "4" => Self::Four,
            "8" => Self::Eight,
            "16" => Self::Sixteen,
            "all" => Self::All,
            _ => Self::Auto,
        }
    }

    /// Number of LZMA2 worker threads requested from 7z.dll. `None` lets
    /// 7-Zip pick its default (all logical processors), which is what both
    /// "자동" and "전체" resolve to.
    pub fn sevenzip_threads(self) -> Option<u32> {
        match self {
            Self::Auto | Self::All => None,
            Self::Four => Some(4),
            Self::Eight => Some(8),
            Self::Sixteen => Some(16),
        }
    }
}

#[derive(Clone)]
pub struct CreateOptions {
    pub format: CreateFormat,
    pub compression_level: u8,
    pub password: Option<String>,
    pub threads: ThreadCount,
}

impl fmt::Debug for CreateOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CreateOptions")
            .field("format", &self.format)
            .field("compression_level", &self.compression_level)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("threads", &self.threads)
            .finish()
    }
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            format: CreateFormat::Zip,
            compression_level: 6,
            password: None,
            threads: ThreadCount::Auto,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CreateOptions, ExtractOptions};

    #[test]
    fn extract_options_debug_redacts_password() {
        let secret = "extract-password-that-must-not-appear";
        let options = ExtractOptions {
            password: Some(secret.to_owned()),
            ..ExtractOptions::default()
        };

        let rendered = format!("{options:?}");
        assert!(!rendered.contains(secret));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn create_options_debug_redacts_password() {
        let secret = "create-password-that-must-not-appear";
        let options = CreateOptions {
            password: Some(secret.to_owned()),
            ..CreateOptions::default()
        };

        let rendered = format!("{options:?}");
        assert!(!rendered.contains(secret));
        assert!(rendered.contains("<redacted>"));
    }
}
