//! Console frontend for the same archive engines used by the desktop app.
use archive_rclick_core::{
    archive::{libarchive::LibArchiveEngine, *},
    tasks::{CancellationToken, ProgressSnapshot},
};
use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

const HELP: &str = r#"Usage: archive-rclick-cli <command> [switches] <archive> [files...]

Commands:
  a  Create a NEW archive (existing archive updates are not supported)
  x  Extract with full paths
  e  Extract files without directory paths
  l  List archive contents
  t  Test archive integrity
  i  Show engine versions and writable formats

Switches:
  -t7z | -tzip | -ttar | -ttar.gz | -ttar.xz | -ttar.zst
  -o{directory}       Extraction directory (default: current directory)
  -p{password}        Archive password (-p prompts for a password)
  -mx=0..9           Compression level (default: 5)
  -zip-backend=7z|zlib-ng|zlib-rs  ZIP create/extract backend (default: saved setting or 7z)
  -mmt=on|off|N      Compression / flate2 ZIP extraction threads
  -mhe=on|off        Encrypt 7z headers
  -v{size}           Split ZIP/7z, e.g. -v100m or -v1g
  -y / -aoa          Overwrite existing extracted files
  -aos               Skip existing extracted files
  -r                 Search input wildcard patterns recursively
  -slt               Technical listing
  -bsp0 / -bd        Disable progress
  -sccUTF-8          UTF-8 console output (always used)
  --                 Stop parsing switches
  -h / --help / -?   Show help

Files accept * and ? wildcards; @listfile reads UTF-8 paths, one per line.
Without input files, a archives the current directory's contents.
Default archive type is 7z; recognized extensions select their format.
Unknown commands/switches fail rather than being silently ignored.
Exit codes: 0 success, 1 warning, 2 error, 7 command-line error, 255 cancelled.
"#;

struct Args {
    command: String,
    archive: PathBuf,
    output: PathBuf,
    files: Vec<String>,
    create: CreateOptions,
    extract: ExtractOptions,
    recursive: bool,
    technical: bool,
    progress: bool,
}

