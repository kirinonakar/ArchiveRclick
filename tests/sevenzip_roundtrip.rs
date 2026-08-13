#![cfg(windows)]

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use archive_rclick_core::{
    archive::{
        ArchiveEngine, ArchiveError, ConflictChoice, ConflictResolver, CreateFormat, CreateOptions,
        ExtractOptions, SevenZipEngine,
    },
    tasks::CancellationToken,
};

struct Work(PathBuf);

impl Work {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock precedes Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "archive-rclick-7z-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Overwrite;

impl ConflictResolver for Overwrite {
    fn resolve(&self, _destination: &Path) -> ConflictChoice {
        ConflictChoice::Overwrite
    }
}

fn quiet(_: archive_rclick_core::tasks::ProgressSnapshot) {}

fn load_engine() -> SevenZipEngine {
    let executable = std::env::current_exe().expect("locate test executable");
    let profile = executable
        .parent()
        .and_then(Path::parent)
        .expect("profile directory");
    SevenZipEngine::load_from_path(&profile.join("7z.dll")).expect("load bundled 7z.dll")
}

fn assert_no_archive_temporary_files(directory: &Path) {
    let leftovers = fs::read_dir(directory)
        .unwrap()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| name.contains("archive-rclick") || name.contains("archiverclick"))
        .collect::<Vec<_>>();
    assert!(
        leftovers.is_empty(),
        "temporary files remain: {leftovers:?}"
    );
}

#[test]
fn bundled_7z_create_extract_round_trip() {
    let work = Work::new();
    let input = work.0.join("payload");
    fs::create_dir_all(&input).unwrap();
    fs::write(input.join("hello.txt"), b"hello from 7z\n").unwrap();

    let engine = load_engine();
    let archive = work.0.join("payload.7z");
    let cancel = CancellationToken::new();
    engine
        .create(
            &archive,
            std::slice::from_ref(&input),
            &CreateOptions {
                format: CreateFormat::SevenZip,
                ..CreateOptions::default()
            },
            &quiet,
            &cancel,
        )
        .expect("create 7z archive");

    let output = work.0.join("out");
    engine
        .extract(
            &archive,
            &output,
            &ExtractOptions::default(),
            &quiet,
            &Overwrite,
            &cancel,
        )
        .expect("extract 7z archive");
    assert_eq!(
        fs::read(output.join("payload").join("hello.txt")).unwrap(),
        b"hello from 7z\n"
    );
}

#[test]
fn cancelled_7z_creation_removes_temporary_file() {
    let work = Work::new();
    let input = work.0.join("payload");
    fs::create_dir_all(&input).unwrap();
    fs::write(input.join("hello.txt"), b"cancel me\n").unwrap();

    let engine = load_engine();
    let archive = work.0.join("cancelled.7z");
    let cancel = CancellationToken::new();
    let cancel_from_progress = cancel.clone();
    let progress = move |_: archive_rclick_core::tasks::ProgressSnapshot| {
        cancel_from_progress.cancel();
    };
    let result = engine.create(
        &archive,
        std::slice::from_ref(&input),
        &CreateOptions {
            format: CreateFormat::SevenZip,
            ..CreateOptions::default()
        },
        &progress,
        &cancel,
    );

    assert!(matches!(result, Err(ArchiveError::Cancelled)));
    assert!(!archive.exists());
    assert_no_archive_temporary_files(&work.0);
}
