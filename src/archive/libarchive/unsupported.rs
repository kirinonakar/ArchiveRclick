use std::path::{Path, PathBuf};

use crate::tasks::{CancellationToken, ProgressSnapshot};

use super::super::{
    ArchiveEngine, ArchiveError, ArchiveListing, ArchiveResult, ConflictResolver, CreateOptions,
    ExtractOptions, OperationSummary, ProgressSink,
};

#[derive(Clone, Default)]
pub struct LibArchiveEngine;

impl LibArchiveEngine {
    pub fn load() -> ArchiveResult<Self> {
        Err(unavailable())
    }

    pub fn load_from_path(_path: &Path) -> ArchiveResult<Self> {
        Err(unavailable())
    }
}

pub fn load() -> ArchiveResult<LibArchiveEngine> {
    LibArchiveEngine::load()
}

fn unavailable() -> ArchiveError {
    ArchiveError::LibraryUnavailable(
        "the dynamic libarchive backend is currently Windows-only".to_owned(),
    )
}

impl ArchiveEngine for LibArchiveEngine {
    fn version(&self) -> String {
        "libarchive unavailable".to_owned()
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

#[allow(dead_code)]
fn _assert_progress_type(_: ProgressSnapshot) {}
