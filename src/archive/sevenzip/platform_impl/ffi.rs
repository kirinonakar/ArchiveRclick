//! 7-Zip SDK ABI types, COM wrappers, and dynamic-library discovery.

use super::*;

// ------------------------------------------------------------------
// HRESULT values
// ------------------------------------------------------------------
pub(super) const S_OK: i32 = 0;
pub(super) const S_FALSE: i32 = 1;
pub(super) const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
pub(super) const E_ABORT: i32 = 0x8000_4004u32 as i32;
pub(super) const E_FAIL: i32 = 0x8000_4005u32 as i32;
pub(super) const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;

// 7-Zip PROPIDs (PropID.h).
pub(super) const KPID_PATH: u32 = 3;
pub(super) const KPID_IS_DIR: u32 = 6;
pub(super) const KPID_SIZE: u32 = 7;
pub(super) const KPID_PACK_SIZE: u32 = 8;
pub(super) const KPID_MTIME: u32 = 12;
pub(super) const KPID_ENCRYPTED: u32 = 15;

// PROPVARIANT types.
pub(super) const VT_EMPTY: u16 = 0;
pub(super) const VT_BSTR: u16 = 8;
pub(super) const VT_BOOL: u16 = 11;
pub(super) const VT_UI4: u16 = 19;
pub(super) const VT_UI8: u16 = 21;
pub(super) const VT_FILETIME: u16 = 64;

// Stream seek origins.
pub(super) const SEEK_SET: u32 = 0;
pub(super) const SEEK_CUR: u32 = 1;
pub(super) const SEEK_END: u32 = 2;

// IArchiveExtractCallback askExtractMode values (AskMode.h).
pub(super) const EXTRACT_MODE_EXTRACT: i32 = 0;
pub(super) const EXTRACT_MODE_TEST: i32 = 1;
pub(super) const EXTRACT_MODE_SKIP: i32 = 2;

// IArchiveExtractCallback / IArchiveUpdateCallback operation results.
pub(super) const OPERATION_RESULT_OK: i32 = 0;
pub(super) const OPERATION_RESULT_UNSUPPORTED_METHOD: i32 = 1;
pub(super) const OPERATION_RESULT_DATA_ERROR: i32 = 2;
pub(super) const OPERATION_RESULT_CRC_ERROR: i32 = 3;

// ------------------------------------------------------------------
// GUIDs (7-Zip SDK: IArchive.h, ICoder.h, IPassword.h, ArchiveExports.cpp)
// ------------------------------------------------------------------
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

pub(super) const fn guid(
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
pub(super) const IID_IUNKNOWN: Guid = guid(0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46);
pub(super) const IID_ISEQUENTIAL_IN_STREAM: Guid =
    guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 3, 0, 0x01, 0, 0);
pub(super) const IID_IIN_STREAM: Guid =
    guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 3, 0, 0x03, 0, 0);
pub(super) const IID_ISEQUENTIAL_OUT_STREAM: Guid =
    guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 3, 0, 0x02, 0, 0);
pub(super) const IID_IOUT_STREAM: Guid =
    guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 3, 0, 0x04, 0, 0);
pub(super) const IID_IPROGRESS: Guid = guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 0, 0, 0x05, 0, 0);
pub(super) const IID_IARCHIVE_EXTRACT_CALLBACK: Guid =
    guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 6, 0, 0x20, 0, 0);
pub(super) const IID_IIN_ARCHIVE: Guid =
    guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 6, 0, 0x60, 0, 0);
pub(super) const IID_IARCHIVE_OPEN_CALLBACK: Guid =
    guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 6, 0, 0x10, 0, 0);
pub(super) const IID_ISET_PROPERTIES: Guid =
    guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 6, 0, 0x03, 0, 0);
pub(super) const IID_IOUT_ARCHIVE: Guid =
    guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 6, 0, 0xA0, 0, 0);
pub(super) const IID_IARCHIVE_UPDATE_CALLBACK: Guid =
    guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 6, 0, 0x80, 0, 0);
pub(super) const IID_ICRYPTO_GET_TEXT_PASSWORD: Guid =
    guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 5, 0, 0x10, 0, 0);
pub(super) const IID_ICRYPTO_GET_TEXT_PASSWORD2: Guid =
    guid(0x2317_0F69, 0x40C1, 0x278A, 0, 0, 0, 5, 0, 0x11, 0, 0);

