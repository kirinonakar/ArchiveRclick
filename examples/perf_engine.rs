use std::{
    env, fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use archive_rclick_core::{
    archive::{
        ArchiveEngine, ConflictChoice, ConflictResolver, ExtractOptions,
        libarchive::LibArchiveEngine,
    },
    tasks::{CancellationToken, ProgressSnapshot},
};

struct Overwrite;

impl ConflictResolver for Overwrite {
    fn resolve(&self, _destination: &std::path::Path) -> ConflictChoice {
        ConflictChoice::Overwrite
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("perf_engine failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let archive = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: perf_engine <archive> <extract-directory>")?;
    let destination = env::args_os()
        .nth(2)
        .map(PathBuf::from)
        .ok_or("usage: perf_engine <archive> <extract-directory>")?;
    let engine = LibArchiveEngine::load()?;
    let cancellation = CancellationToken::new();
    let progress_calls = AtomicU64::new(0);
    let progress = |_: ProgressSnapshot| {
        progress_calls.fetch_add(1, Ordering::Relaxed);
    };

    let list_started = Instant::now();
    let listing = engine.list(&archive, None, 0, &progress, &cancellation)?;
    let list_ms = list_started.elapsed().as_secs_f64() * 1000.0;

    let model_started = Instant::now();
    let (listing, root_rows) = prepare_root_model(listing);
    let model_ms = model_started.elapsed().as_secs_f64() * 1000.0;

    if destination.exists() {
        fs::remove_dir_all(&destination)?;
    }
    let extract_started = Instant::now();
    let summary = engine.extract(
        &archive,
        &destination,
        &ExtractOptions {
            total_entries_hint: Some(listing.entries.len() as u64),
            total_bytes_hint: Some(listing.total_uncompressed_size),
            ..ExtractOptions::default()
        },
        &progress,
        &Overwrite,
        &cancellation,
    )?;
    let extract_seconds = extract_started.elapsed().as_secs_f64();
    let throughput_mib_s = if extract_seconds > 0.0 {
        summary.bytes_processed as f64 / 1_048_576.0 / extract_seconds
    } else {
        0.0
    };

    println!(
        concat!(
            "{{",
            "\"runtime\":\"{}\",",
            "\"archive_entries\":{},",
            "\"archive_uncompressed_bytes\":{},",
            "\"list_ms\":{:.3},",
            "\"root_model_rows\":{},",
            "\"root_model_prepare_ms\":{:.3},",
            "\"extract_seconds\":{:.6},",
            "\"extract_entries\":{},",
            "\"extract_bytes\":{},",
            "\"extract_throughput_mib_s\":{:.3},",
            "\"progress_callbacks\":{}",
            "}}"
        ),
        json_escape(&engine.version()),
        listing.entries.len(),
        listing.total_uncompressed_size,
        list_ms,
        root_rows,
        model_ms,
        extract_seconds,
        summary.entries_processed,
        summary.bytes_processed,
        throughput_mib_s,
        progress_calls.load(Ordering::Relaxed),
    );
    Ok(())
}

fn prepare_root_model(
    listing: archive_rclick_core::archive::ArchiveListing,
) -> (archive_rclick_core::archive::ArchiveListing, usize) {
    let mut root_names = std::collections::HashSet::new();
    for entry in &listing.entries {
        if let Some(name) = entry
            .path
            .to_string_lossy()
            .replace('\\', "/")
            .split('/')
            .find(|part| !part.is_empty() && *part != ".")
        {
            root_names.insert(name.to_ascii_lowercase());
        }
    }
    (listing, root_names.len())
}

fn json_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}
