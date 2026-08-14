//! Native Windows file-drag source used to hand extracted archive entries to
//! Explorer.

#[cfg(windows)]
mod imp {
    use std::{
        cell::Cell,
        mem::{size_of, ManuallyDrop},
        os::windows::ffi::OsStrExt,
        path::PathBuf,
        ptr,
    };

    use windows::{
        core::{implement, Error, Ref, BOOL, HRESULT},
        Win32::{
            Foundation::{
                GlobalFree, DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP,
                DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC, E_NOTIMPL, E_POINTER, HGLOBAL, POINT,
            },
            System::{
                Com::{
                    IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC,
                    IEnumFORMATETC_Impl, IEnumSTATDATA, DATADIR_GET, DVASPECT_CONTENT, FORMATETC,
                    STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
                },
                Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT},
                Ole::{
                    DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, OleUninitialize,
                    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE,
                },
                SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS},
            },
        },
    };

    const CF_HDROP: u16 = 15;
    const S_OK: HRESULT = HRESULT(0);
    const S_FALSE: HRESULT = HRESULT(1);

    #[repr(C)]
    struct DropFilesHeader {
        p_files: u32,
        point: POINT,
        non_client: i32,
        wide: i32,
    }

    #[implement(IEnumFORMATETC)]
    struct FileFormatEnumerator {
        formats: Vec<FORMATETC>,
        index: Cell<usize>,
    }

    impl FileFormatEnumerator {
        fn new(index: usize) -> Self {
            Self {
                formats: vec![file_format()],
                index: Cell::new(index),
            }
        }
    }

    impl IEnumFORMATETC_Impl for FileFormatEnumerator_Impl {
        fn Next(&self, celt: u32, rgelt: *mut FORMATETC, pceltfetched: *mut u32) -> HRESULT {
            if rgelt.is_null() || (celt != 1 && pceltfetched.is_null()) {
                return E_POINTER;
            }

            let start = self.index.get();
            let available = self.formats.len().saturating_sub(start);
            let count = available.min(celt as usize);
            if count > 0 {
                // SAFETY: the caller supplied storage for celt FORMATETC values;
                // count is no larger than celt and the source slice is valid.
                unsafe {
                    ptr::copy_nonoverlapping(self.formats.as_ptr().add(start), rgelt, count);
                }
                self.index.set(start + count);
            }
            if !pceltfetched.is_null() {
                // SAFETY: the pointer was checked above when required and is an
                // optional output for the one-element form of Next.
                unsafe { pceltfetched.write(count as u32) };
            }
            if count == celt as usize {
                S_OK
            } else {
                S_FALSE
            }
        }

        fn Skip(&self, celt: u32) -> windows::core::Result<()> {
            self.index
                .set(self.index.get().saturating_add(celt as usize));
            Ok(())
        }

        fn Reset(&self) -> windows::core::Result<()> {
            self.index.set(0);
            Ok(())
        }

        fn Clone(&self) -> windows::core::Result<IEnumFORMATETC> {
            Ok(FileFormatEnumerator::new(self.index.get()).into())
        }
    }

    #[implement(IDataObject)]
    struct FileDropDataObject {
        paths: Vec<PathBuf>,
    }

    impl FileDropDataObject {
        fn new(paths: Vec<PathBuf>) -> Self {
            Self { paths }
        }
    }

    impl IDataObject_Impl for FileDropDataObject_Impl {
        fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
            let Some(format) = (unsafe { pformatetcin.as_ref() }) else {
                return Err(E_POINTER.into());
            };
            if query_file_format(format) != S_OK {
                return Err(DV_E_FORMATETC.into());
            }

            let hglobal = make_hdrop(&self.paths)?;
            Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                // SAFETY: hglobal is a valid movable global allocation whose
                // ownership is transferred to the receiving IDataObject caller.
                u: STGMEDIUM_0 { hGlobal: hglobal },
                pUnkForRelease: ManuallyDrop::new(None),
            })
        }

        fn GetDataHere(
            &self,
            _pformatetc: *const FORMATETC,
            _pmedium: *mut STGMEDIUM,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
            let Some(format) = (unsafe { pformatetc.as_ref() }) else {
                return E_POINTER;
            };
            query_file_format(format)
        }

        fn GetCanonicalFormatEtc(
            &self,
            pformatectin: *const FORMATETC,
            pformatetcout: *mut FORMATETC,
        ) -> HRESULT {
            if pformatectin.is_null() || pformatetcout.is_null() {
                return E_POINTER;
            }
            // SAFETY: both pointers are validated above and are COM-owned
            // FORMATETC structures supplied by the caller.
            unsafe { pformatetcout.write(*pformatectin) };
            DATA_S_SAMEFORMATETC
        }

        fn SetData(
            &self,
            _pformatetc: *const FORMATETC,
            _pmedium: *const STGMEDIUM,
            _frelease: BOOL,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
            if dwdirection != DATADIR_GET.0 as u32 {
                return Err(E_NOTIMPL.into());
            }
            Ok(FileFormatEnumerator::new(0).into())
        }

        fn DAdvise(
            &self,
            _pformatetc: *const FORMATETC,
            _advf: u32,
            _padvsink: Ref<'_, IAdviseSink>,
        ) -> windows::core::Result<u32> {
            Err(E_NOTIMPL.into())
        }

        fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
            Err(E_NOTIMPL.into())
        }
    }

    #[implement(IDropSource)]
    struct FileDropSource;

    impl IDropSource_Impl for FileDropSource_Impl {
        fn QueryContinueDrag(
            &self,
            fescapepressed: BOOL,
            grfkeystate: MODIFIERKEYS_FLAGS,
        ) -> HRESULT {
            if fescapepressed.as_bool() {
                DRAGDROP_S_CANCEL
            } else if !grfkeystate.contains(MK_LBUTTON) {
                DRAGDROP_S_DROP
            } else {
                S_OK
            }
        }

        fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
            DRAGDROP_S_USEDEFAULTCURSORS
        }
    }

    struct ComApartment;

    impl ComApartment {
        fn initialize() -> Result<Self, String> {
            let result = unsafe { OleInitialize(None) };
            if let Err(error) = result {
                Err(format!("Could not initialize OLE drag-and-drop: {error}"))
            } else {
                Ok(Self)
            }
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            unsafe { OleUninitialize() };
        }
    }

    fn file_format() -> FORMATETC {
        FORMATETC {
            cfFormat: CF_HDROP,
            ptd: ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        }
    }

    fn query_file_format(format: &FORMATETC) -> HRESULT {
        if format.cfFormat == CF_HDROP
            && format.dwAspect == DVASPECT_CONTENT.0
            && format.tymed & TYMED_HGLOBAL.0 as u32 != 0
        {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }

    fn make_hdrop(paths: &[PathBuf]) -> windows::core::Result<HGLOBAL> {
        let mut names = Vec::<u16>::new();
        for path in paths {
            let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
            if wide.contains(&0) {
                return Err(E_POINTER.into());
            }
            names.extend(wide);
            names.push(0);
        }
        names.push(0);

        let header_size = size_of::<DropFilesHeader>();
        let names_size = names
            .len()
            .checked_mul(size_of::<u16>())
            .ok_or_else(|| Error::from(E_POINTER))?;
        let allocation_size = header_size
            .checked_add(names_size)
            .ok_or_else(|| Error::from(E_POINTER))?;
        let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, allocation_size) }?;
        let locked = unsafe { GlobalLock(hglobal) };
        if locked.is_null() {
            let _ = unsafe { GlobalFree(Some(hglobal)) };
            return Err(Error::from_win32());
        }

        let header = DropFilesHeader {
            p_files: header_size as u32,
            point: POINT { x: 0, y: 0 },
            non_client: 0,
            wide: 1,
        };
        // SAFETY: the allocation is at least the header plus the UTF-16 list,
        // and GlobalAlloc provides suitable alignment for both structures.
        unsafe {
            ptr::write(locked.cast::<DropFilesHeader>(), header);
            ptr::copy_nonoverlapping(
                names.as_ptr(),
                locked.cast::<u16>().add(header_size / size_of::<u16>()),
                names.len(),
            );
            let _ = GlobalUnlock(hglobal);
        }
        Ok(hglobal)
    }

    pub fn start_file_drag(paths: &[PathBuf]) -> Result<(), String> {
        if paths.is_empty() {
            return Err("There are no files to drag".to_owned());
        }
        let _com = ComApartment::initialize()?;
        let data_object: IDataObject = FileDropDataObject::new(paths.to_vec()).into();
        let drop_source: IDropSource = FileDropSource.into();
        let mut effect = DROPEFFECT_NONE;
        let result =
            unsafe { DoDragDrop(&data_object, &drop_source, DROPEFFECT_COPY, &mut effect) };
        if result == DRAGDROP_S_CANCEL || result == DRAGDROP_S_DROP {
            Ok(())
        } else {
            Err(format!("Explorer drag-and-drop failed: {result:?}"))
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::PathBuf;

    pub fn start_file_drag(_paths: &[PathBuf]) -> Result<(), String> {
        Err("Explorer drag-and-drop is only available on Windows".to_owned())
    }
}

pub use imp::start_file_drag;
