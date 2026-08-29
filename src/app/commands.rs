//! Application command orchestration.
//!
//! This module selects a launch mode and composes focused controllers. Archive
//! work, window lifecycle, shell verbs, progress UI, and input policies live in
//! their own modules so the entry point does not own their implementation.

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
    tasks::{CancellationToken, ProgressPhase, ProgressSnapshot},
};

use super::{AppState, ArchiveRowModel};

mod archive_paths;

type Engine = Arc<dyn ArchiveEngine>;
type InitialWindowShowError = Arc<Mutex<Option<String>>>;

fn take_initial_window_show_error(error: &InitialWindowShowError) -> Option<String> {
    match error.lock() {
        Ok(mut error) => error.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    }
}

mod callbacks;
mod context_menu;
mod drag_drop;
mod engine;
mod invocation;
mod operations;
mod preferences;
mod progress_window;
mod resources;
mod window;

use archive_paths::{is_archive_drop_path, split_archive_volume_base_name};
use callbacks::wire_callbacks;
use context_menu::OverwriteAllResolver;
pub(crate) use context_menu::{cli_archive_destination, unique_path};
#[cfg(test)]
use context_menu::{
    common_parent_folder, parse_elevated_batch_output, parse_elevated_extract,
    parse_elevated_output, right_drag_extract_destination,
};
use drag_drop::{
    cleanup_drag_staging_directories, handle_file_drop, parse_dropped_path, start_archive_drag,
};
use engine::{create_formats_for_ui, load_engine};
use invocation::{LaunchRequest, parse_launch_request};
use operations::{
    archive_directory_name, close_archive, default_archive_name, extraction_destination,
    progress_title_with_filename, set_create_sources, show_ui_error, start_create, start_extract,
    start_listing, start_test, update_selection_ui,
};
use preferences::{
    CODEPAGE_OPTIONS, FONT_OPTIONS, LANGUAGE_OPTIONS, PROJECT_GITHUB_URL,
    language_preference_selection_index, language_registry_key, language_selection_index,
    pathname_codepage, theme_registry_key, theme_selection_index,
};
use progress_window::{
    compact_bytes, conflict_choice_from_response, open_progress_window, run_progress_window,
    set_initial_progress_window, start_create_batch_window, start_create_window,
    start_extract_batch_window, update_progress_window_details,
};
#[cfg(test)]
use progress_window::{first_conflict_choice_from_response, progress_ui_text};
use resources::{
    context_menu_dll_path, context_menu_state_text, third_party_notices_path,
    third_party_runtime_licenses_path,
};
use window::{MainWindowSession, open_main_window, prepare_main_window_close};

pub fn run() -> Result<(), String> {
    // If this is the packaged build, remove stale HKCU registrations left by
    // a previous portable build. The cleanup is a no-op when nothing is left,
    // so launching the app does not refresh Explorer on every run.
    platform::shell_ext::cleanup_portable_context_menu_entries()?;
    match parse_launch_request(std::env::args_os().skip(1)) {
        LaunchRequest::MainWindow(startup_argument) => run_with_startup_argument(startup_argument),
        LaunchRequest::ContextMenu(command) => command.execute(),
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
    let MainWindowSession {
        ui,
        state,
        engine,
        initial_show_error,
    } = open_main_window(Rc::clone(&operation_progress_window))?;

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

    // The initial show is scheduled from `open_main_window` so the native
    // window can receive its restored geometry while it is still hidden.
    // Calling ComponentHandle::run() here would show it too early.
    let mut result =
        slint::run_event_loop().map_err(|error| format!("UI event loop failed: {error}"));
    let _ = ui.hide();
    if result.is_ok()
        && let Some(error) = take_initial_window_show_error(&initial_show_error)
    {
        result = Err(error);
    }
    cleanup_drag_staging_directories();
    result
}

#[cfg(test)]
mod tests {
    use super::{
        archive_directory_name, cli_archive_destination, common_parent_folder,
        create_formats_for_ui, first_conflict_choice_from_response, is_archive_drop_path,
        language_preference_selection_index, language_registry_key, language_selection_index,
        parse_dropped_path, parse_elevated_batch_output, parse_elevated_extract,
        parse_elevated_output, progress_ui_text, right_drag_extract_destination,
        run_with_startup_argument, unique_path,
    };
    use crate::archive::{ConflictChoice, CreateFormat};
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
    fn right_drag_extract_here_uses_the_receiving_folder_without_a_subfolder() {
        let destination = Path::new(r"C:\Drop Folder");
        let archive = Path::new(r"C:\Source Folder\sample.zip");

        assert_eq!(
            right_drag_extract_destination(destination, archive, true),
            destination
        );
        assert_eq!(
            right_drag_extract_destination(destination, archive, false),
            destination.join("sample")
        );
    }

    #[test]
    fn first_extract_here_conflict_sets_the_policy_for_remaining_files() {
        assert_eq!(
            first_conflict_choice_from_response(0),
            ConflictChoice::OverwriteAll
        );
        assert_eq!(
            first_conflict_choice_from_response(1),
            ConflictChoice::SkipAll
        );
        assert_eq!(
            first_conflict_choice_from_response(4),
            ConflictChoice::Cancel
        );
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

        assert!(
            progress_ui_text(&snapshot)
                .detail
                .starts_with("Files 3 / 4")
        );
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
            &[
                OsString::from(r"C:\Temp\one"),
                OsString::from(r"C:\Temp\two"),
            ],
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
