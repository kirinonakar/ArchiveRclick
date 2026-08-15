//! Dynamically loaded libarchive backend.
//!
//! Archive parsing and encoding are delegated to libarchive, but extraction is
//! deliberately performed with Rust filesystem APIs.  This keeps every output
//! path behind the validation in `path_safety` and prevents archive-provided
//! links or special files from ever reaching a disk writer.

#[cfg(windows)]
mod platform_impl {
    use std::{
        collections::HashSet,
        env,
        ffi::{CStr, CString, OsString, c_char, c_int, c_long, c_void},
        fs::{self, File, OpenOptions},
        io::{Read, Write},
        mem,
        os::windows::{
            ffi::{OsStrExt, OsStringExt},
            fs::{MetadataExt, OpenOptionsExt},
            io::AsRawHandle,
        },
        path::{Path, PathBuf},
        ptr::{self, NonNull},
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use windows::{
        Win32::{
            Foundation::{FreeLibrary, HMODULE},
            System::LibraryLoader::{
                GetProcAddress, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
                LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
            },
        },
        core::{PCSTR, PCWSTR},
    };

    use crate::tasks::{CancellationToken, ProgressPhase, ProgressSnapshot, ThrottledProgress};

    use super::super::{
        ArchiveEngine, ArchiveEntry, ArchiveEntryKind, ArchiveError, ArchiveListing, ArchiveResult,
        ConflictChoice, ConflictResolver, CreateFormat, CreateOptions, ExtractOptions,
        InitialConflictPolicy, OperationSummary, ProgressSink, encoding,
        ensure_no_reparse_ancestors, safe_relative_path,
    };

    const ARCHIVE_EOF: c_int = 1;
    const ARCHIVE_OK: c_int = 0;
    const ARCHIVE_RETRY: c_int = -10;
    const ARCHIVE_WARN: c_int = -20;

    // 3.8.9 is a security release and includes the malformed-archive
    // hardening accumulated in 3.8.8.  Older parsers are only available via
    // an explicit development escape hatch; archive files are untrusted input.
    const MIN_LIBARCHIVE_VERSION: c_int = 3_008_009;
    // The 4.0 development ABI changes public mode/time types.  Bindings in
    // this module intentionally target the stable libarchive 3.x ABI only.
    const MAX_SUPPORTED_LIBARCHIVE_VERSION: c_int = 3_999_000;

    // libarchive 3.x defines __LA_MODE_T as unsigned short on Windows.
    const AE_IFMT: u16 = 0o170000;
    const AE_IFREG: u16 = 0o100000;
    const AE_IFLNK: u16 = 0o120000;
    const AE_IFDIR: u16 = 0o040000;

    // Keep the Rust-to-libarchive handoff large enough that file I/O and FFI
    // call overhead stay negligible next to the codec work.  libarchive has
    // its own internal buffering, so this is deliberately a moderate 1 MiB
    // staging buffer rather than an unbounded read-ahead cache.
    const IO_BUFFER_SIZE: usize = 1024 * 1024;
    const OPEN_BLOCK_SIZE: usize = 64 * 1024;
    const MAX_ARCHIVE_PATH_UNITS: usize = 1024 * 1024;
    const MAX_ARCHIVE_PATH_UTF8_BYTES: usize = 4 * 1024 * 1024;
    const MAX_LIST_ENTRIES: u64 = 1_000_000;
    const MAX_LIST_SCAN_DECODED_BYTES: u64 = 64 * 1024 * 1024 * 1024;
    const MAX_LIST_PATH_BYTES: u64 = 256 * 1024 * 1024;
    const MAX_LIST_DECLARED_BYTES: u64 = 512 * 1024 * 1024 * 1024 * 1024;
    const MAX_EXTRACT_SCAN_ENTRIES: u64 = 1_000_000;
    const MAX_EXTRACT_SCAN_DECODED_BYTES: u64 = 512 * 1024 * 1024 * 1024;
    const MAX_TEST_ENTRIES: u64 = 1_000_000;
    const MAX_TEST_OUTPUT_BYTES: u64 = 512 * 1024 * 1024 * 1024;
    const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn GetFinalPathNameByHandleW(
            file: *mut c_void,
            path: *mut u16,
            path_units: u32,
            flags: u32,
        ) -> u32;
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }

    #[repr(C)]
    struct RawArchive {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct RawEntry {
        _private: [u8; 0],
    }

    type ArchiveVersionString = unsafe extern "C" fn() -> *const c_char;
    type ArchiveVersionNumber = unsafe extern "C" fn() -> c_int;
    type ArchiveReadNew = unsafe extern "C" fn() -> *mut RawArchive;
    type ArchiveReadSupport = unsafe extern "C" fn(*mut RawArchive) -> c_int;
    type ArchiveReadAddPassphrase = unsafe extern "C" fn(*mut RawArchive, *const c_char) -> c_int;
    type ArchiveReadOpenFilenameW =
        unsafe extern "C" fn(*mut RawArchive, *const u16, usize) -> c_int;
    type ArchiveReadNextHeader = unsafe extern "C" fn(*mut RawArchive, *mut *mut RawEntry) -> c_int;
    type ArchiveReadData = unsafe extern "C" fn(*mut RawArchive, *mut c_void, usize) -> isize;
    type ArchiveReadClose = unsafe extern "C" fn(*mut RawArchive) -> c_int;
    type ArchiveReadFree = unsafe extern "C" fn(*mut RawArchive) -> c_int;
    type ArchiveReadHasEncryptedEntries = unsafe extern "C" fn(*mut RawArchive) -> c_int;

    type ArchiveErrorString = unsafe extern "C" fn(*mut RawArchive) -> *const c_char;
    type ArchiveErrno = unsafe extern "C" fn(*mut RawArchive) -> c_int;
    type ArchiveFormatName = unsafe extern "C" fn(*mut RawArchive) -> *const c_char;
    type ArchiveFilterName = unsafe extern "C" fn(*mut RawArchive, c_int) -> *const c_char;
    type ArchiveFilterBytes = unsafe extern "C" fn(*mut RawArchive, c_int) -> i64;

    type ArchiveEntryPathnameW = unsafe extern "C" fn(*mut RawEntry) -> *const u16;
    type ArchiveEntryPathnameUtf8 = unsafe extern "C" fn(*mut RawEntry) -> *const c_char;
    type ArchiveEntryPathname = unsafe extern "C" fn(*mut RawEntry) -> *const c_char;
    type ArchiveEntrySize = unsafe extern "C" fn(*mut RawEntry) -> i64;
    type ArchiveEntryFlag = unsafe extern "C" fn(*mut RawEntry) -> c_int;
    type ArchiveEntryFiletype = unsafe extern "C" fn(*mut RawEntry) -> u16;
    type ArchiveEntryMtime = unsafe extern "C" fn(*mut RawEntry) -> i64;

    type ArchiveWriteNew = unsafe extern "C" fn() -> *mut RawArchive;
    type ArchiveWriteUnary = unsafe extern "C" fn(*mut RawArchive) -> c_int;
    type ArchiveWriteOpenFilenameW = unsafe extern "C" fn(*mut RawArchive, *const u16) -> c_int;
    type ArchiveWriteHeader = unsafe extern "C" fn(*mut RawArchive, *mut RawEntry) -> c_int;
    type ArchiveWriteData = unsafe extern "C" fn(*mut RawArchive, *const c_void, usize) -> isize;
    type ArchiveWriteOption =
        unsafe extern "C" fn(*mut RawArchive, *const c_char, *const c_char, *const c_char) -> c_int;
    type ArchiveWriteSetPassphrase = unsafe extern "C" fn(*mut RawArchive, *const c_char) -> c_int;

    type ArchiveEntryNew = unsafe extern "C" fn() -> *mut RawEntry;
    type ArchiveEntryFree = unsafe extern "C" fn(*mut RawEntry);
    type ArchiveEntryCopyPathnameW = unsafe extern "C" fn(*mut RawEntry, *const u16);
    type ArchiveEntrySetFiletype = unsafe extern "C" fn(*mut RawEntry, u32);
    type ArchiveEntrySetMode = unsafe extern "C" fn(*mut RawEntry, u16);
    type ArchiveEntrySetSize = unsafe extern "C" fn(*mut RawEntry, i64);
    type ArchiveEntrySetMtime = unsafe extern "C" fn(*mut RawEntry, i64, c_long);

    struct DynamicLibrary {
        handle: HMODULE,
    }

    // A loaded module can be queried and freed from any thread.  The handle is
    // kept alive by Arc<Api> for at least as long as every copied function ptr.
    unsafe impl Send for DynamicLibrary {}
    unsafe impl Sync for DynamicLibrary {}

    impl DynamicLibrary {
        fn load(path: &Path, system_only: bool) -> Result<Self, String> {
            let wide = wide_nul(path).map_err(|error| error.to_string())?;
            let flags = if system_only {
                LOAD_LIBRARY_SEARCH_SYSTEM32
            } else {
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS
            };
            // SAFETY: `wide` is NUL-terminated and remains alive for the call.
            let handle = unsafe { LoadLibraryExW(PCWSTR(wide.as_ptr()), None, flags) }
                .map_err(|error| error.to_string())?;
            Ok(Self { handle })
        }

        fn symbol(&self, name: &'static [u8]) -> Option<usize> {
            debug_assert_eq!(name.last(), Some(&0));
            // SAFETY: `name` is static and NUL-terminated; the module is live.
            unsafe { GetProcAddress(self.handle, PCSTR(name.as_ptr())) }
                .map(|symbol| symbol as usize)
        }
    }

    impl Drop for DynamicLibrary {
        fn drop(&mut self) {
            // SAFETY: this object uniquely owns the successful LoadLibrary call.
            let _ = unsafe { FreeLibrary(self.handle) };
        }
    }

    macro_rules! required_symbol {
        ($library:expr, $name:literal, $ty:ty) => {{
            let address = $library
                .symbol(concat!($name, "\0").as_bytes())
                .ok_or(ArchiveError::MissingSymbol($name))?;
            // SAFETY: the symbol name and signature come from archive.h.
            unsafe { mem::transmute::<usize, $ty>(address) }
        }};
    }

    macro_rules! optional_symbol {
        ($library:expr, $name:literal, $ty:ty) => {{
            $library
                .symbol(concat!($name, "\0").as_bytes())
                .map(|address| {
                    // SAFETY: the symbol name and signature come from archive.h.
                    unsafe { mem::transmute::<usize, $ty>(address) }
                })
        }};
    }

    struct Api {
        _library: DynamicLibrary,
        version_string: ArchiveVersionString,
        version_number: c_int,
        version_note: Option<String>,
        read_new: ArchiveReadNew,
        read_filters: Vec<ArchiveReadSupport>,
        read_formats: Vec<ArchiveReadSupport>,
        read_support_format_raw: Option<ArchiveReadSupport>,
        read_add_passphrase: Option<ArchiveReadAddPassphrase>,
        read_open_filename_w: ArchiveReadOpenFilenameW,
        read_next_header: ArchiveReadNextHeader,
        read_data: ArchiveReadData,
        read_close: ArchiveReadClose,
        read_free: ArchiveReadFree,
        read_has_encrypted_entries: Option<ArchiveReadHasEncryptedEntries>,
        error_string: ArchiveErrorString,
        archive_errno: ArchiveErrno,
        format_name: ArchiveFormatName,
        filter_name: ArchiveFilterName,
        filter_bytes: ArchiveFilterBytes,
        entry_pathname_w: ArchiveEntryPathnameW,
        entry_pathname_utf8: Option<ArchiveEntryPathnameUtf8>,
        entry_pathname: ArchiveEntryPathname,
        entry_size: ArchiveEntrySize,
        entry_size_is_set: ArchiveEntryFlag,
        entry_filetype: ArchiveEntryFiletype,
        entry_mtime: ArchiveEntryMtime,
        entry_mtime_is_set: ArchiveEntryFlag,
        entry_hardlink_is_set: ArchiveEntryFlag,
        entry_is_encrypted: Option<ArchiveEntryFlag>,
        write: Option<WriteApi>,
    }

