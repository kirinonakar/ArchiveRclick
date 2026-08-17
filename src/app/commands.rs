use std::{
    cell::RefCell,
    ffi::{OsStr, OsString},
    fs,
    path::{Component, Path, PathBuf},
    rc::Rc,
    sync::{Arc, Condvar, Mutex, mpsc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use slint::winit_030::WinitWindowAccessor;
use slint::{ComponentHandle, Model, ModelRc};

use crate::{
    AppWindow, ProgressWindow,
    archive::{
        ArchiveEngine, ArchiveError, CompositeEngine, ConflictChoice, ConflictResolver,
        CreateFormat, CreateOptions, ExtractOptions, ExtractSelection, InitialConflictPolicy,
        SevenZipEngine, ThreadCount, VolumeSizePreset, libarchive::LibArchiveEngine,
    },
    platform,
    tasks::{CancellationToken, ProgressSnapshot},
};

use super::AppState;

type Engine = Arc<dyn ArchiveEngine>;

/// Builds the shared archive engine: 7z, LZH, RAR, and ISO archives are handled by
/// the bundled 7z.dll when available, while libarchive remains the fallback
/// for other formats. When 7z.dll cannot be loaded, the composite still serves
/// libarchive formats and 7z-specific operations fail with a clear error.
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

fn create_formats_for_ui(formats: Vec<CreateFormat>) -> Vec<CreateFormat> {
    formats
        .into_iter()
        .filter(|format| matches!(*format, CreateFormat::Zip | CreateFormat::SevenZip))
        .collect()
}

const ARCHIVE_DROP_EXTENSIONS: &[&str] = &[
    "zip", "zipx", "7z", "rar", "tar", "gz", "bz2", "xz", "zst", "cab", "lha", "lzh", "tgz",
    "tbz2", "txz", "iso", "img",
];

fn is_archive_drop_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    let extension_matches = path
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            ARCHIVE_DROP_EXTENSIONS
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        });
    extension_matches || split_archive_volume_base_name(&name).is_some()
}

fn split_archive_volume_base_name(name: &str) -> Option<&str> {
    let (base, suffix) = name.rsplit_once('.')?;
    let base_lower = base.to_ascii_lowercase();
    (base_lower.ends_with(".zip") || base_lower.ends_with(".7z"))
        .then_some(())
        .filter(|_| suffix.len() >= 3 && suffix.bytes().all(|byte| byte.is_ascii_digit()))
        .map(|_| base)
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

// Theme choices offered in Settings. The selection index maps to a stored
// registry value: 0 = follow the system, 1 = light, 2 = dark.
fn theme_selection_index(preference: &str) -> i32 {
    match preference {
        "light" => 1,
        "dark" => 2,
        _ => 0,
    }
}

fn theme_registry_key(index: i32) -> &'static str {
    match index {
        1 => "light",
        2 => "dark",
        _ => "auto",
    }
}

const LANGUAGE_OPTIONS: &[(&str, &str)] = &[
    ("Default", "default"),
    ("English", "en"),
    ("한국어", "ko"),
    ("日本語", "ja"),
];
const PROJECT_GITHUB_URL: &str = "https://github.com/kirinonakar/ArchiveRclick";

const CODEPAGE_OPTIONS: &[(&str, u32)] = &[
    ("Auto", 0),
    ("UTF-8", 65001),
    ("CP949 — Korean", 949),
    ("CP932 — Japanese", 932),
    ("CP936 — Simplified Chinese", 936),
    ("CP950 — Traditional Chinese", 950),
    ("CP1361 — Johab", 1361),
    ("CP50220 — ISO-2022-JP", 50220),
    ("CP54936 — GB18030", 54936),
    ("UTF-16 LE", 1200),
    ("UTF-16 BE", 1201),
];

fn language_selection_index(preference: &str) -> i32 {
    match platform::resolve_language_preference(preference) {
        "ko" => 1,
        "ja" => 2,
        _ => 0,
    }
}

fn language_preference_selection_index(preference: &str) -> i32 {
    LANGUAGE_OPTIONS
        .iter()
        .position(|(_, key)| *key == preference)
        .unwrap_or(0) as i32
}

fn language_registry_key(index: i32) -> &'static str {
    LANGUAGE_OPTIONS
        .get(index.max(0) as usize)
        .map(|(_, key)| *key)
        .unwrap_or("default")
}

fn pathname_codepage(index: i32) -> u32 {
    CODEPAGE_OPTIONS
        .get(index.max(0) as usize)
        .map(|(_, codepage)| *codepage)
        .unwrap_or(0)
}

pub fn run() -> Result<(), String> {
    // If this is the packaged build, remove any stale HKCU registrations left
    // by a previous portable build before Explorer can merge both handlers.
    platform::shell_ext::cleanup_portable_context_menu_entries()?;
    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    let elevated_retry = first.as_deref() == Some(OsStr::new("--elevated-retry"));
    let command = if elevated_retry { args.next() } else { first };
    let subcommand = command
        .as_deref()
        .and_then(|value| value.to_str())
        .map(|value| value.to_owned());
    match subcommand.as_deref() {
        Some("extract") => {
            let rest: Vec<OsString> = args.collect();
            run_gui_extract(&rest, elevated_retry)
        }
        // Internal command used by the Explorer right-drag handler. The
        // first argument is the folder that received the drop; the remaining
        // arguments are the dragged archive paths.
        Some("extract-to") => {
            let rest: Vec<OsString> = args.collect();
            run_gui_extract_to(&rest)
        }
        // Internal commands used by the Explorer right-drag handler. The
        // first argument is the folder that received the drop; the remaining
        // arguments are the dragged source paths.
        Some("zip-to") => {
            let rest: Vec<OsString> = args.collect();
            run_gui_create_to(&rest, CreateFormat::Zip)
        }
        Some("7z-to") => {
            let rest: Vec<OsString> = args.collect();
            run_gui_create_to(&rest, CreateFormat::SevenZip)
        }
        Some("zip-each-to") => {
            let rest: Vec<OsString> = args.collect();
            run_gui_create_each_to(&rest, CreateFormat::Zip)
        }
        Some("7z-each-to") => {
            let rest: Vec<OsString> = args.collect();
            run_gui_create_each_to(&rest, CreateFormat::SevenZip)
        }
        Some("zip") => {
            let rest: Vec<OsString> = args.collect();
            run_gui_create(&rest, CreateFormat::Zip, elevated_retry)
        }
        Some("7z") => {
            let rest: Vec<OsString> = args.collect();
            run_gui_create(&rest, CreateFormat::SevenZip, elevated_retry)
        }
        Some("zip-each") => {
            let rest: Vec<OsString> = args.collect();
            run_gui_create_each(&rest, CreateFormat::Zip, elevated_retry)
        }
        Some("7z-each") => {
            let rest: Vec<OsString> = args.collect();
            run_gui_create_each(&rest, CreateFormat::SevenZip, elevated_retry)
        }
        _ => run_with_startup_argument(command),
    }
}

fn run_with_startup_argument(startup_argument: Option<std::ffi::OsString>) -> Result<(), String> {
    cleanup_drag_staging_directories();
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
                let help = "ArchiveRclick [archive|command]\n\nCommands:\n  extract <archive>...  Extract each archive into its own subfolder\n  zip <path>...         Create a ZIP archive named after the source folder\n  7z <path>...          Create a 7z archive named after the source folder\n  zip-each <folder>...  Create one ZIP archive for each folder\n  7z-each <folder>...   Create one 7z archive for each folder\n\nOptions:\n  --register       Register as an available archive handler\n  --unregister     Remove that registration\n  --check-runtime  Verify the bundled archive engine and exit";
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
                SevenZipEngine::load()
                    .map_err(|error| format!("The bundled 7z.dll could not be loaded: {error}"))?;
                return Ok(());
            }
            _ => {}
        }
    }

    let operation_progress_window = Rc::new(RefCell::new(None::<ProgressWindow>));
    let (ui, state, engine, _writable_formats) =
        open_main_window(Rc::clone(&operation_progress_window))?;

    if let Some(path) = startup_argument.map(PathBuf::from) {
        if path.is_file() && is_archive_drop_path(&path) {
            start_listing(
                &ui,
                Rc::clone(&state),
                Arc::clone(&engine),
                path,
                None,
                pathname_codepage(ui.get_encoding_selection()),
                PathBuf::new(),
            );
        } else if path.is_file() {
            ui.set_status_text(format!("Not a supported archive: {}", path.display()).into());
        } else {
            ui.set_status_text(format!("Not a file: {}", path.display()).into());
        }
    }

    let result = ui
        .run()
        .map_err(|error| format!("UI event loop failed: {error}"));
    cleanup_drag_staging_directories();
    result
}

