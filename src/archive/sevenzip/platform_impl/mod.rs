//! Windows composition root for the native 7-Zip backend.

// FFI vtable entry points are inherently unsafe; every body carries SAFETY
// comments, so the edition-2024 unsafe-op lint is relaxed in this module.
#![allow(unsafe_op_in_unsafe_fn)]

use std::{
    cell::Cell,
    collections::{BTreeMap, HashSet, VecDeque},
    env,
    ffi::{OsString, c_void},
    fs::{self, File, OpenOptions},
    io::{BufReader, Read, Seek, SeekFrom, Write},
    mem,
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use windows::{
    Win32::{
        Foundation::{FreeLibrary, HMODULE},
        System::LibraryLoader::{
            GetProcAddress, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
            LoadLibraryExW,
        },
    },
    core::{PCSTR, PCWSTR},
};

use crate::tasks::{CancellationToken, ProgressPhase, ProgressSnapshot, ThrottledProgress};

use super::super::encoding;
use super::super::libarchive::LibArchiveEngine;
use super::super::{
    ArchiveEngine, ArchiveEntry, ArchiveEntryKind, ArchiveError, ArchiveListing, ArchiveResult,
    ConflictChoice, ConflictResolver, CreateFormat, CreateOptions, ExtractOptions,
    InitialConflictPolicy, OperationSummary, ProgressSink, ensure_no_reparse_ancestors,
    safe_relative_path,
};

mod ffi;
use ffi::*;

// Safety budgets mirroring the libarchive backend.
const MAX_LIST_ENTRIES: u64 = 1_000_000;
const MAX_LIST_DECLARED_BYTES: u64 = 512 * 1024 * 1024 * 1024 * 1024;
const MAX_LIST_PATH_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ARCHIVE_PATH_UNITS: usize = 1024 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const STREAM_BUFFER_SIZE: usize = 1024 * 1024;
const MIN_STREAM_BUFFER_SIZE: usize = 4 * 1024;
const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

mod format;
use format::{
    ReadFormat, apply_zip_name_records, archive_format, effective_zip_codepage,
    read_zip_name_records, split_volume_paths, volume_part_path,
};

// FILETIME epoch (1601-01-01) relative to the Unix epoch, in 100ns units
// and in seconds.
const FILETIME_EPOCH_SECONDS: i64 = 11_644_473_600;

mod streams;
use streams::{
    IN_STREAM_VTBL, InStream, MultiInStream, OUT_STREAM_VTBL, OutStream, VOLUME_OUT_STREAM_VTBL,
    VolumeOutStream, VolumeOutput,
};

mod callbacks;
use callbacks::*;

mod compression_progress;
use compression_progress::CompressionProgressIndex;

mod file_ops;
use file_ops::{
    build_path, check_cancel, checked_add_with_limit, file_length, install_temporary,
    install_temporary_volumes, is_reparse, same_windows_path, stream_buffer_size, temporary_path,
};

mod source;
use source::collect_sources;

mod engine;
pub use engine::SevenZipEngine;

mod composite;
pub use composite::CompositeEngine;

#[cfg(test)]
mod tests {
    use super::{
        ArchiveEngine, CompositeEngine, CompressionProgressIndex, CreateFormat, CreateOptions,
        ReadFormat, SevenZipEngine, archive_format, filetime_to_unix_seconds,
        split_callback_progress, unix_seconds_to_filetime,
    };
    use crate::archive::ThreadCount;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    #[test]
    fn thread_count_options_map_to_sevenzip_mt() {
        assert_eq!(
            ThreadCount::Auto.sevenzip_threads(),
            ThreadCount::All.sevenzip_threads()
        );
        assert!(ThreadCount::Auto.sevenzip_threads().is_some());
        assert_eq!(ThreadCount::Four.sevenzip_threads(), Some(4));
        assert_eq!(ThreadCount::Six.sevenzip_threads(), Some(6));
        assert_eq!(ThreadCount::Eight.sevenzip_threads(), Some(8));
        assert_eq!(ThreadCount::Ten.sevenzip_threads(), Some(10));
        assert_eq!(ThreadCount::Sixteen.sevenzip_threads(), Some(16));
        assert_eq!(ThreadCount::from_registry_key("all"), ThreadCount::All);
        assert_eq!(ThreadCount::from_registry_key("6"), ThreadCount::Six);
        assert_eq!(ThreadCount::from_registry_key("10"), ThreadCount::Ten);
        assert_eq!(ThreadCount::from_registry_key("16"), ThreadCount::Sixteen);
        assert_eq!(ThreadCount::from_registry_key("bogus"), ThreadCount::Auto);
        assert_eq!(ThreadCount::from_ui_index(5), ThreadCount::Sixteen);
        assert_eq!(ThreadCount::Auto.ui_index(), 0);
        assert_eq!(ThreadCount::All.ui_index(), 6);
    }

    #[test]
    fn create_options_default_to_auto_threads() {
        assert_eq!(CreateOptions::default().threads, ThreadCount::Auto);
    }

    #[test]
    fn callback_progress_handles_counters_that_restart_per_file() {
        assert_eq!(split_callback_progress(950, 900, Some(100)), (950, 50));
        assert_eq!(split_callback_progress(50, 900, Some(100)), (950, 50));
        assert_eq!(split_callback_progress(1400, 900, Some(100)), (1000, 100));
    }

    #[test]
    fn compression_work_is_split_across_file_progress_ranges() {
        let totals = [25, 25, 0, 25, 25];
        let index = CompressionProgressIndex::new(totals.map(|size| (size, true)));
        assert_eq!(index.current_file(1), Some((0, 1)));
        assert_eq!(index.current_file(25), Some((0, 25)));
        assert_eq!(index.current_file(26), Some((1, 1)));
        assert_eq!(index.current_file(75), Some((3, 25)));
        assert_eq!(index.current_file(76), Some((4, 1)));
        assert_eq!(index.current_file(100), Some((4, 25)));
    }

    #[test]
    fn compression_file_count_ignores_directories_and_tracks_boundaries() {
        let totals = [25, 0, 0, 25];
        let is_file = [true, false, true, true];
        let index = CompressionProgressIndex::new(totals.into_iter().zip(is_file));
        assert_eq!(index.completed_files(0), 0);
        assert_eq!(index.completed_files(24), 0);
        assert_eq!(index.completed_files(25), 2);
        assert_eq!(index.completed_files(26), 2);
        assert_eq!(index.completed_files(50), 3);
    }

    #[test]
    fn missing_password_is_not_reported_as_an_empty_password() {
        let mut password = 1usize as *mut u16;
        assert_eq!(
            super::write_password_bstr(&mut password, None),
            super::E_ABORT
        );
        assert!(password.is_null());
        assert_eq!(
            super::write_password_bstr(&mut password, Some("")),
            super::E_ABORT
        );
        assert!(password.is_null());

        assert_eq!(
            super::write_password_bstr(&mut password, Some("secret")),
            super::S_OK
        );
        assert!(!password.is_null());
        // SAFETY: the helper allocated this BSTR with SysAllocString.
        unsafe { super::SysFreeString(password) };
    }

    #[test]
    fn input_stream_buffer_scales_for_small_files() {
        assert_eq!(super::stream_buffer_size(1), super::MIN_STREAM_BUFFER_SIZE);
        assert_eq!(super::stream_buffer_size(64 * 1024), 64 * 1024);
        assert_eq!(
            super::stream_buffer_size((super::STREAM_BUFFER_SIZE as u64) * 2),
            super::STREAM_BUFFER_SIZE
        );
    }

    #[test]
    fn filetime_round_trips_unix_seconds() {
        for seconds in [-2208988800i64, 0, 1_700_000_000] {
            assert_eq!(
                filetime_to_unix_seconds(unix_seconds_to_filetime(seconds)),
                seconds
            );
        }
    }

    #[test]
    fn detects_sevenzip_signature() {
        let directory = std::env::temp_dir().join(format!(
            "archive-rclick-sz-signature-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("create temp directory");
        let sevenzip = directory.join("sample.7z");
        let mut file = std::fs::File::create(&sevenzip).expect("create 7z file");
        file.write_all(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0x00])
            .expect("write signature");
        drop(file);
        let zip = directory.join("sample.zip");
        std::fs::write(&zip, b"PK\x03\x04junk").expect("create zip file");
        let rar4 = directory.join("sample-rar4.rar");
        std::fs::write(&rar4, b"Rar!\x1a\x07\x00junk").expect("create RAR4 file");
        let rar5 = directory.join("sample-rar5.rar");
        let lzh = directory.join("sample.lzh");
        let iso = directory.join("sample.iso");
        std::fs::write(&iso, b"not an ISO yet").expect("write ISO file");
        let img = directory.join("sample.img");
        std::fs::write(&img, b"not an ISO yet").expect("write IMG file");
        std::fs::write(&rar5, b"Rar!\x1a\x07\x01\x00junk").expect("create RAR5 file");
        std::fs::write(&lzh, [0x5f, 0x00, b'-', b'l', b'h', b'5', b'-', 0x00])
            .expect("create LZH file");
        assert_eq!(archive_format(&sevenzip), Some(ReadFormat::SevenZip));
        assert_eq!(archive_format(&zip), Some(ReadFormat::Zip));
        assert_eq!(archive_format(&rar4), Some(ReadFormat::Rar4));
        assert_eq!(archive_format(&rar5), Some(ReadFormat::Rar5));
        assert_eq!(archive_format(&lzh), Some(ReadFormat::Lzh));
        assert_eq!(archive_format(&iso), Some(ReadFormat::Iso));
        assert_eq!(archive_format(&img), Some(ReadFormat::Iso));
        assert_eq!(archive_format(&directory.join("missing.7z")), None);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn engine_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SevenZipEngine>();
        assert_send_sync::<CompositeEngine>();
    }

    #[test]
    fn detects_nsis_contents_with_a_misleading_extension() {
        let directory = std::env::temp_dir().join(format!(
            "archive-rclick-nsis-signature-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("installer.zip");
        let mut header = [0u8; 28];
        header[4..20].copy_from_slice(b"\xef\xbe\xad\xdeNullsoftInst");
        for offset in [0, 512, 108_544, 1024 * 1024] {
            let mut bytes = vec![0u8; offset + header.len()];
            bytes[..2].copy_from_slice(b"MZ");
            bytes[offset..].copy_from_slice(&header);
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(archive_format(&path), Some(ReadFormat::Nsis));
            bytes[offset + 4] = 0;
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(archive_format(&path), None);
        }
        for bytes in [&b""[..], &b"M"[..], &b"MZ"[..], &header[..19]] {
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(archive_format(&path), None);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn explicit_loader_rejects_relative_path_without_fallback() {
        assert!(SevenZipEngine::load_from_path(Path::new("7z.dll")).is_err());
    }

    #[test]
    fn explicit_loader_accepts_bundled_dll() {
        let executable = std::env::current_exe().expect("resolve test executable");
        let profile = executable
            .parent()
            .and_then(Path::parent)
            .expect("test executable has profile directory");
        for candidate in [
            profile.join("7z.dll"),
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("runtime")
                .join("x64")
                .join("7z.dll"),
        ] {
            if !candidate.is_file() {
                continue;
            }
            let engine = SevenZipEngine::load_from_path(&candidate).expect("bundled 7z.dll loads");
            assert!(engine.can_read(CreateFormat::Zip));
            assert!(engine.can_read_format(ReadFormat::Lzh));
            assert!(engine.can_read_format(ReadFormat::Rar4));
            assert!(engine.can_read_format(ReadFormat::Rar5));
            assert_eq!(
                engine.writable_formats(),
                vec![CreateFormat::Zip, CreateFormat::SevenZip]
            );
            return;
        }
        eprintln!("bundled 7z.dll was not staged for tests; skipping");
    }
}
