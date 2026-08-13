#![cfg(windows)]

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use archive_rclick_core::archive::ThreadCount;
use archive_rclick_core::{
    archive::{
        ArchiveEngine, ArchiveError, CompositeEngine, ConflictChoice, ConflictResolver,
        CreateFormat, CreateOptions, ExtractOptions, ProgressSink, SevenZipEngine,
        libarchive::LibArchiveEngine,
    },
    tasks::{CancellationToken, ProgressPhase, ProgressSnapshot},
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

struct RecordingProgress(Arc<Mutex<Vec<ProgressSnapshot>>>);

impl ProgressSink for RecordingProgress {
    fn report(&self, snapshot: ProgressSnapshot) {
        self.0.lock().unwrap().push(snapshot);
    }
}

struct DelayedRecordingProgress(Arc<Mutex<Vec<ProgressSnapshot>>>);

impl ProgressSink for DelayedRecordingProgress {
    fn report(&self, snapshot: ProgressSnapshot) {
        let delay = snapshot.phase == ProgressPhase::Opening
            || (snapshot.phase == ProgressPhase::Compressing && snapshot.bytes_processed == 0);
        self.0.lock().unwrap().push(snapshot);
        if delay {
            std::thread::sleep(std::time::Duration::from_millis(125));
        }
    }
}

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
    fs::write(input.join("Thumbs.db"), b"thumbnail cache").unwrap();

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
        fs::read(output.join("hello.txt")).unwrap(),
        b"hello from 7z\n"
    );
    assert!(!output.join("Thumbs.db").exists());
}

#[test]
fn bundled_zip_create_extract_round_trip() {
    let work = Work::new();
    let input = work.0.join("payload");
    fs::create_dir_all(input.join("nested")).unwrap();
    fs::write(input.join("hello.txt"), b"hello from ZIP\n").unwrap();
    fs::write(input.join("nested").join("data.bin"), [0_u8, 1, 2, 3, 4]).unwrap();
    fs::write(input.join("nested").join("Thumbs.db"), b"thumbnail cache").unwrap();

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
        fs::read(output.join("hello.txt")).unwrap(),
        b"hello from ZIP\n"
    );
    assert_eq!(
        fs::read(output.join("nested").join("data.bin")).unwrap(),
        [0_u8, 1, 2, 3, 4]
    );
    assert!(!output.join("nested").join("Thumbs.db").exists());
}

