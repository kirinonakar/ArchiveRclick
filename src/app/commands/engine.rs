//! Construction and UI capability filtering for archive engines.

use super::*;

/// Builds the shared archive engine: ZIP, 7z, LZH, RAR, ISO, and NSIS are handled by
/// the bundled 7z.dll when available, while libarchive remains the fallback
/// for other formats. When 7z.dll cannot be loaded, the composite still serves
/// libarchive formats and 7z-specific operations fail with a clear error.
pub(super) fn load_engine() -> Result<Engine, String> {
    let libarchive = LibArchiveEngine::load().map_err(|error| error.to_string())?;
    let sevenzip = match SevenZipEngine::load() {
        Ok(engine) => Some(engine),
        Err(error) => {
            eprintln!("7z.dll unavailable; 7z archives will not open: {error}");
            None
        }
    };
    Ok(Arc::new(CompositeEngine::new(libarchive, sevenzip)))
}

pub(super) fn create_formats_for_ui(formats: Vec<CreateFormat>) -> Vec<CreateFormat> {
    formats
        .into_iter()
        .filter(|format| matches!(*format, CreateFormat::Zip | CreateFormat::SevenZip))
        .collect()
}
