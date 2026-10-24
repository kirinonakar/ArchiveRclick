use super::{DragFile, PrepareFiles};
use crate::tasks::CancellationToken;
use ::windows::{
    Win32::{
        Foundation::{
            DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC,
            DV_E_LINDEX, E_ABORT, E_NOTIMPL, E_POINTER, GlobalFree, HGLOBAL, POINT,
        },
        Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL},
        System::{
            Com::{
                COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize, DATADIR_GET,
                DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl,
                IEnumFORMATETC, IEnumFORMATETC_Impl, IEnumSTATDATA, IStream,
                Marshal::{CoMarshalInterThreadInterfaceInStream, CoReleaseMarshalData},
                STGMEDIUM, STGMEDIUM_0,
                StructuredStorage::CoGetInterfaceAndReleaseStream,
                TYMED_HGLOBAL, TYMED_ISTREAM,
            },
            DataExchange::RegisterClipboardFormatW,
            Memory::{
                GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
            },
            Ole::{
                DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop, IDropSource,
                IDropSource_Impl, OleInitialize, OleUninitialize, ReleaseStgMedium,
            },
            SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS},
            Threading::{AttachThreadInput, GetCurrentThreadId},
        },
        UI::{
            Input::KeyboardAndMouse::{
                GetAsyncKeyState, GetKeyboardState, ReleaseCapture, SetKeyboardState, VK_ESCAPE,
                VK_LBUTTON, VK_RBUTTON,
            },
            Shell::{
                CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, CFSTR_ZONEIDENTIFIER, FD_ATTRIBUTES,
                FD_FILESIZE, FD_UNICODE, FD_WRITESTIME, FILEDESCRIPTORW,
            },
            WindowsAndMessaging::{
                GetCursorPos, GetSystemMetrics, GetWindowThreadProcessId, SM_SWAPBUTTON,
                WindowFromPoint,
            },
        },
    },
    core::{BOOL, Error, HRESULT, Interface, Ref, implement},
};
use std::{
    collections::HashMap,
    mem::ManuallyDrop,
    os::windows::ffi::OsStrExt,
    ptr,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

mod stream;
#[cfg(test)]
mod tests;
const S_OK: HRESULT = HRESULT(0);
const S_FALSE: HRESULT = HRESULT(1);
const E_PENDING: HRESULT = HRESULT(0x8000000Au32 as i32);

struct Payload {
    files: Vec<DragFile>,
    confirmed: Arc<AtomicBool>,
    cancelled: CancellationToken,
    prepare: Mutex<Option<PrepareFiles>>,
    prepared: OnceLock<Result<tempfile::TempDir, String>>,
    error: Mutex<Option<String>>,
}
impl Payload {
    fn directory(&self) -> Result<&std::path::Path, HRESULT> {
        if self.cancelled.is_cancelled() {
            return Err(E_ABORT);
        }
        if !self.confirmed.load(Ordering::Acquire) {
            return Err(E_PENDING);
        }
        let result =
            self.prepared.get_or_init(|| {
                let prepare =
                    self.prepare.lock().unwrap().take().ok_or_else(|| {
                        "The archive payload has already been requested".to_owned()
                    })?;
                std::thread::spawn(prepare)
                    .join()
                    .unwrap_or_else(|_| Err("Archive extraction worker failed".to_owned()))
            });
        if self.cancelled.is_cancelled() {
            return Err(E_ABORT);
        }
        match result {
            Ok(directory) => Ok(directory.path()),
            Err(error) => {
                *self.error.lock().unwrap() = Some(error.clone());
                Err(if self.cancelled.is_cancelled() {
                    E_ABORT
                } else {
                    HRESULT(0x80004005u32 as i32)
                })
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Formats {
    descriptors: u16,
    contents: u16,
    zone: u16,
}
impl Formats {
    fn register() -> Result<Self, String> {
        let descriptors = unsafe { RegisterClipboardFormatW(CFSTR_FILEDESCRIPTORW) } as u16;
        let contents = unsafe { RegisterClipboardFormatW(CFSTR_FILECONTENTS) } as u16;
        let zone = unsafe { RegisterClipboardFormatW(CFSTR_ZONEIDENTIFIER) } as u16;
        if descriptors == 0 || contents == 0 {
            return Err("Could not register Explorer file formats".into());
        }
        Ok(Self {
            descriptors,
            contents,
            zone,
        })
    }
    fn descriptor_format(self) -> FORMATETC {
        format(self.descriptors, TYMED_HGLOBAL.0 as u32, -1)
    }
    fn content_format(self, index: i32) -> FORMATETC {
        format(self.contents, TYMED_ISTREAM.0 as u32, index)
    }
}
fn format(cf: u16, medium: u32, index: i32) -> FORMATETC {
    FORMATETC {
        cfFormat: cf,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: index,
        tymed: medium,
    }
}

#[implement(IEnumFORMATETC)]
struct FormatEnumerator {
    formats: Vec<FORMATETC>,
    index: Mutex<usize>,
}
impl IEnumFORMATETC_Impl for FormatEnumerator_Impl {
    fn Next(&self, count: u32, output: *mut FORMATETC, fetched: *mut u32) -> HRESULT {
        if output.is_null() || (count != 1 && fetched.is_null()) {
            return E_POINTER;
        }
        let mut index = self.index.lock().unwrap();
        let available = self
            .formats
            .len()
            .saturating_sub(*index)
            .min(count as usize);
        if available != 0 {
            unsafe {
                ptr::copy_nonoverlapping(self.formats.as_ptr().add(*index), output, available);
            }
        }
        *index += available;
        if !fetched.is_null() {
            unsafe {
                fetched.write(available as u32);
            }
        }
        if available == count as usize {
            S_OK
        } else {
            S_FALSE
        }
    }
    fn Skip(&self, count: u32) -> windows::core::Result<()> {
        let mut index = self.index.lock().unwrap();
        *index = index.saturating_add(count as usize).min(self.formats.len());
        Ok(())
    }
    fn Reset(&self) -> windows::core::Result<()> {
        *self.index.lock().unwrap() = 0;
        Ok(())
    }
    fn Clone(&self) -> windows::core::Result<IEnumFORMATETC> {
        Ok(FormatEnumerator {
            formats: self.formats.clone(),
            index: Mutex::new(*self.index.lock().unwrap()),
        }
        .into())
    }
}

#[implement(IDataObject)]
struct VirtualFiles {
    payload: Arc<Payload>,
    formats: Formats,
    descriptors: Vec<u8>,
    private_formats: Mutex<HashMap<u16, Vec<u8>>>,
}
impl VirtualFiles {
    fn query(&self, value: &FORMATETC) -> HRESULT {
        if value.dwAspect != DVASPECT_CONTENT.0 {
            return DV_E_FORMATETC;
        }
        if value.cfFormat == 15 && value.tymed & TYMED_HGLOBAL.0 as u32 != 0 {
            return S_OK;
        }
        if value.cfFormat == self.formats.descriptors && value.tymed & TYMED_HGLOBAL.0 as u32 != 0 {
            return S_OK;
        }
        if value.cfFormat == self.formats.contents && value.tymed & TYMED_ISTREAM.0 as u32 != 0 {
            return if value.lindex == -1
                || (value.lindex >= 0
                    && self
                        .payload
                        .files
                        .get(value.lindex as usize)
                        .is_some_and(|file| !file.directory))
            {
                S_OK
            } else {
                DV_E_LINDEX
            };
        }
        if value.tymed & TYMED_HGLOBAL.0 as u32 != 0
            && self
                .private_formats
                .lock()
                .unwrap()
                .contains_key(&value.cfFormat)
        {
            return S_OK;
        }
        DV_E_FORMATETC
    }
}
impl IDataObject_Impl for VirtualFiles_Impl {
    fn GetData(&self, requested: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let value = unsafe { requested.as_ref() }.ok_or_else(|| Error::from(E_POINTER))?;
        self.query(value).ok()?;
        if value.cfFormat == 15 {
            let directory = self.payload.directory().map_err(Error::from)?;
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&20u32.to_le_bytes());
            bytes.extend_from_slice(&[0u8; 12]);
            bytes.extend_from_slice(&1u32.to_le_bytes());
            for file in self
                .payload
                .files
                .iter()
                .filter(|file| file.name.components().count() == 1)
            {
                let path = directory.join(&file.archive_path);
                if self.payload.cancelled.is_cancelled() {
                    return Err(E_ABORT.into());
                }
                if !path.exists() {
                    *self.payload.error.lock().unwrap() = Some(format!(
                        "The selected item was not extracted: {}",
                        file.archive_path.display()
                    ));
                    return Err(Error::from(HRESULT::from_win32(2)));
                }
                for unit in path.as_os_str().encode_wide().chain(std::iter::once(0)) {
                    bytes.extend_from_slice(&unit.to_le_bytes());
                }
            }
            bytes.extend_from_slice(&0u16.to_le_bytes());
            return global_medium(&bytes);
        }
        if value.cfFormat == self.formats.descriptors {
            return global_medium(&self.descriptors);
        }
        if value.cfFormat == self.formats.contents {
            let file = self
                .payload
                .files
                .get(usize::try_from(value.lindex).map_err(|_| Error::from(DV_E_LINDEX))?)
                .filter(|file| !file.directory)
                .ok_or_else(|| Error::from(DV_E_LINDEX))?;
            let path = self
                .payload
                .directory()
                .map_err(Error::from)?
                .join(&file.archive_path);
            let stream: IStream = stream::FileStream::open(path, Arc::clone(&self.payload))
                .map_err(|error| {
                    *self.payload.error.lock().unwrap() =
                        Some(format!("Could not read an extracted file: {error}"));
                    error
                })?
                .into();
            return Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: STGMEDIUM_0 {
                    pstm: ManuallyDrop::new(Some(stream)),
                },
                pUnkForRelease: ManuallyDrop::new(None),
            });
        }
        let formats = self.private_formats.lock().unwrap();
        global_medium(
            formats
                .get(&value.cfFormat)
                .ok_or_else(|| Error::from(DV_E_FORMATETC))?,
        )
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn QueryGetData(&self, value: *const FORMATETC) -> HRESULT {
        unsafe { value.as_ref() }.map_or(E_POINTER, |value| self.query(value))
    }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, _: *mut FORMATETC) -> HRESULT {
        E_NOTIMPL
    }
    fn SetData(
        &self,
        requested: *const FORMATETC,
        medium: *const STGMEDIUM,
        release: BOOL,
    ) -> windows::core::Result<()> {
        let value = unsafe { requested.as_ref() }.ok_or_else(|| Error::from(E_POINTER))?;
        let data = unsafe { medium.as_ref() }.ok_or_else(|| Error::from(E_POINTER))?;
        if data.tymed != TYMED_HGLOBAL.0 as u32 {
            return Err(DV_E_FORMATETC.into());
        }
        let handle = unsafe { data.u.hGlobal };
        let size = unsafe { GlobalSize(handle) };
        if size > 1024 * 1024 {
            return Err(DV_E_FORMATETC.into());
        }
        let pointer = unsafe { GlobalLock(handle) };
        if pointer.is_null() {
            return Err(Error::from_thread());
        }
        let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), size).to_vec() };
        let _ = unsafe { GlobalUnlock(handle) };
        self.private_formats
            .lock()
            .unwrap()
            .insert(value.cfFormat, bytes);
        if release.as_bool() {
            unsafe {
                ReleaseStgMedium(medium.cast_mut());
            }
        }
        Ok(())
    }
    fn EnumFormatEtc(&self, direction: u32) -> windows::core::Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let mut formats = vec![
            format(15, TYMED_HGLOBAL.0 as u32, -1),
            self.formats.descriptor_format(),
            self.formats.content_format(-1),
        ];
        formats.extend(
            self.private_formats
                .lock()
                .unwrap()
                .keys()
                .map(|key| format(*key, TYMED_HGLOBAL.0 as u32, -1)),
        );
        Ok(FormatEnumerator {
            formats,
            index: Mutex::new(0),
        }
        .into())
    }
    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: Ref<'_, IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(E_NOTIMPL.into())
    }
    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(E_NOTIMPL.into())
    }
}