    struct WriteApi {
        write_new: ArchiveWriteNew,
        set_format_zip: ArchiveWriteUnary,
        set_format_pax_restricted: ArchiveWriteUnary,
        set_format_7zip: Option<ArchiveWriteUnary>,
        add_filter_gzip: Option<ArchiveWriteUnary>,
        add_filter_xz: Option<ArchiveWriteUnary>,
        add_filter_zstd: Option<ArchiveWriteUnary>,
        set_format_option: ArchiveWriteOption,
        set_filter_option: ArchiveWriteOption,
        set_passphrase: Option<ArchiveWriteSetPassphrase>,
        open_filename_w: ArchiveWriteOpenFilenameW,
        write_header: ArchiveWriteHeader,
        write_data: ArchiveWriteData,
        finish_entry: ArchiveWriteUnary,
        close: ArchiveWriteUnary,
        free: ArchiveWriteUnary,
        entry_new: ArchiveEntryNew,
        entry_free: ArchiveEntryFree,
        entry_copy_pathname_w: ArchiveEntryCopyPathnameW,
        entry_set_filetype: ArchiveEntrySetFiletype,
        entry_set_perm: ArchiveEntrySetMode,
        entry_set_size: ArchiveEntrySetSize,
        entry_set_mtime: ArchiveEntrySetMtime,
    }

    fn ensure_supported_libarchive_abi(version_number: c_int) -> ArchiveResult<()> {
        if version_number >= MAX_SUPPORTED_LIBARCHIVE_VERSION {
            Err(ArchiveError::LibraryUnavailable(format!(
                "libarchive {version_number} uses an unsupported future ABI; install a \
                 stable libarchive 3.x release below {MAX_SUPPORTED_LIBARCHIVE_VERSION}"
            )))
        } else {
            Ok(())
        }
    }

