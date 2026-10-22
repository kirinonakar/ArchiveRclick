#![cfg(windows)]
use archive_rclick_core::{
    archive::*,
    tasks::{CancellationToken, ProgressSnapshot},
};
use std::{
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
};

fn backends() -> Vec<(Box<dyn ArchiveEngine>, ExtractOptions)> {
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/x64");
    let mut result: Vec<(Box<dyn ArchiveEngine>, ExtractOptions)> = vec![(
        Box::new(
            libarchive::LibArchiveEngine::load_from_path(&runtime.join("archive.dll")).unwrap(),
        ),
        ExtractOptions::default(),
    )];
    for backend in ZipBackend::ALL {
        result.push((
            Box::new(CompositeEngine::new(
                libarchive::LibArchiveEngine::load_from_path(&runtime.join("archive.dll")).unwrap(),
                Some(SevenZipEngine::load_from_path(&runtime.join("7z.dll")).unwrap()),
            )),
            ExtractOptions {
                zip_backend: backend,
                ..Default::default()
            },
        ));
    }
    result
}
struct Overwrite;
impl ConflictResolver for Overwrite {
    fn resolve(&self, _: &Path) -> ConflictChoice {
        ConflictChoice::OverwriteAll
    }
}
fn extract(
    engine: &dyn ArchiveEngine,
    source: &Path,
    destination: &Path,
    options: &ExtractOptions,
) -> ArchiveResult<OperationSummary> {
    engine.extract(
        source,
        destination,
        options,
        &|_: ProgressSnapshot| {},
        &Overwrite,
        &CancellationToken::new(),
    )
}
fn zip(names: &[&str], link: bool) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for name in names {
        if name.ends_with('/') {
            writer
                .add_directory(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
        } else {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(name.as_bytes()).unwrap();
        }
    }
    if link {
        writer
            .add_symlink("link", "safe.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
    }
    writer.finish().unwrap().into_inner()
}

#[test]
fn incompatible_names_skip_and_strict_selection_agree_across_backends() {
    for (engine, options) in backends() {
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join("mixed.zip");
        fs::write(
            &source,
            zip(
                &[
                    "safe.txt",
                    "a:b.txt",
                    "what?.txt",
                    "trailing.",
                    "space ",
                    "CON.txt",
                ],
                false,
            ),
        )
        .unwrap();
        let target = work.path().join("default");
        let summary = extract(engine.as_ref(), &source, &target, &options).unwrap();
        assert_eq!(summary.entries_processed, 1);
        assert_eq!(summary.entries_skipped, 5);
        assert!(summary.warning.unwrap().contains("incompatible"));
        assert_eq!(fs::read(target.join("safe.txt")).unwrap(), b"safe.txt");
        assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
        let strict = ExtractOptions {
            incompatible_names: UnsupportedEntryPolicy::Fail,
            ..options.clone()
        };
        assert!(matches!(
            extract(
                engine.as_ref(),
                &source,
                &work.path().join("strict"),
                &strict
            ),
            Err(ArchiveError::IncompatibleEntryPath(_))
        ));
        let selected = ExtractOptions {
            selection: ExtractSelection::Paths(vec![PathBuf::from("safe.txt")]),
            ..strict
        };
        let summary = extract(
            engine.as_ref(),
            &source,
            &work.path().join("selected"),
            &selected,
        )
        .unwrap();
        assert_eq!(summary.entries_skipped, 0);
        assert!(summary.warning.is_none());
    }
}

#[test]
fn links_skip_with_warnings_and_strict_mode_preserves_existing_files() {
    for (engine, options) in backends() {
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join("links.zip");
        fs::write(&source, zip(&["safe.txt"], true)).unwrap();
        let target = work.path().join("default");
        let summary = extract(engine.as_ref(), &source, &target, &options).unwrap();
        assert_eq!(summary.entries_processed, 1);
        assert_eq!(summary.entries_skipped, 1);
        assert!(summary.warning.unwrap().contains("link"));
        assert!(!target.join("link").exists());
        fs::write(target.join("safe.txt"), b"existing").unwrap();
        let strict = ExtractOptions {
            unsupported_entries: UnsupportedEntryPolicy::Fail,
            ..options
        };
        assert!(matches!(
            extract(engine.as_ref(), &source, &target, &strict),
            Err(ArchiveError::UnsafeEntryType(_))
        ));
        assert_eq!(fs::read(target.join("safe.txt")).unwrap(), b"existing");
    }
}

#[test]
fn traversal_is_fatal_even_when_unselected_or_below_an_incompatible_name() {
    for (engine, options) in backends() {
        for bad in [
            "../escape",
            "/absolute",
            "C:/outside",
            "bad?/../escape",
            "bad?\\..\\escape",
        ] {
            let work = tempfile::tempdir().unwrap();
            let source = work.path().join("traversal.zip");
            fs::write(&source, zip(&[bad, "safe.txt"], false)).unwrap();
            let options = ExtractOptions {
                selection: ExtractSelection::Paths(vec!["safe.txt".into()]),
                ..options.clone()
            };
            assert!(
                matches!(
                    extract(engine.as_ref(), &source, &work.path().join("out"), &options),
                    Err(ArchiveError::UnsafeEntryPath(_))
                ),
                "{bad}"
            );
        }
    }
}

#[test]
fn file_directory_conflicts_and_blocked_parents_skip_without_deleting() {
    for (engine, options) in backends() {
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join("conflicts.zip");
        fs::write(
            &source,
            zip(&["file/", "file/child", "directory", "safe.txt"], false),
        )
        .unwrap();
        let target = work.path().join("out");
        fs::create_dir_all(target.join("directory")).unwrap();
        fs::write(target.join("file"), b"existing file").unwrap();
        fs::write(target.join("directory/child"), b"existing child").unwrap();
        let options = ExtractOptions {
            conflict_policy: InitialConflictPolicy::OverwriteAll,
            ..options
        };
        let summary = extract(engine.as_ref(), &source, &target, &options).unwrap();
        assert_eq!(summary.entries_skipped, 3);
        assert!(summary.warning.unwrap().contains("type conflict"));
        assert_eq!(fs::read(target.join("file")).unwrap(), b"existing file");
        assert_eq!(
            fs::read(target.join("directory/child")).unwrap(),
            b"existing child"
        );
        assert!(target.join("safe.txt").is_file());
    }
}

// A minimal POSIX TAR avoids needing symlink privileges or an external archiver.
fn tar_entry(out: &mut Vec<u8>, name: &str, kind: u8, contents: &[u8], link: &str) {
    let mut header = [0u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    header[100..108].copy_from_slice(b"0000644\0");
    header[108..116].copy_from_slice(b"0000000\0");
    header[116..124].copy_from_slice(b"0000000\0");
    header[124..136].copy_from_slice(format!("{:011o}\0", contents.len()).as_bytes());
    header[136..148].copy_from_slice(b"00000000000\0");
    header[148..156].fill(b' ');
    header[156] = kind;
    header[157..157 + link.len()].copy_from_slice(link.as_bytes());
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum: u32 = header.iter().map(|b| u32::from(*b)).sum();
    header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(contents);
    out.resize(out.len().next_multiple_of(512), 0);
}

#[test]
fn tar_late_symlink_hardlink_and_special_file_do_not_abort_default_extraction() {
    let (engine, options) = backends().remove(0);
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("links.tar");
    let mut bytes = Vec::new();
    tar_entry(&mut bytes, "before", b'0', b"before", "");
    tar_entry(&mut bytes, "symlink", b'2', b"", "before");
    tar_entry(&mut bytes, "hardlink", b'1', b"", "before");
    tar_entry(&mut bytes, "fifo", b'6', b"", "");
    tar_entry(&mut bytes, "after", b'0', b"after", "");
    bytes.resize(bytes.len() + 1024, 0);
    fs::write(&source, bytes).unwrap();
    let target = work.path().join("out");
    let summary = extract(engine.as_ref(), &source, &target, &options).unwrap();
    assert_eq!(summary.entries_processed, 2);
    assert_eq!(summary.entries_skipped, 3);
    assert!(target.join("after").is_file());
    let strict_target = work.path().join("strict");
    let strict = ExtractOptions {
        unsupported_entries: UnsupportedEntryPolicy::Fail,
        ..options
    };
    assert!(matches!(
        extract(engine.as_ref(), &source, &strict_target, &strict),
        Err(ArchiveError::UnsafeEntryType(_))
    ));
    assert!(!strict_target.exists());
}

// A small Copy-coded solid 7z with an uncompressed header, following
// https://github.com/ip7z/7zip/blob/main/DOC/7zFormat.txt . No external tools or
// Windows symlink privileges are needed to exercise Unix-mode link metadata.
fn stored_7z(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320 & (0u32.wrapping_sub(crc & 1)));
            }
        }
        !crc
    }
    let payload: Vec<u8> = entries
        .iter()
        .flat_map(|(_, _, data)| data.iter().copied())
        .collect();
    assert!(payload.len() < 128 && entries.len() < 20);
    let mut header = vec![
        1,
        4,
        6,
        0,
        1,
        9,
        payload.len() as u8,
        0,
        7,
        11,
        1,
        0,
        1,
        1,
        0,
        12,
        payload.len() as u8,
        0,
        8,
        13,
        entries.len() as u8,
        9,
    ];
    header.extend(
        entries[..entries.len() - 1]
            .iter()
            .map(|(_, _, data)| data.len() as u8),
    );
    header.extend([0, 0, 5, entries.len() as u8]);
    let mut names = vec![0];
    for (name, _, _) in entries {
        for unit in name.encode_utf16().chain(std::iter::once(0)) {
            names.extend(unit.to_le_bytes());
        }
    }
    assert!(names.len() < 128);
    header.extend([17, names.len() as u8]);
    header.extend(names);
    header.extend([21, (2 + 4 * entries.len()) as u8, 1, 0]);
    for (_, attributes, _) in entries {
        header.extend(attributes.to_le_bytes());
    }
    header.extend([0, 0]);
    let mut start = Vec::new();
    start.extend((payload.len() as u64).to_le_bytes());
    start.extend((header.len() as u64).to_le_bytes());
    start.extend(crc32(&header).to_le_bytes());
    let mut out = b"7z\xbc\xaf\x27\x1c\x00\x04".to_vec();
    out.extend(crc32(&start).to_le_bytes());
    out.extend(start);
    out.extend(payload);
    out.extend(header);
    out
}

#[test]
fn native_sevenzip_skips_unix_symlinks_and_reparse_payloads() {
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/x64/7z.dll");
    let engine = SevenZipEngine::load_from_path(&runtime).unwrap();
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("links.7z");
    fs::write(
        &source,
        stored_7z(&[
            ("safe.txt", 0x81a48020, b"safe"),
            ("unix-link", 0xa1ff8020, b"safe.txt"),
            ("reparse", 0x420, b"reparse payload"),
        ]),
    )
    .unwrap();
    let listing = engine
        .list(
            &source,
            None,
            0,
            &|_: ProgressSnapshot| {},
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(listing.entries[1].kind, ArchiveEntryKind::Symlink);
    assert_eq!(listing.entries[2].kind, ArchiveEntryKind::Symlink);
    let target = work.path().join("out");
    let summary = extract(&engine, &source, &target, &ExtractOptions::default()).unwrap();
    assert_eq!(summary.entries_skipped, 2);
    assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
    assert_eq!(fs::read(target.join("safe.txt")).unwrap(), b"safe");
}
