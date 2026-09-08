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
    files: Mutex<Vec<(BufReader<File>, Option<u64>)>>,
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
            files.push((BufReader::with_capacity(STREAM_BUFFER_SIZE, file), Some(0)));
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
        let (file, cursor) = &mut files[index];
        if seek_buffered(file, cursor, local).is_err() {
            return E_FAIL;
        }
        let read = match file.read(&mut destination[..amount]) {
            Ok(read) => read,
            Err(_) => {
                *cursor = None;
                return E_FAIL;
            }
        };
        if read == 0 {
            return E_FAIL;
        }
        *cursor = Some(local + read as u64);
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

// BufReader::seek discards read-ahead even for a no-op seek. Keep the logical
// cursor separately so sequential volume reads do not seek, and short backward
// seeks can reuse buffered bytes without querying the OS file position.
fn seek_buffered<R: Read + Seek>(
    file: &mut BufReader<R>,
    cursor: &mut Option<u64>,
    target: u64,
) -> std::io::Result<()> {
    let previous = cursor.take();
    if let Some(offset) =
        previous.and_then(|position| i64::try_from(i128::from(target) - i128::from(position)).ok())
    {
        file.seek_relative(offset)?;
    } else {
        file.seek(SeekFrom::Start(target))?;
    }
    *cursor = Some(target);
    Ok(())
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
    pub(super) budget: Option<Arc<OutputBudget>>,
    // Protected by the file mutex, including across reservation and I/O.
    pub(super) written: AtomicU64,
    pub(super) charged: AtomicU64,
    // Protected by the file mutex. u64::MAX means unknown after an I/O error.
    // Other owners may close the file, but must not move its cursor.
    pub(super) position: AtomicU64,
}

/// One budget for the entire extraction, including parallel native workers.
/// Charge the greater of decoded bytes and the file's high-water extent, so
/// preallocation is not charged again when filled, but rewrites still count.
pub(super) struct OutputBudget {
    max_file: u64,
    max_total: u64,
    total: Mutex<u64>,
    exceeded: AtomicBool,
}

impl OutputBudget {
    pub(super) fn new(max_file: u64, max_total: u64) -> Self {
        Self {
            max_file,
            max_total,
            total: Mutex::new(0),
            exceeded: AtomicBool::new(false),
        }
    }

    pub(super) fn error(&self) -> Option<ArchiveError> {
        self.exceeded.load(Ordering::Acquire).then(|| {
            ArchiveError::LimitExceeded(
                "7z actual output exceeds the extraction byte limit".to_owned(),
            )
        })
    }

    fn reserve(&self, stream: &OutStream, amount: u64, extent: Option<u64>) -> bool {
        let mut total = self
            .total
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if self.exceeded.load(Ordering::Acquire) {
            return false;
        }
        let written = stream.written.load(Ordering::Relaxed).checked_add(amount);
        let previous = stream.charged.load(Ordering::Relaxed);
        let charge = written
            .zip(extent)
            .map(|(written, extent)| previous.max(written).max(extent));
        let next = charge.and_then(|charge| total.checked_add(charge - previous));
        if let (Some(written), Some(charge), Some(next)) = (written, charge, next)
            && charge <= self.max_file
            && next <= self.max_total
        {
            *total = next;
            stream.written.store(written, Ordering::Relaxed);
            stream.charged.store(charge, Ordering::Relaxed);
            return true;
        }
        self.exceeded.store(true, Ordering::Release);
        false
    }
}

#[cfg(test)]
mod output_limit_tests {
    use super::*;

    fn stream(budget: Arc<OutputBudget>) -> OutStream {
        OutStream {
            vtbl: &OUT_STREAM_VTBL,
            refs: AtomicU32::new(1),
            file: Arc::new(Mutex::new(Some(tempfile::tempfile().unwrap()))),
            budget: Some(budget),
            written: AtomicU64::new(0),
            charged: AtomicU64::new(0),
            position: AtomicU64::new(0),
        }
    }

    fn write(stream: &mut OutStream, size: u32) -> i32 {
        let data = vec![42u8; size as usize];
        let mut processed = u32::MAX;
        let hr = unsafe {
            out_stream_write(
                (stream as *mut OutStream).cast(),
                data.as_ptr().cast(),
                size,
                &mut processed,
            )
        };
        assert_eq!(processed, if hr == S_OK { size } else { 0 });
        hr
    }

    fn size(stream: &mut OutStream, size: u64) -> i32 {
        unsafe { out_stream_set_size((stream as *mut OutStream).cast(), size) }
    }

