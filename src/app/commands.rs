use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use slint::{ComponentHandle, Model, ModelRc};

use crate::{
    AppWindow, ProgressWindow,
    archive::{
        ArchiveEngine, ArchiveError, CompositeEngine, ConflictChoice, ConflictResolver,
        CreateFormat, CreateOptions, ExtractOptions, ExtractSelection, InitialConflictPolicy,
        SevenZipEngine, ThreadCount, libarchive::LibArchiveEngine,
    },
    platform,
    tasks::{CancellationToken, ProgressSnapshot},
};

use super::AppState;

type Engine = Arc<dyn ArchiveEngine>;

/// Builds the shared archive engine: 7z archives are handled by the bundled
/// 7z.dll (multicore LZMA2), every other format by libarchive. When 7z.dll
/// cannot be loaded, the composite still serves libarchive formats and 7z
/// operations fail with a clear error.
fn load_engine() -> Result<Engine, String> {
    let libarchive = LibArchiveEngine::load().map_err(|error| error.to_string())?;
    let sevenzip = match SevenZipEngine::load() {
        Ok(engine) => Some(engine),
        Err(error) => {
            eprintln!("7z.dll unavailable; 7z archives will not open: {error}");
            None
        }
    };
    Ok(Arc::new(CompositeEngine::new(libarchive, sevenzip)))
}

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
            run_gui_extract(&rest)
        }
        Some("zip") => {
            let rest: Vec<OsString> = args.collect();
            run_gui_create(&rest, CreateFormat::Zip)
        }
        Some("7z") => {
            let rest: Vec<OsString> = args.collect();
            run_gui_create(&rest, CreateFormat::SevenZip)
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
                let help = "ArchiveRclick [archive|command]\n\nCommands:\n  extract <archive>...  Extract each archive into its own subfolder\n  zip <path>...         Create a ZIP archive named after the source folder\n  7z <path>...          Create a 7z archive named after the source folder\n\nOptions:\n  --register       Register as an available archive handler\n  --unregister     Remove that registration\n  --check-runtime  Verify the bundled archive engine and exit";
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
                SevenZipEngine::load().map_err(|error| {
                    format!("The bundled 7z.dll could not be loaded: {error}")
                })?;
                return Ok(());
            }
            _ => {}
        }
    }

    let (ui, state, engine, _writable_formats) = open_main_window()?;

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

/// Builds the main application window and wires it up; shared by the
/// interactive startup path and the Explorer context-menu operations.
fn open_main_window() -> Result<(AppWindow, Rc<AppState>, Engine, Vec<CreateFormat>), String> {
    let engine: Engine = load_engine()?;
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
    ui.set_context_menu_state(context_menu_state_text().into());
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
        writable_formats.clone(),
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
    Ok((ui, state, engine, writable_formats))
}


// ---------------------------------------------------------------------------
// Explorer context-menu operations. The shell extension launches the app with
// these verbs and the work runs in a small progress-only window: no main
// window appears, and the window closes by itself once the work is finished.
// ---------------------------------------------------------------------------

fn run_gui_extract(args: &[OsString]) -> Result<(), String> {
    if args.is_empty() {
        return Err("Usage: ArchiveRclick extract <archive>...".to_owned());
    }
    let mut archives: Vec<PathBuf> = Vec::with_capacity(args.len());
    for argument in args {
        let requested = PathBuf::from(argument);
        match resolve_existing_path(&requested) {
            Some(path) if path.is_file() => archives.push(path),
            Some(path) => return Err(format!("Not an archive file: {}", path.display())),
            None => return Err(missing_path_message(&requested)),
        }
    }
    let engine: Engine = load_engine()?;
    let (ui, state) = open_progress_window()?;
    start_extract_batch_window(&ui, &state, Arc::clone(&engine), archives);
    ui.run()
        .map_err(|error| format!("UI event loop failed: {error}"))
}

