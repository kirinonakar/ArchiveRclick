//! Routing between the native 7-Zip engine and libarchive fallback.

use super::*;

// Composite engine: 7z/ZIP/LZH/RAR/ISO/NSIS reads -> 7z.dll when available;
// libarchive handles all other formats and remains the fallback when the
// optional 7z DLL is unavailable.
// ------------------------------------------------------------------
pub struct CompositeEngine {
    libarchive: LibArchiveEngine,
    sevenzip: Option<SevenZipEngine>,
}

impl CompositeEngine {
    pub fn new(libarchive: LibArchiveEngine, sevenzip: Option<SevenZipEngine>) -> Self {
        Self {
            libarchive,
            sevenzip,
        }
    }

    fn read_engine(&self, format: Option<ReadFormat>) -> ArchiveResult<&dyn ArchiveEngine> {
        if let Some(format) = format {
            if let Some(sevenzip) = &self.sevenzip
                && sevenzip.can_read_format(format)
            {
                return Ok(sevenzip);
            }
            if format == ReadFormat::SpannedZip {
                return Err(ArchiveError::SevenZip(
                    "split ZIP archives require the bundled 7z.dll ZIP reader".to_owned(),
                ));
            }
        }
        Ok(&self.libarchive)
    }
}

impl ArchiveEngine for CompositeEngine {
    fn version(&self) -> String {
        match &self.sevenzip {
            Some(_) => format!("7-Zip (7z.dll) + {}", self.libarchive.version()),
            None => self.libarchive.version(),
        }
    }

    fn writable_formats(&self) -> Vec<CreateFormat> {
        let mut formats = self.libarchive.writable_formats();
        if let Some(sevenzip) = &self.sevenzip {
            for format in [CreateFormat::Zip, CreateFormat::SevenZip] {
                if sevenzip.can_create(format) && !formats.contains(&format) {
                    formats.push(format);
                }
            }
        }
        formats
    }

    fn list(
        &self,
        path: &Path,
        password: Option<&str>,
        pathname_codepage: u32,
        progress: &dyn ProgressSink,
        cancel: &CancellationToken,
    ) -> ArchiveResult<ArchiveListing> {
        self.read_engine(archive_format(path))?.list(
            path,
            password,
            pathname_codepage,
            progress,
            cancel,
        )
    }

    fn list_directory(
        &self,
        path: &Path,
        directory: &Path,
        password: Option<&str>,
        pathname_codepage: u32,
        progress: &dyn ProgressSink,
        cancel: &CancellationToken,
    ) -> ArchiveResult<ArchiveListing> {
        self.read_engine(archive_format(path))?.list_directory(
            path,
            directory,
            password,
            pathname_codepage,
            progress,
            cancel,
        )
    }

    fn extract(
        &self,
        archive: &Path,
        destination: &Path,
        options: &ExtractOptions,
        progress: &dyn ProgressSink,
        conflicts: &dyn ConflictResolver,
        cancel: &CancellationToken,
    ) -> ArchiveResult<OperationSummary> {
        let format = archive_format(archive);
        if options.zip_backend != crate::archive::ZipBackend::SevenZip
            && matches!(format, Some(ReadFormat::Zip | ReadFormat::ZipVolume))
            && let Some(summary) = super::zip_extract::extract(
                archive,
                destination,
                options,
                progress,
                conflicts,
                cancel,
            )?
        {
            return Ok(summary);
        }
        self.read_engine(format)?.extract(
            archive,
            destination,
            options,
            progress,
            conflicts,
            cancel,
        )
    }

    fn create(
        &self,
        destination: &Path,
        files: &[PathBuf],
        options: &CreateOptions,
        progress: &dyn ProgressSink,
        cancel: &CancellationToken,
    ) -> ArchiveResult<OperationSummary> {
        if options.format == CreateFormat::Zip
            && options.zip_backend != crate::archive::ZipBackend::SevenZip
        {
            return super::zip_flate::create(destination, files, options, progress, cancel);
        }
        if options.format == CreateFormat::SevenZip
            || (options.format == CreateFormat::Zip
                && self
                    .sevenzip
                    .as_ref()
                    .is_some_and(|engine| engine.can_create(CreateFormat::Zip)))
        {
            self.sevenzip
                .as_ref()
                .ok_or_else(sevenzip_unavailable)?
                .create(destination, files, options, progress, cancel)
        } else {
            self.libarchive
                .create(destination, files, options, progress, cancel)
        }
    }

    fn test(
        &self,
        archive: &Path,
        password: Option<&str>,
        progress: &dyn ProgressSink,
        cancel: &CancellationToken,
    ) -> ArchiveResult<OperationSummary> {
        self.read_engine(archive_format(archive))?
            .test(archive, password, progress, cancel)
    }
}

fn sevenzip_unavailable() -> ArchiveError {
    ArchiveError::LibraryUnavailable(
        "the bundled 7z.dll could not be loaded; 7z archives need it".to_owned(),
    )
}