/// Builds the main application window and wires it up; shared by the
/// interactive startup path and the Explorer context-menu operations.
fn open_main_window(
    operation_progress_window: Rc<RefCell<Option<ProgressWindow>>>,
) -> Result<(AppWindow, Rc<AppState>, Engine, Vec<CreateFormat>), String> {
    let engine: Engine = load_engine()?;
    let ui = AppWindow::new().map_err(|error| format!("Could not create the UI: {error}"))?;
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    let column_boundaries = platform::load_column_boundaries();
    ui.set_column_name_boundary(column_boundaries.name);
    ui.set_column_size_boundary(column_boundaries.size);
    ui.set_column_packed_boundary(column_boundaries.packed);
    ui.set_sort_column(0);
    ui.set_sort_ascending(true);
    let saved_geometry = platform::load_window_geometry();
    if let Some(geometry) = saved_geometry {
        ui.window()
            .set_position(slint::PhysicalPosition::new(geometry.x, geometry.y));
    }
    let state = Rc::new(AppState::new());

    ui.set_archive_rows(ModelRc::from(Rc::clone(&state.rows)));
    ui.set_archive_title("".into());
    ui.set_current_folder("".into());
    ui.set_status_text("Ready".into());
    ui.set_summary_text("No archive open".into());
    ui.set_libarchive_version(engine.version().into());
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
    ui.set_settings_thread_selection(
        ThreadCount::from_registry_key(&platform::load_thread_preference()).ui_index(),
    );
    ui.set_settings_header_encryption(platform::load_header_encryption_preference());
    ui.set_create_header_encryption(platform::load_header_encryption_preference());
    ui.set_theme_selection(theme_selection_index(&platform::load_theme_preference()));
    ui.set_language_options(ModelRc::from(
        LANGUAGE_OPTIONS
            .iter()
            .map(|(label, _)| slint::SharedString::from(*label))
            .collect::<Vec<_>>()
            .as_slice(),
    ));
    let language_preference = platform::load_language_preference();
    ui.set_language_selection(language_selection_index(&language_preference));
    ui.set_language_preference_selection(language_preference_selection_index(
        &language_preference,
    ));
    ui.set_encoding_options(ModelRc::from(
        CODEPAGE_OPTIONS
            .iter()
            .map(|(label, _)| slint::SharedString::from(*label))
            .collect::<Vec<_>>()
            .as_slice(),
    ));
    ui.set_encoding_selection(0);
    ui.set_selection_state(0);
    let writable_formats = create_formats_for_ui(engine.writable_formats());
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
        Rc::clone(&operation_progress_window),
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

    // Save the last normal bounds when the main window is closed.  Avoid
    // replacing them with a maximized/minimized work area, so the next launch
    // restores the user's normal window instead of an unusable off-screen
    // rectangle.
    let weak_for_close = ui.as_weak();
    ui.window().on_close_requested(move || {
        cleanup_drag_staging_directories();
        if let Some(ui) = weak_for_close.upgrade()
            && should_save_window_geometry(ui.window())
        {
            let size = ui.window().size();
            let position = ui.window().position();
            let _ = platform::save_window_geometry(&platform::WindowGeometry {
                x: position.x,
                y: position.y,
                width: size.width,
                height: size.height,
            });
        }
        slint::CloseRequestResponse::HideWindow
    });

    ui.show()
        .map_err(|error| format!("Could not show the UI: {error}"))?;
    if let Some(geometry) = saved_geometry {
        // `show()` may only queue native-window creation until the event loop
        // starts. Apply the physical size from an event-loop callback so the
        // winit backend cannot temporarily reinterpret it as logical pixels.
        let weak = ui.as_weak();
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.window()
                    .set_size(slint::PhysicalSize::new(geometry.width, geometry.height));
            }
        })
        .map_err(|error| format!("Could not schedule saved window size: {error}"))?;
    }
    // `show()` can queue native-window creation until the event loop starts.
    // Reapply there so the Win32 title bar receives the selected theme too.
    let weak_for_theme = ui.as_weak();
    let theme_selection = ui.get_theme_selection();
    slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak_for_theme.upgrade() {
            platform::apply_window_theme(ui.window(), theme_selection);
        }
    })
    .map_err(|error| format!("Could not schedule the window theme: {error}"))?;
    Ok((ui, state, engine, writable_formats))
}

/// Returns false for maximized/fullscreen bounds even when the backend has
/// not updated Slint's state flags yet during the close request.  Saving such
/// bounds would make the next normal launch reopen at the monitor's full size.
fn should_save_window_geometry(window: &slint::Window) -> bool {
    if window.is_maximized() || window.is_minimized() || window.is_fullscreen() {
        return false;
    }

    let mut reject = false;
    let _ = window.with_winit_window(|winit_window| {
        if winit_window.is_maximized() || winit_window.fullscreen().is_some() {
            reject = true;
            return;
        }

        let Some(monitor) = winit_window.current_monitor() else {
            return;
        };
        let monitor_size = monitor.size();
        let outer_size = winit_window.outer_size();
        let width_matches = outer_size.width.abs_diff(monitor_size.width) <= 8;
        let height_matches = outer_size.height.abs_diff(monitor_size.height) <= 8;
        reject = width_matches && height_matches;
    });
    !reject
}

// ---------------------------------------------------------------------------
// Explorer context-menu operations. The shell extension launches the app with
// these verbs and the work runs in a small progress-only window: no main
// window appears, and the window closes by itself once the work is finished.
// ---------------------------------------------------------------------------

fn run_gui_extract(args: &[OsString], elevated_retry: bool) -> Result<(), String> {
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
    );
    ui.run()
        .map_err(|error| format!("UI event loop failed: {error}"))
}

/// Extracts archives into a per-archive folder inside the folder that received
/// a right-drag.
/// This is intentionally a separate internal command so the public `extract`
/// command keeps its existing archive-name-folder behavior.
fn run_gui_extract_to(args: &[OsString]) -> Result<(), String> {
    let Some(destination_argument) = args.first() else {
        return Err("Usage: ArchiveRclick extract-to <directory> <archive>...".to_owned());
    };
    let archive_arguments = &args[1..];
    if archive_arguments.is_empty() {
        return Err("Usage: ArchiveRclick extract-to <directory> <archive>...".to_owned());
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
                let output = unique_path(&destination.join(archive_directory_name(&path)));
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
    );
    ui.run()
        .map_err(|error| format!("UI event loop failed: {error}"))
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
        return Err(format!("Usage: ArchiveRclick {verb} <directory> <file-or-folder>..."));
    };
    let source_arguments = &args[1..];
    if source_arguments.is_empty() {
        return Err(format!("Usage: ArchiveRclick {verb} <directory> <file-or-folder>..."));
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
    ui.run()
        .map_err(|error| format!("UI event loop failed: {error}"))
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
        return Err(format!("Usage: ArchiveRclick {verb} <directory> <folder>..."));
    };
    let folder_arguments = &args[1..];
    if folder_arguments.is_empty() {
        return Err(format!("Usage: ArchiveRclick {verb} <directory> <folder>..."));
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
        let destination = unique_path(
            &destination_folder.join(format!("{name}.{}", format.default_extension())),
        );
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
    ui.run()
        .map_err(|error| format!("UI event loop failed: {error}"))
}

/// Parses the output markers used by an elevated extraction retry. Each
/// `--output <directory> <archive>` pair keeps the retry pointed at the exact
/// directory used by the failed attempt. Normal CLI invocations remain a list
/// of archive paths with no overrides.
fn parse_elevated_extract(
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
    ui.run()
        .map_err(|error| format!("UI event loop failed: {error}"))
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
            unique_path(&cli_archive_destination(std::slice::from_ref(&source), format))
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
    ui.run()
        .map_err(|error| format!("UI event loop failed: {error}"))
}

