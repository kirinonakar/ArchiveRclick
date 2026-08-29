//! Dynamic libarchive loading, ABI bindings, and capability probing.

use super::*;

pub(super) const ARCHIVE_EOF: c_int = 1;
pub(super) const ARCHIVE_OK: c_int = 0;
pub(super) const ARCHIVE_RETRY: c_int = -10;
pub(super) const ARCHIVE_WARN: c_int = -20;

// 3.8.9 is a security release and includes the malformed-archive
// hardening accumulated in 3.8.8.  Older parsers are only available via
// an explicit development escape hatch; archive files are untrusted input.
pub(super) const MIN_LIBARCHIVE_VERSION: c_int = 3_008_009;
// The 4.0 development ABI changes public mode/time types.  Bindings in
// this module intentionally target the stable libarchive 3.x ABI only.
pub(super) const MAX_SUPPORTED_LIBARCHIVE_VERSION: c_int = 3_999_000;

// libarchive 3.x defines __LA_MODE_T as unsigned short on Windows.
pub(super) const AE_IFMT: u16 = 0o170000;
pub(super) const AE_IFREG: u16 = 0o100000;
pub(super) const AE_IFLNK: u16 = 0o120000;
pub(super) const AE_IFDIR: u16 = 0o040000;

// Keep the Rust-to-libarchive handoff large enough that file I/O and FFI
// call overhead stay negligible next to the codec work.  libarchive has
// its own internal buffering, so this is deliberately a moderate 1 MiB
// staging buffer rather than an unbounded read-ahead cache.
pub(super) const IO_BUFFER_SIZE: usize = 1024 * 1024;
pub(super) const OPEN_BLOCK_SIZE: usize = 64 * 1024;
pub(super) const MAX_ARCHIVE_PATH_UNITS: usize = 1024 * 1024;
pub(super) const MAX_ARCHIVE_PATH_UTF8_BYTES: usize = 4 * 1024 * 1024;
pub(super) const MAX_LIST_ENTRIES: u64 = 1_000_000;
pub(super) const MAX_LIST_SCAN_DECODED_BYTES: u64 = 64 * 1024 * 1024 * 1024;
pub(super) const MAX_LIST_PATH_BYTES: u64 = 256 * 1024 * 1024;
pub(super) const MAX_LIST_DECLARED_BYTES: u64 = 512 * 1024 * 1024 * 1024 * 1024;
pub(super) const MAX_EXTRACT_SCAN_ENTRIES: u64 = 1_000_000;
pub(super) const MAX_EXTRACT_SCAN_DECODED_BYTES: u64 = 512 * 1024 * 1024 * 1024;
pub(super) const MAX_TEST_ENTRIES: u64 = 1_000_000;
pub(super) const MAX_TEST_OUTPUT_BYTES: u64 = 512 * 1024 * 1024 * 1024;
pub(super) const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
pub(super) const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
pub(super) const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
pub(super) const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
pub(super) const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
pub(super) const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

#[link(name = "Kernel32")]
unsafe extern "system" {
    pub(super) fn GetFinalPathNameByHandleW(
        file: *mut c_void,
        path: *mut u16,
        path_units: u32,
        flags: u32,
    ) -> u32;
    pub(super) fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
}

#[repr(C)]
pub(super) struct RawArchive {
    pub(super) _private: [u8; 0],
}

#[repr(C)]
pub(super) struct RawEntry {
    pub(super) _private: [u8; 0],
}

pub(super) type ArchiveVersionString = unsafe extern "C" fn() -> *const c_char;
pub(super) type ArchiveVersionNumber = unsafe extern "C" fn() -> c_int;
pub(super) type ArchiveReadNew = unsafe extern "C" fn() -> *mut RawArchive;
pub(super) type ArchiveReadSupport = unsafe extern "C" fn(*mut RawArchive) -> c_int;
pub(super) type ArchiveReadAddPassphrase =
    unsafe extern "C" fn(*mut RawArchive, *const c_char) -> c_int;
pub(super) type ArchiveReadOpenFilenameW =
    unsafe extern "C" fn(*mut RawArchive, *const u16, usize) -> c_int;
pub(super) type ArchiveReadNextHeader =
    unsafe extern "C" fn(*mut RawArchive, *mut *mut RawEntry) -> c_int;
pub(super) type ArchiveReadData =
    unsafe extern "C" fn(*mut RawArchive, *mut c_void, usize) -> isize;
pub(super) type ArchiveReadClose = unsafe extern "C" fn(*mut RawArchive) -> c_int;
pub(super) type ArchiveReadFree = unsafe extern "C" fn(*mut RawArchive) -> c_int;
pub(super) type ArchiveReadHasEncryptedEntries = unsafe extern "C" fn(*mut RawArchive) -> c_int;

