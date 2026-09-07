//! Selected flate2 ZIP decoding, with shared metadata and bounded parallel I/O.
use super::*;
use crate::archive::{ZipBackend, libarchive::output};
use rayon::prelude::*;
use std::{io, os::windows::fs::FileExt};
use zip::{CompressionMethod, ZipArchive, read::ZipReadOptions};

// Clones share immutable handles, but maintain independent logical positions.
// Explicit-offset reads avoid races on the OS file cursor and central-directory
// reparsing. Denying writes/deletes keeps the metadata and payload consistent.
#[derive(Clone, Debug)]
struct ZipInput {
    files: Arc<Vec<File>>,
    starts: Arc<Vec<u64>>,
    length: u64,
    position: u64,
    cancel: CancellationToken,
}

impl ZipInput {
    fn open(path: &Path, cancel: &CancellationToken) -> ArchiveResult<Self> {
        let paths = split_volume_paths(path).unwrap_or_else(|| vec![path.to_owned()]);
        let mut files = Vec::new();
        let mut starts = Vec::new();
        let mut length = 0u64;
        for part in paths {
            check_cancel(cancel)?;
            let file = OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(&part)
                .map_err(|e| ArchiveError::io(&part, e))?;
            let size = file
                .metadata()
                .map_err(|e| ArchiveError::io(&part, e))?
                .len();
            starts.push(length);
            length = length
                .checked_add(size)
                .ok_or_else(|| ArchiveError::LimitExceeded("ZIP volume size overflow".into()))?;
            files.push(file);
        }
        Ok(Self {
            files: Arc::new(files),
            starts: Arc::new(starts),
            length,
            position: 0,
            cancel: cancel.clone(),
        })
    }
}

impl Read for ZipInput {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(io::Error::other("operation cancelled"));
        }
        if bytes.is_empty() || self.position >= self.length {
            return Ok(0);
        }
        let index = self
            .starts
            .partition_point(|start| *start <= self.position)
            .saturating_sub(1);
        let end = self.starts.get(index + 1).copied().unwrap_or(self.length);
        let count = bytes
            .len()
            .min((end - self.position).min(usize::MAX as u64) as usize);
        let amount =
            self.files[index].seek_read(&mut bytes[..count], self.position - self.starts[index])?;
        self.position += amount as u64;
        Ok(amount)
    }
}

impl Seek for ZipInput {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(value) => value as i128,
            SeekFrom::Current(value) => self.position as i128 + value as i128,
            SeekFrom::End(value) => self.length as i128 + value as i128,
        };
        self.position =
            u64::try_from(position).map_err(|_| io::Error::other("invalid ZIP seek"))?;
        Ok(self.position)
    }
}

fn zip_error(path: &Path, error: zip::result::ZipError) -> ArchiveError {
    use zip::result::ZipError;
    match error {
        ZipError::InvalidPassword => ArchiveError::PasswordRequired,
        ZipError::UnsupportedArchive(message) if message == ZipError::PASSWORD_REQUIRED => {
            ArchiveError::PasswordRequired
        }
        ZipError::UnsupportedArchive(message) if message.starts_with("ZIP metadata") => {
            ArchiveError::LimitExceeded(message.into())
        }
        ZipError::UnsupportedArchive(message) => ArchiveError::UnsupportedOption(message.into()),
        ZipError::Io(error) => ArchiveError::io(path, error),
        _ => ArchiveError::InvalidArchive(path.to_owned()),
    }
}

#[derive(Clone)]
struct Job {
    index: usize,
    relative: PathBuf,
    size: u64,
    directory: bool,
    modified: Option<SystemTime>,
}

