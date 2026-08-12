use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use slint::{ComponentHandle, Model, ModelRc};

use crate::{
    AppWindow, CliProgressWindow,
    archive::{
        ArchiveEngine, ArchiveError, ArchiveResult, ConflictChoice, ConflictResolver, CreateFormat,
        CreateOptions, ExtractOptions, ExtractSelection, InitialConflictPolicy, ProgressSink,
        libarchive::LibArchiveEngine,
    },
    platform,
    tasks::{CancellationToken, ProgressSnapshot},
};

use super::AppState;

type Engine = Arc<dyn ArchiveEngine>;

// Font choices offered in Settings. The "auto" entry resolves at startup to
// Noto Sans JP when it is installed, otherwise to Yu Gothic.
const FONT_OPTIONS: &[(&str, &str)] = &[
    ("Auto (Noto Sans JP → Yu Gothic)", "auto"),
    ("Noto Sans JP", "Noto Sans JP"),
    ("Yu Gothic", "Yu Gothic"),
    ("Yu Gothic UI", "Yu Gothic UI"),
    ("Meiryo", "Meiryo"),
    ("Malgun Gothic", "Malgun Gothic"),
    ("Segoe UI", "Segoe UI"),
];

pub fn run() -> Result<(), String> {
    let mut args = std::env::args_os().skip(1);
    let command = args.next();
    let subcommand = command
        .as_deref()
        .and_then(|value| value.to_str())
        .map(|value| value.to_owned());
    match subcommand.as_deref() {
        Some("extract") => {
            let rest: Vec<OsString> = args.collect();
            run_cli_extract(&rest)
        }
        Some("zip") => {
            let rest: Vec<OsString> = args.collect();
            run_cli_create(&rest, CreateFormat::Zip)
        }
        Some("7z") => {
            let rest: Vec<OsString> = args.collect();
            run_cli_create(&rest, CreateFormat::SevenZip)
        }
        _ => run_with_startup_argument(command),
    }
}

fn run_with_startup_argument(startup_argument: Option<std::ffi::OsString>) -> Result<(), String> {
    if let Some(command) = startup_argument.as_deref().and_then(|value| value.to_str()) {
        match command {
            "--register" => {
                let executable = std::env::current_exe()
                    .map_err(|error| format!("Could not locate ArchiveRclick: {error}"))?;
                platform::register_file_associations(&executable)?;
                platform::show_info(
                    "ArchiveRclick",
                    "ArchiveRclick is registered as an archive handler. Choose it for the formats you want under Windows Settings > Apps > Default apps.",
                );
                return Ok(());
            }
            "--unregister" => {
                platform::unregister_file_associations()?;
                platform::show_info(
                    "ArchiveRclick",
                    "ArchiveRclick's per-user file-association registration was removed.",
                );
                return Ok(());
            }
            "--help" | "-h" | "/?" => {
                let help = "ArchiveRclick [archive|command]\n\nCommands:\n  extract <archive> [destination]  Extract into a subfolder named after the archive\n  zip <path>...                    Create a ZIP archive named after the source folder\n  7z <path>...                     Create a 7z archive named after the source folder\n\nOptions:\n  --register       Register as an available archive handler\n  --unregister     Remove that registration\n  --check-runtime  Verify the bundled archive engine and exit";
                if cfg!(test) {
                    println!("{help}");
                } else {
                    platform::show_info("ArchiveRclick command line", help);
                }
                return Ok(());
            }
            "--check-runtime" => {
                let engine = LibArchiveEngine::load().map_err(|error| error.to_string())?;
                if engine.writable_formats().is_empty() {
                    return Err(
                        "The loaded libarchive DLL does not expose archive creation support"
                            .to_owned(),
                    );
                }
                return Ok(());
            }
            _ => {}
        }
    }

    let engine: Engine = Arc::new(LibArchiveEngine::load().map_err(|error| error.to_string())?);
    let ui = AppWindow::new().map_err(|error| format!("Could not create the UI: {error}"))?;
    let state = Rc::new(AppState::new());

    ui.set_archive_rows(ModelRc::from(Rc::clone(&state.rows)));
    ui.set_archive_title("".into());
    ui.set_current_folder("".into());
    ui.set_status_text("Ready".into());
    ui.set_summary_text("No archive open".into());
    ui.set_libarchive_version(engine.version().into());
    ui.set_progress_value(-1.0);
    ui.set_create_source_summary("Choose files or a folder to archive".into());
    ui.set_create_destination("Choose after selecting Create…".into());
    let font_preference = platform::load_font_preference();
    let font_family = platform::resolve_font_family(&font_preference);
    ui.set_font_family(font_family.into());
    ui.set_font_options(ModelRc::from(
        FONT_OPTIONS
            .iter()
            .map(|(label, _)| slint::SharedString::from(*label))
            .collect::<Vec<_>>()
            .as_slice(),
    ));
    ui.set_font_selection(
        FONT_OPTIONS
            .iter()
            .position(|(_, key)| *key == font_preference.as_str())
            .unwrap_or(0) as i32,
    );
    let writable_formats = engine.writable_formats();
    if writable_formats.is_empty() {
        return Err(
            "The loaded libarchive DLL does not expose archive creation support".to_owned(),
        );
    }
    let format_labels = writable_formats
        .iter()
        .map(|format| slint::SharedString::from(format.label()))
        .collect::<Vec<_>>();
    ui.set_create_formats(ModelRc::from(format_labels.as_slice()));

    wire_callbacks(
        &ui,
        Rc::clone(&state),
        Arc::clone(&engine),
        writable_formats,
    );

    let weak = ui.as_weak();
    let state_for_drop = Rc::clone(&state);
    let engine_for_drop = Arc::clone(&engine);
    platform::install_file_drop_handler(
        ui.window(),
        Box::new(move |paths| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            handle_file_drop(&ui, &state_for_drop, &engine_for_drop, paths);
        }),
    );

    ui.show()
        .map_err(|error| format!("Could not show the UI: {error}"))?;

    if let Some(path) = startup_argument.map(PathBuf::from) {
        if path.is_file() {
            start_listing(&ui, Rc::clone(&state), Arc::clone(&engine), path, None);
        } else {
            ui.set_status_text(format!("Not a file: {}", path.display()).into());
        }
    }

    ui.run()
        .map_err(|error| format!("UI event loop failed: {error}"))
}