#[implement(IDropSource)]
struct DropSource {
    confirmed: Arc<AtomicBool>,
    cancel: CancellationToken,
    input: Option<Arc<SharedInput>>,
}
impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        let result = self.continue_drag(
            escape.as_bool() || unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) } < 0,
            keys,
            cursor_over_own_window(),
        );
        // Sharing input is needed only while the pointer is held. Detach before
        // a destination performs its potentially long synchronous Drop call.
        if result == DRAGDROP_S_DROP || result == DRAGDROP_S_CANCEL {
            if let Some(input) = &self.input {
                input.detach();
            }
        }
        result
    }
    fn GiveFeedback(&self, _: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}
impl DropSource {
    fn continue_drag(
        &self,
        escape: bool,
        keys: MODIFIERKEYS_FLAGS,
        over_own_window: bool,
    ) -> HRESULT {
        if escape || self.cancel.is_cancelled() || (!keys.contains(MK_LBUTTON) && over_own_window) {
            self.cancel.cancel();
            DRAGDROP_S_CANCEL
        } else if !keys.contains(MK_LBUTTON) {
            self.confirmed.store(true, Ordering::Release);
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }
}

fn cursor_over_own_window() -> bool {
    let mut point = POINT::default();
    if unsafe { GetCursorPos(&mut point) }.is_err() {
        return false;
    }
    let window = unsafe { WindowFromPoint(point) };
    let mut process = 0;
    unsafe {
        GetWindowThreadProcessId(window, Some(&mut process));
    }
    process == std::process::id()
}

pub fn is_drag_button_down() -> bool {
    let key = if unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0 {
        VK_RBUTTON
    } else {
        VK_LBUTTON
    };
    (unsafe { GetAsyncKeyState(key.0 as i32) }) < 0
}

pub fn prepare_descriptors(
    files: &[DragFile],
    cancel: &CancellationToken,
) -> Result<Vec<u8>, String> {
    descriptor_bytes_checked(files, Some(cancel))
}

#[cfg(test)]
fn descriptor_bytes(files: &[DragFile]) -> Result<Vec<u8>, String> {
    descriptor_bytes_checked(files, None)
}

fn descriptor_bytes_checked(
    files: &[DragFile],
    cancel: Option<&CancellationToken>,
) -> Result<Vec<u8>, String> {
    if cancel.is_some_and(CancellationToken::is_cancelled) {
        return Err("Drag cancelled".into());
    }
    let mut result = Vec::with_capacity(4 + files.len() * std::mem::size_of::<FILEDESCRIPTORW>());
    result.extend_from_slice(
        &u32::try_from(files.len())
            .map_err(|_| "Too many selected files")?
            .to_le_bytes(),
    );
    for file in files {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err("Drag cancelled".into());
        }
        crate::archive::safe_relative_path(&file.name).map_err(|error| error.to_string())?;
        crate::archive::safe_relative_path(&file.archive_path)
            .map_err(|error| error.to_string())?;
        let name: Vec<u16> = file.name.as_os_str().encode_wide().collect();
        if name.is_empty() || name.len() >= 260 || name.contains(&0) {
            return Err(format!(
                "The selected path is too long or invalid for Explorer: {}",
                file.name.display()
            ));
        }
        let mut descriptor = FILEDESCRIPTORW::default();
        descriptor.dwFlags = (FD_UNICODE.0 | FD_ATTRIBUTES.0) as u32;
        descriptor.dwFileAttributes = if file.directory {
            FILE_ATTRIBUTE_DIRECTORY.0
        } else {
            FILE_ATTRIBUTE_NORMAL.0
        };
        let mut filename = [0u16; 260];
        filename[..name.len()].copy_from_slice(&name);
        descriptor.cFileName = filename;
        if let Some(size) = file.size.filter(|_| !file.directory) {
            descriptor.dwFlags |= FD_FILESIZE.0 as u32;
            descriptor.nFileSizeLow = size as u32;
            descriptor.nFileSizeHigh = (size >> 32) as u32;
        }
        if let Some(ticks) = file
            .modified_unix_seconds
            .and_then(|value| (i128::from(value) + 11_644_473_600).checked_mul(10_000_000))
            .and_then(|value| u64::try_from(value).ok())
        {
            descriptor.dwFlags |= FD_WRITESTIME.0 as u32;
            descriptor.ftLastWriteTime.dwLowDateTime = ticks as u32;
            descriptor.ftLastWriteTime.dwHighDateTime = (ticks >> 32) as u32;
        }
        // FILEDESCRIPTORW is packed and was initialized entirely to zero.
        result.extend_from_slice(unsafe {
            std::slice::from_raw_parts(
                (&descriptor as *const FILEDESCRIPTORW).cast::<u8>(),
                std::mem::size_of::<FILEDESCRIPTORW>(),
            )
        });
    }
    Ok(result)
}
fn global_medium(bytes: &[u8]) -> windows::core::Result<STGMEDIUM> {
    let handle: HGLOBAL =
        unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len().max(1)) }?;
    let pointer = unsafe { GlobalLock(handle) };
    if pointer.is_null() {
        let _ = unsafe { GlobalFree(Some(handle)) };
        return Err(Error::from_thread());
    }
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len());
    }
    let _ = unsafe { GlobalUnlock(handle) };
    Ok(STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: handle },
        pUnkForRelease: ManuallyDrop::new(None),
    })
}
struct Apartment(bool);
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            if self.0 {
                OleUninitialize();
            } else {
                CoUninitialize();
            }
        }
    }
}

