//! High-level list, extract, create, and test orchestration.

use super::*;

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
        self.api.can_create(format)
    }

    pub fn can_read(&self, format: CreateFormat) -> bool {
        match format {
            CreateFormat::SevenZip => self.can_read_format(ReadFormat::SevenZip),
            CreateFormat::Zip => self.can_read_format(ReadFormat::Zip),
            _ => false,
        }
    }

    pub(super) fn can_read_format(&self, format: ReadFormat) -> bool {
        self.api.can_read(format)
    }

    /// Loads exactly the 7z.dll at `path`.
    pub fn load_from_path(path: &Path) -> ArchiveResult<Self> {
        if !path.is_absolute() {
            return Err(ArchiveError::InvalidInput(
                "7z.dll path must be absolute".to_owned(),
            ));
        }
        let canonical = fs::canonicalize(path).map_err(|error| ArchiveError::io(path, error))?;
        let metadata =
            fs::metadata(&canonical).map_err(|error| ArchiveError::io(&canonical, error))?;
        if !metadata.is_file() {
            return Err(ArchiveError::InvalidInput(format!(
                "7z.dll path is not a file: {}",
                canonical.display()
            )));
        }
        let library = DynamicLibrary::load(&canonical).map_err(ArchiveError::LibraryUnavailable)?;
        Ok(Self {
            api: Arc::new(Api::from_library(library)?),
        })
    }
}