// ---------------------------------------------------------------------------
// Command-line subcommands (also used by the Explorer context-menu entries).
// ---------------------------------------------------------------------------

fn run_cli_extract(args: &[OsString]) -> Result<(), String> {
    let Some(archive_arg) = args.first() else {
        return Err("Usage: ArchiveRclick extract <archive> [destination]".to_owned());
    };
    let requested = PathBuf::from(archive_arg);
    let Some(archive) = resolve_existing_path(&requested) else {
        return Err(missing_path_message(&requested));
    };
    if !archive.is_file() {
        return Err(format!("Not an archive file: {}", archive.display()));
    }
    let destination = match args.get(1) {
        Some(destination) => PathBuf::from(destination),
        // Default: a subfolder named after the archive file, next to it.
        // If that folder already exists, pick the next free name (_2, _3 ...).
        None => unique_path(
            &archive
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(archive_directory_name(&archive)),
        ),
    };
    if cfg!(test) {
        cli_extract_headless(&archive, &destination)
    } else {
        let archive_for_operation = archive.clone();
        let destination_for_operation = destination.clone();
        run_cli_gui(
            "Extracting archive".to_owned(),
            move |progress, cancel| {
                let engine = LibArchiveEngine::load()?;
                let options = ExtractOptions {
                    selection: ExtractSelection::All,
                    // The CLI has no conflict dialog; existing files are overwritten.
                    conflict_policy: InitialConflictPolicy::OverwriteAll,
                    ..ExtractOptions::default()
                };
                engine.extract(
                    &archive_for_operation,
                    &destination_for_operation,
                    &options,
                    progress,
                    &CliConflictResolver,
                    cancel,
                )
            },
            move |summary| {
                format!(
                    "Extracted {} entries to {}",
                    summary.entries_processed,
                    destination.display()
                )
            },
        )
    }
}

fn cli_extract_headless(archive: &Path, destination: &Path) -> Result<(), String> {
    let engine = LibArchiveEngine::load().map_err(|error| error.to_string())?;
    let options = ExtractOptions {
        selection: ExtractSelection::All,
        conflict_policy: InitialConflictPolicy::OverwriteAll,
        ..ExtractOptions::default()
    };
    let cancel = CancellationToken::new();
    let progress = |_snapshot: ProgressSnapshot| {};
    let summary = engine
        .extract(
            archive,
            destination,
            &options,
            &progress,
            &CliConflictResolver,
            &cancel,
        )
        .map_err(|error| error.to_string())?;
    let message = format!(
        "Extracted {} entries to {}",
        summary.entries_processed,
        destination.display()
    );
    println!("{message}");
    platform::show_info("ArchiveRclick", &message);
    Ok(())
}

fn run_cli_create(args: &[OsString], format: CreateFormat) -> Result<(), String> {
    let verb = match format {
        CreateFormat::Zip => "zip",
        CreateFormat::SevenZip => "7z",
        _ => unreachable!("only zip and 7z are exposed as CLI subcommands"),
    };
    if args.is_empty() {
        return Err(format!("Usage: ArchiveRclick {verb} <file-or-folder>..."));
    }
    let mut sources: Vec<PathBuf> = Vec::with_capacity(args.len());
    for argument in args {
        let requested = PathBuf::from(argument);
        match resolve_existing_path(&requested) {
            Some(source) => sources.push(source),
            None => return Err(missing_path_message(&requested)),
        }
    }
    // When a file with the same name already exists, pick the next free name
    // (보고서.zip -> 보고서_2.zip -> 보고서_3.zip ...).
    let destination = unique_path(&cli_archive_destination(&sources, format));
    if cfg!(test) {
        cli_create_headless(&sources, &destination, format)
    } else {
        let destination_for_operation = destination.clone();
        run_cli_gui(
            format!("Creating {} archive", format.label()),
            move |progress, cancel| {
                let engine = LibArchiveEngine::load()?;
                let options = CreateOptions {
                    format,
                    ..CreateOptions::default()
                };
                engine.create(
                    &destination_for_operation,
                    &sources,
                    &options,
                    progress,
                    cancel,
                )
            },
            move |summary| {
                format!(
                    "Created {} ({} entries)",
                    destination.display(),
                    summary.entries_processed
                )
            },
        )
    }
}