fn modified_time(stamp: zip::DateTime) -> Option<SystemTime> {
    use windows::Win32::{
        Foundation::{FILETIME, SYSTEMTIME},
        System::Time::{SystemTimeToFileTime, TzSpecificLocalTimeToSystemTime},
    };
    let local = SYSTEMTIME {
        wYear: stamp.year(),
        wMonth: stamp.month() as u16,
        wDay: stamp.day() as u16,
        wHour: stamp.hour() as u16,
        wMinute: stamp.minute() as u16,
        wSecond: stamp.second() as u16,
        ..Default::default()
    };
    let mut utc = SYSTEMTIME::default();
    let mut filetime = FILETIME::default();
    // SAFETY: all inputs and output structures remain live for these calls.
    unsafe {
        TzSpecificLocalTimeToSystemTime(None, &local, &mut utc).ok()?;
        SystemTimeToFileTime(&utc, &mut filetime).ok()?;
    }
    let ticks = (u64::from(filetime.dwHighDateTime) << 32) | u64::from(filetime.dwLowDateTime);
    let seconds = ticks / 10_000_000;
    if seconds >= FILETIME_EPOCH_SECONDS as u64 {
        UNIX_EPOCH.checked_add(Duration::from_secs(seconds - FILETIME_EPOCH_SECONDS as u64))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(FILETIME_EPOCH_SECONDS as u64 - seconds))
    }
}

struct ExtractProgress<'a> {
    snapshot: Mutex<ProgressSnapshot>,
    sink: ThrottledProgress<'a>,
    max_bytes: u64,
    stop: AtomicBool,
}

impl ExtractProgress<'_> {
    fn advance(&self, job: &Job, current: u64, amount: u64, finished: bool) -> ArchiveResult<()> {
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|p| p.into_inner());
        snapshot.bytes_processed = checked_add_with_limit(
            snapshot.bytes_processed,
            amount,
            self.max_bytes,
            "ZIP extracted bytes",
        )?;
        snapshot.current_file = job.relative.display().to_string();
        snapshot.current_file_bytes_processed = current;
        snapshot.current_file_total_bytes = Some(job.size);
        snapshot.entries_processed += u64::from(finished);
        self.sink.report(snapshot.clone(), false);
        Ok(())
    }
}

fn extract_file(
    archive: &mut ZipArchive<ZipInput>,
    root: &Path,
    job: &Job,
    options: &ExtractOptions,
    progress: &ExtractProgress<'_>,
    cancel: &CancellationToken,
    buffer: &mut [u8],
) -> ArchiveResult<()> {
    check_cancel(cancel)?;
    let target = root.join(&job.relative);
    let read_options = ZipReadOptions::new()
        .password(options.password.as_deref().map(str::as_bytes))
        .zlib_ng(options.zip_backend == ZipBackend::ZlibNg);
    let mut input = archive
        .by_index_with_options(job.index, read_options)
        .map_err(|e| zip_error(&target, e))?;
    // A new file can be streamed directly, with the same cleanup guard, avoiding
    // a temporary-name rename/fsync per tiny entry. Existing files always stage
    // separately so CRC failures and cancellation preserve their old contents.
    let (mut temporary, direct) = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
    {
        Ok(file) => (
            output::TemporaryPath {
                path: target.clone(),
                file: Some(file),
                armed: true,
            },
            true,
        ),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (
            output::temporary_file(target.parent().expect("validated target"))?,
            false,
        ),
        Err(error) => return Err(ArchiveError::io(&target, error)),
    };
    output::verify_file_handle_within_root(root, temporary.file(), &temporary.path)?;
    let mut current = 0u64;
    loop {
        check_cancel(cancel)?;
        if progress.stop.load(Ordering::Relaxed) {
            return Err(ArchiveError::Cancelled);
        }
        let amount = input
            .read(buffer)
            .map_err(|e| ArchiveError::io(&target, e))?;
        if amount == 0 {
            break;
        } // CRC/AES authentication must complete before installing.
        current = checked_add_with_limit(
            current,
            amount as u64,
            options.max_file_bytes,
            "ZIP file bytes",
        )?;
        if current > job.size {
            return Err(ArchiveError::InvalidArchive(target));
        }
        progress.advance(job, current, amount as u64, false)?;
        temporary
            .file_mut()
            .write_all(&buffer[..amount])
            .map_err(|e| ArchiveError::io(&temporary.path, e))?;
    }
    if current != job.size {
        return Err(ArchiveError::InvalidArchive(target));
    }
    if let Some(modified) = job.modified {
        temporary
            .file()
            .set_modified(modified)
            .map_err(|e| ArchiveError::io(&target, e))?;
    }
    check_cancel(cancel)?;
    if progress.stop.load(Ordering::Relaxed) {
        return Err(ArchiveError::Cancelled);
    }
    temporary.close_file();
    if !direct {
        output::install_temporary(root, &temporary.path, &target)?;
    }
    temporary.disarm();
    progress.advance(job, current, 0, true)
}

