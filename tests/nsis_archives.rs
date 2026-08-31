#![cfg(windows)]

use archive_rclick_core::{
    archive::{
        ArchiveEngine, ArchiveEntryKind, CompositeEngine, ConflictChoice, ConflictResolver,
        ExtractOptions, ExtractSelection, SevenZipEngine, libarchive::LibArchiveEngine,
    },
    tasks::{CancellationToken, ProgressSnapshot},
};
use std::{
    fs,
    path::{Path, PathBuf},
};

struct Overwrite;
impl ConflictResolver for Overwrite {
    fn resolve(&self, _: &Path) -> ConflictChoice {
        ConflictChoice::Overwrite
    }
}
fn quiet(_: ProgressSnapshot) {}

// Installer payloads may be proprietary, so the integration fixture is supplied
// explicitly and is never checked into the repository or executed.
#[test]
#[ignore = "set ARCHIVERCLICK_NSIS_FIXTURE to an NSIS installer (any extension)"]
fn nsis_list_test_and_extract() {
    let path = PathBuf::from(std::env::var_os("ARCHIVERCLICK_NSIS_FIXTURE").expect("fixture path"));
    let engine = CompositeEngine::new(
        LibArchiveEngine::load().unwrap(),
        Some(SevenZipEngine::load().unwrap()),
    );
    let cancel = CancellationToken::new();
    let listing = engine.list(&path, None, 0, &quiet, &cancel).unwrap();
    assert_eq!(listing.format_name, "NSIS");
    assert!(!listing.entries.is_empty());
    let root = engine
        .list_directory(&path, Path::new(""), None, 0, &quiet, &cancel)
        .unwrap();
    assert!(!root.entries.is_empty());
    engine.test(&path, None, &quiet, &cancel).unwrap();
    let output = std::env::temp_dir().join(format!("archive-rclick-nsis-{}", std::process::id()));
    assert!(!output.exists());
    let summary = engine
        .extract(
            &path,
            &output,
            &ExtractOptions::default(),
            &quiet,
            &Overwrite,
            &cancel,
        )
        .unwrap();
    assert_eq!(summary.entries_processed, listing.entries.len() as u64);
    for entry in &listing.entries {
        if entry.kind == ArchiveEntryKind::File {
            let metadata = fs::metadata(output.join(&entry.path)).unwrap();
            if let Some(size) = entry.size {
                assert_eq!(metadata.len(), size, "{}", entry.display_path);
            }
        }
    }
    // Exercise extraction of a late item in the solid stream as well.
    let entry = listing
        .entries
        .iter()
        .rev()
        .find(|entry| entry.kind == ArchiveEntryKind::File)
        .unwrap();
    let selected = output.join("selected");
    let summary = engine
        .extract(
            &path,
            &selected,
            &ExtractOptions {
                selection: ExtractSelection::Paths(vec![entry.path.clone()]),
                ..ExtractOptions::default()
            },
            &quiet,
            &Overwrite,
            &cancel,
        )
        .unwrap();
    assert_eq!(summary.entries_processed, 1);
    assert_eq!(
        fs::read(selected.join(&entry.path)).unwrap(),
        fs::read(output.join(&entry.path)).unwrap()
    );
    eprintln!(
        "Verified NSIS: {} entries, {} bytes",
        listing.entries.len(),
        listing.total_uncompressed_size
    );
    fs::remove_dir_all(output).unwrap();
}