fn cli_create_headless(
    sources: &[PathBuf],
    destination: &Path,
    format: CreateFormat,
) -> Result<(), String> {
    let engine = LibArchiveEngine::load().map_err(|error| error.to_string())?;
    let options = CreateOptions {
        format,
        ..CreateOptions::default()
    };
    let cancel = CancellationToken::new();
    let progress = |_snapshot: ProgressSnapshot| {};
    let summary = engine
        .create(destination, sources, &options, &progress, &cancel)
        .map_err(|error| error.to_string())?;
    let message = format!(
        "Created {} ({} entries)",
        destination.display(),
        summary.entries_processed
    );
    println!("{message}");
    platform::show_info("ArchiveRclick", &message);
    Ok(())
}

fn missing_path_message(path: &Path) -> String {
    format!(
        "No such file or folder: {}\n\nIf the name contains non-ASCII characters, use the Explorer right-click menu instead of a console so the exact Unicode name is preserved.",
        path.display()
    )
}

/// Runs a CLI operation behind a small progress window. The operation runs on
/// a worker thread and reports progress through the Slint event loop; the
/// window closes when the worker finishes.
fn run_cli_gui<T: Send + 'static>(
    title: String,
    operation: impl FnOnce(&dyn ProgressSink, &CancellationToken) -> ArchiveResult<T>
        + Send
        + 'static,
    summarize: impl FnOnce(T) -> String,
) -> Result<(), String> {
    let ui = CliProgressWindow::new()
        .map_err(|error| format!("Could not create the progress window: {error}"))?;
    ui.set_operation_title(title.into());
    ui.set_current_file("Starting…".into());
    ui.set_detail("".into());
    ui.set_value(-1.0);

    let cancel = CancellationToken::new();
    let cancel_for_ui = cancel.clone();
    ui.on_cancelled(move || cancel_for_ui.cancel());

    let weak = ui.as_weak();
    let weak_progress = weak.clone();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let progress = move |snapshot: ProgressSnapshot| {
            let detail = progress_detail(&snapshot);
            let value = snapshot.fraction();
            let current = snapshot.current_file;
            let _ = weak_progress.upgrade_in_event_loop(move |ui| {
                ui.set_current_file(current.into());
                ui.set_detail(detail.into());
                ui.set_value(value);
            });
        };
        let result = operation(&progress, &cancel);
        let _ = sender.send(result);
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let _ = ui.hide();
        });
    });

    ui.show()
        .map_err(|error| format!("Could not show the progress window: {error}"))?;
    ui.run()
        .map_err(|error| format!("Progress window event loop failed: {error}"))?;
    match receiver
        .recv()
        .map_err(|error| format!("Operation thread failed: {error}"))?
    {
        Ok(value) => {
            let message = summarize(value);
            println!("{message}");
            platform::show_info("ArchiveRclick", &message);
            Ok(())
        }
        Err(ArchiveError::Cancelled) => {
            platform::show_info("ArchiveRclick", "Operation cancelled");
            Ok(())
        }
        Err(error) => Err(error.to_string()),
    }
}

