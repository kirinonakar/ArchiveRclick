//! Download-origin propagation, limited to the file types documented by Bandizip.
use super::{ArchiveError, ArchiveResult};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Default)]
pub(crate) struct ZoneIdentifier(Option<Arc<[u8]>>);

fn stream_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(":Zone.Identifier");
    name.into()
}

fn unsupported(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(1 | 50 | 123))
}

impl ZoneIdentifier {
    pub(crate) fn read(archive: &Path, enabled: bool) -> ArchiveResult<Self> {
        if !enabled {
            return Ok(Self::default());
        }
        let path = stream_path(archive);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound || unsupported(&error) => {
                return Ok(Self::default());
            }
            Err(error) => return Err(ArchiveError::io(&path, error)),
        };
        // ADS is untrusted input; origin metadata should be small.
        let mut data = Vec::new();
        file.take(65537)
            .read_to_end(&mut data)
            .map_err(|error| ArchiveError::io(&path, error))?;
        if data.len() > 65536 {
            return Err(ArchiveError::LimitExceeded(
                "Zone.Identifier exceeds 64 KiB".into(),
            ));
        }
        Ok(Self(Some(data.into())))
    }

    /// Write to the new/staged file before installation, using its final name
    /// for the extension filter. Skipped files never reach this method.
    pub(crate) fn apply(&self, output: &Path, target: &Path) -> ArchiveResult<()> {
        let Some(data) = &self.0 else {
            return Ok(());
        };
        let extension = target
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(
            extension.as_str(),
            "exe"
                | "com"
                | "msi"
                | "scr"
                | "bat"
                | "cmd"
                | "ps1"           
                | "pif"
                | "lnk"
                | "zip"
                | "zipx"
                | "rar"
                | "7z"
                | "alz"
                | "egg"
                | "cab"
                | "bh"
                | "iso"
                | "img"
                | "isz"
                | "udf"
                | "wim"
                | "bin"
                | "i00"
                | "js"
                | "jse"
                | "vbs"
                | "vbe"
                | "wsf"
                | "url"
                | "reg"
                | "docx"
                | "doc"
                | "xls"
                | "xlsx"
                | "ppt"
                | "pptx"
                | "hwp"
                | "hwpx"
                | "wiz"
        ) {
            return Ok(());
        }
        let path = stream_path(output);
        match fs::write(&path, data) {
            Ok(()) => Ok(()),
            Err(error) if unsupported(&error) => Ok(()),
            Err(error) => Err(ArchiveError::io(&path, error)),
        }
    }
}