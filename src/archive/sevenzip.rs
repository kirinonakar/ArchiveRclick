//! 7z.dll backend for the 7z and ZIP formats.
//!
//! 7z and ZIP archives are listed, extracted, tested, and created with the
//! bundled 7-Zip DLL (`runtime/x64/7z.dll`) so the format handlers can use
//! their native multithreaded paths. 7z compression uses LZMA2 and ZIP uses
//! Deflate; other formats continue to use the libarchive backend.
//!
//! The DLL is used through the classic 7-Zip COM-style interface: the
//! exported `CreateObject` function instantiates the 7z format handler, and
//! the handler is driven through the `IInArchive` / `IOutArchive` /
//! `ISetProperties` vtables. Callbacks implemented by this module provide
//! file data, receive extracted data, and resolve passwords.

#[cfg(windows)]
mod platform_impl {
    // FFI vtable entry points are inherently unsafe; every body carries SAFETY
    // comments, so the edition-2024 unsafe-op lint is relaxed in this module.
    #![allow(unsafe_op_in_unsafe_fn)]

    use std::{
        cell::Cell,
        collections::{BTreeMap, HashSet, VecDeque},
        env,
        ffi::c_void,
        fs::{self, File, OpenOptions},
        io::{BufReader, Read, Seek, SeekFrom, Write},
        mem,
        os::windows::{ffi::OsStrExt, fs::OpenOptionsExt},
        path::{Path, PathBuf},
        ptr,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU32, Ordering},
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

    // ------------------------------------------------------------------
    // HRESULT values
    // ------------------------------------------------------------------
    const S_OK: i32 = 0;
    const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
    const E_ABORT: i32 = 0x8000_4004u32 as i32;
    const E_FAIL: i32 = 0x8000_4005u32 as i32;
    const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;

    // 7-Zip PROPIDs (PropID.h).
    const KPID_PATH: u32 = 3;
    const KPID_IS_DIR: u32 = 6;
    const KPID_SIZE: u32 = 7;
    const KPID_MTIME: u32 = 12;
    const KPID_ENCRYPTED: u32 = 15;

    // PROPVARIANT types.
    const VT_EMPTY: u16 = 0;
    const VT_BSTR: u16 = 8;
    const VT_BOOL: u16 = 11;
    const VT_UI4: u16 = 19;
    const VT_UI8: u16 = 21;
    const VT_FILETIME: u16 = 64;

    // Stream seek origins.
    const SEEK_SET: u32 = 0;
    const SEEK_CUR: u32 = 1;
    const SEEK_END: u32 = 2;

    // IArchiveExtractCallback askExtractMode values (AskMode.h).
    const EXTRACT_MODE_EXTRACT: i32 = 0;
    const EXTRACT_MODE_TEST: i32 = 1;
    const EXTRACT_MODE_SKIP: i32 = 2;

    // IArchiveExtractCallback / IArchiveUpdateCallback operation results.
    const OPERATION_RESULT_OK: i32 = 0;
    const OPERATION_RESULT_UNSUPPORTED_METHOD: i32 = 1;
    const OPERATION_RESULT_DATA_ERROR: i32 = 2;
    const OPERATION_RESULT_CRC_ERROR: i32 = 3;

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

    // FILETIME epoch (1601-01-01) relative to the Unix epoch, in 100ns units
    // and in seconds.
    const FILETIME_EPOCH_SECONDS: i64 = 11_644_473_600;