#[test]
fn bundled_7z_header_encryption_hides_file_names() {
    let work = Work::new();
    let input = work.0.join("secret-folder");
    fs::create_dir_all(input.join("nested")).unwrap();
    fs::write(input.join("secret.txt"), b"hidden 7z name\n").unwrap();

    let engine = load_engine();
    let archive = work.0.join("header-encrypted.7z");
    let cancel = CancellationToken::new();
    let password = "header-password";
    engine
        .create(
            &archive,
            std::slice::from_ref(&input),
            &CreateOptions {
                format: CreateFormat::SevenZip,
                password: Some(password.to_owned()),
                encrypt_headers: true,
                ..CreateOptions::default()
            },
            &quiet,
            &cancel,
        )
        .expect("create header-encrypted 7z archive");

    assert!(matches!(
        engine.list(&archive, None, 0, &quiet, &cancel),
        Err(ArchiveError::PasswordRequired)
    ));

    let listing = engine
        .list(&archive, Some(password), 0, &quiet, &cancel)
        .expect("list header-encrypted 7z archive with password");
    assert!(
        listing
            .entries
            .iter()
            .any(|entry| entry.display_path == "secret.txt")
    );
    assert!(listing
        .entries
        .iter()
        .all(|entry| !entry.display_path.contains("secret-folder")));

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
        .expect("extract header-encrypted 7z archive");
    assert_eq!(
        fs::read(output.join("secret.txt")).unwrap(),
        b"hidden 7z name\n"
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
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let progress = RecordingProgress(Arc::clone(&recorded));
    let summary = engine
        .extract(
            &archive,
            &output,
            &ExtractOptions::default(),
            &progress,
            &Overwrite,
            &cancel,
        )
        .expect("extract many-file ZIP archive");
    let snapshots = recorded.lock().unwrap();
    let mut previous_bytes = 0;
    let mut previous_fraction = 0.0;
    for snapshot in snapshots
        .iter()
        .filter(|snapshot| snapshot.phase == ProgressPhase::Extracting)
    {
        assert!(
            snapshot.bytes_processed >= previous_bytes,
            "overall progress moved backwards: {previous_bytes} -> {}",
            snapshot.bytes_processed
        );
        previous_bytes = snapshot.bytes_processed;
        let fraction = snapshot.fraction();
        assert!(
            fraction + f32::EPSILON >= previous_fraction,
            "overall percentage moved backwards: {previous_fraction:.3} -> {fraction:.3}"
        );
        previous_fraction = fraction;
        if let Some(total) = snapshot.current_file_total_bytes {
            assert!(
                snapshot.current_file_bytes_processed <= total,
                "current file progress exceeded its total: {} > {total}",
                snapshot.current_file_bytes_processed
            );
        }
    }
    assert!(previous_bytes > 0, "parallel extraction emitted no progress");
    assert_eq!(summary.entries_processed, 512);
    assert_eq!(fs::read_dir(&output).unwrap().count(), 512);
    assert_eq!(
        fs::read(output.join("file-0511.bin")).unwrap(),
        [0xFF_u8; 4096]
    );
}

#[test]
fn compression_progress_uses_source_bytes_for_zip_and_7z() {
    let work = Work::new();
    let input = work.0.join("compression-progress");
    fs::create_dir_all(&input).unwrap();
    for file_index in 0..4u8 {
        let mut bytes = vec![0u8; 8 * 1024 * 1024];
        let mut state = u32::from(file_index) + 1;
        for byte in &mut bytes {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *byte = (state >> 24) as u8;
        }
        fs::write(input.join(format!("file-{file_index}.bin")), bytes).unwrap();
    }

    let engine = load_composite();
    let cancel = CancellationToken::new();
    for format in [CreateFormat::Zip, CreateFormat::SevenZip] {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let progress = DelayedRecordingProgress(Arc::clone(&recorded));
        let archive = work.0.join(format!("debug.{}", format.default_extension()));
        let summary = engine
            .create(
                &archive,
                std::slice::from_ref(&input),
                &CreateOptions {
                    format,
                    ..CreateOptions::default()
                },
                &progress,
                &cancel,
            )
            .unwrap();
        assert_eq!(summary.bytes_processed, 32 * 1024 * 1024);
        assert_eq!(summary.entries_processed, 4);
        let snapshots = recorded.lock().unwrap();
        let mut previous_bytes = 0;
        let mut saw_partial = false;
        let mut saw_last_quarter_advance = false;
        for snapshot in snapshots
            .iter()
            .filter(|snapshot| snapshot.phase == ProgressPhase::Compressing)
        {
            let total = snapshot.total_bytes.expect("compression total bytes");
            assert!(
                snapshot.bytes_processed <= total,
                "compression progress exceeded its input total: {} > {total}",
                snapshot.bytes_processed
            );
            assert!(
                snapshot.bytes_processed >= previous_bytes,
                "compression progress moved backwards: {previous_bytes} -> {}",
                snapshot.bytes_processed
            );
            previous_bytes = snapshot.bytes_processed;
            saw_partial |= snapshot.bytes_processed < total;
            if format == CreateFormat::SevenZip
                && snapshot.bytes_processed > total * 3 / 4
                && snapshot.bytes_processed < total
            {
                saw_last_quarter_advance = true;
            }
            assert_eq!(snapshot.total_entries, Some(4));
            assert!(snapshot.entries_processed <= 4);
            if let Some(current_total) = snapshot.current_file_total_bytes {
                assert!(
                    snapshot.current_file_bytes_processed <= current_total,
                    "current file progress exceeded its total: {} > {current_total}",
                    snapshot.current_file_bytes_processed
                );
                if snapshot.bytes_processed == total {
                    assert_eq!(
                        snapshot.current_file_bytes_processed, current_total,
                        "overall compression reached 100% before the current file finished"
                    );
                }
                if current_total > 0 && !snapshot.current_file.is_empty() {
                    assert!(
                        snapshot.current_file_bytes_processed > 0,
                        "{} was displayed before 7-Zip read any of its bytes",
                        snapshot.current_file,
                    );
                }
            }
        }
        assert!(
            saw_partial,
            "compression did not emit an intermediate source-byte snapshot for {format:?}"
        );
        if format == CreateFormat::SevenZip {
            assert!(
                saw_last_quarter_advance,
                "7z compression jumped directly from 75% to completion"
            );
        }
    }
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
    fs::create_dir_all(&output).unwrap();
    fs::write(output.join("hello.txt"), b"old\n").unwrap();
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
        fs::read(output.join("hello.txt")).unwrap(),
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