fn progress_detail(snapshot: &ProgressSnapshot) -> String {
    match (snapshot.total_entries, snapshot.total_bytes) {
        (Some(entries), Some(bytes)) => format!(
            "{} / {} entries · {} / {}",
            snapshot.entries_processed,
            entries,
            compact_bytes(snapshot.bytes_processed),
            compact_bytes(bytes)
        ),
        (Some(entries), _) => format!("{} / {} entries", snapshot.entries_processed, entries),
        _ => format!("{} entries", snapshot.entries_processed),
    }
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

fn common_parent_folder(paths: &[PathBuf]) -> PathBuf {
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

struct CliConflictResolver;

impl ConflictResolver for CliConflictResolver {
    fn resolve(&self, _destination: &Path) -> ConflictChoice {
        ConflictChoice::OverwriteAll
    }
}
fn handle_file_drop(ui: &AppWindow, state: &Rc<AppState>, engine: &Engine, paths: Vec<PathBuf>) {
    if ui.get_busy()
        || ui.get_password_visible()
        || ui.get_conflict_visible()
        || ui.get_create_visible()
        || ui.get_extract_visible()
        || ui.get_settings_visible()
    {
        ui.set_status_text("Finish or cancel the current action before dropping items".into());
        return;
    }
    if paths.is_empty()
        || paths
            .iter()
            .any(|path| !path.is_absolute() || (!path.is_file() && !path.is_dir()))
    {
        ui.set_status_text("The drop did not contain valid files or folders".into());
        return;
    }
    if paths.len() > 1 || paths.first().is_some_and(|path| path.is_dir()) {
        set_create_sources(ui, state, paths);
        return;
    }
    let Some(path) = paths.into_iter().next() else {
        ui.set_status_text("The drop did not contain a file or folder".into());
        return;
    };
    start_listing(ui, Rc::clone(state), Arc::clone(engine), path, None);
}

fn wire_callbacks(
    ui: &AppWindow,
    state: Rc<AppState>,
    engine: Engine,
    writable_formats: Vec<CreateFormat>,
) {
    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        let engine = Arc::clone(&engine);
        ui.on_open_requested(move || match platform::pick_archive() {
            Ok(Some(path)) => {
                if let Some(ui) = weak.upgrade() {
                    start_listing(&ui, Rc::clone(&state), Arc::clone(&engine), path, None);
                }
            }
            Ok(None) => {}
            Err(error) => show_ui_error(&weak, "Open archive", error),
        });
    }

    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_up_requested(move || {
            state.current_folder.borrow_mut().pop();
            state.rows.clear_selection();
            state.rebuild_display();
            update_folder_ui(&weak, &state);
        });
    }

    {
        let state = Rc::clone(&state);
        ui.on_toggle_selection(move |row| {
            if row >= 0 {
                state.rows.toggle(row as usize);
            }
        });
    }

    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_activate_row(move |row| {
            if row < 0 {
                return;
            }
            if let Some(folder) = state.activate_row(row as usize) {
                *state.current_folder.borrow_mut() = folder;
                state.rows.clear_selection();
                state.rebuild_display();
                update_folder_ui(&weak, &state);
            }
        });
    }

    {
        let state = Rc::clone(&state);
        ui.on_sort_requested(move |column| {
            let column = column.max(0) as usize;
            if state.sort_column.get() == column {
                state.sort_ascending.set(!state.sort_ascending.get());
            } else {
                state.sort_column.set(column);
                state.sort_ascending.set(true);
            }
            state.rebuild_display();
        });
    }

    ui.on_show_extract_requested(|| {});

    {
        let weak = ui.as_weak();
        ui.on_settings_requested(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_settings_visible(true);
            }
        });
    }

    {
        let weak = ui.as_weak();
        ui.on_settings_applied(move |selection| {
            let preference = FONT_OPTIONS
                .get(selection.max(0) as usize)
                .map(|(_, key)| *key)
                .unwrap_or("auto");
            if let Err(error) = platform::save_font_preference(preference) {
                if let Some(ui) = weak.upgrade() {
                    ui.set_status_text(format!("Could not save settings: {error}").into());
                }
                return;
            }
            let family = platform::resolve_font_family(preference);
            if let Some(ui) = weak.upgrade() {
                ui.set_font_family(family.into());
            }
        });
    }

    {
        let weak = ui.as_weak();
        ui.on_settings_cancelled(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_settings_visible(false);
            }
        });
    }

    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        let engine = Arc::clone(&engine);
        ui.on_extract_requested(
            move |destination_mode, conflict_policy, password, selected_only| {
                let Some(listing) = state.listing.borrow().as_ref().cloned() else {
                    return;
                };
                let destination =
                    match extraction_destination(&listing.archive_path, destination_mode) {
                        Ok(Some(path)) => path,
                        Ok(None) => return,
                        Err(error) => {
                            show_ui_error(&weak, "Extract archive", error);
                            return;
                        }
                    };

                let selection = if selected_only {
                    let paths = state.selected_paths();
                    if paths.is_empty() {
                        show_ui_error(
                            &weak,
                            "Extract archive",
                            "No entries are selected".to_owned(),
                        );
                        return;
                    }
                    ExtractSelection::Paths(paths)
                } else {
                    ExtractSelection::All
                };

                let options = ExtractOptions {
                    selection,
                    password: (!password.is_empty()).then(|| password.to_string()),
                    conflict_policy: match conflict_policy {
                        1 => InitialConflictPolicy::OverwriteAll,
                        2 => InitialConflictPolicy::SkipAll,
                        _ => InitialConflictPolicy::Ask,
                    },
                    ..ExtractOptions::default()
                };
                if let Some(ui) = weak.upgrade() {
                    start_extract(
                        &ui,
                        Rc::clone(&state),
                        Arc::clone(&engine),
                        listing.archive_path,
                        destination,
                        options,
                    );
                }
            },
        );
    }

    {
        let weak = ui.as_weak();
        ui.on_prepare_create_requested(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_create_visible(true);
            }
        });
    }

    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_choose_create_files_requested(move || match platform::pick_files() {
            Ok(Some(paths)) if !paths.is_empty() => {
                if let Some(ui) = weak.upgrade() {
                    set_create_sources(&ui, &state, paths);
                }
            }
            Ok(_) => {}
            Err(error) => show_ui_error(&weak, "Choose archive inputs", error),
        });
    }

    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_choose_create_folder_requested(move || {
            match platform::pick_folder("Choose folder to archive") {
                Ok(Some(path)) => {
                    if let Some(ui) = weak.upgrade() {
                        set_create_sources(&ui, &state, vec![path]);
                    }
                }
                Ok(None) => {}
                Err(error) => show_ui_error(&weak, "Choose archive folder", error),
            }
        });
    }

    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        let engine = Arc::clone(&engine);
        ui.on_create_requested(move |format_index, level, password| {
            let sources = state.pending_create_sources.borrow().clone();
            if sources.is_empty() {
                show_ui_error(
                    &weak,
                    "Create archive",
                    "No input files were selected".to_owned(),
                );
                return;
            }
            let Some(format) = writable_formats.get(format_index.max(0) as usize).copied() else {
                show_ui_error(
                    &weak,
                    "Create archive",
                    "Unsupported archive format".to_owned(),
                );
                return;
            };
            let default_name = default_archive_name(&sources, format);
            let destination =
                match platform::save_archive(&default_name, format.default_extension()) {
                    Ok(Some(path)) => path,
                    Ok(None) => return,
                    Err(error) => {
                        show_ui_error(&weak, "Create archive", error);
                        return;
                    }
                };
            let options = CreateOptions {
                format,
                compression_level: level.clamp(0, 9) as u8,
                password: (!password.is_empty()).then(|| password.to_string()),
            };
            if let Some(ui) = weak.upgrade() {
                start_create(
                    &ui,
                    Rc::clone(&state),
                    Arc::clone(&engine),
                    destination,
                    sources,
                    options,
                );
            }
        });
    }

    {
        let state = Rc::clone(&state);
        let engine = Arc::clone(&engine);
        let weak = ui.as_weak();
        ui.on_test_requested(move || {
            let Some(path) = state
                .listing
                .borrow()
                .as_ref()
                .map(|listing| listing.archive_path.clone())
            else {
                return;
            };
            if let Some(ui) = weak.upgrade() {
                start_test(
                    &ui,
                    Rc::clone(&state),
                    Arc::clone(&engine),
                    path,
                    state
                        .open_password
                        .lock()
                        .expect("password mutex poisoned")
                        .clone(),
                );
            }
        });
    }

    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        let engine = Arc::clone(&engine);
        ui.on_password_response(move |password, accepted| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            ui.set_password_visible(false);
            if !accepted || password.is_empty() {
                *state
                    .pending_password_path
                    .lock()
                    .expect("password-path mutex poisoned") = None;
                *state
                    .pending_test_password_path
                    .lock()
                    .expect("test-password-path mutex poisoned") = None;
                ui.set_status_text("Password entry cancelled".into());
                return;
            }
            let password = password.to_string();
            let pending_test = state
                .pending_test_password_path
                .lock()
                .expect("test-password-path mutex poisoned")
                .take();
            if let Some(path) = pending_test {
                start_test(
                    &ui,
                    Rc::clone(&state),
                    Arc::clone(&engine),
                    path,
                    Some(password),
                );
                return;
            }
            let pending = state
                .pending_password_path
                .lock()
                .expect("password-path mutex poisoned")
                .take();
            let Some(path) = pending.or_else(|| {
                state
                    .listing
                    .borrow()
                    .as_ref()
                    .map(|listing| listing.archive_path.clone())
            }) else {
                return;
            };
            start_listing(
                &ui,
                Rc::clone(&state),
                Arc::clone(&engine),
                path,
                Some(password),
            );
        });
    }

    {
        let cancellation = Arc::clone(&state.cancellation);
        let pending_conflict = Arc::clone(&state.pending_conflict);
        let weak = ui.as_weak();
        ui.on_cancel_requested(move || {
            if let Some(token) = cancellation
                .lock()
                .expect("cancellation mutex poisoned")
                .as_ref()
            {
                token.cancel();
            }
            if let Some(sender) = pending_conflict
                .lock()
                .expect("conflict mutex poisoned")
                .take()
            {
                let _ = sender.send(ConflictChoice::Cancel);
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_status_text("Cancelling…".into());
                ui.set_conflict_visible(false);
            }
        });
    }

    {
        let pending_conflict = Arc::clone(&state.pending_conflict);
        let weak = ui.as_weak();
        ui.on_conflict_response(move |response| {
            let choice = match response {
                0 => ConflictChoice::Overwrite,
                1 => ConflictChoice::Skip,
                2 => ConflictChoice::OverwriteAll,
                3 => ConflictChoice::SkipAll,
                _ => ConflictChoice::Cancel,
            };
            if let Some(sender) = pending_conflict
                .lock()
                .expect("conflict mutex poisoned")
                .take()
            {
                let _ = sender.send(choice);
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_conflict_visible(false);
            }
        });
    }

    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        let engine = Arc::clone(&engine);
        ui.on_dropped(move |data| {
            let Ok(text) = data.plain_text() else {
                if let Some(ui) = weak.upgrade() {
                    ui.set_status_text("This drag did not contain a readable file path".into());
                }
                return;
            };
            let Some(path) = parse_dropped_path(&text) else {
                if let Some(ui) = weak.upgrade() {
                    ui.set_status_text("The dropped value is not a local file".into());
                }
                return;
            };
            if !path.is_file() {
                if let Some(ui) = weak.upgrade() {
                    ui.set_status_text(format!("Not a file: {}", path.display()).into());
                }
                return;
            }
            if let Some(ui) = weak.upgrade() {
                start_listing(&ui, Rc::clone(&state), Arc::clone(&engine), path, None);
            }
        });
    }
}