fn usage(message: impl Into<String>) -> (i32, String) {
    (7, message.into())
}
fn failure(error: impl std::fmt::Display) -> (i32, String) {
    (2, error.to_string())
}
fn archive_error(error: ArchiveError) -> (i32, String) {
    (
        if matches!(error, ArchiveError::Cancelled) {
            255
        } else {
            2
        },
        error.to_string(),
    )
}
fn format(name: &str) -> Result<CreateFormat, (i32, String)> {
    match name.to_ascii_lowercase().as_str() {
        "7z" => Ok(CreateFormat::SevenZip),
        "zip" => Ok(CreateFormat::Zip),
        "tar" => Ok(CreateFormat::Tar),
        "tar.gz" | "tgz" => Ok(CreateFormat::TarGzip),
        "tar.xz" | "txz" => Ok(CreateFormat::TarXz),
        "tar.zst" | "tzst" => Ok(CreateFormat::TarZstd),
        _ => Err(usage("Unsupported archive type")),
    }
}
fn parse(raw: Vec<String>) -> Result<Args, (i32, String)> {
    let mut args = Args {
        command: String::new(),
        archive: PathBuf::new(),
        output: PathBuf::from("."),
        files: Vec::new(),
        create: CreateOptions {
            preserve_root: true,
            zip_backend: ZipBackend::from_registry_key(
                &archive_rclick_core::platform::load_zip_backend_preference(),
            ),
            format: CreateFormat::SevenZip,
            ..Default::default()
        },
        extract: ExtractOptions {
            zip_backend: ZipBackend::from_registry_key(&archive_rclick_core::platform::load_zip_backend_preference()),
            ..Default::default()
        },
        recursive: false,
        technical: false,
        progress: true,
    };
    let mut positional = Vec::new();
    let mut switches = true;
    let mut explicit_format = false;
    for arg in raw {
        if switches && arg == "--" {
            switches = false;
            continue;
        }
        if switches && arg.starts_with('-') {
            match arg.as_str() {
                "-y" | "-aoa" => args.extract.conflict_policy = InitialConflictPolicy::OverwriteAll,
                "-aos" => args.extract.conflict_policy = InitialConflictPolicy::SkipAll,
                "-r" => args.recursive = true,
                "-slt" => args.technical = true,
                "-bsp0" | "-bd" => args.progress = false,
                "-sccUTF-8" => {}
                "-mhe=on" => args.create.encrypt_headers = true,
                "-mhe=off" => args.create.encrypt_headers = false,
                _ if arg.starts_with("-zip-backend=") => {
                    args.create.zip_backend = match &arg[13..] {
                        "7z" => ZipBackend::SevenZip,
                        "zlib-ng" => ZipBackend::ZlibNg,
                        "zlib-rs" => ZipBackend::ZlibRs,
                        _ => return Err(usage("ZIP backend must be 7z, zlib-ng, or zlib-rs")),
                    };
                    args.extract.zip_backend = args.create.zip_backend;
                }
                _ if arg.starts_with("-mmt=") => {
                    args.create.threads = match &arg[5..] {
                        "on" => ThreadCount::Auto,
                        "off" => ThreadCount::Exact(1),
                        value => ThreadCount::Exact(
                            value
                                .parse::<u32>()
                                .ok()
                                .filter(|n| *n > 0 && *n <= 1024)
                                .ok_or_else(|| usage("-mmt requires on, off, or 1..1024"))?,
                        ),
                    };
                    args.extract.threads = args.create.threads;
                }
                _ if arg.starts_with("-mx=") => {
                    args.create.compression_level = arg[4..]
                        .parse::<u8>()
                        .ok()
                        .filter(|n| *n <= 9)
                        .ok_or_else(|| usage("-mx requires 0..9"))?
                }
                _ if arg.starts_with("-o") && arg.len() > 2 => {
                    args.output = PathBuf::from(&arg[2..]);
                }
                _ if arg.starts_with("-p") => {
                    let password = if arg.len() == 2 {
                        if !io::stdin().is_terminal() {
                            return Err(usage("Use -p{password} when stdin is redirected"));
                        }
                        eprint!("Password (input visible): ");
                        io::stderr().flush().map_err(failure)?;
                        let mut text = String::new();
                        io::stdin().read_line(&mut text).map_err(failure)?;
                        text.trim_end_matches(['\r', '\n']).to_owned()
                    } else {
                        arg[2..].to_owned()
                    };
                    args.create.password = Some(password.clone());
                    args.extract.password = Some(password);
                }
                _ if arg.starts_with("-t") => {
                    args.create.format = format(&arg[2..])?;
                    explicit_format = true;
                }
                _ if arg.starts_with("-v") => {
                    args.create.split_size = Some(
                        parse_volume_size(&arg[2..])
                            .ok_or_else(|| usage("Invalid split size (minimum 1 KiB)"))?,
                    )
                }
                _ => return Err(usage(format!("Unsupported switch: {arg}"))),
            }
        } else {
            positional.push(arg);
        }
    }
    if positional.is_empty() {
        return Err(usage("Missing command"));
    }
    args.command = positional.remove(0);
    if !["a", "x", "e", "l", "t", "i"].contains(&args.command.as_str()) {
        return Err(usage(format!("Unsupported command: {}", args.command)));
    }
    if args.command == "i" {
        if !positional.is_empty() {
            return Err(usage("i does not accept an archive"));
        }
        return Ok(args);
    }
    if positional.is_empty() {
        return Err(usage("Missing archive path"));
    }
    args.archive = PathBuf::from(positional.remove(0));
    if args.command == "a" {
        if !explicit_format {
            let name = args.archive.to_string_lossy().to_ascii_lowercase();
            for ext in [
                "tar.gz", "tar.xz", "tar.zst", "tgz", "txz", "tzst", "zip", "7z", "tar",
            ] {
                if name.ends_with(&format!(".{ext}")) {
                    args.create.format = format(ext)?;
                    break;
                }
            }
        }
        if args.archive.extension().is_none() {
            args.archive
                .set_extension(args.create.format.default_extension());
        }
    }
    if args.command == "t" && !positional.is_empty() {
        return Err(usage(
            "t tests the entire archive; file filters are not supported",
        ));
    }
    for path in positional {
        if let Some(list) = path.strip_prefix('@') {
            let text = fs::read_to_string(list).map_err(failure)?;
            args.files.extend(
                text.trim_start_matches('\u{feff}')
                    .lines()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.trim_matches('"').to_owned()),
            );
        } else {
            args.files.push(path);
        }
    }
    Ok(args)
}

