//! Explorer shell-command execution and its command-line/path validation.

use super::*;

/// A validated routing decision for an Explorer shell verb. Parsing command
/// names is kept out of the execution functions so each handler only owns its
/// path validation, option construction, and operation startup.
pub(super) enum ContextMenuCommand {
    Extract {
        args: Vec<OsString>,
        elevated_retry: bool,
    },
    ExtractTo {
        args: Vec<OsString>,
        extract_here: bool,
    },
    CreateTo {
        args: Vec<OsString>,
        format: CreateFormat,
    },
    CreateEachTo {
        args: Vec<OsString>,
        format: CreateFormat,
    },
    Create {
        args: Vec<OsString>,
        format: CreateFormat,
        elevated_retry: bool,
    },
    CreateEach {
        args: Vec<OsString>,
        format: CreateFormat,
        elevated_retry: bool,
    },
}

impl ContextMenuCommand {
    pub(super) fn execute(self) -> Result<(), String> {
        match self {
            Self::Extract {
                args,
                elevated_retry,
            } => run_gui_extract(&args, elevated_retry),
            Self::ExtractTo { args, extract_here } => run_gui_extract_to(&args, extract_here),
            Self::CreateTo { args, format } => run_gui_create_to(&args, format),
            Self::CreateEachTo { args, format } => run_gui_create_each_to(&args, format),
            Self::Create {
                args,
                format,
                elevated_retry,
            } => run_gui_create(&args, format, elevated_retry),
            Self::CreateEach {
                args,
                format,
                elevated_retry,
            } => run_gui_create_each(&args, format, elevated_retry),
        }
    }
}

fn run_gui_extract(args: &[OsString], elevated_retry: bool) -> Result<(), String> {
    let (ask_conflicts, args) =
        if elevated_retry && args.first().is_some_and(|arg| arg == "--ask-conflicts") {
            (true, &args[1..])
        } else {
            (false, args)
        };
    let requested_archives = parse_elevated_extract(args, elevated_retry)?;
    if requested_archives.is_empty() {
        return Err("Usage: ArchiveRclick extract <archive>...".to_owned());
    }
    let mut archives: Vec<PathBuf> = Vec::with_capacity(requested_archives.len());
    let mut destination_overrides = Vec::with_capacity(requested_archives.len());
    for (argument, destination_override) in requested_archives {
        let requested = PathBuf::from(argument);
        match resolve_existing_path(&requested) {
            Some(path) if path.is_file() => {
                archives.push(path);
                destination_overrides.push(destination_override);
            }
            Some(path) => return Err(format!("Not an archive file: {}", path.display())),
            None => return Err(missing_path_message(&requested)),
        }
    }
    let engine: Engine = load_engine()?;
    let (ui, state) = open_progress_window()?;
    start_extract_batch_window(
        &ui,
        &state,
        Arc::clone(&engine),
        archives,
        destination_overrides,
        elevated_retry,
        ask_conflicts,
    );
    run_progress_window(&ui, &state)
}