    fn seek(stream: &mut OutStream, position: i64) {
        assert_eq!(
            unsafe {
                out_stream_seek(
                    (stream as *mut OutStream).cast(),
                    position,
                    SEEK_SET,
                    ptr::null_mut(),
                )
            },
            S_OK
        );
    }

    fn length(stream: &OutStream) -> u64 {
        stream
            .file
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .metadata()
            .unwrap()
            .len()
    }

    #[test]
    fn native_output_counts_rewrites_and_rejects_before_io() {
        let budget = Arc::new(OutputBudget::new(8, 100));
        let mut output = stream(Arc::clone(&budget));
        assert_eq!(write(&mut output, 4), S_OK);
        seek(&mut output, 0);
        assert_eq!(write(&mut output, 4), S_OK);
        assert_eq!(write(&mut output, 1), E_ABORT);
        assert_eq!(length(&output), 4);
        assert!(matches!(
            budget.error(),
            Some(ArchiveError::LimitExceeded(_))
        ));
    }

    #[test]
    fn native_output_shares_total_across_threads() {
        let budget = Arc::new(OutputBudget::new(8, 8));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let budget = Arc::clone(&budget);
                std::thread::spawn(move || write(&mut stream(budget), 4))
            })
            .collect();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|&&hr| hr == S_OK).count(), 2);
        assert_eq!(results.iter().filter(|&&hr| hr == E_ABORT).count(), 2);
    }

    #[test]
    fn native_output_preallocation_is_not_double_charged_or_refunded() {
        let budget = Arc::new(OutputBudget::new(8, 8));
        let mut first = stream(Arc::clone(&budget));
        assert_eq!(size(&mut first, 8), S_OK);
        assert_eq!(write(&mut first, 8), S_OK);
        assert_eq!(size(&mut first, 0), S_OK);
        let mut second = stream(budget);
        assert_eq!(size(&mut second, 1), E_ABORT);
        assert_eq!(length(&second), 0);
    }

    #[test]
    fn native_output_rejects_oversized_set_size_and_sparse_write() {
        let mut output = stream(Arc::new(OutputBudget::new(8, 100)));
        assert_eq!(size(&mut output, 9), E_ABORT);
        assert_eq!(length(&output), 0);
        let mut output = stream(Arc::new(OutputBudget::new(8, 100)));
        seek(&mut output, 8);
        assert_eq!(write(&mut output, 1), E_ABORT);
        assert_eq!(length(&output), 0);
    }

    #[test]
    fn native_output_rejects_counter_overflow() {
        let mut output = stream(Arc::new(OutputBudget::new(u64::MAX, u64::MAX)));
        output.written.store(u64::MAX, Ordering::Relaxed);
        assert_eq!(write(&mut output, 1), E_ABORT);
        assert_eq!(length(&output), 0);
    }

    #[test]
    fn native_output_tracks_relative_end_and_unknown_positions() {
        let mut output = stream(Arc::new(OutputBudget::new(64, 64)));
        assert_eq!(write(&mut output, 8), S_OK);
        for (offset, origin, expected) in [(-3, SEEK_CUR, 5), (-2, SEEK_END, 6)] {
            let mut position = 0;
            assert_eq!(
                unsafe {
                    out_stream_seek(
                        (&mut output as *mut OutStream).cast(),
                        offset,
                        origin,
                        &mut position,
                    )
                },
                S_OK
            );
            assert_eq!(position, expected);
            assert_eq!(output.position.load(Ordering::Relaxed), expected);
        }
        // Exercise recovery after a write/seek failure invalidates the cache.
        output.position.store(u64::MAX, Ordering::Relaxed);
        assert_eq!(write(&mut output, 4), S_OK);
        assert_eq!(output.position.load(Ordering::Relaxed), 10);
        assert_eq!(length(&output), 10);
        assert_eq!(size(&mut output, 2), S_OK);
        assert_eq!(write(&mut output, 1), S_OK);
        assert_eq!(length(&output), 11);
    }
}

#[cfg(test)]
mod seek_performance_tests {
    use super::*;
    use std::io::Cursor;

    struct CountedInput {
        data: Cursor<Vec<u8>>,
        reads: usize,
        seeks: usize,
    }