/// None requests the native fallback for ZIP methods other than STORE/DEFLATE.
/// Unsupported methods are detected before creating any output paths.
pub(super) fn extract(
    path: &Path,
    destination: &Path,
    options: &ExtractOptions,
    sink: &dyn ProgressSink,
    conflicts: &dyn ConflictResolver,
    cancel: &CancellationToken,
) -> ArchiveResult<Option<OperationSummary>> {
    check_cancel(cancel)?;
    let input = ZipInput::open(path, cancel)?;
    let archive = ZipArchive::with_config(
        zip::read::Config {
            max_entries: Some(MAX_LIST_ENTRIES as usize),
            max_metadata_bytes: Some(MAX_LIST_PATH_BYTES),
            ..Default::default()
        },
        input,
    )
    .map_err(|e| zip_error(path, e))?;
    let mut records = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        check_cancel(cancel)?;
        let entry = archive
            .by_index_metadata(index)
            .map_err(|e| zip_error(path, e))?;
        records.push(format::ZipNameRecord {
            raw_name: entry.name_raw().to_vec(),
            flags: if entry.has_unicode_name() { 0x800 } else { 0 },
            unicode_name: entry.has_unicode_name().then(|| entry.name().to_owned()),
        });
    }
    let codepage =
        effective_zip_codepage(ReadFormat::Zip, options.pathname_codepage, Some(&records));
    let mut jobs = Vec::new();
    let mut total = 0;
    for (index, record) in records.iter().enumerate() {
        check_cancel(cancel)?;
        let entry = archive
            .by_index_metadata(index)
            .map_err(|e| zip_error(path, e))?;
        let name =
            format::decode_zip_name(record, codepage).unwrap_or_else(|| entry.name().to_owned());
        let relative = safe_relative_path(Path::new(&name))?;
        if !options.selection.includes(&relative) {
            continue;
        }
        if entry.is_symlink()
            || entry
                .unix_mode()
                .is_some_and(|mode| !matches!(mode & 0o170000, 0 | 0o100000 | 0o040000))
        {
            return Err(ArchiveError::UnsafeEntryType(name));
        }
        if !matches!(
            entry.compression(),
            CompressionMethod::Stored | CompressionMethod::Deflated
        ) {
            return Ok(None);
        }
        if options.flatten_paths && entry.is_dir() {
            continue;
        }
        if jobs.len() as u64 >= options.max_entries || entry.size() > options.max_file_bytes {
            return Err(ArchiveError::LimitExceeded(
                "ZIP entry count or file size exceeds configured limits".into(),
            ));
        }
        total = checked_add_with_limit(
            total,
            entry.size(),
            options.max_total_bytes,
            "ZIP declared bytes",
        )?;
        jobs.push(Job {
            index,
            relative: if options.flatten_paths {
                PathBuf::from(relative.file_name().expect("validated path"))
            } else {
                relative
            },
            size: entry.size(),
            directory: entry.is_dir(),
            modified: entry.last_modified().and_then(modified_time),
        });
    }
    drop(records);
    ensure_no_reparse_ancestors(destination, destination)?;
    fs::create_dir_all(destination).map_err(|e| ArchiveError::io(destination, e))?;
    let root = fs::canonicalize(destination).map_err(|e| ArchiveError::io(destination, e))?;
    output::verified_root_final_path(&root)?;
    let mut snapshot = ProgressSnapshot::new(ProgressPhase::Extracting);
    snapshot.total_bytes = Some(total);
    snapshot.total_entries = Some(jobs.len() as u64);
    let progress = ExtractProgress {
        snapshot: Mutex::new(snapshot),
        sink: ThrottledProgress::new(sink, PROGRESS_INTERVAL),
        max_bytes: options.max_total_bytes,
        stop: AtomicBool::new(false),
    };
    // Flattened duplicate names and file/parent collisions must retain archive order.
    let mut targets = HashSet::new();
    let mut unique = true;
    let file_targets: HashSet<_> = jobs
        .iter()
        .filter(|j| !j.directory)
        .map(|j| j.relative.to_string_lossy().to_lowercase())
        .collect();
    for job in &jobs {
        unique &= targets.insert(job.relative.to_string_lossy().to_lowercase());
        unique &= !job
            .relative
            .ancestors()
            .skip(1)
            .any(|p| file_targets.contains(&p.to_string_lossy().to_lowercase()));
    }
    let workers = if unique && total >= 256 * 1024 {
        (options.threads.sevenzip_threads().unwrap_or(1) as usize)
            .clamp(1, 32)
            .min(jobs.len().max(1))
    } else {
        1
    };
    let pool = (workers > 1)
        .then(|| rayon::ThreadPoolBuilder::new().num_threads(workers).build())
        .transpose()
        .map_err(|e| ArchiveError::Worker(e.to_string()))?;
    let mut policy = output::RuntimeConflictPolicy::from(options.conflict_policy);
    let mut prepared = HashSet::new();
    let mut skipped = 0;
    // Resolve conflicts serially, then decode distinct files in parallel. A shared
    // metadata clone is cheap; each chunk reuses its reader and 1 MiB output buffer.
    let batch_size = if workers == 1 { 1 } else { workers * 32 };
    let mut sequential_archive = archive.clone();
    let mut sequential_buffer = vec![0; STREAM_BUFFER_SIZE];
    for batch in jobs.chunks(batch_size) {
        let mut ready = Vec::new();
        for job in batch {
            check_cancel(cancel)?;
            let target = root.join(&job.relative);
            ensure_no_reparse_ancestors(&root, &target)?;
            if job.directory {
                match output::prepare_directory(&root, &target, &mut policy, conflicts)? {
                    output::ConflictAction::Overwrite => {
                        progress.advance(job, 0, 0, true)?;
                    }
                    output::ConflictAction::Skip => {
                        skipped += 1;
                    }
                }
            } else {
                match output::resolve_existing(&target, &mut policy, conflicts)? {
                    output::ConflictAction::Skip => {
                        skipped += 1;
                        continue;
                    }
                    output::ConflictAction::Overwrite => {}
                }
                if prepared.insert(target.parent().expect("validated target").to_owned()) {
                    output::ensure_parent_directories(&root, &target)?;
                }
                ready.push(job);
            }
        }
        let results = if let Some(pool) = &pool {
            let chunk_size = ready.len().div_ceil(workers).max(1);
            pool.install(|| {
                ready
                    .par_chunks(chunk_size)
                    .map(|chunk| {
                        let mut reader = archive.clone();
                        let mut buffer = vec![0; STREAM_BUFFER_SIZE];
                        let result = chunk.iter().try_for_each(|job| {
                            extract_file(
                                &mut reader,
                                &root,
                                job,
                                options,
                                &progress,
                                cancel,
                                &mut buffer,
                            )
                        });
                        if result.is_err() {
                            progress.stop.store(true, Ordering::Relaxed);
                        }
                        result
                    })
                    .collect::<Vec<_>>()
            })
        } else {
            vec![ready.iter().try_for_each(|job| {
                extract_file(
                    &mut sequential_archive,
                    &root,
                    job,
                    options,
                    &progress,
                    cancel,
                    &mut sequential_buffer,
                )
            })]
        };
        check_cancel(cancel)?;
        let mut error = None;
        for result in results {
            if let Err(next) = result {
                if error.is_none() || matches!(error, Some(ArchiveError::Cancelled)) {
                    error = Some(next);
                }
            }
        }
        if let Some(error) = error {
            return Err(error);
        }
    }
    check_cancel(cancel)?;
    let mut snapshot = progress
        .snapshot
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let summary = OperationSummary {
        entries_processed: snapshot.entries_processed,
        bytes_processed: snapshot.bytes_processed,
        entries_skipped: skipped,
        ..Default::default()
    };
    snapshot.phase = ProgressPhase::Finished;
    snapshot.current_file.clear();
    progress.sink.report(snapshot, true);
    Ok(Some(summary))
}