/// Extracts archives into the folder that received a right-drag. The normal
/// mode creates one subfolder per archive; `extract_here` uses the receiving
/// folder itself. These internal modes leave the public `extract` behavior
/// unchanged.
fn run_gui_extract_to(args: &[OsString], extract_here: bool) -> Result<(), String> {
    let verb = if extract_here {
        "extract-here-to"
    } else {
        "extract-to"
    };
    let Some(destination_argument) = args.first() else {
        return Err(format!(
            "Usage: ArchiveRclick {verb} <directory> <archive>..."
        ));
    };
    let archive_arguments = &args[1..];
    if archive_arguments.is_empty() {
        return Err(format!(
            "Usage: ArchiveRclick {verb} <directory> <archive>..."
        ));
    }

    let requested_destination = PathBuf::from(destination_argument);
    let destination = match resolve_existing_path(&requested_destination) {
        Some(path) if path.is_dir() => path,
        Some(path) => {
            return Err(format!(
                "Extraction destination is not a folder: {}",
                path.display()
            ));
        }
        None => return Err(missing_path_message(&requested_destination)),
    };

    let mut archives = Vec::with_capacity(archive_arguments.len());
    let mut destination_overrides = Vec::with_capacity(archive_arguments.len());
    for argument in archive_arguments {
        let requested = PathBuf::from(argument);
        match resolve_existing_path(&requested) {
            Some(path) if path.is_file() => {
                let output = right_drag_extract_destination(&destination, &path, extract_here);
                archives.push(path);
                destination_overrides.push(Some(output));
            }
            Some(path) => return Err(format!("Not an archive file: {}", path.display())),
            None => return Err(missing_path_message(&requested)),
        }
    }

    let engine: Engine = load_engine()?;
    let (ui, state) = open_progress_window()?;
    start_extract_batch_window(
        &ui,
        &state,
        Arc::clone(&engine),
        archives,
        destination_overrides,
        false,
        extract_here,
    );
    run_progress_window(&ui, &state)
}

pub(super) fn right_drag_extract_destination(
    destination: &Path,
    archive: &Path,
    extract_here: bool,
) -> PathBuf {
    if extract_here {
        destination.to_path_buf()
    } else {
        unique_path(&destination.join(archive_directory_name(archive)))
    }
}

/// Creates one archive from the dragged items inside the folder that received
/// the drop. The archive name still follows the normal source-name rule, but
/// its parent is the drop destination instead of the source's parent folder.
fn run_gui_create_to(args: &[OsString], format: CreateFormat) -> Result<(), String> {
    let verb = match format {
        CreateFormat::Zip => "zip-to",
        CreateFormat::SevenZip => "7z-to",
        _ => unreachable!("only zip and 7z reach the right-drag create flow"),
    };
    let Some(destination_argument) = args.first() else {
        return Err(format!(
            "Usage: ArchiveRclick {verb} <directory> <file-or-folder>..."
        ));
    };
    let source_arguments = &args[1..];
    if source_arguments.is_empty() {
        return Err(format!(
            "Usage: ArchiveRclick {verb} <directory> <file-or-folder>..."
        ));
    }

    let requested_destination = PathBuf::from(destination_argument);
    let destination_folder = match resolve_existing_path(&requested_destination) {
        Some(path) if path.is_dir() => path,
        Some(path) => {
            return Err(format!(
                "Archive destination is not a folder: {}",
                path.display()
            ));
        }
        None => return Err(missing_path_message(&requested_destination)),
    };

    let mut sources = Vec::with_capacity(source_arguments.len());
    for argument in source_arguments {
        let requested = PathBuf::from(argument);
        match resolve_existing_path(&requested) {
            Some(path) => sources.push(path),
            None => return Err(missing_path_message(&requested)),
        }
    }

    let archive_name = cli_archive_destination(&sources, format)
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_else(|| OsString::from(format!("archive.{}", format.default_extension())));
    let destination = unique_path(&destination_folder.join(archive_name));
    let engine: Engine = load_engine()?;
    let options = CreateOptions {
        format,
        threads: ThreadCount::from_registry_key(&platform::load_thread_preference()),
        ..CreateOptions::default()
    };
    let (ui, state) = open_progress_window()?;
    start_create_window(
        &ui,
        &state,
        Arc::clone(&engine),
        destination,
        sources,
        options,
        false,
    );
    run_progress_window(&ui, &state)
}

