//! Progress-only shell operation window and batch-operation orchestration.

use super::*;

mod presentation;
mod prompts;

pub(super) use presentation::{compact_bytes, progress_ui_text, set_initial_progress_window};
use prompts::{ProgressConflictPrompt, ProgressConflictResolver, ProgressPasswordPrompt};
pub(super) use prompts::{conflict_choice_from_response, first_conflict_choice_from_response};

pub(super) struct ProgressWindowState {
    pub(super) cancellation: Arc<Mutex<Option<CancellationToken>>>,
    password_prompt: Arc<ProgressPasswordPrompt>,
    conflict_prompt: Arc<ProgressConflictPrompt>,
    initial_show_error: InitialWindowShowError,
}

// Keep these in sync with ProgressWindow's preferred size in
// ui/progress_window.slint. They are applied before the first visible frame.
pub(super) const PROGRESS_WINDOW_LOGICAL_WIDTH: f32 = 600.0;
pub(super) const PROGRESS_WINDOW_LOGICAL_HEIGHT: f32 = 280.0;

/// Builds and shows the small progress window; its Cancel button cancels the
/// running operation.
pub(super) fn open_progress_window() -> Result<(ProgressWindow, Rc<ProgressWindowState>), String> {
    let ui = ProgressWindow::new().map_err(|error| format!("Could not create the UI: {error}"))?;
    let theme_selection = theme_selection_index(&platform::load_theme_preference());
    ui.set_theme_selection(theme_selection);
    ui.set_language_selection(language_selection_index(
        &platform::load_language_preference(),
    ));
    let initial_show_error = Arc::new(Mutex::new(None));
    let state = Rc::new(ProgressWindowState {
        cancellation: Arc::new(Mutex::new(None)),
        password_prompt: Arc::new(ProgressPasswordPrompt::new()),
        conflict_prompt: Arc::new(ProgressConflictPrompt::new()),
        initial_show_error: Arc::clone(&initial_show_error),
    });
    let state_for_cancel = Rc::clone(&state);
    let password_for_cancel = Arc::clone(&state.password_prompt);
    let conflict_for_cancel = Arc::clone(&state.conflict_prompt);
    let weak_for_cancel = ui.as_weak();
    ui.on_cancel_requested(move || {
        if let Some(ui) = weak_for_cancel.upgrade()
            && ui.get_error_visible()
        {
            close_progress_window(&ui);
            return;
        }
        if let Some(token) = state_for_cancel
            .cancellation
            .lock()
            .expect("cancellation mutex poisoned")
            .as_ref()
        {
            token.cancel();
        }
        password_for_cancel.respond(None);
        conflict_for_cancel.respond(ConflictChoice::Cancel);
        if let Some(ui) = weak_for_cancel.upgrade() {
            ui.set_progress_title("Cancelling…".into());
            ui.set_conflict_visible(false);
            platform::taskbar::pause(ui.window());
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
    let conflict_for_close = Arc::clone(&state.conflict_prompt);
    let weak_for_close = ui.as_weak();
    ui.window().on_close_requested(move || {
        if let Some(ui) = weak_for_close.upgrade()
            && ui.get_error_visible()
        {
            close_progress_window(&ui);
            return slint::CloseRequestResponse::HideWindow;
        }
        if let Some(token) = state_for_close
            .cancellation
            .lock()
            .expect("cancellation mutex poisoned")
            .as_ref()
        {
            token.cancel();
        }
        password_for_close.respond(None);
        conflict_for_close.respond(ConflictChoice::Cancel);
        if let Some(ui) = weak_for_close.upgrade() {
            ui.set_progress_title("Cancelling…".into());
            ui.set_conflict_visible(false);
            platform::taskbar::pause(ui.window());
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
    let conflict_for_response = Arc::clone(&state.conflict_prompt);
    let weak_for_conflict_response = ui.as_weak();
    ui.on_conflict_response(move |response| {
        if let Some(ui) = weak_for_conflict_response.upgrade() {
            ui.set_conflict_visible(false);
        }
        conflict_for_response.respond(first_conflict_choice_from_response(response));
    });
    let font_family = platform::resolve_font_family(&platform::load_font_preference());
    let weak_for_error = ui.as_weak();
    ui.on_error_dismissed(move || {
        if let Some(ui) = weak_for_error.upgrade() {
            close_progress_window(&ui);
        }
    });
    ui.set_font_family(font_family.into());
    ui.set_progress_title("Working…".into());
    ui.set_progress_file("".into());
    set_initial_progress_window(&ui);
    // Select the cursor's monitor before the native window exists. The exact
    // position is recalculated from the real hidden window below.
    platform::center_window_with_logical_size(
        ui.window(),
        slint::LogicalSize::new(
            PROGRESS_WINDOW_LOGICAL_WIDTH,
            PROGRESS_WINDOW_LOGICAL_HEIGHT,
        ),
    );

    // Let the event loop create the native window hidden. Apply its final
    // size, centered position, and title-bar theme before mapping it so no
    // default-size/default-theme border can flash first.
    let weak_for_show = ui.as_weak();
    slint::invoke_from_event_loop(move || {
        let Some(ui) = weak_for_show.upgrade() else {
            return;
        };
        ui.window().set_size(slint::LogicalSize::new(
            PROGRESS_WINDOW_LOGICAL_WIDTH,
            PROGRESS_WINDOW_LOGICAL_HEIGHT,
        ));
        platform::center_window(ui.window());
        platform::apply_window_theme(ui.window(), theme_selection);
        if let Err(error) = ui.show() {
            let message = format!("Could not show the progress window: {error}");
            match initial_show_error.lock() {
                Ok(mut slot) => *slot = Some(message),
                Err(poisoned) => *poisoned.into_inner() = Some(message),
            }
            let _ = ui.hide();
        }
    })
    .map_err(|error| format!("Could not schedule the initial progress-window show: {error}"))?;
    Ok((ui, state))
}

/// Runs a progress-only window whose initial show was deferred until its
/// native geometry and title-bar theme were ready.
pub(super) fn run_progress_window(
    ui: &ProgressWindow,
    state: &ProgressWindowState,
) -> Result<(), String> {
    let mut result =
        slint::run_event_loop().map_err(|error| format!("UI event loop failed: {error}"));
    let _ = ui.hide();
    if result.is_ok()
        && let Some(error) = take_initial_window_show_error(&state.initial_show_error)
    {
        result = Err(error);
    }
    result
}

/// Hides the progress window and ends the event loop so the process exits.
fn close_progress_window(ui: &ProgressWindow) {
    // Drop the taskbar progress overlay before the window handle disappears.
    platform::taskbar::clear(ui.window());
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
    if snapshot.phase == ProgressPhase::Finished {
        platform::taskbar::clear(ui.window());
    } else if text.value < 0.0 {
        platform::taskbar::show_indeterminate(ui.window());
    } else {
        platform::taskbar::show_fraction(ui.window(), text.value);
    }
}

/// Keeps the operation title while progress values stream in.
pub(super) fn update_progress_window_details(
    weak: &slint::Weak<ProgressWindow>,
    snapshot: ProgressSnapshot,
) {
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
    ask_conflicts: bool,
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
    if ask_conflicts {
        args.push(OsString::from("--ask-conflicts"));
    }
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

pub(super) fn start_extract_batch_window(
    ui: &ProgressWindow,
    state: &Rc<ProgressWindowState>,
    engine: Engine,
    archives: Vec<PathBuf>,
    destination_overrides: Vec<Option<PathBuf>>,
    elevated_retry: bool,
    ask_conflicts: bool,
) {
    let total = archives.len();
    let first = archives[0].display().to_string();
    let initial_title =
        progress_title_with_filename(&format!("Extracting archive 1/{total}"), &archives[0]);
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
    let conflict_resolver = ProgressConflictResolver {
        weak: weak.clone(),
        prompt: Arc::clone(&state.conflict_prompt),
        cancel: cancel.clone(),
        selected_policy: Mutex::new(None),
    };
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
                    conflict_policy: if ask_conflicts {
                        InitialConflictPolicy::Ask
                    } else {
                        InitialConflictPolicy::OverwriteAll
                    },
                    ..ExtractOptions::default()
                };
                match engine.extract(
                    archive,
                    &destination,
                    &options,
                    &progress,
                    if ask_conflicts {
                        &conflict_resolver
                    } else {
                        &OverwriteAllResolver
                    },
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
                && request_context_extract_elevation(elevated_retry, &failures, ask_conflicts)
            {
                close_progress_window(&ui);
                return;
            }
            if !cancel.is_cancelled() && !failures.is_empty() {
                if processed == 0 {
                    let (path, _, error) = failures.remove(0);
                    let message = format!(
                        "{}: {}",
                        path.display(),
                        operations::archive_error_message(&error, ui.get_language_selection())
                    );
                    show_progress_error(&ui, "Extract archives", &message);
                } else {
                    let details = failures
                        .iter()
                        .map(|(path, _, error)| {
                            format!(
                                "{}: {}",
                                path.display(),
                                operations::archive_error_message(
                                    error,
                                    ui.get_language_selection()
                                )
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    show_progress_error(
                        &ui,
                        "Extract archives",
                        &format!("Some archives failed:\n{details}"),
                    );
                }
                return;
            }
            close_progress_window(&ui);
        });
    });
}

/// Creates an archive from the Explorer right-click menu inside the
/// progress-only window; the window closes when the work is done.
pub(super) fn start_create_window(
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
    let progress =
        move |snapshot: ProgressSnapshot| update_progress_window_details(&weak_progress, snapshot);
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
                        show_progress_error(&ui, "Create archive", &error.to_string());
                        return;
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
pub(super) fn start_create_batch_window(
    ui: &ProgressWindow,
    state: &Rc<ProgressWindowState>,
    engine: Engine,
    items: Vec<(PathBuf, PathBuf)>,
    options: CreateOptions,
    elevated_retry: bool,
) {
    let total = items.len();
    let initial_title =
        progress_title_with_filename(&format!("Creating archive 1/{total}"), &items[0].1);
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
                && request_context_create_batch_elevation(elevated_retry, subcommand, &failures)
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
                show_progress_error(
                    &ui,
                    "Create archives",
                    &format!("Some folders failed:\n{details}"),
                );
                return;
            }
            close_progress_window(&ui);
        });
    });
}

fn show_progress_error(ui: &ProgressWindow, title: &str, message: &str) {
    platform::taskbar::clear(ui.window());
    ui.set_password_visible(false);
    ui.set_conflict_visible(false);
    ui.set_error_title(operations::error_title(title, ui.get_language_selection()).into());
    ui.set_error_message(message.into());
    ui.set_error_visible(true);
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn progress_error_can_be_dismissed_at_minimum_window_size() {
        slint::platform::set_platform(Box::new(i_slint_backend_testing::TestingBackend::new(
            i_slint_backend_testing::TestingBackendOptions {
                renderer_name: Some("software".into()),
                ..Default::default()
            },
        )))
        .unwrap();
        let ui = ProgressWindow::new().unwrap();
        ui.set_language_selection(1);
        ui.window().set_size(slint::PhysicalSize::new(540, 280));
        let dismissed = Rc::new(Cell::new(false));
        let acknowledged = Rc::clone(&dismissed);
        ui.on_error_dismissed(move || acknowledged.set(true));
        ui.on_cancel_requested(|| panic!("error dismissal must not cancel another operation"));
        show_progress_error(
            &ui,
            "Extract archives",
            "지원하는 압축파일이 아니거나 파일이 손상되었습니다.\nC:\\Downloads\\not-an-archive.zip",
        );
        ui.show().unwrap();
        let snapshot = ui.window().take_snapshot().unwrap();
        if let Some(path) = std::env::var_os("ARCHIVERCLICK_PROGRESS_ERROR_SNAPSHOT") {
            image::save_buffer(
                path,
                snapshot.as_bytes(),
                snapshot.width(),
                snapshot.height(),
                image::ColorType::Rgba8,
            )
            .unwrap();
        }
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: slint::platform::Key::Escape.into(),
            });
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyReleased {
                text: slint::platform::Key::Escape.into(),
            });
        assert!(dismissed.get());
        ui.hide().unwrap();
    }
}