fn start_listing(
    ui: &AppWindow,
    state: Rc<AppState>,
    engine: Engine,
    path: PathBuf,
    password: Option<String>,
) {
    let cancel = begin_operation(ui, &state, "Opening archive", &path.display().to_string());
    let open_password = Arc::clone(&state.open_password);
    let pending_password_path = Arc::clone(&state.pending_password_path);
    let weak = ui.as_weak();
    let weak_progress = weak.clone();
    let progress = move |snapshot: ProgressSnapshot| update_progress(&weak_progress, snapshot);
    std::thread::spawn(move || {
        let result = engine
            .list(&path, password.as_deref(), &progress, &cancel)
            .map(super::ArchiveRowModel::prepare_listing);
        let _ = weak.upgrade_in_event_loop(move |ui| {
            finish_operation(&ui);
            match result {
                Ok((listing, display)) => {
                    let entry_count = listing.entries.len();
                    let total = listing.total_uncompressed_size;
                    let format_name = listing.format_name.clone();
                    let archive_name = listing
                        .archive_path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| listing.archive_path.display().to_string());
                    if let Some(rows) = ui
                        .get_archive_rows()
                        .as_any()
                        .downcast_ref::<super::ArchiveRowModel>()
                    {
                        rows.set_prepared_listing(listing, display);
                    } else {
                        display_operation_error(
                            &ui,
                            "Open archive",
                            ArchiveError::Worker("archive row model is unavailable".to_owned()),
                        );
                        return;
                    }
                    ui.set_archive_title(archive_name.into());
                    ui.set_current_folder("".into());
                    ui.set_has_archive(true);
                    ui.set_can_go_up(false);
                    ui.set_status_text(format!("{format_name} archive").into());
                    ui.set_summary_text(
                        format!("{entry_count} entries  •  {}", compact_bytes(total)).into(),
                    );
                    *open_password.lock().expect("password mutex poisoned") = password;
                    *pending_password_path
                        .lock()
                        .expect("password-path mutex poisoned") = None;
                }
                Err(ArchiveError::PasswordRequired) => {
                    *pending_password_path
                        .lock()
                        .expect("password-path mutex poisoned") = Some(path.clone());
                    ui.set_progress_visible(false);
                    ui.set_busy(false);
                    ui.set_password_value("".into());
                    ui.set_password_operation(
                        format!("Enter the password for {}", path.display()).into(),
                    );
                    ui.set_password_visible(true);
                    ui.set_status_text("Password required".into());
                }
                Err(error) => display_operation_error(&ui, "Open archive", error),
            }
        });
    });
}