/// Creates one archive for each dragged folder inside the folder that received
/// the drop. This is the target-aware counterpart of `zip-each`/`7z-each`.
fn run_gui_create_each_to(args: &[OsString], format: CreateFormat) -> Result<(), String> {
    let verb = match format {
        CreateFormat::Zip => "zip-each-to",
        CreateFormat::SevenZip => "7z-each-to",
        _ => unreachable!("only zip and 7z reach the right-drag create flow"),
    };
    let Some(destination_argument) = args.first() else {
        return Err(format!(
            "Usage: ArchiveRclick {verb} <directory> <folder>..."
        ));
    };
    let folder_arguments = &args[1..];
    if folder_arguments.is_empty() {
        return Err(format!(
            "Usage: ArchiveRclick {verb} <directory> <folder>..."
        ));
    }

    let requested_destination = PathBuf::from(destination_argument);
    let destination_folder = match resolve_existing_path(&requested_destination) {
        Some(path) if path.is_dir() => path,
        Some(path) => {
            return Err(format!(
                "Archive destination is not a folder: {}",
                path.display()
            ));
        }
        None => return Err(missing_path_message(&requested_destination)),
    };

    let mut items = Vec::with_capacity(folder_arguments.len());
    for argument in folder_arguments {
        let requested = PathBuf::from(argument);
        let source = match resolve_existing_path(&requested) {
            Some(path) if path.is_dir() => path,
            Some(path) => return Err(format!("Not a folder: {}", path.display())),
            None => return Err(missing_path_message(&requested)),
        };
        let name = source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "archive".to_owned());
        let destination =
            unique_path(&destination_folder.join(format!("{name}.{}", format.default_extension())));
        items.push((source, destination));
    }

    let engine: Engine = load_engine()?;
    let options = CreateOptions {
        format,
        threads: ThreadCount::from_registry_key(&platform::load_thread_preference()),
        ..CreateOptions::default()
    };
    let (ui, state) = open_progress_window()?;
    start_create_batch_window(&ui, &state, Arc::clone(&engine), items, options, false);
    run_progress_window(&ui, &state)
}

/// Parses the output markers used by an elevated extraction retry. Each
/// `--output <directory> <archive>` pair keeps the retry pointed at the exact
/// directory used by the failed attempt. Normal CLI invocations remain a list
/// of archive paths with no overrides.
pub(super) fn parse_elevated_extract(
    args: &[OsString],
    elevated_retry: bool,
) -> Result<Vec<(OsString, Option<PathBuf>)>, String> {
    if !elevated_retry {
        return Ok(args.iter().cloned().map(|path| (path, None)).collect());
    }

    let mut pending_output: Option<PathBuf> = None;
    let mut archives = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        if args[index].as_os_str() == OsStr::new("--output") {
            if pending_output.is_some() {
                return Err(
                    "The elevated extraction retry has an output without an archive".to_owned(),
                );
            }
            let Some(output) = args.get(index + 1) else {
                return Err("The elevated extraction retry is missing an output path".to_owned());
            };
            pending_output = Some(PathBuf::from(output));
            index += 2;
        } else {
            archives.push((args[index].clone(), pending_output.take()));
            index += 1;
        }
    }
    if pending_output.is_some() {
        return Err("The elevated extraction retry is missing an archive path".to_owned());
    }
    Ok(archives)
}

fn run_gui_create(
    args: &[OsString],
    format: CreateFormat,
    elevated_retry: bool,
) -> Result<(), String> {
    let verb = match format {
        CreateFormat::Zip => "zip",
        CreateFormat::SevenZip => "7z",
        _ => unreachable!("only zip and 7z reach the context-menu create flow"),
    };
    let (destination_override, source_args) = parse_elevated_output(args, elevated_retry)?;
    if source_args.is_empty() {
        return Err(format!("Usage: ArchiveRclick {verb} <file-or-folder>..."));
    }
    let mut sources: Vec<PathBuf> = Vec::with_capacity(source_args.len());
    for argument in &source_args {
        let requested = PathBuf::from(argument);
        match resolve_existing_path(&requested) {
            Some(source) => sources.push(source),
            None => return Err(missing_path_message(&requested)),
        }
    }
    // When a file with the same name already exists, pick the next free name
    // (보고서.zip -> 보고서_2.zip -> 보고서_3.zip ...).
    let destination = destination_override
        .unwrap_or_else(|| unique_path(&cli_archive_destination(&sources, format)));
    let engine: Engine = load_engine()?;
    let options = CreateOptions {
        format,
        threads: ThreadCount::from_registry_key(&platform::load_thread_preference()),
        ..CreateOptions::default()
    };
    let (ui, state) = open_progress_window()?;
    start_create_window(
        &ui,
        &state,
        Arc::clone(&engine),
        destination,
        sources,
        options,
        elevated_retry,
    );
    run_progress_window(&ui, &state)
}

