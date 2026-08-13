#![cfg(windows)]

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use archive_rclick_core::archive::ThreadCount;
use archive_rclick_core::{
    archive::{
        ArchiveEngine, ArchiveError, CompositeEngine, ConflictChoice, ConflictResolver,
        CreateFormat, CreateOptions, ExtractOptions, SevenZipEngine, libarchive::LibArchiveEngine,
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

struct CancelAfterResolve {
    cancel: CancellationToken,
}

impl ConflictResolver for CancelAfterResolve {
    fn resolve(&self, _destination: &Path) -> ConflictChoice {
        self.cancel.cancel();
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

fn load_composite() -> CompositeEngine {
    let executable = std::env::current_exe().expect("locate test executable");
    let profile = executable
        .parent()
        .and_then(Path::parent)
        .expect("profile directory");
    let libarchive = LibArchiveEngine::load_from_path(&profile.join("archive.dll"))
        .expect("load bundled archive.dll");
    CompositeEngine::new(libarchive, Some(load_engine()))
}

fn assert_no_archive_temporary_files(directory: &Path) {
    let mut pending = vec![directory.to_owned()];
    let mut leftovers = Vec::new();
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if entry.file_name().to_str().is_some_and(|name| {
                name.contains("archive-rclick") || name.contains("archiverclick")
            }) {
                leftovers.push(path);
            }
        }
    }
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
fn bundled_zip_create_extract_round_trip() {
    let work = Work::new();
    let input = work.0.join("payload");
    fs::create_dir_all(input.join("nested")).unwrap();
    fs::write(input.join("hello.txt"), b"hello from ZIP\n").unwrap();
    fs::write(input.join("nested").join("data.bin"), [0_u8, 1, 2, 3, 4]).unwrap();

    let engine = load_composite();
    let archive = work.0.join("payload.zip");
    let cancel = CancellationToken::new();
    let password = "zip-password";
    engine
        .create(
            &archive,
            std::slice::from_ref(&input),
            &CreateOptions {
                format: CreateFormat::Zip,
                password: Some(password.to_owned()),
                ..CreateOptions::default()
            },
            &quiet,
            &cancel,
        )
        .expect("create ZIP archive through 7z.dll");

    let output = work.0.join("out");
    engine
        .extract(
            &archive,
            &output,
            &ExtractOptions {
                password: Some(password.to_owned()),
                ..ExtractOptions::default()
            },
            &quiet,
            &Overwrite,
            &cancel,
        )
        .expect("extract ZIP archive through 7z.dll");
    assert_eq!(
        fs::read(output.join("payload").join("hello.txt")).unwrap(),
        b"hello from ZIP\n"
    );
    assert_eq!(
        fs::read(output.join("payload").join("nested").join("data.bin")).unwrap(),
        [0_u8, 1, 2, 3, 4]
    );
}

#[test]
fn legacy_zip_codepage_is_applied_by_7z_for_list_and_extract() {
    let work = Work::new();
    let archive = work.0.join("legacy-korean.zip");
    write_legacy_zip(&archive, b"\xC7\xD1\xB1\xDB.txt", b"legacy ZIP\n");

    let engine = load_composite();
    let cancel = CancellationToken::new();
    let listing = engine
        .list(&archive, None, 0, &quiet, &cancel)
        .expect("list legacy-name ZIP archive");
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].display_path, "한글.txt");

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
        .expect("extract legacy-name ZIP archive");
    assert_eq!(
        fs::read(output.join("한글.txt")).unwrap(),
        b"legacy ZIP\n"
    );
}

#[test]
fn bundled_zip_many_files_extracts_with_parallel_workers() {
    let work = Work::new();
    let input = work.0.join("payload");
    fs::create_dir_all(&input).unwrap();
    for index in 0..512 {
        let name = format!("file-{index:04}.bin");
        fs::write(input.join(name), [index as u8; 4096]).unwrap();
    }

    let engine = load_composite();
    let archive = work.0.join("many-files.zip");
    let cancel = CancellationToken::new();
    engine
        .create(
            &archive,
            std::slice::from_ref(&input),
            &CreateOptions {
                format: CreateFormat::Zip,
                ..CreateOptions::default()
            },
            &quiet,
            &cancel,
        )
        .expect("create many-file ZIP archive");

    let output = work.0.join("out");
    let summary = engine
        .extract(
            &archive,
            &output,
            &ExtractOptions::default(),
            &quiet,
            &Overwrite,
            &cancel,
        )
        .expect("extract many-file ZIP archive");
    assert_eq!(summary.entries_processed, 512);
    assert_eq!(fs::read_dir(output.join("payload")).unwrap().count(), 512);
    assert_eq!(
        fs::read(output.join("payload").join("file-0511.bin")).unwrap(),
        [0xFF_u8; 4096]
    );
}

fn write_legacy_zip(path: &Path, name: &[u8], contents: &[u8]) {
    let crc = crc32(contents);
    let name_length = u16::try_from(name.len()).unwrap();
    let size = u32::try_from(contents.len()).unwrap();
    let mut file = fs::File::create(path).unwrap();

    file.write_all(&0x0403_4B50u32.to_le_bytes()).unwrap();
    file.write_all(&20u16.to_le_bytes()).unwrap();
    file.write_all(&0u16.to_le_bytes()).unwrap();
    file.write_all(&0u16.to_le_bytes()).unwrap();
    file.write_all(&0u16.to_le_bytes()).unwrap();
    file.write_all(&0u16.to_le_bytes()).unwrap();
    file.write_all(&crc.to_le_bytes()).unwrap();
    file.write_all(&size.to_le_bytes()).unwrap();
    file.write_all(&size.to_le_bytes()).unwrap();
    file.write_all(&name_length.to_le_bytes()).unwrap();
    file.write_all(&0u16.to_le_bytes()).unwrap();
    file.write_all(name).unwrap();
    file.write_all(contents).unwrap();

    let central_offset = 30u32 + u32::try_from(name.len()).unwrap() + size;
    let central_size = 46u32 + u32::try_from(name.len()).unwrap();
    file.write_all(&0x0201_4B50u32.to_le_bytes()).unwrap();
    file.write_all(&20u16.to_le_bytes()).unwrap();
    file.write_all(&20u16.to_le_bytes()).unwrap();
    file.write_all(&0u16.to_le_bytes()).unwrap();
    file.write_all(&0u16.to_le_bytes()).unwrap();
    file.write_all(&0u16.to_le_bytes()).unwrap();
    file.write_all(&0u16.to_le_bytes()).unwrap();
    file.write_all(&crc.to_le_bytes()).unwrap();
    file.write_all(&size.to_le_bytes()).unwrap();
    file.write_all(&size.to_le_bytes()).unwrap();
    file.write_all(&name_length.to_le_bytes()).unwrap();
    file.write_all(&0u16.to_le_bytes()).unwrap();
    file.write_all(&0u16.to_le_bytes()).unwrap();
    file.write_all(&0u16.to_le_bytes()).unwrap();
    file.write_all(&0u16.to_le_bytes()).unwrap();
    file.write_all(&0u32.to_le_bytes()).unwrap();
    file.write_all(&0u32.to_le_bytes()).unwrap();
    file.write_all(name).unwrap();

    file.write_all(&0x0605_4B50u32.to_le_bytes()).unwrap();
    file.write_all(&0u16.to_le_bytes()).unwrap();
    file.write_all(&0u16.to_le_bytes()).unwrap();
    file.write_all(&1u16.to_le_bytes()).unwrap();
    file.write_all(&1u16.to_le_bytes()).unwrap();
    file.write_all(&central_size.to_le_bytes()).unwrap();
    file.write_all(&central_offset.to_le_bytes()).unwrap();
    file.write_all(&0u16.to_le_bytes()).unwrap();
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
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

#[test]
fn cancelled_7z_extraction_removes_temporary_file() {
    let work = Work::new();
    let input = work.0.join("payload");
    fs::create_dir_all(&input).unwrap();
    fs::write(input.join("hello.txt"), b"cancel me\n").unwrap();

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
        .unwrap();

    let output = work.0.join("out");
    fs::create_dir_all(output.join("payload")).unwrap();
    fs::write(output.join("payload").join("hello.txt"), b"old\n").unwrap();
    let resolver = CancelAfterResolve {
        cancel: cancel.clone(),
    };
    let result = engine.extract(
        &archive,
        &output,
        &ExtractOptions::default(),
        &quiet,
        &resolver,
        &cancel,
    );

    assert!(matches!(result, Err(ArchiveError::Cancelled)));
    assert_eq!(
        fs::read(output.join("payload").join("hello.txt")).unwrap(),
        b"old\n"
    );
    assert_no_archive_temporary_files(&output);
}

#[test]
fn bundled_7z_create_handles_multiple_inputs_and_password() {
    let work = Work::new();
    let folder = work.0.join("folder");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("inside.txt"), b"inside 7z\n").unwrap();
    let standalone = work.0.join("standalone.txt");
    fs::write(&standalone, b"standalone 7z\n").unwrap();

    let engine = load_engine();
    let archive = work.0.join("multiple.7z");
    let cancel = CancellationToken::new();
    engine
        .create(
            &archive,
            &[folder.clone(), standalone.clone()],
            &CreateOptions {
                format: CreateFormat::SevenZip,
                password: Some("test-password".to_owned()),
                threads: ThreadCount::Four,
                ..CreateOptions::default()
            },
            &quiet,
            &cancel,
        )
        .expect("create password-protected 7z archive");

    let output = work.0.join("out");
    engine
        .extract(
            &archive,
            &output,
            &ExtractOptions {
                password: Some("test-password".to_owned()),
                ..ExtractOptions::default()
            },
            &quiet,
            &Overwrite,
            &cancel,
        )
        .expect("extract password-protected 7z archive");
    assert_eq!(
        fs::read(output.join("folder").join("inside.txt")).unwrap(),
        b"inside 7z\n"
    );
    assert_eq!(
        fs::read(output.join("standalone.txt")).unwrap(),
        b"standalone 7z\n"
    );
}