fn run_gui_create(args: &[OsString], format: CreateFormat) -> Result<(), String> {
    let verb = match format {
        CreateFormat::Zip => "zip",
        CreateFormat::SevenZip => "7z",
        _ => unreachable!("only zip and 7z reach the context-menu create flow"),
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
    let engine: Engine = load_engine()?;
    let options = CreateOptions {
        format,
        threads: ThreadCount::from_registry_key(&platform::load_thread_preference()),
        ..CreateOptions::default()
    };
    let (ui, state) = open_progress_window()?;
    start_create_window(&ui, &state, Arc::clone(&engine), destination, sources, options);
    ui.run()
        .map_err(|error| format!("UI event loop failed: {error}"))
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

/// Conflict policy for Explorer context-menu operations: existing files are
/// simply overwritten because no interactive conflict dialog is shown there.
struct OverwriteAllResolver;

impl ConflictResolver for OverwriteAllResolver {
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
                ui.set_context_menu_state(context_menu_state_text().into());
                ui.set_settings_visible(true);
            }
        });
    }

    {
        let weak = ui.as_weak();
        ui.on_context_menu_register_requested(move || {
            let result = context_menu_dll_path()
                .and_then(|dll| platform::shell_ext::register_context_menu(&dll));
            let state_text = context_menu_state_text();
            if let Some(ui) = weak.upgrade() {
                ui.set_context_menu_state(state_text.into());
                match result {
                    Ok(()) => {
                        ui.set_status_text("Explorer right-click menu registered".into());
                    }
                    Err(error) => {
                        ui.set_status_text(error.clone().into());
                        platform::show_error("Register right-click menu", &error);
                    }
                }
            }
        });
    }

    {
        let weak = ui.as_weak();
        ui.on_context_menu_unregister_requested(move || {
            let result = platform::shell_ext::unregister_context_menu();
            let state_text = context_menu_state_text();
            if let Some(ui) = weak.upgrade() {
                ui.set_context_menu_state(state_text.into());
                match result {
                    Ok(()) => {
                        ui.set_status_text("Explorer right-click menu removed".into());
                    }
                    Err(error) => {
                        ui.set_status_text(error.clone().into());
                        platform::show_error("Remove right-click menu", &error);
                    }
                }
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
                let thread_selection =
                    ThreadCount::from_registry_key(&platform::load_thread_preference()).ui_index();
                ui.set_create_thread_selection(thread_selection);
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
        ui.on_create_requested(move |format_index, level, thread_index, password| {
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
                threads: ThreadCount::from_ui_index(thread_index),
            };
            let _ = platform::save_thread_preference(options.threads.registry_key());
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

// ---------------------------------------------------------------------------
// Progress-only window used by the Explorer right-click verbs. The shell
// extension launches the app with "extract"/"zip"/"7z" and this window is the
// only UI: no main window appears, and the window closes by itself once the
// operation is finished.
// ---------------------------------------------------------------------------

struct ProgressWindowState {
    cancellation: Arc<Mutex<Option<CancellationToken>>>,
}

/// Builds and shows the small progress window; its Cancel button cancels the
/// running operation.
fn open_progress_window() -> Result<(ProgressWindow, Rc<ProgressWindowState>), String> {
    let ui = ProgressWindow::new().map_err(|error| format!("Could not create the UI: {error}"))?;
    let state = Rc::new(ProgressWindowState {
        cancellation: Arc::new(Mutex::new(None)),
    });
    let state_for_cancel = Rc::clone(&state);
    ui.on_cancel_requested(move || {
        if let Some(token) = state_for_cancel
            .cancellation
            .lock()
            .expect("cancellation mutex poisoned")
            .as_ref()
        {
            token.cancel();
        }
    });
    let font_family = platform::resolve_font_family(&platform::load_font_preference());
    ui.set_font_family(font_family.into());
    ui.set_progress_title("Working…".into());
    ui.set_progress_file("".into());
    ui.set_progress_detail("Starting…".into());
    ui.set_progress_value(-1.0);
    ui.show()
        .map_err(|error| format!("Could not show the progress window: {error}"))?;
    Ok((ui, state))
}

/// Hides the progress window and ends the event loop so the process exits.
fn close_progress_window(ui: &ProgressWindow) {
    let _ = ui.hide();
    let _ = slint::quit_event_loop();
}

fn apply_progress_window(ui: &ProgressWindow, snapshot: &ProgressSnapshot) {
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
}

fn update_progress_window(weak: &slint::Weak<ProgressWindow>, snapshot: ProgressSnapshot) {
    let weak = weak.clone();
    let _ = weak.upgrade_in_event_loop(move |ui| {
        ui.set_progress_title(snapshot.phase.label().into());
        apply_progress_window(&ui, &snapshot);
    });
}

/// Like [`update_progress_window`], but keeps the operation title so that a
/// batch operation can show "Extracting archive 2/3" while entries stream in.
fn update_progress_window_details(
    weak: &slint::Weak<ProgressWindow>,
    snapshot: ProgressSnapshot,
) {
    let weak = weak.clone();
    let _ = weak.upgrade_in_event_loop(move |ui| apply_progress_window(&ui, &snapshot));
}

/// Extracts each archive into its own `<archive-name>` subfolder inside the
/// progress-only window; the window closes when the batch finishes.
fn start_extract_batch_window(
    ui: &ProgressWindow,
    state: &Rc<ProgressWindowState>,
    engine: Engine,
    archives: Vec<PathBuf>,
) {
    let total = archives.len();
    let first = archives[0].display().to_string();
    let cancel = CancellationToken::new();
    *state
        .cancellation
        .lock()
        .expect("cancellation mutex poisoned") = Some(cancel.clone());
    ui.set_progress_title("Extracting archives".into());
    ui.set_progress_file(first.into());
    ui.set_progress_detail("Starting…".into());
    ui.set_progress_value(-1.0);
    let weak = ui.as_weak();
    let weak_progress = weak.clone();
    std::thread::spawn(move || {
        let mut processed = 0usize;
        let mut failures: Vec<(PathBuf, ArchiveError)> = Vec::new();
        for (index, archive) in archives.iter().enumerate() {
            if cancel.is_cancelled() {
                break;
            }
            let title = format!("Extracting archive {}/{}", index + 1, total);
            let archive_display = archive.display().to_string();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_progress_title(title.into());
                ui.set_progress_file(archive_display.into());
                ui.set_progress_detail("Starting…".into());
                ui.set_progress_value(-1.0);
            });
            let parent = archive.parent().unwrap_or_else(|| Path::new("."));
            let destination = unique_path(&parent.join(archive_directory_name(archive)));
            let options = ExtractOptions {
                selection: ExtractSelection::All,
                conflict_policy: InitialConflictPolicy::OverwriteAll,
                ..ExtractOptions::default()
            };
            let progress = |snapshot: ProgressSnapshot| {
                update_progress_window_details(&weak_progress, snapshot)
            };
            match engine.extract(
                archive,
                &destination,
                &options,
                &progress,
                &OverwriteAllResolver,
                &cancel,
            ) {
                Ok(_) => processed += 1,
                Err(ArchiveError::Cancelled) => break,
                Err(error) => failures.push((archive.clone(), error)),
            }
        }
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if !cancel.is_cancelled() && !failures.is_empty() {
                if processed == 0 {
                    let (path, error) = failures.remove(0);
                    let message = format!("{}: {error}", path.display());
                    platform::show_error("Extract archives", &message);
                } else {
                    let details = failures
                        .iter()
                        .map(|(path, error)| format!("{}: {error}", path.display()))
                        .collect::<Vec<_>>()
                        .join("\n");
                    platform::show_error(
                        "Extract archives",
                        &format!("Some archives failed:\n{details}"),
                    );
                }
            }
            close_progress_window(&ui);
        });
    });
}

/// Creates an archive from the Explorer right-click menu inside the
/// progress-only window; the window closes when the work is done.
fn start_create_window(
    ui: &ProgressWindow,
    state: &Rc<ProgressWindowState>,
    engine: Engine,
    destination: PathBuf,
    sources: Vec<PathBuf>,
    options: CreateOptions,
) {
    let cancel = CancellationToken::new();
    *state
        .cancellation
        .lock()
        .expect("cancellation mutex poisoned") = Some(cancel.clone());
    ui.set_progress_title("Creating archive".into());
    ui.set_progress_file(destination.display().to_string().into());
    ui.set_progress_detail("Starting…".into());
    ui.set_progress_value(-1.0);
    let weak = ui.as_weak();
    let weak_progress = weak.clone();
    let progress =
        move |snapshot: ProgressSnapshot| update_progress_window(&weak_progress, snapshot);
    std::thread::spawn(move || {
        let result = engine.create(&destination, &sources, &options, &progress, &cancel);
        let _ = weak.upgrade_in_event_loop(move |ui| {
            match result {
                Ok(_) => {}
                Err(error) if !cancel.is_cancelled() => {
                    platform::show_error("Create archive", &error.to_string());
                }
                Err(_) => {}
            }
            close_progress_window(&ui);
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

fn apply_progress_to(ui: &AppWindow, snapshot: &ProgressSnapshot) {
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
}

fn update_progress(weak: &slint::Weak<AppWindow>, snapshot: ProgressSnapshot) {
    let weak = weak.clone();
    let _ = weak.upgrade_in_event_loop(move |ui| {
        ui.set_progress_title(snapshot.phase.label().into());
        apply_progress_to(&ui, &snapshot);
    });
}

/// Like [`update_progress`], but keeps the current operation title so that a
/// batch operation can show "Extracting archive 2/3" while entries stream in.
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

/// Human-readable state of the Explorer context-menu registration.
fn context_menu_state_text() -> &'static str {
    if platform::shell_ext::is_context_menu_registered() {
        "Registered"
    } else {
        "Not registered"
    }
}

/// Locates the shell extension DLL that ships next to the app executable.
fn context_menu_dll_path() -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not locate ArchiveRclick: {error}"))?;
    let dll = executable
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("archive_rclick_core.dll");
    if dll.is_file() {
        Ok(dll)
    } else {
        Err(format!(
            "The shell extension DLL was not found next to the app ({}).\nBuild or package ArchiveRclick and try again.",
            dll.display()
        ))
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