/// Creates one archive beside every selected folder. The normal invocation
/// receives only folder paths; an elevated retry also carries the exact
/// destination for each folder so a partially completed first attempt is not
/// redirected to a new `_2` archive.
fn run_gui_create_each(
    args: &[OsString],
    format: CreateFormat,
    elevated_retry: bool,
) -> Result<(), String> {
    let verb = match format {
        CreateFormat::Zip => "zip-each",
        CreateFormat::SevenZip => "7z-each",
        _ => unreachable!("only zip and 7z reach the per-folder create flow"),
    };
    let requested_sources = parse_elevated_batch_output(args, elevated_retry)?;
    if requested_sources.is_empty() {
        return Err(format!("Usage: ArchiveRclick {verb} <folder>..."));
    }

    let mut items = Vec::with_capacity(requested_sources.len());
    for (destination_override, argument) in requested_sources {
        let requested = PathBuf::from(argument);
        let source = match resolve_existing_path(&requested) {
            Some(path) if path.is_dir() => path,
            Some(path) => return Err(format!("Not a folder: {}", path.display())),
            None => return Err(missing_path_message(&requested)),
        };
        let destination = destination_override.unwrap_or_else(|| {
            unique_path(&cli_archive_destination(
                std::slice::from_ref(&source),
                format,
            ))
        });
        items.push((source, destination));
    }

    let engine: Engine = load_engine()?;
    let options = CreateOptions {
        format,
        threads: ThreadCount::from_registry_key(&platform::load_thread_preference()),
        ..CreateOptions::default()
    };
    let (ui, state) = open_progress_window()?;
    start_create_batch_window(
        &ui,
        &state,
        Arc::clone(&engine),
        items,
        options,
        elevated_retry,
    );
    run_progress_window(&ui, &state)
}

/// Parses the destination marker used only by an elevated retry.  Normal CLI
/// invocations keep the original `zip <source>...` shape, while a retry gets
/// the exact destination selected before the first attempt.  Keeping that
/// destination avoids silently retrying into `_2` after a failed attempt has
/// already created part of the output folder/archive.
pub(super) fn parse_elevated_output(
    args: &[OsString],
    elevated_retry: bool,
) -> Result<(Option<PathBuf>, Vec<OsString>), String> {
    if !elevated_retry || args.first().map(OsString::as_os_str) != Some(OsStr::new("--output")) {
        return Ok((None, args.to_vec()));
    }
    let destination = args
        .get(1)
        .cloned()
        .map(PathBuf::from)
        .ok_or_else(|| "The elevated retry is missing its output path".to_owned())?;
    if args.len() < 3 {
        return Err("The elevated retry is missing its input path".to_owned());
    }
    Ok((Some(destination), args[2..].to_vec()))
}

/// Parses the repeated `--output <destination> <folder>` markers used by an
/// elevated per-folder compression retry. Normal invocations are just a list
/// of folder arguments and have no destination overrides.
pub(super) fn parse_elevated_batch_output(
    args: &[OsString],
    elevated_retry: bool,
) -> Result<Vec<(Option<PathBuf>, OsString)>, String> {
    if !elevated_retry {
        return Ok(args.iter().cloned().map(|source| (None, source)).collect());
    }

    let mut items = Vec::with_capacity(args.len() / 3);
    let mut index = 0;
    while index < args.len() {
        if args[index].as_os_str() != OsStr::new("--output") {
            return Err(
                "The elevated per-folder compression retry is missing an output marker".to_owned(),
            );
        }
        let Some(destination) = args.get(index + 1) else {
            return Err(
                "The elevated per-folder compression retry is missing an output path".to_owned(),
            );
        };
        let Some(source) = args.get(index + 2) else {
            return Err(
                "The elevated per-folder compression retry is missing a folder path".to_owned(),
            );
        };
        items.push((Some(PathBuf::from(destination)), source.clone()));
        index += 3;
    }
    Ok(items)
}

