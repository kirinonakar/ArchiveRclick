//! COM stream adapters for files and split archive volumes.

use super::*;

// ------------------------------------------------------------------
// File streams handed to 7-Zip (IInStream / IOutStream)
// ------------------------------------------------------------------
#[repr(C)]
pub(super) struct InStreamVtbl {
    query_interface: QueryInterfaceFn,
    add_ref: AddRefFn,
    release: ReleaseFn,
    read: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, *mut u32) -> i32,
    seek: unsafe extern "system" fn(*mut c_void, i64, u32, *mut u64) -> i32,
}

pub(super) static IN_STREAM_VTBL: InStreamVtbl = InStreamVtbl {
    query_interface: in_stream_query_interface,
    add_ref: stream_add_ref,
    release: in_stream_release,
    read: in_stream_read,
    seek: in_stream_seek,
};

#[repr(C)]
pub(super) struct InStream {
    pub(super) vtbl: &'static InStreamVtbl,
    pub(super) refs: AtomicU32,
    pub(super) file: Mutex<BufReader<File>>,
    pub(super) position: AtomicU64,
    pub(super) progress: Option<Arc<UpdateInputProgress>>,
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
    let result = file.read(buffer);
    drop(file);
    match result {
        Ok(amount) => {
            if !processed.is_null() {
                unsafe { *processed = amount as u32 };
            }
            if amount != 0 {
                let position = stream
                    .position
                    .fetch_add(amount as u64, Ordering::AcqRel)
                    .saturating_add(amount as u64);
                if let Some(progress) = &stream.progress {
                    progress.report(position);
                }
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
    stream.position.store(position, Ordering::Release);
    if !new_position.is_null() {
        unsafe { *new_position = position };
    }
    S_OK
}

static MULTI_IN_STREAM_VTBL: InStreamVtbl = InStreamVtbl {
    query_interface: multi_in_stream_query_interface,
    add_ref: stream_add_ref,
    release: multi_in_stream_release,
    read: multi_in_stream_read,
    seek: multi_in_stream_seek,
};

/// Presents `<archive>.<nnn>` files as one logical input stream.  7-Zip's
/// volume writer splits the byte stream outside the format handler, so
/// joining the parts again is the inverse operation for ZIP and 7z.
#[repr(C)]
pub(super) struct MultiInStream {
    vtbl: &'static InStreamVtbl,
    refs: AtomicU32,
    files: Mutex<Vec<BufReader<File>>>,
    starts: Vec<u64>,
    lengths: Vec<u64>,
    total_length: u64,
    position: AtomicU64,
}

impl MultiInStream {
    pub(super) fn open(paths: &[PathBuf]) -> ArchiveResult<Self> {
        if paths.is_empty() {
            return Err(ArchiveError::InvalidInput(
                "split archive has no volume files".to_owned(),
            ));
        }
        let mut files = Vec::with_capacity(paths.len());
        let mut starts = Vec::with_capacity(paths.len());
        let mut lengths = Vec::with_capacity(paths.len());
        let mut total_length = 0u64;
        for path in paths {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
                .open(path)
                .map_err(|error| ArchiveError::io(path, error))?;
            let length = file
                .metadata()
                .map_err(|error| ArchiveError::io(path, error))?
                .len();
            starts.push(total_length);
            lengths.push(length);
            total_length = total_length.checked_add(length).ok_or_else(|| {
                ArchiveError::LimitExceeded("split archive size overflow".to_owned())
            })?;
            files.push(BufReader::with_capacity(STREAM_BUFFER_SIZE, file));
        }
        Ok(Self {
            vtbl: &MULTI_IN_STREAM_VTBL,
            refs: AtomicU32::new(2),
            files: Mutex::new(files),
            starts,
            lengths,
            total_length,
            position: AtomicU64::new(0),
        })
    }

    fn volume_index(&self, position: u64) -> usize {
        self.starts
            .partition_point(|start| *start <= position)
            .saturating_sub(1)
    }
}

unsafe extern "system" fn multi_in_stream_query_interface(
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

unsafe extern "system" fn multi_in_stream_release(this: *mut c_void) -> u32 {
    let stream = unsafe { &*(this as *const MultiInStream) };
    let remaining = stream.refs.fetch_sub(1, Ordering::AcqRel).saturating_sub(1);
    if remaining == 0 {
        unsafe { drop(Box::from_raw(this as *mut MultiInStream)) };
    }
    remaining
}

unsafe extern "system" fn multi_in_stream_read(
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
    let stream = unsafe { &*(this as *const MultiInStream) };
    let mut files = stream
        .files
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let mut position = stream.position.load(Ordering::Acquire);
    let mut remaining = size as usize;
    let mut destination = unsafe { std::slice::from_raw_parts_mut(data.cast::<u8>(), remaining) };

    while remaining != 0 && position < stream.total_length {
        let index = stream.volume_index(position);
        let local = position.saturating_sub(stream.starts[index]);
        let available = stream.lengths[index].saturating_sub(local);
        if available == 0 {
            position = stream
                .starts
                .get(index + 1)
                .copied()
                .unwrap_or(stream.total_length);
            continue;
        }
        let amount = remaining.min(usize::try_from(available).unwrap_or(remaining));
        let file = &mut files[index];
        if file.seek(SeekFrom::Start(local)).is_err() {
            return E_FAIL;
        }
        let read = match file.read(&mut destination[..amount]) {
            Ok(read) => read,
            Err(_) => return E_FAIL,
        };
        if read == 0 {
            return E_FAIL;
        }
        position = position.saturating_add(read as u64);
        remaining -= read;
        destination = &mut destination[read..];
    }
    stream.position.store(position, Ordering::Release);
    if !processed.is_null() {
        unsafe { *processed = (size as usize - remaining) as u32 };
    }
    S_OK
}

unsafe extern "system" fn multi_in_stream_seek(
    this: *mut c_void,
    offset: i64,
    origin: u32,
    new_position: *mut u64,
) -> i32 {
    let stream = unsafe { &*(this as *const MultiInStream) };
    let current = stream.position.load(Ordering::Acquire);
    let base = match origin {
        SEEK_SET => 0,
        SEEK_CUR => current,
        SEEK_END => stream.total_length,
        _ => return E_INVALIDARG,
    };
    let position = if offset >= 0 {
        base.checked_add(offset as u64)
    } else {
        base.checked_sub(offset.unsigned_abs())
    };
    let Some(position) = position else {
        return E_FAIL;
    };
    stream.position.store(position, Ordering::Release);
    if !new_position.is_null() {
        unsafe { *new_position = position };
    }
    S_OK
}

#[repr(C)]
pub(super) struct OutStreamVtbl {
    query_interface: QueryInterfaceFn,
    add_ref: AddRefFn,
    release: ReleaseFn,
    write: unsafe extern "system" fn(*mut c_void, *const c_void, u32, *mut u32) -> i32,
    seek: unsafe extern "system" fn(*mut c_void, i64, u32, *mut u64) -> i32,
    set_size: unsafe extern "system" fn(*mut c_void, u64) -> i32,
}

pub(super) static OUT_STREAM_VTBL: OutStreamVtbl = OutStreamVtbl {
    query_interface: out_stream_query_interface,
    add_ref: stream_add_ref,
    release: out_stream_release,
    write: out_stream_write,
    seek: out_stream_seek,
    set_size: out_stream_set_size,
};

#[repr(C)]
pub(super) struct OutStream {
    pub(super) vtbl: &'static OutStreamVtbl,
    pub(super) refs: AtomicU32,
    pub(super) file: Arc<Mutex<Option<File>>>,
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

pub(super) static VOLUME_OUT_STREAM_VTBL: OutStreamVtbl = OutStreamVtbl {
    query_interface: volume_out_stream_query_interface,
    add_ref: stream_add_ref,
    release: volume_out_stream_release,
    write: volume_out_stream_write,
    seek: volume_out_stream_seek,
    set_size: volume_out_stream_set_size,
};

struct VolumePart {
    path: PathBuf,
    file: Option<File>,
    length: u64,
}

pub(super) struct VolumeOutput {
    prefix: PathBuf,
    volume_size: u64,
    parts: Vec<VolumePart>,
    position: u64,
    length: u64,
    error: Option<ArchiveError>,
}

impl VolumeOutput {
    pub(super) fn new(prefix: PathBuf, volume_size: u64) -> Self {
        Self {
            prefix,
            volume_size,
            parts: Vec::new(),
            position: 0,
            length: 0,
            error: None,
        }
    }

    fn part_path(&self, index: usize) -> PathBuf {
        volume_part_path(&self.prefix, index as u32 + 1)
    }

    fn remember_error(&mut self, path: impl Into<PathBuf>, error: std::io::Error) {
        if self.error.is_none() {
            self.error = Some(ArchiveError::io(path, error));
        }
    }

    fn remember_message(&mut self, message: impl Into<String>) {
        if self.error.is_none() {
            self.error = Some(ArchiveError::SevenZip(message.into()));
        }
    }

    fn ensure_part(&mut self, index: usize) -> Result<(), ()> {
        while self.parts.len() <= index {
            let path = self.part_path(self.parts.len());
            let file = match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(error) => {
                    self.remember_error(&path, error);
                    return Err(());
                }
            };
            self.parts.push(VolumePart {
                path,
                file: Some(file),
                length: 0,
            });
        }
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), ()> {
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let volume_index = self.position / self.volume_size;
            let Ok(volume_index) = usize::try_from(volume_index) else {
                self.remember_message("too many output volumes");
                return Err(());
            };
            let local_position = self.position % self.volume_size;
            if self.ensure_part(volume_index).is_err() {
                return Err(());
            }
            let available = self.volume_size.saturating_sub(local_position);
            let amount = remaining
                .len()
                .min(usize::try_from(available).unwrap_or(remaining.len()));
            if amount == 0 {
                self.remember_message("invalid output volume size");
                return Err(());
            }
            let path = self.parts[volume_index].path.clone();
            let write_result = match self.parts[volume_index].file.as_mut() {
                Some(file) => file
                    .seek(SeekFrom::Start(local_position))
                    .and_then(|_| file.write_all(&remaining[..amount])),
                None => {
                    self.remember_message(format!(
                        "output volume {} is already closed",
                        path.display()
                    ));
                    return Err(());
                }
            };
            if let Err(error) = write_result {
                self.remember_error(path, error);
                return Err(());
            }
            let amount = amount as u64;
            let part = &mut self.parts[volume_index];
            part.length = part.length.max(local_position.saturating_add(amount));
            self.position = self.position.saturating_add(amount);
            self.length = self.length.max(self.position);
            remaining = &remaining[amount as usize..];
        }
        Ok(())
    }

    fn seek(&mut self, offset: i64, origin: u32) -> Result<u64, ()> {
        let base = match origin {
            SEEK_SET => 0,
            SEEK_CUR => self.position,
            SEEK_END => self.length,
            _ => {
                self.remember_message("invalid output stream seek origin");
                return Err(());
            }
        };
        let position = if offset >= 0 {
            base.checked_add(offset as u64)
        } else {
            base.checked_sub(offset.unsigned_abs())
        };
        let Some(position) = position else {
            self.remember_message("output stream seek moved before the beginning");
            return Err(());
        };
        self.position = position;
        Ok(position)
    }

    fn set_size(&mut self, size: u64) -> Result<(), ()> {
        let required_parts = if size == 0 {
            0
        } else {
            usize::try_from((size - 1) / self.volume_size + 1).map_err(|_| {
                self.remember_message("too many output volumes");
            })?
        };
        if required_parts > 0 && self.ensure_part(required_parts - 1).is_err() {
            return Err(());
        }

        let mut remaining = size;
        for index in 0..required_parts {
            let desired = remaining.min(self.volume_size);
            let path = self.parts[index].path.clone();
            let set_result = match self.parts[index].file.as_mut() {
                Some(file) => file.set_len(desired),
                None => {
                    self.remember_message(format!(
                        "output volume {} is already closed",
                        path.display()
                    ));
                    return Err(());
                }
            };
            if let Err(error) = set_result {
                self.remember_error(path, error);
                return Err(());
            }
            self.parts[index].length = desired;
            remaining -= desired;
        }
        while self.parts.len() > required_parts {
            let part = self.parts.pop().expect("length checked");
            drop(part.file);
            if let Err(error) = fs::remove_file(&part.path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                self.remember_error(part.path, error);
                return Err(());
            }
        }
        self.length = size;
        self.position = self.position.min(size);
        Ok(())
    }

    pub(super) fn close_files(&mut self) {
        for part in &mut self.parts {
            drop(part.file.take());
        }
    }

    pub(super) fn paths(&self) -> Vec<PathBuf> {
        self.parts.iter().map(|part| part.path.clone()).collect()
    }

    pub(super) fn take_error(&mut self) -> Option<ArchiveError> {
        self.error.take()
    }
}

#[repr(C)]
pub(super) struct VolumeOutStream {
    pub(super) vtbl: &'static OutStreamVtbl,
    pub(super) refs: AtomicU32,
    pub(super) output: Arc<Mutex<VolumeOutput>>,
}

unsafe extern "system" fn volume_out_stream_query_interface(
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

unsafe extern "system" fn volume_out_stream_release(this: *mut c_void) -> u32 {
    let stream = unsafe { &*(this as *const VolumeOutStream) };
    let remaining = stream.refs.fetch_sub(1, Ordering::AcqRel).saturating_sub(1);
    if remaining == 0 {
        unsafe { drop(Box::from_raw(this as *mut VolumeOutStream)) };
    }
    remaining
}

unsafe extern "system" fn volume_out_stream_write(
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
    let stream = unsafe { &*(this as *const VolumeOutStream) };
    let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size as usize) };
    let mut output = stream
        .output
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if output.write(bytes).is_err() {
        return E_FAIL;
    }
    if !processed.is_null() {
        unsafe { *processed = size };
    }
    S_OK
}

unsafe extern "system" fn volume_out_stream_seek(
    this: *mut c_void,
    offset: i64,
    origin: u32,
    new_position: *mut u64,
) -> i32 {
    let stream = unsafe { &*(this as *const VolumeOutStream) };
    let mut output = stream
        .output
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let Ok(position) = output.seek(offset, origin) else {
        return E_FAIL;
    };
    if !new_position.is_null() {
        unsafe { *new_position = position };
    }
    S_OK
}

unsafe extern "system" fn volume_out_stream_set_size(this: *mut c_void, size: u64) -> i32 {
    let stream = unsafe { &*(this as *const VolumeOutStream) };
    let mut output = stream
        .output
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if output.set_size(size).is_err() {
        E_FAIL
    } else {
        S_OK
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