    impl Read for CountedInput {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            self.data.read(bytes)
        }
    }

    impl Seek for CountedInput {
        fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
            self.seeks += 1;
            self.data.seek(from)
        }
    }

    #[test]
    fn repeated_small_reads_and_backward_seek_reuse_read_ahead() {
        let mut reader = BufReader::with_capacity(
            64,
            CountedInput {
                data: Cursor::new((0..128).collect()),
                reads: 0,
                seeks: 0,
            },
        );
        let mut cursor = Some(0);
        for target in [0, 8, 16, 24, 8, 16] {
            seek_buffered(&mut reader, &mut cursor, target).unwrap();
            let mut bytes = [0; 8];
            reader.read_exact(&mut bytes).unwrap();
            cursor = Some(target + 8);
            assert_eq!(
                bytes.to_vec(),
                (target as u8..target as u8 + 8).collect::<Vec<_>>()
            );
        }
        assert_eq!(reader.get_ref().reads, 1);
        assert_eq!(reader.get_ref().seeks, 0);
        seek_buffered(&mut reader, &mut cursor, 100).unwrap();
        let mut byte = [0];
        reader.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [100]);
        // Unknown positions force an absolute seek before reusing the stream.
        cursor = None;
        seek_buffered(&mut reader, &mut cursor, 3).unwrap();
        reader.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [3]);
    }

    #[test]
    fn split_output_revisits_volumes_and_preserves_position_after_resize() {
        let directory = tempfile::tempdir().unwrap();
        let mut output = VolumeOutput::new(directory.path().join("sample.7z"), 8);
        output.write(b"abcdefghijklmnopqrst").unwrap();
        output.seek(6, SEEK_SET).unwrap();
        output.write(b"123456").unwrap();
        output.set_size(22).unwrap();
        output.write(b"XY").unwrap();
        output.close_files();
        let contents: Vec<u8> = output
            .paths()
            .iter()
            .flat_map(|path| fs::read(path).unwrap())
            .collect();
        assert_eq!(contents, b"abcdef123456XYopqrst\0\0");
    }
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
    if let Some(budget) = &stream.budget {
        let position = match stream.position.load(Ordering::Relaxed) {
            u64::MAX => match file.stream_position() {
                Ok(position) => position,
                Err(_) => return E_FAIL,
            },
            position => position,
        };
        // Reserve before writing. Keep the reservation on an I/O failure,
        // because write_all may already have written part of the buffer.
        if !budget.reserve(
            stream,
            u64::from(size),
            position.checked_add(u64::from(size)),
        ) {
            return E_ABORT;
        }
        stream.position.store(position, Ordering::Relaxed);
    }
    match file.write_all(bytes) {
        Ok(()) => {
            let position = stream.position.load(Ordering::Relaxed);
            stream
                .position
                .store(position.saturating_add(u64::from(size)), Ordering::Relaxed);
            if !processed.is_null() {
                unsafe { *processed = size };
            }
            S_OK
        }
        Err(_) => {
            // write_all may have advanced the file before failing.
            stream.position.store(u64::MAX, Ordering::Relaxed);
            E_FAIL
        }
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
            Ok(position) => {
                stream.position.store(position, Ordering::Relaxed);
                position
            }
            Err(_) => {
                stream.position.store(u64::MAX, Ordering::Relaxed);
                return E_FAIL;
            }
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
    if let Some(budget) = &stream.budget
        && !budget.reserve(stream, 0, Some(size))
    {
        return E_ABORT;
    }
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
    position: Option<u64>,
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
                position: Some(0),
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
            let part = &mut self.parts[volume_index];
            let previous = part.position.take();
            let write_result = match part.file.as_mut() {
                Some(file) => (if previous == Some(local_position) {
                    Ok(local_position)
                } else {
                    file.seek(SeekFrom::Start(local_position))
                })
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
            part.position = Some(local_position + amount);
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

// Standard I/O adapter shared by the flate2 ZIP writer and the native stream.
impl std::io::Write for VolumeOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        VolumeOutput::write(self, bytes).map_err(|()| {
            std::io::Error::other(
                self.take_error()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "volume write failed".into()),
            )
        })?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        for part in &mut self.parts {
            if let Some(file) = &mut part.file {
                file.flush()?;
            }
        }
        Ok(())
    }
}

impl std::io::Seek for VolumeOutput {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let (offset, origin) = match from {
            SeekFrom::Start(value) => (
                i64::try_from(value).map_err(std::io::Error::other)?,
                SEEK_SET,
            ),
            SeekFrom::Current(value) => (value, SEEK_CUR),
            SeekFrom::End(value) => (value, SEEK_END),
        };
        VolumeOutput::seek(self, offset, origin).map_err(|()| {
            std::io::Error::other(
                self.take_error()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "volume seek failed".into()),
            )
        })
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