    impl Api {
        fn from_library(
            library: DynamicLibrary,
            allow_unsupported: bool,
            archiveint_fallback: bool,
        ) -> ArchiveResult<Self> {
            let version_number_fn =
                required_symbol!(library, "archive_version_number", ArchiveVersionNumber);
            // SAFETY: this process-global accessor has no preconditions.
            let version_number = unsafe { version_number_fn() };
            ensure_supported_libarchive_abi(version_number)?;
            if version_number < MIN_LIBARCHIVE_VERSION && !allow_unsupported {
                return Err(ArchiveError::LibraryUnavailable(format!(
                    "libarchive {version_number} is below the required security baseline \
                     {MIN_LIBARCHIVE_VERSION}; install libarchive 3.8.9 or newer (the \
                     ARCHIVERCLICK_ALLOW_UNSUPPORTED_LIBARCHIVE=1 override is for development only)"
                )));
            }

            let mut version_notes = Vec::new();
            if version_number < MIN_LIBARCHIVE_VERSION {
                version_notes.push(format!(
                    "unsupported version {version_number}; development override enabled"
                ));
            }
            if archiveint_fallback {
                version_notes.push(
                    "archiveint.dll fallback; internal Windows ABI, unsupported for security-sensitive use"
                        .to_owned(),
                );
            }

            let read_new = required_symbol!(library, "archive_read_new", ArchiveReadNew);
            let read_free = required_symbol!(library, "archive_read_free", ArchiveReadFree);
            // Probe each in-process filter on a disposable reader.  When a
            // codec was omitted at build time, several libarchive support
            // functions return ARCHIVE_WARN and register an external command
            // fallback.  Discarding that reader and excluding the function
            // guarantees this backend never shells out while parsing.
            let filter_candidates: [(&str, ArchiveReadSupport); 11] = [
                (
                    "none",
                    required_symbol!(
                        library,
                        "archive_read_support_filter_none",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "gzip",
                    required_symbol!(
                        library,
                        "archive_read_support_filter_gzip",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "bzip2",
                    required_symbol!(
                        library,
                        "archive_read_support_filter_bzip2",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "compress",
                    required_symbol!(
                        library,
                        "archive_read_support_filter_compress",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "lzma",
                    required_symbol!(
                        library,
                        "archive_read_support_filter_lzma",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "lzip",
                    required_symbol!(
                        library,
                        "archive_read_support_filter_lzip",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "xz",
                    required_symbol!(
                        library,
                        "archive_read_support_filter_xz",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "lz4",
                    required_symbol!(
                        library,
                        "archive_read_support_filter_lz4",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "zstd",
                    required_symbol!(
                        library,
                        "archive_read_support_filter_zstd",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "uuencode",
                    required_symbol!(
                        library,
                        "archive_read_support_filter_uu",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "rpm",
                    required_symbol!(
                        library,
                        "archive_read_support_filter_rpm",
                        ArchiveReadSupport
                    ),
                ),
            ];
            let read_filters = probe_internal_filters(read_new, read_free, &filter_candidates)?;

            // Enable archive formats individually so mtree is deliberately
            // absent: mtree `contents=` entries are allowed to read unrelated
            // filesystem paths, which is not acceptable for an untrusted
            // archive viewer/extractor.
            let format_candidates: [(&str, ArchiveReadSupport); 13] = [
                (
                    "7zip",
                    required_symbol!(
                        library,
                        "archive_read_support_format_7zip",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "ar",
                    required_symbol!(
                        library,
                        "archive_read_support_format_ar",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "cab",
                    required_symbol!(
                        library,
                        "archive_read_support_format_cab",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "cpio",
                    required_symbol!(
                        library,
                        "archive_read_support_format_cpio",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "empty",
                    required_symbol!(
                        library,
                        "archive_read_support_format_empty",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "iso9660",
                    required_symbol!(
                        library,
                        "archive_read_support_format_iso9660",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "lha",
                    required_symbol!(
                        library,
                        "archive_read_support_format_lha",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "rar",
                    required_symbol!(
                        library,
                        "archive_read_support_format_rar",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "rar5",
                    required_symbol!(
                        library,
                        "archive_read_support_format_rar5",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "tar",
                    required_symbol!(
                        library,
                        "archive_read_support_format_tar",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "warc",
                    required_symbol!(
                        library,
                        "archive_read_support_format_warc",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "xar",
                    required_symbol!(
                        library,
                        "archive_read_support_format_xar",
                        ArchiveReadSupport
                    ),
                ),
                (
                    "zip",
                    required_symbol!(
                        library,
                        "archive_read_support_format_zip",
                        ArchiveReadSupport
                    ),
                ),
            ];
            let read_formats = probe_supported_formats(read_new, read_free, &format_candidates)?;
            if read_formats.is_empty() {
                return Err(ArchiveError::LibraryUnavailable(
                    "libarchive has no supported safe input formats".to_owned(),
                ));
            }
            let raw_format = required_symbol!(
                library,
                "archive_read_support_format_raw",
                ArchiveReadSupport
            );
            let read_support_format_raw =
                probe_supported_formats(read_new, read_free, &[("raw", raw_format)])?
                    .into_iter()
                    .next();

            let write = WriteApi::load(&library);
            Ok(Self {
                version_string: required_symbol!(
                    library,
                    "archive_version_string",
                    ArchiveVersionString
                ),
                version_number,
                version_note: (!version_notes.is_empty()).then(|| version_notes.join("; ")),
                read_new,
                read_filters,
                read_formats,
                read_support_format_raw,
                read_add_passphrase: optional_symbol!(
                    library,
                    "archive_read_add_passphrase",
                    ArchiveReadAddPassphrase
                ),
                read_open_filename_w: required_symbol!(
                    library,
                    "archive_read_open_filename_w",
                    ArchiveReadOpenFilenameW
                ),
                read_next_header: required_symbol!(
                    library,
                    "archive_read_next_header",
                    ArchiveReadNextHeader
                ),
                read_data: required_symbol!(library, "archive_read_data", ArchiveReadData),
                read_close: required_symbol!(library, "archive_read_close", ArchiveReadClose),
                read_free,
                read_has_encrypted_entries: optional_symbol!(
                    library,
                    "archive_read_has_encrypted_entries",
                    ArchiveReadHasEncryptedEntries
                ),
                error_string: required_symbol!(library, "archive_error_string", ArchiveErrorString),
                archive_errno: required_symbol!(library, "archive_errno", ArchiveErrno),
                format_name: required_symbol!(library, "archive_format_name", ArchiveFormatName),
                filter_name: required_symbol!(library, "archive_filter_name", ArchiveFilterName),
                filter_bytes: required_symbol!(library, "archive_filter_bytes", ArchiveFilterBytes),
                entry_pathname_w: required_symbol!(
                    library,
                    "archive_entry_pathname_w",
                    ArchiveEntryPathnameW
                ),
                entry_pathname_utf8: optional_symbol!(
                    library,
                    "archive_entry_pathname_utf8",
                    ArchiveEntryPathnameUtf8
                ),
                entry_pathname: required_symbol!(
                    library,
                    "archive_entry_pathname",
                    ArchiveEntryPathname
                ),
                entry_size: required_symbol!(library, "archive_entry_size", ArchiveEntrySize),
                entry_size_is_set: required_symbol!(
                    library,
                    "archive_entry_size_is_set",
                    ArchiveEntryFlag
                ),
                entry_filetype: required_symbol!(
                    library,
                    "archive_entry_filetype",
                    ArchiveEntryFiletype
                ),
                entry_mtime: required_symbol!(library, "archive_entry_mtime", ArchiveEntryMtime),
                entry_mtime_is_set: required_symbol!(
                    library,
                    "archive_entry_mtime_is_set",
                    ArchiveEntryFlag
                ),
                entry_hardlink_is_set: required_symbol!(
                    library,
                    "archive_entry_hardlink_is_set",
                    ArchiveEntryFlag
                ),
                entry_is_encrypted: optional_symbol!(
                    library,
                    "archive_entry_is_encrypted",
                    ArchiveEntryFlag
                ),
                write,
                _library: library,
            })
        }

        fn version(&self) -> String {
            // SAFETY: this function returns a process-lifetime static C string.
            let pointer = unsafe { (self.version_string)() };
            let base = copy_c_string(pointer)
                .unwrap_or_else(|| format!("libarchive ({})", self.version_number));
            match &self.version_note {
                Some(note) => format!("{base} [{note}]"),
                None => base,
            }
        }

        fn last_error(&self, archive: *mut RawArchive, operation: &'static str) -> ArchiveError {
            // SAFETY: the handle is live and both accessors borrow its error state.
            let message = unsafe {
                let pointer = (self.error_string)(archive);
                copy_c_string(pointer)
                    .unwrap_or_else(|| format!("error {}", (self.archive_errno)(archive)))
            };
            if looks_like_password_error(&message) {
                ArchiveError::PasswordRequired
            } else {
                ArchiveError::LibArchive { operation, message }
            }
        }
    }

    impl WriteApi {
        fn load(library: &DynamicLibrary) -> Option<Self> {
            Some(Self {
                write_new: optional_symbol!(library, "archive_write_new", ArchiveWriteNew)?,
                set_format_zip: optional_symbol!(
                    library,
                    "archive_write_set_format_zip",
                    ArchiveWriteUnary
                )?,
                set_format_pax_restricted: optional_symbol!(
                    library,
                    "archive_write_set_format_pax_restricted",
                    ArchiveWriteUnary
                )?,
                set_format_7zip: optional_symbol!(
                    library,
                    "archive_write_set_format_7zip",
                    ArchiveWriteUnary
                ),
                add_filter_gzip: optional_symbol!(
                    library,
                    "archive_write_add_filter_gzip",
                    ArchiveWriteUnary
                ),
                add_filter_xz: optional_symbol!(
                    library,
                    "archive_write_add_filter_xz",
                    ArchiveWriteUnary
                ),
                add_filter_zstd: optional_symbol!(
                    library,
                    "archive_write_add_filter_zstd",
                    ArchiveWriteUnary
                ),
                set_format_option: optional_symbol!(
                    library,
                    "archive_write_set_format_option",
                    ArchiveWriteOption
                )?,
                set_filter_option: optional_symbol!(
                    library,
                    "archive_write_set_filter_option",
                    ArchiveWriteOption
                )?,
                set_passphrase: optional_symbol!(
                    library,
                    "archive_write_set_passphrase",
                    ArchiveWriteSetPassphrase
                ),
                open_filename_w: optional_symbol!(
                    library,
                    "archive_write_open_filename_w",
                    ArchiveWriteOpenFilenameW
                )?,
                write_header: optional_symbol!(
                    library,
                    "archive_write_header",
                    ArchiveWriteHeader
                )?,
                write_data: optional_symbol!(library, "archive_write_data", ArchiveWriteData)?,
                finish_entry: optional_symbol!(
                    library,
                    "archive_write_finish_entry",
                    ArchiveWriteUnary
                )?,
                close: optional_symbol!(library, "archive_write_close", ArchiveWriteUnary)?,
                free: optional_symbol!(library, "archive_write_free", ArchiveWriteUnary)?,
                entry_new: optional_symbol!(library, "archive_entry_new", ArchiveEntryNew)?,
                entry_free: optional_symbol!(library, "archive_entry_free", ArchiveEntryFree)?,
                entry_copy_pathname_w: optional_symbol!(
                    library,
                    "archive_entry_copy_pathname_w",
                    ArchiveEntryCopyPathnameW
                )?,
                entry_set_filetype: optional_symbol!(
                    library,
                    "archive_entry_set_filetype",
                    ArchiveEntrySetFiletype
                )?,
                entry_set_perm: optional_symbol!(
                    library,
                    "archive_entry_set_perm",
                    ArchiveEntrySetMode
                )?,
                entry_set_size: optional_symbol!(
                    library,
                    "archive_entry_set_size",
                    ArchiveEntrySetSize
                )?,
                entry_set_mtime: optional_symbol!(
                    library,
                    "archive_entry_set_mtime",
                    ArchiveEntrySetMtime
                )?,
            })
        }
    }

    #[derive(Clone)]
    pub struct LibArchiveEngine {
        api: Arc<Api>,
        version: Arc<str>,
    }

    impl LibArchiveEngine {
        pub fn load() -> ArchiveResult<Self> {
            Ok(Self::from_api(load_api()?))
        }

        /// Loads exactly the libarchive DLL at `path`.
        ///
        /// Unlike [`Self::load`], this constructor never examines environment
        /// variables, application-local conventional names, or system DLLs.
        /// The explicit DLL must satisfy the normal supported-version policy.
        pub fn load_from_path(path: &Path) -> ArchiveResult<Self> {
            let path = canonical_library_file(path)?;
            let library =
                DynamicLibrary::load(&path, false).map_err(ArchiveError::LibraryUnavailable)?;
            Ok(Self::from_api(Api::from_library(library, false, false)?))
        }

        fn from_api(api: Api) -> Self {
            let api = Arc::new(api);
            let version: Arc<str> = api.version().into();
            Self { api, version }
        }
    }

    pub fn load() -> ArchiveResult<LibArchiveEngine> {
        LibArchiveEngine::load()
    }

    fn canonical_library_file(path: &Path) -> ArchiveResult<PathBuf> {
        if !path.is_absolute() {
            return Err(ArchiveError::InvalidInput(
                "libarchive DLL path must be absolute".to_owned(),
            ));
        }
        let canonical = fs::canonicalize(path).map_err(|error| ArchiveError::io(path, error))?;
        let metadata =
            fs::metadata(&canonical).map_err(|error| ArchiveError::io(&canonical, error))?;
        if !metadata.is_file() {
            return Err(ArchiveError::InvalidInput(format!(
                "libarchive DLL path is not a file: {}",
                canonical.display()
            )));
        }
        Ok(canonical)
    }

    fn load_api() -> ArchiveResult<Api> {
        let allow_unsupported = env::var("ARCHIVERCLICK_ALLOW_UNSUPPORTED_LIBARCHIVE")
            .ok()
            .as_deref()
            == Some("1");

        if let Some(configured) = env::var_os("ARCHIVERCLICK_LIBARCHIVE") {
            let path = PathBuf::from(configured);
            if !path.is_absolute() {
                return Err(ArchiveError::LibraryUnavailable(
                    "ARCHIVERCLICK_LIBARCHIVE must be an absolute path".to_owned(),
                ));
            }
            let library =
                DynamicLibrary::load(&path, false).map_err(ArchiveError::LibraryUnavailable)?;
            return Api::from_library(library, allow_unsupported, false);
        }

        let conventional = ["archive.dll", "libarchive.dll", "libarchive-13.dll"];
        let mut failures = Vec::new();

        let mut application_directories = Vec::with_capacity(2);
        if let Ok(executable) = env::current_exe()
            && let Some(directory) = executable.parent()
        {
            application_directories.push(directory.to_path_buf());
            let is_cargo_subdirectory = directory
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.eq_ignore_ascii_case("deps") || name.eq_ignore_ascii_case("examples")
                });
            if is_cargo_subdirectory && let Some(profile_directory) = directory.parent() {
                application_directories.push(profile_directory.to_path_buf());
            }
        }

        for directory in application_directories {
            for name in conventional {
                let path = directory.join(name);
                match DynamicLibrary::load(&path, false) {
                    Ok(library) => match Api::from_library(library, allow_unsupported, false) {
                        Ok(api) => return Ok(api),
                        Err(error) => failures.push(format!("{}: {error}", path.display())),
                    },
                    Err(error) => failures.push(format!("{}: {error}", path.display())),
                }
            }
        }

        for name in conventional {
            let path = Path::new(name);
            match DynamicLibrary::load(path, true) {
                Ok(library) => match Api::from_library(library, allow_unsupported, false) {
                    Ok(api) => return Ok(api),
                    Err(error) => failures.push(format!("system {name}: {error}")),
                },
                Err(error) => failures.push(format!("system {name}: {error}")),
            }
        }

        // Windows ships this internal libarchive-derived DLL on some builds.
        // Its ABI is unsupported and known versions are below our security
        // baseline, so never even load it without the explicit dev override.
        if allow_unsupported {
            let fallback = Path::new("archiveint.dll");
            match DynamicLibrary::load(fallback, true) {
                Ok(library) => match Api::from_library(library, true, true) {
                    Ok(api) => return Ok(api),
                    Err(error) => failures.push(format!("system archiveint.dll: {error}")),
                },
                Err(error) => failures.push(format!("system archiveint.dll: {error}")),
            }
        } else {
            failures
                .push("archiveint.dll skipped (development-only override is disabled)".to_owned());
        }

        Err(ArchiveError::LibraryUnavailable(failures.join("; ")))
    }

    fn probe_internal_filters(
        read_new: ArchiveReadNew,
        read_free: ArchiveReadFree,
        candidates: &[(&str, ArchiveReadSupport)],
    ) -> ArchiveResult<Vec<ArchiveReadSupport>> {
        let mut enabled = Vec::with_capacity(candidates.len());
        for &(name, support) in candidates {
            // SAFETY: each probe owns a fresh reader in NEW state.
            let raw = NonNull::new(unsafe { read_new() }).ok_or_else(|| {
                ArchiveError::LibraryUnavailable(
                    "archive_read_new returned null while probing filters".to_owned(),
                )
            })?;
            // SAFETY: support operates on a live NEW reader, then free consumes
            // the entire probe state (including any external fallback bidder).
            let status = unsafe { support(raw.as_ptr()) };
            let _ = unsafe { read_free(raw.as_ptr()) };
            match status {
                ARCHIVE_OK => enabled.push(support),
                ARCHIVE_WARN => {}
                _ => {
                    return Err(ArchiveError::LibraryUnavailable(format!(
                        "libarchive failed while probing the {name} filter (status {status})"
                    )));
                }
            }
        }
        if enabled.is_empty() {
            return Err(ArchiveError::LibraryUnavailable(
                "libarchive has no safe in-process input filters".to_owned(),
            ));
        }
        Ok(enabled)
    }

    fn probe_supported_formats(
        read_new: ArchiveReadNew,
        read_free: ArchiveReadFree,
        candidates: &[(&str, ArchiveReadSupport)],
    ) -> ArchiveResult<Vec<ArchiveReadSupport>> {
        let mut enabled = Vec::with_capacity(candidates.len());
        for &(_name, support) in candidates {
            // SAFETY: each candidate is tested on its own fresh reader so a
            // partially supported format cannot poison operational readers.
            let raw = NonNull::new(unsafe { read_new() }).ok_or_else(|| {
                ArchiveError::LibraryUnavailable(
                    "archive_read_new returned null while probing formats".to_owned(),
                )
            })?;
            // SAFETY: `raw` is a live reader in NEW state and is always freed
            // immediately after the support call.
            let status = unsafe { support(raw.as_ptr()) };
            let _ = unsafe { read_free(raw.as_ptr()) };
            if status == ARCHIVE_OK {
                enabled.push(support);
            }
        }
        Ok(enabled)
    }

    struct Reader<'a> {
        api: &'a Api,
        raw: NonNull<RawArchive>,
        closed: bool,
    }

    struct RawEntryInfo {
        path: PathBuf,
        display_path: String,
        size: Option<u64>,
        modified_unix_seconds: Option<i64>,
        kind: ArchiveEntryKind,
        encrypted: bool,
    }

    struct ScanBudget {
        entries_visited: u64,
        decoded_bytes: u64,
        max_entries: u64,
        max_decoded_bytes: u64,
        operation: &'static str,
    }

    impl ScanBudget {
        fn new(max_entries: u64, max_decoded_bytes: u64, operation: &'static str) -> Self {
            Self {
                entries_visited: 0,
                decoded_bytes: 0,
                max_entries,
                max_decoded_bytes,
                operation,
            }
        }

        fn visit_entry(&mut self) -> ArchiveResult<()> {
            self.entries_visited = self.entries_visited.checked_add(1).ok_or_else(|| {
                ArchiveError::LimitExceeded(format!("{} entry scan overflow", self.operation))
            })?;
            if self.entries_visited > self.max_entries {
                return Err(ArchiveError::LimitExceeded(format!(
                    "{} entry scan exceeds {}",
                    self.operation, self.max_entries
                )));
            }
            Ok(())
        }

        fn account_decoded(&mut self, amount: usize) -> ArchiveResult<()> {
            self.decoded_bytes =
                self.decoded_bytes
                    .checked_add(amount as u64)
                    .ok_or_else(|| {
                        ArchiveError::LimitExceeded(format!(
                            "{} decoded-data scan overflow",
                            self.operation
                        ))
                    })?;
            if self.decoded_bytes > self.max_decoded_bytes {
                return Err(ArchiveError::LimitExceeded(format!(
                    "{} decoded-data scan exceeds {} bytes",
                    self.operation, self.max_decoded_bytes
                )));
            }
            Ok(())
        }
    }

    impl<'a> Reader<'a> {
        fn open(api: &'a Api, path: &Path, password: Option<&str>) -> ArchiveResult<Self> {
            // SAFETY: constructor has no preconditions and returns an owned handle.
            let raw = NonNull::new(unsafe { (api.read_new)() }).ok_or_else(|| {
                ArchiveError::LibraryUnavailable("archive_read_new returned null".to_owned())
            })?;
            let reader = Self {
                api,
                raw,
                closed: false,
            };

            for support in &api.read_filters {
                reader.require_ok(
                    // SAFETY: reader owns a live read handle in NEW state.
                    unsafe { support(raw.as_ptr()) },
                    "enabling an archive filter",
                )?;
            }
            for support in &api.read_formats {
                reader.require_ok(
                    // SAFETY: reader owns a live read handle in NEW state.
                    unsafe { support(raw.as_ptr()) },
                    "enabling an archive format",
                )?;
            }
            if is_standalone_filter(path)? {
                let support_raw = api.read_support_format_raw.ok_or_else(|| {
                    ArchiveError::UnsupportedOption(
                        "this libarchive build cannot read standalone compressed streams"
                            .to_owned(),
                    )
                })?;
                reader.require_ok(
                    // SAFETY: reader owns a live read handle in NEW state.
                    unsafe { support_raw(raw.as_ptr()) },
                    "enabling raw compressed streams",
                )?;
            }

            if let Some(password) = password {
                let password = secret_c_string(password)?;
                let add = api.read_add_passphrase.ok_or_else(|| {
                    ArchiveError::UnsupportedOption(
                        "this libarchive build has no password reader API".to_owned(),
                    )
                })?;
                reader.require_ok(
                    // SAFETY: libarchive copies the passphrase during this call.
                    unsafe { add(raw.as_ptr(), password.as_ptr()) },
                    "adding archive password",
                )?;
            }

            let wide = wide_nul(path)?;
            reader.require_ok(
                // SAFETY: path is NUL-terminated and the handle is in NEW state.
                unsafe { (api.read_open_filename_w)(raw.as_ptr(), wide.as_ptr(), OPEN_BLOCK_SIZE) },
                "opening archive",
            )?;
            Ok(reader)
        }

        fn next_entry(&mut self, pathname_codepage: u32) -> ArchiveResult<Option<RawEntryInfo>> {
            let mut retries = 0;
            loop {
                let mut entry = ptr::null_mut();
                // SAFETY: the read handle is open; libarchive owns the entry ptr.
                let status = unsafe { (self.api.read_next_header)(self.raw.as_ptr(), &mut entry) };
                match status {
                    ARCHIVE_EOF => return Ok(None),
                    ARCHIVE_OK | ARCHIVE_WARN if !entry.is_null() => {
                        return self.copy_entry(entry, pathname_codepage).map(Some);
                    }
                    ARCHIVE_RETRY if retries < 3 => retries += 1,
                    _ => return Err(self.error("reading archive header")),
                }
            }
        }

        fn copy_entry(
            &self,
            entry: *mut RawEntry,
            pathname_codepage: u32,
        ) -> ArchiveResult<RawEntryInfo> {
            // SAFETY: entry is borrowed from the live reader until next_header.
            // Prefer the raw pathname bytes.  libarchive keeps legacy
            // (non-UTF-8) names as raw bytes, while the locale-based wide and
            // UTF-8 getters would either fail or return mojibake for them
            // under the default CRT locale.  Valid UTF-8 passes through
            // unchanged; other encodings are recovered by the heuristic
            // encoding detector (CP949 / Shift_JIS / GBK / Big5 / ...).
            let raw = unsafe {
                copy_c_string_bytes_bounded(
                    (self.api.entry_pathname)(entry),
                    MAX_ARCHIVE_PATH_UTF8_BYTES,
                    "archive raw pathname",
                )?
            };
            let path = raw
                .and_then(|raw| encoding::decode_name_with_codepage(&raw, pathname_codepage))
                .filter(|name| !name.is_empty())
                .map(PathBuf::from);
            let path = match path {
                Some(path) => path,
                None => {
                    // Formats that store Unicode directly (7z, RAR, Joliet
                    // ISO 9660) do not expose raw mbs bytes; fall back to the
                    // wide getter, which libarchive fills from its internal
                    // Unicode copy.
                    let wide_path =
                        unsafe { copy_wide_string((self.api.entry_pathname_w)(entry))? };
                    match wide_path {
                        Some(path) => PathBuf::from(path),
                        None => match self.api.entry_pathname_utf8 {
                            Some(getter) => {
                                // SAFETY: getter borrows a NUL-terminated string
                                // from this live entry; the bounded copy prevents
                                // an attacker-controlled pathname from causing an
                                // unbounded scan/allocation in the UTF-8 fallback.
                                unsafe {
                                    copy_c_string_bounded(
                                        getter(entry),
                                        MAX_ARCHIVE_PATH_UTF8_BYTES,
                                        "archive UTF-8 pathname",
                                    )?
                                }
                                .map(PathBuf::from)
                                .ok_or_else(|| {
                                    ArchiveError::LibArchive {
                                        operation: "decoding archive pathname",
                                        message: "entry pathname is missing or cannot be decoded"
                                            .to_owned(),
                                    }
                                })?
                            }
                            None => {
                                return Err(ArchiveError::LibArchive {
                                    operation: "decoding archive pathname",
                                    message: "entry pathname is missing or cannot be decoded"
                                        .to_owned(),
                                });
                            }
                        },
                    }
                }
            };

            // SAFETY: all accessors borrow immutable fields from `entry`.
            let (size, filetype, modified, hardlink, encrypted) = unsafe {
                let size = if (self.api.entry_size_is_set)(entry) != 0 {
                    u64::try_from((self.api.entry_size)(entry)).ok()
                } else {
                    None
                };
                let modified = if (self.api.entry_mtime_is_set)(entry) != 0 {
                    Some((self.api.entry_mtime)(entry))
                } else {
                    None
                };
                (
                    size,
                    (self.api.entry_filetype)(entry) & AE_IFMT,
                    modified,
                    (self.api.entry_hardlink_is_set)(entry) != 0,
                    self.api
                        .entry_is_encrypted
                        .is_some_and(|getter| getter(entry) > 0),
                )
            };

            let kind = if hardlink {
                ArchiveEntryKind::Hardlink
            } else {
                match filetype {
                    AE_IFREG => ArchiveEntryKind::File,
                    AE_IFDIR => ArchiveEntryKind::Directory,
                    AE_IFLNK => ArchiveEntryKind::Symlink,
                    _ => ArchiveEntryKind::Other,
                }
            };
            let display_path = path.to_string_lossy().into_owned();
            Ok(RawEntryInfo {
                path,
                display_path,
                size,
                modified_unix_seconds: modified,
                kind,
                encrypted,
            })
        }

        fn read(&mut self, buffer: &mut [u8]) -> ArchiveResult<usize> {
            // SAFETY: buffer is writable for its length and handle is on entry data.
            let amount = unsafe {
                (self.api.read_data)(
                    self.raw.as_ptr(),
                    buffer.as_mut_ptr().cast::<c_void>(),
                    buffer.len(),
                )
            };
            if amount < 0 {
                Err(self.error("reading archive entry data"))
            } else {
                Ok(amount as usize)
            }
        }

        fn drain_current_entry(
            &mut self,
            buffer: &mut [u8],
            cancel: &CancellationToken,
            scan: &mut ScanBudget,
            mut on_chunk: impl FnMut(u64, u64),
        ) -> ArchiveResult<u64> {
            let mut entry_bytes = 0u64;
            loop {
                check_cancel(cancel)?;
                let amount = self.read(buffer)?;
                if amount == 0 {
                    return Ok(entry_bytes);
                }
                scan.account_decoded(amount)?;
                entry_bytes = entry_bytes.checked_add(amount as u64).ok_or_else(|| {
                    ArchiveError::LimitExceeded("entry scan byte count overflow".to_owned())
                })?;
                on_chunk(entry_bytes, self.consumed_bytes());
            }
        }

        fn consumed_bytes(&self) -> u64 {
            // -1 is the client-facing/raw stream filter.
            let value = unsafe { (self.api.filter_bytes)(self.raw.as_ptr(), -1) };
            u64::try_from(value).unwrap_or(0)
        }

        fn format_name(&self) -> String {
            // SAFETY: returned strings live until the reader is freed.
            unsafe { copy_c_string((self.api.format_name)(self.raw.as_ptr())) }
                .unwrap_or_else(|| "unknown".to_owned())
        }

        fn filter_name(&self) -> Option<String> {
            // SAFETY: returned strings live until the reader is freed.
            let name = unsafe { copy_c_string((self.api.filter_name)(self.raw.as_ptr(), 0)) }?;
            (!name.eq_ignore_ascii_case("none")).then_some(name)
        }

        fn has_encrypted_entries(&self) -> bool {
            self.api.read_has_encrypted_entries.is_some_and(|getter| {
                // SAFETY: the getter only examines reader state.
                unsafe { getter(self.raw.as_ptr()) > 0 }
            })
        }

        fn finish(&mut self) -> ArchiveResult<()> {
            if self.closed {
                return Ok(());
            }
            // SAFETY: handle is live and close is called once explicitly.
            let status = unsafe { (self.api.read_close)(self.raw.as_ptr()) };
            self.closed = true;
            self.require_ok(status, "closing archive")
        }

        fn require_ok(&self, status: c_int, operation: &'static str) -> ArchiveResult<()> {
            if status >= ARCHIVE_OK {
                Ok(())
            } else {
                Err(self.error(operation))
            }
        }

        fn error(&self, operation: &'static str) -> ArchiveError {
            self.api.last_error(self.raw.as_ptr(), operation)
        }
    }

    impl Drop for Reader<'_> {
        fn drop(&mut self) {
            // SAFETY: reader uniquely owns the handle; free also closes if needed.
            let _ = unsafe { (self.api.read_free)(self.raw.as_ptr()) };
        }
    }

    struct Writer<'a> {
        api: &'a Api,
        write: &'a WriteApi,
        raw: NonNull<RawArchive>,
        finished: bool,
    }

    impl<'a> Writer<'a> {
        fn create(api: &'a Api, path: &Path, options: &CreateOptions) -> ArchiveResult<Self> {
            let write = api.write.as_ref().ok_or_else(|| {
                ArchiveError::UnsupportedOption(
                    "this libarchive DLL does not expose the archive write API".to_owned(),
                )
            })?;
            // SAFETY: constructor has no preconditions and returns an owned handle.
            let raw = NonNull::new(unsafe { (write.write_new)() }).ok_or_else(|| {
                ArchiveError::LibArchive {
                    operation: "creating archive writer",
                    message: "archive_write_new returned null".to_owned(),
                }
            })?;
            let mut writer = Self {
                api,
                write,
                raw,
                finished: false,
            };
            writer.configure(options)?;
            let wide = wide_nul(path)?;
            writer.require_ok(
                // SAFETY: writer is configured and path is NUL-terminated.
                unsafe { (write.open_filename_w)(raw.as_ptr(), wide.as_ptr()) },
                "opening archive output",
            )?;
            Ok(writer)
        }

        fn configure(&mut self, options: &CreateOptions) -> ArchiveResult<()> {
            validate_compression_level(options)?;
            match options.format {
                CreateFormat::Zip => {
                    self.call_unary(self.write.set_format_zip, "selecting ZIP format")?;
                    // Write entry names as UTF-8 and set the ZIP language
                    // encoding flag (general purpose bit 11), the standard
                    // UTF-8 extension for ZIP headers.  Without this option
                    // the writer falls back to the CRT locale and fails with
                    // "Can't translate pathname to current locale".
                    self.format_option("zip", "hdrcharset", "UTF-8")?;
                    let method = if options.compression_level == 0 {
                        "store"
                    } else {
                        "deflate"
                    };
                    self.format_option("zip", "compression", method)?;
                    self.format_option(
                        "zip",
                        "compression-level",
                        &options.compression_level.to_string(),
                    )?;
                }
                CreateFormat::SevenZip => {
                    let setter = self.write.set_format_7zip.ok_or_else(|| {
                        ArchiveError::UnsupportedOption(
                            "this libarchive build cannot write 7z archives".to_owned(),
                        )
                    })?;
                    self.call_unary(setter, "selecting 7z format")?;
                    self.format_option(
                        "7zip",
                        "compression",
                        if options.compression_level == 0 {
                            "copy"
                        } else {
                            "lzma2"
                        },
                    )?;
                    self.format_option(
                        "7zip",
                        "compression-level",
                        &options.compression_level.to_string(),
                    )?;
                }
                CreateFormat::Tar
                | CreateFormat::TarGzip
                | CreateFormat::TarXz
                | CreateFormat::TarZstd => {
                    self.call_unary(self.write.set_format_pax_restricted, "selecting TAR format")?;
                    let filter = match options.format {
                        CreateFormat::Tar => None,
                        CreateFormat::TarGzip => Some((
                            "gzip",
                            self.write.add_filter_gzip,
                            options.compression_level.to_string(),
                        )),
                        CreateFormat::TarXz => Some((
                            "xz",
                            self.write.add_filter_xz,
                            options.compression_level.to_string(),
                        )),
                        CreateFormat::TarZstd => Some((
                            "zstd",
                            self.write.add_filter_zstd,
                            options.compression_level.to_string(),
                        )),
                        _ => unreachable!(),
                    };
                    if let Some((name, setter, level)) = filter {
                        let setter = setter.ok_or_else(|| {
                            ArchiveError::UnsupportedOption(format!(
                                "this libarchive build has no {name} writer"
                            ))
                        })?;
                        self.call_unary(setter, "enabling compression filter")?;
                        self.filter_option(name, "compression-level", &level)?;
                    }
                }
            }

            if let Some(password) = options.password.as_deref() {
                if options.format != CreateFormat::Zip {
                    return Err(ArchiveError::UnsupportedOption(
                        "libarchive password creation is supported here only for ZIP".to_owned(),
                    ));
                }
                let setter = self.write.set_passphrase.ok_or_else(|| {
                    ArchiveError::UnsupportedOption(
                        "this libarchive build has no encrypted ZIP writer".to_owned(),
                    )
                })?;
                let password = secret_c_string(password)?;
                self.format_option("zip", "encryption", "aes256")?;
                self.require_ok(
                    // SAFETY: libarchive copies the password during this call.
                    unsafe { setter(self.raw.as_ptr(), password.as_ptr()) },
                    "setting archive password",
                )?;
            }
            Ok(())
        }

        fn write_header(&mut self, item: &SourceItem) -> ArchiveResult<()> {
            // SAFETY: constructor returns a uniquely owned entry.
            let entry = NonNull::new(unsafe { (self.write.entry_new)() }).ok_or_else(|| {
                ArchiveError::LibArchive {
                    operation: "allocating archive entry",
                    message: "archive_entry_new returned null".to_owned(),
                }
            })?;
            let guard = EntryGuard {
                raw: entry,
                free: self.write.entry_free,
            };
            let mut name = item.archive_name.clone();
            if item.kind == SourceKind::Directory && !name.ends_with('/') {
                name.push('/');
            }
            if name.contains('\0') {
                return Err(ArchiveError::InvalidInput(
                    "archive pathname contains NUL".to_owned(),
                ));
            }
            // The wide API keeps the entry's internal copy independent of the
            // CRT locale (the default "C" locale cannot translate UTF-8
            // names); ZIP's `hdrcharset=UTF-8` option then writes the name
            // as UTF-8 with the language-encoding flag set.
            let mut wide: Vec<u16> = name.encode_utf16().collect();
            wide.push(0);
            let raw_entry = guard.raw.as_ptr();
            // SAFETY: setters copy scalar/string metadata into the live entry.
            unsafe {
                (self.write.entry_copy_pathname_w)(raw_entry, wide.as_ptr());
                (self.write.entry_set_filetype)(
                    raw_entry,
                    u32::from(match item.kind {
                        SourceKind::File => AE_IFREG,
                        SourceKind::Directory => AE_IFDIR,
                    }),
                );
                (self.write.entry_set_perm)(
                    raw_entry,
                    match item.kind {
                        SourceKind::File => 0o644,
                        SourceKind::Directory => 0o755,
                    },
                );
                (self.write.entry_set_size)(raw_entry, item.size as i64);
                if let Some(mtime) = item.modified_unix_seconds {
                    (self.write.entry_set_mtime)(raw_entry, mtime, 0);
                }
            }
            self.require_ok(
                // SAFETY: writer is ready for a header and entry remains live.
                unsafe { (self.write.write_header)(self.raw.as_ptr(), raw_entry) },
                "writing archive header",
            )
        }

        fn write_all(&mut self, mut bytes: &[u8]) -> ArchiveResult<()> {
            while !bytes.is_empty() {
                // SAFETY: input slice is readable and writer is in DATA state.
                let amount = unsafe {
                    (self.write.write_data)(
                        self.raw.as_ptr(),
                        bytes.as_ptr().cast::<c_void>(),
                        bytes.len(),
                    )
                };
                if amount <= 0 || amount as usize > bytes.len() {
                    return Err(self.error("writing archive data"));
                }
                bytes = &bytes[amount as usize..];
            }
            Ok(())
        }

        fn finish_entry(&mut self) -> ArchiveResult<()> {
            self.call_unary(self.write.finish_entry, "finishing archive entry")
        }

        fn finish(&mut self) -> ArchiveResult<()> {
            let status = unsafe { (self.write.close)(self.raw.as_ptr()) };
            self.finished = true;
            self.require_ok(status, "closing archive output")
        }

        fn call_unary(
            &mut self,
            function: ArchiveWriteUnary,
            operation: &'static str,
        ) -> ArchiveResult<()> {
            // SAFETY: caller selects a function valid for the current writer state.
            self.require_ok(unsafe { function(self.raw.as_ptr()) }, operation)
        }

        fn format_option(&self, module: &str, option: &str, value: &str) -> ArchiveResult<()> {
            self.option(self.write.set_format_option, module, option, value)
        }

        fn filter_option(&self, module: &str, option: &str, value: &str) -> ArchiveResult<()> {
            self.option(self.write.set_filter_option, module, option, value)
        }

        fn option(
            &self,
            setter: ArchiveWriteOption,
            module: &str,
            option: &str,
            value: &str,
        ) -> ArchiveResult<()> {
            let module = CString::new(module).expect("static module contains no NUL");
            let option = CString::new(option).expect("static option contains no NUL");
            let value = CString::new(value).map_err(|_| {
                ArchiveError::InvalidInput("archive option contains NUL".to_owned())
            })?;
            // SAFETY: all C strings live for the call; writer is in NEW state.
            let status = unsafe {
                setter(
                    self.raw.as_ptr(),
                    module.as_ptr(),
                    option.as_ptr(),
                    value.as_ptr(),
                )
            };
            if status == ARCHIVE_OK {
                Ok(())
            } else {
                Err(ArchiveError::UnsupportedOption(format!(
                    "{module:?} option {option:?}: {}",
                    self.api
                        .last_error(self.raw.as_ptr(), "setting archive option")
                )))
            }
        }

        fn require_ok(&self, status: c_int, operation: &'static str) -> ArchiveResult<()> {
            if status >= ARCHIVE_OK {
                Ok(())
            } else {
                Err(self.error(operation))
            }
        }

        fn error(&self, operation: &'static str) -> ArchiveError {
            self.api.last_error(self.raw.as_ptr(), operation)
        }
    }

    impl Drop for Writer<'_> {
        fn drop(&mut self) {
            if !self.finished {
                // `archive_write_fail` aborts the writer state, but some
                // Windows builds leave the output handle open until the
                // normal close path runs.  Always close before freeing so
                // TemporaryPath can remove a cancelled/failed output file.
                let _ = unsafe { (self.write.close)(self.raw.as_ptr()) };
                self.finished = true;
            }
            // SAFETY: writer uniquely owns this handle.
            let _ = unsafe { (self.write.free)(self.raw.as_ptr()) };
        }
    }

    struct EntryGuard {
        raw: NonNull<RawEntry>,
        free: ArchiveEntryFree,
    }

    impl Drop for EntryGuard {
        fn drop(&mut self) {
            // SAFETY: guard uniquely owns this entry.
            unsafe { (self.free)(self.raw.as_ptr()) };
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SourceKind {
        File,
        Directory,
    }

    struct SourceItem {
        source: PathBuf,
        archive_name: String,
        kind: SourceKind,
        size: u64,
        modified_unix_seconds: Option<i64>,
    }

    enum RuntimeConflictPolicy {
        Ask,
        OverwriteAll,
        SkipAll,
    }

    impl From<InitialConflictPolicy> for RuntimeConflictPolicy {
        fn from(value: InitialConflictPolicy) -> Self {
            match value {
                InitialConflictPolicy::Ask => Self::Ask,
                InitialConflictPolicy::OverwriteAll => Self::OverwriteAll,
                InitialConflictPolicy::SkipAll => Self::SkipAll,
            }
        }
    }

    enum ConflictAction {
        Overwrite,
        Skip,
    }

    impl ArchiveEngine for LibArchiveEngine {
        fn version(&self) -> String {
            self.version.to_string()
        }

        fn writable_formats(&self) -> Vec<CreateFormat> {
            let Some(write) = self.api.write.as_ref() else {
                return Vec::new();
            };
            let mut formats = vec![CreateFormat::Zip, CreateFormat::Tar];
            if write.set_format_7zip.is_some() {
                formats.push(CreateFormat::SevenZip);
            }
            if write.add_filter_gzip.is_some() {
                formats.push(CreateFormat::TarGzip);
            }
            if write.add_filter_xz.is_some() {
                formats.push(CreateFormat::TarXz);
            }
            if write.add_filter_zstd.is_some() {
                formats.push(CreateFormat::TarZstd);
            }
            formats
        }

        fn list(
            &self,
            path: &Path,
            password: Option<&str>,
            pathname_codepage: u32,
            progress: &dyn ProgressSink,
            cancel: &CancellationToken,
        ) -> ArchiveResult<ArchiveListing> {
            check_cancel(cancel)?;
            let total_input = file_length(path)?;
            let throttled = ThrottledProgress::new(progress, PROGRESS_INTERVAL);
            throttled.report(opening_snapshot(path, total_input), true);
            let mut reader = Reader::open(&self.api, path, password)?;
            let mut entries = Vec::new();
            let mut total_uncompressed_size = 0u64;
            let mut total_path_bytes = 0u64;
            let mut scan = ScanBudget::new(
                MAX_LIST_ENTRIES,
                MAX_LIST_SCAN_DECODED_BYTES,
                "archive listing",
            );
            let mut buffer = vec![0u8; IO_BUFFER_SIZE];
            let mut snapshot = ProgressSnapshot::new(ProgressPhase::Listing);
            snapshot.total_bytes = Some(total_input);

            while let Some(entry) = {
                check_cancel(cancel)?;
                reader.next_entry(pathname_codepage)?
            } {
                let index = entries.len() as u64;
                scan.visit_entry()?;
                let path_bytes = u64::try_from(entry.display_path.encode_utf16().count())
                    .unwrap_or(u64::MAX)
                    .saturating_mul(2);
                total_path_bytes = checked_add_with_limit(
                    total_path_bytes,
                    path_bytes,
                    MAX_LIST_PATH_BYTES,
                    "archive listing pathname metadata",
                )?;
                if let Some(size) = entry.size {
                    total_uncompressed_size = checked_add_with_limit(
                        total_uncompressed_size,
                        size,
                        MAX_LIST_DECLARED_BYTES,
                        "archive listing declared size",
                    )?;
                }
                snapshot.current_file.clone_from(&entry.display_path);
                snapshot.entries_processed = index + 1;
                // libarchive treats directory entries as header-only.  Some
                // RAR5 archives reject archive_read_data() for those entries
                // with "Can't decompress an entry marked as a directory".
                // Only drain regular-file data while building the listing;
                // next_header() advances over header-only entries for us.
                if entry.kind == ArchiveEntryKind::File {
                    reader.drain_current_entry(
                        &mut buffer,
                        cancel,
                        &mut scan,
                        |_, consumed_input| {
                            snapshot.bytes_processed = consumed_input.min(total_input);
                            throttled.report(snapshot.clone(), false);
                        },
                    )?;
                }
                snapshot.bytes_processed = reader.consumed_bytes().min(total_input);
                throttled.report(snapshot.clone(), false);
                entries.push(ArchiveEntry {
                    index,
                    path: entry.path,
                    display_path: entry.display_path,
                    size: entry.size,
                    compressed_size: None,
                    modified_unix_seconds: entry.modified_unix_seconds,
                    kind: entry.kind,
                    encrypted: entry.encrypted,
                });
            }

            let format_name = reader.format_name();
            let filter_name = reader.filter_name();
            let archive_encrypted = reader.has_encrypted_entries();
            if archive_encrypted {
                for entry in &mut entries {
                    // Header-encrypted formats cannot expose per-entry flags.  Once
                    // the reader confirms archive encryption, conservatively mark
                    // entries whose format did not expose the individual bit.
                    entry.encrypted = true;
                }
            }
            reader.finish()?;
            snapshot.phase = ProgressPhase::Finished;
            snapshot.current_file.clear();
            snapshot.bytes_processed = total_input;
            throttled.report(snapshot, true);
            Ok(ArchiveListing {
                archive_path: path.to_path_buf(),
                format_name,
                filter_name,
                entries,
                total_uncompressed_size,
            })
        }

        fn extract(
            &self,
            archive: &Path,
            destination: &Path,
            options: &ExtractOptions,
            progress: &dyn ProgressSink,
            conflicts: &dyn ConflictResolver,
            cancel: &CancellationToken,
        ) -> ArchiveResult<OperationSummary> {
            check_cancel(cancel)?;
            fs::create_dir_all(destination)
                .map_err(|error| ArchiveError::io(destination, error))?;
            let root = fs::canonicalize(destination)
                .map_err(|error| ArchiveError::io(destination, error))?;
            verify_directory_handle(&root, &root)?;
            let total_input = file_length(archive)?;
            let throttled = ThrottledProgress::new(progress, PROGRESS_INTERVAL);
            throttled.report(opening_snapshot(archive, total_input), true);
            let mut reader = Reader::open(&self.api, archive, options.password.as_deref())?;
            let mut summary = OperationSummary::default();
            let mut selected_entries = 0u64;
            let mut progress_entries = 0u64;
            let mut progress_bytes = 0u64;
            let mut scan = ScanBudget::new(
                MAX_EXTRACT_SCAN_ENTRIES,
                MAX_EXTRACT_SCAN_DECODED_BYTES,
                "selective extraction",
            );
            let mut policy = RuntimeConflictPolicy::from(options.conflict_policy);
            let mut snapshot = ProgressSnapshot::new(ProgressPhase::Extracting);
            snapshot.total_entries = options.total_entries_hint;
            snapshot.total_bytes = options.total_bytes_hint;
            let mut buffer = vec![0u8; IO_BUFFER_SIZE];

            while let Some(entry) = {
                check_cancel(cancel)?;
                reader.next_entry(options.pathname_codepage)?
            } {
                scan.visit_entry()?;
                let relative = safe_relative_path(&entry.path)?;
                if !options.selection.includes(&relative) {
                    snapshot.current_file.clone_from(&entry.display_path);
                    snapshot.current_file_total_bytes = entry
                        .size
                        .or_else(|| (entry.kind == ArchiveEntryKind::Directory).then_some(0));
                    snapshot.current_file_bytes_processed = 0;
                    if entry.kind == ArchiveEntryKind::File {
                        reader.drain_current_entry(
                            &mut buffer,
                            cancel,
                            &mut scan,
                            |entry_bytes, _| {
                                snapshot.current_file_bytes_processed = entry_bytes;
                                throttled.report(snapshot.clone(), false)
                            },
                        )?;
                    }
                    continue;
                }
                selected_entries = selected_entries.checked_add(1).ok_or_else(|| {
                    ArchiveError::LimitExceeded("entry count overflow".to_owned())
                })?;
                if selected_entries > options.max_entries {
                    return Err(ArchiveError::LimitExceeded(format!(
                        "more than {} entries",
                        options.max_entries
                    )));
                }
                if let Some(size) = entry.size {
                    if size > options.max_file_bytes {
                        return Err(ArchiveError::LimitExceeded(format!(
                            "{} is larger than the per-file limit",
                            entry.display_path
                        )));
                    }
                    if summary.bytes_processed.saturating_add(size) > options.max_total_bytes {
                        return Err(ArchiveError::LimitExceeded(
                            "declared extraction size exceeds the total limit".to_owned(),
                        ));
                    }
                }

                snapshot.current_file.clone_from(&entry.display_path);
                let declared_progress_bytes = entry.size.unwrap_or(0);
                snapshot.current_file_total_bytes = entry
                    .size
                    .or_else(|| (entry.kind == ArchiveEntryKind::Directory).then_some(0));
                snapshot.current_file_bytes_processed = 0;
                let mut completed_progress_bytes = declared_progress_bytes;
                let target = root.join(&relative);
                ensure_no_reparse_ancestors(&root, &target)?;
                match entry.kind {
                    ArchiveEntryKind::Directory => {
                        let action = prepare_directory(&root, &target, &mut policy, conflicts)?;
                        match action {
                            ConflictAction::Overwrite => summary.entries_processed += 1,
                            ConflictAction::Skip => summary.entries_skipped += 1,
                        }
                    }
                    ArchiveEntryKind::File => {
                        match resolve_existing(&target, &mut policy, conflicts)? {
                            ConflictAction::Skip => {
                                let drained = reader.drain_current_entry(
                                    &mut buffer,
                                    cancel,
                                    &mut scan,
                                    |entry_bytes, _| {
                                        snapshot.current_file_bytes_processed = entry_bytes;
                                        snapshot.bytes_processed =
                                            progress_bytes.saturating_add(entry_bytes);
                                        snapshot.entries_processed = progress_entries;
                                        throttled.report(snapshot.clone(), false);
                                    },
                                )?;
                                completed_progress_bytes = completed_progress_bytes.max(drained);
                                summary.entries_skipped += 1;
                            }
                            ConflictAction::Overwrite => {
                                ensure_parent_directories(&root, &target)?;
                                let mut temporary = temporary_file(
                                    target.parent().expect("validated target has parent"),
                                )?;
                                verify_file_handle_within_root(
                                    &root,
                                    temporary.file(),
                                    &temporary.path,
                                )?;
                                let mut file_bytes = 0u64;
                                loop {
                                    check_cancel(cancel)?;
                                    let amount = reader.read(&mut buffer)?;
                                    if amount == 0 {
                                        break;
                                    }
                                    scan.account_decoded(amount)?;
                                    file_bytes =
                                        file_bytes.checked_add(amount as u64).ok_or_else(|| {
                                            ArchiveError::LimitExceeded(
                                                "file size overflow".to_owned(),
                                            )
                                        })?;
                                    if file_bytes > options.max_file_bytes
                                        || summary.bytes_processed.saturating_add(file_bytes)
                                            > options.max_total_bytes
                                    {
                                        return Err(ArchiveError::LimitExceeded(format!(
                                            "extracted data for {} exceeds configured limits",
                                            entry.display_path
                                        )));
                                    }
                                    temporary.file_mut().write_all(&buffer[..amount]).map_err(
                                        |error| ArchiveError::io(&temporary.path, error),
                                    )?;
                                    snapshot.current_file_bytes_processed = file_bytes;
                                    snapshot.bytes_processed =
                                        progress_bytes.saturating_add(file_bytes);
                                    snapshot.entries_processed = progress_entries;
                                    throttled.report(snapshot.clone(), false);
                                }
                                temporary
                                    .file_mut()
                                    .flush()
                                    .map_err(|error| ArchiveError::io(&temporary.path, error))?;
                                temporary.close_file();
                                install_temporary(&root, &temporary.path, &target)?;
                                temporary.disarm();
                                summary.bytes_processed = summary
                                    .bytes_processed
                                    .checked_add(file_bytes)
                                    .ok_or_else(|| {
                                        ArchiveError::LimitExceeded(
                                            "extracted byte count overflow".to_owned(),
                                        )
                                    })?;
                                summary.entries_processed += 1;
                                completed_progress_bytes = completed_progress_bytes.max(file_bytes);
                            }
                        }
                    }
                    ArchiveEntryKind::Symlink
                    | ArchiveEntryKind::Hardlink
                    | ArchiveEntryKind::Other => {
                        return Err(ArchiveError::UnsafeEntryType(entry.display_path));
                    }
                }
                progress_entries = progress_entries.checked_add(1).ok_or_else(|| {
                    ArchiveError::LimitExceeded("progress entry count overflow".to_owned())
                })?;
                progress_bytes = progress_bytes
                    .checked_add(completed_progress_bytes)
                    .ok_or_else(|| {
                        ArchiveError::LimitExceeded("progress byte count overflow".to_owned())
                    })?;
                snapshot.current_file_bytes_processed = completed_progress_bytes;
                snapshot.entries_processed = progress_entries;
                snapshot.bytes_processed = progress_bytes;
                throttled.report(snapshot.clone(), false);
            }

            reader.finish()?;
            snapshot.phase = ProgressPhase::Finished;
            snapshot.current_file.clear();
            snapshot.current_file_bytes_processed = 0;
            snapshot.current_file_total_bytes = None;
            snapshot.entries_processed = options
                .total_entries_hint
                .unwrap_or(progress_entries)
                .max(progress_entries);
            snapshot.bytes_processed = options
                .total_bytes_hint
                .unwrap_or(progress_bytes)
                .max(progress_bytes);
            throttled.report(snapshot, true);
            Ok(summary)
        }

        fn create(
            &self,
            destination: &Path,
            files: &[PathBuf],
            options: &CreateOptions,
            progress: &dyn ProgressSink,
            cancel: &CancellationToken,
        ) -> ArchiveResult<OperationSummary> {
            check_cancel(cancel)?;
            if files.is_empty() {
                return Err(ArchiveError::InvalidInput(
                    "select at least one input".to_owned(),
                ));
            }
            if options.split_size.is_some() {
                return Err(ArchiveError::UnsupportedOption(
                    "split compression requires the bundled 7z backend".to_owned(),
                ));
            }
            let parent = destination
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .ok_or_else(|| {
                    ArchiveError::InvalidInput("destination has no parent directory".to_owned())
                })?;
            fs::create_dir_all(parent).map_err(|error| ArchiveError::io(parent, error))?;
            let parent =
                fs::canonicalize(parent).map_err(|error| ArchiveError::io(parent, error))?;
            verify_directory_handle(&parent, &parent)?;
            let file_name = destination.file_name().ok_or_else(|| {
                ArchiveError::InvalidInput("destination has no file name".to_owned())
            })?;
            let final_destination = parent.join(file_name);
            ensure_no_reparse_ancestors(&parent, &final_destination)?;

            let (items, total_bytes) = collect_sources(files, &final_destination, cancel)?;
            let throttled = ThrottledProgress::new(progress, PROGRESS_INTERVAL);
            let mut opening = opening_snapshot(&final_destination, total_bytes);
            opening.total_entries = Some(items.len() as u64);
            throttled.report(opening, true);
            let mut temporary = temporary_file(&parent)?;
            verify_file_handle_within_root(&parent, temporary.file(), &temporary.path)?;
            temporary.close_file();
            let mut writer = Writer::create(&self.api, &temporary.path, options)?;
            let mut summary = OperationSummary::default();
            let mut snapshot = ProgressSnapshot::new(ProgressPhase::Compressing);
            snapshot.total_entries = Some(items.len() as u64);
            snapshot.total_bytes = Some(total_bytes);
            let mut buffer = vec![0u8; IO_BUFFER_SIZE];
            let mut next_progress_bytes = 0u64;

            for item in &items {
                check_cancel(cancel)?;
                snapshot.current_file.clone_from(&item.archive_name);
                snapshot.current_file_total_bytes = Some(item.size);
                snapshot.current_file_bytes_processed = 0;
                writer.write_header(item)?;
                if item.kind == SourceKind::File {
                    let mut input = File::open(&item.source)
                        .map_err(|error| ArchiveError::io(&item.source, error))?;
                    let mut remaining = item.size;
                    while remaining > 0 {
                        check_cancel(cancel)?;
                        let wanted = usize::try_from(remaining.min(buffer.len() as u64))
                            .expect("buffer size fits usize");
                        let amount = input
                            .read(&mut buffer[..wanted])
                            .map_err(|error| ArchiveError::io(&item.source, error))?;
                        if amount == 0 {
                            return Err(ArchiveError::Io {
                                path: item.source.clone(),
                                source: std::io::Error::new(
                                    std::io::ErrorKind::UnexpectedEof,
                                    "file changed while it was being archived",
                                ),
                            });
                        }
                        writer.write_all(&buffer[..amount])?;
                        remaining -= amount as u64;
                        snapshot.current_file_bytes_processed = item.size.saturating_sub(remaining);
                        summary.bytes_processed += amount as u64;
                        snapshot.bytes_processed = summary.bytes_processed;
                        snapshot.entries_processed = summary.entries_processed;
                        if summary.bytes_processed >= next_progress_bytes {
                            throttled.report(snapshot.clone(), false);
                            next_progress_bytes = summary
                                .bytes_processed
                                .saturating_add(IO_BUFFER_SIZE as u64);
                        }
                    }
                }
                writer.finish_entry()?;
                summary.entries_processed += 1;
                snapshot.current_file_bytes_processed = item.size;
                snapshot.entries_processed = summary.entries_processed;
                throttled.report(snapshot.clone(), false);
            }

            writer.finish()?;
            drop(writer);
            install_temporary(&parent, &temporary.path, &final_destination)?;
            temporary.disarm();
            snapshot.phase = ProgressPhase::Finished;
            snapshot.current_file.clear();
            snapshot.current_file_bytes_processed = 0;
            snapshot.current_file_total_bytes = None;
            snapshot.entries_processed = summary.entries_processed;
            snapshot.bytes_processed = summary.bytes_processed;
            throttled.report(snapshot, true);
            Ok(summary)
        }

        fn test(
            &self,
            archive: &Path,
            password: Option<&str>,
            progress: &dyn ProgressSink,
            cancel: &CancellationToken,
        ) -> ArchiveResult<OperationSummary> {
            check_cancel(cancel)?;
            let total_input = file_length(archive)?;
            let throttled = ThrottledProgress::new(progress, PROGRESS_INTERVAL);
            throttled.report(opening_snapshot(archive, total_input), true);
            let mut reader = Reader::open(&self.api, archive, password)?;
            let mut summary = OperationSummary::default();
            let mut snapshot = ProgressSnapshot::new(ProgressPhase::Testing);
            snapshot.total_bytes = Some(total_input);
            let mut buffer = vec![0u8; IO_BUFFER_SIZE];

            while let Some(entry) = {
                check_cancel(cancel)?;
                reader.next_entry(0)?
            } {
                enforce_limit(
                    summary.entries_processed.saturating_add(1),
                    MAX_TEST_ENTRIES,
                    "archive test entry count",
                )?;
                snapshot.current_file = entry.display_path;
                if entry.kind == ArchiveEntryKind::File {
                    loop {
                        check_cancel(cancel)?;
                        let amount = reader.read(&mut buffer)?;
                        if amount == 0 {
                            break;
                        }
                        summary.bytes_processed = checked_add_with_limit(
                            summary.bytes_processed,
                            amount as u64,
                            MAX_TEST_OUTPUT_BYTES,
                            "archive test decompressed data",
                        )?;
                        snapshot.bytes_processed = reader.consumed_bytes().min(total_input);
                        snapshot.entries_processed = summary.entries_processed;
                        throttled.report(snapshot.clone(), false);
                    }
                }
                summary.entries_processed += 1;
                snapshot.entries_processed = summary.entries_processed;
                snapshot.bytes_processed = reader.consumed_bytes().min(total_input);
                throttled.report(snapshot.clone(), false);
            }
            reader.finish()?;
            snapshot.phase = ProgressPhase::Finished;
            snapshot.current_file.clear();
            snapshot.bytes_processed = total_input;
            snapshot.entries_processed = summary.entries_processed;
            throttled.report(snapshot, true);
            Ok(summary)
        }
    }

    fn opening_snapshot(path: &Path, total_bytes: u64) -> ProgressSnapshot {
        let mut snapshot = ProgressSnapshot::new(ProgressPhase::Opening);
        snapshot.current_file = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned();
        snapshot.total_bytes = Some(total_bytes);
        snapshot
    }

    fn file_length(path: &Path) -> ArchiveResult<u64> {
        fs::metadata(path)
            .map(|metadata| metadata.len())
            .map_err(|error| ArchiveError::io(path, error))
    }

    fn check_cancel(cancel: &CancellationToken) -> ArchiveResult<()> {
        if cancel.is_cancelled() {
            Err(ArchiveError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn enforce_limit(value: u64, limit: u64, subject: &str) -> ArchiveResult<()> {
        if value > limit {
            Err(ArchiveError::LimitExceeded(format!(
                "{subject} exceeds {limit}"
            )))
        } else {
            Ok(())
        }
    }

    fn checked_add_with_limit(
        current: u64,
        amount: u64,
        limit: u64,
        subject: &str,
    ) -> ArchiveResult<u64> {
        let value = current
            .checked_add(amount)
            .ok_or_else(|| ArchiveError::LimitExceeded(format!("{subject} overflow")))?;
        enforce_limit(value, limit, subject)?;
        Ok(value)
    }

    fn resolve_existing(
        target: &Path,
        policy: &mut RuntimeConflictPolicy,
        resolver: &dyn ConflictResolver,
    ) -> ArchiveResult<ConflictAction> {
        match fs::symlink_metadata(target) {
            Ok(metadata) => {
                if is_reparse(&metadata) {
                    return Err(ArchiveError::ReparsePoint(target.to_path_buf()));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ConflictAction::Overwrite);
            }
            Err(error) => return Err(ArchiveError::io(target, error)),
        }

        match policy {
            RuntimeConflictPolicy::OverwriteAll => Ok(ConflictAction::Overwrite),
            RuntimeConflictPolicy::SkipAll => Ok(ConflictAction::Skip),
            RuntimeConflictPolicy::Ask => match resolver.resolve(target) {
                ConflictChoice::Overwrite => Ok(ConflictAction::Overwrite),
                ConflictChoice::Skip => Ok(ConflictAction::Skip),
                ConflictChoice::OverwriteAll => {
                    *policy = RuntimeConflictPolicy::OverwriteAll;
                    Ok(ConflictAction::Overwrite)
                }
                ConflictChoice::SkipAll => {
                    *policy = RuntimeConflictPolicy::SkipAll;
                    Ok(ConflictAction::Skip)
                }
                ConflictChoice::Cancel => Err(ArchiveError::Cancelled),
            },
        }
    }

    fn prepare_directory(
        root: &Path,
        target: &Path,
        policy: &mut RuntimeConflictPolicy,
        resolver: &dyn ConflictResolver,
    ) -> ArchiveResult<ConflictAction> {
        match fs::symlink_metadata(target) {
            Ok(metadata) if is_reparse(&metadata) => {
                return Err(ArchiveError::ReparsePoint(target.to_path_buf()));
            }
            Ok(metadata) if metadata.is_dir() => return Ok(ConflictAction::Overwrite),
            Ok(_) => match resolve_existing(target, policy, resolver)? {
                ConflictAction::Skip => return Ok(ConflictAction::Skip),
                ConflictAction::Overwrite => {
                    fs::remove_file(target).map_err(|error| ArchiveError::io(target, error))?;
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ArchiveError::io(target, error)),
        }
        ensure_parent_directories(root, target)?;
        fs::create_dir(target)
            .or_else(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .map_err(|error| ArchiveError::io(target, error))?;
        ensure_no_reparse_ancestors(root, target)?;
        verify_directory_handle(root, target)?;
        Ok(ConflictAction::Overwrite)
    }

    fn ensure_parent_directories(root: &Path, target: &Path) -> ArchiveResult<()> {
        let parent = target
            .parent()
            .ok_or_else(|| ArchiveError::UnsafeEntryPath(target.display().to_string()))?;
        ensure_no_reparse_ancestors(root, parent)?;
        fs::create_dir_all(parent).map_err(|error| ArchiveError::io(parent, error))?;
        ensure_no_reparse_ancestors(root, parent)?;
        verify_directory_handle(root, parent)
    }

    fn verification_handle(path: &Path) -> ArchiveResult<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|error| ArchiveError::io(path, error))
    }

    fn final_path_by_handle(file: &File, subject: &Path) -> ArchiveResult<PathBuf> {
        let mut buffer = vec![0u16; 512];
        loop {
            // SAFETY: `file` owns a live Windows handle and `buffer` is writable
            // for the supplied capacity. Zero selects normalized DOS paths.
            let length = unsafe {
                GetFinalPathNameByHandleW(
                    file.as_raw_handle(),
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    0,
                )
            };
            if length == 0 {
                return Err(ArchiveError::io(subject, std::io::Error::last_os_error()));
            }
            let length = length as usize;
            if length < buffer.len() {
                buffer.truncate(length);
                return Ok(PathBuf::from(OsString::from_wide(&buffer)));
            }
            let required = length.checked_add(1).ok_or_else(|| {
                ArchiveError::LimitExceeded("resolved Windows path length overflow".to_owned())
            })?;
            if required > MAX_ARCHIVE_PATH_UNITS {
                return Err(ArchiveError::LimitExceeded(
                    "resolved Windows path is too long".to_owned(),
                ));
            }
            buffer.resize(required, 0);
        }
    }

    fn path_is_within_case_insensitive(candidate: &Path, root: &Path) -> bool {
        let mut candidate = candidate.components();
        root.components().all(|root_component| {
            candidate.next().is_some_and(|component| {
                component
                    .as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&root_component.as_os_str().to_string_lossy())
            })
        })
    }

    fn verified_root_final_path(root: &Path) -> ArchiveResult<PathBuf> {
        let metadata = fs::symlink_metadata(root).map_err(|error| ArchiveError::io(root, error))?;
        if is_reparse(&metadata) {
            return Err(ArchiveError::ReparsePoint(root.to_path_buf()));
        }
        let handle = verification_handle(root)?;
        final_path_by_handle(&handle, root)
    }

    fn verify_directory_handle(root: &Path, directory: &Path) -> ArchiveResult<()> {
        let root_final = verified_root_final_path(root)?;
        let metadata =
            fs::symlink_metadata(directory).map_err(|error| ArchiveError::io(directory, error))?;
        if is_reparse(&metadata) {
            return Err(ArchiveError::ReparsePoint(directory.to_path_buf()));
        }
        if !metadata.is_dir() {
            return Err(ArchiveError::UnsafeEntryPath(
                directory.display().to_string(),
            ));
        }
        let handle = verification_handle(directory)?;
        let resolved = final_path_by_handle(&handle, directory)?;
        if path_is_within_case_insensitive(&resolved, &root_final) {
            Ok(())
        } else {
            Err(ArchiveError::UnsafeEntryPath(format!(
                "{} resolves outside {}",
                directory.display(),
                root.display()
            )))
        }
    }

    fn verify_file_handle_within_root(
        root: &Path,
        file: &File,
        subject: &Path,
    ) -> ArchiveResult<()> {
        let root_final = verified_root_final_path(root)?;
        let resolved = final_path_by_handle(file, subject)?;
        if path_is_within_case_insensitive(&resolved, &root_final) {
            Ok(())
        } else {
            Err(ArchiveError::UnsafeEntryPath(format!(
                "{} resolves outside {}",
                subject.display(),
                root.display()
            )))
        }
    }

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TemporaryPath {
        path: PathBuf,
        file: Option<File>,
        armed: bool,
    }

    impl TemporaryPath {
        fn file(&self) -> &File {
            self.file
                .as_ref()
                .expect("temporary file handle is still open")
        }

        fn file_mut(&mut self) -> &mut File {
            self.file
                .as_mut()
                .expect("temporary file handle is still open")
        }

        fn close_file(&mut self) {
            drop(self.file.take());
        }

        fn disarm(&mut self) {
            self.armed = false;
        }
    }

    impl Drop for TemporaryPath {
        fn drop(&mut self) {
            self.close_file();
            if self.armed {
                let _ = fs::remove_file(&self.path);
            }
        }
    }

    fn temporary_file(directory: &Path) -> ArchiveResult<TemporaryPath> {
        for _ in 0..128 {
            let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!(".archiverclick-{}-{id}.tmp", std::process::id()));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(TemporaryPath {
                        path,
                        file: Some(file),
                        armed: true,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(ArchiveError::io(path, error)),
            }
        }
        Err(ArchiveError::InvalidInput(
            "could not allocate a unique temporary file".to_owned(),
        ))
    }

    fn install_temporary(root: &Path, temporary: &Path, target: &Path) -> ArchiveResult<()> {
        ensure_no_reparse_ancestors(root, target)?;
        let target_parent = target
            .parent()
            .ok_or_else(|| ArchiveError::UnsafeEntryPath(target.display().to_string()))?;
        verify_directory_handle(root, target_parent)?;
        match fs::symlink_metadata(target) {
            Ok(metadata) if is_reparse(&metadata) => {
                return Err(ArchiveError::ReparsePoint(target.to_path_buf()));
            }
            Ok(metadata) if metadata.is_dir() => {
                return Err(ArchiveError::InvalidInput(format!(
                    "cannot replace a directory with a file: {}",
                    target.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ArchiveError::io(target, error)),
        }
        ensure_no_reparse_ancestors(root, target)?;
        verify_directory_handle(root, target_parent)?;
        let existing = wide_nul(temporary)?;
        let replacement = wide_nul(target)?;
        // SAFETY: both strings are NUL-terminated and the paths share a
        // directory/volume. REPLACE_EXISTING preserves the old file if the
        // atomic rename cannot be completed. The handle-resolved parent check
        // above detects path swaps that precede it. A same-user swap in the
        // final path-based Win32 call's narrow race window cannot be eliminated
        // without NT handle-relative rename APIs; see the security handoff.
        let moved = unsafe {
            MoveFileExW(
                existing.as_ptr(),
                replacement.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if moved != 0 {
            Ok(())
        } else {
            Err(ArchiveError::io(target, std::io::Error::last_os_error()))
        }
    }

    fn collect_sources(
        files: &[PathBuf],
        destination: &Path,
        cancel: &CancellationToken,
    ) -> ArchiveResult<(Vec<SourceItem>, u64)> {
        let mut items = Vec::new();
        let mut names = HashSet::new();
        let mut total = 0u64;
        for source in files {
            check_cancel(cancel)?;
            let selected_metadata =
                fs::symlink_metadata(source).map_err(|error| ArchiveError::io(source, error))?;
            if is_reparse(&selected_metadata) {
                return Err(ArchiveError::UnsafeEntryType(format!(
                    "input is a link or reparse point: {}",
                    source.display()
                )));
            }
            let source =
                fs::canonicalize(source).map_err(|error| ArchiveError::io(source, error))?;
            if destination.starts_with(&source) {
                return Err(ArchiveError::InvalidInput(format!(
                    "archive destination is inside selected input {}",
                    source.display()
                )));
            }
            if source == destination {
                return Err(ArchiveError::InvalidInput(
                    "archive cannot contain its own output".to_owned(),
                ));
            }
            let root_name = source.file_name().ok_or_else(|| {
                ArchiveError::InvalidInput(format!(
                    "cannot derive an archive name for {}",
                    source.display()
                ))
            })?;
            let root_name = root_name.to_str().ok_or_else(|| {
                ArchiveError::InvalidInput(format!(
                    "input name is not valid Unicode: {}",
                    source.display()
                ))
            })?;
            if selected_metadata.is_file() && is_thumbs_db_name(root_name) {
                continue;
            }
            let archive_name = if files.len() == 1 && selected_metadata.is_dir() {
                String::new()
            } else {
                root_name.to_owned()
            };
            collect_source(
                &source,
                archive_name,
                destination,
                cancel,
                &mut names,
                &mut items,
                &mut total,
            )?;
        }
        Ok((items, total))
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_source(
        source: &Path,
        archive_name: String,
        destination: &Path,
        cancel: &CancellationToken,
        names: &mut HashSet<String>,
        items: &mut Vec<SourceItem>,
        total: &mut u64,
    ) -> ArchiveResult<()> {
        check_cancel(cancel)?;
        if source == destination {
            return Err(ArchiveError::InvalidInput(
                "archive cannot contain its own output".to_owned(),
            ));
        }
        let metadata =
            fs::symlink_metadata(source).map_err(|error| ArchiveError::io(source, error))?;
        if is_reparse(&metadata) {
            return Err(ArchiveError::UnsafeEntryType(format!(
                "input is a link or reparse point: {}",
                source.display()
            )));
        }
        if !archive_name.is_empty() {
            let key = archive_name.to_lowercase();
            if !names.insert(key) {
                return Err(ArchiveError::InvalidInput(format!(
                    "duplicate archive pathname {archive_name}"
                )));
            }
        }
        let modified_unix_seconds = metadata.modified().ok().and_then(system_time_seconds);
        if metadata.is_file() {
            let size = metadata.len();
            if size > i64::MAX as u64 {
                return Err(ArchiveError::LimitExceeded(format!(
                    "{} is too large for libarchive",
                    source.display()
                )));
            }
            *total = total.checked_add(size).ok_or_else(|| {
                ArchiveError::LimitExceeded("input byte count overflow".to_owned())
            })?;
            items.push(SourceItem {
                source: source.to_path_buf(),
                archive_name,
                kind: SourceKind::File,
                size,
                modified_unix_seconds,
            });
            return Ok(());
        }
        if !metadata.is_dir() {
            return Err(ArchiveError::UnsafeEntryType(format!(
                "unsupported input type: {}",
                source.display()
            )));
        }
        if !archive_name.is_empty() {
            items.push(SourceItem {
                source: source.to_path_buf(),
                archive_name: archive_name.clone(),
                kind: SourceKind::Directory,
                size: 0,
                modified_unix_seconds,
            });
        }
        let mut children = fs::read_dir(source)
            .map_err(|error| ArchiveError::io(source, error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| ArchiveError::io(source, error))?;
        // `sort_by_key` rebuilds the lowercase allocation for every
        // comparison.  Directory enumeration can contain thousands of
        // siblings, so cache the key once per entry instead.
        children.sort_by_cached_key(|entry| entry.file_name().to_string_lossy().to_lowercase());
        for child in children {
            let child_name = child.file_name();
            let child_name = child_name.to_str().ok_or_else(|| {
                ArchiveError::InvalidInput(format!(
                    "input name is not valid Unicode: {}",
                    child.path().display()
                ))
            })?;
            if child_name.contains(['/', '\\', '\0']) {
                return Err(ArchiveError::InvalidInput(format!(
                    "invalid input name {child_name:?}"
                )));
            }
            if !child.path().is_dir() && is_thumbs_db_name(child_name) {
                continue;
            }
            let child_archive_name = if archive_name.is_empty() {
                child_name.to_owned()
            } else {
                format!("{archive_name}/{child_name}")
            };
            collect_source(
                &child.path(),
                child_archive_name,
                destination,
                cancel,
                names,
                items,
                total,
            )?;
        }
        Ok(())
    }

    fn is_thumbs_db_name(name: &str) -> bool {
        name.eq_ignore_ascii_case("Thumbs.db")
    }

    fn validate_compression_level(options: &CreateOptions) -> ArchiveResult<()> {
        let maximum = match options.format {
            CreateFormat::TarZstd => 22,
            _ => 9,
        };
        if options.compression_level > maximum {
            return Err(ArchiveError::UnsupportedOption(format!(
                "compression level {} is outside 0..={maximum}",
                options.compression_level
            )));
        }
        Ok(())
    }

    fn system_time_seconds(time: SystemTime) -> Option<i64> {
        match time.duration_since(UNIX_EPOCH) {
            Ok(duration) => i64::try_from(duration.as_secs()).ok(),
            Err(error) => i64::try_from(error.duration().as_secs())
                .ok()
                .and_then(|seconds| seconds.checked_neg()),
        }
    }

    fn is_reparse(metadata: &fs::Metadata) -> bool {
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    fn is_standalone_filter(path: &Path) -> ArchiveResult<bool> {
        let mut file = File::open(path).map_err(|error| ArchiveError::io(path, error))?;
        let mut signature = [0u8; 8];
        let amount = file
            .read(&mut signature)
            .map_err(|error| ArchiveError::io(path, error))?;
        let bytes = &signature[..amount];
        Ok(bytes.starts_with(&[0x1f, 0x8b])
            || bytes.starts_with(b"BZh")
            || bytes.starts_with(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00])
            || bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd])
            || bytes.starts_with(&[0x04, 0x22, 0x4d, 0x18])
            || bytes.starts_with(b"LZIP")
            || bytes.starts_with(&[0x1f, 0x9d]))
    }

    fn wide_nul(path: &Path) -> ArchiveResult<Vec<u16>> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(ArchiveError::InvalidInput(format!(
                "path contains NUL: {}",
                path.display()
            )));
        }
        wide.push(0);
        Ok(wide)
    }

    unsafe fn copy_wide_string(pointer: *const u16) -> ArchiveResult<Option<OsString>> {
        if pointer.is_null() {
            return Ok(None);
        }
        let mut length = 0usize;
        // SAFETY: libarchive promises a NUL-terminated wchar_t string.  The
        // explicit cap prevents pathological metadata from consuming memory.
        while length <= MAX_ARCHIVE_PATH_UNITS && unsafe { *pointer.add(length) } != 0 {
            length += 1;
        }
        if length > MAX_ARCHIVE_PATH_UNITS {
            return Err(ArchiveError::LimitExceeded(
                "archive pathname is longer than 1,048,576 UTF-16 units".to_owned(),
            ));
        }
        // SAFETY: the scan above established `length` initialized units.
        let units = unsafe { std::slice::from_raw_parts(pointer, length) };
        Ok(Some(OsString::from_wide(units)))
    }

    unsafe fn copy_c_string_bounded(
        pointer: *const c_char,
        max_bytes: usize,
        subject: &str,
    ) -> ArchiveResult<Option<String>> {
        if pointer.is_null() {
            return Ok(None);
        }
        let mut length = 0usize;
        // SAFETY: libarchive promises a NUL-terminated string. The explicit
        // maximum bounds the attacker-influenced scan and resulting allocation.
        while length <= max_bytes && unsafe { *pointer.add(length) } != 0 {
            length += 1;
        }
        if length > max_bytes {
            return Err(ArchiveError::LimitExceeded(format!(
                "{subject} exceeds {max_bytes} bytes"
            )));
        }
        // SAFETY: the bounded scan established `length` initialized bytes.
        let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), length) };
        Ok(Some(String::from_utf8_lossy(bytes).into_owned()))
    }

    unsafe fn copy_c_string_bytes_bounded(
        pointer: *const c_char,
        max_bytes: usize,
        subject: &str,
    ) -> ArchiveResult<Option<Vec<u8>>> {
        if pointer.is_null() {
            return Ok(None);
        }
        let mut length = 0usize;
        // SAFETY: libarchive promises a NUL-terminated string. The explicit
        // maximum bounds the attacker-influenced scan and resulting allocation.
        while length <= max_bytes && unsafe { *pointer.add(length) } != 0 {
            length += 1;
        }
        if length > max_bytes {
            return Err(ArchiveError::LimitExceeded(format!(
                "{subject} exceeds {max_bytes} bytes"
            )));
        }
        // SAFETY: the bounded scan established `length` initialized bytes.
        let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), length) };
        Ok(Some(bytes.to_vec()))
    }

    fn copy_c_string(pointer: *const c_char) -> Option<String> {
        if pointer.is_null() {
            None
        } else {
            // SAFETY: all callers pass libarchive-owned NUL-terminated strings.
            Some(
                unsafe { CStr::from_ptr(pointer) }
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }

    fn secret_c_string(secret: &str) -> ArchiveResult<CString> {
        if secret.is_empty() {
            return Err(ArchiveError::InvalidInput(
                "archive password cannot be empty".to_owned(),
            ));
        }
        CString::new(secret)
            .map_err(|_| ArchiveError::InvalidInput("archive password contains NUL".to_owned()))
    }

    fn looks_like_password_error(message: &str) -> bool {
        let message = message.to_ascii_lowercase();
        [
            "passphrase required",
            "passphrase is required",
            "password required",
            "password is required",
            "no passphrase",
            "no password",
            "incorrect passphrase",
            "incorrect password",
            "wrong passphrase",
            "wrong password",
            "invalid passphrase",
            "invalid password",
            "bad passphrase",
            "bad password",
        ]
        .iter()
        .any(|needle| message.contains(needle))
    }

    #[cfg(test)]
    mod tests {
        use super::{
            ARCHIVE_OK, ARCHIVE_WARN, ArchiveError, ArchiveReadSupport, LibArchiveEngine,
            MAX_LIST_ENTRIES, MAX_LIST_PATH_BYTES, MAX_SUPPORTED_LIBARCHIVE_VERSION,
            MAX_TEST_OUTPUT_BYTES, RawArchive, ScanBudget, canonical_library_file,
            checked_add_with_limit, copy_c_string_bounded, copy_c_string_bytes_bounded,
            enforce_limit, ensure_supported_libarchive_abi, looks_like_password_error,
            probe_supported_formats, system_time_seconds,
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
            let engine = LibArchiveEngine::load_from_path(&dll)
                .expect("explicit bundled libarchive path loads");
            assert!(engine.version.starts_with("libarchive 3."));
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
                checked_add_with_limit(MAX_LIST_PATH_BYTES - 1, 1, MAX_LIST_PATH_BYTES, "paths")
                    .unwrap(),
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
    }
}

#[cfg(not(windows))]
mod platform_impl {
    use std::path::{Path, PathBuf};

    use crate::tasks::{CancellationToken, ProgressSnapshot};

    use super::super::{
        ArchiveEngine, ArchiveError, ArchiveListing, ArchiveResult, ConflictResolver,
        CreateOptions, ExtractOptions, OperationSummary, ProgressSink,
    };

    #[derive(Clone, Default)]
    pub struct LibArchiveEngine;

    impl LibArchiveEngine {
        pub fn load() -> ArchiveResult<Self> {
            Err(unavailable())
        }

        pub fn load_from_path(_path: &Path) -> ArchiveResult<Self> {
            Err(unavailable())
        }
    }

    pub fn load() -> ArchiveResult<LibArchiveEngine> {
        LibArchiveEngine::load()
    }

    fn unavailable() -> ArchiveError {
        ArchiveError::LibraryUnavailable(
            "the dynamic libarchive backend is currently Windows-only".to_owned(),
        )
    }

    impl ArchiveEngine for LibArchiveEngine {
        fn version(&self) -> String {
            "libarchive unavailable".to_owned()
        }

        fn writable_formats(&self) -> Vec<super::super::CreateFormat> {
            Vec::new()
        }

        fn list(
            &self,
            _path: &Path,
            _password: Option<&str>,
            _pathname_codepage: u32,
            _progress: &dyn ProgressSink,
            _cancel: &CancellationToken,
        ) -> ArchiveResult<ArchiveListing> {
            Err(unavailable())
        }

        fn extract(
            &self,
            _archive: &Path,
            _destination: &Path,
            _options: &ExtractOptions,
            _progress: &dyn ProgressSink,
            _conflicts: &dyn ConflictResolver,
            _cancel: &CancellationToken,
        ) -> ArchiveResult<OperationSummary> {
            Err(unavailable())
        }

        fn create(
            &self,
            _destination: &Path,
            _files: &[PathBuf],
            _options: &CreateOptions,
            _progress: &dyn ProgressSink,
            _cancel: &CancellationToken,
        ) -> ArchiveResult<OperationSummary> {
            Err(unavailable())
        }

        fn test(
            &self,
            _archive: &Path,
            _password: Option<&str>,
            _progress: &dyn ProgressSink,
            _cancel: &CancellationToken,
        ) -> ArchiveResult<OperationSummary> {
            Err(unavailable())
        }
    }

    #[allow(dead_code)]
    fn _assert_progress_type(_: ProgressSnapshot) {}
}

pub use platform_impl::{LibArchiveEngine, load};
