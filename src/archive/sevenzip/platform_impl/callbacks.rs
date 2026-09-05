//! Native callback objects and progress/conflict state machines.

use super::*;

// ------------------------------------------------------------------
// Callback objects implemented by this module
// ------------------------------------------------------------------

// --- IArchiveOpenCallback + ICryptoGetTextPassword -------------------
#[repr(C)]
pub(super) struct OpenVtbl {
    pub(super) query_interface: QueryInterfaceFn,
    pub(super) add_ref: AddRefFn,
    pub(super) release: ReleaseFn,
    pub(super) set_total: unsafe extern "system" fn(*mut c_void, *const u64, *const u64) -> i32,
    pub(super) set_completed: unsafe extern "system" fn(*mut c_void, *const u64, *const u64) -> i32,
}

// IArchiveOpenVolumeCallback is a separate IUnknown interface, not an
// extension of IArchiveOpenCallback. Its methods start at vtable slot 3.
#[repr(C)]
pub(super) struct OpenVolumeVtbl {
    query_interface: QueryInterfaceFn,
    add_ref: AddRefFn,
    release: ReleaseFn,
    pub(super) get_property: unsafe extern "system" fn(*mut c_void, u32, *mut PropVariant) -> i32,
    pub(super) get_stream:
        unsafe extern "system" fn(*mut c_void, *const u16, *mut *mut c_void) -> i32,
}

pub(super) static OPEN_VTBL: OpenVtbl = OpenVtbl {
    query_interface: open_query_interface,
    add_ref: open_callback_add_ref,
    release: open_callback_release,
    set_total: open_set_total,
    set_completed: open_set_completed,
};

pub(super) static OPEN_VOLUME_VTBL: OpenVolumeVtbl = OpenVolumeVtbl {
    query_interface: open_volume_query_interface,
    add_ref: open_volume_add_ref,
    release: open_volume_release,
    get_property: open_get_property,
    get_stream: open_get_stream,
};

#[repr(C)]
pub(super) struct CryptoGetTextPasswordVtbl {
    pub(super) query_interface: QueryInterfaceFn,
    pub(super) add_ref: AddRefFn,
    pub(super) release: ReleaseFn,
    pub(super) crypto_get_text_password:
        unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> i32,
}

pub(super) static CRYPTO_GET_TEXT_PASSWORD_VTBL: CryptoGetTextPasswordVtbl =
    CryptoGetTextPasswordVtbl {
        query_interface: crypto_query_interface,
        add_ref: open_crypto_add_ref,
        release: open_crypto_release,
        crypto_get_text_password: crypto_get_text_password,
    };

/// Crypto vtable for the extract callback: the shared open-callback
/// implementation reads `OpenCallback.password`, whose offset overlaps
/// `ExtractCallback.context`, so the extract callback needs its own entry
/// point that reads the password from its own context.
pub(super) static EXTRACT_CRYPTO_VTBL: CryptoGetTextPasswordVtbl = CryptoGetTextPasswordVtbl {
    query_interface: crypto_query_interface,
    add_ref: extract_crypto_add_ref,
    release: extract_crypto_release,
    crypto_get_text_password: extract_crypto_get_text_password,
};

