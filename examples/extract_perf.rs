use std::{env, path::PathBuf, time::Instant};

use archive_rclick_core::{
    archive::{
        ArchiveEngine, CompositeEngine, ConflictChoice, ConflictResolver, ExtractOptions,
        ProgressSink, SevenZipEngine, libarchive::LibArchiveEngine,
    },
    tasks::{CancellationToken, ProgressSnapshot},
};

struct QuietProgress;

impl ProgressSink for QuietProgress {
    fn report(&self, _progress: ProgressSnapshot) {}
}

struct Overwrite;

impl ConflictResolver for Overwrite {
    fn resolve(&self, _destination: &std::path::Path) -> ConflictChoice {
        ConflictChoice::Overwrite
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("extract_perf failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let archive = PathBuf::from(
        args.next()
            .ok_or("usage: extract_perf <archive> <destination>")?,
    );
    let destination = PathBuf::from(
        args.next()
            .ok_or("usage: extract_perf <archive> <destination>")?,
    );
    if destination.exists() {
        return Err(format!("destination already exists: {}", destination.display()).into());
    }

    let libarchive = LibArchiveEngine::load()?;
    let sevenzip = SevenZipEngine::load().ok();
    let engine = CompositeEngine::new(libarchive, sevenzip);
    let cancellation = CancellationToken::new();
    let progress = QuietProgress;
    let started = Instant::now();
    let summary = engine.extract(
        &archive,
        &destination,
        &ExtractOptions::default(),
        &progress,
        &Overwrite,
        &cancellation,
    )?;
    let seconds = started.elapsed().as_secs_f64();
    let throughput_mib_s = if seconds > 0.0 {
        summary.bytes_processed as f64 / 1_048_576.0 / seconds
    } else {
        0.0
    };
    println!(
        "{{\"seconds\":{:.6},\"entries\":{},\"output_bytes\":{},\"throughput_mib_s\":{:.3}}}",
        seconds, summary.entries_processed, summary.bytes_processed, throughput_mib_s,
    );
    Ok(())
}
