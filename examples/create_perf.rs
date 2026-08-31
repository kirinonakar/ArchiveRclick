use std::{env, path::PathBuf, time::Instant};

use archive_rclick_core::{
    archive::{
        ArchiveEngine, CompositeEngine, CreateFormat, CreateOptions, ProgressSink, SevenZipEngine,
        ThreadCount, libarchive::LibArchiveEngine,
    },
    tasks::{CancellationToken, ProgressSnapshot},
};

struct QuietProgress;

impl ProgressSink for QuietProgress {
    fn report(&self, _progress: ProgressSnapshot) {}
}

fn main() {
    if let Err(error) = run() {
        eprintln!("create_perf failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let format = parse_format(
        &args
            .next()
            .ok_or("usage: create_perf <zip|7z> <source> <output> [level] [threads] [composite|7zip|libarchive] [absolute-dll-path]")?,
    )?;
    let source = PathBuf::from(args.next().ok_or("missing source")?);
    let output = PathBuf::from(args.next().ok_or("missing output")?);
    let level = args
        .next()
        .map(|value| value.to_string_lossy().parse::<u8>())
        .transpose()?
        .unwrap_or(6);
    let threads = args
        .next()
        .map(|value| ThreadCount::from_registry_key(&value.to_string_lossy()))
        .unwrap_or(ThreadCount::Auto);

    let backend = args.next().unwrap_or_else(|| "composite".into());
    let library = args.next().map(PathBuf::from);
    let engine: Box<dyn ArchiveEngine> = match backend.to_str() {
        Some("composite") if library.is_none() => Box::new(CompositeEngine::new(
            LibArchiveEngine::load()?,
            SevenZipEngine::load().ok(),
        )),
        Some("7zip") => Box::new(match library.as_deref() {
            Some(path) => SevenZipEngine::load_from_path(path)?,
            None => SevenZipEngine::load()?,
        }),
        Some("libarchive") => Box::new(match library.as_deref() {
            Some(path) => LibArchiveEngine::load_from_path(path)?,
            None => LibArchiveEngine::load()?,
        }),
        _ => {
            return Err("backend must be composite (without DLL path), 7zip, or libarchive".into());
        }
    };
    if args.next().is_some() {
        return Err("unexpected extra arguments".into());
    }
    let cancel = CancellationToken::new();
    let options = CreateOptions {
        format,
        compression_level: level,
        threads,
        ..CreateOptions::default()
    };
    let progress = QuietProgress;
    let started = Instant::now();
    let summary = engine.create(&output, &[source], &options, &progress, &cancel)?;
    let elapsed = started.elapsed().as_secs_f64();
    let output_bytes = std::fs::metadata(&output)?.len();
    let mib_s = if elapsed > 0.0 {
        summary.bytes_processed as f64 / 1_048_576.0 / elapsed
    } else {
        0.0
    };
    println!(
        "{{\"format\":\"{}\",\"backend\":\"{}\",\"level\":{},\"threads\":\"{}\",\"seconds\":{:.6},\"entries\":{},\"input_bytes\":{},\"output_bytes\":{},\"throughput_mib_s\":{:.3}}}",
        format.label(),
        backend.to_string_lossy(),
        level,
        threads.registry_key(),
        elapsed,
        summary.entries_processed,
        summary.bytes_processed,
        output_bytes,
        mib_s,
    );
    Ok(())
}

fn parse_format(value: &std::ffi::OsStr) -> Result<CreateFormat, Box<dyn std::error::Error>> {
    match value.to_string_lossy().to_ascii_lowercase().as_str() {
        "zip" => Ok(CreateFormat::Zip),
        "7z" => Ok(CreateFormat::SevenZip),
        _ => Err("format must be zip or 7z".into()),
    }
}