// CLSID_CFormat7z keeps the classic layout: {23170F69-40C1-278A-1000-
// 000110070000} with the format id (7) in Data4[5]; CreateArchiver zeroes
// Data4[5] and compares the rest against CLSID_CArchiveHandler.
pub(super) const CLSID_C_FORMAT_7Z: Guid = guid(
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
pub(super) const CLSID_C_FORMAT_ZIP: Guid = guid(
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

// The 7-Zip archive-handler registry assigns format id 6 to LZH/LHA.
// Api::from_library still discovers the class id from the DLL so this
// fallback only serves reduced or older builds without the enumeration
// exports.
pub(super) const CLSID_C_FORMAT_LZH: Guid = guid(
    0x2317_0F69,
    0x40C1,
    0x278A,
    0x10,
    0,
    0,
    0x01,
    0x10,
    0x06,
    0,
    0,
);

// 7z file signature: "7z\xBC\xAF\x27\x1C".
pub(super) const SEVENZIP_SIGNATURE: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];

// ------------------------------------------------------------------
// Dynamic library loading
// ------------------------------------------------------------------
pub(super) struct DynamicLibrary {
    handle: HMODULE,
}

// A loaded module can be queried and freed from any thread. The handle is
// kept alive by Arc<Api> for at least as long as every copied function ptr.
unsafe impl Send for DynamicLibrary {}
unsafe impl Sync for DynamicLibrary {}

impl DynamicLibrary {
    pub(super) fn load(path: &Path) -> Result<Self, String> {
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

    pub(super) fn symbol(&self, name: &'static [u8]) -> Option<usize> {
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
        // SAFETY: the symbol name and signature come from the 7-Zip SDK.
        unsafe { mem::transmute::<usize, $ty>(address) }
    }};
}

// ------------------------------------------------------------------
// PROPVARIANT and BSTR helpers
// ------------------------------------------------------------------
#[link(name = "OleAut32")]
unsafe extern "system" {
    pub(super) fn SysAllocString(value: *const u16) -> *mut u16;
    pub(super) fn SysFreeString(value: *mut u16);
    pub(super) fn SysStringByteLen(value: *const u16) -> u32;
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct PropVariant {
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
    pub(super) fn empty() -> Self {
        Self {
            vt: VT_EMPTY,
            w_reserved1: 0,
            w_reserved2: 0,
            w_reserved3: 0,
            payload: 0,
            payload2: 0,
        }
    }

    pub(super) fn u32_value(value: u32) -> Self {
        Self {
            vt: VT_UI4,
            payload: u64::from(value),
            ..Self::empty()
        }
    }

    pub(super) fn u64_value(value: u64) -> Self {
        Self {
            vt: VT_UI8,
            payload: value,
            ..Self::empty()
        }
    }

    pub(super) fn bool_value(value: bool) -> Self {
        Self {
            vt: VT_BOOL,
            // VARIANT_TRUE is -1.
            payload: if value { 0xFFFF } else { 0 },
            ..Self::empty()
        }
    }

    pub(super) fn bstr(value: &str) -> Self {
        Self {
            vt: VT_BSTR,
            payload: sys_alloc_string(value) as u64,
            ..Self::empty()
        }
    }

    pub(super) fn filetime(seconds: i64) -> Self {
        Self {
            vt: VT_FILETIME,
            payload: unix_seconds_to_filetime(seconds),
            ..Self::empty()
        }
    }

    pub(super) fn as_u64(&self) -> Option<u64> {
        match self.vt {
            VT_UI8 => Some(self.payload),
            VT_UI4 => Some(u64::from(self.payload as u32)),
            _ => None,
        }
    }

    pub(super) fn as_bool(&self) -> Option<bool> {
        (self.vt == VT_BOOL).then(|| self.payload != 0)
    }

    pub(super) fn as_bstr(&self) -> Option<String> {
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

    pub(super) fn as_guid(&self) -> Option<Guid> {
        if self.vt != VT_BSTR || self.payload == 0 {
            return None;
        }
        // GetHandlerProperty2 returns the class ID as a binary BSTR, so
        // it may contain embedded NULs and cannot use `as_bstr`.
        let length = unsafe { SysStringByteLen(self.payload as *const u16) } as usize;
        (length >= mem::size_of::<Guid>())
            .then(|| unsafe { ptr::read_unaligned(self.payload as *const Guid) })
    }

    pub(super) fn as_filetime_seconds(&self) -> Option<i64> {
        (self.vt == VT_FILETIME).then(|| filetime_to_unix_seconds(self.payload))
    }

    /// Releases strings owned by this variant. Must be called for every
    /// variant received from 7-Zip (GetProperty) and for every variant we
    /// handed to 7-Zip after SetProperties returned.
    pub(super) fn clear(&mut self) {
        if self.vt == VT_BSTR && self.payload != 0 {
            // SAFETY: the variant owns a SysAllocString allocation.
            unsafe { SysFreeString(self.payload as *mut u16) };
        }
        *self = Self::empty();
    }
}

pub(super) fn sys_alloc_string(value: &str) -> *mut u16 {
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
pub(super) fn write_password_bstr(password: *mut *mut u16, value: Option<&str>) -> i32 {
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

pub(super) fn unix_seconds_to_filetime(seconds: i64) -> u64 {
    seconds
        .saturating_add(FILETIME_EPOCH_SECONDS)
        .saturating_mul(10_000_000) as u64
}

pub(super) fn filetime_to_unix_seconds(value: u64) -> i64 {
    (value / 10_000_000)
        .try_into()
        .unwrap_or(i64::MAX)
        .saturating_sub(FILETIME_EPOCH_SECONDS)
}

// ------------------------------------------------------------------
// Raw vtables of the 7-Zip handler objects (their vtables live in 7z.dll)
// ------------------------------------------------------------------
pub(super) type QueryInterfaceFn =
    unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32;
pub(super) type AddRefFn = unsafe extern "system" fn(*mut c_void) -> u32;
pub(super) type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;

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
pub(super) struct RawInArchive {
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
    pub(super) unsafe fn from_raw(pointer: *mut c_void) -> Self {
        Self {
            object: pointer,
            vtbl: *pointer.cast::<*const InArchiveVtbl>(),
            closed: Cell::new(false),
        }
    }

    pub(super) fn ptr(&self) -> *mut c_void {
        self.object
    }

    pub(super) fn get_number_of_items(&self, count: *mut u32) -> i32 {
        // SAFETY: the object is live; `count` is a valid out-parameter.
        unsafe { ((*self.vtbl).get_number_of_items)(self.ptr(), count) }
    }

    pub(super) fn get_property(&self, index: u32, prop_id: u32, value: *mut PropVariant) -> i32 {
        // SAFETY: the object is live; `value` is a valid out-parameter.
        unsafe { ((*self.vtbl).get_property)(self.ptr(), index, prop_id, value) }
    }

    pub(super) fn get_archive_property(&self, prop_id: u32, value: *mut PropVariant) -> i32 {
        // SAFETY: the object is live; `value` is a valid out-parameter.
        unsafe { ((*self.vtbl).get_archive_property)(self.ptr(), prop_id, value) }
    }

    pub(super) fn extract(
        &self,
        indices: *const u32,
        num_items: u32,
        test_mode: i32,
        callback: *mut c_void,
    ) -> i32 {
        // SAFETY: all pointers stay live for the call.
        unsafe { ((*self.vtbl).extract)(self.ptr(), indices, num_items, test_mode, callback) }
    }

    pub(super) fn open(&self, stream: *mut c_void, open_callback: *mut c_void) -> i32 {
        // SAFETY: the stream and callback stay live for the call.
        unsafe { ((*self.vtbl).open)(self.ptr(), stream, ptr::null(), open_callback) }
    }

    /// Closes the archive explicitly so the handler releases its input
    /// stream while the stream wrapper is still alive. Idempotent; the
    /// destructor still releases the interface reference.
    pub(super) fn close_now(&self) {
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
pub(super) struct RawOutArchive {
    object: *mut c_void,
    vtbl: *const OutArchiveVtbl,
}

impl RawOutArchive {
    /// # Safety
    /// `pointer` must come from `CreateObject(CLSID_CFormat7z or
    /// CLSID_CFormatZip, IID_IOutArchive)`.
    pub(super) unsafe fn from_raw(pointer: *mut c_void) -> Self {
        Self {
            object: pointer,
            vtbl: *pointer.cast::<*const OutArchiveVtbl>(),
        }
    }

    pub(super) fn ptr(&self) -> *mut c_void {
        self.object
    }

    pub(super) fn query_interface(&self, iid: &Guid) -> Option<*mut c_void> {
        let mut out: *mut c_void = ptr::null_mut();
        // SAFETY: the object and iid are live for the call.
        let hr = unsafe { ((*self.vtbl).query_interface)(self.ptr(), iid, &mut out) };
        (hr == S_OK).then_some(out)
    }

    pub(super) fn update_items(
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
    set_properties:
        unsafe extern "system" fn(*mut c_void, *const *const u16, *const PropVariant, u32) -> i32,
}

#[repr(C)]
pub(super) struct RawSetProperties {
    object: *mut c_void,
    vtbl: *const SetPropertiesVtbl,
}

impl RawSetProperties {
    /// # Safety
    /// `pointer` must come from a successful QueryInterface for
    /// `IID_ISetProperties` on a 7z handler object.
    pub(super) unsafe fn from_raw(pointer: *mut c_void) -> Self {
        Self {
            object: pointer,
            vtbl: *pointer.cast::<*const SetPropertiesVtbl>(),
        }
    }

    pub(super) fn ptr(&self) -> *mut c_void {
        self.object
    }

    pub(super) fn set_properties(
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
// API entry point
// ------------------------------------------------------------------
pub(super) type CreateObjectFn =
    unsafe extern "system" fn(*const Guid, *const Guid, *mut *mut c_void) -> i32;
pub(super) type GetNumberOfFormatsFn = unsafe extern "system" fn(*mut u32) -> i32;
pub(super) type GetHandlerProperty2Fn =
    unsafe extern "system" fn(u32, u32, *mut PropVariant) -> i32;

pub(super) const HANDLER_PROPERTY_NAME: u32 = 0;
pub(super) const HANDLER_PROPERTY_CLASS_ID: u32 = 1;

pub(super) struct Api {
    _library: DynamicLibrary,
    create_object: CreateObjectFn,
    zip_clsid: Guid,
    lzh_clsid: Option<Guid>,
    rar4_clsid: Option<Guid>,
    rar5_clsid: Option<Guid>,
    iso_clsid: Option<Guid>,
    nsis_clsid: Option<Guid>,
    zip_reader_available: bool,
    zip_writer_available: bool,
}

pub(super) fn discover_handler_clsid(
    library: &DynamicLibrary,
    expected_name: &str,
) -> Option<Guid> {
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
    pub(super) fn can_create(&self, format: CreateFormat) -> bool {
        match format {
            CreateFormat::SevenZip => true,
            CreateFormat::Zip => self.zip_writer_available,
            _ => false,
        }
    }

    pub(super) fn from_library(library: DynamicLibrary) -> ArchiveResult<Self> {
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
        // LZH is read-only in 7-Zip.  Prefer the advertised class id,
        // with the stable format-id fallback for reduced builds whose
        // enumeration exports are unavailable.
        let lzh_clsid = discover_handler_clsid(&library, "lzh").unwrap_or(CLSID_C_FORMAT_LZH);
        let mut lzh_reader_probe: *mut c_void = ptr::null_mut();
        let lzh_reader_hr =
            unsafe { create_object(&lzh_clsid, &IID_IIN_ARCHIVE, &mut lzh_reader_probe) };
        let lzh_clsid = if lzh_reader_hr == S_OK && !lzh_reader_probe.is_null() {
            // SAFETY: the probe is a live IInArchive interface returned
            // by the DLL and is released exactly once.
            let lzh_vtbl = unsafe { *lzh_reader_probe.cast::<*const InArchiveVtbl>() };
            unsafe { ((*lzh_vtbl).release)(lzh_reader_probe) };
            Some(lzh_clsid)
        } else {
            None
        };
        // ISO reading is optional for reduced development DLLs.  Resolve
        // the handler by name because 7-Zip does not guarantee handler
        // ordering or class IDs across builds.
        let iso_clsid = discover_handler_clsid(&library, "iso").and_then(|iso_clsid| {
            let mut iso_probe: *mut c_void = ptr::null_mut();
            let iso_hr = unsafe { create_object(&iso_clsid, &IID_IIN_ARCHIVE, &mut iso_probe) };
            if iso_hr != S_OK || iso_probe.is_null() {
                return None;
            }
            // SAFETY: the probe is a live IInArchive interface returned
            // by the DLL and is released exactly once.
            let iso_vtbl = unsafe { *iso_probe.cast::<*const InArchiveVtbl>() };
            unsafe { ((*iso_vtbl).release)(iso_probe) };
            Some(iso_clsid)
        });
        // RAR reading is optional for reduced development DLLs. Full
        // 7-Zip builds expose distinct RAR4 ("Rar") and RAR5 ("Rar5")
        // handlers, so selecting only by the .rar extension is not enough.
        let probe_optional_reader = |handler_name: &str| {
            let rar_clsid = discover_handler_clsid(&library, handler_name)?;
            let mut rar_probe: *mut c_void = ptr::null_mut();
            let rar_hr = unsafe { create_object(&rar_clsid, &IID_IIN_ARCHIVE, &mut rar_probe) };
            if rar_hr != S_OK || rar_probe.is_null() {
                return None;
            }
            // SAFETY: the probe is a live IInArchive interface returned
            // by the DLL and is released exactly once.
            let rar_vtbl = unsafe { *rar_probe.cast::<*const InArchiveVtbl>() };
            unsafe { ((*rar_vtbl).release)(rar_probe) };
            Some(rar_clsid)
        };
        let rar4_clsid = probe_optional_reader("rar");
        let rar5_clsid = probe_optional_reader("rar5");
        let nsis_clsid = probe_optional_reader("Nsis");
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
            lzh_clsid,
            rar4_clsid,
            rar5_clsid,
            iso_clsid,
            nsis_clsid,
            zip_reader_available,
            zip_writer_available,
        })
    }

    pub(super) fn create_in_archive(&self, format: ReadFormat) -> ArchiveResult<RawInArchive> {
        let format = format.base();
        let clsid = match format {
            ReadFormat::SevenZip => &CLSID_C_FORMAT_7Z,
            ReadFormat::Zip => &self.zip_clsid,
            ReadFormat::Lzh => self.lzh_clsid.as_ref().ok_or_else(|| {
                ArchiveError::UnsupportedOption(
                    "the loaded 7z.dll does not provide an LZH reader".to_owned(),
                )
            })?,
            ReadFormat::Rar4 => self.rar4_clsid.as_ref().ok_or_else(|| {
                ArchiveError::UnsupportedOption(
                    "the loaded 7z.dll does not provide a RAR4 reader".to_owned(),
                )
            })?,
            ReadFormat::Rar5 => self.rar5_clsid.as_ref().ok_or_else(|| {
                ArchiveError::UnsupportedOption(
                    "the loaded 7z.dll does not provide a RAR5 reader".to_owned(),
                )
            })?,
            ReadFormat::Iso => self.iso_clsid.as_ref().ok_or_else(|| {
                ArchiveError::UnsupportedOption(
                    "the loaded 7z.dll does not provide an ISO reader".to_owned(),
                )
            })?,
            ReadFormat::Nsis => self.nsis_clsid.as_ref().ok_or_else(|| {
                ArchiveError::UnsupportedOption(
                    "the loaded 7z.dll does not provide an NSIS reader".to_owned(),
                )
            })?,
            ReadFormat::SevenZipVolume | ReadFormat::ZipVolume => unreachable!(),
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

    pub(super) fn create_out_archive(&self, format: CreateFormat) -> ArchiveResult<RawOutArchive> {
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

    pub(super) fn can_read(&self, format: ReadFormat) -> bool {
        match format.base() {
            ReadFormat::SevenZip => true,
            ReadFormat::Zip => self.zip_reader_available,
            ReadFormat::Lzh => self.lzh_clsid.is_some(),
            ReadFormat::Rar4 => self.rar4_clsid.is_some(),
            ReadFormat::Rar5 => self.rar5_clsid.is_some(),
            ReadFormat::Iso => self.iso_clsid.is_some(),
            ReadFormat::Nsis => self.nsis_clsid.is_some(),
            ReadFormat::SevenZipVolume | ReadFormat::ZipVolume => unreachable!(),
        }
    }
}

// ------------------------------------------------------------------

// ------------------------------------------------------------------
// Loading
// ------------------------------------------------------------------
pub(super) fn load_7z_library() -> ArchiveResult<DynamicLibrary> {
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
