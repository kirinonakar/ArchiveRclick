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
    /// Windows code page used to decode legacy archive pathnames. Zero keeps
    /// the automatic detector.
    pub pathname_codepage: u32,
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
            .field("pathname_codepage", &self.pathname_codepage)
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
            pathname_codepage: 0,
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
    Six,
    Eight,
    Ten,
    Sixteen,
    All,
}

/// Volume sizes offered by the create dialog.  The values use the same
/// binary units as 7-Zip's volume-size input (1 GB = 1024^3 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeSizePreset {
    None,
    TenMb,
    FiftyMb,
    OneHundredMb,
    ThreeHundredMb,
    SixHundredFiftyMb,
    OneGb,
    FourGb,
    TwentyThreeGb,
    NinetyTwoPointFourGb,
}

impl VolumeSizePreset {
    pub const ALL: [Self; 10] = [
        Self::None,
        Self::TenMb,
        Self::FiftyMb,
        Self::OneHundredMb,
        Self::ThreeHundredMb,
        Self::SixHundredFiftyMb,
        Self::OneGb,
        Self::FourGb,
        Self::TwentyThreeGb,
        Self::NinetyTwoPointFourGb,
    ];

    pub fn bytes(self) -> Option<u64> {
        const MB: u64 = 1024 * 1024;
        const GB: u64 = 1024 * 1024 * 1024;

        match self {
            Self::None => None,
            Self::TenMb => Some(10 * MB),
            Self::FiftyMb => Some(50 * MB),
            Self::OneHundredMb => Some(100 * MB),
            Self::ThreeHundredMb => Some(300 * MB),
            Self::SixHundredFiftyMb => Some(650 * MB),
            Self::OneGb => Some(GB),
            Self::FourGb => Some(4 * GB),
            Self::TwentyThreeGb => Some(23 * GB),
            // 92.4 GiB, rounded to the nearest byte.  This corresponds to
            // the familiar 100 GB optical-media capacity label.
            Self::NinetyTwoPointFourGb => Some(99_213_744_538),
        }
    }

    pub fn from_ui_index(index: i32) -> Self {
        Self::ALL
            .get(index.max(0) as usize)
            .copied()
            .unwrap_or(Self::None)
    }

    pub fn ui_index(self) -> i32 {
        Self::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(0) as i32
    }
}

impl ThreadCount {
    pub const ALL: [Self; 7] = [
        Self::Auto,
        Self::Four,
        Self::Six,
        Self::Eight,
        Self::Ten,
        Self::Sixteen,
        Self::All,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Four => "4",
            Self::Six => "6",
            Self::Eight => "8",
            Self::Ten => "10",
            Self::Sixteen => "16",
            Self::All => "All",
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
            Self::Six => "6",
            Self::Eight => "8",
            Self::Ten => "10",
            Self::Sixteen => "16",
            Self::All => "all",
        }
    }

    pub fn from_registry_key(key: &str) -> Self {
        match key {
            "4" => Self::Four,
            "6" => Self::Six,
            "8" => Self::Eight,
            "10" => Self::Ten,
            "16" => Self::Sixteen,
            "all" => Self::All,
            _ => Self::Auto,
        }
    }

    /// Number of codec worker threads requested from 7z.dll. Automatic and
    /// "All" explicitly resolve to the process-visible logical CPU count so
    /// ZIP follows the same `-mmt=on` behavior as the 7-Zip CLI.
    pub fn sevenzip_threads(self) -> Option<u32> {
        match self {
            Self::Auto | Self::All => std::thread::available_parallelism()
                .ok()
                .and_then(|count| u32::try_from(count.get()).ok()),
            Self::Four => Some(4),
            Self::Six => Some(6),
            Self::Eight => Some(8),
            Self::Ten => Some(10),
            Self::Sixteen => Some(16),
        }
    }
}

#[derive(Clone)]
pub struct CreateOptions {
    pub format: CreateFormat,
    pub compression_level: u8,
    /// Optional maximum physical size of each output volume.  When set, the
    /// 7z backend writes `<archive>.<nnn>` parts through a multi-volume stream.
    pub split_size: Option<u64>,
    pub password: Option<String>,
    /// Encrypt 7z headers so filenames and directory names require the
    /// archive password to be listed.
    pub encrypt_headers: bool,
    pub threads: ThreadCount,
}

impl fmt::Debug for CreateOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CreateOptions")
            .field("format", &self.format)
            .field("compression_level", &self.compression_level)
            .field("split_size", &self.split_size)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("encrypt_headers", &self.encrypt_headers)
            .field("threads", &self.threads)
            .finish()
    }
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            format: CreateFormat::Zip,
            compression_level: 6,
            split_size: None,
            password: None,
            encrypt_headers: false,
            threads: ThreadCount::Auto,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CreateOptions, ExtractOptions, VolumeSizePreset};

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

    #[test]
    fn volume_size_presets_round_trip_through_ui_indices() {
        let expected = [
            None,
            Some(10 * 1024 * 1024),
            Some(50 * 1024 * 1024),
            Some(100 * 1024 * 1024),
            Some(300 * 1024 * 1024),
            Some(650 * 1024 * 1024),
            Some(1024 * 1024 * 1024),
            Some(4 * 1024 * 1024 * 1024),
            Some(23 * 1024 * 1024 * 1024),
            Some(99_213_744_538),
        ];

        for (index, expected_bytes) in expected.into_iter().enumerate() {
            let preset = VolumeSizePreset::from_ui_index(index as i32);
            assert_eq!(preset.bytes(), expected_bytes);
            assert_eq!(preset.ui_index(), index as i32);
        }
    }
}