#[repr(C)]
pub(super) struct OpenCallback {
    pub(super) vtbl: *const OpenVtbl,
    pub(super) crypto_vtbl: *const CryptoGetTextPasswordVtbl,
    pub(super) volume_vtbl: *const OpenVolumeVtbl,
    pub(super) refs: AtomicU32,
    pub(super) password: Mutex<Option<String>>,
    pub(super) password_requested: AtomicBool,
    pub(super) cancel: CancellationToken,
    pub(super) archive_path: PathBuf,
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
    } else if requested == IID_IARCHIVE_OPEN_VOLUME_CALLBACK {
        let volume = unsafe { ptr::addr_of_mut!((*(this as *mut OpenCallback)).volume_vtbl) };
        unsafe { *out = volume.cast() };
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
    this: *mut c_void,
    prop_id: u32,
    value: *mut PropVariant,
) -> i32 {
    if value.is_null() {
        return E_INVALIDARG;
    }
    unsafe { *value = PropVariant::empty() };
    let callback = unsafe { &*open_volume_base(this) };
    unsafe {
        *value = match prop_id {
            KPID_NAME => PropVariant::bstr(
                &callback
                    .archive_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
            ),
            KPID_IS_DIR => PropVariant::bool_value(false),
            KPID_SIZE => match fs::metadata(&callback.archive_path) {
                Ok(metadata) => PropVariant::u64_value(metadata.len()),
                Err(_) => return E_FAIL,
            },
            _ => PropVariant::empty(),
        };
    }
    S_OK
}

unsafe extern "system" fn open_get_stream(
    this: *mut c_void,
    name: *const u16,
    out: *mut *mut c_void,
) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    unsafe { *out = ptr::null_mut() };
    if name.is_null() {
        return E_INVALIDARG;
    }
    let callback = unsafe { &*open_volume_base(this) };
    if callback.cancel.is_cancelled() {
        return E_ABORT;
    }
    let mut length = 0;
    while length < MAX_ARCHIVE_PATH_UNITS && unsafe { *name.add(length) } != 0 {
        length += 1;
    }
    if length == MAX_ARCHIVE_PATH_UNITS {
        return E_INVALIDARG;
    }
    let name = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(name, length) });
    // Volume requests must stay alongside the archive, including on Windows
    // where drive prefixes, alternate streams and backslashes are special.
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', ':']) {
        return E_INVALIDARG;
    }
    let path = callback.archive_path.with_file_name(name);
    match engine::open_input_stream(&path) {
        Ok(mut stream) => {
            // Transfer the sole reference to 7-Zip; Release frees this Box.
            stream.refs = AtomicU32::new(1);
            unsafe { *out = Box::into_raw(Box::new(stream)).cast() };
            S_OK
        }
        Err(ArchiveError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            S_FALSE
        }
        Err(_) => E_FAIL,
    }
}

unsafe fn open_volume_base(this: *mut c_void) -> *mut OpenCallback {
    unsafe {
        this.cast::<u8>()
            .sub(mem::offset_of!(OpenCallback, volume_vtbl))
            .cast()
    }
}

