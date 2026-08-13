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
fn bundled_runtime_is_supported_libarchive_3_8_9() {
    let engine = load_runtime();
    let version = engine.version();
    assert_eq!(version, "libarchive 3.8.9");
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
            .list(&archive, None, &quiet_progress, &cancel)
            .unwrap_or_else(|error| panic!("listing {} failed: {error}", format.label()));
        assert!(
            listing
                .entries
                .iter()
                .any(|entry| entry.display_path.replace('\\', "/") == "payload/hello.txt"),
            "{} listing omitted payload/hello.txt",
            format.label()
        );
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
        assert_eq!(
            fs::read(output.join(r"payload\hello.txt")).unwrap(),
            b"hello from ArchiveRclick\n"
        );
        assert_eq!(
            fs::read(output.join(r"payload\nested\bytes.bin")).unwrap(),
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
