#![cfg(windows)]

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use archive_rclick_core::{
    archive::{
        ArchiveEngine, ArchiveError, ConflictChoice, ConflictResolver, CreateFormat, CreateOptions,
        ExtractOptions, libarchive::LibArchiveEngine,
    },
    tasks::CancellationToken,
};

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

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock precedes Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "archive-rclick-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn load_runtime() -> LibArchiveEngine {
    let executable = std::env::current_exe().expect("locate integration test executable");
    let profile_directory = executable
        .parent()
        .and_then(Path::parent)
        .expect("integration test executable should be under <profile>/deps");
    let bundled_runtime = profile_directory.join("archive.dll");
    LibArchiveEngine::load_from_path(&bundled_runtime).unwrap_or_else(|error| {
        panic!(
            "the bundled libarchive runtime at {} must load without discovery or development \
             overrides: {error}",
            bundled_runtime.display()
        )
    })
}

fn quiet_progress(_: archive_rclick_core::tasks::ProgressSnapshot) {}

#[test]
fn cancelled_zip_creation_removes_temporary_file() {
    let work = TestDirectory::new("cancelled-zip");
    let input = work.0.join("payload");
    fs::create_dir_all(&input).unwrap();
    fs::write(input.join("hello.txt"), b"cancel me\n").unwrap();

    let engine = load_runtime();
    let archive = work.0.join("cancelled.zip");
    let cancel = CancellationToken::new();
    let cancel_from_progress = cancel.clone();
    let progress = move |_: archive_rclick_core::tasks::ProgressSnapshot| {
        cancel_from_progress.cancel();
    };
    let result = engine.create(
        &archive,
        std::slice::from_ref(&input),
        &CreateOptions {
            format: CreateFormat::Zip,
            ..CreateOptions::default()
        },
        &progress,
        &cancel,
    );

    assert!(matches!(result, Err(ArchiveError::Cancelled)));
    assert!(!archive.exists());
    let leftovers = fs::read_dir(&work.0)
        .unwrap()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| name.contains("archiverclick"))
        .collect::<Vec<_>>();
    assert!(
        leftovers.is_empty(),
        "temporary files remain: {leftovers:?}"
    );
}

#[test]
fn cancelled_zip_extraction_removes_temporary_file() {
    let work = TestDirectory::new("cancelled-zip-extract");
    let input = work.0.join("payload");
    fs::create_dir_all(&input).unwrap();
    fs::write(input.join("hello.txt"), b"cancel me\n").unwrap();

    let engine = load_runtime();
    let archive = work.0.join("payload.zip");
    let cancel = CancellationToken::new();
    engine
        .create(
            &archive,
            std::slice::from_ref(&input),
            &CreateOptions {
                format: CreateFormat::Zip,
                ..CreateOptions::default()
            },
            &quiet_progress,
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
        &quiet_progress,
        &resolver,
        &cancel,
    );

    assert!(matches!(result, Err(ArchiveError::Cancelled)));
    assert_eq!(
        fs::read(output.join("hello.txt")).unwrap(),
        b"old\n"
    );
    let leftovers = fs::read_dir(&output)
        .unwrap()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| name.contains("archiverclick"))
        .collect::<Vec<_>>();
    assert!(
        leftovers.is_empty(),
        "temporary files remain: {leftovers:?}"
    );
}

#[test]
fn bundled_runtime_is_supported_libarchive_3_8_9() {
    let engine = load_runtime();
    let version = engine.version();
    assert_eq!(version, "libarchive 3.8.9");
}

#[test]
fn standalone_gzip_uses_source_stem_for_payload_name() {
    let work = TestDirectory::new("standalone-gzip-name");
    let archive = work.0.join("volume.nii.gz");
    // gzip-compressed `hello world`, with no original-name field in its header.
    fs::write(
        &archive,
        [
            0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xcb, 0x48, 0xcd, 0xc9,
            0xc9, 0x57, 0x28, 0xcf, 0x2f, 0xca, 0x49, 0x01, 0x00, 0x85, 0x11, 0x4a, 0x0d, 0x0b,
            0x00, 0x00, 0x00,
        ],
    )
    .unwrap();

    let engine = load_runtime();
    let cancel = CancellationToken::new();
    let listing = engine
        .list(&archive, None, 0, &quiet_progress, &cancel)
        .unwrap();
    assert_eq!(listing.format_name, "raw");
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].path, PathBuf::from("volume.nii"));
    assert_eq!(listing.entries[0].display_path, "volume.nii");

    let output = work.0.join("out");
    engine
        .extract(
            &archive,
            &output,
            &ExtractOptions::default(),
            &quiet_progress,
            &Overwrite,
            &cancel,
        )
        .unwrap();
    assert_eq!(fs::read(output.join("volume.nii")).unwrap(), b"hello world");
    assert!(!output.join("data").exists());
}

