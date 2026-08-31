#![cfg(windows)]

use std::{
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use archive_rclick_core::{
    archive::{
        ArchiveEngine, ArchiveError, CompositeEngine, ConflictChoice, ConflictResolver,
        ExtractOptions, SevenZipEngine, libarchive::LibArchiveEngine,
    },
    tasks::{CancellationToken, ProgressSnapshot},
};

struct Overwrite;
impl ConflictResolver for Overwrite {
    fn resolve(&self, _: &Path) -> ConflictChoice {
        ConflictChoice::Overwrite
    }
}

fn quiet(_: ProgressSnapshot) {}

#[test]
fn invalid_archives_return_errors() {
    let directory = std::env::temp_dir().join(format!(
        "archive-rclick-invalid-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&directory).unwrap();
    let engine = CompositeEngine::new(
        LibArchiveEngine::load().unwrap(),
        Some(SevenZipEngine::load().unwrap()),
    );
    let cancel = CancellationToken::new();
    let cases: &[(&str, &[u8])] = &[
        ("empty.zip", b""),
        ("short.zip", b"x"),
        ("exe.zip", b"MZ\x90\0This executable has no archive payload"),
        ("text.zip", b"This is a text file, not an archive.\n"),
        ("text.7z", b"This is a text file, not an archive.\n"),
        ("text.iso", b"This is a text file, not an archive.\n"),
        ("text.lzh", b"This is a text file, not an archive.\n"),
        ("text.zip.001", b"This is a text file, not an archive.\n"),
        ("truncated.zip", b"PK\x03\x04junk"),
        ("truncated.7z", b"7z\xbc\xaf\x27\x1cjunk"),
        ("truncated.rar", b"Rar!\x1a\x07\x01\x00junk"),
    ];
    for &(name, bytes) in cases {
        let path = directory.join(name);
        fs::write(&path, bytes).unwrap();
        eprintln!("list {name}");
        let error = engine.list(&path, None, 0, &quiet, &cancel).unwrap_err();
        assert!(
            matches!(error, ArchiveError::InvalidArchive(_)),
            "{name}: {error}"
        );
        eprintln!("list_directory {name}");
        assert!(
            engine
                .list_directory(&path, Path::new(""), None, 0, &quiet, &cancel)
                .is_err(),
            "{name}"
        );
        eprintln!("test {name}");
        assert!(engine.test(&path, None, &quiet, &cancel).is_err(), "{name}");
        eprintln!("extract {name}");
        assert!(
            engine
                .extract(
                    &path,
                    &directory.join(format!("out-{name}")),
                    &ExtractOptions::default(),
                    &quiet,
                    &Overwrite,
                    &cancel
                )
                .is_err(),
            "{name}"
        );
    }
    fs::remove_dir_all(directory).unwrap();
}