pub fn source_zone_metadata(archive: &std::path::Path) -> Result<Option<Vec<u8>>, String> {
    use ::windows::{
        Win32::UI::Shell::{BHID_DataObject, IShellItem, SHCreateItemFromParsingName},
        core::HSTRING,
    };
    let mut ads = archive.as_os_str().to_owned();
    ads.push(":Zone.Identifier");
    match std::fs::metadata(std::path::Path::new(&ads)) {
        Err(error)
            if matches!(error.kind(), std::io::ErrorKind::NotFound)
                || matches!(error.raw_os_error(), Some(1 | 50 | 123)) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(format!("Could not read archive origin metadata: {error}")),
        Ok(metadata) if metadata.len() > 65536 => {
            return Err("Zone.Identifier exceeds 64 KiB".into());
        }
        Ok(_) => {}
    }
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .map_err(|error| error.to_string())?;
    let _apartment = Apartment(false);
    let item: ::windows::core::Result<IShellItem> =
        unsafe { SHCreateItemFromParsingName(&HSTRING::from(archive.as_os_str()), None) };
    let item = item.map_err(|error| error.to_string())?;
    let object: IDataObject =
        unsafe { item.BindToHandler(None, &BHID_DataObject) }.map_err(|error| error.to_string())?;
    let zone = unsafe { RegisterClipboardFormatW(CFSTR_ZONEIDENTIFIER) } as u16;
    let mut medium = unsafe { object.GetData(&format(zone, TYMED_HGLOBAL.0 as u32, -1)) }
        .map_err(|error| format!("Could not transfer archive origin metadata: {error}"))?;
    let result = (|| {
        let handle = unsafe { medium.u.hGlobal };
        let size = unsafe { GlobalSize(handle) };
        if size > 1024 * 1024 {
            return Err("Archive origin clipboard metadata is too large".to_owned());
        }
        let pointer = unsafe { GlobalLock(handle) };
        if pointer.is_null() {
            return Err(Error::from_thread().to_string());
        }
        let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), size).to_vec() };
        let _ = unsafe { GlobalUnlock(handle) };
        Ok(Some(bytes))
    })();
    unsafe {
        ReleaseStgMedium(&mut medium);
    }
    result
}