fn missing_path_message(path: &Path) -> String {
    format!(
        "No such file or folder: {}\n\nIf the name contains non-ASCII characters, use the Explorer right-click menu instead of a console so the exact Unicode name is preserved.",
        path.display()
    )
}

/// Resolves a CLI source path. Console codepages (for example CP949 on
/// Korean Windows) replace characters that are not in the codepage with
/// '?', so a name typed into cmd or PowerShell may not match the real file.
/// When the exact path is missing, scan the parent folder and accept the
/// single entry that matches with each '?' treated as a wildcard.
fn resolve_existing_path(requested: &Path) -> Option<PathBuf> {
    if requested.exists() {
        return Some(requested.to_path_buf());
    }
    let name = requested.file_name()?.to_string_lossy();
    if !name.contains('?') {
        return None;
    }
    let pattern = name.to_lowercase();
    let parent = requested.parent()?;
    let mut matches = Vec::new();
    for entry in std::fs::read_dir(parent).ok()?.flatten() {
        let candidate = entry.file_name().to_string_lossy().to_lowercase();
        if loose_name_matches(&pattern, &candidate) {
            matches.push(entry.path());
        }
    }
    (matches.len() == 1).then(|| matches.remove(0))
}

fn loose_name_matches(pattern: &str, candidate: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let candidate: Vec<char> = candidate.chars().collect();
    pattern.len() == candidate.len()
        && pattern
            .iter()
            .zip(&candidate)
            .all(|(pattern_char, candidate_char)| {
                *pattern_char == '?' || pattern_char == candidate_char
            })
}

/// Places the new archive next to the sources, naming it after the folder.
/// A single folder becomes `<folder>.<ext>` beside it; a single file uses the
/// file's own stem; several items use their common parent folder's name.
pub(crate) fn cli_archive_destination(sources: &[PathBuf], format: CreateFormat) -> PathBuf {
    let parent = common_parent_folder(sources);
    let stem = if sources.len() == 1 {
        let single = &sources[0];
        if single.is_dir() {
            single
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "archive".to_owned())
        } else {
            single
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "archive".to_owned())
        }
    } else {
        parent
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "archive".to_owned())
    };
    parent.join(format!("{stem}.{}", format.default_extension()))
}

pub(super) fn common_parent_folder(paths: &[PathBuf]) -> PathBuf {
    let mut common = paths[0]
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    for path in &paths[1..] {
        let mut ancestor = path.parent().unwrap_or_else(|| Path::new("."));
        while !common.starts_with(ancestor) {
            match ancestor.parent() {
                Some(parent) => ancestor = parent,
                None => return PathBuf::from("."),
            }
        }
        common = ancestor.to_path_buf();
    }
    common
}

/// Returns `path` when nothing exists there yet; otherwise appends `_2`, `_3`,
/// ... to the file stem (or folder name) until an unused name is found, e.g.
/// `보고서.zip` -> `보고서_2.zip`, `보고서\` -> `보고서_2\`.
pub(crate) fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().into_owned());
    for index in 2.. {
        let candidate = match &extension {
            Some(extension) => parent.join(format!("{stem}_{index}.{extension}")),
            None => parent.join(format!("{stem}_{index}")),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("the loop always finds a free name")
}

/// Conflict policy for Explorer context-menu operations: existing files are
/// simply overwritten because no interactive conflict dialog is shown there.
pub(super) struct OverwriteAllResolver;

impl ConflictResolver for OverwriteAllResolver {
    fn resolve(&self, _destination: &Path) -> ConflictChoice {
        ConflictChoice::OverwriteAll
    }
}