fn start_extract(
    ui: &AppWindow,
    state: Rc<AppState>,
    engine: Engine,
    archive: PathBuf,
    destination: PathBuf,
    options: ExtractOptions,
) {
    let mut options = options;
    let (hint_entries, hint_bytes) =
        extraction_progress_hints(state.listing.borrow().as_ref(), &options.selection);
    options.total_entries_hint = hint_entries;
    options.total_bytes_hint = hint_bytes;
    let cancel = begin_operation(
        ui,
        &state,
        "Extracting archive",
        &archive.display().to_string(),
    );
    let resolver = UiConflictResolver {
        weak: ui.as_weak(),
        pending: Arc::clone(&state.pending_conflict),
        cancel: cancel.clone(),
    };
    let weak = ui.as_weak();
    let weak_progress = weak.clone();
    let progress = move |snapshot: ProgressSnapshot| update_progress(&weak_progress, snapshot);
    std::thread::spawn(move || {
        let result = engine.extract(
            &archive,
            &destination,
            &options,
            &progress,
            &resolver,
            &cancel,
        );
        let destination_for_ui = destination.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            finish_operation(&ui);
            match result {
                Ok(summary) => {
                    ui.set_status_text(
                        format!(
                            "Extracted {} entries to {}",
                            summary.entries_processed,
                            destination_for_ui.display()
                        )
                        .into(),
                    );
                }
                Err(error) => display_operation_error(&ui, "Extract archive", error),
            }
        });
    });
}

fn start_create(
    ui: &AppWindow,
    state: Rc<AppState>,
    engine: Engine,
    destination: PathBuf,
    sources: Vec<PathBuf>,
    options: CreateOptions,
) {
    let cancel = begin_operation(
        ui,
        &state,
        "Creating archive",
        &destination.display().to_string(),
    );
    let weak = ui.as_weak();
    let weak_progress = weak.clone();
    let progress = move |snapshot: ProgressSnapshot| update_progress(&weak_progress, snapshot);
    std::thread::spawn(move || {
        let result = engine.create(&destination, &sources, &options, &progress, &cancel);
        let destination_for_ui = destination.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            finish_operation(&ui);
            match result {
                Ok(summary) => {
                    ui.set_status_text(
                        format!(
                            "Created {} ({} entries)",
                            destination_for_ui.display(),
                            summary.entries_processed
                        )
                        .into(),
                    );
                }
                Err(error) => display_operation_error(&ui, "Create archive", error),
            }
        });
    });
}