/// 7-Zip style wildcard matching, including extensionless files for *.*.
fn wildcard(pattern: &str, value: &str) -> bool {
    let pattern = if pattern == "*.*" { "*" } else { pattern };
    let p: Vec<char> = pattern.replace('\\', "/").to_lowercase().chars().collect();
    let v: Vec<char> = value.replace('\\', "/").to_lowercase().chars().collect();
    let (mut i, mut j, mut star, mut retry) = (0, 0, None, 0);
    while j < v.len() {
        if i < p.len() && (p[i] == '?' || p[i] == v[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            i += 1;
            retry = j;
        } else if let Some(s) = star {
            retry += 1;
            j = retry;
            i = s + 1;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == '*' {
        i += 1;
    }
    i == p.len()
}
fn expand(pattern: &str, recursive: bool) -> Result<Vec<PathBuf>, (i32, String)> {
    if !pattern.contains(['*', '?']) && pattern != "." {
        return Ok(vec![PathBuf::from(pattern)]);
    }
    let path = Path::new(pattern);
    let (parent, mask) = if pattern == "." {
        (Path::new("."), "*")
    } else {
        (
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
            path.file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| usage("Invalid input pattern"))?,
        )
    };
    if parent.to_string_lossy().contains(['*', '?']) {
        return Err(usage("Wildcards in directory components are not supported"));
    }
    let mut pending = vec![parent.to_owned()];
    let mut found = Vec::new();
    while let Some(dir) = pending.pop() {
        for item in fs::read_dir(&dir).map_err(failure)? {
            let item = item.map_err(failure)?;
            let metadata = fs::symlink_metadata(item.path()).map_err(failure)?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    continue;
                }
            }
            if wildcard(mask, &item.file_name().to_string_lossy()) {
                found.push(item.path());
            } else if recursive && metadata.is_dir() {
                pending.push(item.path());
            }
        }
    }
    found.sort();
    if found.is_empty() {
        return Err(failure(format!("No files match: {pattern}")));
    }
    Ok(found)
}