pub(super) type ArchiveErrorString = unsafe extern "C" fn(*mut RawArchive) -> *const c_char;
pub(super) type ArchiveErrno = unsafe extern "C" fn(*mut RawArchive) -> c_int;
pub(super) type ArchiveFormatName = unsafe extern "C" fn(*mut RawArchive) -> *const c_char;
pub(super) type ArchiveFilterName = unsafe extern "C" fn(*mut RawArchive, c_int) -> *const c_char;
pub(super) type ArchiveFilterBytes = unsafe extern "C" fn(*mut RawArchive, c_int) -> i64;

pub(super) type ArchiveEntryPathnameW = unsafe extern "C" fn(*mut RawEntry) -> *const u16;
pub(super) type ArchiveEntryPathnameUtf8 = unsafe extern "C" fn(*mut RawEntry) -> *const c_char;
pub(super) type ArchiveEntryPathname = unsafe extern "C" fn(*mut RawEntry) -> *const c_char;
pub(super) type ArchiveEntrySize = unsafe extern "C" fn(*mut RawEntry) -> i64;
pub(super) type ArchiveEntryFlag = unsafe extern "C" fn(*mut RawEntry) -> c_int;
pub(super) type ArchiveEntryFiletype = unsafe extern "C" fn(*mut RawEntry) -> u16;
pub(super) type ArchiveEntryMtime = unsafe extern "C" fn(*mut RawEntry) -> i64;

pub(super) type ArchiveWriteNew = unsafe extern "C" fn() -> *mut RawArchive;
pub(super) type ArchiveWriteUnary = unsafe extern "C" fn(*mut RawArchive) -> c_int;
pub(super) type ArchiveWriteOpenFilenameW =
    unsafe extern "C" fn(*mut RawArchive, *const u16) -> c_int;
pub(super) type ArchiveWriteHeader = unsafe extern "C" fn(*mut RawArchive, *mut RawEntry) -> c_int;
pub(super) type ArchiveWriteData =
    unsafe extern "C" fn(*mut RawArchive, *const c_void, usize) -> isize;
pub(super) type ArchiveWriteOption =
    unsafe extern "C" fn(*mut RawArchive, *const c_char, *const c_char, *const c_char) -> c_int;
pub(super) type ArchiveWriteSetPassphrase =
    unsafe extern "C" fn(*mut RawArchive, *const c_char) -> c_int;

pub(super) type ArchiveEntryNew = unsafe extern "C" fn() -> *mut RawEntry;
pub(super) type ArchiveEntryFree = unsafe extern "C" fn(*mut RawEntry);
pub(super) type ArchiveEntryCopyPathnameW = unsafe extern "C" fn(*mut RawEntry, *const u16);
pub(super) type ArchiveEntrySetFiletype = unsafe extern "C" fn(*mut RawEntry, u32);
pub(super) type ArchiveEntrySetMode = unsafe extern "C" fn(*mut RawEntry, u16);
pub(super) type ArchiveEntrySetSize = unsafe extern "C" fn(*mut RawEntry, i64);
pub(super) type ArchiveEntrySetMtime = unsafe extern "C" fn(*mut RawEntry, i64, c_long);

pub(super) struct DynamicLibrary {
    pub(super) handle: HMODULE,
}

// A loaded module can be queried and freed from any thread.  The handle is
// kept alive by Arc<Api> for at least as long as every copied function ptr.
unsafe impl Send for DynamicLibrary {}
unsafe impl Sync for DynamicLibrary {}

