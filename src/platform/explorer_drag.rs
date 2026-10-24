//! Virtual-file dragging: hover transfers metadata, while file contents are
//! rendered only after the user releases the mouse over an accepting target.
#[cfg(not(windows))]
use crate::tasks::CancellationToken;
use std::path::PathBuf;
#[cfg(not(windows))]
use std::sync::mpsc;

#[derive(Clone, Debug)]
pub struct DragFile {
    pub archive_path: PathBuf,
    pub name: PathBuf,
    pub size: Option<u64>,
    pub directory: bool,
    pub modified_unix_seconds: Option<i64>,
}
pub type PrepareFiles = Box<dyn FnOnce() -> Result<tempfile::TempDir, String> + Send>;
mod staging;
pub use staging::{STAGING_PREFIX, cleanup_staging, cleanup_stale_staging};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    is_drag_button_down, prepare_descriptors, source_zone_metadata, start_file_drag,
};

#[cfg(not(windows))]
pub fn prepare_descriptors(
    _files: &[DragFile],
    _cancel: &CancellationToken,
) -> Result<Vec<u8>, String> {
    Err("Explorer drag-and-drop is only available on Windows".to_owned())
}
#[cfg(not(windows))]
pub fn is_drag_button_down() -> bool {
    false
}

#[cfg(not(windows))]
pub fn source_zone_metadata(_archive: &std::path::Path) -> Result<Option<Vec<u8>>, String> {
    Ok(None)
}

#[cfg(not(windows))]
pub fn start_file_drag(
    _files: Vec<DragFile>,
    _descriptors: Vec<u8>,
    _origin: Option<Vec<u8>>,
    _prepare: PrepareFiles,
    _cancel: CancellationToken,
    _completion: mpsc::SyncSender<Result<bool, String>>,
) -> Result<(), String> {
    Err("Explorer drag-and-drop is only available on Windows".to_owned())
}