#[test]
fn tar_gzip_lists_the_wrapped_tar_without_scanning_members() {
    let work = TestDirectory::new("tar-gzip-data-name");
    let input = work.0.join("data");
    fs::write(&input, b"tar member").unwrap();

    let engine = load_runtime();
    let cancel = CancellationToken::new();
    let archive = work.0.join("bundle.tar.gz");
    engine
        .create(
            &archive,
            std::slice::from_ref(&input),
            &CreateOptions {
                format: CreateFormat::TarGzip,
                ..CreateOptions::default()
            },
            &quiet_progress,
            &cancel,
        )
        .unwrap();

    let listing = engine
        .list(&archive, None, 0, &quiet_progress, &cancel)
        .unwrap();
    assert_eq!(listing.format_name, "raw");
    assert_eq!(listing.filter_name.as_deref(), Some("gzip"));
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].display_path, "bundle.tar");
    assert_eq!(listing.entries[0].size, Some(fs::metadata(&archive).unwrap().len()));
    assert_eq!(
        listing.entries[0].compressed_size,
        Some(fs::metadata(&archive).unwrap().len())
    );
    assert!(listing.entries[0].modified_unix_seconds.is_some());

    let output = work.0.join("out");
    engine
        .extract(
            &archive,
            &output,
            &ExtractOptions::default(),
            &quiet_progress,
            &Overwrite,
            &cancel,
        )
        .unwrap();
    let tar = output.join("bundle.tar");
    assert!(tar.is_file());
    let tar_listing = engine
        .list(&tar, None, 0, &quiet_progress, &cancel)
        .unwrap();
    assert!(tar_listing
        .entries
        .iter()
        .any(|entry| entry.display_path == "data"));
}

#[test]
fn prioritized_creation_formats_round_trip() {
    let work = TestDirectory::new("formats");
    let input = work.0.join("payload");
    fs::create_dir_all(input.join("nested")).unwrap();
    fs::write(input.join("hello.txt"), b"hello from ArchiveRclick\n").unwrap();
    fs::write(
        input.join(r"nested\bytes.bin"),
        (0_u8..=255).collect::<Vec<_>>(),
    )
    .unwrap();
    fs::write(input.join(r"nested\Thumbs.db"), b"thumbnail cache").unwrap();

    let engine = load_runtime();
    let writable = engine.writable_formats();
    for format in CreateFormat::ALL {
        assert!(
            writable.contains(&format),
            "bundled libarchive cannot write {}",
            format.label()
        );
        let archive = work.0.join(format!(
            "roundtrip-{}.{}",
            format.label(),
            format.default_extension()
        ));
        let options = CreateOptions {
            format,
            compression_level: 6,
            password: None,
            ..CreateOptions::default()
        };
        let cancel = CancellationToken::new();
        engine
            .create(
                &archive,
                std::slice::from_ref(&input),
                &options,
                &quiet_progress,
                &cancel,
            )
            .unwrap_or_else(|error| panic!("creating {} failed: {error}", format.label()));

        let listing = engine
            .list(&archive, None, 0, &quiet_progress, &cancel)
            .unwrap_or_else(|error| panic!("listing {} failed: {error}", format.label()));
        if format == CreateFormat::TarGzip {
            assert_eq!(listing.format_name, "raw");
            assert_eq!(listing.entries.len(), 1);
            assert!(listing.entries[0].display_path.ends_with(".tar"));
        } else {
            assert!(
                listing
                    .entries
                    .iter()
                    .any(|entry| entry.display_path.replace('\\', "/") == "hello.txt"),
                "{} listing omitted hello.txt",
                format.label()
            );
            assert!(listing
                .entries
                .iter()
                .all(|entry| !entry.display_path.to_ascii_lowercase().ends_with("thumbs.db")));
        }
        engine
            .test(&archive, None, &quiet_progress, &cancel)
            .unwrap_or_else(|error| panic!("testing {} failed: {error}", format.label()));

        let output = work.0.join(format!("out-{}", format.label()));
        engine
            .extract(
                &archive,
                &output,
                &ExtractOptions::default(),
                &quiet_progress,
                &Overwrite,
                &cancel,
            )
            .unwrap_or_else(|error| panic!("extracting {} failed: {error}", format.label()));
        let content_output = if format == CreateFormat::TarGzip {
            let tar = output.join(&listing.entries[0].path);
            let inner_output = work.0.join("out-TAR.GZ-inner");
            engine
                .extract(
                    &tar,
                    &inner_output,
                    &ExtractOptions::default(),
                    &quiet_progress,
                    &Overwrite,
                    &cancel,
                )
                .expect("extract wrapped TAR payload");
            inner_output
        } else {
            output
        };
        assert_eq!(
            fs::read(content_output.join(r"hello.txt")).unwrap(),
            b"hello from ArchiveRclick\n"
        );
        assert_eq!(
            fs::read(content_output.join(r"nested\bytes.bin")).unwrap(),
            (0_u8..=255).collect::<Vec<_>>()
        );
    }
}