pub fn start_file_drag(
    files: Vec<DragFile>,
    descriptors: Vec<u8>,
    // The archive's own Zone.Identifier stream, forwarded so files dropped into
    // Explorer keep their download origin instead of silently losing it.
    origin: Option<Vec<u8>>,
    prepare: PrepareFiles,
    cancel: CancellationToken,
    completion: mpsc::SyncSender<Result<bool, String>>,
) -> Result<(), String> {
    let ui_thread = unsafe { GetCurrentThreadId() };
    let mut keyboard = [0u8; 256];
    unsafe { GetKeyboardState(&mut keyboard) }.map_err(|error| error.to_string())?;
    // Slint must release its row's capture before OLE takes capture on the
    // dedicated STA. No OLE modal loop or archive I/O runs on the UI thread.
    let _ = unsafe { ReleaseCapture() };
    let on_error = completion.clone();
    let _ = spawn_sta_task(on_error, move || {
        run_file_drag(
            files,
            descriptors,
            origin,
            prepare,
            cancel,
            completion,
            ui_thread,
            keyboard,
        )
    })?;
    Ok(())
}

fn run_file_drag(
    files: Vec<DragFile>,
    descriptors: Vec<u8>,
    origin: Option<Vec<u8>>,
    prepare: PrepareFiles,
    cancel: CancellationToken,
    completion: mpsc::SyncSender<Result<bool, String>>,
    ui_thread: u32,
    mut keyboard: [u8; 256],
) -> Result<(), String> {
    if files.is_empty() {
        return Err("There are no files to drag".into());
    }
    let formats = Formats::register()?;
    let confirmed = Arc::new(AtomicBool::new(false));
    let input = Arc::new(SharedInput {
        native_thread: unsafe { GetCurrentThreadId() },
        ui_thread,
        attached: AtomicBool::new(false),
    });
    let source: IDropSource = DropSource {
        confirmed: Arc::clone(&confirmed),
        cancel: cancel.clone(),
        input: Some(Arc::clone(&input)),
    }
    .into();
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let (done_sender, done_receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let setup = (|| {
            unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
                .ok()
                .map_err(|error| error.to_string())?;
            let apartment = Apartment(false);
            let payload = Arc::new(Payload {
                files,
                confirmed,
                cancelled: cancel,
                prepare: Mutex::new(Some(prepare)),
                prepared: OnceLock::new(),
                error: Mutex::new(None),
            });
            let mut private_formats = HashMap::new();
            if let Some(origin) = origin {
                private_formats.insert(formats.zone, origin);
            }
            let object: IDataObject = VirtualFiles {
                payload: Arc::clone(&payload),
                formats,
                descriptors,
                private_formats: Mutex::new(private_formats),
            }
            .into();
            let stream =
                unsafe { CoMarshalInterThreadInterfaceInStream(&IDataObject::IID, &object) }
                    .map_err(|error| error.to_string())?;
            Ok::<_, String>((apartment, payload, object, stream))
        })();
        let (apartment, payload, object, stream) = match setup {
            Ok(setup) => setup,
            Err(error) => {
                let _ = ready_sender.send(Err(error));
                return;
            }
        };
        let raw = stream.into_raw() as usize;
        if ready_sender.send(Ok(raw)).is_err() {
            let stream = unsafe { IStream::from_raw(raw as *mut _) };
            let _ = unsafe { CoReleaseMarshalData(&stream) };
            return;
        }
        let result = done_receiver
            .recv()
            .unwrap_or_else(|_| Err("The Explorer drag was interrupted".into()));
        drop(object);
        // Every data object and file stream retains the payload. Keep the MTA
        // alive while Explorer reads, without relying on EndOperation to clean up.
        while Arc::strong_count(&payload) > 1 {
            std::thread::sleep(Duration::from_millis(20));
        }
        let error = payload.error.lock().unwrap().take();
        let result = if payload.cancelled.is_cancelled() {
            Ok(false)
        } else if let Some(error) = error {
            Err(error)
        } else {
            result
        };
        // All transfer references are gone. Consume the owned TempDir here and
        // retry transient sharing failures before reporting the operation done.
        if let Ok(mut payload) = Arc::try_unwrap(payload) {
            if let Some(Ok(directory)) = payload.prepared.take() {
                super::cleanup_staging(directory);
            }
        }
        drop(apartment);
        let _ = completion.try_send(result);
    });
    let raw = ready_receiver.recv().map_err(|error| error.to_string())??;
    // CoGetInterfaceAndReleaseStream consumes this marshaling-stream reference.
    let stream = ManuallyDrop::new(unsafe { IStream::from_raw(raw as *mut _) });
    let object: IDataObject =
        unsafe { CoGetInterfaceAndReleaseStream(&*stream) }.map_err(|error| error.to_string())?;
    let mut effect = DROPEFFECT_NONE;
    let _input = InputAttachment::attach(input)?;
    // Attaching queues resets key state. Restore physical down states, including
    // the held mouse button, so starting OLE cannot accidentally commit a drop.
    for (key, state) in keyboard.iter_mut().enumerate() {
        *state = (*state & 1)
            | if unsafe { GetAsyncKeyState(key as i32) } < 0 {
                0x80
            } else {
                0
            };
    }
    if unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0 {
        keyboard.swap(VK_LBUTTON.0 as usize, VK_RBUTTON.0 as usize);
    }
    unsafe { SetKeyboardState(&keyboard) }.map_err(|error| error.to_string())?;
    if keyboard[VK_LBUTTON.0 as usize] & 0x80 == 0 {
        drop(object);
        let _ = done_sender.send(Ok(false));
        return Ok(());
    }
    let status = unsafe { DoDragDrop(&object, &source, DROPEFFECT_COPY, &mut effect) };
    let result = if status == DRAGDROP_S_CANCEL {
        Ok(false)
    } else if status == DRAGDROP_S_DROP {
        Ok(effect != DROPEFFECT_NONE)
    } else {
        Err(format!("Explorer drag-and-drop failed: {status:?}"))
    };
    drop(object);
    let _ = done_sender.send(result);
    Ok(())
}