struct ConsoleConflict;
impl ConflictResolver for ConsoleConflict {
    fn resolve(&self, destination: &Path) -> ConflictChoice {
        if !io::stdin().is_terminal() {
            eprintln!("File exists: {} (use -aoa or -aos)", destination.display());
            return ConflictChoice::Cancel;
        }
        eprint!(
            "Overwrite {}? [y]es/[n]o/[a]ll/[s]kip all/[q]uit: ",
            destination.display()
        );
        let _ = io::stderr().flush();
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer).is_err() {
            return ConflictChoice::Cancel;
        }
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => ConflictChoice::Overwrite,
            "n" | "no" => ConflictChoice::Skip,
            "a" => ConflictChoice::OverwriteAll,
            "s" => ConflictChoice::SkipAll,
            _ => ConflictChoice::Cancel,
        }
    }
}
fn run(mut args: Args) -> Result<i32, (i32, String)> {
    let cwd = std::env::current_dir().map_err(failure)?;
    args.archive = cwd.join(&args.archive);
    args.output = cwd.join(&args.output);
    let engine = CompositeEngine::new(
        LibArchiveEngine::load().map_err(archive_error)?,
        Some(SevenZipEngine::load().map_err(archive_error)?),
    );
    if args.command == "i" {
        println!(
            "{}\nWritable formats: {:?}",
            engine.version(),
            engine.writable_formats()
        );
        return Ok(0);
    }
    let cancel = CancellationToken::new();
    let show_progress = args.progress && io::stderr().is_terminal();
    let progress = |p: ProgressSnapshot| {
        if show_progress {
            eprint!(
                "\r{:3.0}% {}                    ",
                p.fraction() * 100.0,
                p.phase.label()
            );
        }
    };
    let summary = match args.command.as_str() {
        "a" => {
            if args.archive.exists()
                || PathBuf::from(format!("{}.001", args.archive.display())).exists()
            {
                return Err(failure(
                    "Archive already exists. Updating existing archives is not supported; choose a new output path.",
                ));
            }
            if args.files.is_empty() {
                args.files.push("*".to_owned());
            }
            let mut files = Vec::new();
            for input in &args.files {
                files.extend(expand(input, args.recursive)?);
            }
            files.sort();
            files.dedup();
            let mut names = std::collections::HashSet::new();
            for file in &files {
                let name = file
                    .file_name()
                    .ok_or_else(|| usage("Input needs a file name"))?
                    .to_string_lossy()
                    .to_lowercase();
                if !names.insert(name) {
                    return Err(usage(
                        "Input files have duplicate base names; select their common parent directory",
                    ));
                }
            }
            engine
                .create(&args.archive, &files, &args.create, &progress, &cancel)
                .map_err(archive_error)?
        }
        "t" => engine
            .test(
                &args.archive,
                args.extract.password.as_deref(),
                &progress,
                &cancel,
            )
            .map_err(archive_error)?,
        _ => {
            let listing = engine
                .list(
                    &args.archive,
                    args.extract.password.as_deref(),
                    0,
                    &progress,
                    &cancel,
                )
                .map_err(archive_error)?;
            let selected: Vec<_> = listing
                .entries
                .iter()
                .filter(|entry| {
                    args.files.is_empty()
                        || args.files.iter().any(|p| {
                            wildcard(p, &entry.display_path)
                                || (args.recursive
                                    && wildcard(
                                        p,
                                        &entry
                                            .path
                                            .file_name()
                                            .unwrap_or_default()
                                            .to_string_lossy(),
                                    ))
                                || entry.path.starts_with(Path::new(p))
                        })
                })
                .collect();
            if args.command == "l" {
                println!(
                    "\nPath = {}\nType = {}\n",
                    args.archive.display(),
                    listing.format_name
                );
                for e in &selected {
                    if args.technical {
                        println!(
                            "Path = {}\nSize = {}\nPacked Size = {}\nFolder = {}\nEncrypted = {}\n",
                            e.display_path,
                            e.size.unwrap_or(0),
                            e.compressed_size.unwrap_or(0),
                            if e.kind == ArchiveEntryKind::Directory {
                                "+"
                            } else {
                                "-"
                            },
                            if e.encrypted { "+" } else { "-" }
                        );
                    } else {
                        println!(
                            "{:>12}  {}  {}",
                            e.size.unwrap_or(0),
                            if e.kind == ArchiveEntryKind::Directory {
                                "D"
                            } else {
                                "F"
                            },
                            e.display_path
                        );
                    }
                }
                if let Some(warning) = listing.warning {
                    eprintln!("WARNING: {warning}");
                    return Ok(1);
                }
                return Ok(0);
            }
            if !args.files.is_empty() {
                if selected.is_empty() {
                    return Err(failure("No archive entries match the file filters"));
                }
                args.extract.selection =
                    ExtractSelection::Paths(selected.iter().map(|e| e.path.clone()).collect());
            }
            args.extract.flatten_paths = args.command == "e";
            engine
                .extract(
                    &args.archive,
                    &args.output,
                    &args.extract,
                    &progress,
                    &ConsoleConflict,
                    &cancel,
                )
                .map_err(archive_error)?
        }
    };
    if show_progress {
        eprintln!();
    }
    println!(
        "Files: {}\nSize: {}\nSkipped: {}",
        summary.entries_processed, summary.bytes_processed, summary.entries_skipped
    );
    if let Some(warning) = summary.warning {
        eprintln!("WARNING: {warning}");
        Ok(1)
    } else {
        println!("Everything is Ok");
        Ok(0)
    }
}
fn main() {
    let raw: Vec<_> = std::env::args().skip(1).collect();
    if raw.is_empty() || (raw.len() == 1 && ["-h", "--help", "-?"].contains(&raw[0].as_str())) {
        println!("ArchiveRclick CLI {}\n\n{HELP}", env!("CARGO_PKG_VERSION"));
        return;
    }
    match parse(raw).and_then(run) {
        Ok(code) => std::process::exit(code),
        Err((code, message)) => {
            eprintln!("\nERROR: {message}");
            std::process::exit(code);
        }
    }
}

#[cfg(test)]
mod zip_backend_tests {
    use super::*;

    #[test]
    fn explicit_zip_backend_overrides_saved_preference() {
        for backend in ZipBackend::ALL {
            let args = parse(vec![
                "a".into(),
                "test.zip".into(),
                format!("-zip-backend={}", backend.registry_key()),
            ])
            .unwrap();
            assert_eq!(args.create.zip_backend, backend);
            assert_eq!(args.extract.zip_backend, backend);
        }
    }

    #[test]
    fn invalid_zip_backend_is_a_command_line_error() {
        for value in ["", "zlib", "ZLIB-NG", "unknown"] {
            let error = parse(vec![
                "a".into(),
                "test.zip".into(),
                format!("-zip-backend={value}"),
            ])
            .err()
            .unwrap();
            assert_eq!(error.0, 7);
            assert!(error.1.contains("ZIP backend"));
        }
    }
}