impl DynamicLibrary {
    pub(super) fn load(path: &Path, system_only: bool) -> Result<Self, String> {
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
        unsafe { GetProcAddress(self.handle, PCSTR(name.as_ptr())) }.map(|symbol| symbol as usize)
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

pub(super) struct Api {
    pub(super) _library: DynamicLibrary,
    pub(super) version_string: ArchiveVersionString,
    pub(super) version_number: c_int,
    pub(super) version_note: Option<String>,
    pub(super) read_new: ArchiveReadNew,
    pub(super) read_filters: Vec<ArchiveReadSupport>,
    pub(super) read_formats: Vec<ArchiveReadSupport>,
    pub(super) read_support_format_iso9660: ArchiveReadSupport,
    pub(super) read_support_format_lha: ArchiveReadSupport,
    pub(super) read_support_format_raw: Option<ArchiveReadSupport>,
    pub(super) read_add_passphrase: Option<ArchiveReadAddPassphrase>,
    pub(super) read_open_filename_w: ArchiveReadOpenFilenameW,
    pub(super) read_next_header: ArchiveReadNextHeader,
    pub(super) read_data: ArchiveReadData,
    pub(super) read_close: ArchiveReadClose,
    pub(super) read_free: ArchiveReadFree,
    pub(super) read_has_encrypted_entries: Option<ArchiveReadHasEncryptedEntries>,
    pub(super) error_string: ArchiveErrorString,
    pub(super) archive_errno: ArchiveErrno,
    pub(super) format_name: ArchiveFormatName,
    pub(super) filter_name: ArchiveFilterName,
    pub(super) filter_bytes: ArchiveFilterBytes,
    pub(super) entry_pathname_w: ArchiveEntryPathnameW,
    pub(super) entry_pathname_utf8: Option<ArchiveEntryPathnameUtf8>,
    pub(super) entry_pathname: ArchiveEntryPathname,
    pub(super) entry_size: ArchiveEntrySize,
    pub(super) entry_size_is_set: ArchiveEntryFlag,
    pub(super) entry_filetype: ArchiveEntryFiletype,
    pub(super) entry_mtime: ArchiveEntryMtime,
    pub(super) entry_mtime_is_set: ArchiveEntryFlag,
    pub(super) entry_hardlink_is_set: ArchiveEntryFlag,
    pub(super) entry_is_encrypted: Option<ArchiveEntryFlag>,
    pub(super) write: Option<WriteApi>,
}

pub(super) struct WriteApi {
    pub(super) write_new: ArchiveWriteNew,
    pub(super) set_format_zip: ArchiveWriteUnary,
    pub(super) set_format_pax_restricted: ArchiveWriteUnary,
    pub(super) set_format_7zip: Option<ArchiveWriteUnary>,
    pub(super) add_filter_gzip: Option<ArchiveWriteUnary>,
    pub(super) add_filter_xz: Option<ArchiveWriteUnary>,
    pub(super) add_filter_zstd: Option<ArchiveWriteUnary>,
    pub(super) set_format_option: ArchiveWriteOption,
    pub(super) set_filter_option: ArchiveWriteOption,
    pub(super) set_passphrase: Option<ArchiveWriteSetPassphrase>,
    pub(super) open_filename_w: ArchiveWriteOpenFilenameW,
    pub(super) write_header: ArchiveWriteHeader,
    pub(super) write_data: ArchiveWriteData,
    pub(super) finish_entry: ArchiveWriteUnary,
    pub(super) close: ArchiveWriteUnary,
    pub(super) free: ArchiveWriteUnary,
    pub(super) entry_new: ArchiveEntryNew,
    pub(super) entry_free: ArchiveEntryFree,
    pub(super) entry_copy_pathname_w: ArchiveEntryCopyPathnameW,
    pub(super) entry_set_filetype: ArchiveEntrySetFiletype,
    pub(super) entry_set_perm: ArchiveEntrySetMode,
    pub(super) entry_set_size: ArchiveEntrySetSize,
    pub(super) entry_set_mtime: ArchiveEntrySetMtime,
}

pub(super) fn ensure_supported_libarchive_abi(version_number: c_int) -> ArchiveResult<()> {
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
    pub(super) fn from_library(
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
        let read_support_format_iso9660 = required_symbol!(
            library,
            "archive_read_support_format_iso9660",
            ArchiveReadSupport
        );
        let read_support_format_lha = required_symbol!(
            library,
            "archive_read_support_format_lha",
            ArchiveReadSupport
        );
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
            ("iso9660", read_support_format_iso9660),
            ("lha", read_support_format_lha),
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
            read_support_format_iso9660,
            read_support_format_lha,
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

    pub(super) fn version(&self) -> String {
        // SAFETY: this function returns a process-lifetime static C string.
        let pointer = unsafe { (self.version_string)() };
        let base = copy_c_string(pointer)
            .unwrap_or_else(|| format!("libarchive ({})", self.version_number));
        match &self.version_note {
            Some(note) => format!("{base} [{note}]"),
            None => base,
        }
    }

    pub(super) fn last_error(
        &self,
        archive: *mut RawArchive,
        operation: &'static str,
    ) -> ArchiveError {
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
            write_header: optional_symbol!(library, "archive_write_header", ArchiveWriteHeader)?,
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

pub(super) fn canonical_library_file(path: &Path) -> ArchiveResult<PathBuf> {
    if !path.is_absolute() {
        return Err(ArchiveError::InvalidInput(
            "libarchive DLL path must be absolute".to_owned(),
        ));
    }
    let canonical = fs::canonicalize(path).map_err(|error| ArchiveError::io(path, error))?;
    let metadata = fs::metadata(&canonical).map_err(|error| ArchiveError::io(&canonical, error))?;
    if !metadata.is_file() {
        return Err(ArchiveError::InvalidInput(format!(
            "libarchive DLL path is not a file: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

pub(super) fn load_api() -> ArchiveResult<Api> {
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
        failures.push("archiveint.dll skipped (development-only override is disabled)".to_owned());
    }

    Err(ArchiveError::LibraryUnavailable(failures.join("; ")))
}

pub(super) fn probe_internal_filters(
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

pub(super) fn probe_supported_formats(
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
