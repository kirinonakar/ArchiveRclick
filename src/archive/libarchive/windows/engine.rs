//! Public archive engine orchestration for listing, extraction, creation, and testing.

use super::*;

#[derive(Clone)]
pub struct LibArchiveEngine {
    api: Arc<Api>,
    version: Arc<str>,
}

impl LibArchiveEngine {
    pub fn load() -> ArchiveResult<Self> {
        Ok(Self::from_api(load_api()?))
    }

    /// Loads exactly the libarchive DLL at `path`.
    ///
    /// Unlike [`Self::load`], this constructor never examines environment
    /// variables, application-local conventional names, or system DLLs.
    /// The explicit DLL must satisfy the normal supported-version policy.
    pub fn load_from_path(path: &Path) -> ArchiveResult<Self> {
        let path = canonical_library_file(path)?;
        let library =
            DynamicLibrary::load(&path, false).map_err(ArchiveError::LibraryUnavailable)?;
        Ok(Self::from_api(Api::from_library(library, false, false)?))
    }

    fn from_api(api: Api) -> Self {
        let api = Arc::new(api);
        let version: Arc<str> = api.version().into();
        Self { api, version }
    }
}

pub fn load() -> ArchiveResult<LibArchiveEngine> {
    LibArchiveEngine::load()
}

pub(super) fn is_iso_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "iso" | "img"))
}

pub(super) fn is_lha_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "lha" | "lzh"))
}

fn effective_lha_codepage(path: &Path, requested: u32) -> u32 {
    // LHA/LZH archives in the wild conventionally store Japanese names
    // as Shift_JIS.  The generic detector can mistake long Japanese
    // names for CP949 when the UI is set to Auto, so keep Auto useful for
    // other formats while giving this legacy Japanese format its stable
    // default.  An explicit user choice still wins.
    if is_lha_path(path) && requested == 0 {
        932
    } else {
        requested
    }
}