fn start_test(
    ui: &AppWindow,
    state: Rc<AppState>,
    engine: Engine,
    archive: PathBuf,
    password: Option<String>,
) {
    let cancel = begin_operation(
        ui,
        &state,
        "Testing archive",
        &archive.display().to_string(),
    );
    let weak = ui.as_weak();
    let weak_progress = weak.clone();
    let open_password = Arc::clone(&state.open_password);
    let pending_test_password_path = Arc::clone(&state.pending_test_password_path);
    let progress = move |snapshot: ProgressSnapshot| update_progress(&weak_progress, snapshot);
    std::thread::spawn(move || {
        let result = engine.test(&archive, password.as_deref(), &progress, &cancel);
        let _ = weak.upgrade_in_event_loop(move |ui| {
            finish_operation(&ui);
            match result {
                Ok(summary) => {
                    *open_password.lock().expect("password mutex poisoned") = password;
                    ui.set_status_text(
                        format!(
                            "Archive is OK ({} entries tested)",
                            summary.entries_processed
                        )
                        .into(),
                    );
                }
                Err(ArchiveError::PasswordRequired) => {
                    *pending_test_password_path
                        .lock()
                        .expect("test-password-path mutex poisoned") = Some(archive.clone());
                    ui.set_password_value("".into());
                    ui.set_password_operation(
                        format!("Enter the password to test {}", archive.display()).into(),
                    );
                    ui.set_password_visible(true);
                    ui.set_status_text("Password required".into());
                }
                Err(error) => display_operation_error(&ui, "Test archive", error),
            }
        });
    });
}

fn extraction_progress_hints(
    listing: Option<&crate::archive::ArchiveListing>,
    selection: &ExtractSelection,
) -> (Option<u64>, Option<u64>) {
    let Some(listing) = listing else {
        return (None, None);
    };
    let mut entries = 0u64;
    let mut bytes = 0u64;
    for entry in &listing.entries {
        let path = normalize_hint_path(&entry.path);
        if selection.includes(&path) {
            entries = entries.saturating_add(1);
            bytes = bytes.saturating_add(entry.size.unwrap_or(0));
        }
    }
    (Some(entries), Some(bytes))
}

fn normalize_hint_path(path: &Path) -> PathBuf {
    path.to_string_lossy()
        .replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect()
}

fn begin_operation(
    ui: &AppWindow,
    state: &AppState,
    title: &str,
    current_file: &str,
) -> CancellationToken {
    let cancel = CancellationToken::new();
    *state
        .cancellation
        .lock()
        .expect("cancellation mutex poisoned") = Some(cancel.clone());
    ui.set_busy(true);
    ui.set_progress_visible(true);
    ui.set_progress_title(title.into());
    ui.set_progress_file(current_file.into());
    ui.set_progress_detail("Starting…".into());
    ui.set_progress_value(-1.0);
    cancel
}

fn finish_operation(ui: &AppWindow) {
    ui.set_busy(false);
    ui.set_progress_visible(false);
    ui.set_conflict_visible(false);
}

fn update_progress(weak: &slint::Weak<AppWindow>, snapshot: ProgressSnapshot) {
    let weak = weak.clone();
    let _ = weak.upgrade_in_event_loop(move |ui| {
        ui.set_progress_title(snapshot.phase.label().into());
        ui.set_progress_file(snapshot.current_file.clone().into());
        ui.set_progress_detail(
            format!(
                "{} entries  •  {} processed",
                snapshot.entries_processed,
                compact_bytes(snapshot.bytes_processed)
            )
            .into(),
        );
        let fraction = if snapshot.total_bytes.is_some() || snapshot.total_entries.is_some() {
            snapshot.fraction()
        } else {
            -1.0
        };
        ui.set_progress_value(fraction);
    });
}

struct UiConflictResolver {
    weak: slint::Weak<AppWindow>,
    pending: Arc<Mutex<Option<mpsc::SyncSender<ConflictChoice>>>>,
    cancel: CancellationToken,
}

impl ConflictResolver for UiConflictResolver {
    fn resolve(&self, destination: &Path) -> ConflictChoice {
        let (sender, receiver) = mpsc::sync_channel(1);
        *self.pending.lock().expect("conflict mutex poisoned") = Some(sender);
        let display = destination.display().to_string();
        let _ = self.weak.clone().upgrade_in_event_loop(move |ui| {
            ui.set_conflict_path(display.into());
            ui.set_conflict_visible(true);
        });

        loop {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(choice) => return choice,
                Err(mpsc::RecvTimeoutError::Timeout) if !self.cancel.is_cancelled() => {}
                Err(_) => return ConflictChoice::Cancel,
            }
        }
    }
}

fn extraction_destination(archive: &Path, mode: i32) -> Result<Option<PathBuf>, String> {
    let parent = archive.parent().unwrap_or_else(|| Path::new("."));
    match mode {
        0 => platform::pick_folder("Extract to"),
        1 => Ok(Some(parent.to_path_buf())),
        _ => {
            let name = archive_directory_name(archive);
            Ok(Some(parent.join(name)))
        }
    }
}

fn set_create_sources(ui: &AppWindow, state: &AppState, paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    let summary = if paths.len() == 1 {
        paths[0].display().to_string()
    } else {
        format!("{} items selected", paths.len())
    };
    *state.pending_create_sources.borrow_mut() = paths;
    ui.set_create_source_summary(summary.into());
    ui.set_create_has_sources(true);
    ui.set_create_visible(true);
}

