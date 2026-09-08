//! ZIP creation with isolated flate2 backends and bounded file-level parallelism.
use super::*;
use crate::archive::ZipBackend;
use std::io::{self, BufWriter};
use tempfile::NamedTempFile;
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

// At most 256 MiB of compressed staging data, plus codec and I/O buffers.
// Large entries spill to disk; a single worker writes directly to the output.
const SPOOL_BYTES: usize = 8 * 1024 * 1024;
const MAX_WORKERS: usize = 32;

// Only `workers` jobs may own a spool, including the one being merged. Each
// slot is refilled after its result is consumed, so slow groups cannot create
// an unbounded reorder buffer. Other slots compress while the caller merges.
fn ordered_pipeline<T: Send>(
    count: usize,
    workers: usize,
    produce: impl Fn(usize) -> ArchiveResult<T> + Sync,
    mut consume: impl FnMut(T) -> ArchiveResult<()>,
) -> ArchiveResult<()> {
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        let result = (|| {
            let mut slots = Vec::new();
            for first in 0..workers.min(count) {
                let (jobs, input) = std::sync::mpsc::sync_channel(1);
                let (output, results) = std::sync::mpsc::sync_channel(1);
                let produce = &produce;
                handles.push(
                    std::thread::Builder::new()
                        .name(format!("zip-compress-{first}"))
                        .spawn_scoped(scope, move || {
                            while let Ok(index) = input.recv() {
                                let result = produce(index);
                                let failed = result.is_err();
                                if output.send(result).is_err() || failed {
                                    break;
                                }
                            }
                        })
                        .map_err(|error| ArchiveError::Worker(error.to_string()))?,
                );
                jobs.send(first)
                    .map_err(|error| ArchiveError::Worker(error.to_string()))?;
                slots.push((jobs, results));
            }
            for index in 0..count {
                let (jobs, results) = &slots[index % slots.len()];
                let value = results
                    .recv()
                    .map_err(|error| ArchiveError::Worker(error.to_string()))??;
                consume(value)?;
                let next = index + slots.len();
                if next < count {
                    jobs.send(next)
                        .map_err(|error| ArchiveError::Worker(error.to_string()))?;
                }
            }
            Ok(())
            // Drop all channel endpoints before joining, also on an error. This
            // releases workers waiting to receive jobs or send pending results.
        })();
        let mut panicked = false;
        for handle in handles {
            panicked |= handle.join().is_err();
        }
        result?;
        if panicked {
            return Err(ArchiveError::Worker(
                "ZIP compression worker panicked".into(),
            ));
        }
        Ok(())
    })
}

enum Output {
    Single(NamedTempFile),
    Split(VolumeOutput),
}

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Single(file) => file.write(bytes),
            Self::Split(volumes) => Write::write(volumes, bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Single(file) => file.flush(),
            Self::Split(volumes) => Write::flush(volumes),
        }
    }
}

impl Seek for Output {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        match self {
            Self::Single(file) => file.seek(from),
            Self::Split(volumes) => Seek::seek(volumes, from),
        }
    }
}

// merge_archive uses io::copy internally. Check cancellation on each read,
// including when merging an entry that has spilled to disk.
struct CancelReader<'a, R>(R, &'a CancellationToken);
impl<R: Read> Read for CancelReader<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.1.is_cancelled() {
            return Err(io::Error::other("operation cancelled"));
        }
        self.0.read(bytes)
    }
}
impl<R: Seek> Seek for CancelReader<'_, R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.0.seek(from)
    }
}

struct Progress<'a> {
    sink: ThrottledProgress<'a>,
    snapshot: Mutex<ProgressSnapshot>,
}

impl Progress<'_> {
    fn advance(&self, item: &SourceItem, current: u64, bytes: u64, completed: bool) {
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|p| p.into_inner());
        snapshot.current_file.clone_from(&item.archive_name);
        snapshot.current_file_bytes_processed = current;
        snapshot.current_file_total_bytes = Some(item.size);
        snapshot.bytes_processed += bytes;
        snapshot.entries_processed += u64::from(completed && item.kind == SourceKind::File);
        self.sink.report(snapshot.clone(), false);
    }
}

fn zip_error(error: zip::result::ZipError) -> ArchiveError {
    ArchiveError::Worker(format!("ZIP creation failed: {error}"))
}