#[test]
fn encrypted_zip_requires_and_accepts_password() {
    let work = TestDirectory::new("password");
    let input = work.0.join("secret.txt");
    fs::write(&input, b"classified archive payload").unwrap();
    let archive = work.0.join("secret.zip");
    let engine = load_runtime();
    let cancel = CancellationToken::new();
    let password = "correct horse battery staple";
    engine
        .create(
            &archive,
            std::slice::from_ref(&input),
            &CreateOptions {
                format: CreateFormat::Zip,
                compression_level: 6,
                password: Some(password.to_owned()),
                ..CreateOptions::default()
            },
            &quiet_progress,
            &cancel,
        )
        .unwrap();

    assert!(matches!(
        engine.test(&archive, None, &quiet_progress, &cancel),
        Err(ArchiveError::PasswordRequired)
    ));
    assert!(matches!(
        engine.test(&archive, Some("wrong"), &quiet_progress, &cancel),
        Err(ArchiveError::PasswordRequired)
    ));
    engine
        .test(&archive, Some(password), &quiet_progress, &cancel)
        .unwrap();

    let output = work.0.join("out");
    let options = ExtractOptions {
        password: Some(password.to_owned()),
        ..ExtractOptions::default()
    };
    engine
        .extract(
            &archive,
            &output,
            &options,
            &quiet_progress,
            &Overwrite,
            &cancel,
        )
        .unwrap();
    assert_eq!(
        fs::read(output.join("secret.txt")).unwrap(),
        b"classified archive payload"
    );
}

#[test]
fn extraction_rejects_parent_traversal() {
    let work = TestDirectory::new("traversal");
    let archive = work.0.join("malicious.tar");
    write_tar(&archive, "../escape.txt", b"must not escape");
    let output = work.0.join("out");
    let engine = load_runtime();
    let error = engine
        .extract(
            &archive,
            &output,
            &ExtractOptions::default(),
            &quiet_progress,
            &Overwrite,
            &CancellationToken::new(),
        )
        .unwrap_err();
    assert!(matches!(error, ArchiveError::UnsafeEntryPath(_)));
    assert!(!work.0.join("escape.txt").exists());
}

#[test]
fn extraction_enforces_actual_output_limits() {
    let work = TestDirectory::new("limits");
    let archive = work.0.join("payload.tar");
    write_tar(&archive, "payload.bin", &[0x5a; 32]);
    let output = work.0.join("out");
    let options = ExtractOptions {
        max_total_bytes: 8,
        max_file_bytes: 8,
        ..ExtractOptions::default()
    };
    let error = load_runtime()
        .extract(
            &archive,
            &output,
            &options,
            &quiet_progress,
            &Overwrite,
            &CancellationToken::new(),
        )
        .unwrap_err();
    assert!(matches!(error, ArchiveError::LimitExceeded(_)));
    assert!(!output.join("payload.bin").exists());
}

fn write_tar(path: &Path, entry_name: &str, contents: &[u8]) {
    let mut header = [0_u8; 512];
    assert!(entry_name.len() <= 100);
    header[..entry_name.len()].copy_from_slice(entry_name.as_bytes());
    write_octal(&mut header[100..108], 0o644);
    write_octal(&mut header[108..116], 0);
    write_octal(&mut header[116..124], 0);
    write_octal(&mut header[124..136], contents.len() as u64);
    write_octal(&mut header[136..148], 0);
    header[148..156].fill(b' ');
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum = header.iter().map(|byte| u64::from(*byte)).sum();
    write_checksum(&mut header[148..156], checksum);

    let mut file = fs::File::create(path).unwrap();
    file.write_all(&header).unwrap();
    file.write_all(contents).unwrap();
    let padding = (512 - contents.len() % 512) % 512;
    file.write_all(&vec![0_u8; padding]).unwrap();
    file.write_all(&[0_u8; 1024]).unwrap();
    file.flush().unwrap();
}

fn write_octal(field: &mut [u8], value: u64) {
    field.fill(0);
    let digits = format!("{:0width$o}", value, width = field.len() - 1);
    field[..digits.len()].copy_from_slice(digits.as_bytes());
}

fn write_checksum(field: &mut [u8], value: u64) {
    let digits = format!("{:06o}", value);
    field[..6].copy_from_slice(digits.as_bytes());
    field[6] = 0;
    field[7] = b' ';
}
