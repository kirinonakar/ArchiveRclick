#![cfg(windows)]
use archive_rclick_core::{
    archive::*,
    tasks::{CancellationToken, ProgressPhase, ProgressSnapshot},
};
use std::{
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
};

fn engine() -> CompositeEngine {
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/x64");
    CompositeEngine::new(
        libarchive::LibArchiveEngine::load_from_path(&runtime.join("archive.dll")).unwrap(),
        Some(SevenZipEngine::load_from_path(&runtime.join("7z.dll")).unwrap()),
    )
}
fn quiet(_: ProgressSnapshot) {}
fn overwrite(_: &Path) -> ConflictChoice {
    ConflictChoice::Overwrite
}
struct Resolver;
impl ConflictResolver for Resolver {
    fn resolve(&self, path: &Path) -> ConflictChoice {
        overwrite(path)
    }
}
fn archive(entries: &[(&str, &[u8])], password: Option<&str>, stored: bool) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        let mut options = zip::write::SimpleFileOptions::default().compression_method(if stored {
            zip::CompressionMethod::Stored
        } else {
            zip::CompressionMethod::Deflated
        });
        if let Some(password) = password {
            options = options.with_aes_encryption(zip::AesMode::Aes256, password);
        }
        writer.start_file(*name, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}
fn extract(
    engine: &CompositeEngine,
    path: &Path,
    target: &Path,
    options: &ExtractOptions,
) -> ArchiveResult<OperationSummary> {
    engine.extract(
        path,
        target,
        options,
        &quiet,
        &Resolver,
        &CancellationToken::new(),
    )
}

#[test]
fn selected_backends_decode_native_and_flate_archives_including_split_aes() {
    let engine = engine();
    let payload = vec![b'a'; 180_000];
    for writer in ZipBackend::ALL {
        for reader in [ZipBackend::ZlibNg, ZipBackend::ZlibRs] {
            for split in [None, Some(4096)] {
                let work = tempfile::tempdir().unwrap();
                let source = work.path().join("input");
                fs::create_dir_all(&source).unwrap();
                for name in ["한글.txt", "second.bin", "third.bin"] {
                    fs::write(source.join(name), &payload).unwrap();
                }
                fs::write(source.join("empty"), []).unwrap();
                let path = work.path().join("test.zip");
                engine
                    .create(
                        &path,
                        &[source],
                        &CreateOptions {
                            zip_backend: writer,
                            password: Some("password".into()),
                            threads: ThreadCount::Four,
                            split_size: split,
                            ..Default::default()
                        },
                        &quiet,
                        &CancellationToken::new(),
                    )
                    .unwrap();
                let path = if split.is_some() {
                    work.path().join("test.zip.001")
                } else {
                    path
                };
                let target = work.path().join("out");
                let summary = extract(
                    &engine,
                    &path,
                    &target,
                    &ExtractOptions {
                        zip_backend: reader,
                        password: Some("password".into()),
                        threads: ThreadCount::Four,
                        ..Default::default()
                    },
                )
                .unwrap();
                assert_eq!(summary.bytes_processed, payload.len() as u64 * 3);
                for name in ["한글.txt", "second.bin", "third.bin"] {
                    assert_eq!(fs::read(target.join(name)).unwrap(), payload);
                }
                assert_eq!(fs::metadata(target.join("empty")).unwrap().len(), 0);
            }
        }
    }
}

#[test]
fn decoder_choice_selects_the_requested_implementation() {
    for ng in [false, true] {
        let mut reader =
            zip::ZipArchive::new(Cursor::new(archive(&[("a", b"payload")], None, false))).unwrap();
        let entry = reader
            .by_index_with_options(0, zip::read::ZipReadOptions::new().zlib_ng(ng))
            .unwrap();
        let debug = format!("{entry:?}");
        assert!(
            debug.contains(if ng {
                "ZlibNgDecompressor"
            } else {
                "DeflatedDecompressor"
            }),
            "{debug}"
        );
    }
}

#[test]
fn extraction_rejects_traversal_symlinks_and_declared_limits() {
    let engine = engine();
    for backend in [ZipBackend::ZlibNg, ZipBackend::ZlibRs] {
        for name in ["../escape", "/absolute", "C:/outside", "file:stream"] {
            let work = tempfile::tempdir().unwrap();
            let path = work.path().join("test.zip");
            fs::write(&path, archive(&[(name, b"bad")], None, false)).unwrap();
            assert!(matches!(
                extract(
                    &engine,
                    &path,
                    &work.path().join("out"),
                    &ExtractOptions {
                        zip_backend: backend,
                        ..Default::default()
                    }
                ),
                Err(ArchiveError::UnsafeEntryPath(_))
            ));
        }
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join("test.zip");
        fs::write(
            &path,
            archive(&[("a", b"123456"), ("b", b"123456")], None, false),
        )
        .unwrap();
        for options in [
            ExtractOptions {
                zip_backend: backend,
                max_file_bytes: 5,
                ..Default::default()
            },
            ExtractOptions {
                zip_backend: backend,
                max_total_bytes: 10,
                ..Default::default()
            },
            ExtractOptions {
                zip_backend: backend,
                max_entries: 1,
                ..Default::default()
            },
        ] {
            assert!(matches!(
                extract(&engine, &path, &work.path().join("out"), &options),
                Err(ArchiveError::LimitExceeded(_))
            ));
        }
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .add_symlink("link", "outside", zip::write::SimpleFileOptions::default())
            .unwrap();
        fs::write(&path, writer.finish().unwrap().into_inner()).unwrap();
        assert!(matches!(
            extract(
                &engine,
                &path,
                &work.path().join("out"),
                &ExtractOptions {
                    zip_backend: backend,
                    ..Default::default()
                }
            ),
            Err(ArchiveError::UnsafeEntryType(_))
        ));
    }
}