fn spawn_sta_task(
    on_error: mpsc::SyncSender<Result<bool, String>>,
    work: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> Result<std::thread::JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name("archive-drag".into())
        .spawn(move || {
            let result = (|| {
                unsafe { OleInitialize(None) }.map_err(|error| error.to_string())?;
                let _sta = Apartment(true);
                work()
            })();
            if let Err(error) = result {
                let _ = on_error.try_send(Err(error));
            }
        })
        .map_err(|error| error.to_string())
}

struct SharedInput {
    native_thread: u32,
    ui_thread: u32,
    attached: AtomicBool,
}
impl SharedInput {
    fn detach(&self) {
        if self.attached.swap(false, Ordering::AcqRel) {
            unsafe {
                let _ = AttachThreadInput(self.native_thread, self.ui_thread, false);
            }
        }
    }
}
struct InputAttachment(Arc<SharedInput>);
impl InputAttachment {
    fn attach(input: Arc<SharedInput>) -> Result<Self, String> {
        if !unsafe { AttachThreadInput(input.native_thread, input.ui_thread, true) }.as_bool() {
            return Err(format!(
                "Could not attach native drag input: {}",
                Error::from_thread()
            ));
        }
        input.attached.store(true, Ordering::Release);
        Ok(Self(input))
    }
}
impl Drop for InputAttachment {
    fn drop(&mut self) {
        self.0.detach();
    }
}