fn write_item<W: Write + Seek>(
    writer: &mut ZipWriter<W>,
    item: &SourceItem,
    options: &CreateOptions,
    progress: &Progress<'_>,
    cancel: &CancellationToken,
    buffer: &mut [u8],
) -> ArchiveResult<()> {
    check_cancel(cancel)?;
    let mut entry_options = SimpleFileOptions::default()
        .compression_method(if options.compression_level == 0 {
            CompressionMethod::Stored
        } else {
            CompressionMethod::Deflated
        })
        .compression_level(
            (options.compression_level != 0).then_some(i64::from(options.compression_level)),
        )
        // Include DEFLATE expansion when reserving ZIP64 local fields.
        .large_file(item.size >= u64::from(u32::MAX) / 2);
    if let Some(stamp) = item
        .modified_unix_seconds
        .and_then(|stamp| {
            time::OffsetDateTime::from_unix_timestamp(crate::platform::utc_to_local_seconds(stamp))
                .ok()
        })
        .and_then(|stamp| {
            zip::DateTime::from_date_and_time(
                stamp.year() as u16,
                stamp.month() as u8,
                stamp.day(),
                stamp.hour(),
                stamp.minute(),
                stamp.second(),
            )
            .ok()
        })
    {
        entry_options = entry_options.last_modified_time(stamp);
    }
    if item.kind == SourceKind::Directory {
        writer
            .add_directory(&item.archive_name, entry_options)
            .map_err(zip_error)?;
        return Ok(());
    }
    if let Some(password) = options.password.as_deref().filter(|p| !p.is_empty()) {
        entry_options = entry_options.with_aes_encryption(zip::AesMode::Aes256, password);
    }
    // Open the link itself if a source changed after enumeration; never follow it.
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN | 0x0020_0000) // FILE_FLAG_OPEN_REPARSE_POINT
        .open(&item.source)
        .map_err(|error| ArchiveError::io(&item.source, error))?;
    match options.zip_backend {
        ZipBackend::ZlibNg => writer.start_file_zlib_ng(&item.archive_name, entry_options),
        ZipBackend::ZlibRs => writer.start_file(&item.archive_name, entry_options),
        ZipBackend::SevenZip => unreachable!("7z is routed to the original engine"),
    }
    .map_err(zip_error)?;
    let mut current = 0;
    progress.advance(item, current, 0, false);
    loop {
        check_cancel(cancel)?;
        let count = input
            .read(buffer)
            .map_err(|error| ArchiveError::io(&item.source, error))?;
        if count == 0 {
            break;
        }
        writer
            .write_all(&buffer[..count])
            .map_err(|error| ArchiveError::io(&item.source, error))?;
        current += count as u64;
        progress.advance(item, current, count as u64, false);
    }
    progress.advance(item, current, 0, true);
    Ok(())
}