    // ------------------------------------------------------------------
    // GUIDs (7-Zip SDK: IArchive.h, ICoder.h, IPassword.h, ArchiveExports.cpp)
    // ------------------------------------------------------------------
    #[repr(C)]
    #[derive(Clone, Copy, PartialEq, Eq)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    const fn guid(
        data1: u32,
        data2: u16,
        data3: u16,
        b1: u8,
        b2: u8,
        b3: u8,
        b4: u8,
        b5: u8,
        b6: u8,
        b7: u8,
        b8: u8,
    ) -> Guid {
        Guid {
            data1,
            data2,
            data3,
            data4: [b1, b2, b3, b4, b5, b6, b7, b8],
        }
    }

    // 7-Zip 24.x+ encodes its interface GUIDs as
    //   {23170F69-40C1-278A-0000-00GG00SS0000}
    // where Data4 = { 0, 0, 0, groupId, 0, subId, 0, 0 } (IDecl.h:
    // Z7_DECL_IFACE_7ZIP_SUB).  The archive group is 6, the stream group is
    // 3, and the password group is 5.  (Versions before 24.0 used the
    // 23170F69-40C1-278A-1000-000110XX0000 layout instead.)
    const IID_IUNKNOWN: Guid = guid(0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46);
    const IID_ISEQUENTIAL_IN_STREAM: Guid =
        guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 3, 0, 0x01, 0, 0);
    const IID_IIN_STREAM: Guid = guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 3, 0, 0x03, 0, 0);
    const IID_ISEQUENTIAL_OUT_STREAM: Guid =
        guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 3, 0, 0x02, 0, 0);
    const IID_IOUT_STREAM: Guid = guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 3, 0, 0x04, 0, 0);
    const IID_IPROGRESS: Guid = guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 0, 0, 0x05, 0, 0);
    const IID_IARCHIVE_EXTRACT_CALLBACK: Guid =
        guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 6, 0, 0x20, 0, 0);
    const IID_IIN_ARCHIVE: Guid = guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 6, 0, 0x60, 0, 0);
    const IID_IARCHIVE_OPEN_CALLBACK: Guid =
        guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 6, 0, 0x10, 0, 0);
    const IID_ISET_PROPERTIES: Guid = guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 6, 0, 0x03, 0, 0);
    const IID_IOUT_ARCHIVE: Guid = guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 6, 0, 0xA0, 0, 0);
    const IID_IARCHIVE_UPDATE_CALLBACK: Guid =
        guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 6, 0, 0x80, 0, 0);
    const IID_ICRYPTO_GET_TEXT_PASSWORD: Guid =
        guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 5, 0, 0x10, 0, 0);
    const IID_ICRYPTO_GET_TEXT_PASSWORD2: Guid =
        guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 5, 0, 0x11, 0, 0);

    // CLSID_CFormat7z keeps the classic layout: {23170F69-40C1-278A-1000-
    // 000110070000} with the format id (7) in Data4[5]; CreateArchiver zeroes
    // Data4[5] and compares the rest against CLSID_CArchiveHandler.
    const CLSID_C_FORMAT_7Z: Guid = guid(
        0x2317_0F69,
        0x40C1,
        0x278A,
        0x10,
        0,
        0,
        0x01,
        0x10,
        0x07,
        0,
        0,
    );

    // The full 7z.dll also exposes the ZIP handler.  Using it for ZIP keeps
    // the application on the same multithreaded codec path as 7z.exe instead
    // of falling back to libarchive's single-threaded deflate writer.
    const CLSID_C_FORMAT_ZIP: Guid = guid(
        0x2317_0F69,
        0x40C1,
        0x278A,
        0x10,
        0,
        0,
        0x01,
        0x10,
        0x01,
        0,
        0,
    );

    // 7z file signature: "7z\xBC\xAF\x27\x1C".
    const SEVENZIP_SIGNATURE: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];

    // ------------------------------------------------------------------
    // Dynamic library loading
    // ------------------------------------------------------------------
    struct DynamicLibrary {
        handle: HMODULE,
    }

    // A loaded module can be queried and freed from any thread. The handle is
    // kept alive by Arc<Api> for at least as long as every copied function ptr.
    unsafe impl Send for DynamicLibrary {}
    unsafe impl Sync for DynamicLibrary {}

    impl DynamicLibrary {
        fn load(path: &Path) -> Result<Self, String> {
            let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
            if wide.contains(&0) {
                return Err("DLL path contains NUL".to_owned());
            }
            wide.push(0);
            // SAFETY: `wide` is NUL-terminated and remains alive for the call.
            let handle = unsafe {
                LoadLibraryExW(
                    PCWSTR(wide.as_ptr()),
                    None,
                    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
                )
            }
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
            // SAFETY: the symbol name and signature come from the 7-Zip SDK.
            unsafe { mem::transmute::<usize, $ty>(address) }
        }};
    }

    // ------------------------------------------------------------------
    // PROPVARIANT and BSTR helpers
    // ------------------------------------------------------------------
    #[link(name = "OleAut32")]
    unsafe extern "system" {
        fn SysAllocString(value: *const u16) -> *mut u16;
        fn SysFreeString(value: *mut u16);
        fn SysStringByteLen(value: *const u16) -> u32;
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct PropVariant {
        vt: u16,
        w_reserved1: u16,
        w_reserved2: u16,
        w_reserved3: u16,
        payload: u64,
        // PROPVARIANT's union is as large as DECIMAL (16 bytes) even when the
        // active value is a scalar. Keeping the full union width is essential
        // when 7-Zip walks an array containing more than one property.
        payload2: u64,
    }

    impl PropVariant {
        fn empty() -> Self {
            Self {
                vt: VT_EMPTY,
                w_reserved1: 0,
                w_reserved2: 0,
                w_reserved3: 0,
                payload: 0,
                payload2: 0,
            }
        }

        fn u32_value(value: u32) -> Self {
            Self {
                vt: VT_UI4,
                payload: u64::from(value),
                ..Self::empty()
            }
        }

        fn u64_value(value: u64) -> Self {
            Self {
                vt: VT_UI8,
                payload: value,
                ..Self::empty()
            }
        }

        fn bool_value(value: bool) -> Self {
            Self {
                vt: VT_BOOL,
                // VARIANT_TRUE is -1.
                payload: if value { 0xFFFF } else { 0 },
                ..Self::empty()
            }
        }

        fn bstr(value: &str) -> Self {
            Self {
                vt: VT_BSTR,
                payload: sys_alloc_string(value) as u64,
                ..Self::empty()
            }
        }

        fn filetime(seconds: i64) -> Self {
            Self {
                vt: VT_FILETIME,
                payload: unix_seconds_to_filetime(seconds),
                ..Self::empty()
            }
        }

        fn as_u64(&self) -> Option<u64> {
            match self.vt {
                VT_UI8 => Some(self.payload),
                VT_UI4 => Some(u64::from(self.payload as u32)),
                _ => None,
            }
        }

        fn as_bool(&self) -> Option<bool> {
            (self.vt == VT_BOOL).then(|| self.payload != 0)
        }

        fn as_bstr(&self) -> Option<String> {
            if self.vt != VT_BSTR || self.payload == 0 {
                return None;
            }
            // BSTRs are NUL-terminated; bound the scan like the libarchive
            // pathname reader does.
            let pointer = self.payload as *const u16;
            let mut length = 0usize;
            // SAFETY: 7-Zip allocated this BSTR with SysAllocString, so it is
            // NUL-terminated and readable for at least `length + 1` units.
            while length < MAX_ARCHIVE_PATH_UNITS && unsafe { *pointer.add(length) } != 0 {
                length += 1;
            }
            if length >= MAX_ARCHIVE_PATH_UNITS {
                return None;
            }
            // SAFETY: the scan above established `length` initialized units.
            let units = unsafe { std::slice::from_raw_parts(pointer, length) };
            Some(String::from_utf16_lossy(units))
        }

        fn as_guid(&self) -> Option<Guid> {
            if self.vt != VT_BSTR || self.payload == 0 {
                return None;
            }
            // GetHandlerProperty2 returns the class ID as a binary BSTR, so
            // it may contain embedded NULs and cannot use `as_bstr`.
            let length = unsafe { SysStringByteLen(self.payload as *const u16) } as usize;
            (length >= mem::size_of::<Guid>())
                .then(|| unsafe { ptr::read_unaligned(self.payload as *const Guid) })
        }

        fn as_filetime_seconds(&self) -> Option<i64> {
            (self.vt == VT_FILETIME).then(|| filetime_to_unix_seconds(self.payload))
        }

        /// Releases strings owned by this variant. Must be called for every
        /// variant received from 7-Zip (GetProperty) and for every variant we
        /// handed to 7-Zip after SetProperties returned.
        fn clear(&mut self) {
            if self.vt == VT_BSTR && self.payload != 0 {
                // SAFETY: the variant owns a SysAllocString allocation.
                unsafe { SysFreeString(self.payload as *mut u16) };
            }
            *self = Self::empty();
        }
    }

    fn sys_alloc_string(value: &str) -> *mut u16 {
        let mut wide: Vec<u16> = value.encode_utf16().collect();
        wide.push(0);
        // SAFETY: `wide` is NUL-terminated and stays alive for the call.
        unsafe { SysAllocString(wide.as_ptr()) }
    }

    /// Supplies a password to 7-Zip's `ICryptoGetTextPassword` callback.
    ///
    /// A missing password must be reported as a failed callback, not as a
    /// successful callback containing an empty BSTR.  Returning `S_OK` with
    /// an empty string makes 7-Zip continue as if an empty password had been
    /// supplied; encrypted archives can then re-enter the callback/error
    /// path in native code and destabilize the host process.  `E_ABORT` is the
    /// HRESULT used by the 7-Zip SDK examples when no password is available.
    fn write_password_bstr(password: *mut *mut u16, value: Option<&str>) -> i32 {
        if password.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: the caller supplied a valid out-parameter.
        unsafe { *password = ptr::null_mut() };

        let Some(value) = value.filter(|value| !value.is_empty()) else {
            return E_ABORT;
        };
        let allocated = sys_alloc_string(value);
        if allocated.is_null() {
            return E_FAIL;
        }
        // SAFETY: `allocated` is a valid BSTR returned by SysAllocString.
        unsafe { *password = allocated };
        S_OK
    }

    fn unix_seconds_to_filetime(seconds: i64) -> u64 {
        seconds
            .saturating_add(FILETIME_EPOCH_SECONDS)
            .saturating_mul(10_000_000) as u64
    }

    fn filetime_to_unix_seconds(value: u64) -> i64 {
        (value / 10_000_000)
            .try_into()
            .unwrap_or(i64::MAX)
            .saturating_sub(FILETIME_EPOCH_SECONDS)
    }

    // ------------------------------------------------------------------
    // Raw vtables of the 7-Zip handler objects (their vtables live in 7z.dll)
    // ------------------------------------------------------------------
    type QueryInterfaceFn =
        unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32;
    type AddRefFn = unsafe extern "system" fn(*mut c_void) -> u32;
    type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;

    #[repr(C)]
    struct InArchiveVtbl {
        query_interface: QueryInterfaceFn,
        add_ref: AddRefFn,
        release: ReleaseFn,
        open: unsafe extern "system" fn(*mut c_void, *mut c_void, *const u64, *mut c_void) -> i32,
        close: unsafe extern "system" fn(*mut c_void) -> i32,
        get_number_of_items: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
        get_property: unsafe extern "system" fn(*mut c_void, u32, u32, *mut PropVariant) -> i32,
        extract: unsafe extern "system" fn(*mut c_void, *const u32, u32, i32, *mut c_void) -> i32,
        get_archive_property: unsafe extern "system" fn(*mut c_void, u32, *mut PropVariant) -> i32,
        get_number_of_properties: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
        get_property_info:
            unsafe extern "system" fn(*mut c_void, u32, *mut *mut u16, *mut u32, *mut u16) -> i32,
        get_number_of_archive_properties: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
        get_archive_property_info:
            unsafe extern "system" fn(*mut c_void, u32, *mut *mut u16, *mut u32, *mut u16) -> i32,
    }

    #[repr(C)]
    struct RawInArchive {
        /// The interface pointer returned by 7-Zip. Calls must use this
        /// address, not the Rust ownership wrapper below it: the handler has
        /// private state immediately after its vtable.
        object: *mut c_void,
        vtbl: *const InArchiveVtbl,
        /// True once `Close` has been issued; the destructor then only
        /// releases the interface reference.
        closed: Cell<bool>,
    }

    impl RawInArchive {
        /// # Safety
        /// `pointer` must come from `CreateObject(CLSID_CFormat7z or
        /// CLSID_CFormatZip, IID_IInArchive)`.
        unsafe fn from_raw(pointer: *mut c_void) -> Self {
            Self {
                object: pointer,
                vtbl: *pointer.cast::<*const InArchiveVtbl>(),
                closed: Cell::new(false),
            }
        }

        fn ptr(&self) -> *mut c_void {
            self.object
        }

        fn get_number_of_items(&self, count: *mut u32) -> i32 {
            // SAFETY: the object is live; `count` is a valid out-parameter.
            unsafe { ((*self.vtbl).get_number_of_items)(self.ptr(), count) }
        }

        fn get_property(&self, index: u32, prop_id: u32, value: *mut PropVariant) -> i32 {
            // SAFETY: the object is live; `value` is a valid out-parameter.
            unsafe { ((*self.vtbl).get_property)(self.ptr(), index, prop_id, value) }
        }

        fn get_archive_property(&self, prop_id: u32, value: *mut PropVariant) -> i32 {
            // SAFETY: the object is live; `value` is a valid out-parameter.
            unsafe { ((*self.vtbl).get_archive_property)(self.ptr(), prop_id, value) }
        }

        fn extract(
            &self,
            indices: *const u32,
            num_items: u32,
            test_mode: i32,
            callback: *mut c_void,
        ) -> i32 {
            // SAFETY: all pointers stay live for the call.
            unsafe { ((*self.vtbl).extract)(self.ptr(), indices, num_items, test_mode, callback) }
        }

        fn open(&self, stream: *mut c_void, open_callback: *mut c_void) -> i32 {
            // SAFETY: the stream and callback stay live for the call.
            unsafe { ((*self.vtbl).open)(self.ptr(), stream, ptr::null(), open_callback) }
        }

        /// Closes the archive explicitly so the handler releases its input
        /// stream while the stream wrapper is still alive. Idempotent; the
        /// destructor still releases the interface reference.
        fn close_now(&self) {
            if !self.closed.get() {
                self.closed.set(true);
                // SAFETY: the object is live; Close releases archive data.
                unsafe { ((*self.vtbl).close)(self.ptr()) };
            }
        }
    }

    impl Drop for RawInArchive {
        fn drop(&mut self) {
            if !self.closed.get() {
                // SAFETY: the object is live; Close releases archive data.
                unsafe { ((*self.vtbl).close)(self.ptr()) };
            }
            // SAFETY: this object uniquely owns the handler reference.
            unsafe { ((*self.vtbl).release)(self.ptr()) };
        }
    }

    #[repr(C)]
    struct OutArchiveVtbl {
        query_interface: QueryInterfaceFn,
        add_ref: AddRefFn,
        release: ReleaseFn,
        update_items: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, *mut c_void) -> i32,
        get_file_time_type: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
    }

    #[repr(C)]
    struct RawOutArchive {
        object: *mut c_void,
        vtbl: *const OutArchiveVtbl,
    }

    impl RawOutArchive {
        /// # Safety
        /// `pointer` must come from `CreateObject(CLSID_CFormat7z or
        /// CLSID_CFormatZip, IID_IOutArchive)`.
        unsafe fn from_raw(pointer: *mut c_void) -> Self {
            Self {
                object: pointer,
                vtbl: *pointer.cast::<*const OutArchiveVtbl>(),
            }
        }

        fn ptr(&self) -> *mut c_void {
            self.object
        }

        fn query_interface(&self, iid: &Guid) -> Option<*mut c_void> {
            let mut out: *mut c_void = ptr::null_mut();
            // SAFETY: the object and iid are live for the call.
            let hr = unsafe { ((*self.vtbl).query_interface)(self.ptr(), iid, &mut out) };
            (hr == S_OK).then_some(out)
        }

        fn update_items(
            &self,
            out_stream: *mut c_void,
            num_items: u32,
            callback: *mut c_void,
        ) -> i32 {
            // SAFETY: the stream and callback stay live for the call.
            unsafe { ((*self.vtbl).update_items)(self.ptr(), out_stream, num_items, callback) }
        }
    }

    impl Drop for RawOutArchive {
        fn drop(&mut self) {
            // SAFETY: this object uniquely owns the handler reference.
            unsafe { ((*self.vtbl).release)(self.ptr()) };
        }
    }

    #[repr(C)]
    struct SetPropertiesVtbl {
        query_interface: QueryInterfaceFn,
        add_ref: AddRefFn,
        release: ReleaseFn,
        set_properties: unsafe extern "system" fn(
            *mut c_void,
            *const *const u16,
            *const PropVariant,
            u32,
        ) -> i32,
    }

    #[repr(C)]
    struct RawSetProperties {
        object: *mut c_void,
        vtbl: *const SetPropertiesVtbl,
    }

    impl RawSetProperties {
        /// # Safety
        /// `pointer` must come from a successful QueryInterface for
        /// `IID_ISetProperties` on a 7z handler object.
        unsafe fn from_raw(pointer: *mut c_void) -> Self {
            Self {
                object: pointer,
                vtbl: *pointer.cast::<*const SetPropertiesVtbl>(),
            }
        }

        fn ptr(&self) -> *mut c_void {
            self.object
        }

        fn set_properties(
            &self,
            names: *const *const u16,
            values: *const PropVariant,
            count: u32,
        ) -> i32 {
            // SAFETY: the arrays stay live for the call.
            unsafe { ((*self.vtbl).set_properties)(self.ptr(), names, values, count) }
        }
    }

    impl Drop for RawSetProperties {
        fn drop(&mut self) {
            // SAFETY: this object uniquely owns the interface reference.
            unsafe { ((*self.vtbl).release)(self.ptr()) };
        }
    }

    // ------------------------------------------------------------------
    // File streams handed to 7-Zip (IInStream / IOutStream)
    // ------------------------------------------------------------------
    #[repr(C)]
    struct InStreamVtbl {
        query_interface: QueryInterfaceFn,
        add_ref: AddRefFn,
        release: ReleaseFn,
        read: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, *mut u32) -> i32,
        seek: unsafe extern "system" fn(*mut c_void, i64, u32, *mut u64) -> i32,
    }

    static IN_STREAM_VTBL: InStreamVtbl = InStreamVtbl {
        query_interface: in_stream_query_interface,
        add_ref: stream_add_ref,
        release: in_stream_release,
        read: in_stream_read,
        seek: in_stream_seek,
    };

    #[repr(C)]
    struct InStream {
        vtbl: &'static InStreamVtbl,
        refs: AtomicU32,
        file: Mutex<BufReader<File>>,
    }

    unsafe extern "system" fn in_stream_query_interface(
        this: *mut c_void,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> i32 {
        stream_query_interface(
            this,
            iid,
            out,
            &[IID_ISEQUENTIAL_IN_STREAM, IID_IIN_STREAM],
            stream_add_ref,
        )
    }

    unsafe extern "system" fn in_stream_release(this: *mut c_void) -> u32 {
        let stream = unsafe { &*(this as *const InStream) };
        let remaining = stream.refs.fetch_sub(1, Ordering::AcqRel).saturating_sub(1);
        if remaining == 0 {
            // 7-Zip released the last reference; the Box we created in
            // `Box::into_raw` is now exclusively ours again.
            unsafe { drop(Box::from_raw(this as *mut InStream)) };
        }
        remaining
    }

    unsafe extern "system" fn in_stream_read(
        this: *mut c_void,
        data: *mut c_void,
        size: u32,
        processed: *mut u32,
    ) -> i32 {
        if !processed.is_null() {
            unsafe { *processed = 0 };
        }
        if this.is_null() || (data.is_null() && size != 0) {
            return E_INVALIDARG;
        }
        if size == 0 {
            return S_OK;
        }
        let stream = unsafe { &*(this as *const InStream) };
        let buffer = unsafe { std::slice::from_raw_parts_mut(data.cast::<u8>(), size as usize) };
        let mut file = stream
            .file
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match file.read(buffer) {
            Ok(amount) => {
                if !processed.is_null() {
                    unsafe { *processed = amount as u32 };
                }
                S_OK
            }
            Err(_) => E_FAIL,
        }
    }

    unsafe extern "system" fn in_stream_seek(
        this: *mut c_void,
        offset: i64,
        origin: u32,
        new_position: *mut u64,
    ) -> i32 {
        let stream = unsafe { &*(this as *const InStream) };
        let from = match origin {
            SEEK_SET => SeekFrom::Start(offset.max(0) as u64),
            SEEK_CUR => SeekFrom::Current(offset),
            SEEK_END => SeekFrom::End(offset),
            _ => return E_INVALIDARG,
        };
        let position = {
            let mut file = stream
                .file
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            match file.seek(from) {
                Ok(position) => position,
                Err(_) => return E_FAIL,
            }
        };
        if !new_position.is_null() {
            unsafe { *new_position = position };
        }
        S_OK
    }

    #[repr(C)]
    struct OutStreamVtbl {
        query_interface: QueryInterfaceFn,
        add_ref: AddRefFn,
        release: ReleaseFn,
        write: unsafe extern "system" fn(*mut c_void, *const c_void, u32, *mut u32) -> i32,
        seek: unsafe extern "system" fn(*mut c_void, i64, u32, *mut u64) -> i32,
        set_size: unsafe extern "system" fn(*mut c_void, u64) -> i32,
    }

    static OUT_STREAM_VTBL: OutStreamVtbl = OutStreamVtbl {
        query_interface: out_stream_query_interface,
        add_ref: stream_add_ref,
        release: out_stream_release,
        write: out_stream_write,
        seek: out_stream_seek,
        set_size: out_stream_set_size,
    };

    #[repr(C)]
    struct OutStream {
        vtbl: &'static OutStreamVtbl,
        refs: AtomicU32,
        file: Arc<Mutex<Option<File>>>,
    }

    unsafe extern "system" fn out_stream_query_interface(
        this: *mut c_void,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> i32 {
        stream_query_interface(
            this,
            iid,
            out,
            &[IID_ISEQUENTIAL_OUT_STREAM, IID_IOUT_STREAM],
            stream_add_ref,
        )
    }

    unsafe extern "system" fn out_stream_release(this: *mut c_void) -> u32 {
        let stream = unsafe { &*(this as *const OutStream) };
        let remaining = stream.refs.fetch_sub(1, Ordering::AcqRel).saturating_sub(1);
        if remaining == 0 {
            // The Box we created in `Box::into_raw` is now exclusively ours.
            unsafe { drop(Box::from_raw(this as *mut OutStream)) };
        }
        remaining
    }

    unsafe extern "system" fn out_stream_write(
        this: *mut c_void,
        data: *const c_void,
        size: u32,
        processed: *mut u32,
    ) -> i32 {
        if !processed.is_null() {
            unsafe { *processed = 0 };
        }
        if this.is_null() || (data.is_null() && size != 0) {
            return E_INVALIDARG;
        }
        if size == 0 {
            return S_OK;
        }
        let stream = unsafe { &*(this as *const OutStream) };
        let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size as usize) };
        let mut guard = stream
            .file
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some(file) = guard.as_mut() else {
            return E_FAIL;
        };
        match file.write_all(bytes) {
            Ok(()) => {
                if !processed.is_null() {
                    unsafe { *processed = size };
                }
                S_OK
            }
            Err(_) => E_FAIL,
        }
    }

    unsafe extern "system" fn out_stream_seek(
        this: *mut c_void,
        offset: i64,
        origin: u32,
        new_position: *mut u64,
    ) -> i32 {
        let stream = unsafe { &*(this as *const OutStream) };
        let from = match origin {
            SEEK_SET => SeekFrom::Start(offset.max(0) as u64),
            SEEK_CUR => SeekFrom::Current(offset),
            SEEK_END => SeekFrom::End(offset),
            _ => return E_INVALIDARG,
        };
        let position = {
            let mut guard = stream
                .file
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let Some(file) = guard.as_mut() else {
                return E_FAIL;
            };
            match file.seek(from) {
                Ok(position) => position,
                Err(_) => return E_FAIL,
            }
        };
        if !new_position.is_null() {
            unsafe { *new_position = position };
        }
        S_OK
    }

    unsafe extern "system" fn out_stream_set_size(this: *mut c_void, size: u64) -> i32 {
        let stream = unsafe { &*(this as *const OutStream) };
        let mut guard = stream
            .file
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some(file) = guard.as_mut() else {
            return E_FAIL;
        };
        match file.set_len(size) {
            Ok(()) => S_OK,
            Err(_) => E_FAIL,
        }
    }

    unsafe extern "system" fn stream_add_ref(this: *mut c_void) -> u32 {
        let object = unsafe { &*this.cast::<InStream>() };
        object.refs.fetch_add(1, Ordering::AcqRel).saturating_add(1)
    }

    /// Shared QueryInterface for the stream objects. Both stream structs start
    /// with the same `vtbl`/`refs` layout, so the pointer cast is layout-safe.
    /// Internal helper only (never installed in a vtable), so it is a plain
    /// unsafe function rather than an extern entry point.
    unsafe fn stream_query_interface(
        this: *mut c_void,
        iid: *const Guid,
        out: *mut *mut c_void,
        supported: &[Guid],
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    ) -> i32 {
        if out.is_null() || iid.is_null() {
            return E_INVALIDARG;
        }
        unsafe { *out = ptr::null_mut() };
        let requested = unsafe { *iid };
        if requested == IID_IUNKNOWN || supported.contains(&requested) {
            unsafe { *out = this };
            // QueryInterface returns a new owned interface reference.  7-Zip
            // releases that reference independently from the original one.
            unsafe { add_ref(this) };
            S_OK
        } else {
            E_NOINTERFACE
        }
    }

    // ------------------------------------------------------------------
    // Callback objects implemented by this module
    // ------------------------------------------------------------------

    // --- IArchiveOpenCallback + ICryptoGetTextPassword -------------------
    #[repr(C)]
    struct OpenVtbl {
        query_interface: QueryInterfaceFn,
        add_ref: AddRefFn,
        release: ReleaseFn,
        set_total: unsafe extern "system" fn(*mut c_void, *const u64, *const u64) -> i32,
        set_completed: unsafe extern "system" fn(*mut c_void, *const u64, *const u64) -> i32,
        get_property: unsafe extern "system" fn(*mut c_void, u32, *mut PropVariant) -> i32,
        get_stream: unsafe extern "system" fn(*mut c_void, *const u16, *mut *mut c_void) -> i32,
        set_sub_archive_name: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
    }

    static OPEN_VTBL: OpenVtbl = OpenVtbl {
        query_interface: open_query_interface,
        add_ref: open_callback_add_ref,
        release: open_callback_release,
        set_total: open_set_total,
        set_completed: open_set_completed,
        get_property: open_get_property,
        get_stream: open_get_stream,
        set_sub_archive_name: open_set_sub_archive_name,
    };

    #[repr(C)]
    struct CryptoGetTextPasswordVtbl {
        query_interface: QueryInterfaceFn,
        add_ref: AddRefFn,
        release: ReleaseFn,
        crypto_get_text_password: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> i32,
    }

    static CRYPTO_GET_TEXT_PASSWORD_VTBL: CryptoGetTextPasswordVtbl = CryptoGetTextPasswordVtbl {
        query_interface: crypto_query_interface,
        add_ref: open_crypto_add_ref,
        release: open_crypto_release,
        crypto_get_text_password: crypto_get_text_password,
    };

    /// Crypto vtable for the extract callback: the shared open-callback
    /// implementation reads `OpenCallback.password`, whose offset overlaps
    /// `ExtractCallback.context`, so the extract callback needs its own entry
    /// point that reads the password from its own context.
    static EXTRACT_CRYPTO_VTBL: CryptoGetTextPasswordVtbl = CryptoGetTextPasswordVtbl {
        query_interface: crypto_query_interface,
        add_ref: extract_crypto_add_ref,
        release: extract_crypto_release,
        crypto_get_text_password: extract_crypto_get_text_password,
    };

    #[repr(C)]
    struct OpenCallback {
        vtbl: *const OpenVtbl,
        crypto_vtbl: *const CryptoGetTextPasswordVtbl,
        refs: AtomicU32,
        password: Mutex<Option<String>>,
        password_requested: AtomicBool,
        cancel: CancellationToken,
    }

    unsafe extern "system" fn open_query_interface(
        this: *mut c_void,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> i32 {
        if out.is_null() || iid.is_null() {
            return E_INVALIDARG;
        }
        unsafe { *out = ptr::null_mut() };
        let requested = unsafe { *iid };
        if requested == IID_IUNKNOWN
            || requested == IID_IARCHIVE_OPEN_CALLBACK
            || requested == IID_IPROGRESS
        {
            unsafe { *out = this };
            unsafe { open_callback_add_ref(this) };
            S_OK
        } else if requested == IID_ICRYPTO_GET_TEXT_PASSWORD {
            // The crypto interface is the second vtable pointer of the object.
            let crypto = unsafe {
                (this.cast::<u8>())
                    .add(mem::offset_of!(OpenCallback, crypto_vtbl))
                    .cast::<c_void>()
            };
            unsafe { *out = crypto };
            unsafe { open_crypto_add_ref(crypto) };
            S_OK
        } else {
            E_NOINTERFACE
        }
    }

    unsafe extern "system" fn open_set_total(
        _this: *mut c_void,
        _files: *const u64,
        _bytes: *const u64,
    ) -> i32 {
        S_OK
    }

    unsafe extern "system" fn open_set_completed(
        this: *mut c_void,
        _files: *const u64,
        _bytes: *const u64,
    ) -> i32 {
        let callback = unsafe { &*(this as *const OpenCallback) };
        if callback.cancel.is_cancelled() {
            E_ABORT
        } else {
            S_OK
        }
    }

    unsafe extern "system" fn open_get_property(
        _this: *mut c_void,
        _prop_id: u32,
        value: *mut PropVariant,
    ) -> i32 {
        if value.is_null() {
            return E_INVALIDARG;
        }
        unsafe { *value = PropVariant::empty() };
        S_OK
    }

    unsafe extern "system" fn open_get_stream(
        _this: *mut c_void,
        _name: *const u16,
        out: *mut *mut c_void,
    ) -> i32 {
        if out.is_null() {
            return E_INVALIDARG;
        }
        unsafe { *out = ptr::null_mut() };
        S_OK
    }

    unsafe extern "system" fn open_set_sub_archive_name(
        _this: *mut c_void,
        _name: *const u16,
    ) -> i32 {
        S_OK
    }

    unsafe extern "system" fn crypto_query_interface(
        this: *mut c_void,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> i32 {
        if out.is_null() || iid.is_null() {
            return E_INVALIDARG;
        }
        unsafe { *out = ptr::null_mut() };
        let requested = unsafe { *iid };
        if requested == IID_IUNKNOWN || requested == IID_ICRYPTO_GET_TEXT_PASSWORD {
            unsafe { *out = this };
            unsafe { open_crypto_add_ref(this) };
            S_OK
        } else {
            E_NOINTERFACE
        }
    }

    /// `this` points at the `crypto_vtbl` field of the owning callback.
    unsafe extern "system" fn crypto_get_text_password(
        this: *mut c_void,
        password: *mut *mut u16,
    ) -> i32 {
        let base = unsafe {
            this.cast::<u8>()
                .sub(mem::offset_of!(OpenCallback, crypto_vtbl))
                .cast::<OpenCallback>()
        };
        let callback = unsafe { &*base };
        callback.password_requested.store(true, Ordering::Relaxed);
        let value = callback
            .password
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        write_password_bstr(password, value.as_deref())
    }

    /// `this` points at the `crypto_vtbl` field of the owning ExtractCallback.
    /// Reads the password from the extract context instead of the open
    /// callback's fields (the layouts differ past the vtable pointers).
    unsafe extern "system" fn extract_crypto_get_text_password(
        this: *mut c_void,
        password: *mut *mut u16,
    ) -> i32 {
        let base = unsafe {
            this.cast::<u8>()
                .sub(mem::offset_of!(ExtractCallback, crypto_vtbl))
                .cast::<ExtractCallback>()
        };
        let callback = unsafe { &*base };
        let mut context = callback
            .context
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        context.password_requested = true;
        let value = context.password.clone();
        drop(context);
        write_password_bstr(password, value.as_deref())
    }

    // --- IArchiveExtractCallback + ICryptoGetTextPassword ---------------
    #[repr(C)]
    struct ExtractVtbl {
        query_interface: QueryInterfaceFn,
        add_ref: AddRefFn,
        release: ReleaseFn,
        set_total: unsafe extern "system" fn(*mut c_void, u64) -> i32,
        set_completed: unsafe extern "system" fn(*mut c_void, *const u64) -> i32,
        get_stream: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void, i32) -> i32,
        prepare_operation: unsafe extern "system" fn(*mut c_void, i32) -> i32,
        set_operation_result: unsafe extern "system" fn(*mut c_void, i32) -> i32,
    }

    static EXTRACT_VTBL: ExtractVtbl = ExtractVtbl {
        query_interface: extract_query_interface,
        add_ref: extract_callback_add_ref,
        release: extract_callback_release,
        set_total: extract_set_total,
        set_completed: extract_set_completed,
        get_stream: extract_get_stream,
        prepare_operation: extract_prepare_operation,
        set_operation_result: extract_set_operation_result,
    };

    #[derive(Clone)]
    struct ExtractItem {
        index: u32,
        relative: PathBuf,
        display_path: String,
        is_dir: bool,
        size: Option<u64>,
        mtime_unix: Option<i64>,
    }

    struct PendingFile {
        temp_path: Option<PathBuf>,
        target: PathBuf,
        /// Shared with the OutStream handed to 7-Zip; `None` once the file has
        /// been flushed and closed by SetOperationResult.
        file: Arc<Mutex<Option<File>>>,
        size: u64,
        mtime_unix: Option<i64>,
        armed: bool,
    }

    impl PendingFile {
        fn disarm(&mut self) {
            self.armed = false;
        }
    }

    impl Drop for PendingFile {
        fn drop(&mut self) {
            close_pending_file(self);
            if self.armed {
                let path = self.temp_path.as_deref().unwrap_or(&self.target);
                let _ = fs::remove_file(path);
            }
        }
    }

    /// 7-Zip callbacks are not consistent about whether their byte counter is
    /// operation-wide or starts at zero for each item. Convert either form to
    /// one operation-wide value plus the current item's value before exposing
    /// it to the UI.
    fn split_callback_progress(
        reported_bytes: u64,
        current_file_base_bytes: u64,
        current_file_total_bytes: Option<u64>,
    ) -> (u64, u64) {
        let current_file_bytes = if reported_bytes >= current_file_base_bytes {
            reported_bytes - current_file_base_bytes
        } else {
            reported_bytes
        };
        let current_file_bytes = current_file_total_bytes
            .map_or(current_file_bytes, |total| current_file_bytes.min(total));
        (
            current_file_base_bytes.saturating_add(current_file_bytes),
            current_file_bytes,
        )
    }

    struct ParallelWorkerProgress {
        aggregate: Arc<ParallelProgress>,
        worker_index: usize,
    }

    struct ParallelProgress {
        inner: Arc<ThrottledProgress<'static>>,
        state: Mutex<ParallelProgressState>,
    }

    struct ParallelProgressState {
        sequence: u64,
        workers: Vec<ParallelWorkerState>,
    }

    struct ParallelWorkerState {
        sequence: u64,
        current_file: String,
        current_file_bytes_processed: u64,
        current_file_total_bytes: Option<u64>,
        entries_processed: u64,
        bytes_processed: u64,
        phase: ProgressPhase,
    }

    impl ParallelProgress {
        fn new(inner: Arc<ThrottledProgress<'static>>, worker_count: usize) -> Arc<Self> {
            Arc::new(Self {
                inner,
                state: Mutex::new(ParallelProgressState {
                    sequence: 0,
                    workers: (0..worker_count)
                        .map(|_| ParallelWorkerState {
                            sequence: 0,
                            current_file: String::new(),
                            current_file_bytes_processed: 0,
                            current_file_total_bytes: None,
                            entries_processed: 0,
                            bytes_processed: 0,
                            phase: ProgressPhase::Extracting,
                        })
                        .collect(),
                }),
            })
        }

        fn report(&self, worker_index: usize, snapshot: ProgressSnapshot) {
            let combined = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                state.sequence = state.sequence.saturating_add(1);
                let sequence = state.sequence;
                let Some(worker) = state.workers.get_mut(worker_index) else {
                    return;
                };
                worker.sequence = sequence;
                worker.phase = snapshot.phase;
                worker.current_file.clone_from(&snapshot.current_file);
                worker.current_file_bytes_processed = snapshot.current_file_bytes_processed;
                worker.current_file_total_bytes = snapshot.current_file_total_bytes;
                worker.entries_processed = worker.entries_processed.max(snapshot.entries_processed);
                worker.bytes_processed = worker.bytes_processed.max(snapshot.bytes_processed);

                let latest = state
                    .workers
                    .iter()
                    .max_by_key(|worker| worker.sequence)
                    .expect("parallel progress has at least one worker");
                let mut combined = snapshot;
                combined.phase = latest.phase;
                combined.current_file = latest.current_file.clone();
                combined.current_file_bytes_processed = latest.current_file_bytes_processed;
                combined.current_file_total_bytes = latest.current_file_total_bytes;
                combined.entries_processed = state
                    .workers
                    .iter()
                    .map(|worker| worker.entries_processed)
                    .sum();
                combined.bytes_processed = state
                    .workers
                    .iter()
                    .map(|worker| worker.bytes_processed)
                    .sum();
                combined
            };
            self.inner.report(combined, false);
        }
    }

    impl ProgressSink for ParallelWorkerProgress {
        fn report(&self, snapshot: ProgressSnapshot) {
            self.aggregate.report(self.worker_index, snapshot);
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum RuntimePolicy {
        Ask,
        OverwriteAll,
        SkipAll,
    }

    impl From<InitialConflictPolicy> for RuntimePolicy {
        fn from(value: InitialConflictPolicy) -> Self {
            match value {
                InitialConflictPolicy::Ask => Self::Ask,
                InitialConflictPolicy::OverwriteAll => Self::OverwriteAll,
                InitialConflictPolicy::SkipAll => Self::SkipAll,
            }
        }
    }

    struct ExtractContext {
        root: PathBuf,
        prepared_dirs: HashSet<PathBuf>,
        assume_targets_missing: bool,
        policy: RuntimePolicy,
        // SAFETY invariant: this reference is only valid for the duration of
        // the enclosing extract() call; the callback never outlives it.
        conflicts: &'static dyn ConflictResolver,
        password: Option<String>,
        password_requested: bool,
        current_file_base_bytes: u64,
        snapshot: ProgressSnapshot,
        summary: OperationSummary,
        pending: VecDeque<PendingFile>,
        error: Option<ArchiveError>,
        test_mode: bool,
    }

    #[repr(C)]
    struct ExtractCallback {
        vtbl: *const ExtractVtbl,
        crypto_vtbl: *const CryptoGetTextPasswordVtbl,
        refs: AtomicU32,
        items: Arc<Vec<ExtractItem>>,
        selected: Arc<HashSet<u32>>,
        cancel: CancellationToken,
        progress: Arc<dyn ProgressSink>,
        context: Arc<Mutex<ExtractContext>>,
    }

    unsafe extern "system" fn extract_query_interface(
        this: *mut c_void,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> i32 {
        if out.is_null() || iid.is_null() {
            return E_INVALIDARG;
        }
        unsafe { *out = ptr::null_mut() };
        let requested = unsafe { *iid };
        if requested == IID_IUNKNOWN
            || requested == IID_IARCHIVE_EXTRACT_CALLBACK
            || requested == IID_IPROGRESS
        {
            unsafe { *out = this };
            unsafe { extract_callback_add_ref(this) };
            S_OK
        } else if requested == IID_ICRYPTO_GET_TEXT_PASSWORD {
            let crypto = unsafe {
                (this.cast::<u8>())
                    .add(mem::offset_of!(ExtractCallback, crypto_vtbl))
                    .cast::<c_void>()
            };
            unsafe { *out = crypto };
            unsafe { extract_crypto_add_ref(crypto) };
            S_OK
        } else {
            E_NOINTERFACE
        }
    }

    unsafe extern "system" fn extract_set_total(this: *mut c_void, total: u64) -> i32 {
        let callback = unsafe { &*(this as *const ExtractCallback) };
        let snapshot = {
            let mut context = callback
                .context
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            // Keep the total supplied by the caller when one is available.
            // It is based on the selected uncompressed entries, while some
            // handlers report a different (archive-side) total here.
            if context.snapshot.total_bytes.is_none() {
                context.snapshot.total_bytes = Some(total);
            }
            context.snapshot.clone()
        };
        callback.progress.report(snapshot);
        S_OK
    }

    unsafe extern "system" fn extract_set_completed(
        this: *mut c_void,
        complete: *const u64,
    ) -> i32 {
        let callback = unsafe { &*(this as *const ExtractCallback) };
        if callback.cancel.is_cancelled() {
            return E_ABORT;
        }
        if complete.is_null() {
            return S_OK;
        }
        let snapshot = {
            let mut context = callback
                .context
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let (bytes_processed, current_file_bytes) = split_callback_progress(
                unsafe { *complete },
                context.current_file_base_bytes,
                context.snapshot.current_file_total_bytes,
            );
            // A per-file callback must never move the overall bar backwards
            // when the next file starts reporting from zero.
            context.snapshot.bytes_processed =
                context.snapshot.bytes_processed.max(bytes_processed);
            context.snapshot.current_file_bytes_processed = current_file_bytes;
            context.snapshot.clone()
        };
        callback.progress.report(snapshot);
        S_OK
    }

    unsafe extern "system" fn extract_prepare_operation(
        _this: *mut c_void,
        _ask_extract_mode: i32,
    ) -> i32 {
        S_OK
    }

    unsafe extern "system" fn extract_get_stream(
        this: *mut c_void,
        index: u32,
        out_stream: *mut *mut c_void,
        ask_extract_mode: i32,
    ) -> i32 {
        if out_stream.is_null() {
            return E_INVALIDARG;
        }
        unsafe { *out_stream = ptr::null_mut() };
        let callback = unsafe { &*(this as *const ExtractCallback) };
        if callback.cancel.is_cancelled() {
            return E_ABORT;
        }
        let mut context = callback
            .context
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // Solid-archive items that are not part of the request are decoded and
        // discarded by 7-Zip with kSkip; no output is wanted.
        if ask_extract_mode == EXTRACT_MODE_SKIP {
            context.snapshot.current_file.clear();
            context.snapshot.current_file_total_bytes = None;
            context.snapshot.current_file_bytes_processed = 0;
            return S_OK;
        }
        let Some(item) = callback.items.get(index as usize).cloned() else {
            return S_OK;
        };
        context.snapshot.current_file.clone_from(&item.display_path);
        context.snapshot.current_file_total_bytes = if item.is_dir { Some(0) } else { item.size };
        context.snapshot.current_file_bytes_processed = 0;
        context.current_file_base_bytes = context.snapshot.bytes_processed;
        if context.test_mode {
            return S_OK;
        }
        if !callback.selected.contains(&item.index) {
            return S_OK;
        }
        let target = context.root.join(&item.relative);
        if item.is_dir {
            // A large archive commonly contains thousands of files under the
            // same few directories. Validate and create each directory once;
            // file extraction below still rechecks the full ancestor chain.
            if !context.prepared_dirs.contains(&target) {
                if let Err(error) = ensure_no_reparse_ancestors(&context.root, &target) {
                    context.error = Some(error);
                    return E_ABORT;
                }
                if let Err(error) = fs::create_dir_all(&target) {
                    context.error = Some(ArchiveError::io(&target, error));
                    return E_ABORT;
                }
                context.prepared_dirs.insert(target);
            }
            return S_OK;
        }
        let parent = target.parent().ok_or_else(|| {
            context.error = Some(ArchiveError::InvalidInput(
                "extraction target has no parent directory".to_owned(),
            ));
            E_ABORT
        });
        let parent = match parent {
            Ok(parent) => parent,
            Err(code) => return code,
        };
        if !context.prepared_dirs.contains(parent) {
            if let Err(error) = ensure_no_reparse_ancestors(&context.root, parent) {
                context.error = Some(error);
                return E_ABORT;
            }
            if let Err(error) = fs::create_dir_all(parent) {
                context.error = Some(ArchiveError::io(parent, error));
                return E_ABORT;
            }
            context.prepared_dirs.insert(parent.to_path_buf());
        }
        // Keep the per-file reparse-point check even when the parent was
        // prepared earlier. An external junction replacement between files
        // must not redirect later outputs outside the extraction root.
        if let Err(error) = ensure_no_reparse_ancestors(&context.root, &target) {
            context.error = Some(error);
            return E_ABORT;
        }
        let mut write_direct = context.assume_targets_missing;
        if !write_direct {
            match fs::symlink_metadata(&target) {
                Ok(metadata) => {
                    if is_reparse(&metadata) {
                        context.error = Some(ArchiveError::ReparsePoint(target.clone()));
                        return E_ABORT;
                    }
                    // Resolve the conflict exactly once per file; the "all" answers
                    // latch into the policy so later files are not asked again.
                    let choice = match context.policy {
                        RuntimePolicy::OverwriteAll => ConflictChoice::Overwrite,
                        RuntimePolicy::SkipAll => ConflictChoice::Skip,
                        RuntimePolicy::Ask => context.conflicts.resolve(&target),
                    };
                    match choice {
                        ConflictChoice::Overwrite => {}
                        ConflictChoice::OverwriteAll => {
                            context.policy = RuntimePolicy::OverwriteAll;
                        }
                        ConflictChoice::Skip | ConflictChoice::SkipAll => {
                            if matches!(choice, ConflictChoice::SkipAll) {
                                context.policy = RuntimePolicy::SkipAll;
                            }
                            context.summary.entries_skipped += 1;
                            return S_OK;
                        }
                        ConflictChoice::Cancel => {
                            context.error = Some(ArchiveError::Cancelled);
                            return E_ABORT;
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // A new target can be created atomically with create_new.
                    // This avoids the temporary-file rename pair for the
                    // common case of extracting into an empty directory.
                    write_direct = true;
                }
                Err(error) => {
                    context.error = Some(ArchiveError::io(&target, error));
                    return E_ABORT;
                }
            }
        }

        let temp_path = (!write_direct).then(|| temporary_path(parent, &target));
        let output_path = temp_path.as_deref().unwrap_or(&target);
        let file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
            .open(output_path)
        {
            Ok(file) => file,
            Err(error) => {
                context.error = Some(ArchiveError::io(output_path, error));
                return E_ABORT;
            }
        };
        let shared = Arc::new(Mutex::new(Some(file)));
        let stream = Box::new(OutStream {
            vtbl: &OUT_STREAM_VTBL,
            refs: AtomicU32::new(1),
            file: Arc::clone(&shared),
        });
        // Ownership of the stream (and the final reference to `shared`) moves
        // to 7-Zip; it releases the stream when the item is finished.
        unsafe { *out_stream = Box::into_raw(stream).cast::<c_void>() };
        context.pending.push_back(PendingFile {
            temp_path,
            target,
            file: shared,
            size: item.size.unwrap_or(0),
            mtime_unix: item.mtime_unix,
            armed: true,
        });
        S_OK
    }

    unsafe extern "system" fn extract_set_operation_result(
        this: *mut c_void,
        operation_result: i32,
    ) -> i32 {
        let callback = unsafe { &*(this as *const ExtractCallback) };
        if callback.cancel.is_cancelled() {
            return E_ABORT;
        }
        let mut context = callback
            .context
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if operation_result != OPERATION_RESULT_OK {
            let message = match operation_result {
                OPERATION_RESULT_UNSUPPORTED_METHOD => "unsupported compression method",
                OPERATION_RESULT_DATA_ERROR => "data error",
                OPERATION_RESULT_CRC_ERROR => "CRC check failed",
                _ => "unknown extraction error",
            };
            if context.error.is_none() {
                context.error = Some(ArchiveError::SevenZip(format!(
                    "7z extraction failed for {}: {message}",
                    context.snapshot.current_file
                )));
            }
            return E_ABORT;
        }
        let Some(mut pending) = context.pending.pop_front() else {
            // Directories and skipped entries have no output stream.
            let should_report = !context.snapshot.current_file.is_empty()
                && context.snapshot.current_file_total_bytes.is_some();
            if should_report {
                let current_file_total_bytes =
                    context.snapshot.current_file_total_bytes.unwrap_or(0);
                let completed_bytes = context
                    .current_file_base_bytes
                    .saturating_add(current_file_total_bytes);
                context.snapshot.current_file_bytes_processed = current_file_total_bytes;
                context.snapshot.bytes_processed =
                    context.snapshot.bytes_processed.max(completed_bytes);
                context.current_file_base_bytes = context.snapshot.bytes_processed;
            }
            if context.test_mode {
                context.summary.entries_processed += 1;
                context.snapshot.entries_processed = context.summary.entries_processed;
            }
            let snapshot = should_report.then(|| context.snapshot.clone());
            drop(context);
            if let Some(snapshot) = snapshot {
                callback.progress.report(snapshot);
            }
            return S_OK;
        };
        {
            let mut guard = pending
                .file
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if let Some(file) = guard.take() {
                if let Some(seconds) = pending.mtime_unix.filter(|seconds| *seconds >= 0) {
                    let modified = UNIX_EPOCH + Duration::from_secs(seconds as u64);
                    let _ = file.set_modified(modified);
                }
                // Dropping the file closes the handle before the rename.
            }
        }
        if let Some(temp_path) = pending.temp_path.as_deref() {
            if let Err(error) = install_temporary(&context.root, temp_path, &pending.target) {
                if context.error.is_none() {
                    context.error = Some(error);
                }
                return E_ABORT;
            }
        }
        pending.disarm();
        context.summary.entries_processed += 1;
        context.summary.bytes_processed =
            context.summary.bytes_processed.saturating_add(pending.size);
        context.snapshot.current_file_total_bytes = Some(pending.size);
        context.snapshot.current_file_bytes_processed = pending.size;
        context.snapshot.entries_processed = context.summary.entries_processed;
        let completed_bytes = context.current_file_base_bytes.saturating_add(pending.size);
        context.snapshot.bytes_processed = context
            .snapshot
            .bytes_processed
            .max(completed_bytes)
            .max(context.summary.bytes_processed);
        context.current_file_base_bytes = context.snapshot.bytes_processed;
        let snapshot = context.snapshot.clone();
        drop(context);
        callback.progress.report(snapshot);
        S_OK
    }

    // --- IArchiveUpdateCallback + ICryptoGetTextPassword2 ---------------
    #[repr(C)]
    struct UpdateVtbl {
        query_interface: QueryInterfaceFn,
        add_ref: AddRefFn,
        release: ReleaseFn,
        set_total: unsafe extern "system" fn(*mut c_void, u64) -> i32,
        set_completed: unsafe extern "system" fn(*mut c_void, *const u64) -> i32,
        get_update_item_info:
            unsafe extern "system" fn(*mut c_void, u32, *mut i32, *mut i32, *mut u32) -> i32,
        get_property: unsafe extern "system" fn(*mut c_void, u32, u32, *mut PropVariant) -> i32,
        get_stream: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32,
        set_operation_result: unsafe extern "system" fn(*mut c_void, i32) -> i32,
    }

    static UPDATE_VTBL: UpdateVtbl = UpdateVtbl {
        query_interface: update_query_interface,
        add_ref: update_callback_add_ref,
        release: update_callback_release,
        set_total: update_set_total,
        set_completed: update_set_completed,
        get_update_item_info: update_get_update_item_info,
        get_property: update_get_property,
        get_stream: update_get_stream,
        set_operation_result: update_set_operation_result,
    };

    #[repr(C)]
    struct CryptoGetTextPassword2Vtbl {
        query_interface: QueryInterfaceFn,
        add_ref: AddRefFn,
        release: ReleaseFn,
        crypto_get_text_password2:
            unsafe extern "system" fn(*mut c_void, *mut i32, *mut *mut u16) -> i32,
    }

    static CRYPTO_GET_TEXT_PASSWORD2_VTBL: CryptoGetTextPassword2Vtbl =
        CryptoGetTextPassword2Vtbl {
            query_interface: crypto2_query_interface,
            add_ref: update_crypto_add_ref,
            release: update_crypto_release,
            crypto_get_text_password2: crypto_get_text_password2,
        };

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SourceKind {
        File,
        Directory,
    }

    #[derive(Clone)]
    struct SourceItem {
        source: PathBuf,
        archive_name: String,
        kind: SourceKind,
        size: u64,
        modified_unix_seconds: Option<i64>,
    }

    struct UpdateContext {
        total_bytes: u64,
        password: Option<String>,
        current_file_base_bytes: u64,
        snapshot: ProgressSnapshot,
        summary: OperationSummary,
        error: Option<ArchiveError>,
    }

    #[repr(C)]
    struct UpdateCallback {
        vtbl: *const UpdateVtbl,
        crypto_vtbl: *const CryptoGetTextPassword2Vtbl,
        refs: AtomicU32,
        items: Arc<Vec<SourceItem>>,
        cancel: CancellationToken,
        progress: Arc<dyn ProgressSink>,
        context: Arc<Mutex<UpdateContext>>,
    }

    unsafe extern "system" fn update_query_interface(
        this: *mut c_void,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> i32 {
        if out.is_null() || iid.is_null() {
            return E_INVALIDARG;
        }
        unsafe { *out = ptr::null_mut() };
        let requested = unsafe { *iid };
        if requested == IID_IUNKNOWN
            || requested == IID_IARCHIVE_UPDATE_CALLBACK
            || requested == IID_IPROGRESS
        {
            unsafe { *out = this };
            unsafe { update_callback_add_ref(this) };
            S_OK
        } else if requested == IID_ICRYPTO_GET_TEXT_PASSWORD2 {
            let crypto = unsafe {
                (this.cast::<u8>())
                    .add(mem::offset_of!(UpdateCallback, crypto_vtbl))
                    .cast::<c_void>()
            };
            unsafe { *out = crypto };
            unsafe { update_crypto_add_ref(crypto) };
            S_OK
        } else {
            E_NOINTERFACE
        }
    }

    unsafe extern "system" fn update_set_total(this: *mut c_void, _total: u64) -> i32 {
        let callback = unsafe { &*(this as *const UpdateCallback) };
        let snapshot = {
            let mut context = callback
                .context
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            // The input sizes collected before the update are the stable
            // denominator for the UI. Some handlers report a compressed or
            // output-side total through this callback.
            context.snapshot.total_bytes = Some(context.total_bytes);
            context.snapshot.clone()
        };
        callback.progress.report(snapshot);
        S_OK
    }

    unsafe extern "system" fn update_set_completed(this: *mut c_void, complete: *const u64) -> i32 {
        let callback = unsafe { &*(this as *const UpdateCallback) };
        if callback.cancel.is_cancelled() {
            return E_ABORT;
        }
        if complete.is_null() {
            return S_OK;
        }
        let snapshot = {
            let mut context = callback
                .context
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let (bytes_processed, current_file_bytes) = split_callback_progress(
                unsafe { *complete },
                context.current_file_base_bytes,
                context.snapshot.current_file_total_bytes,
            );
            // A handler may restart its counter for each input item. Keep the
            // overall value monotonic and derive the file value separately.
            context.snapshot.bytes_processed =
                context.snapshot.bytes_processed.max(bytes_processed);
            context.snapshot.current_file_bytes_processed = current_file_bytes;
            context.snapshot.clone()
        };
        callback.progress.report(snapshot);
        S_OK
    }

    unsafe extern "system" fn update_get_update_item_info(
        this: *mut c_void,
        index: u32,
        new_data: *mut i32,
        new_properties: *mut i32,
        index_in_archive: *mut u32,
    ) -> i32 {
        if new_data.is_null() || new_properties.is_null() || index_in_archive.is_null() {
            return E_INVALIDARG;
        }
        let callback = unsafe { &*(this as *const UpdateCallback) };
        let Some(item) = callback.items.get(index as usize) else {
            return E_INVALIDARG;
        };
        {
            let mut context = callback
                .context
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            context.snapshot.current_file.clone_from(&item.archive_name);
            context.snapshot.current_file_total_bytes = Some(item.size);
            context.snapshot.current_file_bytes_processed = 0;
            context.current_file_base_bytes = context.snapshot.bytes_processed;
        }
        unsafe {
            *new_data = i32::from(item.kind == SourceKind::File);
            *new_properties = 1;
            *index_in_archive = u32::MAX;
        }
        S_OK
    }

    unsafe extern "system" fn update_get_property(
        this: *mut c_void,
        index: u32,
        prop_id: u32,
        value: *mut PropVariant,
    ) -> i32 {
        if value.is_null() {
            return E_INVALIDARG;
        }
        let callback = unsafe { &*(this as *const UpdateCallback) };
        let Some(item) = callback.items.get(index as usize) else {
            unsafe { *value = PropVariant::empty() };
            return E_INVALIDARG;
        };
        unsafe {
            *value = match prop_id {
                KPID_PATH => PropVariant::bstr(&item.archive_name),
                KPID_IS_DIR => PropVariant::bool_value(item.kind == SourceKind::Directory),
                KPID_SIZE if item.kind == SourceKind::File => PropVariant::u64_value(item.size),
                KPID_MTIME => item
                    .modified_unix_seconds
                    .map(PropVariant::filetime)
                    .unwrap_or_else(PropVariant::empty),
                _ => PropVariant::empty(),
            };
        }
        S_OK
    }

    unsafe extern "system" fn update_get_stream(
        this: *mut c_void,
        index: u32,
        out_stream: *mut *mut c_void,
    ) -> i32 {
        if out_stream.is_null() {
            return E_INVALIDARG;
        }
        unsafe { *out_stream = ptr::null_mut() };
        let callback = unsafe { &*(this as *const UpdateCallback) };
        if callback.cancel.is_cancelled() {
            return E_ABORT;
        }
        let Some(item) = callback.items.get(index as usize).cloned() else {
            return E_INVALIDARG;
        };
        {
            let mut context = callback
                .context
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            context.snapshot.current_file.clone_from(&item.archive_name);
            context.snapshot.current_file_total_bytes = Some(item.size);
            context.snapshot.current_file_bytes_processed = 0;
            context.current_file_base_bytes = context.snapshot.bytes_processed;
        }
        if item.kind == SourceKind::Directory {
            return S_OK;
        }
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
            .open(&item.source)
        {
            Ok(file) => file,
            Err(error) => {
                let mut context = callback
                    .context
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if context.error.is_none() {
                    context.error = Some(ArchiveError::io(&item.source, error));
                }
                return E_ABORT;
            }
        };
        let stream = Box::new(InStream {
            vtbl: &IN_STREAM_VTBL,
            refs: AtomicU32::new(1),
            file: Mutex::new(BufReader::with_capacity(
                stream_buffer_size(item.size),
                file,
            )),
        });
        // Ownership of the stream moves to 7-Zip; it releases it after the
        // item has been read.
        unsafe { *out_stream = Box::into_raw(stream).cast::<c_void>() };
        S_OK
    }

    unsafe extern "system" fn update_set_operation_result(
        this: *mut c_void,
        operation_result: i32,
    ) -> i32 {
        let callback = unsafe { &*(this as *const UpdateCallback) };
        if callback.cancel.is_cancelled() {
            return E_ABORT;
        }
        let mut context = callback
            .context
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if operation_result != OPERATION_RESULT_OK {
            if context.error.is_none() {
                context.error = Some(ArchiveError::SevenZip(format!(
                    "7z creation failed for {} (7-Zip result {operation_result})",
                    context.snapshot.current_file
                )));
            }
            return E_ABORT;
        }
        context.summary.entries_processed += 1;
        let current_file_total_bytes = context.snapshot.current_file_total_bytes.unwrap_or(0);
        context.snapshot.current_file_bytes_processed = current_file_total_bytes;
        context.summary.bytes_processed = context
            .summary
            .bytes_processed
            .saturating_add(current_file_total_bytes);
        context.snapshot.bytes_processed = context
            .snapshot
            .bytes_processed
            .max(
                context
                    .current_file_base_bytes
                    .saturating_add(current_file_total_bytes),
            )
            .max(context.summary.bytes_processed);
        context.current_file_base_bytes = context.snapshot.bytes_processed;
        context.snapshot.entries_processed = context.summary.entries_processed;
        let snapshot = context.snapshot.clone();
        drop(context);
        callback.progress.report(snapshot);
        S_OK
    }

    unsafe extern "system" fn crypto2_query_interface(
        this: *mut c_void,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> i32 {
        if out.is_null() || iid.is_null() {
            return E_INVALIDARG;
        }
        unsafe { *out = ptr::null_mut() };
        let requested = unsafe { *iid };
        if requested == IID_IUNKNOWN || requested == IID_ICRYPTO_GET_TEXT_PASSWORD2 {
            unsafe { *out = this };
            unsafe { update_crypto_add_ref(this) };
            S_OK
        } else {
            E_NOINTERFACE
        }
    }

    /// `this` points at the `crypto_vtbl` field of the owning UpdateCallback.
    unsafe extern "system" fn crypto_get_text_password2(
        this: *mut c_void,
        password_is_defined: *mut i32,
        password: *mut *mut u16,
    ) -> i32 {
        if password_is_defined.is_null() || password.is_null() {
            return E_INVALIDARG;
        }
        unsafe {
            *password_is_defined = 0;
            *password = ptr::null_mut();
        }
        let base = unsafe {
            this.cast::<u8>()
                .sub(mem::offset_of!(UpdateCallback, crypto_vtbl))
                .cast::<UpdateCallback>()
        };
        let callback = unsafe { &*base };
        let value = callback
            .context
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .password
            .clone();
        if let Some(value) = value {
            unsafe {
                *password_is_defined = 1;
                *password = sys_alloc_string(&value);
            }
        }
        S_OK
    }

    // The callback objects live on the caller's stack, so their Release never
    // deallocates; 7-Zip only uses the reference count for its own bookkeeping
    // and releases everything before the call returns. Each secondary crypto
    // interface points at its own vtable field, so it needs a base-pointer
    // adjustment before touching the owning callback's reference count.
    unsafe extern "system" fn open_callback_add_ref(this: *mut c_void) -> u32 {
        let object = unsafe { &*this.cast::<OpenCallback>() };
        object.refs.fetch_add(1, Ordering::AcqRel).saturating_add(1)
    }

    unsafe extern "system" fn open_callback_release(this: *mut c_void) -> u32 {
        let object = unsafe { &*this.cast::<OpenCallback>() };
        object.refs.fetch_sub(1, Ordering::AcqRel).saturating_sub(1)
    }

    unsafe extern "system" fn open_crypto_add_ref(this: *mut c_void) -> u32 {
        let base = unsafe {
            this.cast::<u8>()
                .sub(mem::offset_of!(OpenCallback, crypto_vtbl))
                .cast::<OpenCallback>()
        };
        unsafe { open_callback_add_ref(base.cast()) }
    }

    unsafe extern "system" fn open_crypto_release(this: *mut c_void) -> u32 {
        let base = unsafe {
            this.cast::<u8>()
                .sub(mem::offset_of!(OpenCallback, crypto_vtbl))
                .cast::<OpenCallback>()
        };
        unsafe { open_callback_release(base.cast()) }
    }

    unsafe extern "system" fn extract_callback_add_ref(this: *mut c_void) -> u32 {
        let object = unsafe { &*this.cast::<ExtractCallback>() };
        object.refs.fetch_add(1, Ordering::AcqRel).saturating_add(1)
    }

    unsafe extern "system" fn extract_callback_release(this: *mut c_void) -> u32 {
        let object = unsafe { &*this.cast::<ExtractCallback>() };
        object.refs.fetch_sub(1, Ordering::AcqRel).saturating_sub(1)
    }

    unsafe extern "system" fn extract_crypto_add_ref(this: *mut c_void) -> u32 {
        let base = unsafe {
            this.cast::<u8>()
                .sub(mem::offset_of!(ExtractCallback, crypto_vtbl))
                .cast::<ExtractCallback>()
        };
        unsafe { extract_callback_add_ref(base.cast()) }
    }

    unsafe extern "system" fn extract_crypto_release(this: *mut c_void) -> u32 {
        let base = unsafe {
            this.cast::<u8>()
                .sub(mem::offset_of!(ExtractCallback, crypto_vtbl))
                .cast::<ExtractCallback>()
        };
        unsafe { extract_callback_release(base.cast()) }
    }

    unsafe extern "system" fn update_callback_add_ref(this: *mut c_void) -> u32 {
        let object = unsafe { &*this.cast::<UpdateCallback>() };
        object.refs.fetch_add(1, Ordering::AcqRel).saturating_add(1)
    }

    unsafe extern "system" fn update_callback_release(this: *mut c_void) -> u32 {
        let object = unsafe { &*this.cast::<UpdateCallback>() };
        object.refs.fetch_sub(1, Ordering::AcqRel).saturating_sub(1)
    }

    unsafe extern "system" fn update_crypto_add_ref(this: *mut c_void) -> u32 {
        let base = unsafe {
            this.cast::<u8>()
                .sub(mem::offset_of!(UpdateCallback, crypto_vtbl))
                .cast::<UpdateCallback>()
        };
        unsafe { update_callback_add_ref(base.cast()) }
    }

    unsafe extern "system" fn update_crypto_release(this: *mut c_void) -> u32 {
        let base = unsafe {
            this.cast::<u8>()
                .sub(mem::offset_of!(UpdateCallback, crypto_vtbl))
                .cast::<UpdateCallback>()
        };
        unsafe { update_callback_release(base.cast()) }
    }

    // ------------------------------------------------------------------
    // API entry point
    // ------------------------------------------------------------------
    type CreateObjectFn =
        unsafe extern "system" fn(*const Guid, *const Guid, *mut *mut c_void) -> i32;
    type GetNumberOfFormatsFn = unsafe extern "system" fn(*mut u32) -> i32;
    type GetHandlerProperty2Fn = unsafe extern "system" fn(u32, u32, *mut PropVariant) -> i32;

    const HANDLER_PROPERTY_NAME: u32 = 0;
    const HANDLER_PROPERTY_CLASS_ID: u32 = 1;

    struct Api {
        _library: DynamicLibrary,
        create_object: CreateObjectFn,
        zip_clsid: Guid,
        zip_reader_available: bool,
        zip_writer_available: bool,
    }

    fn discover_handler_clsid(library: &DynamicLibrary, expected_name: &str) -> Option<Guid> {
        let number_address = library.symbol(b"GetNumberOfFormats\0")?;
        let property_address = library.symbol(b"GetHandlerProperty2\0")?;
        // SAFETY: both symbols are the documented 7-Zip plugin API exports.
        let get_number: GetNumberOfFormatsFn = unsafe { mem::transmute(number_address) };
        let get_property: GetHandlerProperty2Fn = unsafe { mem::transmute(property_address) };
        let mut count = 0u32;
        if unsafe { get_number(&mut count) } != S_OK || count > 1024 {
            return None;
        }

        for index in 0..count {
            let mut name = PropVariant::empty();
            let name_result = unsafe { get_property(index, HANDLER_PROPERTY_NAME, &mut name) };
            let matches = name_result == S_OK
                && name
                    .as_bstr()
                    .is_some_and(|value| value.eq_ignore_ascii_case(expected_name));
            name.clear();
            if !matches {
                continue;
            }

            let mut class_id = PropVariant::empty();
            let result = unsafe { get_property(index, HANDLER_PROPERTY_CLASS_ID, &mut class_id) };
            let value = (result == S_OK).then(|| class_id.as_guid()).flatten();
            class_id.clear();
            if value.is_some() {
                return value;
            }
        }
        None
    }

    impl Api {
        fn from_library(library: DynamicLibrary) -> ArchiveResult<Self> {
            let create_object = required_symbol!(library, "CreateObject", CreateObjectFn);
            // The format IDs are normally stable, but 7-Zip explicitly
            // exposes the registered class IDs through its plugin API.  Use
            // that value so the same 7z.dll build is used for ZIP as for 7z,
            // including builds whose handler ordering or IDs differ.
            let zip_clsid = discover_handler_clsid(&library, "zip").unwrap_or(CLSID_C_FORMAT_ZIP);
            // Probe the handler: CreateObject must return a live 7z reader.
            let mut probe: *mut c_void = ptr::null_mut();
            // SAFETY: the function pointer came from the 7z.dll export table.
            let hr = unsafe { create_object(&CLSID_C_FORMAT_7Z, &IID_IIN_ARCHIVE, &mut probe) };
            if hr != S_OK || probe.is_null() {
                return Err(ArchiveError::LibraryUnavailable(format!(
                    "the loaded 7z.dll does not expose the 7z format handler \
                     (CreateObject returned {:#010x}, interface={})",
                    hr as u32,
                    !probe.is_null()
                )));
            }
            // SAFETY: CreateObject returned a live handler; release the probe
            // reference directly without wrapping it in an owning handle.
            let probe_vtbl = unsafe { *probe.cast::<*const InArchiveVtbl>() };
            unsafe { ((*probe_vtbl).release)(probe) };
            let mut zip_reader_probe: *mut c_void = ptr::null_mut();
            // ZIP reading is optional for development builds that stage a
            // reduced 7z-only DLL. The bundled runtime exposes both readers.
            let zip_reader_hr =
                unsafe { create_object(&zip_clsid, &IID_IIN_ARCHIVE, &mut zip_reader_probe) };
            let zip_reader_available = zip_reader_hr == S_OK && !zip_reader_probe.is_null();
            if zip_reader_available {
                // SAFETY: the probe is a live IInArchive interface returned
                // by the DLL and is released exactly once.
                let zip_vtbl = unsafe { *zip_reader_probe.cast::<*const InArchiveVtbl>() };
                unsafe { ((*zip_vtbl).release)(zip_reader_probe) };
            }
            let mut zip_probe: *mut c_void = ptr::null_mut();
            // ZIP creation is optional because development builds may stage a
            // 7z-only DLL.  The normal bundled runtime is the full DLL.
            let zip_hr = unsafe { create_object(&zip_clsid, &IID_IOUT_ARCHIVE, &mut zip_probe) };
            let zip_writer_available = zip_hr == S_OK && !zip_probe.is_null();
            if zip_writer_available {
                // SAFETY: the probe is a live IOutArchive interface returned
                // by the DLL and is released exactly once.
                let zip_vtbl = unsafe { *zip_probe.cast::<*const OutArchiveVtbl>() };
                unsafe { ((*zip_vtbl).release)(zip_probe) };
            }
            Ok(Self {
                _library: library,
                create_object,
                zip_clsid,
                zip_reader_available,
                zip_writer_available,
            })
        }

        fn create_in_archive(&self, format: CreateFormat) -> ArchiveResult<RawInArchive> {
            let clsid = match format {
                CreateFormat::SevenZip => &CLSID_C_FORMAT_7Z,
                CreateFormat::Zip => &self.zip_clsid,
                _ => {
                    return Err(ArchiveError::UnsupportedOption(format!(
                        "7z.dll cannot read {} archives",
                        format.label()
                    )));
                }
            };
            let mut raw: *mut c_void = ptr::null_mut();
            // SAFETY: the function pointer came from the 7z.dll export table.
            let hr = unsafe { (self.create_object)(clsid, &IID_IIN_ARCHIVE, &mut raw) };
            if hr != S_OK || raw.is_null() {
                return Err(ArchiveError::SevenZip(format!(
                    "creating the {} reader failed with HRESULT {:#010x}",
                    format.label(),
                    hr as u32
                )));
            }
            // SAFETY: CreateObject returned a live handler for the selected
            // format CLSID.
            Ok(unsafe { RawInArchive::from_raw(raw) })
        }

        fn create_out_archive(&self, format: CreateFormat) -> ArchiveResult<RawOutArchive> {
            let clsid = match format {
                CreateFormat::SevenZip => &CLSID_C_FORMAT_7Z,
                CreateFormat::Zip => &self.zip_clsid,
                _ => {
                    return Err(ArchiveError::UnsupportedOption(format!(
                        "7z.dll cannot create {} archives",
                        format.label()
                    )));
                }
            };
            let mut raw: *mut c_void = ptr::null_mut();
            // SAFETY: the function pointer came from the 7z.dll export table.
            let hr = unsafe { (self.create_object)(clsid, &IID_IOUT_ARCHIVE, &mut raw) };
            if hr != S_OK || raw.is_null() {
                return Err(ArchiveError::SevenZip(format!(
                    "creating the 7z writer failed with HRESULT {:#010x}",
                    hr as u32
                )));
            }
            // SAFETY: CreateObject returned a live handler for this CLSID.
            Ok(unsafe { RawOutArchive::from_raw(raw) })
        }

        fn can_read(&self, format: CreateFormat) -> bool {
            match format {
                CreateFormat::SevenZip => true,
                CreateFormat::Zip => self.zip_reader_available,
                _ => false,
            }
        }
    }

    // ------------------------------------------------------------------
    // Engine
    // ------------------------------------------------------------------
    #[derive(Clone)]
    pub struct SevenZipEngine {
        api: Arc<Api>,
    }

    impl SevenZipEngine {
        pub fn load() -> ArchiveResult<Self> {
            Ok(Self {
                api: Arc::new(Api::from_library(load_7z_library()?)?),
            })
        }

        pub fn can_create(&self, format: CreateFormat) -> bool {
            match format {
                CreateFormat::SevenZip => true,
                CreateFormat::Zip => self.api.zip_writer_available,
                _ => false,
            }
        }

        pub fn can_read(&self, format: CreateFormat) -> bool {
            self.api.can_read(format)
        }

        /// Loads exactly the 7z.dll at `path`.
        pub fn load_from_path(path: &Path) -> ArchiveResult<Self> {
            if !path.is_absolute() {
                return Err(ArchiveError::InvalidInput(
                    "7z.dll path must be absolute".to_owned(),
                ));
            }
            let canonical =
                fs::canonicalize(path).map_err(|error| ArchiveError::io(path, error))?;
            let metadata =
                fs::metadata(&canonical).map_err(|error| ArchiveError::io(&canonical, error))?;
            if !metadata.is_file() {
                return Err(ArchiveError::InvalidInput(format!(
                    "7z.dll path is not a file: {}",
                    canonical.display()
                )));
            }
            let library =
                DynamicLibrary::load(&canonical).map_err(ArchiveError::LibraryUnavailable)?;
            Ok(Self {
                api: Arc::new(Api::from_library(library)?),
            })
        }
    }

    fn read_all_entries(
        api: &Api,
        path: &Path,
        format: CreateFormat,
        password: Option<&str>,
        pathname_codepage: u32,
        throttled: &ThrottledProgress,
        cancel: &CancellationToken,
    ) -> ArchiveResult<(OpenArchive, Vec<ArchiveEntry>)> {
        let open_archive = open_for_read(api, path, format, password, pathname_codepage, cancel)?;
        let entries = read_entries(
            &open_archive.in_archive,
            ProgressPhase::Opening,
            throttled,
            cancel,
            true,
            true,
        )?;
        Ok((open_archive, entries))
    }

    fn run_extract_worker(
        api: &Api,
        archive: &Path,
        format: CreateFormat,
        password: Option<&str>,
        pathname_codepage: u32,
        root: PathBuf,
        items: Arc<Vec<ExtractItem>>,
        selected: Arc<HashSet<u32>>,
        indices: Vec<u32>,
        total_entries: u64,
        total_bytes: Option<u64>,
        cancel: CancellationToken,
        progress: Arc<dyn ProgressSink>,
        conflicts: &'static dyn ConflictResolver,
        policy: RuntimePolicy,
        assume_targets_missing: bool,
    ) -> ArchiveResult<OperationSummary> {
        let open_archive =
            open_for_read(api, archive, format, password, pathname_codepage, &cancel)?;
        let mut snapshot = ProgressSnapshot::new(ProgressPhase::Extracting);
        snapshot.total_entries = Some(total_entries);
        snapshot.total_bytes = total_bytes;
        let mut prepared_dirs = HashSet::new();
        prepared_dirs.insert(root.clone());
        let context = Arc::new(Mutex::new(ExtractContext {
            root,
            prepared_dirs,
            assume_targets_missing,
            policy,
            conflicts,
            password: password.map(str::to_owned),
            password_requested: false,
            current_file_base_bytes: 0,
            snapshot,
            summary: OperationSummary::default(),
            pending: VecDeque::new(),
            error: None,
            test_mode: false,
        }));
        let callback = ExtractCallback {
            vtbl: &EXTRACT_VTBL,
            crypto_vtbl: &EXTRACT_CRYPTO_VTBL,
            refs: AtomicU32::new(1),
            items,
            selected,
            cancel: cancel.clone(),
            progress,
            context: Arc::clone(&context),
        };

        // 7-Zip extracts everything when indices is NULL with numItems
        // (u32)-1; with a subset (possibly empty) the explicit list is used.
        let (indices_ptr, indices_count) = if indices.is_empty() {
            (ptr::null(), 0)
        } else {
            (indices.as_ptr(), indices.len() as u32)
        };
        let hr = open_archive.in_archive.extract(
            indices_ptr,
            indices_count,
            EXTRACT_MODE_EXTRACT,
            (&callback as *const ExtractCallback)
                .cast_mut()
                .cast::<c_void>(),
        );

        let mut context = context.lock().unwrap_or_else(|poison| poison.into_inner());
        let error = context.error.take();
        let password_requested = context.password_requested;
        let summary = context.summary.clone();
        drop(context);

        open_archive.in_archive.close_now();
        // Release 7-Zip's archive/stream references before removing the
        // files left in the pending queue by cancellation or failure.
        cleanup_pending_temp_files(&callback);

        // If 7-Zip requested a password while none was supplied, preserve the
        // retryable password error even when the native callback also reported
        // a generic extraction error.
        if password_requested && !password.is_some_and(|value| !value.is_empty()) {
            return Err(ArchiveError::PasswordRequired);
        }
        if let Some(error) = error {
            return Err(error);
        }
        if cancel.is_cancelled() {
            return Err(ArchiveError::Cancelled);
        }
        if hr != S_OK {
            if password_requested {
                return Err(ArchiveError::PasswordRequired);
            }
            return Err(ArchiveError::SevenZip(format!(
                "7z extraction failed with HRESULT {:#010x}",
                hr as u32
            )));
        }
        Ok(summary)
    }

    fn run_parallel_zip_extract(
        api: Arc<Api>,
        archive: &Path,
        password: Option<&str>,
        pathname_codepage: u32,
        root: PathBuf,
        items: Arc<Vec<ExtractItem>>,
        indices: &[u32],
        total_entries: u64,
        total_bytes: Option<u64>,
        cancel: &CancellationToken,
        progress: Arc<ThrottledProgress<'static>>,
        conflicts: &'static dyn ConflictResolver,
    ) -> ArchiveResult<OperationSummary> {
        const MAX_WORKERS: usize = 8;
        const MIN_ENTRIES_PER_WORKER: usize = 128;
        let logical_cpus = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1);
        let worker_count = logical_cpus
            .min(MAX_WORKERS)
            .min(indices.len() / MIN_ENTRIES_PER_WORKER)
            .max(1);
        if worker_count < 2 {
            return run_extract_worker(
                &api,
                archive,
                CreateFormat::Zip,
                password,
                pathname_codepage,
                root,
                items,
                Arc::new(indices.iter().copied().collect()),
                indices.to_vec(),
                total_entries,
                total_bytes,
                cancel.clone(),
                progress,
                conflicts,
                RuntimePolicy::OverwriteAll,
                true,
            );
        }

        let chunk_size = indices.len().div_ceil(worker_count);
        let aggregate = ParallelProgress::new(Arc::clone(&progress), worker_count);
        let mut workers = Vec::with_capacity(worker_count);
        for (worker_index, chunk) in indices.chunks(chunk_size).enumerate() {
            let api = Arc::clone(&api);
            let archive = archive.to_path_buf();
            let password = password.map(str::to_owned);
            let root = root.clone();
            let items = Arc::clone(&items);
            let selected = Arc::new(chunk.iter().copied().collect::<HashSet<_>>());
            let indices = chunk.to_vec();
            let cancel = cancel.clone();
            let progress: Arc<dyn ProgressSink> = Arc::new(ParallelWorkerProgress {
                aggregate: Arc::clone(&aggregate),
                worker_index,
            });
            workers.push(std::thread::spawn(move || {
                run_extract_worker(
                    &api,
                    &archive,
                    CreateFormat::Zip,
                    password.as_deref(),
                    pathname_codepage,
                    root,
                    items,
                    selected,
                    indices,
                    total_entries,
                    total_bytes,
                    cancel,
                    progress,
                    conflicts,
                    RuntimePolicy::OverwriteAll,
                    true,
                )
            }));
        }

        let mut summary = OperationSummary::default();
        let mut first_error = None;
        for worker in workers {
            match worker.join() {
                Ok(Ok(worker_summary)) => {
                    summary.entries_processed = summary
                        .entries_processed
                        .saturating_add(worker_summary.entries_processed);
                    summary.entries_skipped = summary
                        .entries_skipped
                        .saturating_add(worker_summary.entries_skipped);
                    summary.bytes_processed = summary
                        .bytes_processed
                        .saturating_add(worker_summary.bytes_processed);
                }
                Ok(Err(error)) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                    cancel.cancel();
                }
                Err(_) => {
                    if first_error.is_none() {
                        first_error = Some(ArchiveError::SevenZip(
                            "parallel ZIP extraction worker panicked".to_owned(),
                        ));
                    }
                    cancel.cancel();
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(summary)
    }

    impl ArchiveEngine for SevenZipEngine {
        fn version(&self) -> String {
            "7-Zip (7z.dll)".to_owned()
        }

        fn writable_formats(&self) -> Vec<CreateFormat> {
            let mut formats = vec![CreateFormat::SevenZip];
            if self.api.zip_writer_available {
                formats.insert(0, CreateFormat::Zip);
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
            let mut opening = ProgressSnapshot::new(ProgressPhase::Opening);
            opening.total_bytes = Some(total_input);
            opening.current_file = path.display().to_string();
            throttled.report(opening, true);

            let format = archive_format(path).ok_or_else(|| {
                ArchiveError::UnsupportedOption(
                    "7z.dll could not identify the archive format".to_owned(),
                )
            })?;
            let zip_name_records = if format == CreateFormat::Zip {
                read_zip_name_records(path)?
            } else {
                None
            };
            let pathname_codepage =
                effective_zip_codepage(format, pathname_codepage, zip_name_records.as_deref());
            let (open_archive, mut entries) = read_all_entries(
                &self.api,
                path,
                format,
                password,
                pathname_codepage,
                &throttled,
                cancel,
            )?;
            apply_zip_name_records(&mut entries, zip_name_records.as_deref(), pathname_codepage)?;
            let mut archive_encrypted = false;
            let mut archive_prop = PropVariant::empty();
            let hr = open_archive
                .in_archive
                .get_archive_property(KPID_ENCRYPTED, &mut archive_prop);
            if hr == S_OK {
                archive_encrypted = archive_prop.as_bool().unwrap_or(false);
            }
            archive_prop.clear();
            open_archive.in_archive.close_now();
            let total_uncompressed = entries
                .iter()
                .filter_map(|entry| entry.size)
                .fold(0u64, u64::saturating_add);
            if archive_encrypted {
                for entry in &mut entries {
                    // Header-encrypted archives cannot expose per-entry flags;
                    // conservatively mark every entry.
                    entry.encrypted = true;
                }
            }
            let mut snapshot = ProgressSnapshot::new(ProgressPhase::Finished);
            snapshot.current_file.clear();
            snapshot.entries_processed = entries.len() as u64;
            snapshot.total_entries = Some(entries.len() as u64);
            snapshot.bytes_processed = total_input;
            snapshot.total_bytes = Some(total_input);
            throttled.report(snapshot, true);
            Ok(ArchiveListing {
                archive_path: path.to_path_buf(),
                format_name: format.label().to_owned(),
                filter_name: None,
                entries,
                total_uncompressed_size: total_uncompressed,
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
            let destination_was_missing = match fs::symlink_metadata(destination) {
                Ok(_) => false,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(error) => return Err(ArchiveError::io(destination, error)),
            };
            fs::create_dir_all(destination)
                .map_err(|error| ArchiveError::io(destination, error))?;
            let root = fs::canonicalize(destination)
                .map_err(|error| ArchiveError::io(destination, error))?;
            let root_metadata =
                fs::symlink_metadata(&root).map_err(|error| ArchiveError::io(&root, error))?;
            if !root_metadata.is_dir() {
                return Err(ArchiveError::InvalidInput(
                    "extraction destination is not a directory".to_owned(),
                ));
            }
            if is_reparse(&root_metadata) {
                return Err(ArchiveError::ReparsePoint(root.clone()));
            }
            let _ = file_length(archive)?;

            // SAFETY: the references are only used by callbacks while this
            // function is running, and the shared contexts are dropped here
            // before the borrows end, so extending the lifetimes is sound.
            let conflicts: &'static dyn ConflictResolver = unsafe { mem::transmute(conflicts) };
            let progress: &'static dyn ProgressSink = unsafe { mem::transmute(progress) };
            let throttled = Arc::new(ThrottledProgress::new(progress, PROGRESS_INTERVAL));

            let format = archive_format(archive).ok_or_else(|| {
                ArchiveError::UnsupportedOption(
                    "7z.dll could not identify the archive format".to_owned(),
                )
            })?;
            let zip_name_records = if format == CreateFormat::Zip {
                read_zip_name_records(archive)?
            } else {
                None
            };
            let pathname_codepage = effective_zip_codepage(
                format,
                options.pathname_codepage,
                zip_name_records.as_deref(),
            );
            let open_archive = open_for_read(
                &self.api,
                archive,
                format,
                options.password.as_deref(),
                pathname_codepage,
                cancel,
            )?;
            let mut entries = read_entries(
                &open_archive.in_archive,
                ProgressPhase::Extracting,
                &throttled,
                cancel,
                false,
                false,
            )?;
            apply_zip_name_records(&mut entries, zip_name_records.as_deref(), pathname_codepage)?;
            open_archive.in_archive.close_now();
            drop(open_archive);

            let mut items = Vec::with_capacity(entries.len());
            let mut selected = HashSet::with_capacity(entries.len());
            let mut selected_count = 0u64;
            let mut selected_bytes = 0u64;
            for entry in &entries {
                let relative = safe_relative_path(&entry.path)?;
                let include = options.selection.includes(&relative);
                if include {
                    selected_count = selected_count.checked_add(1).ok_or_else(|| {
                        ArchiveError::LimitExceeded("entry count overflow".to_owned())
                    })?;
                    if selected_count > options.max_entries {
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
                        selected_bytes = selected_bytes.saturating_add(size);
                        if selected_bytes > options.max_total_bytes {
                            return Err(ArchiveError::LimitExceeded(
                                "declared extraction size exceeds the total limit".to_owned(),
                            ));
                        }
                    }
                }
                let index = u32::try_from(entry.index)
                    .map_err(|_| ArchiveError::LimitExceeded("too many entries".to_owned()))?;
                items.push(ExtractItem {
                    index,
                    relative,
                    display_path: entry.display_path.clone(),
                    is_dir: entry.kind == ArchiveEntryKind::Directory,
                    size: entry.size,
                    mtime_unix: entry.modified_unix_seconds,
                });
                if include {
                    selected.insert(index);
                }
            }

            // When the destination was just created, every selected target is
            // absent unless the archive contains duplicate paths. Detect
            // duplicates once so the hot callback path can skip one metadata
            // query per file while retaining normal conflict handling for
            // pre-existing or ambiguous targets.
            let assume_targets_missing =
                destination_was_missing && !has_parallel_path_conflict(&items, &selected);

            let items = Arc::new(items);
            let selected = Arc::new(selected);
            let indices: Vec<u32> = items
                .iter()
                .filter(|item| selected.contains(&item.index))
                .map(|item| item.index)
                .collect();
            let total_entries = options.total_entries_hint.unwrap_or(items.len() as u64);
            let total_bytes = options.total_bytes_hint.or(Some(selected_bytes));
            let summary =
                if format == CreateFormat::Zip && assume_targets_missing && indices.len() >= 256 {
                    run_parallel_zip_extract(
                        Arc::clone(&self.api),
                        archive,
                        options.password.as_deref(),
                        pathname_codepage,
                        root,
                        Arc::clone(&items),
                        &indices,
                        total_entries,
                        total_bytes,
                        cancel,
                        Arc::clone(&throttled),
                        conflicts,
                    )?
                } else {
                    run_extract_worker(
                        &self.api,
                        archive,
                        format,
                        options.password.as_deref(),
                        pathname_codepage,
                        root,
                        items,
                        selected,
                        indices,
                        total_entries,
                        total_bytes,
                        cancel.clone(),
                        throttled.clone(),
                        conflicts,
                        RuntimePolicy::from(options.conflict_policy),
                        assume_targets_missing,
                    )?
                };
            let mut snapshot = ProgressSnapshot::new(ProgressPhase::Finished);
            snapshot.total_entries = Some(total_entries);
            snapshot.total_bytes = total_bytes;
            snapshot.current_file.clear();
            snapshot.phase = ProgressPhase::Finished;
            snapshot.entries_processed = total_entries.max(summary.entries_processed);
            snapshot.bytes_processed = total_bytes
                .unwrap_or(summary.bytes_processed)
                .max(summary.bytes_processed);
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
            if !matches!(options.format, CreateFormat::SevenZip | CreateFormat::Zip) {
                return Err(ArchiveError::UnsupportedOption(format!(
                    "the 7z backend only writes ZIP and 7z archives, not {:?}",
                    options.format
                )));
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
            let file_name = destination.file_name().ok_or_else(|| {
                ArchiveError::InvalidInput("destination has no file name".to_owned())
            })?;
            let final_destination = parent.join(file_name);
            ensure_no_reparse_ancestors(&parent, &final_destination)?;

            let (mut items, total_bytes) = collect_sources(files, &final_destination, cancel)?;
            if options.format == CreateFormat::Zip {
                // ZIP readers infer parent directories from file paths, and
                // the bundled ZIP handler does not safely accept explicit
                // directory records through this callback ABI. Omitting them
                // also avoids an empty update callback for every directory in
                // large trees.
                items.retain(|item| item.kind == SourceKind::File);
            }
            let item_count = u32::try_from(items.len())
                .map_err(|_| ArchiveError::LimitExceeded("too many entries".to_owned()))?;

            // SAFETY: the reference is only used by callbacks while this
            // function is running, and the shared context is dropped here
            // before the borrow ends, so extending the lifetime is sound.
            let progress: &'static dyn ProgressSink = unsafe { mem::transmute(progress) };
            let throttled = Arc::new(ThrottledProgress::new(progress, PROGRESS_INTERVAL));
            let mut opening = ProgressSnapshot::new(ProgressPhase::Opening);
            opening.total_entries = Some(items.len() as u64);
            opening.total_bytes = Some(total_bytes);
            opening.current_file = final_destination.display().to_string();
            throttled.report(opening, true);

            let temporary_path = temporary_path(&parent, &final_destination);
            let mut temp_file = Some(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary_path)
                    .map_err(|error| ArchiveError::io(&temporary_path, error))?,
            );

            let out_archive = match self.api.create_out_archive(options.format) {
                Ok(archive) => archive,
                Err(error) => {
                    drop(temp_file.take());
                    let _ = fs::remove_file(&temporary_path);
                    return Err(error);
                }
            };
            let result: ArchiveResult<OperationSummary> = (|| {
                match apply_create_properties(&out_archive, options) {
                    Ok(()) => {
                        let shared_file: Arc<Mutex<Option<File>>> = Arc::new(Mutex::new(Some(
                            temp_file.take().expect("temporary file is live"),
                        )));
                        let stream = Box::new(OutStream {
                            vtbl: &OUT_STREAM_VTBL,
                            refs: AtomicU32::new(1),
                            file: Arc::clone(&shared_file),
                        });
                        let stream_ptr = Box::into_raw(stream).cast::<c_void>();
                        let mut snapshot = ProgressSnapshot::new(ProgressPhase::Compressing);
                        snapshot.total_entries = Some(items.len() as u64);
                        snapshot.total_bytes = Some(total_bytes);
                        let items = Arc::new(items);
                        let context = Arc::new(Mutex::new(UpdateContext {
                            total_bytes,
                            password: options.password.clone(),
                            current_file_base_bytes: 0,
                            snapshot,
                            summary: OperationSummary::default(),
                            error: None,
                        }));
                        let callback = UpdateCallback {
                            vtbl: &UPDATE_VTBL,
                            crypto_vtbl: &CRYPTO_GET_TEXT_PASSWORD2_VTBL,
                            refs: AtomicU32::new(1),
                            items: Arc::clone(&items),
                            cancel: cancel.clone(),
                            progress: throttled.clone(),
                            context: Arc::clone(&context),
                        };
                        let hr = out_archive.update_items(
                            stream_ptr,
                            item_count,
                            (&callback as *const UpdateCallback)
                                .cast_mut()
                                .cast::<c_void>(),
                        );
                        let mut context =
                            context.lock().unwrap_or_else(|poison| poison.into_inner());
                        let error = context.error.take();
                        let mut summary = context.summary.clone();
                        summary.bytes_processed = total_bytes;
                        let mut snapshot = context.snapshot.clone();
                        drop(context);
                        // 7-Zip released the stream; close the file ourselves
                        // through the Arc we kept.
                        let mut guard = shared_file
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner());
                        if let Some(file) = guard.take() {
                            drop(file);
                        }
                        drop(guard);
                        if let Some(error) = error {
                            return create_error(error);
                        }
                        if cancel.is_cancelled() {
                            return create_error(ArchiveError::Cancelled);
                        }
                        if hr != S_OK {
                            return create_error(ArchiveError::SevenZip(format!(
                                "7z creation failed with HRESULT {:#010x}",
                                hr as u32
                            )));
                        }
                        if let Err(error) =
                            install_temporary(&parent, &temporary_path, &final_destination)
                        {
                            return create_error(error);
                        }
                        snapshot.phase = ProgressPhase::Finished;
                        snapshot.current_file.clear();
                        snapshot.entries_processed = summary.entries_processed;
                        snapshot.bytes_processed = summary.bytes_processed;
                        throttled.report(snapshot, true);
                        Ok(summary)
                    }
                    Err(error) => {
                        // Close the still-owned file before the archive is
                        // released. The temporary path is removed below, after
                        // `out_archive` has released any stream references.
                        drop(temp_file.take());
                        create_error(error)
                    }
                }
            })();
            drop(out_archive);
            if result.is_err() {
                // 7-Zip may retain the output stream until the archive object
                // is released. Remove the path only after that release so a
                // cancelled operation cannot strand its temporary file.
                let _ = fs::remove_file(&temporary_path);
            }
            result
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
            // SAFETY: the reference is only used by callbacks while this
            // function is running, and the shared context is dropped here
            // before the borrow ends, so extending the lifetime is sound.
            let progress: &'static dyn ProgressSink = unsafe { mem::transmute(progress) };
            let throttled = Arc::new(ThrottledProgress::new(progress, PROGRESS_INTERVAL));
            let mut opening = ProgressSnapshot::new(ProgressPhase::Opening);
            opening.total_bytes = Some(total_input);
            opening.current_file = archive.display().to_string();
            throttled.report(opening, true);

            let format = archive_format(archive).ok_or_else(|| {
                ArchiveError::UnsupportedOption(
                    "7z.dll could not identify the archive format".to_owned(),
                )
            })?;
            let open_archive = open_for_read(&self.api, archive, format, password, 0, cancel)?;
            let entries = read_entries(
                &open_archive.in_archive,
                ProgressPhase::Testing,
                &throttled,
                cancel,
                false,
                false,
            )?;
            let mut snapshot = ProgressSnapshot::new(ProgressPhase::Testing);
            snapshot.total_entries = Some(entries.len() as u64);
            snapshot.total_bytes = Some(total_input);
            // The test path never resolves conflicts (no output streams are
            // requested), so a trivial resolver that is never called is enough.
            // SAFETY: the resolver outlives the Extract call, and test mode
            // returns before `resolve` could ever be invoked.
            let resolver: &'static dyn ConflictResolver =
                unsafe { &*(&NeverResolver as *const dyn ConflictResolver) };
            let items = Arc::new(
                entries
                    .iter()
                    .map(|entry| ExtractItem {
                        index: u32::try_from(entry.index).unwrap_or(u32::MAX),
                        relative: safe_relative_path(&entry.path).unwrap_or_default(),
                        display_path: entry.display_path.clone(),
                        is_dir: entry.kind == ArchiveEntryKind::Directory,
                        size: entry.size,
                        mtime_unix: entry.modified_unix_seconds,
                    })
                    .collect::<Vec<_>>(),
            );
            let selected = Arc::new(HashSet::new());
            let context = Arc::new(Mutex::new(ExtractContext {
                root: PathBuf::new(),
                prepared_dirs: HashSet::new(),
                assume_targets_missing: false,
                policy: RuntimePolicy::OverwriteAll,
                conflicts: resolver,
                password: password.map(str::to_owned),
                password_requested: false,
                current_file_base_bytes: 0,
                snapshot,
                summary: OperationSummary::default(),
                pending: VecDeque::new(),
                error: None,
                test_mode: true,
            }));
            let callback = ExtractCallback {
                vtbl: &EXTRACT_VTBL,
                crypto_vtbl: &EXTRACT_CRYPTO_VTBL,
                refs: AtomicU32::new(1),
                items,
                selected,
                cancel: cancel.clone(),
                progress: throttled.clone(),
                context: Arc::clone(&context),
            };
            let hr = open_archive.in_archive.extract(
                ptr::null(),
                u32::MAX,
                EXTRACT_MODE_TEST,
                (&callback as *const ExtractCallback)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            let mut context = context.lock().unwrap_or_else(|poison| poison.into_inner());
            let error = context.error.take();
            let password_requested = context.password_requested;
            let summary = context.summary.clone();
            let mut snapshot = context.snapshot.clone();
            drop(context);
            open_archive.in_archive.close_now();
            if password_requested && !password.is_some_and(|value| !value.is_empty()) {
                return Err(ArchiveError::PasswordRequired);
            }
            if let Some(error) = error {
                return Err(error);
            }
            if cancel.is_cancelled() {
                return Err(ArchiveError::Cancelled);
            }
            if hr != S_OK {
                if password_requested {
                    return Err(ArchiveError::PasswordRequired);
                }
                return Err(ArchiveError::SevenZip(format!(
                    "7z test failed with HRESULT {:#010x}",
                    hr as u32
                )));
            }
            snapshot.phase = ProgressPhase::Finished;
            snapshot.current_file.clear();
            snapshot.entries_processed = summary.entries_processed;
            snapshot.bytes_processed = total_input;
            throttled.report(snapshot, true);
            Ok(summary)
        }
    }

    // The extract context requires a ConflictResolver; the test path never
    // resolves conflicts (test mode returns no streams), so a trivial
    // never-called resolver is used.
    struct NeverResolver;

    impl ConflictResolver for NeverResolver {
        fn resolve(&self, _destination: &Path) -> ConflictChoice {
            ConflictChoice::Cancel
        }
    }

    // ------------------------------------------------------------------
    // Composite engine: 7z/ZIP read and creation -> 7z.dll when available;
    // libarchive handles all other formats and remains the fallback when the
    // optional 7z DLL is unavailable.
    // ------------------------------------------------------------------
    pub struct CompositeEngine {
        libarchive: LibArchiveEngine,
        sevenzip: Option<SevenZipEngine>,
    }

    impl CompositeEngine {
        pub fn new(libarchive: LibArchiveEngine, sevenzip: Option<SevenZipEngine>) -> Self {
            Self {
                libarchive,
                sevenzip,
            }
        }
    }

    impl ArchiveEngine for CompositeEngine {
        fn version(&self) -> String {
            match &self.sevenzip {
                Some(_) => format!("7-Zip (7z.dll) + {}", self.libarchive.version()),
                None => self.libarchive.version(),
            }
        }

        fn writable_formats(&self) -> Vec<CreateFormat> {
            let mut formats = self.libarchive.writable_formats();
            if let Some(sevenzip) = &self.sevenzip {
                for format in [CreateFormat::Zip, CreateFormat::SevenZip] {
                    if sevenzip.can_create(format) && !formats.contains(&format) {
                        formats.push(format);
                    }
                }
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
            if let Some(sevenzip) = &self.sevenzip {
                if archive_format(path).is_some_and(|format| sevenzip.can_read(format)) {
                    return sevenzip.list(path, password, pathname_codepage, progress, cancel);
                }
            }
            self.libarchive
                .list(path, password, pathname_codepage, progress, cancel)
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
            if let Some(sevenzip) = &self.sevenzip {
                if archive_format(archive).is_some_and(|format| sevenzip.can_read(format)) {
                    return sevenzip.extract(
                        archive,
                        destination,
                        options,
                        progress,
                        conflicts,
                        cancel,
                    );
                }
            }
            self.libarchive
                .extract(archive, destination, options, progress, conflicts, cancel)
        }

        fn create(
            &self,
            destination: &Path,
            files: &[PathBuf],
            options: &CreateOptions,
            progress: &dyn ProgressSink,
            cancel: &CancellationToken,
        ) -> ArchiveResult<OperationSummary> {
            if options.format == CreateFormat::SevenZip
                || (options.format == CreateFormat::Zip
                    && self
                        .sevenzip
                        .as_ref()
                        .is_some_and(|engine| engine.can_create(CreateFormat::Zip)))
            {
                self.sevenzip
                    .as_ref()
                    .ok_or_else(sevenzip_unavailable)?
                    .create(destination, files, options, progress, cancel)
            } else {
                self.libarchive
                    .create(destination, files, options, progress, cancel)
            }
        }

        fn test(
            &self,
            archive: &Path,
            password: Option<&str>,
            progress: &dyn ProgressSink,
            cancel: &CancellationToken,
        ) -> ArchiveResult<OperationSummary> {
            if let Some(sevenzip) = &self.sevenzip {
                if archive_format(archive).is_some_and(|format| sevenzip.can_read(format)) {
                    return sevenzip.test(archive, password, progress, cancel);
                }
            }
            self.libarchive.test(archive, password, progress, cancel)
        }
    }

    fn sevenzip_unavailable() -> ArchiveError {
        ArchiveError::LibraryUnavailable(
            "the bundled 7z.dll could not be loaded; 7z archives need it".to_owned(),
        )
    }

    // ------------------------------------------------------------------
    // Shared helpers
    // ------------------------------------------------------------------

    fn archive_format(path: &Path) -> Option<CreateFormat> {
        let Ok(mut file) = File::open(path) else {
            return None;
        };
        let mut signature = [0u8; 6];
        let Ok(amount) = file.read(&mut signature) else {
            return None;
        };
        if amount == SEVENZIP_SIGNATURE.len() && signature == SEVENZIP_SIGNATURE {
            return Some(CreateFormat::SevenZip);
        }
        if amount >= 4
            && signature[0] == b'P'
            && signature[1] == b'K'
            && matches!(signature[2], 0x03 | 0x05 | 0x07)
            && matches!(signature[3], 0x04 | 0x06 | 0x08)
        {
            return Some(CreateFormat::Zip);
        }
        None
    }

    const ZIP_EOCD_SIGNATURE: u32 = 0x0605_4B50;
    const ZIP64_EOCD_SIGNATURE: u32 = 0x0606_4B50;
    const ZIP64_LOCATOR_SIGNATURE: u32 = 0x0706_4B50;
    const ZIP_CENTRAL_SIGNATURE: u32 = 0x0201_4B50;
    const ZIP_EOCD_SIZE: usize = 22;
    const ZIP_MAX_COMMENT_SIZE: u64 = 65_535;

    struct ZipNameRecord {
        raw_name: Vec<u8>,
        flags: u16,
        unicode_name: Option<String>,
    }

    struct ZipDirectoryLayout {
        entries: u64,
        offset: u64,
        size: u64,
    }

    /// Determines the code page used for legacy ZIP names.  The ZIP format
    /// does not declare a code page for names without the UTF-8 flag, so
    /// automatic mode samples the raw central-directory names with the
    /// detector shared by the libarchive backend.
    fn effective_zip_codepage(
        format: CreateFormat,
        requested: u32,
        records: Option<&[ZipNameRecord]>,
    ) -> u32 {
        if format != CreateFormat::Zip || requested != 0 {
            return requested;
        }
        records.map(detect_zip_codepage).unwrap_or(0)
    }

    fn detect_zip_codepage(records: &[ZipNameRecord]) -> u32 {
        let mut weights = BTreeMap::<u32, u64>::new();
        for record in records {
            if record.flags & 0x0800 != 0 || record.unicode_name.is_some() {
                continue;
            }
            let detected = encoding::detect(&record.raw_name);
            if matches!(
                detected,
                encoding::DetectedEncoding::Utf8
                    | encoding::DetectedEncoding::Utf16Le
                    | encoding::DetectedEncoding::Utf16Be
            ) {
                continue;
            }
            let weight = record
                .raw_name
                .iter()
                .filter(|byte| **byte >= 0x80)
                .count()
                .max(1) as u64;
            *weights.entry(detected.codepage()).or_default() += weight;
        }

        weights
            .into_iter()
            .max_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)))
            .map(|(codepage, _)| codepage)
            .unwrap_or(0)
    }

    fn read_zip_name_records(path: &Path) -> ArchiveResult<Option<Vec<ZipNameRecord>>> {
        let mut file = File::open(path).map_err(|error| ArchiveError::io(path, error))?;
        let file_length = file
            .metadata()
            .map_err(|error| ArchiveError::io(path, error))?
            .len();
        if file_length < ZIP_EOCD_SIZE as u64 {
            return Ok(None);
        }

        let tail_length = file_length.min(ZIP_EOCD_SIZE as u64 + ZIP_MAX_COMMENT_SIZE);
        file.seek(SeekFrom::Start(file_length - tail_length))
            .map_err(|error| ArchiveError::io(path, error))?;
        let mut tail = Vec::with_capacity(tail_length as usize);
        file.read_to_end(&mut tail)
            .map_err(|error| ArchiveError::io(path, error))?;
        let Some(eocd_index) = find_zip_eocd(&tail) else {
            return Ok(None);
        };
        let eocd_offset = file_length - tail_length + eocd_index as u64;
        let Some(layout) =
            zip_directory_layout(&mut file, file_length, eocd_offset, &tail[eocd_index..])?
        else {
            return Ok(None);
        };
        if layout.entries > MAX_LIST_ENTRIES
            || layout
                .offset
                .checked_add(layout.size)
                .is_none_or(|end| end > file_length)
        {
            return Ok(None);
        }

        file.seek(SeekFrom::Start(layout.offset))
            .map_err(|error| ArchiveError::io(path, error))?;
        let mut records = Vec::with_capacity(layout.entries as usize);
        let mut consumed = 0u64;
        for _ in 0..layout.entries {
            if layout.size.saturating_sub(consumed) < 46 {
                return Ok(None);
            }
            let mut header = [0u8; 46];
            file.read_exact(&mut header)
                .map_err(|error| ArchiveError::io(path, error))?;
            consumed += 46;
            if read_u32(&header, 0) != Some(ZIP_CENTRAL_SIGNATURE) {
                return Ok(None);
            }

            let flags = read_u16(&header, 8).unwrap_or(0);
            let name_length = u64::from(read_u16(&header, 28).unwrap_or(0));
            let extra_length = u64::from(read_u16(&header, 30).unwrap_or(0));
            let comment_length = u64::from(read_u16(&header, 32).unwrap_or(0));
            let variable_length = name_length
                .checked_add(extra_length)
                .and_then(|length| length.checked_add(comment_length))
                .ok_or_else(|| {
                    ArchiveError::LimitExceeded("ZIP central-directory length overflow".to_owned())
                })?;
            if variable_length > layout.size.saturating_sub(consumed) {
                return Ok(None);
            }

            let mut raw_name = vec![0u8; name_length as usize];
            file.read_exact(&mut raw_name)
                .map_err(|error| ArchiveError::io(path, error))?;
            let mut extra = vec![0u8; extra_length as usize];
            file.read_exact(&mut extra)
                .map_err(|error| ArchiveError::io(path, error))?;
            if comment_length > 0 {
                file.seek(SeekFrom::Current(comment_length as i64))
                    .map_err(|error| ArchiveError::io(path, error))?;
            }
            consumed += variable_length;
            records.push(ZipNameRecord {
                unicode_name: unicode_path_extra(&raw_name, &extra),
                raw_name,
                flags,
            });
        }

        Some(records)
            .filter(|records| records.len() as u64 == layout.entries)
            .map_or(Ok(None), |records| Ok(Some(records)))
    }

    fn decode_zip_name(record: &ZipNameRecord, codepage: u32) -> Option<String> {
        if let Some(unicode_name) = &record.unicode_name {
            return Some(unicode_name.clone());
        }
        if record.flags & 0x0800 != 0 {
            return Some(String::from_utf8_lossy(&record.raw_name).into_owned());
        }
        encoding::decode_name_with_codepage(&record.raw_name, codepage)
    }

    fn apply_zip_name_records(
        entries: &mut [ArchiveEntry],
        records: Option<&[ZipNameRecord]>,
        codepage: u32,
    ) -> ArchiveResult<()> {
        let Some(records) = records.filter(|records| records.len() == entries.len()) else {
            return Ok(());
        };

        let mut total_path_bytes = 0u64;
        for (entry, record) in entries.iter_mut().zip(records) {
            let Some(display_path) = decode_zip_name(record, codepage) else {
                continue;
            };
            let Ok(path) = build_path(&display_path) else {
                continue;
            };
            total_path_bytes = checked_add_with_limit(
                total_path_bytes,
                (display_path.encode_utf16().count() as u64).saturating_mul(2),
                MAX_LIST_PATH_BYTES,
                "7z ZIP listing pathname metadata",
            )?;
            entry.path = path;
            entry.display_path = display_path;
        }
        Ok(())
    }

    fn find_zip_eocd(tail: &[u8]) -> Option<usize> {
        if tail.len() < ZIP_EOCD_SIZE {
            return None;
        }
        (0..=tail.len() - ZIP_EOCD_SIZE).rev().find(|&index| {
            read_u32(tail, index) == Some(ZIP_EOCD_SIGNATURE)
                && read_u16(tail, index + 20).is_some_and(|comment_length| {
                    index + ZIP_EOCD_SIZE + usize::from(comment_length) <= tail.len()
                })
        })
    }

    fn zip_directory_layout(
        file: &mut File,
        file_length: u64,
        eocd_offset: u64,
        eocd: &[u8],
    ) -> ArchiveResult<Option<ZipDirectoryLayout>> {
        if eocd.len() < ZIP_EOCD_SIZE {
            return Ok(None);
        }
        let disk = read_u16(eocd, 4).unwrap_or(u16::MAX);
        let central_disk = read_u16(eocd, 6).unwrap_or(u16::MAX);
        let entries_on_disk = read_u16(eocd, 8).unwrap_or(u16::MAX);
        let entries = read_u16(eocd, 10).unwrap_or(u16::MAX);
        let size = u64::from(read_u32(eocd, 12).unwrap_or(u32::MAX));
        let offset = u64::from(read_u32(eocd, 16).unwrap_or(u32::MAX));
        let needs_zip64 = disk == u16::MAX
            || central_disk == u16::MAX
            || entries_on_disk == u16::MAX
            || entries == u16::MAX
            || size == u64::from(u32::MAX)
            || offset == u64::from(u32::MAX);
        if !needs_zip64 {
            return Ok(Some(ZipDirectoryLayout {
                entries: u64::from(entries),
                offset,
                size,
            }));
        }

        if eocd_offset < 20 {
            return Ok(None);
        }
        let mut locator = [0u8; 20];
        file.seek(SeekFrom::Start(eocd_offset - 20))
            .map_err(|error| ArchiveError::io("ZIP64 locator", error))?;
        file.read_exact(&mut locator)
            .map_err(|error| ArchiveError::io("ZIP64 locator", error))?;
        if read_u32(&locator, 0) != Some(ZIP64_LOCATOR_SIGNATURE)
            || read_u32(&locator, 4) != Some(0)
        {
            return Ok(None);
        }
        let Some(zip64_offset) = read_u64(&locator, 8) else {
            return Ok(None);
        };
        if zip64_offset
            .checked_add(56)
            .is_none_or(|end| end > file_length)
        {
            return Ok(None);
        }
        let mut record = [0u8; 56];
        file.seek(SeekFrom::Start(zip64_offset))
            .map_err(|error| ArchiveError::io("ZIP64 end record", error))?;
        file.read_exact(&mut record)
            .map_err(|error| ArchiveError::io("ZIP64 end record", error))?;
        if read_u32(&record, 0) != Some(ZIP64_EOCD_SIGNATURE)
            || read_u64(&record, 4).is_none_or(|size| size < 44)
        {
            return Ok(None);
        }
        Ok(Some(ZipDirectoryLayout {
            entries: read_u64(&record, 32).unwrap_or(0),
            size: read_u64(&record, 40).unwrap_or(0),
            offset: read_u64(&record, 48).unwrap_or(0),
        }))
    }

    fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
        bytes
            .get(offset..offset.checked_add(2)?)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
        bytes
            .get(offset..offset.checked_add(4)?)
            .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
        bytes.get(offset..offset.checked_add(8)?).map(|bytes| {
            u64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ])
        })
    }

    fn unicode_path_extra(raw_name: &[u8], extra: &[u8]) -> Option<String> {
        let mut offset = 0usize;
        while offset.checked_add(4).is_some_and(|end| end <= extra.len()) {
            let id = read_u16(extra, offset).unwrap_or(0);
            let length = usize::from(read_u16(extra, offset + 2).unwrap_or(0));
            let data_start = offset + 4;
            let Some(data_end) = data_start.checked_add(length) else {
                return None;
            };
            if data_end > extra.len() {
                return None;
            }
            if id == 0x7075
                && length >= 5
                && extra[data_start] == 1
                && crc32(raw_name) == read_u32(extra, data_start + 1).unwrap_or(0)
            {
                return std::str::from_utf8(&extra[data_start + 5..data_end])
                    .ok()
                    .map(str::to_owned);
            }
            offset = data_end;
        }
        None
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let mask = 0u32.wrapping_sub(crc & 1);
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    struct OpenArchive {
        in_archive: RawInArchive,
        /// The input stream handed to 7-Zip. Its reference count includes one
        /// reference held by this Box; 7-Zip releases its own reference in
        /// Close, so this Box must outlive `in_archive`.
        _stream: Box<InStream>,
        _callback: OpenCallback,
    }

    fn open_for_read(
        api: &Api,
        path: &Path,
        format: CreateFormat,
        password: Option<&str>,
        _pathname_codepage: u32,
        cancel: &CancellationToken,
    ) -> ArchiveResult<OpenArchive> {
        let in_archive = api.create_in_archive(format)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
            .open(path)
            .map_err(|error| ArchiveError::io(path, error))?;
        let stream = Box::new(InStream {
            vtbl: &IN_STREAM_VTBL,
            refs: AtomicU32::new(2),
            file: Mutex::new(BufReader::with_capacity(STREAM_BUFFER_SIZE, file)),
        });
        let callback = OpenCallback {
            vtbl: &OPEN_VTBL,
            crypto_vtbl: &CRYPTO_GET_TEXT_PASSWORD_VTBL,
            refs: AtomicU32::new(1),
            password: Mutex::new(password.map(str::to_owned)),
            password_requested: AtomicBool::new(false),
            cancel: cancel.clone(),
        };
        let hr = in_archive.open(
            (stream.as_ref() as *const InStream)
                .cast_mut()
                .cast::<c_void>(),
            (&callback as *const OpenCallback)
                .cast_mut()
                .cast::<c_void>(),
        );
        if hr != S_OK {
            if callback.password_requested.load(Ordering::Relaxed) {
                return Err(ArchiveError::PasswordRequired);
            }
            return Err(ArchiveError::SevenZip(format!(
                "opening {} archive failed with HRESULT {:#010x}",
                format.label(),
                hr as u32
            )));
        }
        Ok(OpenArchive {
            in_archive,
            _stream: stream,
            _callback: callback,
        })
    }

    fn read_entries(
        in_archive: &RawInArchive,
        phase: ProgressPhase,
        throttled: &ThrottledProgress,
        cancel: &CancellationToken,
        include_encryption: bool,
        report_progress: bool,
    ) -> ArchiveResult<Vec<ArchiveEntry>> {
        let mut count: u32 = 0;
        require_hr(
            in_archive.get_number_of_items(&mut count),
            "reading 7z item count",
        )?;
        if u64::from(count) > MAX_LIST_ENTRIES {
            return Err(ArchiveError::LimitExceeded(format!(
                "7z archive has more than {MAX_LIST_ENTRIES} entries"
            )));
        }
        let mut entries = Vec::with_capacity(count as usize);
        let mut snapshot = ProgressSnapshot::new(phase);
        snapshot.total_entries = Some(u64::from(count));
        let mut total_path_bytes = 0u64;
        for index in 0..count {
            check_cancel(cancel)?;
            let mut path_prop = PropVariant::empty();
            require_hr(
                in_archive.get_property(index, KPID_PATH, &mut path_prop),
                "reading 7z entry name",
            )?;
            let display_path = path_prop.as_bstr().unwrap_or_default();
            path_prop.clear();
            total_path_bytes = checked_add_with_limit(
                total_path_bytes,
                (display_path.encode_utf16().count() as u64).saturating_mul(2),
                MAX_LIST_PATH_BYTES,
                "7z listing pathname metadata",
            )?;

            let mut is_dir_prop = PropVariant::empty();
            require_hr(
                in_archive.get_property(index, KPID_IS_DIR, &mut is_dir_prop),
                "reading 7z entry type",
            )?;
            let is_dir = is_dir_prop
                .as_bool()
                .unwrap_or_else(|| display_path.ends_with(['/', '\\']));
            is_dir_prop.clear();

            let mut size_prop = PropVariant::empty();
            require_hr(
                in_archive.get_property(index, KPID_SIZE, &mut size_prop),
                "reading 7z entry size",
            )?;
            let size = size_prop.as_u64();
            size_prop.clear();
            if let Some(size) = size {
                checked_add_with_limit(
                    0,
                    size,
                    MAX_LIST_DECLARED_BYTES,
                    "7z listing declared size",
                )?;
            }

            let mut mtime_prop = PropVariant::empty();
            require_hr(
                in_archive.get_property(index, KPID_MTIME, &mut mtime_prop),
                "reading 7z entry time",
            )?;
            let modified_unix_seconds = mtime_prop.as_filetime_seconds();
            mtime_prop.clear();

            let encrypted = if include_encryption {
                let mut encrypted_prop = PropVariant::empty();
                require_hr(
                    in_archive.get_property(index, KPID_ENCRYPTED, &mut encrypted_prop),
                    "reading 7z entry encryption flag",
                )?;
                let encrypted = encrypted_prop.as_bool().unwrap_or(false);
                encrypted_prop.clear();
                encrypted
            } else {
                false
            };

            let path = build_path(&display_path)?;
            entries.push(ArchiveEntry {
                index: u64::from(index),
                path,
                display_path,
                size,
                compressed_size: None,
                modified_unix_seconds,
                kind: if is_dir {
                    ArchiveEntryKind::Directory
                } else {
                    ArchiveEntryKind::File
                },
                encrypted,
            });
            if report_progress {
                snapshot.entries_processed = u64::from(index) + 1;
                snapshot.current_file = entries
                    .last()
                    .expect("entry was pushed")
                    .display_path
                    .clone();
                throttled.report(snapshot.clone(), false);
            }
        }
        Ok(entries)
    }

    fn build_path(display: &str) -> ArchiveResult<PathBuf> {
        let mut path = PathBuf::new();
        for component in display.trim_end_matches(['/', '\\']).split(['/', '\\']) {
            if !component.is_empty() {
                path.push(component);
            }
        }
        if path.as_os_str().is_empty() {
            return Err(ArchiveError::UnsafeEntryPath(display.to_owned()));
        }
        Ok(path)
    }

    fn has_parallel_path_conflict(items: &[ExtractItem], selected: &HashSet<u32>) -> bool {
        let mut paths = items
            .iter()
            .filter(|item| selected.contains(&item.index))
            .map(|item| {
                (
                    item.relative
                        .to_string_lossy()
                        .replace('\\', "/")
                        .to_ascii_lowercase(),
                    item.is_dir,
                )
            })
            .collect::<Vec<_>>();
        paths.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        for window in paths.windows(2) {
            let (previous, previous_is_dir) = &window[0];
            let (current, _) = &window[1];
            if previous == current
                || (!previous_is_dir
                    && current.starts_with(previous)
                    && current.as_bytes().get(previous.len()) == Some(&b'/'))
            {
                return true;
            }
        }
        false
    }

    fn apply_create_properties(
        out_archive: &RawOutArchive,
        options: &CreateOptions,
    ) -> ArchiveResult<()> {
        let raw = out_archive
            .query_interface(&IID_ISET_PROPERTIES)
            .ok_or_else(|| {
                ArchiveError::SevenZip("the 7z handler does not expose ISetProperties".to_owned())
            })?;
        // SAFETY: QueryInterface returned a live ISetProperties interface.
        let set_properties = unsafe { RawSetProperties::from_raw(raw) };

        let mut names: Vec<Vec<u16>> = Vec::with_capacity(6);
        let mut name_ptrs: Vec<*const u16> = Vec::with_capacity(6);
        let mut values: Vec<PropVariant> = Vec::with_capacity(6);
        let mut push = |name: &str, value: PropVariant| {
            let mut wide: Vec<u16> = name.encode_utf16().collect();
            wide.push(0);
            name_ptrs.push(wide.as_ptr());
            names.push(wide);
            values.push(value);
        };
        if options.compression_level > 9 {
            return Err(ArchiveError::UnsupportedOption(format!(
                "compression level {} is outside 0..=9",
                options.compression_level
            )));
        }
        // Compression level. Level 0 stores without compression, exactly like
        // 7-Zip's -mx0. The ZIP handler selects Store/Deflate from `x` when no
        // explicit `m` method is supplied.
        push(
            "x",
            PropVariant::u32_value(u32::from(options.compression_level)),
        );
        if options.format == CreateFormat::SevenZip {
            push(
                "m",
                PropVariant::bstr(if options.compression_level == 0 {
                    "Copy"
                } else {
                    "LZMA2"
                }),
            );
        }
        if options.format == CreateFormat::Zip {
            // Force the standard UTF-8 filename flag in ZIP headers.
            push("cu", PropVariant::bool_value(true));
        }
        // `mt` is the worker-thread count. Auto/All are resolved by
        // ThreadCount to the process-visible logical CPU count, matching
        // 7-Zip CLI `-mmt=on` instead of relying on handler defaults.
        if let Some(threads) = options.threads.sevenzip_threads() {
            push("mt", PropVariant::u32_value(threads));
        }
        if options.format == CreateFormat::Zip && options.password.is_some() {
            // Match the existing ZIP backend's AES-256 password behavior.
            push("em", PropVariant::bstr("AES256"));
        }
        // Passwords are requested through ICryptoGetTextPassword2 on the
        // update callback. The 7z handler does not accept a `p` property in
        // ISetProperties and returns E_INVALIDARG for it.
        // The closure borrows the arrays; end that borrow before reading them.
        drop(push);
        let hr =
            set_properties.set_properties(name_ptrs.as_ptr(), values.as_ptr(), values.len() as u32);
        // 7-Zip copied the property values during SetProperties; the BSTRs
        // (and the wide name buffers, which 7-Zip only borrows) are released
        // here.
        for value in &mut values {
            value.clear();
        }
        drop(set_properties);
        if hr != S_OK {
            return Err(ArchiveError::SevenZip(format!(
                "setting 7z options failed with HRESULT {:#010x}",
                hr as u32
            )));
        }
        Ok(())
    }

    fn require_hr(hr: i32, operation: &'static str) -> ArchiveResult<()> {
        if hr == S_OK {
            Ok(())
        } else {
            Err(ArchiveError::SevenZip(format!(
                "{operation} failed with HRESULT {:#010x}",
                hr as u32
            )))
        }
    }

    fn check_cancel(cancel: &CancellationToken) -> ArchiveResult<()> {
        if cancel.is_cancelled() {
            Err(ArchiveError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn checked_add_with_limit(
        total: u64,
        add: u64,
        limit: u64,
        subject: &str,
    ) -> ArchiveResult<u64> {
        let next = total
            .checked_add(add)
            .ok_or_else(|| ArchiveError::LimitExceeded(format!("{subject} overflow")))?;
        if next > limit {
            return Err(ArchiveError::LimitExceeded(format!(
                "{subject} exceeds the configured limit"
            )));
        }
        Ok(next)
    }

    fn file_length(path: &Path) -> ArchiveResult<u64> {
        fs::metadata(path)
            .map(|metadata| metadata.len())
            .map_err(|error| ArchiveError::io(path, error))
    }

    fn is_reparse(metadata: &fs::Metadata) -> bool {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    fn same_windows_path(left: &Path, right: &Path) -> bool {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }

    fn stream_buffer_size(file_size: u64) -> usize {
        usize::try_from(file_size.min(STREAM_BUFFER_SIZE as u64))
            .unwrap_or(STREAM_BUFFER_SIZE)
            .max(MIN_STREAM_BUFFER_SIZE)
    }

    fn temporary_path(parent: &Path, target: &Path) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let name = target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("output");
        parent.join(format!(
            ".{name}.archive-rclick-{}-{nonce}.tmp",
            std::process::id()
        ))
    }

    /// Moves `temp` onto `target`, replacing an existing file when present.
    fn install_temporary(root: &Path, temp: &Path, target: &Path) -> ArchiveResult<()> {
        ensure_no_reparse_ancestors(root, target)?;
        match fs::rename(temp, target) {
            Ok(()) => Ok(()),
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists
                    || fs::symlink_metadata(target).is_ok() =>
            {
                fs::remove_file(target).map_err(|error| ArchiveError::io(target, error))?;
                fs::rename(temp, target).map_err(|error| ArchiveError::io(target, error))
            }
            Err(error) => Err(ArchiveError::io(target, error)),
        }
    }

    fn create_error(error: ArchiveError) -> ArchiveResult<OperationSummary> {
        Err(error)
    }

    /// Removes temp files of entries whose SetOperationResult never arrived
    /// (cancelled or failed extraction).
    fn cleanup_pending_temp_files(callback: &ExtractCallback) {
        let mut context = callback
            .context
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while let Some(_pending) = context.pending.pop_front() {}
    }

    fn close_pending_file(pending: &PendingFile) {
        let mut guard = pending
            .file
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        drop(guard.take());
    }

    // ------------------------------------------------------------------
    // Loading
    // ------------------------------------------------------------------
    fn load_7z_library() -> ArchiveResult<DynamicLibrary> {
        if let Some(configured) = env::var_os("ARCHIVERCLICK_7ZDLL") {
            let path = PathBuf::from(configured);
            if !path.is_absolute() {
                return Err(ArchiveError::LibraryUnavailable(
                    "ARCHIVERCLICK_7ZDLL must be an absolute path".to_owned(),
                ));
            }
            return DynamicLibrary::load(&path).map_err(ArchiveError::LibraryUnavailable);
        }

        let mut candidates: Vec<PathBuf> = Vec::with_capacity(8);
        if let Ok(executable) = env::current_exe()
            && let Some(directory) = executable.parent()
        {
            // 1. Next to the executable (the packaged layout).
            candidates.push(directory.join("7z.dll"));
            // 2. <exe>/runtime/x64 (the repository layout, dev runs).
            candidates.push(directory.join("runtime").join("x64").join("7z.dll"));
            // 3. Walk up a few levels looking for <root>/runtime/x64/7z.dll
            //    (cargo target/debug -> repository root).
            let mut current = directory.parent();
            for _ in 0..3 {
                if let Some(parent) = current {
                    candidates.push(parent.join("runtime").join("x64").join("7z.dll"));
                    current = parent.parent();
                } else {
                    break;
                }
            }
            // 4. Cargo profile directory (deps/examples test binaries).
            let is_cargo_subdirectory = directory
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.eq_ignore_ascii_case("deps") || name.eq_ignore_ascii_case("examples")
                });
            if is_cargo_subdirectory && let Some(profile_directory) = directory.parent() {
                candidates.push(profile_directory.join("7z.dll"));
            }
        }

        let mut failures = Vec::new();
        for path in candidates {
            match DynamicLibrary::load(&path) {
                Ok(library) => {
                    if library.symbol(b"CreateObject\0").is_some() {
                        return Ok(library);
                    }
                    failures.push(format!("{}: missing CreateObject export", path.display()));
                }
                Err(error) => failures.push(format!("{}: {error}", path.display())),
            }
        }
        Err(ArchiveError::LibraryUnavailable(format!(
            "could not load the bundled 7z.dll; searched: {}",
            if failures.is_empty() {
                "(no candidates)".to_owned()
            } else {
                failures.join("; ")
            }
        )))
    }

    // ------------------------------------------------------------------
    // Source collection for creation
    // ------------------------------------------------------------------
    fn collect_sources(
        files: &[PathBuf],
        destination: &Path,
        cancel: &CancellationToken,
    ) -> ArchiveResult<(Vec<SourceItem>, u64)> {
        let mut items = Vec::new();
        let mut total_bytes = 0u64;
        for root in files {
            check_cancel(cancel)?;
            let canonical =
                fs::canonicalize(root).map_err(|error| ArchiveError::io(root, error))?;
            if same_windows_path(&canonical, destination) {
                continue;
            }
            let Some(base_name) = root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                return Err(ArchiveError::InvalidInput(format!(
                    "input has no file name: {}",
                    root.display()
                )));
            };
            let metadata =
                fs::symlink_metadata(root).map_err(|error| ArchiveError::io(root, error))?;
            // Skip reparse-point inputs (symlinks/junctions).
            if is_reparse(&metadata) {
                continue;
            }
            if metadata.is_dir() {
                walk_directory(
                    &canonical,
                    &base_name,
                    destination,
                    &mut items,
                    &mut total_bytes,
                    cancel,
                )?;
            } else {
                total_bytes = checked_add_with_limit(
                    total_bytes,
                    metadata.len(),
                    MAX_LIST_DECLARED_BYTES,
                    "7z creation input size",
                )?;
                items.push(SourceItem {
                    source: root.clone(),
                    archive_name: base_name,
                    kind: SourceKind::File,
                    size: metadata.len(),
                    modified_unix_seconds: metadata_modified_seconds(&metadata),
                });
            }
        }
        // Deterministic order regardless of filesystem enumeration order.
        items.sort_by(|left, right| left.archive_name.cmp(&right.archive_name));
        Ok((items, total_bytes))
    }

    fn walk_directory(
        canonical_root: &Path,
        prefix: &str,
        destination: &Path,
        items: &mut Vec<SourceItem>,
        total_bytes: &mut u64,
        cancel: &CancellationToken,
    ) -> ArchiveResult<()> {
        let mut pending = vec![(prefix.to_owned(), canonical_root.to_path_buf())];
        while let Some((archive_prefix, directory)) = pending.pop() {
            check_cancel(cancel)?;
            let mut children: Vec<(String, PathBuf, fs::Metadata)> = Vec::new();
            let entries =
                fs::read_dir(&directory).map_err(|error| ArchiveError::io(&directory, error))?;
            for entry in entries {
                let entry = entry.map_err(|error| ArchiveError::io(&directory, error))?;
                let path = entry.path();
                let metadata =
                    fs::symlink_metadata(&path).map_err(|error| ArchiveError::io(&path, error))?;
                // Skip reparse points (symlinks/junctions): archiving them
                // through 7z.dll would dereference them on extraction.
                if is_reparse(&metadata) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                children.push((name, path, metadata));
            }
            children.sort_by(|left, right| left.0.cmp(&right.0));
            items.push(SourceItem {
                source: directory.clone(),
                // The archive handler adds the directory separator based on
                // KPID_IS_DIR. Keeping it out of the callback property also
                // matches the path form used for child entries.
                archive_name: archive_prefix.clone(),
                kind: SourceKind::Directory,
                size: 0,
                modified_unix_seconds: None,
            });
            for (name, path, metadata) in children {
                check_cancel(cancel)?;
                if same_windows_path(&path, destination) {
                    continue;
                }
                let child_archive_name = format!("{archive_prefix}/{name}");
                if metadata.is_dir() {
                    pending.push((child_archive_name, path));
                } else {
                    *total_bytes = checked_add_with_limit(
                        *total_bytes,
                        metadata.len(),
                        MAX_LIST_DECLARED_BYTES,
                        "7z creation input size",
                    )?;
                    items.push(SourceItem {
                        source: path,
                        archive_name: child_archive_name,
                        kind: SourceKind::File,
                        size: metadata.len(),
                        modified_unix_seconds: metadata_modified_seconds(&metadata),
                    });
                }
            }
        }
        Ok(())
    }

    fn metadata_modified_seconds(metadata: &fs::Metadata) -> Option<i64> {
        use std::os::windows::fs::MetadataExt;
        let filetime = i64::try_from(metadata.last_write_time()).ok()?;
        Some((filetime / 10_000_000).saturating_sub(FILETIME_EPOCH_SECONDS))
    }

    #[cfg(test)]
    mod tests {
        use super::{
            ArchiveEngine, CompositeEngine, CreateFormat, CreateOptions, SevenZipEngine,
            archive_format, filetime_to_unix_seconds, split_callback_progress,
            unix_seconds_to_filetime,
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
            assert_eq!(archive_format(&sevenzip), Some(CreateFormat::SevenZip));
            assert_eq!(archive_format(&zip), Some(CreateFormat::Zip));
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
                let engine =
                    SevenZipEngine::load_from_path(&candidate).expect("bundled 7z.dll loads");
                assert!(engine.can_read(CreateFormat::Zip));
                assert_eq!(
                    engine.writable_formats(),
                    vec![CreateFormat::Zip, CreateFormat::SevenZip]
                );
                return;
            }
            eprintln!("bundled 7z.dll was not staged for tests; skipping");
        }
    }
}

#[cfg(not(windows))]
mod platform_impl {
    use std::path::{Path, PathBuf};

    use crate::tasks::{CancellationToken, ProgressSnapshot};

    use super::super::libarchive::LibArchiveEngine;
    use super::super::{
        ArchiveEngine, ArchiveError, ArchiveListing, ArchiveResult, ConflictResolver,
        CreateOptions, ExtractOptions, OperationSummary, ProgressSink,
    };

    #[derive(Clone, Default)]
    pub struct SevenZipEngine;

    impl SevenZipEngine {
        pub fn load() -> ArchiveResult<Self> {
            Err(unavailable())
        }

        pub fn load_from_path(_path: &Path) -> ArchiveResult<Self> {
            Err(unavailable())
        }
    }

    fn unavailable() -> ArchiveError {
        ArchiveError::LibraryUnavailable(
            "the dynamic 7z.dll backend is currently Windows-only".to_owned(),
        )
    }

    impl ArchiveEngine for SevenZipEngine {
        fn version(&self) -> String {
            "7z.dll unavailable".to_owned()
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

    pub struct CompositeEngine {
        libarchive: LibArchiveEngine,
        _sevenzip: Option<SevenZipEngine>,
    }

    impl CompositeEngine {
        pub fn new(libarchive: LibArchiveEngine, sevenzip: Option<SevenZipEngine>) -> Self {
            Self {
                libarchive,
                _sevenzip: sevenzip,
            }
        }
    }

    impl ArchiveEngine for CompositeEngine {
        fn version(&self) -> String {
            self.libarchive.version()
        }

        fn writable_formats(&self) -> Vec<super::super::CreateFormat> {
            self.libarchive.writable_formats()
        }

        fn list(
            &self,
            path: &Path,
            password: Option<&str>,
            pathname_codepage: u32,
            progress: &dyn ProgressSink,
            cancel: &CancellationToken,
        ) -> ArchiveResult<ArchiveListing> {
            self.libarchive
                .list(path, password, pathname_codepage, progress, cancel)
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
            self.libarchive
                .extract(archive, destination, options, progress, conflicts, cancel)
        }

        fn create(
            &self,
            destination: &Path,
            files: &[PathBuf],
            options: &CreateOptions,
            progress: &dyn ProgressSink,
            cancel: &CancellationToken,
        ) -> ArchiveResult<OperationSummary> {
            self.libarchive
                .create(destination, files, options, progress, cancel)
        }

        fn test(
            &self,
            archive: &Path,
            password: Option<&str>,
            progress: &dyn ProgressSink,
            cancel: &CancellationToken,
        ) -> ArchiveResult<OperationSummary> {
            self.libarchive.test(archive, password, progress, cancel)
        }
    }

    #[allow(dead_code)]
    fn _assert_progress_type(_: ProgressSnapshot) {}
}

pub use platform_impl::{CompositeEngine, SevenZipEngine};