fn is_recoverable_archive_corruption(error: &ArchiveError) -> bool {
    let ArchiveError::LibArchive { operation, message } = error else {
        return false;
    };
    if !matches!(
        *operation,
        "reading archive header" | "reading archive entry data" | "closing archive"
    ) {
        return false;
    }
    let message = message.to_ascii_lowercase();
    [
        "bad lzh data",
        "data error",
        "truncated",
        "unexpected end",
        "invalid header",
        "checksum",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

fn partial_warning(error: &ArchiveError) -> String {
    format!("partial archive: {error}")
}

fn archive_path_components(path: &Path) -> Vec<String> {
    path.to_string_lossy()
        .replace('\\', "/")
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .map(str::to_owned)
        .collect()
}

fn archive_entry_from_raw(index: u64, entry: RawEntryInfo) -> ArchiveEntry {
    ArchiveEntry {
        index,
        path: entry.path,
        display_path: entry.display_path,
        size: entry.size,
        compressed_size: None,
        modified_unix_seconds: entry.modified_unix_seconds,
        kind: entry.kind,
        encrypted: entry.encrypted,
    }
}

fn synthetic_directory_entry(index: u64, components: &[String]) -> ArchiveEntry {
    let path = components
        .iter()
        .fold(PathBuf::new(), |mut path, component| {
            path.push(component);
            path
        });
    ArchiveEntry {
        index,
        path,
        display_path: components.join("/"),
        size: None,
        compressed_size: None,
        modified_unix_seconds: None,
        kind: ArchiveEntryKind::Directory,
        encrypted: false,
    }
}

fn insert_iso_child(
    entries: &mut Vec<ArchiveEntry>,
    indices: &mut HashMap<String, usize>,
    key: String,
    entry: ArchiveEntry,
) {
    if let Some(&index) = indices.get(&key) {
        // A synthetic directory can be replaced by the real directory
        // header when libarchive reaches it later in the ISO stream.
        if entries[index].size.is_none() && entries[index].kind == ArchiveEntryKind::Directory {
            let mut entry = entry;
            entry.index = index as u64;
            entries[index] = entry;
        }
        return;
    }
    indices.insert(key, entries.len());
    entries.push(entry);
}

/// Reads only the current directory's metadata for ISO images.  The
/// normal listing path intentionally drains file data to validate every
/// entry; that is unnecessary for the first browse view and makes opening
/// a large image scale with its entire payload.  libarchive automatically
/// skips unread data when the next header is requested, so this pass reads
/// headers and names without copying file contents into Rust.
fn list_iso_directory(
    api: &Api,
    path: &Path,
    directory: &Path,
    password: Option<&str>,
    pathname_codepage: u32,
    progress: &dyn ProgressSink,
    cancel: &CancellationToken,
) -> ArchiveResult<ArchiveListing> {
    check_cancel(cancel)?;
    let total_input = file_length(path)?;
    let throttled = ThrottledProgress::new(progress, PROGRESS_INTERVAL);
    throttled.report(opening_snapshot(path, total_input), true);
    let mut reader = Reader::open(api, path, password)?;
    let scope = archive_path_components(directory);
    let mut entries = Vec::new();
    let mut indices = HashMap::<String, usize>::new();
    let mut total_uncompressed_size = 0u64;
    let mut total_path_bytes = 0u64;
    let mut scan = ScanBudget::new(
        MAX_LIST_ENTRIES,
        MAX_LIST_SCAN_DECODED_BYTES,
        "ISO directory listing",
    );
    let mut snapshot = ProgressSnapshot::new(ProgressPhase::Listing);
    snapshot.total_bytes = Some(total_input);

    while let Some(entry) = {
        check_cancel(cancel)?;
        reader.next_entry(pathname_codepage)?
    } {
        scan.visit_entry()?;
        let path_bytes = u64::try_from(entry.display_path.encode_utf16().count())
            .unwrap_or(u64::MAX)
            .saturating_mul(2);
        total_path_bytes = checked_add_with_limit(
            total_path_bytes,
            path_bytes,
            MAX_LIST_PATH_BYTES,
            "ISO directory listing pathname metadata",
        )?;

        let components = archive_path_components(&entry.path);
        let display_path = entry.display_path.clone();
        if components.len() >= scope.len() && components[..scope.len()] == scope[..] {
            let relative = &components[scope.len()..];
            if !relative.is_empty() {
                if let Some(size) = entry.size {
                    total_uncompressed_size = checked_add_with_limit(
                        total_uncompressed_size,
                        size,
                        MAX_LIST_DECLARED_BYTES,
                        "ISO directory listing declared size",
                    )?;
                }

                let child_components = &components[..scope.len() + 1];
                let child_key = child_components.join("/");
                if relative.len() == 1 {
                    let index = entries.len() as u64;
                    insert_iso_child(
                        &mut entries,
                        &mut indices,
                        child_key,
                        archive_entry_from_raw(index, entry),
                    );
                } else {
                    let index = entries.len() as u64;
                    insert_iso_child(
                        &mut entries,
                        &mut indices,
                        child_key,
                        synthetic_directory_entry(index, child_components),
                    );
                }
            }
        }

        snapshot.current_file = display_path;
        snapshot.entries_processed = scan.entries_visited;
        snapshot.bytes_processed = reader.consumed_bytes().min(total_input);
        throttled.report(snapshot.clone(), false);
    }

    let format_name = reader.format_name();
    let filter_name = reader.filter_name();
    let archive_encrypted = reader.has_encrypted_entries();
    if archive_encrypted {
        for entry in &mut entries {
            entry.encrypted = true;
        }
    }
    reader.finish()?;
    snapshot.phase = ProgressPhase::Finished;
    snapshot.current_file.clear();
    snapshot.bytes_processed = total_input;
    snapshot.entries_processed = entries.len() as u64;
    throttled.report(snapshot, true);
    Ok(ArchiveListing {
        archive_path: path.to_path_buf(),
        format_name,
        filter_name,
        warning: None,
        entries,
        total_uncompressed_size,
    })
}

impl ArchiveEngine for LibArchiveEngine {
    fn version(&self) -> String {
        self.version.to_string()
    }

    fn writable_formats(&self) -> Vec<CreateFormat> {
        let Some(write) = self.api.write.as_ref() else {
            return Vec::new();
        };
        let mut formats = vec![CreateFormat::Zip, CreateFormat::Tar];
        if write.set_format_7zip.is_some() {
            formats.push(CreateFormat::SevenZip);
        }
        if write.add_filter_gzip.is_some() {
            formats.push(CreateFormat::TarGzip);
        }
        if write.add_filter_xz.is_some() {
            formats.push(CreateFormat::TarXz);
        }
        if write.add_filter_zstd.is_some() {
            formats.push(CreateFormat::TarZstd);
        }
        formats
    }

    fn list_directory(
        &self,
        path: &Path,
        directory: &Path,
        password: Option<&str>,
        pathname_codepage: u32,
        progress: &dyn ProgressSink,
        cancel: &CancellationToken,
    ) -> ArchiveResult<ArchiveListing> {
        if is_iso_path(path) {
            return list_iso_directory(
                &self.api,
                path,
                directory,
                password,
                pathname_codepage,
                progress,
                cancel,
            );
        }
        self.list(path, password, pathname_codepage, progress, cancel)
            .map(|listing| listing.restrict_to_directory(directory))
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
        let pathname_codepage = effective_lha_codepage(path, pathname_codepage);
        let throttled = ThrottledProgress::new(progress, PROGRESS_INTERVAL);
        throttled.report(opening_snapshot(path, total_input), true);
        let mut reader = Reader::open(&self.api, path, password)?;
        let mut entries = Vec::new();
        let mut total_uncompressed_size = 0u64;
        let mut total_path_bytes = 0u64;
        let mut scan = ScanBudget::new(
            MAX_LIST_ENTRIES,
            MAX_LIST_SCAN_DECODED_BYTES,
            "archive listing",
        );
        let mut snapshot = ProgressSnapshot::new(ProgressPhase::Listing);
        snapshot.total_bytes = Some(total_input);
        let mut warning = None;

        loop {
            check_cancel(cancel)?;
            let entry = match reader.next_entry(pathname_codepage) {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(error) if !entries.is_empty() && is_recoverable_archive_corruption(&error) => {
                    warning = Some(partial_warning(&error));
                    break;
                }
                Err(error) => return Err(error),
            };
            let index = entries.len() as u64;
            scan.visit_entry()?;
            let path_bytes = u64::try_from(entry.display_path.encode_utf16().count())
                .unwrap_or(u64::MAX)
                .saturating_mul(2);
            total_path_bytes = checked_add_with_limit(
                total_path_bytes,
                path_bytes,
                MAX_LIST_PATH_BYTES,
                "archive listing pathname metadata",
            )?;
            if let Some(size) = entry.size {
                total_uncompressed_size = checked_add_with_limit(
                    total_uncompressed_size,
                    size,
                    MAX_LIST_DECLARED_BYTES,
                    "archive listing declared size",
                )?;
            }
            snapshot.current_file.clone_from(&entry.display_path);
            snapshot.entries_processed = index + 1;
            // Listing is metadata-only. Asking libarchive to decompress
            // every file here turns a damaged payload into an all-or-
            // nothing open failure and needlessly exercises the codec
            // before the user asks to extract or test it. next_header()
            // still advances safely; a later parser error becomes a
            // partial listing.
            snapshot.bytes_processed = reader.consumed_bytes().min(total_input);
            throttled.report(snapshot.clone(), false);
            entries.push(ArchiveEntry {
                index,
                path: entry.path,
                display_path: entry.display_path,
                size: entry.size,
                compressed_size: None,
                modified_unix_seconds: entry.modified_unix_seconds,
                kind: entry.kind,
                encrypted: entry.encrypted,
            });
            // Raw compressed streams contain exactly one logical file.
            // Asking for another header makes libarchive drain and decode
            // the entire payload just to discover EOF; for multi-gigabyte
            // .tar.gz files that defeats the fast wrapper view.
            if reader.format_name().eq_ignore_ascii_case("raw") {
                break;
            }
        }

        let format_name = reader.format_name();
        let filter_name = reader.filter_name();
        if format_name.eq_ignore_ascii_case("raw") && entries.len() == 1 {
            let source_metadata = fs::metadata(path).ok();
            let entry = &mut entries[0];
            entry.size.get_or_insert(total_input);
            entry.compressed_size.get_or_insert(total_input);
            if entry.modified_unix_seconds.is_none() {
                entry.modified_unix_seconds = source_metadata
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(system_time_seconds);
            }
            total_uncompressed_size = entry.size.unwrap_or(total_input);
        }
        let archive_encrypted = reader.has_encrypted_entries();
        if archive_encrypted {
            for entry in &mut entries {
                // Header-encrypted formats cannot expose per-entry flags.  Once
                // the reader confirms archive encryption, conservatively mark
                // entries whose format did not expose the individual bit.
                entry.encrypted = true;
            }
        }
        if let Err(error) = reader.finish() {
            if entries.is_empty() || !is_recoverable_archive_corruption(&error) {
                return Err(error);
            }
            warning.get_or_insert_with(|| partial_warning(&error));
        }
        snapshot.phase = ProgressPhase::Finished;
        snapshot.current_file.clear();
        snapshot.bytes_processed = total_input;
        throttled.report(snapshot, true);
        Ok(ArchiveListing {
            archive_path: path.to_path_buf(),
            format_name,
            filter_name,
            warning,
            entries,
            total_uncompressed_size,
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
        fs::create_dir_all(destination).map_err(|error| ArchiveError::io(destination, error))?;
        let root =
            fs::canonicalize(destination).map_err(|error| ArchiveError::io(destination, error))?;
        verify_directory_handle(&root, &root)?;
        let total_input = file_length(archive)?;
        let pathname_codepage = effective_lha_codepage(archive, options.pathname_codepage);
        let throttled = ThrottledProgress::new(progress, PROGRESS_INTERVAL);
        throttled.report(opening_snapshot(archive, total_input), true);
        let mut reader = Reader::open(&self.api, archive, options.password.as_deref())?;
        let mut summary = OperationSummary::default();
        let mut selected_entries = 0u64;
        let mut progress_entries = 0u64;
        let mut progress_bytes = 0u64;
        let mut scan = ScanBudget::new(
            MAX_EXTRACT_SCAN_ENTRIES,
            MAX_EXTRACT_SCAN_DECODED_BYTES,
            "selective extraction",
        );
        let mut policy = RuntimeConflictPolicy::from(options.conflict_policy);
        let mut snapshot = ProgressSnapshot::new(ProgressPhase::Extracting);
        snapshot.total_entries = options.total_entries_hint;
        snapshot.total_bytes = options.total_bytes_hint;
        let mut buffer = vec![0u8; IO_BUFFER_SIZE];
        let mut warning = None;
        let mut visited_entries = 0u64;

        'entries: loop {
            check_cancel(cancel)?;
            let entry = match reader.next_entry(pathname_codepage) {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(error) if visited_entries > 0 && is_recoverable_archive_corruption(&error) => {
                    warning = Some(partial_warning(&error));
                    break;
                }
                Err(error) => return Err(error),
            };
            visited_entries = visited_entries.saturating_add(1);
            scan.visit_entry()?;
            let relative = safe_relative_path(&entry.path)?;
            if !options.selection.includes(&relative) {
                snapshot.current_file.clone_from(&entry.display_path);
                snapshot.current_file_total_bytes = entry
                    .size
                    .or_else(|| (entry.kind == ArchiveEntryKind::Directory).then_some(0));
                snapshot.current_file_bytes_processed = 0;
                if entry.kind == ArchiveEntryKind::File {
                    let drained = reader.drain_current_entry(
                        &mut buffer,
                        cancel,
                        &mut scan,
                        |entry_bytes, _| {
                            snapshot.current_file_bytes_processed = entry_bytes;
                            throttled.report(snapshot.clone(), false)
                        },
                    );
                    if let Err(error) = drained {
                        if is_recoverable_archive_corruption(&error) {
                            warning = Some(partial_warning(&error));
                            break 'entries;
                        }
                        return Err(error);
                    }
                }
                continue;
            }
            selected_entries = selected_entries
                .checked_add(1)
                .ok_or_else(|| ArchiveError::LimitExceeded("entry count overflow".to_owned()))?;
            if selected_entries > options.max_entries {
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
                if summary.bytes_processed.saturating_add(size) > options.max_total_bytes {
                    return Err(ArchiveError::LimitExceeded(
                        "declared extraction size exceeds the total limit".to_owned(),
                    ));
                }
            }

            snapshot.current_file.clone_from(&entry.display_path);
            let declared_progress_bytes = entry.size.unwrap_or(0);
            snapshot.current_file_total_bytes = entry
                .size
                .or_else(|| (entry.kind == ArchiveEntryKind::Directory).then_some(0));
            snapshot.current_file_bytes_processed = 0;
            let mut completed_progress_bytes = declared_progress_bytes;
            let target = root.join(&relative);
            ensure_no_reparse_ancestors(&root, &target)?;
            match entry.kind {
                ArchiveEntryKind::Directory => {
                    let action = prepare_directory(&root, &target, &mut policy, conflicts)?;
                    match action {
                        ConflictAction::Overwrite => summary.entries_processed += 1,
                        ConflictAction::Skip => summary.entries_skipped += 1,
                    }
                }
                ArchiveEntryKind::File => {
                    match resolve_existing(&target, &mut policy, conflicts)? {
                        ConflictAction::Skip => {
                            let drained = reader.drain_current_entry(
                                &mut buffer,
                                cancel,
                                &mut scan,
                                |entry_bytes, _| {
                                    snapshot.current_file_bytes_processed = entry_bytes;
                                    snapshot.bytes_processed =
                                        progress_bytes.saturating_add(entry_bytes);
                                    snapshot.entries_processed = progress_entries;
                                    throttled.report(snapshot.clone(), false);
                                },
                            );
                            let drained = match drained {
                                Ok(drained) => drained,
                                Err(error) if is_recoverable_archive_corruption(&error) => {
                                    warning = Some(partial_warning(&error));
                                    break 'entries;
                                }
                                Err(error) => return Err(error),
                            };
                            completed_progress_bytes = completed_progress_bytes.max(drained);
                            summary.entries_skipped += 1;
                        }
                        ConflictAction::Overwrite => {
                            ensure_parent_directories(&root, &target)?;
                            let mut temporary = temporary_file(
                                target.parent().expect("validated target has parent"),
                            )?;
                            verify_file_handle_within_root(
                                &root,
                                temporary.file(),
                                &temporary.path,
                            )?;
                            let mut file_bytes = 0u64;
                            loop {
                                check_cancel(cancel)?;
                                let amount = match reader.read(&mut buffer) {
                                    Ok(amount) => amount,
                                    Err(error) if is_recoverable_archive_corruption(&error) => {
                                        warning = Some(partial_warning(&error));
                                        break 'entries;
                                    }
                                    Err(error) => return Err(error),
                                };
                                if amount == 0 {
                                    break;
                                }
                                scan.account_decoded(amount)?;
                                file_bytes =
                                    file_bytes.checked_add(amount as u64).ok_or_else(|| {
                                        ArchiveError::LimitExceeded("file size overflow".to_owned())
                                    })?;
                                if file_bytes > options.max_file_bytes
                                    || summary.bytes_processed.saturating_add(file_bytes)
                                        > options.max_total_bytes
                                {
                                    return Err(ArchiveError::LimitExceeded(format!(
                                        "extracted data for {} exceeds configured limits",
                                        entry.display_path
                                    )));
                                }
                                temporary
                                    .file_mut()
                                    .write_all(&buffer[..amount])
                                    .map_err(|error| ArchiveError::io(&temporary.path, error))?;
                                snapshot.current_file_bytes_processed = file_bytes;
                                snapshot.bytes_processed =
                                    progress_bytes.saturating_add(file_bytes);
                                snapshot.entries_processed = progress_entries;
                                throttled.report(snapshot.clone(), false);
                            }
                            temporary
                                .file_mut()
                                .flush()
                                .map_err(|error| ArchiveError::io(&temporary.path, error))?;
                            temporary.close_file();
                            install_temporary(&root, &temporary.path, &target)?;
                            temporary.disarm();
                            summary.bytes_processed = summary
                                .bytes_processed
                                .checked_add(file_bytes)
                                .ok_or_else(|| {
                                    ArchiveError::LimitExceeded(
                                        "extracted byte count overflow".to_owned(),
                                    )
                                })?;
                            summary.entries_processed += 1;
                            completed_progress_bytes = completed_progress_bytes.max(file_bytes);
                        }
                    }
                }
                ArchiveEntryKind::Symlink
                | ArchiveEntryKind::Hardlink
                | ArchiveEntryKind::Other => {
                    return Err(ArchiveError::UnsafeEntryType(entry.display_path));
                }
            }
            progress_entries = progress_entries.checked_add(1).ok_or_else(|| {
                ArchiveError::LimitExceeded("progress entry count overflow".to_owned())
            })?;
            progress_bytes = progress_bytes
                .checked_add(completed_progress_bytes)
                .ok_or_else(|| {
                    ArchiveError::LimitExceeded("progress byte count overflow".to_owned())
                })?;
            snapshot.current_file_bytes_processed = completed_progress_bytes;
            snapshot.entries_processed = progress_entries;
            snapshot.bytes_processed = progress_bytes;
            throttled.report(snapshot.clone(), false);
        }

        if let Err(error) = reader.finish() {
            if warning.is_none() && visited_entries > 0 {
                if is_recoverable_archive_corruption(&error) {
                    warning = Some(partial_warning(&error));
                } else {
                    return Err(error);
                }
            } else if warning.is_none() {
                return Err(error);
            }
        }
        snapshot.phase = ProgressPhase::Finished;
        snapshot.current_file.clear();
        snapshot.current_file_bytes_processed = 0;
        snapshot.current_file_total_bytes = None;
        snapshot.entries_processed = options
            .total_entries_hint
            .unwrap_or(progress_entries)
            .max(progress_entries);
        snapshot.bytes_processed = options
            .total_bytes_hint
            .unwrap_or(progress_bytes)
            .max(progress_bytes);
        throttled.report(snapshot, true);
        summary.warning = warning;
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
        if options.split_size.is_some() {
            return Err(ArchiveError::UnsupportedOption(
                "split compression requires the bundled 7z backend".to_owned(),
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
        verify_directory_handle(&parent, &parent)?;
        let file_name = destination
            .file_name()
            .ok_or_else(|| ArchiveError::InvalidInput("destination has no file name".to_owned()))?;
        let final_destination = parent.join(file_name);
        ensure_no_reparse_ancestors(&parent, &final_destination)?;

        let (items, total_bytes) = collect_sources(files, &final_destination, cancel)?;
        let throttled = ThrottledProgress::new(progress, PROGRESS_INTERVAL);
        let mut opening = opening_snapshot(&final_destination, total_bytes);
        opening.total_entries = Some(items.len() as u64);
        throttled.report(opening, true);
        let mut temporary = temporary_file(&parent)?;
        verify_file_handle_within_root(&parent, temporary.file(), &temporary.path)?;
        temporary.close_file();
        let mut writer = Writer::create(&self.api, &temporary.path, options)?;
        let mut summary = OperationSummary::default();
        let mut snapshot = ProgressSnapshot::new(ProgressPhase::Compressing);
        snapshot.total_entries = Some(items.len() as u64);
        snapshot.total_bytes = Some(total_bytes);
        let mut buffer = vec![0u8; IO_BUFFER_SIZE];
        let mut next_progress_bytes = 0u64;

        for item in &items {
            check_cancel(cancel)?;
            snapshot.current_file.clone_from(&item.archive_name);
            snapshot.current_file_total_bytes = Some(item.size);
            snapshot.current_file_bytes_processed = 0;
            writer.write_header(item)?;
            if item.kind == SourceKind::File {
                let mut input = File::open(&item.source)
                    .map_err(|error| ArchiveError::io(&item.source, error))?;
                let mut remaining = item.size;
                while remaining > 0 {
                    check_cancel(cancel)?;
                    let wanted = usize::try_from(remaining.min(buffer.len() as u64))
                        .expect("buffer size fits usize");
                    let amount = input
                        .read(&mut buffer[..wanted])
                        .map_err(|error| ArchiveError::io(&item.source, error))?;
                    if amount == 0 {
                        return Err(ArchiveError::Io {
                            path: item.source.clone(),
                            source: std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "file changed while it was being archived",
                            ),
                        });
                    }
                    writer.write_all(&buffer[..amount])?;
                    remaining -= amount as u64;
                    snapshot.current_file_bytes_processed = item.size.saturating_sub(remaining);
                    summary.bytes_processed += amount as u64;
                    snapshot.bytes_processed = summary.bytes_processed;
                    snapshot.entries_processed = summary.entries_processed;
                    if summary.bytes_processed >= next_progress_bytes {
                        throttled.report(snapshot.clone(), false);
                        next_progress_bytes = summary
                            .bytes_processed
                            .saturating_add(IO_BUFFER_SIZE as u64);
                    }
                }
            }
            writer.finish_entry()?;
            summary.entries_processed += 1;
            snapshot.current_file_bytes_processed = item.size;
            snapshot.entries_processed = summary.entries_processed;
            throttled.report(snapshot.clone(), false);
        }

        writer.finish()?;
        drop(writer);
        install_temporary(&parent, &temporary.path, &final_destination)?;
        temporary.disarm();
        snapshot.phase = ProgressPhase::Finished;
        snapshot.current_file.clear();
        snapshot.current_file_bytes_processed = 0;
        snapshot.current_file_total_bytes = None;
        snapshot.entries_processed = summary.entries_processed;
        snapshot.bytes_processed = summary.bytes_processed;
        throttled.report(snapshot, true);
        Ok(summary)
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
        let throttled = ThrottledProgress::new(progress, PROGRESS_INTERVAL);
        throttled.report(opening_snapshot(archive, total_input), true);
        let mut reader = Reader::open(&self.api, archive, password)?;
        let pathname_codepage = effective_lha_codepage(archive, 0);
        let mut summary = OperationSummary::default();
        let mut snapshot = ProgressSnapshot::new(ProgressPhase::Testing);
        snapshot.total_bytes = Some(total_input);
        let mut buffer = vec![0u8; IO_BUFFER_SIZE];

        while let Some(entry) = {
            check_cancel(cancel)?;
            reader.next_entry(pathname_codepage)?
        } {
            enforce_limit(
                summary.entries_processed.saturating_add(1),
                MAX_TEST_ENTRIES,
                "archive test entry count",
            )?;
            snapshot.current_file = entry.display_path;
            if entry.kind == ArchiveEntryKind::File {
                loop {
                    check_cancel(cancel)?;
                    let amount = reader.read(&mut buffer)?;
                    if amount == 0 {
                        break;
                    }
                    summary.bytes_processed = checked_add_with_limit(
                        summary.bytes_processed,
                        amount as u64,
                        MAX_TEST_OUTPUT_BYTES,
                        "archive test decompressed data",
                    )?;
                    snapshot.bytes_processed = reader.consumed_bytes().min(total_input);
                    snapshot.entries_processed = summary.entries_processed;
                    throttled.report(snapshot.clone(), false);
                }
            }
            summary.entries_processed += 1;
            snapshot.entries_processed = summary.entries_processed;
            snapshot.bytes_processed = reader.consumed_bytes().min(total_input);
            throttled.report(snapshot.clone(), false);
        }
        reader.finish()?;
        snapshot.phase = ProgressPhase::Finished;
        snapshot.current_file.clear();
        snapshot.bytes_processed = total_input;
        snapshot.entries_processed = summary.entries_processed;
        throttled.report(snapshot, true);
        Ok(summary)
    }
}
