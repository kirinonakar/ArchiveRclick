use std::path::{Path, PathBuf};

use crate::tasks::{CancellationToken, ProgressSnapshot};

use super::{
    ArchiveListing, ArchiveResult, ConflictChoice, CreateOptions, ExtractOptions, OperationSummary,
};

pub trait ProgressSink: Send + Sync {
    fn report(&self, progress: ProgressSnapshot);
}

impl<F> ProgressSink for F
where
    F: Fn(ProgressSnapshot) + Send + Sync,
{
    fn report(&self, progress: ProgressSnapshot) {
        self(progress);
    }
}

pub trait ConflictResolver: Send + Sync {
    fn resolve(&self, destination: &Path) -> ConflictChoice;
}

pub trait ArchiveEngine: Send + Sync {
    fn version(&self) -> String;

    fn writable_formats(&self) -> Vec<super::CreateFormat>;

    fn list(
        &self,
        path: &Path,
        password: Option<&str>,
        pathname_codepage: u32,
        progress: &dyn ProgressSink,
        cancel: &CancellationToken,
    ) -> ArchiveResult<ArchiveListing>;

    /// Lists one archive directory.  Backends with a cheap metadata-only
    /// reader can override this to avoid loading the whole archive just to
    /// render the current folder.
    fn list_directory(
        &self,
        path: &Path,
        directory: &Path,
        password: Option<&str>,
        pathname_codepage: u32,
        progress: &dyn ProgressSink,
        cancel: &CancellationToken,
    ) -> ArchiveResult<ArchiveListing> {
        self.list(path, password, pathname_codepage, progress, cancel)
            .map(|listing| listing.restrict_to_directory(directory))
    }

    fn extract(
        &self,
        archive: &Path,
        destination: &Path,
        options: &ExtractOptions,
        progress: &dyn ProgressSink,
        conflicts: &dyn ConflictResolver,
        cancel: &CancellationToken,
    ) -> ArchiveResult<OperationSummary>;

    fn create(
        &self,
        destination: &Path,
        files: &[PathBuf],
        options: &CreateOptions,
        progress: &dyn ProgressSink,
        cancel: &CancellationToken,
    ) -> ArchiveResult<OperationSummary>;

    fn test(
        &self,
        archive: &Path,
        password: Option<&str>,
        progress: &dyn ProgressSink,
        cancel: &CancellationToken,
    ) -> ArchiveResult<OperationSummary>;
}