/// Parses the destination marker used only by an elevated retry.  Normal CLI
/// invocations keep the original `zip <source>...` shape, while a retry gets
/// the exact destination selected before the first attempt.  Keeping that
/// destination avoids silently retrying into `_2` after a failed attempt has
/// already created part of the output folder/archive.
fn parse_elevated_output(
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
fn parse_elevated_batch_output(
    args: &[OsString],
    elevated_retry: bool,
) -> Result<Vec<(Option<PathBuf>, OsString)>, String> {
    if !elevated_retry {
        return Ok(args
            .iter()
            .cloned()
            .map(|source| (None, source))
            .collect());
    }

    let mut items = Vec::with_capacity(args.len() / 3);
    let mut index = 0;
    while index < args.len() {
        if args[index].as_os_str() != OsStr::new("--output") {
            return Err(
                "The elevated per-folder compression retry is missing an output marker"
                    .to_owned(),
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

const DRAG_STAGING_PREFIX: &str = "ArchiveRclick-drag-";

fn start_archive_drag(ui: &AppWindow, state: &Rc<AppState>, engine: &Engine, row: usize) {
    if ui.get_busy()
        || ui.get_password_visible()
        || ui.get_conflict_visible()
        || ui.get_create_visible()
        || ui.get_extract_visible()
        || ui.get_settings_visible()
        || !ui.get_has_archive()
    {
        return;
    }

    // Starting a drag from an unselected row follows Explorer's usual
    // behavior: that row becomes the sole selection before the drag begins.
    if !state
        .display
        .borrow()
        .get(row)
        .is_some_and(|entry| state.selected.borrow().contains(&entry.relative_path))
    {
        if !state.rows.select_for_context(row) {
            return;
        }
        update_selection_ui(&ui.as_weak(), state);
    }

    let Some(archive) = state
        .listing
        .borrow()
        .as_ref()
        .map(|listing| listing.archive_path.clone())
    else {
        return;
    };
    let mut selection = state.selected_paths();
    if selection.is_empty() {
        return;
    }
    selection.sort();

    let staging = match create_drag_staging_directory() {
        Ok(path) => path,
        Err(error) => {
            show_ui_error(&ui.as_weak(), "Extract selected items", error);
            return;
        }
    };

    ui.set_busy(true);
    ui.set_status_text("Preparing selected items for Explorer…".into());

    let password = state
        .open_password
        .lock()
        .expect("password mutex poisoned")
        .clone();
    let options = ExtractOptions {
        selection: ExtractSelection::Paths(selection.clone()),
        password,
        conflict_policy: InitialConflictPolicy::OverwriteAll,
        pathname_codepage: pathname_codepage(ui.get_encoding_selection()),
        ..ExtractOptions::default()
    };
    let cancel = CancellationToken::new();
    let progress = |_: ProgressSnapshot| {};
    let extraction = engine.extract(
        &archive,
        &staging,
        &options,
        &progress,
        &OverwriteAllResolver,
        &cancel,
    );

    let result = match extraction {
        Ok(_) => staged_drag_paths(&staging, &selection)
            .and_then(|paths| platform::start_file_drag(&paths).map(|()| paths)),
        Err(error) => Err(error.to_string()),
    };
    let _ = fs::remove_dir_all(&staging);
    ui.set_busy(false);

    match result {
        Ok(_) => ui.set_status_text("Selected items were sent to Explorer".into()),
        Err(error) => show_ui_error(&ui.as_weak(), "Drag selected items", error),
    }
}

fn create_drag_staging_directory() -> Result<PathBuf, String> {
    let temp = std::env::temp_dir();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("Could not create a drag staging name: {error}"))?
        .as_nanos();
    let process = std::process::id();
    for attempt in 0..100u32 {
        let path = temp.join(format!(
            "{DRAG_STAGING_PREFIX}{process}-{timestamp}-{attempt}"
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "Could not create the drag staging directory {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Err("Could not create a unique drag staging directory".to_owned())
}

fn staged_drag_paths(staging: &Path, selection: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    selection
        .iter()
        .map(|relative| {
            if !relative.is_relative()
                || relative.components().any(|component| {
                    matches!(
                        component,
                        Component::Prefix(_) | Component::RootDir | Component::ParentDir
                    )
                })
            {
                return Err(format!(
                    "The selected archive path is not a safe relative path: {}",
                    relative.display()
                ));
            }
            let staged = staging.join(relative);
            if !staged.exists() {
                return Err(format!(
                    "The selected item was not extracted: {}",
                    relative.display()
                ));
            }
            Ok(staged)
        })
        .collect()
}

/// Removes this process's temporary Explorer-drag folders. The explicit
/// process prefix prevents one running instance from deleting another
/// instance's active drag payload.
fn cleanup_drag_staging_directories() {
    let process_prefix = format!("{DRAG_STAGING_PREFIX}{}-", std::process::id());
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_owned = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(&process_prefix));
        if is_owned && path.is_dir() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn handle_file_drop(
    ui: &AppWindow,
    state: &Rc<AppState>,
    engine: &Engine,
    paths: Vec<PathBuf>,
) {
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
    if !is_archive_drop_path(&path) {
        set_create_sources(ui, state, vec![path]);
        return;
    }
    start_listing(
        ui,
        Rc::clone(state),
        Arc::clone(engine),
        path,
        None,
        pathname_codepage(ui.get_encoding_selection()),
        PathBuf::new(),
    );
}

fn wire_callbacks(
    ui: &AppWindow,
    state: Rc<AppState>,
    engine: Engine,
    writable_formats: Vec<CreateFormat>,
    operation_progress_window: Rc<RefCell<Option<ProgressWindow>>>,
) {
    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        let engine = Arc::clone(&engine);
        ui.on_open_requested(move || match platform::pick_archive() {
            Ok(Some(path)) => {
                if let Some(ui) = weak.upgrade() {
                    start_listing(
                        &ui,
                        Rc::clone(&state),
                        Arc::clone(&engine),
                        path,
                        None,
                        pathname_codepage(ui.get_encoding_selection()),
                        PathBuf::new(),
                    );
                }
            }
            Ok(None) => {}
            Err(error) => show_ui_error(&weak, "Open archive", error),
        });
    }

    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_close_archive_requested(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_busy() {
                return;
            }
            close_archive(&ui, &state);
        });
    }

    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        let engine = Arc::clone(&engine);
        ui.on_up_requested(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_busy() {
                return;
            }
            let current = state.current_folder.borrow().clone();
            if current.as_os_str().is_empty() {
                return;
            }
            let mut parent = current;
            parent.pop();
            let Some(path) = state
                .listing
                .borrow()
                .as_ref()
                .map(|listing| listing.archive_path.clone())
            else {
                return;
            };
            let password = state
                .open_password
                .lock()
                .expect("password mutex poisoned")
                .clone();
            start_listing(
                &ui,
                Rc::clone(&state),
                Arc::clone(&engine),
                path,
                password,
                pathname_codepage(ui.get_encoding_selection()),
                parent,
            );
        });
    }

    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_toggle_selection(move |row, shift| {
            if row >= 0 {
                state.rows.select(row as usize, shift);
                update_selection_ui(&weak, &state);
            }
        });
    }

    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        let engine = Arc::clone(&engine);
        ui.on_drag_row_requested(move |row| {
            if row < 0 {
                return;
            }
            let Some(ui) = weak.upgrade() else {
                return;
            };
            start_archive_drag(&ui, &state, &engine, row as usize);
        });
    }

    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_select_all_requested(move |select_all| {
            if select_all {
                state.rows.select_all_visible();
            } else {
                state.rows.clear_selection();
            }
            update_selection_ui(&weak, &state);
        });
    }

    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        let engine = Arc::clone(&engine);
        ui.on_activate_row(move |row| {
            if row < 0 {
                return;
            }
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_busy() {
                return;
            }
            let Some(folder) = state.activate_row(row as usize) else {
                return;
            };
            let Some(path) = state
                .listing
                .borrow()
                .as_ref()
                .map(|listing| listing.archive_path.clone())
            else {
                return;
            };
            let password = state
                .open_password
                .lock()
                .expect("password mutex poisoned")
                .clone();
            start_listing(
                &ui,
                Rc::clone(&state),
                Arc::clone(&engine),
                path,
                password,
                pathname_codepage(ui.get_encoding_selection()),
                folder,
            );
        });
    }

    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_extract_context_requested(move |row| {
            if row < 0 {
                return;
            }
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_busy() || !ui.get_has_archive() {
                return;
            }
            if !state.rows.select_for_context(row as usize) {
                return;
            }
            update_selection_ui(&weak, &state);
            ui.set_extract_selected_only(true);
            ui.set_extract_visible(true);
        });
    }

    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_sort_requested(move |column| {
            let column = column.max(0) as usize;
            if state.sort_column.get() == column {
                state.sort_ascending.set(!state.sort_ascending.get());
            } else {
                state.sort_column.set(column);
                state.sort_ascending.set(true);
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_sort_column(column as i32);
                ui.set_sort_ascending(state.sort_ascending.get());
            }
            state.rebuild_display();
        });
    }

    {
        let weak = ui.as_weak();
        ui.on_column_widths_changed(move |name, size, packed| {
            let boundaries = platform::ColumnBoundaries { name, size, packed };
            if let Err(error) = platform::save_column_boundaries(&boundaries) {
                if let Some(ui) = weak.upgrade() {
                    ui.set_status_text(format!("Could not save column widths: {error}").into());
                }
            }
        });
    }

    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        let engine = Arc::clone(&engine);
        ui.on_encoding_selection_changed(move |selection| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            ui.set_encoding_selection(selection.max(0));
            if ui.get_busy() || !ui.get_has_archive() {
                return;
            }
            let Some(path) = state
                .listing
                .borrow()
                .as_ref()
                .map(|listing| listing.archive_path.clone())
            else {
                return;
            };
            let password = state
                .open_password
                .lock()
                .expect("password mutex poisoned")
                .clone();
            start_listing(
                &ui,
                Rc::clone(&state),
                Arc::clone(&engine),
                path,
                password,
                pathname_codepage(selection),
                state.current_folder.borrow().clone(),
            );
        });
    }

    ui.on_show_extract_requested(|| {});

    {
        let weak = ui.as_weak();
        ui.on_settings_requested(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_context_menu_state(context_menu_state_text().into());
                ui.set_context_menu_managed_by_package(
                    platform::shell_ext::is_context_menu_managed_by_package(),
                );
                ui.set_settings_thread_selection(
                    ThreadCount::from_registry_key(&platform::load_thread_preference()).ui_index(),
                );
                ui.set_settings_header_encryption(platform::load_header_encryption_preference());
                ui.set_theme_selection(theme_selection_index(&platform::load_theme_preference()));
                let language_preference = platform::load_language_preference();
                ui.set_language_selection(language_selection_index(&language_preference));
                ui.set_language_preference_selection(language_preference_selection_index(
                    &language_preference,
                ));
                ui.set_settings_visible(true);
            }
        });
    }

    {
        let weak = ui.as_weak();
        ui.on_github_requested(move || {
            let result = platform::open_url(PROJECT_GITHUB_URL);
            if let Some(ui) = weak.upgrade() {
                match result {
                    Ok(()) => ui.set_status_text("GitHub project opened".into()),
                    Err(error) => {
                        ui.set_status_text(error.clone().into());
                        platform::show_error("Open GitHub project", &error);
                    }
                }
            }
        });
    }

    {
        let weak = ui.as_weak();
        ui.on_third_party_licenses_requested(move || {
            let result = third_party_notices_path().and_then(|path| platform::open_file(&path));
            if let Some(ui) = weak.upgrade() {
                match result {
                    Ok(()) => ui.set_status_text("Third-party license notices opened".into()),
                    Err(error) => {
                        ui.set_status_text(error.clone().into());
                        platform::show_error("Open third-party license notices", &error);
                    }
                }
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
        ui.on_settings_applied(
            move |font_selection,
                  thread_selection,
                  theme_selection,
                  language_preference_selection,
                  header_encryption| {
                let preference = FONT_OPTIONS
                    .get(font_selection.max(0) as usize)
                    .map(|(_, key)| *key)
                    .unwrap_or("auto");
                let mut failure: Option<String> = None;
                if let Err(error) = platform::save_font_preference(preference) {
                    failure = Some(format!("Could not save settings: {error}"));
                } else if let Err(error) = platform::save_thread_preference(
                    ThreadCount::from_ui_index(thread_selection).registry_key(),
                ) {
                    failure = Some(format!("Could not save settings: {error}"));
                } else if let Err(error) =
                    platform::save_header_encryption_preference(header_encryption)
                {
                    failure = Some(format!("Could not save settings: {error}"));
                } else if let Err(error) =
                    platform::save_theme_preference(theme_registry_key(theme_selection))
                {
                    failure = Some(format!("Could not save settings: {error}"));
                } else if let Err(error) = platform::save_language_preference(
                    language_registry_key(language_preference_selection),
                )
                {
                    failure = Some(format!("Could not save settings: {error}"));
                }
                if let Some(message) = failure {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_status_text(message.into());
                    }
                    return;
                }
                let family = platform::resolve_font_family(preference);
                if let Some(ui) = weak.upgrade() {
                    ui.set_font_family(family.into());
                    ui.set_settings_header_encryption(header_encryption);
                    ui.set_create_header_encryption(header_encryption);
                    ui.set_theme_selection(theme_selection);
                    let language_preference = language_registry_key(language_preference_selection);
                    ui.set_language_selection(language_selection_index(language_preference));
                    ui.set_language_preference_selection(language_preference_selection);
                    platform::apply_window_theme(ui.window(), theme_selection);
                }
            },
        );
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
        let progress_window = Rc::clone(&operation_progress_window);
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
                let pathname_codepage = weak
                    .upgrade()
                    .map(|ui| pathname_codepage(ui.get_encoding_selection()))
                    .unwrap_or(0);

                let options = ExtractOptions {
                    selection,
                    password: (!password.is_empty()).then(|| password.to_string()),
                    conflict_policy: match conflict_policy {
                        1 => InitialConflictPolicy::OverwriteAll,
                        2 => InitialConflictPolicy::SkipAll,
                        _ => InitialConflictPolicy::Ask,
                    },
                    pathname_codepage,
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
                        Rc::clone(&progress_window),
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
                ui.set_create_volume_selection(VolumeSizePreset::None.ui_index());
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
        let progress_window = Rc::clone(&operation_progress_window);
        ui.on_create_requested(
            move |format_index,
                  level,
                  thread_index,
                  volume_index,
                  password,
                  password_confirmation,
                  encrypt_headers| {
                let sources = state.pending_create_sources.borrow().clone();
                if sources.is_empty() {
                    show_ui_error(
                        &weak,
                        "Create archive",
                        "No input files were selected".to_owned(),
                    );
                    return;
                }
                if password != password_confirmation {
                    show_ui_error(&weak, "Create archive", "Passwords do not match".to_owned());
                    return;
                }
                let Some(format) = writable_formats.get(format_index.max(0) as usize).copied()
                else {
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
                let split_size = matches!(format, CreateFormat::Zip | CreateFormat::SevenZip)
                    .then(|| VolumeSizePreset::from_ui_index(volume_index).bytes())
                    .flatten();
                let options = CreateOptions {
                    format,
                    compression_level: level.clamp(0, 9) as u8,
                    split_size,
                    password: (!password.is_empty()).then(|| password.to_string()),
                    encrypt_headers,
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
                        Rc::clone(&progress_window),
                    );
                }
            },
        );
    }

    {
        let state = Rc::clone(&state);
        let engine = Arc::clone(&engine);
        let weak = ui.as_weak();
        let progress_window = Rc::clone(&operation_progress_window);
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
                    Rc::clone(&progress_window),
                );
            }
        });
    }

    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        let engine = Arc::clone(&engine);
        let progress_window = Rc::clone(&operation_progress_window);
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
                    Rc::clone(&progress_window),
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
            let same_archive = state
                .listing
                .borrow()
                .as_ref()
                .is_some_and(|listing| listing.archive_path == path);
            let directory = if same_archive {
                state.current_folder.borrow().clone()
            } else {
                PathBuf::new()
            };
            start_listing(
                &ui,
                Rc::clone(&state),
                Arc::clone(&engine),
                path,
                Some(password),
                pathname_codepage(ui.get_encoding_selection()),
                directory,
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
            if !is_archive_drop_path(&path) {
                if let Some(ui) = weak.upgrade() {
                    set_create_sources(&ui, &state, vec![path]);
                }
                return;
            }
            if let Some(ui) = weak.upgrade() {
                start_listing(
                    &ui,
                    Rc::clone(&state),
                    Arc::clone(&engine),
                    path,
                    None,
                    pathname_codepage(ui.get_encoding_selection()),
                    PathBuf::new(),
                );
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
    pathname_codepage: u32,
    directory: PathBuf,
) {
    let progress_title = progress_title_with_filename("Opening archive", &path);
    let cancel = CancellationToken::new();
    *state
        .cancellation
        .lock()
        .expect("cancellation mutex poisoned") = Some(cancel.clone());
    ui.set_busy(true);
    ui.set_status_text(progress_title.into());
    let open_password = Arc::clone(&state.open_password);
    let pending_password_path = Arc::clone(&state.pending_password_path);
    let sort_column = state.sort_column.get();
    let sort_ascending = state.sort_ascending.get();
    let directory_for_worker = directory.clone();
    let weak = ui.as_weak();
    let progress = |_: ProgressSnapshot| {};
    std::thread::spawn(move || {
        let result = engine
            .list_directory(
                &path,
                &directory_for_worker,
                password.as_deref(),
                pathname_codepage,
                &progress,
                &cancel,
            )
            .map(|listing| {
                super::ArchiveRowModel::prepare_listing_at(
                    listing,
                    &directory_for_worker,
                    sort_column,
                    sort_ascending,
                )
            });
        let _ = weak.upgrade_in_event_loop(move |ui| {
            finish_operation(&ui);
            match result {
                Ok((listing, display)) => {
                    let entry_count = listing.entries.len();
                    let total = listing.total_uncompressed_size;
                    let format_name = listing.format_name.clone();
                    let warning = listing.warning.clone();
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
                        rows.set_prepared_listing_at(listing, display, directory.clone());
                    } else {
                        display_operation_error(
                            &ui,
                            "Open archive",
                            ArchiveError::Worker("archive row model is unavailable".to_owned()),
                        );
                        return;
                    }
                    ui.set_selection_state(0);
                    ui.set_selection_count(0);
                    ui.set_archive_title(archive_name.into());
                    ui.set_current_folder(directory.to_string_lossy().replace('\\', "/").into());
                    ui.set_has_archive(true);
                    ui.set_can_go_up(!directory.as_os_str().is_empty());
                    let status = warning.map_or_else(
                        || format!("{format_name} archive"),
                        |warning| format!("{format_name} archive — {warning}"),
                    );
                    ui.set_status_text(status.into());
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

fn progress_title_with_filename(operation: &str, path: &Path) -> String {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.display().to_string());
    format!("{operation} - {file_name}")
}

fn start_extract(
    ui: &AppWindow,
    state: Rc<AppState>,
    engine: Engine,
    archive: PathBuf,
    destination: PathBuf,
    options: ExtractOptions,
    progress_window: Rc<RefCell<Option<ProgressWindow>>>,
) {
    let mut options = options;
    let (hint_entries, hint_bytes) =
        extraction_progress_hints(state.listing.borrow().as_ref(), &options.selection);
    options.total_entries_hint = hint_entries;
    options.total_bytes_hint = hint_bytes;
    let progress_title = progress_title_with_filename("Extracting archive", &archive);
    let archive_display = archive.display().to_string();
    let (cancel, weak_progress) = match begin_progress_window_operation(
        ui,
        &state,
        &progress_window,
        &progress_title,
        &archive_display,
    ) {
        Ok(operation) => operation,
        Err(error) => {
            ui.set_status_text(error.clone().into());
            platform::show_error("Extract archive", &error);
            return;
        }
    };
    let resolver = UiConflictResolver {
        weak: ui.as_weak(),
        progress: weak_progress.clone(),
        pending: Arc::clone(&state.pending_conflict),
        cancel: cancel.clone(),
    };
    let weak = ui.as_weak();
    let weak_progress_updates = weak_progress.clone();
    let weak_progress_finished = weak_progress.clone();
    let cancellation = Arc::clone(&state.cancellation);
    let pending_conflict = Arc::clone(&state.pending_conflict);
    let progress = move |snapshot: ProgressSnapshot| {
        update_progress_window_details(&weak_progress_updates, snapshot)
    };
    std::thread::spawn(move || {
        let result = engine.extract(
            &archive,
            &destination,
            &options,
            &progress,
            &resolver,
            &cancel,
        );
        let cancelled = cancel.is_cancelled() || matches!(&result, Err(ArchiveError::Cancelled));
        let destination_for_ui = destination.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if let Some(progress_ui) = weak_progress_finished.upgrade() {
                let _ = progress_ui.hide();
            }
            *cancellation.lock().expect("cancellation mutex poisoned") = None;
            pending_conflict
                .lock()
                .expect("conflict mutex poisoned")
                .take();
            finish_operation(&ui);
            if cancelled {
                ui.set_status_text("Extraction cancelled".into());
                return;
            }
            match result {
                Ok(summary) => {
                    let status = match summary.warning {
                        Some(warning) => format!(
                            "Extracted {} entries to {} — {}",
                            summary.entries_processed,
                            destination_for_ui.display(),
                            warning
                        ),
                        None => format!(
                            "Extracted {} entries to {}",
                            summary.entries_processed,
                            destination_for_ui.display()
                        ),
                    };
                    ui.set_status_text(status.into());
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
    progress_window: Rc<RefCell<Option<ProgressWindow>>>,
) {
    let progress_title = progress_title_with_filename("Creating archive", &destination);
    let destination_display = destination.display().to_string();
    let (cancel, weak_progress) = match begin_progress_window_operation(
        ui,
        &state,
        &progress_window,
        &progress_title,
        &destination_display,
    ) {
        Ok(operation) => operation,
        Err(error) => {
            ui.set_status_text(error.clone().into());
            platform::show_error("Create archive", &error);
            return;
        }
    };
    let weak = ui.as_weak();
    let weak_progress_updates = weak_progress.clone();
    let weak_progress_finished = weak_progress.clone();
    let cancellation = Arc::clone(&state.cancellation);
    let progress = move |snapshot: ProgressSnapshot| {
        update_progress_window_details(&weak_progress_updates, snapshot)
    };
    std::thread::spawn(move || {
        let result = engine.create(&destination, &sources, &options, &progress, &cancel);
        let cancelled = cancel.is_cancelled() || matches!(&result, Err(ArchiveError::Cancelled));
        let destination_for_ui = destination.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if let Some(progress_ui) = weak_progress_finished.upgrade() {
                let _ = progress_ui.hide();
            }
            *cancellation.lock().expect("cancellation mutex poisoned") = None;
            finish_operation(&ui);
            if cancelled {
                ui.set_status_text("Archive creation cancelled".into());
                return;
            }
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
    password_prompt: Arc<ProgressPasswordPrompt>,
}

/// Coordinates a password dialog shown by the progress-only Explorer window
/// with the worker thread that is waiting to retry the current archive.
struct ProgressPasswordPrompt {
    response: Mutex<Option<Option<String>>>,
    wake: Condvar,
}

impl ProgressPasswordPrompt {
    fn new() -> Self {
        Self {
            response: Mutex::new(None),
            wake: Condvar::new(),
        }
    }

    fn respond(&self, password: Option<String>) {
        let mut response = self
            .response
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *response = Some(password);
        self.wake.notify_all();
    }

    fn wait(
        &self,
        weak: &slint::Weak<ProgressWindow>,
        archive: &Path,
        cancel: &CancellationToken,
    ) -> Option<String> {
        {
            let mut response = self
                .response
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            *response = None;
        }
        let operation = format!("Enter the password for {}", archive.display());
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_password_operation(operation.into());
            ui.set_password_value("".into());
            ui.set_password_visible(true);
        });

        let mut response = self
            .response
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while !cancel.is_cancelled() {
            if let Some(password) = response.take() {
                return password;
            }
            response = self
                .wake
                .wait_timeout(response, Duration::from_millis(50))
                .unwrap_or_else(|poison| poison.into_inner())
                .0;
        }
        None
    }
}

const PROGRESS_WINDOW_LOGICAL_WIDTH: f32 = 640.0;
const PROGRESS_WINDOW_LOGICAL_HEIGHT: f32 = 300.0;

/// Builds and shows the small progress window; its Cancel button cancels the
/// running operation.
fn open_progress_window() -> Result<(ProgressWindow, Rc<ProgressWindowState>), String> {
    let ui = ProgressWindow::new().map_err(|error| format!("Could not create the UI: {error}"))?;
    let theme_selection = theme_selection_index(&platform::load_theme_preference());
    ui.set_theme_selection(theme_selection);
    let state = Rc::new(ProgressWindowState {
        cancellation: Arc::new(Mutex::new(None)),
        password_prompt: Arc::new(ProgressPasswordPrompt::new()),
    });
    let state_for_cancel = Rc::clone(&state);
    let password_for_cancel = Arc::clone(&state.password_prompt);
    let weak_for_cancel = ui.as_weak();
    ui.on_cancel_requested(move || {
        if let Some(token) = state_for_cancel
            .cancellation
            .lock()
            .expect("cancellation mutex poisoned")
            .as_ref()
        {
            token.cancel();
        }
        password_for_cancel.respond(None);
        if let Some(ui) = weak_for_cancel.upgrade() {
            ui.set_progress_title("Cancelling…".into());
        }
    });
    // The title-bar X must behave exactly like the Cancel button: cancel the
    // running operation (and any pending password prompt) so the worker aborts
    // and cleans up its temporary files instead of leaving them behind. Keep
    // the window alive until the worker calls `close_progress_window`; hiding
    // the only window here would end the event loop and terminate the process
    // before the worker gets a chance to remove its temporary files.
    let state_for_close = Rc::clone(&state);
    let password_for_close = Arc::clone(&state.password_prompt);
    let weak_for_close = ui.as_weak();
    ui.window().on_close_requested(move || {
        if let Some(token) = state_for_close
            .cancellation
            .lock()
            .expect("cancellation mutex poisoned")
            .as_ref()
        {
            token.cancel();
        }
        password_for_close.respond(None);
        if let Some(ui) = weak_for_close.upgrade() {
            ui.set_progress_title("Cancelling…".into());
        }
        slint::CloseRequestResponse::KeepWindowShown
    });
    let password_for_response = Arc::clone(&state.password_prompt);
    let weak_for_response = ui.as_weak();
    ui.on_password_response(move |password, accepted| {
        if let Some(ui) = weak_for_response.upgrade() {
            ui.set_password_visible(false);
        }
        let password = (accepted && !password.is_empty()).then(|| password.to_string());
        password_for_response.respond(password);
    });
    let font_family = platform::resolve_font_family(&platform::load_font_preference());
    ui.set_font_family(font_family.into());
    ui.set_progress_title("Working…".into());
    ui.set_progress_file("".into());
    set_initial_progress_window(&ui);
    // The native window is created when the event loop starts, so its size is
    // still zero here. Supply the preferred logical size so the initial
    // position can be calculated before the first frame is shown.
    platform::center_window_with_logical_size(
        ui.window(),
        slint::LogicalSize::new(
            PROGRESS_WINDOW_LOGICAL_WIDTH,
            PROGRESS_WINDOW_LOGICAL_HEIGHT,
        ),
    );
    ui.show()
        .map_err(|error| format!("Could not show the progress window: {error}"))?;
    let weak_for_theme = ui.as_weak();
    slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak_for_theme.upgrade() {
            platform::apply_window_theme(ui.window(), theme_selection);
        }
    })
    .map_err(|error| format!("Could not schedule the progress-window theme: {error}"))?;
    Ok((ui, state))
}

/// Hides the progress window and ends the event loop so the process exits.
fn close_progress_window(ui: &ProgressWindow) {
    let _ = ui.hide();
    let _ = slint::quit_event_loop();
}

fn apply_progress_window(ui: &ProgressWindow, snapshot: &ProgressSnapshot) {
    let text = progress_ui_text(snapshot);
    ui.set_progress_file(snapshot.current_file.clone().into());
    ui.set_progress_file_percent(text.file_percent.into());
    ui.set_progress_percent(text.percent.into());
    ui.set_progress_elapsed(text.elapsed.into());
    ui.set_progress_remaining(text.remaining.into());
    ui.set_progress_total(text.total.into());
    ui.set_progress_detail(text.detail.into());
    ui.set_progress_file_value(text.file_value);
    ui.set_progress_value(text.value);
}

/// Keeps the operation title while progress values stream in.
fn update_progress_window_details(weak: &slint::Weak<ProgressWindow>, snapshot: ProgressSnapshot) {
    let weak = weak.clone();
    let _ = weak.upgrade_in_event_loop(move |ui| apply_progress_window(&ui, &snapshot));
}

/// Extracts each archive into its own `<archive-name>` subfolder inside the
/// progress-only window; the window closes when the batch finishes.
fn request_context_elevation(
    retry_already_attempted: bool,
    subcommand: &str,
    paths: &[PathBuf],
    destination: Option<&Path>,
) -> bool {
    if retry_already_attempted || paths.is_empty() {
        return false;
    }
    let mut args = Vec::with_capacity(paths.len() + 4);
    args.push(OsString::from("--elevated-retry"));
    args.push(OsString::from(subcommand));
    if let Some(destination) = destination {
        args.push(OsString::from("--output"));
        args.push(destination.as_os_str().to_owned());
    }
    args.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
    match platform::run_elevated(&args) {
        Ok(launched) => launched,
        Err(error) => {
            platform::show_error("Request administrator access", &error);
            false
        }
    }
}

fn request_context_create_batch_elevation(
    retry_already_attempted: bool,
    subcommand: &str,
    failures: &[(PathBuf, PathBuf, ArchiveError)],
) -> bool {
    if retry_already_attempted {
        return false;
    }
    let failures = failures
        .iter()
        .filter(|(_, _, error)| error.requires_elevation())
        .collect::<Vec<_>>();
    if failures.is_empty() {
        return false;
    }

    let mut args = Vec::with_capacity(failures.len() * 3 + 2);
    args.push(OsString::from("--elevated-retry"));
    args.push(OsString::from(subcommand));
    for (source, destination, _) in failures {
        args.push(OsString::from("--output"));
        args.push(destination.as_os_str().to_owned());
        args.push(source.as_os_str().to_owned());
    }
    match platform::run_elevated(&args) {
        Ok(launched) => launched,
        Err(error) => {
            platform::show_error("Request administrator access", &error);
            false
        }
    }
}

fn request_context_extract_elevation(
    retry_already_attempted: bool,
    failures: &[(PathBuf, PathBuf, ArchiveError)],
) -> bool {
    if retry_already_attempted {
        return false;
    }
    let failures = failures
        .iter()
        .filter(|(_, _, error)| error.requires_elevation())
        .collect::<Vec<_>>();
    if failures.is_empty() {
        return false;
    }

    let mut args = Vec::with_capacity(failures.len() * 3 + 2);
    args.push(OsString::from("--elevated-retry"));
    args.push(OsString::from("extract"));
    for (archive, destination, _) in failures {
        args.push(OsString::from("--output"));
        args.push(destination.as_os_str().to_owned());
        args.push(archive.as_os_str().to_owned());
    }
    match platform::run_elevated(&args) {
        Ok(launched) => launched,
        Err(error) => {
            platform::show_error("Request administrator access", &error);
            false
        }
    }
}

fn start_extract_batch_window(
    ui: &ProgressWindow,
    state: &Rc<ProgressWindowState>,
    engine: Engine,
    archives: Vec<PathBuf>,
    destination_overrides: Vec<Option<PathBuf>>,
    elevated_retry: bool,
) {
    let total = archives.len();
    let first = archives[0].display().to_string();
    let initial_title = progress_title_with_filename(
        &format!("Extracting archive 1/{total}"),
        &archives[0],
    );
    let cancel = CancellationToken::new();
    *state
        .cancellation
        .lock()
        .expect("cancellation mutex poisoned") = Some(cancel.clone());
    ui.set_progress_title(initial_title.into());
    ui.set_progress_file(first.into());
    set_initial_progress_window(ui);
    let weak = ui.as_weak();
    let weak_progress = weak.clone();
    let weak_password = weak.clone();
    let password_prompt = Arc::clone(&state.password_prompt);
    std::thread::spawn(move || {
        let mut processed = 0usize;
        let mut failures: Vec<(PathBuf, PathBuf, ArchiveError)> = Vec::new();
        for (index, archive) in archives.iter().enumerate() {
            if cancel.is_cancelled() {
                break;
            }
            let title = progress_title_with_filename(
                &format!("Extracting archive {}/{}", index + 1, total),
                archive,
            );
            let archive_display = archive.display().to_string();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_progress_title(title.into());
                ui.set_progress_file(archive_display.into());
                set_initial_progress_window(&ui);
            });
            let parent = archive.parent().unwrap_or_else(|| Path::new("."));
            let destination = destination_overrides
                .get(index)
                .and_then(|path| path.clone())
                .unwrap_or_else(|| unique_path(&parent.join(archive_directory_name(archive))));
            let progress = |snapshot: ProgressSnapshot| {
                update_progress_window_details(&weak_progress, snapshot)
            };
            let mut password = None;
            loop {
                let options = ExtractOptions {
                    selection: ExtractSelection::All,
                    password: password.clone(),
                    conflict_policy: InitialConflictPolicy::OverwriteAll,
                    ..ExtractOptions::default()
                };
                match engine.extract(
                    archive,
                    &destination,
                    &options,
                    &progress,
                    &OverwriteAllResolver,
                    &cancel,
                ) {
                    Ok(_) => {
                        processed += 1;
                        break;
                    }
                    Err(ArchiveError::PasswordRequired) => {
                        let Some(next_password) =
                            password_prompt.wait(&weak_password, archive, &cancel)
                        else {
                            cancel.cancel();
                            break;
                        };
                        password = Some(next_password);
                    }
                    Err(ArchiveError::Cancelled) => break,
                    Err(error) => {
                        failures.push((archive.clone(), destination.clone(), error));
                        break;
                    }
                }
            }
            if cancel.is_cancelled() {
                break;
            }
        }
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if !cancel.is_cancelled()
                && request_context_extract_elevation(elevated_retry, &failures)
            {
                close_progress_window(&ui);
                return;
            }
            if !cancel.is_cancelled() && !failures.is_empty() {
                if processed == 0 {
                    let (path, _, error) = failures.remove(0);
                    let message = format!("{}: {error}", path.display());
                    platform::show_error("Extract archives", &message);
                } else {
                    let details = failures
                        .iter()
                        .map(|(path, _, error)| format!("{}: {error}", path.display()))
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
    elevated_retry: bool,
) {
    let cancel = CancellationToken::new();
    *state
        .cancellation
        .lock()
        .expect("cancellation mutex poisoned") = Some(cancel.clone());
    let progress_title = progress_title_with_filename("Creating archive", &destination);
    ui.set_progress_title(progress_title.into());
    ui.set_progress_file(destination.display().to_string().into());
    set_initial_progress_window(ui);
    let weak = ui.as_weak();
    let weak_progress = weak.clone();
    let elevation_sources = sources.clone();
    let progress = move |snapshot: ProgressSnapshot| {
        update_progress_window_details(&weak_progress, snapshot)
    };
    std::thread::spawn(move || {
        let result = engine.create(&destination, &sources, &options, &progress, &cancel);
        let _ = weak.upgrade_in_event_loop(move |ui| {
            match result {
                Ok(_) => {}
                Err(error) if !cancel.is_cancelled() => {
                    let subcommand = match options.format {
                        CreateFormat::Zip => "zip",
                        CreateFormat::SevenZip => "7z",
                        _ => "",
                    };
                    let relaunched = error.requires_elevation()
                        && request_context_elevation(
                            elevated_retry,
                            subcommand,
                            &elevation_sources,
                            Some(&destination),
                        );
                    if !relaunched {
                        platform::show_error("Create archive", &error.to_string());
                    }
                }
                Err(_) => {}
            }
            close_progress_window(&ui);
        });
    });
}

/// Creates one archive per selected folder inside the progress-only window.
/// The items are processed sequentially so each folder gets its own output
/// and the progress window remains a single, predictable operation.
fn start_create_batch_window(
    ui: &ProgressWindow,
    state: &Rc<ProgressWindowState>,
    engine: Engine,
    items: Vec<(PathBuf, PathBuf)>,
    options: CreateOptions,
    elevated_retry: bool,
) {
    let total = items.len();
    let initial_title = progress_title_with_filename(
        &format!("Creating archive 1/{total}"),
        &items[0].1,
    );
    let cancel = CancellationToken::new();
    *state
        .cancellation
        .lock()
        .expect("cancellation mutex poisoned") = Some(cancel.clone());
    ui.set_progress_title(initial_title.into());
    ui.set_progress_file(items[0].1.display().to_string().into());
    set_initial_progress_window(ui);
    let weak = ui.as_weak();
    let weak_progress = weak.clone();
    let subcommand = match options.format {
        CreateFormat::Zip => "zip-each",
        CreateFormat::SevenZip => "7z-each",
        _ => "",
    };
    std::thread::spawn(move || {
        let mut failures: Vec<(PathBuf, PathBuf, ArchiveError)> = Vec::new();
        for (index, (source, destination)) in items.iter().enumerate() {
            if cancel.is_cancelled() {
                break;
            }
            let title = progress_title_with_filename(
                &format!("Creating archive {}/{}", index + 1, total),
                destination,
            );
            let destination_display = destination.display().to_string();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_progress_title(title.into());
                ui.set_progress_file(destination_display.into());
                set_initial_progress_window(&ui);
            });
            let source_files = vec![source.clone()];
            let progress = |snapshot: ProgressSnapshot| {
                update_progress_window_details(&weak_progress, snapshot)
            };
            match engine.create(destination, &source_files, &options, &progress, &cancel) {
                Ok(_) => {}
                Err(ArchiveError::Cancelled) => break,
                Err(error) => failures.push((source.clone(), destination.clone(), error)),
            }
        }

        let _ = weak.upgrade_in_event_loop(move |ui| {
            if !cancel.is_cancelled()
                && request_context_create_batch_elevation(
                    elevated_retry,
                    subcommand,
                    &failures,
                )
            {
                close_progress_window(&ui);
                return;
            }
            if !cancel.is_cancelled() && !failures.is_empty() {
                let details = failures
                    .iter()
                    .map(|(source, _, error)| format!("{}: {error}", source.display()))
                    .collect::<Vec<_>>()
                    .join("\n");
                platform::show_error("Create archives", &format!("Some folders failed:\n{details}"));
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
    progress_window: Rc<RefCell<Option<ProgressWindow>>>,
) {
    let progress_title = progress_title_with_filename("Testing archive", &archive);
    let archive_display = archive.display().to_string();
    let (cancel, weak_progress) = match begin_progress_window_operation(
        ui,
        &state,
        &progress_window,
        &progress_title,
        &archive_display,
    ) {
        Ok(operation) => operation,
        Err(error) => {
            ui.set_status_text(error.clone().into());
            platform::show_error("Test archive", &error);
            return;
        }
    };
    let weak = ui.as_weak();
    let weak_progress_updates = weak_progress.clone();
    let weak_progress_finished = weak_progress.clone();
    let open_password = Arc::clone(&state.open_password);
    let pending_test_password_path = Arc::clone(&state.pending_test_password_path);
    let progress = move |snapshot: ProgressSnapshot| {
        update_progress_window_details(&weak_progress_updates, snapshot)
    };
    std::thread::spawn(move || {
        let result = engine.test(&archive, password.as_deref(), &progress, &cancel);
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if let Some(progress_ui) = weak_progress_finished.upgrade() {
                let _ = progress_ui.hide();
            }
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

/// Starts an operation from the main UI but renders progress in the same
/// standalone window used by Explorer verbs. The strong handle is retained on
/// the UI thread; workers receive only weak handles and cancellation tokens.
fn begin_progress_window_operation(
    ui: &AppWindow,
    state: &AppState,
    progress_window: &Rc<RefCell<Option<ProgressWindow>>>,
    title: &str,
    current_file: &str,
) -> Result<(CancellationToken, slint::Weak<ProgressWindow>), String> {
    let (progress_ui, progress_state) = open_progress_window()?;
    let cancel = CancellationToken::new();
    *state
        .cancellation
        .lock()
        .expect("cancellation mutex poisoned") = Some(cancel.clone());
    *progress_state
        .cancellation
        .lock()
        .expect("progress cancellation mutex poisoned") = Some(cancel.clone());

    ui.set_busy(true);
    ui.set_status_text(title.into());
    progress_ui.set_progress_title(title.into());
    progress_ui.set_progress_file(current_file.into());
    set_initial_progress_window(&progress_ui);
    let weak = progress_ui.as_weak();
    *progress_window.borrow_mut() = Some(progress_ui);
    Ok((cancel, weak))
}

fn finish_operation(ui: &AppWindow) {
    ui.set_busy(false);
    ui.set_conflict_visible(false);
}

/// Keeps the current operation title while progress values stream in, so a
/// batch operation can show "Extracting archive 2/3" while entries stream in.
struct UiConflictResolver {
    weak: slint::Weak<AppWindow>,
    progress: slint::Weak<ProgressWindow>,
    pending: Arc<Mutex<Option<mpsc::SyncSender<ConflictChoice>>>>,
    cancel: CancellationToken,
}

impl ConflictResolver for UiConflictResolver {
    fn resolve(&self, destination: &Path) -> ConflictChoice {
        let (sender, receiver) = mpsc::sync_channel(1);
        *self.pending.lock().expect("conflict mutex poisoned") = Some(sender);
        let display = destination.display().to_string();
        let progress = self.progress.clone();
        let _ = self.weak.clone().upgrade_in_event_loop(move |ui| {
            if let Some(progress_ui) = progress.upgrade() {
                let _ = progress_ui.hide();
            }
            ui.set_conflict_path(display.into());
            ui.set_conflict_visible(true);
        });

        let choice = loop {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(choice) => break choice,
                Err(mpsc::RecvTimeoutError::Timeout) if !self.cancel.is_cancelled() => {}
                Err(_) => break ConflictChoice::Cancel,
            }
        };
        if choice != ConflictChoice::Cancel {
            let _ = self.progress.clone().upgrade_in_event_loop(|ui| {
                let _ = ui.show();
            });
        }
        choice
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
    if let Some(base_length) = split_archive_volume_base_name(&name).map(str::len) {
        name.truncate(base_length);
    }
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

fn close_archive(ui: &AppWindow, state: &AppState) {
    state.clear_archive();
    *state.open_password.lock().expect("password mutex poisoned") = None;
    *state
        .pending_password_path
        .lock()
        .expect("password-path mutex poisoned") = None;
    *state
        .pending_test_password_path
        .lock()
        .expect("test-password-path mutex poisoned") = None;

    ui.set_archive_title("".into());
    ui.set_current_folder("".into());
    ui.set_sort_column(0);
    ui.set_sort_ascending(true);
    ui.set_has_archive(false);
    ui.set_can_go_up(false);
    ui.set_selection_state(0);
    ui.set_selection_count(0);
    ui.set_status_text("Archive closed".into());
    ui.set_summary_text("No archive open".into());
}

fn update_selection_ui(weak: &slint::Weak<AppWindow>, state: &AppState) {
    let display = state.display.borrow();
    let selected = state.selected.borrow();
    let selection_count = i32::try_from(selected.len()).unwrap_or(i32::MAX);
    let state_value = if display.is_empty() || selected.is_empty() {
        0
    } else if display
        .iter()
        .all(|entry| selected.contains(&entry.relative_path))
    {
        2
    } else {
        1
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_selection_state(state_value);
        ui.set_selection_count(selection_count);
    }
}

/// Human-readable state of the Explorer context-menu registration.
fn context_menu_state_text() -> &'static str {
    if platform::shell_ext::is_context_menu_managed_by_package() {
        if platform::shell_ext::is_context_menu_registered() {
            "Registered (MSIX)"
        } else {
            "Not registered (MSIX)"
        }
    } else if platform::shell_ext::is_context_menu_registered() {
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

/// Locates the bundled third-party notice file in portable, packaged, and
/// Cargo build output layouts.
fn third_party_notices_path() -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not locate ArchiveRclick: {error}"))?;
    let directory = executable.parent().unwrap_or_else(|| Path::new("."));
    let candidates = [
        directory.join("THIRD-PARTY-NOTICES.md"),
        directory.join("runtime").join("THIRD-PARTY-NOTICES.md"),
        directory.join("THIRD-PARTY-LICENSES.md"),
    ];
    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .ok_or_else(|| {
            format!(
                "The bundled third-party license notice was not found next to the app. Expected {}.",
                candidates[0].display()
            )
        })
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

struct ProgressUiText {
    file_percent: String,
    percent: String,
    elapsed: String,
    remaining: String,
    total: String,
    detail: String,
    file_value: f32,
    value: f32,
}

fn initial_progress_text() -> ProgressUiText {
    let snapshot = ProgressSnapshot::new(crate::tasks::ProgressPhase::Opening);
    progress_ui_text(&snapshot)
}

fn set_initial_progress_window(ui: &ProgressWindow) {
    let text = initial_progress_text();
    ui.set_progress_file_percent(text.file_percent.into());
    ui.set_progress_percent(text.percent.into());
    ui.set_progress_elapsed(text.elapsed.into());
    ui.set_progress_remaining(text.remaining.into());
    ui.set_progress_total(text.total.into());
    ui.set_progress_detail(text.detail.into());
    ui.set_progress_file_value(text.file_value);
    ui.set_progress_value(text.value);
}

fn progress_ui_text(snapshot: &ProgressSnapshot) -> ProgressUiText {
    let file_value = snapshot.current_file_fraction().unwrap_or(-1.0);
    let value = if snapshot.total_bytes.is_some() || snapshot.total_entries.is_some() {
        snapshot.fraction()
    } else {
        -1.0
    };
    let file_percent = if file_value < 0.0 {
        "—%".to_owned()
    } else {
        format!("{:.0}%", file_value * 100.0)
    };
    let percent = if value < 0.0 {
        "—%".to_owned()
    } else {
        format!("{:.0}%", value * 100.0)
    };
    let entry_detail = if let Some(total_entries) = snapshot.total_entries {
        format!(
            "Files {} / {}",
            snapshot.entries_processed.min(total_entries),
            total_entries
        )
    } else {
        format!("Files {}", snapshot.entries_processed)
    };
    ProgressUiText {
        file_percent,
        percent,
        elapsed: format!("Elapsed {}", format_duration(Some(snapshot.elapsed))),
        remaining: format!(
            "Remaining {}",
            format_duration(snapshot.estimated_remaining)
        ),
        total: format!("Total {}", format_duration(snapshot.estimated_total)),
        detail: format!(
            "{entry_detail}  •  {} processed",
            compact_bytes(snapshot.bytes_processed)
        ),
        file_value,
        value,
    }
}

fn format_duration(duration: Option<Duration>) -> String {
    let Some(duration) = duration else {
        return "—".to_owned();
    };
    let seconds = duration.as_secs();
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
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
        archive_directory_name, cli_archive_destination, common_parent_folder,
        create_formats_for_ui, is_archive_drop_path, parse_dropped_path, parse_elevated_batch_output,
        parse_elevated_extract, parse_elevated_output, progress_ui_text, run_with_startup_argument,
        unique_path, language_preference_selection_index, language_registry_key,
        language_selection_index,
    };
    use crate::archive::CreateFormat;
    use crate::tasks::{ProgressPhase, ProgressSnapshot};
    use std::{
        ffi::OsString,
        path::{Path, PathBuf},
    };

    #[test]
    fn strips_compound_archive_extensions() {
        assert_eq!(
            archive_directory_name(Path::new("backup.tar.zst")),
            "backup"
        );
        assert_eq!(archive_directory_name(Path::new("docs.zip")), "docs");
        assert_eq!(archive_directory_name(Path::new("adf.7z.001")), "adf");
        assert_eq!(archive_directory_name(Path::new("adf.7Z.001")), "adf");
    }

    #[test]
    fn parses_file_uri_drop_text() {
        assert_eq!(
            parse_dropped_path("file:///C:/Temp/My%20Archive.zip\r\n"),
            Some(PathBuf::from(r"C:\Temp\My Archive.zip"))
        );
    }

    #[test]
    fn drop_filter_rejects_media_files_and_accepts_archive_names() {
        assert!(!is_archive_drop_path(Path::new("movie.mkv")));
        assert!(is_archive_drop_path(Path::new("backup.ZIP")));
        assert!(is_archive_drop_path(Path::new("backup.7z.001")));
        assert!(is_archive_drop_path(Path::new("installer.ISO")));
        assert!(is_archive_drop_path(Path::new("installer.IMG")));
        assert!(!is_archive_drop_path(Path::new("movie.mkv.001")));
    }

    #[test]
    fn progress_detail_shows_processed_and_total_file_counts() {
        let mut snapshot = ProgressSnapshot::new(ProgressPhase::Compressing);
        snapshot.entries_processed = 3;
        snapshot.total_entries = Some(4);

        assert!(progress_ui_text(&snapshot).detail.starts_with("Files 3 / 4"));
    }

    #[test]
    fn help_command_does_not_load_libarchive() {
        assert!(run_with_startup_argument(Some("--help".into())).is_ok());
    }

    #[test]
    fn default_language_is_a_separate_preference_from_the_effective_ui_language() {
        assert_eq!(language_preference_selection_index("default"), 0);
        assert_eq!(language_preference_selection_index("en"), 1);
        assert_eq!(language_preference_selection_index("ko"), 2);
        assert_eq!(language_preference_selection_index("ja"), 3);
        assert_eq!(language_registry_key(0), "default");

        assert_eq!(language_selection_index("en"), 0);
        assert_eq!(language_selection_index("ko"), 1);
        assert_eq!(language_selection_index("ja"), 2);
        assert!(matches!(language_selection_index("default"), 0..=2));
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
    fn create_formats_for_ui_keeps_only_zip_and_sevenzip() {
        assert_eq!(
            create_formats_for_ui(CreateFormat::ALL.to_vec()),
            vec![CreateFormat::Zip, CreateFormat::SevenZip]
        );
    }

    #[test]
    fn elevated_create_retry_preserves_the_original_destination() {
        let args = vec![
            OsString::from("--output"),
            OsString::from(r"C:\Windows\Archive.zip"),
            OsString::from(r"C:\Windows\Input folder"),
        ];
        let (destination, sources) = parse_elevated_output(&args, true).expect("valid retry");
        assert_eq!(destination, Some(PathBuf::from(r"C:\Windows\Archive.zip")));
        assert_eq!(sources, vec![OsString::from(r"C:\Windows\Input folder")]);

        let (destination, sources) = parse_elevated_output(&args[2..], false).expect("normal CLI");
        assert_eq!(destination, None);
        assert_eq!(sources, vec![OsString::from(r"C:\Windows\Input folder")]);
    }

    #[test]
    fn elevated_per_folder_create_retry_keeps_each_destination() {
        let args = vec![
            OsString::from("--output"),
            OsString::from(r"C:\Windows\one.zip"),
            OsString::from(r"C:\Temp\one"),
            OsString::from("--output"),
            OsString::from(r"C:\Windows\two.7z"),
            OsString::from(r"C:\Temp\two"),
        ];
        let parsed = parse_elevated_batch_output(&args, true).expect("valid batch retry");
        assert_eq!(
            parsed,
            vec![
                (
                    Some(PathBuf::from(r"C:\Windows\one.zip")),
                    OsString::from(r"C:\Temp\one"),
                ),
                (
                    Some(PathBuf::from(r"C:\Windows\two.7z")),
                    OsString::from(r"C:\Temp\two"),
                ),
            ]
        );

        let normal = parse_elevated_batch_output(
            &[OsString::from(r"C:\Temp\one"), OsString::from(r"C:\Temp\two")],
            false,
        )
        .expect("valid normal batch invocation");
        assert_eq!(
            normal,
            vec![
                (None, OsString::from(r"C:\Temp\one")),
                (None, OsString::from(r"C:\Temp\two")),
            ]
        );
    }

    #[test]
    fn elevated_extract_retry_preserves_each_output_directory() {
        let args = vec![
            OsString::from("--output"),
            OsString::from(r"C:\Windows\one"),
            OsString::from(r"C:\Temp\one.zip"),
            OsString::from("--output"),
            OsString::from(r"C:\Windows\two"),
            OsString::from(r"C:\Temp\two.zip"),
        ];
        let parsed = parse_elevated_extract(&args, true).expect("valid retry");
        assert_eq!(
            parsed,
            vec![
                (
                    OsString::from(r"C:\Temp\one.zip"),
                    Some(PathBuf::from(r"C:\Windows\one")),
                ),
                (
                    OsString::from(r"C:\Temp\two.zip"),
                    Some(PathBuf::from(r"C:\Windows\two")),
                ),
            ]
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
        assert_eq!(common_parent_folder(&sources), PathBuf::from("data/photos"));
    }

    #[test]
    fn unique_path_appends_suffix_when_name_taken() {
        let dir =
            std::env::temp_dir().join(format!("archive-rclick-unique-{}", std::process::id()));
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
