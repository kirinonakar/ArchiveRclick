use super::{
    ARCHIVE_OK, ARCHIVE_WARN, ArchiveEngine, ArchiveError, ArchiveReadSupport, LibArchiveEngine,
    MAX_LIST_ENTRIES, MAX_LIST_PATH_BYTES, MAX_SUPPORTED_LIBARCHIVE_VERSION, MAX_TEST_OUTPUT_BYTES,
    RawArchive, ScanBudget, canonical_library_file, checked_add_with_limit, copy_c_string_bounded,
    copy_c_string_bytes_bounded, enforce_limit, ensure_supported_libarchive_abi,
    looks_like_password_error, probe_supported_formats, system_time_seconds,
};
use std::{
    ffi::CString,
    path::Path,
    time::{Duration, UNIX_EPOCH},
};

#[test]
fn engine_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<LibArchiveEngine>();
}

#[test]
fn explicit_loader_rejects_relative_path_without_fallback() {
    assert!(matches!(
        canonical_library_file(Path::new("archive.dll")),
        Err(ArchiveError::InvalidInput(_))
    ));
}

#[test]
fn explicit_loader_accepts_bundled_absolute_dll() {
    let executable = std::env::current_exe().expect("resolve test executable");
    let dll = executable
        .parent()
        .and_then(Path::parent)
        .expect("test executable has profile directory")
        .join("archive.dll");
    if !dll.is_file() {
        eprintln!("bundled runtime was not staged at {}", dll.display());
        return;
    }
    let engine =
        LibArchiveEngine::load_from_path(&dll).expect("explicit bundled libarchive path loads");
    assert!(engine.version().starts_with("libarchive 3."));
}

unsafe extern "C" fn fake_read_new() -> *mut RawArchive {
    std::ptr::NonNull::<RawArchive>::dangling().as_ptr()
}

unsafe extern "C" fn fake_read_free(_: *mut RawArchive) -> i32 {
    ARCHIVE_OK
}

unsafe extern "C" fn supported_format(_: *mut RawArchive) -> i32 {
    ARCHIVE_OK
}

unsafe extern "C" fn unsupported_format(_: *mut RawArchive) -> i32 {
    ARCHIVE_WARN
}

#[test]
fn format_probe_keeps_only_successful_formats() {
    let candidates = [
        ("supported", supported_format as ArchiveReadSupport),
        ("unsupported", unsupported_format as ArchiveReadSupport),
    ];
    let formats = probe_supported_formats(fake_read_new, fake_read_free, &candidates)
        .expect("one format is supported");
    assert_eq!(formats.len(), 1);
    assert!(std::ptr::fn_addr_eq(
        formats[0],
        supported_format as ArchiveReadSupport
    ));
}

#[test]
fn inspection_limits_accept_boundary_and_reject_excess() {
    assert!(enforce_limit(MAX_LIST_ENTRIES, MAX_LIST_ENTRIES, "entries").is_ok());
    assert!(enforce_limit(MAX_LIST_ENTRIES + 1, MAX_LIST_ENTRIES, "entries").is_err());
    assert_eq!(
        checked_add_with_limit(MAX_LIST_PATH_BYTES - 1, 1, MAX_LIST_PATH_BYTES, "paths").unwrap(),
        MAX_LIST_PATH_BYTES
    );
    assert!(
        checked_add_with_limit(
            MAX_TEST_OUTPUT_BYTES,
            1,
            MAX_TEST_OUTPUT_BYTES,
            "test bytes"
        )
        .is_err()
    );
}

#[test]
fn scan_budget_counts_every_entry_and_decoded_chunk() {
    let mut scan = ScanBudget::new(2, 5, "test");
    assert!(scan.visit_entry().is_ok());
    assert!(scan.visit_entry().is_ok());
    assert!(scan.account_decoded(2).is_ok());
    assert!(scan.account_decoded(3).is_ok());
    assert_eq!(scan.entries_visited, 2);
    assert_eq!(scan.decoded_bytes, 5);
    assert!(scan.visit_entry().is_err());

    let mut bytes = ScanBudget::new(2, 5, "test");
    assert!(bytes.account_decoded(6).is_err());
}

#[test]
fn utf8_pathname_copy_is_bounded() {
    let value = CString::new("four").unwrap();
    // SAFETY: `value` is live and NUL-terminated for both calls.
    assert_eq!(
        unsafe { copy_c_string_bounded(value.as_ptr(), 4, "path") }.unwrap(),
        Some("four".to_owned())
    );
    // SAFETY: same valid string, with an intentionally smaller cap.
    assert!(unsafe { copy_c_string_bounded(value.as_ptr(), 3, "path") }.is_err());
}

#[test]
fn raw_pathname_copy_is_bounded_and_preserves_bytes() {
    let value = CString::new("four").unwrap();
    // SAFETY: `value` is live and NUL-terminated for both calls.
    assert_eq!(
        unsafe { copy_c_string_bytes_bounded(value.as_ptr(), 4, "path") }.unwrap(),
        Some(b"four".to_vec())
    );
    // SAFETY: same valid string, with an intentionally smaller cap.
    assert!(unsafe { copy_c_string_bytes_bounded(value.as_ptr(), 3, "path") }.is_err());
    // SAFETY: a null pointer yields None without touching memory.
    assert_eq!(
        unsafe { copy_c_string_bytes_bounded(std::ptr::null(), 4, "path") }.unwrap(),
        None
    );
}

#[test]
fn rejects_future_libarchive_abi() {
    assert!(ensure_supported_libarchive_abi(MAX_SUPPORTED_LIBARCHIVE_VERSION - 1).is_ok());
    assert!(ensure_supported_libarchive_abi(MAX_SUPPORTED_LIBARCHIVE_VERSION).is_err());
}

#[test]
fn recognizes_only_explicit_password_errors() {
    assert!(looks_like_password_error("Incorrect passphrase"));
    assert!(looks_like_password_error(
        "Passphrase required for this entry"
    ));
    assert!(!looks_like_password_error("Encrypted file is unsupported"));
    assert!(!looks_like_password_error(
        "Decryption failed: malformed data"
    ));
    assert!(!looks_like_password_error("Incorrect data check"));
    assert!(!looks_like_password_error("truncated archive"));
}

#[test]
fn converts_times_on_both_sides_of_epoch() {
    assert_eq!(
        system_time_seconds(UNIX_EPOCH + Duration::from_secs(42)),
        Some(42)
    );
    assert_eq!(
        system_time_seconds(UNIX_EPOCH - Duration::from_secs(42)),
        Some(-42)
    );
}
