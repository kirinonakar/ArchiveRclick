use super::*;
use ::windows::Win32::{
    Foundation::{STG_E_ACCESSDENIED, STG_E_INVALIDFUNCTION, STG_E_WRITEFAULT},
    System::Com::{
        ISequentialStream_Impl, IStream_Impl, LOCKTYPE, STATFLAG, STATSTG, STGC, STGM_READ,
        STGTY_STREAM, STREAM_SEEK,
    },
};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
};

#[implement(IStream)]
pub(super) struct FileStream {
    path: PathBuf,
    file: Mutex<File>,
    payload: Arc<Payload>,
}
fn io_error(error: std::io::Error) -> Error {
    Error::from(HRESULT::from_win32(error.raw_os_error().unwrap_or(1) as u32))
}
impl FileStream {
    pub(super) fn open(path: PathBuf, payload: Arc<Payload>) -> windows::core::Result<Self> {
        let file = File::open(&path).map_err(io_error)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
            payload,
        })
    }
}
impl ISequentialStream_Impl for FileStream_Impl {
    fn Read(&self, output: *mut core::ffi::c_void, count: u32, read: *mut u32) -> HRESULT {
        if !read.is_null() {
            unsafe {
                read.write(0);
            }
        }
        if self.payload.cancelled.is_cancelled() {
            return E_ABORT;
        }
        if count == 0 {
            return S_OK;
        }
        if output.is_null() {
            return E_POINTER;
        }
        let buffer = unsafe { std::slice::from_raw_parts_mut(output.cast::<u8>(), count as usize) };
        match self.file.lock().unwrap().read(buffer) {
            Ok(actual) => {
                if !read.is_null() {
                    unsafe {
                        read.write(actual as u32);
                    }
                }
                if actual == count as usize {
                    S_OK
                } else {
                    S_FALSE
                }
            }
            Err(error) => {
                *self.payload.error.lock().unwrap() =
                    Some(format!("Could not read an extracted file: {error}"));
                io_error(error).code()
            }
        }
    }
    fn Write(&self, _: *const core::ffi::c_void, _: u32, written: *mut u32) -> HRESULT {
        if !written.is_null() {
            unsafe {
                written.write(0);
            }
        }
        STG_E_ACCESSDENIED
    }
}
impl IStream_Impl for FileStream_Impl {
    fn Seek(
        &self,
        offset: i64,
        origin: STREAM_SEEK,
        position: *mut u64,
    ) -> windows::core::Result<()> {
        let seek = match origin.0 {
            0 => SeekFrom::Start(
                u64::try_from(offset).map_err(|_| Error::from(STG_E_INVALIDFUNCTION))?,
            ),
            1 => SeekFrom::Current(offset),
            2 => SeekFrom::End(offset),
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let actual = self.file.lock().unwrap().seek(seek).map_err(io_error)?;
        if !position.is_null() {
            unsafe {
                position.write(actual);
            }
        }
        Ok(())
    }
    fn SetSize(&self, _: u64) -> windows::core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }
    fn CopyTo(
        &self,
        target: Ref<'_, IStream>,
        count: u64,
        read: *mut u64,
        written: *mut u64,
    ) -> windows::core::Result<()> {
        if !read.is_null() {
            unsafe {
                read.write(0);
            }
        }
        if !written.is_null() {
            unsafe {
                written.write(0);
            }
        }
        let target = target.as_ref().ok_or_else(|| Error::from(E_POINTER))?;
        let mut buffer = vec![0u8; 128 * 1024];
        let mut total_read = 0;
        let mut total_written = 0;
        while total_read < count {
            let requested = (count - total_read).min(buffer.len() as u64) as u32;
            let mut actual = 0;
            self.Read(buffer.as_mut_ptr().cast(), requested, &mut actual)
                .ok()?;
            total_read += u64::from(actual);
            if !read.is_null() {
                unsafe {
                    read.write(total_read);
                }
            }
            if actual == 0 {
                break;
            }
            let mut sent = 0;
            unsafe { target.Write(buffer.as_ptr().cast(), actual, Some(&mut sent)) }.ok()?;
            total_written += u64::from(sent);
            if !written.is_null() {
                unsafe {
                    written.write(total_written);
                }
            }
            if sent != actual {
                return Err(STG_E_WRITEFAULT.into());
            }
        }
        Ok(())
    }
    fn Commit(&self, _: &STGC) -> windows::core::Result<()> {
        Ok(())
    }
    fn Revert(&self) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn Stat(&self, output: *mut STATSTG, _: &STATFLAG) -> windows::core::Result<()> {
        if output.is_null() {
            return Err(E_POINTER.into());
        }
        let size = self
            .file
            .lock()
            .unwrap()
            .metadata()
            .map_err(io_error)?
            .len();
        let stat = STATSTG {
            r#type: STGTY_STREAM.0 as u32,
            cbSize: size,
            grfMode: STGM_READ,
            ..Default::default()
        };
        unsafe {
            output.write(stat);
        }
        Ok(())
    }
    fn Clone(&self) -> windows::core::Result<IStream> {
        let position = self
            .file
            .lock()
            .unwrap()
            .stream_position()
            .map_err(io_error)?;
        let clone = FileStream::open(self.path.clone(), Arc::clone(&self.payload))?;
        clone
            .file
            .lock()
            .unwrap()
            .seek(SeekFrom::Start(position))
            .map_err(io_error)?;
        Ok(clone.into())
    }
}
