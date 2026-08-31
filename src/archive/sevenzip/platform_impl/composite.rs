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
        if let Some(sevenzip) = &self.sevenzip {
            if archive_format(path).is_some_and(|format| sevenzip.can_read_format(format)) {
                return sevenzip.list(path, password, pathname_codepage, progress, cancel);
            }
        }
        self.libarchive
            .list(path, password, pathname_codepage, progress, cancel)
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
        if let Some(sevenzip) = &self.sevenzip {
            if archive_format(path).is_some_and(|format| sevenzip.can_read_format(format)) {
                return sevenzip.list_directory(
                    path,
                    directory,
                    password,
                    pathname_codepage,
                    progress,
                    cancel,
                );
            }
        }
        self.libarchive.list_directory(
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
        if let Some(sevenzip) = &self.sevenzip {
            if archive_format(archive).is_some_and(|format| sevenzip.can_read_format(format)) {
                return sevenzip.extract(
                    archive,
                    destination,
                    options,
                    progress,
                    conflicts,
                    cancel,
                );
            }
        }
        self.libarchive
            .extract(archive, destination, options, progress, conflicts, cancel)
    }

    fn create(
        &self,
        destination: &Path,
        files: &[PathBuf],
        options: &CreateOptions,
        progress: &dyn ProgressSink,
        cancel: &CancellationToken,
    ) -> ArchiveResult<OperationSummary> {
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
        if let Some(sevenzip) = &self.sevenzip {
            if archive_format(archive).is_some_and(|format| sevenzip.can_read_format(format)) {
                return sevenzip.test(archive, password, progress, cancel);
            }
        }
        self.libarchive.test(archive, password, progress, cancel)
    }
}

fn sevenzip_unavailable() -> ArchiveError {
    ArchiveError::LibraryUnavailable(
        "the bundled 7z.dll could not be loaded; 7z archives need it".to_owned(),
    )
}