pub(super) fn create(
    destination: &Path,
    files: &[PathBuf],
    options: &CreateOptions,
    sink: &dyn ProgressSink,
    cancel: &CancellationToken,
) -> ArchiveResult<OperationSummary> {
    check_cancel(cancel)?;
    if files.is_empty() || options.compression_level > 9 || options.split_size == Some(0) {
        return Err(ArchiveError::InvalidInput(
            "select inputs, a level from 0 to 9, and a nonzero volume size".into(),
        ));
    }
    // The existing Windows 7-Zip ZIP reader interprets passwords in the OEM
    // code page, whereas zip uses UTF-8. Match the native ZIP writer's ASCII
    // restriction instead of creating an archive this application cannot open.
    if options.password.as_deref().is_some_and(|p| !p.is_ascii()) {
        return Err(ArchiveError::UnsupportedOption(
            "ZIP passwords must use ASCII characters for 7-Zip compatibility; use 7z format for Unicode passwords".into(),
        ));
    }
    if options.encrypt_headers {
        return Err(ArchiveError::UnsupportedOption(
            "header encryption is supported only for 7z archives".into(),
        ));
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|e| ArchiveError::io(parent, e))?;
    let parent = fs::canonicalize(parent).map_err(|e| ArchiveError::io(parent, e))?;
    let target = parent.join(
        destination
            .file_name()
            .ok_or_else(|| ArchiveError::InvalidInput("destination has no file name".into()))?,
    );
    ensure_no_reparse_ancestors(&parent, &target)?;
    let (items, total) = collect_sources(files, &target, options.preserve_root, cancel)?;
    // Reject duplicate archive names before creating output or starting workers.
    let mut names = HashSet::new();
    for item in &items {
        if !names.insert(&item.archive_name) {
            return Err(ArchiveError::InvalidInput(format!(
                "duplicate ZIP entry: {}",
                item.archive_name
            )));
        }
    }
    let work = tempfile::Builder::new()
        .prefix(".archive-rclick-zip-")
        .tempdir_in(&parent)
        .map_err(|e| ArchiveError::io(&parent, e))?;
    let mut output = match options.split_size {
        Some(size) => Output::Split(VolumeOutput::new(work.path().join("output.zip"), size)),
        None => Output::Single(
            NamedTempFile::new_in(work.path()).map_err(|e| ArchiveError::io(&parent, e))?,
        ),
    };
    let mut snapshot = ProgressSnapshot::new(ProgressPhase::Compressing);
    snapshot.total_bytes = Some(total);
    snapshot.total_entries =
        Some(items.iter().filter(|i| i.kind == SourceKind::File).count() as u64);
    let progress = Progress {
        sink: ThrottledProgress::new(sink, PROGRESS_INTERVAL),
        snapshot: Mutex::new(snapshot),
    };
    let workers = if total < 256 * 1024 {
        1
    } else {
        (options.threads.sevenzip_threads().unwrap_or(1) as usize)
            .clamp(1, MAX_WORKERS)
            .min(items.len().max(1))
    };
    // Amortize ZIP metadata, task scheduling, and merge costs across groups.
    // Bound both input bytes and entry count so tiny-file workloads remain balanced.
    let target_bytes = (total / workers as u64).clamp(256 * 1024, SPOOL_BYTES as u64);
    let mut groups = Vec::new();
    let mut start = 0;
    let mut group_bytes = 0;
    for (index, item) in items.iter().enumerate() {
        group_bytes += item.size;
        if group_bytes >= target_bytes || index + 1 - start >= 256 {
            groups.push(&items[start..=index]);
            start = index + 1;
            group_bytes = 0;
        }
    }
    if start < items.len() {
        groups.push(&items[start..]);
    }
    let workers = workers.min(groups.len().max(1));
    let result = (|| {
        let buffered = BufWriter::with_capacity(STREAM_BUFFER_SIZE, &mut output);
        let mut writer = ZipWriter::new(buffered);
        if workers == 1 {
            let mut buffer = vec![0; STREAM_BUFFER_SIZE];
            for item in &items {
                write_item(&mut writer, item, options, &progress, cancel, &mut buffer)?;
            }
        } else {
            ordered_pipeline(
                groups.len(),
                workers,
                |index| {
                    check_cancel(cancel)?;
                    let group = groups[index];
                    let spool = tempfile::spooled_tempfile_in(SPOOL_BYTES, work.path());
                    let mut entry = ZipWriter::new(spool);
                    let max_size = group.iter().map(|item| item.size).max().unwrap_or(0);
                    let mut buffer = vec![0; stream_buffer_size(max_size)];
                    for item in group {
                        write_item(&mut entry, item, options, &progress, cancel, &mut buffer)?;
                    }
                    entry.finish().map_err(zip_error)
                },
                |spool| {
                    check_cancel(cancel)?;
                    let archive =
                        ZipArchive::new(CancelReader(spool, cancel)).map_err(zip_error)?;
                    writer.merge_archive(archive).map_err(zip_error)
                },
            )?;
        }
        let mut buffered = writer.finish().map_err(zip_error)?;
        buffered.flush().map_err(|e| ArchiveError::io(&target, e))?;
        Ok(())
    })();
    // Prefer the cancellation result even if it interrupted io::copy.
    check_cancel(cancel)?;
    result?;
    ensure_no_reparse_ancestors(&parent, &target)?;
    match output {
        Output::Single(file) => {
            file.persist(&target)
                .map_err(|e| ArchiveError::io(&target, e.error))?;
        }
        Output::Split(mut volumes) => {
            volumes.close_files();
            install_temporary_volumes(&parent, &volumes.paths(), &target)?;
        }
    }
    let mut snapshot = progress
        .snapshot
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let summary = OperationSummary {
        entries_processed: snapshot.entries_processed,
        bytes_processed: snapshot.bytes_processed,
        ..Default::default()
    };
    snapshot.phase = ProgressPhase::Finished;
    snapshot.current_file.clear();
    progress.sink.report(snapshot, true);
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    use std::time::Duration;

    #[test]
    fn pipeline_refills_before_slow_group_finishes_and_bounds_spools() {
        struct Staged(usize, Arc<AtomicUsize>);
        impl Drop for Staged {
            fn drop(&mut self) {
                self.1.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let live = Arc::new(AtomicUsize::new(0));
        let peak = AtomicUsize::new(0);
        let (ready, wait) = mpsc::channel();
        let wait = Mutex::new(wait);
        let mut order = Vec::new();
        ordered_pipeline(
            19,
            2,
            |index| {
                let count = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                let staged = Staged(index, live.clone());
                if index == 1 {
                    // A batch barrier would time out: group 2 must start while
                    // group 1 is still running, after the caller merges group 0.
                    wait.lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                }
                if index == 2 {
                    ready.send(()).unwrap();
                }
                Ok(staged)
            },
            |staged| {
                order.push(staged.0);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(order, (0..19).collect::<Vec<_>>());
        assert!(peak.load(Ordering::SeqCst) <= 2);
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn pipeline_disconnects_pending_workers_on_produce_or_merge_error() {
        for fail_in_producer in [false, true] {
            let result = ordered_pipeline(
                19,
                4,
                |index| {
                    if fail_in_producer && index == 1 {
                        Err(ArchiveError::Worker("producer failure".into()))
                    } else {
                        Ok(index)
                    }
                },
                |index| {
                    if !fail_in_producer && index == 1 {
                        Err(ArchiveError::Worker("merge failure".into()))
                    } else {
                        Ok(())
                    }
                },
            );
            let expected = if fail_in_producer {
                "producer failure"
            } else {
                "merge failure"
            };
            assert!(matches!(result, Err(ArchiveError::Worker(message)) if message == expected));
        }
    }
}
