#![cfg(windows)]

use archive_rclick_core::{
    archive::{libarchive::LibArchiveEngine, *},
    tasks::{CancellationToken, ProgressSnapshot},
};
use std::{
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
};

fn engine() -> LibArchiveEngine {
    LibArchiveEngine::load_from_path(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/x64/archive.dll"),
    )
    .unwrap()
}

struct Overwrite;
impl ConflictResolver for Overwrite {
    fn resolve(&self, _: &Path) -> ConflictChoice {
        ConflictChoice::Overwrite
    }
}
fn quiet(_: ProgressSnapshot) {}

fn zip(names: &[&str]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for name in names {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(name.as_bytes()).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

#[test]
fn zip_unicode_names_keep_accents_cjk_emoji_and_distinct_ascii_names() {
    let names = [
        "Ea.txt",
        "Éa.txt",
        "café.txt",
        "한글.txt",
        "日本語.txt",
        "emoji-😀.txt",
        "폴더/日本語/😀.txt",
        "e\u{301}.txt",
    ];
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("unicode.zip");
    fs::write(&source, zip(&names)).unwrap();
    let engine = engine();
    let cancel = CancellationToken::new();
    for codepage in [0, 65001, 949] {
        let listing = engine
            .list(&source, None, codepage, &quiet, &cancel)
            .unwrap();
        let listed: Vec<_> = listing
            .entries
            .iter()
            .map(|entry| entry.display_path.as_str())
            .collect();
        assert_eq!(listed, names, "codepage {codepage}");
        let output = work.path().join(format!("out-{codepage}"));
        engine
            .extract(
                &source,
                &output,
                &ExtractOptions {
                    pathname_codepage: codepage,
                    ..Default::default()
                },
                &quiet,
                &Overwrite,
                &cancel,
            )
            .unwrap();
        for name in names {
            assert_eq!(fs::read(output.join(name)).unwrap(), name.as_bytes());
        }
    }
    // Selection must use the same names as listing, including non-ASCII folders.
    let output = work.path().join("selected");
    engine
        .extract(
            &source,
            &output,
            &ExtractOptions {
                selection: ExtractSelection::Paths(vec![PathBuf::from("폴더")]),
                ..Default::default()
            },
            &quiet,
            &Overwrite,
            &cancel,
        )
        .unwrap();
    assert_eq!(
        fs::read(output.join("폴더/日本語/😀.txt")).unwrap(),
        "폴더/日本語/😀.txt".as_bytes()
    );
    assert!(!output.join("Ea.txt").exists());
}

#[test]
fn tar_and_sevenzip_unicode_names_survive_listing_and_extraction() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("input");
    fs::create_dir_all(&input).unwrap();
    let names = ["Éa.txt", "日本語.txt", "한글-😀.txt"];
    for name in names {
        fs::write(input.join(name), name.as_bytes()).unwrap();
    }
    let engine = engine();
    let cancel = CancellationToken::new();
    for format in [CreateFormat::Tar, CreateFormat::SevenZip] {
        let source = work
            .path()
            .join(format!("unicode.{}", format.default_extension()));
        engine
            .create(
                &source,
                std::slice::from_ref(&input),
                &CreateOptions {
                    format,
                    ..Default::default()
                },
                &quiet,
                &cancel,
            )
            .unwrap();
        let listing = engine.list(&source, None, 0, &quiet, &cancel).unwrap();
        for name in names {
            assert!(
                listing
                    .entries
                    .iter()
                    .any(|entry| entry.path == Path::new(name)),
                "{format:?}: {listing:?}"
            );
        }
        let output = work
            .path()
            .join(format!("out-{}", format.default_extension()));
        engine
            .extract(
                &source,
                &output,
                &ExtractOptions::default(),
                &quiet,
                &Overwrite,
                &cancel,
            )
            .unwrap();
        for name in names {
            assert_eq!(fs::read(output.join(name)).unwrap(), name.as_bytes());
        }
    }
}

#[test]
fn legacy_zip_names_keep_detection_and_explicit_codepage_overrides() {
    for (raw, expected, codepages) in [
        (&b"\xc7\xd1\xb1\xdb.txt"[..], "한글.txt", &[0, 949][..]),
        (
            &b"\x93\xfa\x96\x7b\x8c\xea.txt"[..],
            "日本語.txt",
            &[932][..],
        ),
        (
            &b"\x82\xb1\x82\xf1\x82\xc9\x82\xbf\x82\xcd.txt"[..],
            "こんにちは.txt",
            &[0, 932][..],
        ),
        (&b"caf\xe9.txt"[..], "café.txt", &[1252][..]),
    ] {
        let mut bytes = zip(&[&"a".repeat(raw.len())]);
        let central = bytes
            .windows(4)
            .position(|bytes| bytes == b"PK\x01\x02")
            .unwrap();
        bytes[30..30 + raw.len()].copy_from_slice(raw);
        bytes[central + 46..central + 46 + raw.len()].copy_from_slice(raw);
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join("legacy.zip");
        fs::write(&source, bytes).unwrap();
        let engine = engine();
        let cancel = CancellationToken::new();
        for &codepage in codepages {
            let listing = engine
                .list(&source, None, codepage, &quiet, &cancel)
                .unwrap();
            assert_eq!(
                listing.entries[0].display_path, expected,
                "codepage {codepage}"
            );
            let output = work.path().join(format!("out-{codepage}"));
            engine
                .extract(
                    &source,
                    &output,
                    &ExtractOptions {
                        pathname_codepage: codepage,
                        ..Default::default()
                    },
                    &quiet,
                    &Overwrite,
                    &cancel,
                )
                .unwrap();
            assert!(output.join(expected).is_file());
        }
    }
}

#[test]
fn zip_names_follow_payload_offsets_when_central_directory_order_differs() {
    let names = ["Éa.txt", "日本語.txt", "한글-😀.txt"];
    let mut bytes = zip(&names);
    let eocd = bytes
        .windows(4)
        .rposition(|part| part == b"PK\x05\x06")
        .unwrap();
    let central = u32::from_le_bytes(bytes[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
    let mut records = Vec::new();
    let mut offset = central;
    while offset < eocd {
        let mut length = 46;
        for field in [28, 30, 32] {
            length += u16::from_le_bytes(
                bytes[offset + field..offset + field + 2]
                    .try_into()
                    .unwrap(),
            ) as usize;
        }
        records.push(bytes[offset..offset + length].to_vec());
        offset += length;
    }
    let reordered: Vec<u8> = records.into_iter().rev().flatten().collect();
    bytes[central..eocd].copy_from_slice(&reordered);
    let work = tempfile::tempdir().unwrap();
    // An SFX prefix also exercises adjusted physical offsets.
    for (label, prefix) in [("ordinary", &b""[..]), ("sfx", &b"MZ archive prefix\0"[..])] {
        let source = work.path().join(format!("{label}.zip"));
        fs::write(&source, [prefix, bytes.as_slice()].concat()).unwrap();
        let engine = engine();
        let cancel = CancellationToken::new();
        let listing = engine.list(&source, None, 0, &quiet, &cancel).unwrap();
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|entry| entry.display_path.as_str())
                .collect::<Vec<_>>(),
            names
        );
        let output = work.path().join(label);
        engine
            .extract(
                &source,
                &output,
                &ExtractOptions::default(),
                &quiet,
                &Overwrite,
                &cancel,
            )
            .unwrap();
        for name in names {
            assert_eq!(fs::read(output.join(name)).unwrap(), name.as_bytes());
        }
    }
}