#[test]
fn corrupted_or_cancelled_entries_do_not_replace_existing_files() {
    let engine = engine();
    for backend in [ZipBackend::ZlibNg, ZipBackend::ZlibRs] {
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join("test.zip");
        let target = work.path().join("out");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("a"), b"original").unwrap();
        let options = ExtractOptions {
            zip_backend: backend,
            conflict_policy: InitialConflictPolicy::OverwriteAll,
            ..Default::default()
        };
        let mut bytes = archive(&[("a", b"payload")], None, true);
        bytes[31] ^= 1; // first byte of the stored payload, leaving the CRC unchanged
        fs::write(&path, bytes).unwrap();
        assert!(extract(&engine, &path, &target, &options).is_err());
        assert_eq!(fs::read(target.join("a")).unwrap(), b"original");
        let fresh = work.path().join("fresh");
        assert!(extract(&engine, &path, &fresh, &options).is_err());
        assert_eq!(fs::read_dir(&fresh).unwrap().count(), 0);
        fs::write(
            &path,
            archive(&[("a", &vec![1; 2 * 1024 * 1024])], None, false),
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let progress = |snapshot: ProgressSnapshot| {
            if snapshot.phase == ProgressPhase::Extracting {
                cancel.cancel();
            }
        };
        assert!(matches!(
            engine.extract(&path, &target, &options, &progress, &Resolver, &cancel),
            Err(ArchiveError::Cancelled)
        ));
        assert_eq!(fs::read(target.join("a")).unwrap(), b"original");
        assert_eq!(fs::read_dir(target).unwrap().count(), 1);
        let cancel = CancellationToken::new();
        let progress = |snapshot: ProgressSnapshot| {
            if snapshot.phase == ProgressPhase::Extracting {
                cancel.cancel();
            }
        };
        assert!(matches!(
            engine.extract(&path, &fresh, &options, &progress, &Resolver, &cancel),
            Err(ArchiveError::Cancelled)
        ));
        assert_eq!(fs::read_dir(fresh).unwrap().count(), 0);
    }
}

#[test]
fn selection_flatten_collisions_skip_and_password_errors_work() {
    let engine = engine();
    for backend in [ZipBackend::ZlibNg, ZipBackend::ZlibRs] {
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join("test.zip");
        let first = vec![1; 180_000];
        let second = vec![2; 180_000];
        fs::write(
            &path,
            archive(
                &[("a/same", &first), ("b/same", &second)],
                Some("password"),
                false,
            ),
        )
        .unwrap();
        let target = work.path().join("out");
        let mut options = ExtractOptions {
            zip_backend: backend,
            flatten_paths: true,
            threads: ThreadCount::Four,
            conflict_policy: InitialConflictPolicy::OverwriteAll,
            ..Default::default()
        };
        assert!(matches!(
            extract(&engine, &path, &target, &options),
            Err(ArchiveError::PasswordRequired)
        ));
        options.password = Some("wrong".into());
        assert!(matches!(
            extract(&engine, &path, &target, &options),
            Err(ArchiveError::PasswordRequired)
        ));
        options.password = Some("password".into());
        extract(&engine, &path, &target, &options).unwrap();
        assert_eq!(fs::read(target.join("same")).unwrap(), second);
        options.conflict_policy = InitialConflictPolicy::SkipAll;
        assert_eq!(
            extract(&engine, &path, &target, &options)
                .unwrap()
                .entries_skipped,
            2
        );
        options.selection = ExtractSelection::Paths(vec![PathBuf::from("a")]);
        options.conflict_policy = InitialConflictPolicy::OverwriteAll;
        extract(&engine, &path, &target, &options).unwrap();
        assert_eq!(fs::read(target.join("same")).unwrap(), first);
    }
}