unsafe extern "system" fn open_volume_query_interface(
    this: *mut c_void,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> i32 {
    unsafe { open_query_interface(open_volume_base(this).cast(), iid, out) }
}

unsafe extern "system" fn open_volume_add_ref(this: *mut c_void) -> u32 {
    unsafe { open_callback_add_ref(open_volume_base(this).cast()) }
}

unsafe extern "system" fn open_volume_release(this: *mut c_void) -> u32 {
    unsafe { open_callback_release(open_volume_base(this).cast()) }
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
pub(super) struct ExtractVtbl {
    pub(super) query_interface: QueryInterfaceFn,
    pub(super) add_ref: AddRefFn,
    pub(super) release: ReleaseFn,
    pub(super) set_total: unsafe extern "system" fn(*mut c_void, u64) -> i32,
    pub(super) set_completed: unsafe extern "system" fn(*mut c_void, *const u64) -> i32,
    pub(super) get_stream:
        unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void, i32) -> i32,
    pub(super) prepare_operation: unsafe extern "system" fn(*mut c_void, i32) -> i32,
    pub(super) set_operation_result: unsafe extern "system" fn(*mut c_void, i32) -> i32,
}

pub(super) static EXTRACT_VTBL: ExtractVtbl = ExtractVtbl {
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
pub(super) struct ExtractItem {
    pub(super) index: u32,
    pub(super) relative: PathBuf,
    pub(super) display_path: String,
    pub(super) is_dir: bool,
    pub(super) size: Option<u64>,
    pub(super) mtime_unix: Option<i64>,
}

pub(super) struct PendingFile {
    pub(super) temp_path: Option<PathBuf>,
    pub(super) target: PathBuf,
    /// Shared with the OutStream handed to 7-Zip; `None` once the file has
    /// been flushed and closed by SetOperationResult.
    pub(super) file: Arc<Mutex<Option<File>>>,
    pub(super) size: u64,
    pub(super) mtime_unix: Option<i64>,
    pub(super) armed: bool,
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
pub(super) fn split_callback_progress(
    reported_bytes: u64,
    current_file_base_bytes: u64,
    current_file_total_bytes: Option<u64>,
) -> (u64, u64) {
    let current_file_bytes = if reported_bytes >= current_file_base_bytes {
        reported_bytes - current_file_base_bytes
    } else {
        reported_bytes
    };
    let current_file_bytes =
        current_file_total_bytes.map_or(current_file_bytes, |total| current_file_bytes.min(total));
    (
        current_file_base_bytes.saturating_add(current_file_bytes),
        current_file_bytes,
    )
}

pub(super) struct ParallelWorkerProgress {
    pub(super) aggregate: Arc<ParallelProgress>,
    pub(super) worker_index: usize,
}

pub(super) struct ParallelProgress {
    pub(super) inner: Arc<ThrottledProgress<'static>>,
    pub(super) state: Mutex<ParallelProgressState>,
}

pub(super) struct ParallelProgressState {
    pub(super) sequence: u64,
    pub(super) workers: Vec<ParallelWorkerState>,
}

pub(super) struct ParallelWorkerState {
    pub(super) sequence: u64,
    pub(super) current_file: String,
    pub(super) current_file_bytes_processed: u64,
    pub(super) current_file_total_bytes: Option<u64>,
    pub(super) entries_processed: u64,
    pub(super) bytes_processed: u64,
    pub(super) phase: ProgressPhase,
}

impl ParallelProgress {
    pub(super) fn new(inner: Arc<ThrottledProgress<'static>>, worker_count: usize) -> Arc<Self> {
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
pub(super) enum RuntimePolicy {
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

pub(super) struct ExtractContext {
    pub(super) root: PathBuf,
    pub(super) prepared_dirs: HashSet<PathBuf>,
    pub(super) assume_targets_missing: bool,
    pub(super) policy: RuntimePolicy,
    // SAFETY invariant: this reference is only valid for the duration of
    // the enclosing extract() call; the callback never outlives it.
    pub(super) conflicts: &'static dyn ConflictResolver,
    pub(super) password: Option<String>,
    pub(super) password_requested: bool,
    pub(super) current_file_base_bytes: u64,
    pub(super) snapshot: ProgressSnapshot,
    pub(super) summary: OperationSummary,
    pub(super) pending: VecDeque<PendingFile>,
    pub(super) error: Option<ArchiveError>,
    pub(super) test_mode: bool,
}

#[repr(C)]
pub(super) struct ExtractCallback {
    pub(super) vtbl: *const ExtractVtbl,
    pub(super) crypto_vtbl: *const CryptoGetTextPasswordVtbl,
    pub(super) refs: AtomicU32,
    pub(super) items: Arc<Vec<ExtractItem>>,
    pub(super) selected: Arc<HashSet<u32>>,
    pub(super) cancel: CancellationToken,
    pub(super) progress: Arc<dyn ProgressSink>,
    pub(super) context: Arc<Mutex<ExtractContext>>,
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

unsafe extern "system" fn extract_set_completed(this: *mut c_void, complete: *const u64) -> i32 {
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
        context.snapshot.bytes_processed = context.snapshot.bytes_processed.max(bytes_processed);
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
            let current_file_total_bytes = context.snapshot.current_file_total_bytes.unwrap_or(0);
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
    context.summary.bytes_processed = context.summary.bytes_processed.saturating_add(pending.size);
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
pub(super) struct UpdateVtbl {
    pub(super) query_interface: QueryInterfaceFn,
    pub(super) add_ref: AddRefFn,
    pub(super) release: ReleaseFn,
    pub(super) set_total: unsafe extern "system" fn(*mut c_void, u64) -> i32,
    pub(super) set_completed: unsafe extern "system" fn(*mut c_void, *const u64) -> i32,
    pub(super) get_update_item_info:
        unsafe extern "system" fn(*mut c_void, u32, *mut i32, *mut i32, *mut u32) -> i32,
    pub(super) get_property:
        unsafe extern "system" fn(*mut c_void, u32, u32, *mut PropVariant) -> i32,
    pub(super) get_stream: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32,
    pub(super) set_operation_result: unsafe extern "system" fn(*mut c_void, i32) -> i32,
}

pub(super) static UPDATE_VTBL: UpdateVtbl = UpdateVtbl {
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
pub(super) struct CryptoGetTextPassword2Vtbl {
    pub(super) query_interface: QueryInterfaceFn,
    pub(super) add_ref: AddRefFn,
    pub(super) release: ReleaseFn,
    pub(super) crypto_get_text_password2:
        unsafe extern "system" fn(*mut c_void, *mut i32, *mut *mut u16) -> i32,
}

pub(super) static CRYPTO_GET_TEXT_PASSWORD2_VTBL: CryptoGetTextPassword2Vtbl =
    CryptoGetTextPassword2Vtbl {
        query_interface: crypto2_query_interface,
        add_ref: update_crypto_add_ref,
        release: update_crypto_release,
        crypto_get_text_password2: crypto_get_text_password2,
    };

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SourceKind {
    File,
    Directory,
}

#[derive(Clone)]
pub(super) struct SourceItem {
    pub(super) source: PathBuf,
    pub(super) archive_name: String,
    pub(super) kind: SourceKind,
    pub(super) size: u64,
    pub(super) modified_unix_seconds: Option<i64>,
}

pub(super) struct UpdateContext {
    pub(super) total_bytes: u64,
    pub(super) handler_total_bytes: Option<u64>,
    pub(super) password: Option<String>,
    pub(super) current_item_index: Option<usize>,
    pub(super) item_bytes_processed: Vec<u64>,
    pub(super) item_total_bytes: Vec<u64>,
    pub(super) progress_index: CompressionProgressIndex,
    pub(super) item_archive_names: Vec<String>,
    pub(super) snapshot: ProgressSnapshot,
    pub(super) summary: OperationSummary,
    pub(super) error: Option<ArchiveError>,
}

pub(super) struct UpdateInputProgress {
    pub(super) context: Arc<Mutex<UpdateContext>>,
    pub(super) sink: Arc<dyn ProgressSink>,
    pub(super) item_index: usize,
    pub(super) archive_name: String,
    pub(super) total_bytes: u64,
}

impl UpdateInputProgress {
    pub(super) fn report(&self, position: u64) {
        let position = position.min(self.total_bytes);
        let snapshot = {
            let mut context = self
                .context
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let (delta, processed) = {
                let Some(processed) = context.item_bytes_processed.get_mut(self.item_index) else {
                    return;
                };
                let delta = position.saturating_sub(*processed);
                *processed = (*processed).max(position);
                (delta, *processed)
            };

            // SetCompleted is the primary source because it keeps moving
            // while 7-Zip is doing CPU-heavy compression after reading an
            // input buffer. Use stream positions only as a fallback for a
            // handler that never supplies its own total.
            if context.handler_total_bytes.is_some() {
                return;
            }
            context.snapshot.bytes_processed = context
                .snapshot
                .bytes_processed
                .saturating_add(delta)
                .min(context.total_bytes);
            let current_item_complete = context.current_item_index.is_some_and(|index| {
                context
                    .item_bytes_processed
                    .get(index)
                    .copied()
                    .unwrap_or(0)
                    >= context.item_total_bytes.get(index).copied().unwrap_or(0)
            });
            let should_display = context.current_item_index.is_none()
                || context.current_item_index == Some(self.item_index)
                || context
                    .current_item_index
                    .is_some_and(|index| self.item_index < index)
                || current_item_complete;
            if should_display {
                context.current_item_index = Some(self.item_index);
                context.snapshot.current_file.clone_from(&self.archive_name);
                context.snapshot.current_file_total_bytes = Some(self.total_bytes);
                context.snapshot.current_file_bytes_processed = processed;
            }
            context.snapshot.clone()
        };
        self.sink.report(snapshot);
    }
}

#[repr(C)]
pub(super) struct UpdateCallback {
    pub(super) vtbl: *const UpdateVtbl,
    pub(super) crypto_vtbl: *const CryptoGetTextPassword2Vtbl,
    pub(super) refs: AtomicU32,
    pub(super) items: Arc<Vec<SourceItem>>,
    pub(super) cancel: CancellationToken,
    pub(super) progress: Arc<dyn ProgressSink>,
    pub(super) context: Arc<Mutex<UpdateContext>>,
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

unsafe extern "system" fn update_set_total(this: *mut c_void, total: u64) -> i32 {
    let callback = unsafe { &*(this as *const UpdateCallback) };
    let snapshot = {
        let mut context = callback
            .context
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // The input sizes collected before the update are the stable
        // denominator for the UI. Some handlers report a compressed or
        // output-side total through this callback.
        context.handler_total_bytes = (total > 0).then_some(total);
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
        let Some(handler_total) = context.handler_total_bytes.filter(|total| *total > 0) else {
            return S_OK;
        };
        let completed = unsafe { *complete }.min(handler_total);
        let source_completed = ((u128::from(completed) * u128::from(context.total_bytes))
            / u128::from(handler_total)) as u64;
        context.snapshot.bytes_processed = context
            .snapshot
            .bytes_processed
            .max(source_completed)
            .min(context.total_bytes);
        // Index lookups keep callback work logarithmic, even when the archive
        // contains hundreds of thousands of files. Use the monotonic counter
        // for both file and byte progress if codec workers report out of order.
        let source_completed = context.snapshot.bytes_processed;
        context.snapshot.entries_processed =
            context.progress_index.completed_files(source_completed);

        if source_completed > 0
            && let Some((item_index, item_bytes)) =
                context.progress_index.current_file(source_completed)
        {
            context.current_item_index = Some(item_index);
            context.snapshot.current_file = context
                .item_archive_names
                .get(item_index)
                .cloned()
                .unwrap_or_default();
            let item_total = context
                .item_total_bytes
                .get(item_index)
                .copied()
                .unwrap_or(0);
            context.snapshot.current_file_total_bytes = Some(item_total);
            context.snapshot.current_file_bytes_processed = item_bytes.min(item_total);
        }
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
    let item_index = index as usize;
    let total_bytes = item.size;
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
        position: AtomicU64::new(0),
        progress: Some(Arc::new(UpdateInputProgress {
            context: Arc::clone(&callback.context),
            sink: Arc::clone(&callback.progress),
            item_index,
            archive_name: item.archive_name.clone(),
            total_bytes,
        })),
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
    // Source-byte progress is accounted for by the individual input
    // streams. SetOperationResult carries no item index, and 7-Zip can
    // already have requested the next stream when this arrives, so using
    // the currently displayed item here would charge bytes to the wrong
    // file.
    context.summary.bytes_processed = context.snapshot.bytes_processed;
    if context.handler_total_bytes.is_none() {
        context.snapshot.entries_processed = context.summary.entries_processed;
    }
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

pub(super) fn close_pending_file(pending: &PendingFile) {
    let mut guard = pending
        .file
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    drop(guard.take());
}
