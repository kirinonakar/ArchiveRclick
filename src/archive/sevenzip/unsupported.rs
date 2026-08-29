//! Non-Windows fallback that keeps libarchive routing available.

use std::path::{Path, PathBuf};

use crate::tasks::{CancellationToken, ProgressSnapshot};

use super::super::libarchive::LibArchiveEngine;
use super::super::{
    ArchiveEngine, ArchiveError, ArchiveListing, ArchiveResult, ConflictResolver, CreateOptions,
    ExtractOptions, OperationSummary, ProgressSink,
};

#[derive(Clone, Default)]
pub struct SevenZipEngine;

impl SevenZipEngine {
    pub fn load() -> ArchiveResult<Self> {
        Err(unavailable())
    }

    pub fn load_from_path(_path: &Path) -> ArchiveResult<Self> {
        Err(unavailable())
    }
}

fn unavailable() -> ArchiveError {
    ArchiveError::LibraryUnavailable(
        "the dynamic 7z.dll backend is currently Windows-only".to_owned(),
    )
}

impl ArchiveEngine for SevenZipEngine {
    fn version(&self) -> String {
        "7z.dll unavailable".to_owned()
    }

    fn writable_formats(&self) -> Vec<super::super::CreateFormat> {
        Vec::new()
    }

    fn list(
        &self,
        _path: &Path,
        _password: Option<&str>,
        _pathname_codepage: u32,
        _progress: &dyn ProgressSink,
        _cancel: &CancellationToken,
    ) -> ArchiveResult<ArchiveListing> {
        Err(unavailable())
    }

    fn extract(
        &self,
        _archive: &Path,
        _destination: &Path,
        _options: &ExtractOptions,
        _progress: &dyn ProgressSink,
        _conflicts: &dyn ConflictResolver,
        _cancel: &CancellationToken,
    ) -> ArchiveResult<OperationSummary> {
        Err(unavailable())
    }

    fn create(
        &self,
        _destination: &Path,
        _files: &[PathBuf],
        _options: &CreateOptions,
        _progress: &dyn ProgressSink,
        _cancel: &CancellationToken,
    ) -> ArchiveResult<OperationSummary> {
        Err(unavailable())
    }

    fn test(
        &self,
        _archive: &Path,
        _password: Option<&str>,
        _progress: &dyn ProgressSink,
        _cancel: &CancellationToken,
    ) -> ArchiveResult<OperationSummary> {
        Err(unavailable())
    }
}

pub struct CompositeEngine {
    libarchive: LibArchiveEngine,
    _sevenzip: Option<SevenZipEngine>,
}

impl CompositeEngine {
    pub fn new(libarchive: LibArchiveEngine, sevenzip: Option<SevenZipEngine>) -> Self {
        Self {
            libarchive,
            _sevenzip: sevenzip,
        }
    }
}

impl ArchiveEngine for CompositeEngine {
    fn version(&self) -> String {
        self.libarchive.version()
    }

    fn writable_formats(&self) -> Vec<super::super::CreateFormat> {
        self.libarchive.writable_formats()
    }

    fn list(
        &self,
        path: &Path,
        password: Option<&str>,
        pathname_codepage: u32,
        progress: &dyn ProgressSink,
        cancel: &CancellationToken,
    ) -> ArchiveResult<ArchiveListing> {
        self.libarchive
            .list(path, password, pathname_codepage, progress, cancel)
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
        self.libarchive
            .create(destination, files, options, progress, cancel)
    }

    fn test(
        &self,
        archive: &Path,
        password: Option<&str>,
        progress: &dyn ProgressSink,
        cancel: &CancellationToken,
    ) -> ArchiveResult<OperationSummary> {
        self.libarchive.test(archive, password, progress, cancel)
    }
}

#[allow(dead_code)]
fn _assert_progress_type(_: ProgressSnapshot) {}