#[test]
fn legacy_zip_names_use_the_selected_codepage() {
    let mut bytes = archive(&[("1234.txt", b"content")], None, false);
    let central = bytes.windows(4).position(|b| b == b"PK\x01\x02").unwrap();
    let name = b"\xc7\xd1\xb1\xdb.txt";
    bytes[30..38].copy_from_slice(name);
    bytes[central + 46..central + 54].copy_from_slice(name);
    let engine = engine();
    for backend in [ZipBackend::ZlibNg, ZipBackend::ZlibRs] {
        for codepage in [0, 949] {
            let work = tempfile::tempdir().unwrap();
            let path = work.path().join("test.zip");
            fs::write(&path, &bytes).unwrap();
            let target = work.path().join("out");
            extract(
                &engine,
                &path,
                &target,
                &ExtractOptions {
                    zip_backend: backend,
                    pathname_codepage: codepage,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(fs::read(target.join("한글.txt")).unwrap(), b"content");
        }
    }
}

#[test]
fn pooled_codecs_reset_across_levels_empty_members_and_flushes() {
    let work = tempfile::tempdir().unwrap();
    let path = work.path().join("pooled.zip");
    let mut writer = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    for (index, (ng, level)) in [(true, 1), (true, 1), (false, 9), (false, 9), (true, 5)]
        .into_iter()
        .enumerate()
    {
        let options = zip::write::SimpleFileOptions::default().compression_level(Some(level));
        if ng {
            writer
                .start_file_zlib_ng(index.to_string(), options)
                .unwrap();
        } else {
            writer.start_file(index.to_string(), options).unwrap();
        }
        if index != 1 {
            writer.write_all(&vec![index as u8; 256_000]).unwrap();
            writer.flush().unwrap();
            writer.write_all(b"tail").unwrap();
        }
    }
    writer.finish().unwrap();
    engine()
        .test(&path, None, &quiet, &CancellationToken::new())
        .unwrap();
    let mut reader = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
    for index in 0..5 {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut reader.by_index(index).unwrap(), &mut bytes).unwrap();
        let mut expected = if index == 1 {
            Vec::new()
        } else {
            vec![index as u8; 256_000]
        };
        if index != 1 {
            expected.extend_from_slice(b"tail");
        }
        assert_eq!(bytes, expected);
    }
}

#[test]
fn zone_identifier_propagates_only_to_extracted_supported_files() {
    fn ads(path: &Path) -> PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(":Zone.Identifier");
        name.into()
    }
    let engine = engine();
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/x64/archive.dll");
    let libarchive = libarchive::LibArchiveEngine::load_from_path(&runtime).unwrap();
    let work = tempfile::tempdir().unwrap();
    let path = work.path().join("origin.zip");
    let origin = b"[ZoneTransfer]\r\nZoneId=3\r\nHostUrl=https://example.com/origin.zip\r\n";
    fs::write(
        &path,
        archive(
            &[
                ("nested/APP.EXE", b"executable"),
                ("readme.txt", b"text"),
                ("document.docx", b"document"),
            ],
            None,
            false,
        ),
    )
    .unwrap();
    fs::write(ads(&path), origin).unwrap();
    assert!(ExtractOptions::default().copy_zone_identifier);
    for index in 0..4 {
        let backend: &dyn ArchiveEngine = if index == 3 { &libarchive } else { &engine };
        for enabled in [false, true] {
            let target = work.path().join(format!("out-{index}-{enabled}"));
            let options = ExtractOptions {
                copy_zone_identifier: enabled,
                zip_backend: ZipBackend::ALL[index.min(2)],
                ..Default::default()
            };
            backend
                .extract(
                    &path,
                    &target,
                    &options,
                    &quiet,
                    &Resolver,
                    &CancellationToken::new(),
                )
                .unwrap();
            for name in ["nested/APP.EXE", "document.docx"] {
                let stream = ads(&target.join(name));
                if enabled {
                    assert_eq!(fs::read(stream).unwrap(), origin);
                } else {
                    assert!(!stream.exists());
                }
            }
            assert!(!ads(&target.join("readme.txt")).exists());
            // Skip must leave both the existing bytes and its origin untouched.
            let preserved = target.join("document.docx");
            fs::write(&preserved, b"existing").unwrap();
            fs::write(ads(&preserved), b"existing origin").unwrap();
            let skipped = ExtractOptions {
                copy_zone_identifier: true,
                conflict_policy: InitialConflictPolicy::SkipAll,
                ..options.clone()
            };
            backend
                .extract(
                    &path,
                    &target,
                    &skipped,
                    &quiet,
                    &Resolver,
                    &CancellationToken::new(),
                )
                .unwrap();
            assert_eq!(fs::read(&preserved).unwrap(), b"existing");
            assert_eq!(fs::read(ads(&preserved)).unwrap(), b"existing origin");
            let overwritten = ExtractOptions {
                copy_zone_identifier: true,
                conflict_policy: InitialConflictPolicy::OverwriteAll,
                ..options
            };
            backend
                .extract(
                    &path,
                    &target,
                    &overwritten,
                    &quiet,
                    &Resolver,
                    &CancellationToken::new(),
                )
                .unwrap();
            assert_eq!(fs::read(ads(&preserved)).unwrap(), origin);
        }
    }
    fs::remove_file(ads(&path)).unwrap();
    let target = work.path().join("no-origin");
    extract(
        &engine,
        &path,
        &target,
        &ExtractOptions {
            copy_zone_identifier: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!ads(&target.join("document.docx")).exists());
}