fn read_all_entries(
    api: &Api,
    path: &Path,
    format: ReadFormat,
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

/// 7-Zip's LZH handler decodes legacy names with the process OEM code
/// page.  That is not necessarily the archive's code page (a Korean
/// Windows installation, for example, misdecodes Japanese Shift_JIS
/// names and can turn bytes into literal `?` characters).  The bundled
/// libarchive reader already exposes the raw pathname and the selected
/// legacy code page correctly, so use its metadata-only pass to repair
/// the names while keeping 7-Zip responsible for the reliable LZH data
/// decoder.
fn repair_lzh_paths(
    path: &Path,
    password: Option<&str>,
    pathname_codepage: u32,
    entries: &mut [ArchiveEntry],
    cancel: &CancellationToken,
) -> ArchiveResult<()> {
    let libarchive = LibArchiveEngine::load()?;
    let quiet_progress = |_: ProgressSnapshot| {};
    let listing = libarchive.list(path, password, pathname_codepage, &quiet_progress, cancel)?;
    if listing.entries.len() != entries.len() {
        return Err(ArchiveError::SevenZip(format!(
            "LZH readers disagree about the entry count (7z: {}, libarchive: {})",
            entries.len(),
            listing.entries.len()
        )));
    }
    for (entry, repaired) in entries.iter_mut().zip(listing.entries) {
        entry.path = repaired.path;
        entry.display_path = repaired.display_path;
    }
    Ok(())
}

fn run_extract_worker(
    api: &Api,
    archive: &Path,
    format: ReadFormat,
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
    let open_archive = open_for_read(api, archive, format, password, pathname_codepage, &cancel)?;
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
            ReadFormat::Zip,
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
                ReadFormat::Zip,
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
                if summary.warning.is_none() {
                    summary.warning = worker_summary.warning;
                }
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
        if self.api.can_create(CreateFormat::Zip) {
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
        let zip_name_records = if format == ReadFormat::Zip {
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
        if format == ReadFormat::Lzh {
            repair_lzh_paths(path, password, pathname_codepage, &mut entries, cancel)?;
        }
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
            warning: None,
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
        fs::create_dir_all(destination).map_err(|error| ArchiveError::io(destination, error))?;
        let root =
            fs::canonicalize(destination).map_err(|error| ArchiveError::io(destination, error))?;
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
        let zip_name_records = if format == ReadFormat::Zip {
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
        if format == ReadFormat::Lzh {
            repair_lzh_paths(
                archive,
                options.password.as_deref(),
                pathname_codepage,
                &mut entries,
                cancel,
            )?;
        }
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
        let summary = if format == ReadFormat::Zip && assume_targets_missing && indices.len() >= 256
        {
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
        if let Some(split_size) = options.split_size
            && split_size == 0
        {
            return Err(ArchiveError::InvalidInput(
                "split volume size must be greater than zero".to_owned(),
            ));
        }
        if options.encrypt_headers && options.format != CreateFormat::SevenZip {
            return Err(ArchiveError::UnsupportedOption(
                "header encryption is supported only for 7z archives".to_owned(),
            ));
        }
        if options.encrypt_headers
            && !options
                .password
                .as_deref()
                .is_some_and(|value| !value.is_empty())
        {
            return Err(ArchiveError::InvalidInput(
                "header encryption requires a non-empty password".to_owned(),
            ));
        }
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .ok_or_else(|| {
                ArchiveError::InvalidInput("destination has no parent directory".to_owned())
            })?;
        fs::create_dir_all(parent).map_err(|error| ArchiveError::io(parent, error))?;
        let parent = fs::canonicalize(parent).map_err(|error| ArchiveError::io(parent, error))?;
        let file_name = destination
            .file_name()
            .ok_or_else(|| ArchiveError::InvalidInput("destination has no file name".to_owned()))?;
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
        let file_count = items
            .iter()
            .filter(|item| item.kind == SourceKind::File)
            .count() as u64;
        let item_count = u32::try_from(items.len())
            .map_err(|_| ArchiveError::LimitExceeded("too many entries".to_owned()))?;

        // SAFETY: the reference is only used by callbacks while this
        // function is running, and the shared context is dropped here
        // before the borrow ends, so extending the lifetime is sound.
        let progress: &'static dyn ProgressSink = unsafe { mem::transmute(progress) };
        let throttled = Arc::new(ThrottledProgress::new(progress, PROGRESS_INTERVAL));
        let mut opening = ProgressSnapshot::new(ProgressPhase::Opening);
        opening.total_entries = Some(file_count);
        opening.total_bytes = Some(total_bytes);
        opening.current_file = final_destination.display().to_string();
        throttled.report(opening, true);

        let temporary_path = temporary_path(&parent, &final_destination);
        let mut temp_file = if options.split_size.is_some() {
            None
        } else {
            Some(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary_path)
                    .map_err(|error| ArchiveError::io(&temporary_path, error))?,
            )
        };

        let out_archive = match self.api.create_out_archive(options.format) {
            Ok(archive) => archive,
            Err(error) => {
                drop(temp_file.take());
                return Err(error);
            }
        };
        let mut temporary_volumes: Option<Arc<Mutex<VolumeOutput>>> = None;
        let result: ArchiveResult<OperationSummary> = (|| {
            match apply_create_properties(&out_archive, options) {
                Ok(()) => {
                    let volume_output = options.split_size.map(|size| {
                        Arc::new(Mutex::new(VolumeOutput::new(temporary_path.clone(), size)))
                    });
                    temporary_volumes = volume_output.clone();
                    let shared_file: Option<Arc<Mutex<Option<File>>>> = if volume_output.is_none() {
                        Some(Arc::new(Mutex::new(Some(
                            temp_file.take().expect("temporary file is live"),
                        ))))
                    } else {
                        None
                    };
                    let stream_ptr = if let Some(output) = &volume_output {
                        let stream = Box::new(VolumeOutStream {
                            vtbl: &VOLUME_OUT_STREAM_VTBL,
                            refs: AtomicU32::new(1),
                            output: Arc::clone(output),
                        });
                        Box::into_raw(stream).cast::<c_void>()
                    } else {
                        let stream = Box::new(OutStream {
                            vtbl: &OUT_STREAM_VTBL,
                            refs: AtomicU32::new(1),
                            file: Arc::clone(
                                shared_file.as_ref().expect("single-volume file is live"),
                            ),
                        });
                        Box::into_raw(stream).cast::<c_void>()
                    };
                    let mut snapshot = ProgressSnapshot::new(ProgressPhase::Compressing);
                    snapshot.total_entries = Some(file_count);
                    snapshot.total_bytes = Some(total_bytes);
                    let items = Arc::new(items);
                    let context = Arc::new(Mutex::new(UpdateContext {
                        total_bytes,
                        handler_total_bytes: None,
                        password: options.password.clone(),
                        current_item_index: None,
                        item_bytes_processed: vec![0; items.len()],
                        item_total_bytes: items.iter().map(|item| item.size).collect(),
                        item_is_file: items
                            .iter()
                            .map(|item| item.kind == SourceKind::File)
                            .collect(),
                        item_archive_names: items
                            .iter()
                            .map(|item| item.archive_name.clone())
                            .collect(),
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
                    let mut context = context.lock().unwrap_or_else(|poison| poison.into_inner());
                    let error = context.error.take();
                    let mut summary = context.summary.clone();
                    summary.bytes_processed = total_bytes;
                    summary.entries_processed = file_count;
                    let mut snapshot = context.snapshot.clone();
                    drop(context);
                    if let Some(shared_file) = &shared_file {
                        // 7-Zip released the stream; close the file
                        // ourselves through the Arc we kept.
                        let mut guard = shared_file
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner());
                        if let Some(file) = guard.take() {
                            drop(file);
                        }
                    }
                    if let Some(output) = &volume_output {
                        let mut output = output.lock().unwrap_or_else(|poison| poison.into_inner());
                        output.close_files();
                        if let Some(error) = output.take_error() {
                            return create_error(error);
                        }
                    }
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
                    if let Some(output) = &volume_output {
                        let paths = output
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .paths();
                        if paths.is_empty() {
                            return create_error(ArchiveError::SevenZip(
                                "7z creation produced no output volumes".to_owned(),
                            ));
                        }
                        if let Err(error) =
                            install_temporary_volumes(&parent, &paths, &final_destination)
                        {
                            return create_error(error);
                        }
                    } else if let Err(error) =
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
                    // Close the still-owned single-volume file before the
                    // archive is released. Temporary volume files are
                    // created lazily by the output stream.
                    drop(temp_file.take());
                    create_error(error)
                }
            }
        })();
        drop(out_archive);
        if result.is_err() {
            // 7-Zip may retain the output stream until the archive object
            // is released. Remove paths only after that release so a
            // cancelled operation cannot strand temporary files.
            if let Some(output) = temporary_volumes {
                let mut output = output.lock().unwrap_or_else(|poison| poison.into_inner());
                output.close_files();
                for path in output.paths() {
                    let _ = fs::remove_file(path);
                }
            } else {
                let _ = fs::remove_file(&temporary_path);
            }
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

struct OpenArchive {
    in_archive: RawInArchive,
    /// The input stream handed to 7-Zip. Its reference count includes one
    /// reference held by this owner; 7-Zip releases its own reference in
    /// Close, so the holder must outlive `in_archive`.
    _stream: InputStream,
    _callback: OpenCallback,
}

enum InputStream {
    Single(Box<InStream>),
    Multi(Box<MultiInStream>),
}

impl InputStream {
    fn as_ptr(&self) -> *mut c_void {
        match self {
            Self::Single(stream) => (stream.as_ref() as *const InStream)
                .cast_mut()
                .cast::<c_void>(),
            Self::Multi(stream) => (stream.as_ref() as *const MultiInStream)
                .cast_mut()
                .cast::<c_void>(),
        }
    }
}

fn open_input_stream(path: &Path) -> ArchiveResult<InStream> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
        .open(path)
        .map_err(|error| ArchiveError::io(path, error))?;
    Ok(InStream {
        vtbl: &IN_STREAM_VTBL,
        refs: AtomicU32::new(2),
        file: Mutex::new(BufReader::with_capacity(STREAM_BUFFER_SIZE, file)),
        position: AtomicU64::new(0),
        progress: None,
    })
}

fn open_for_read(
    api: &Api,
    path: &Path,
    format: ReadFormat,
    password: Option<&str>,
    _pathname_codepage: u32,
    cancel: &CancellationToken,
) -> ArchiveResult<OpenArchive> {
    let in_archive = api.create_in_archive(format.base())?;
    let stream = if format.is_volume() {
        let paths = split_volume_paths(path).ok_or_else(|| {
            ArchiveError::InvalidInput(format!(
                "split archive volume is missing its .001 file: {}",
                path.display()
            ))
        })?;
        if paths.len() > 1 {
            InputStream::Multi(Box::new(MultiInStream::open(&paths)?))
        } else {
            InputStream::Single(Box::new(open_input_stream(path)?))
        }
    } else {
        InputStream::Single(Box::new(open_input_stream(path)?))
    };
    let callback = OpenCallback {
        vtbl: &OPEN_VTBL,
        crypto_vtbl: &CRYPTO_GET_TEXT_PASSWORD_VTBL,
        refs: AtomicU32::new(1),
        password: Mutex::new(password.map(str::to_owned)),
        password_requested: AtomicBool::new(false),
        cancel: cancel.clone(),
    };
    let hr = in_archive.open(
        stream.as_ptr(),
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
            checked_add_with_limit(0, size, MAX_LIST_DECLARED_BYTES, "7z listing declared size")?;
        }

        let mut packed_size_prop = PropVariant::empty();
        require_hr(
            in_archive.get_property(index, KPID_PACK_SIZE, &mut packed_size_prop),
            "reading 7z entry packed size",
        )?;
        let compressed_size = packed_size_prop.as_u64();
        packed_size_prop.clear();
        if let Some(compressed_size) = compressed_size {
            checked_add_with_limit(
                0,
                compressed_size,
                MAX_LIST_DECLARED_BYTES,
                "7z listing declared packed size",
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
            compressed_size,
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
        if options.encrypt_headers {
            push("he", PropVariant::bstr("on"));
        }
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