fn archive_directory_name(archive: &Path) -> String {
    let mut name = archive
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "extracted".to_owned());
    for extension in [".tar.zst", ".tar.xz", ".tar.gz", ".tar.bz2"] {
        if name.to_ascii_lowercase().ends_with(extension) {
            name.truncate(name.len() - extension.len());
            return if name.is_empty() {
                "extracted".to_owned()
            } else {
                name
            };
        }
    }
    Path::new(&name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "extracted".to_owned())
}

fn default_archive_name(sources: &[PathBuf], format: CreateFormat) -> String {
    let stem = sources
        .first()
        .and_then(|path| path.file_stem())
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "archive".to_owned());
    format!("{stem}.{}", format.default_extension())
}

fn update_folder_ui(weak: &slint::Weak<AppWindow>, state: &AppState) {
    if let Some(ui) = weak.upgrade() {
        let current = state.current_folder.borrow();
        ui.set_current_folder(current.to_string_lossy().replace('\\', "/").into());
        ui.set_can_go_up(!current.as_os_str().is_empty());
    }
}

fn display_operation_error(ui: &AppWindow, title: &str, error: ArchiveError) {
    let message = error.to_string();
    ui.set_status_text(message.clone().into());
    platform::show_error(title, &message);
}

fn show_ui_error(weak: &slint::Weak<AppWindow>, title: &str, message: String) {
    if let Some(ui) = weak.upgrade() {
        ui.set_status_text(message.clone().into());
    }
    platform::show_error(title, &message);
}

fn compact_bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = value as f64;
    let mut unit = 0usize;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn parse_dropped_path(text: &str) -> Option<PathBuf> {
    let first = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    if let Some(uri) = first.strip_prefix("file:///") {
        return percent_decode(uri).map(|path| PathBuf::from(path.replace('/', "\\")));
    }
    if first.starts_with("file://") {
        return None;
    }
    Some(PathBuf::from(first.trim_matches('"')))
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = *bytes.get(index + 1)?;
            let low = *bytes.get(index + 2)?;
            decoded.push(hex(high)? << 4 | hex(low)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        archive_directory_name, cli_archive_destination, common_parent_folder, parse_dropped_path,
        run_with_startup_argument, unique_path,
    };
    use crate::archive::CreateFormat;
    use std::path::{Path, PathBuf};

    #[test]
    fn strips_compound_archive_extensions() {
        assert_eq!(
            archive_directory_name(Path::new("backup.tar.zst")),
            "backup"
        );
        assert_eq!(archive_directory_name(Path::new("docs.zip")), "docs");
    }

    #[test]
    fn parses_file_uri_drop_text() {
        assert_eq!(
            parse_dropped_path("file:///C:/Temp/My%20Archive.zip\r\n"),
            Some(PathBuf::from(r"C:\Temp\My Archive.zip"))
        );
    }

    #[test]
    fn help_command_does_not_load_libarchive() {
        assert!(run_with_startup_argument(Some("--help".into())).is_ok());
    }

    #[test]
    fn cli_archive_destination_names_archive_after_folder() {
        // Single folder -> sibling archive named after the folder.
        assert_eq!(
            cli_archive_destination(&[PathBuf::from("data/photos")], CreateFormat::Zip),
            PathBuf::from("data/photos.zip")
        );
        // Single file -> same folder, named after the file's stem.
        assert_eq!(
            cli_archive_destination(&[PathBuf::from("data/notes.txt")], CreateFormat::SevenZip),
            PathBuf::from("data/notes.7z")
        );
    }

    #[test]
    fn cli_archive_destination_names_multi_selection_after_parent() {
        // Multiple items -> named after their common parent folder.
        let sources = vec![
            PathBuf::from("data/photos/a.jpg"),
            PathBuf::from("data/photos/b.jpg"),
        ];
        assert_eq!(
            cli_archive_destination(&sources, CreateFormat::Zip),
            PathBuf::from("data/photos/photos.zip")
        );
    }

    #[test]
    fn common_parent_folder_handles_mixed_depths() {
        let sources = vec![
            PathBuf::from("data/photos/a.jpg"),
            PathBuf::from("data/photos/2025/b.jpg"),
        ];
        assert_eq!(
            common_parent_folder(&sources),
            PathBuf::from("data/photos")
        );
    }

    #[test]
    fn unique_path_appends_suffix_when_name_taken() {
        let dir = std::env::temp_dir().join(format!("archive-rclick-unique-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let first = dir.join("보고서.zip");
        std::fs::write(&first, b"x").expect("write first");
        assert_eq!(unique_path(&first), dir.join("보고서_2.zip"));

        let second = dir.join("보고서_2.zip");
        std::fs::write(&second, b"x").expect("write second");
        assert_eq!(unique_path(&first), dir.join("보고서_3.zip"));

        let unused = dir.join("새파일.zip");
        assert_eq!(unique_path(&unused), unused);

        // Folder targets get the same suffix treatment.
        std::fs::create_dir_all(dir.join("보고서")).expect("create folder");
        assert_eq!(unique_path(&dir.join("보고서")), dir.join("보고서_2"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}