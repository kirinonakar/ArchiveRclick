//! Main-window archive use cases: list, extract, create, and integrity test.

use super::*;

pub(super) fn start_listing(
    ui: &AppWindow,
    state: Rc<AppState>,
    engine: Engine,
    path: PathBuf,
    password: Option<String>,
    pathname_codepage: u32,
    directory: PathBuf,
) {
    if ui.get_busy() || ui.get_error_visible() {
        return;
    }
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
    let cancellation = Arc::clone(&state.cancellation);
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
            *cancellation.lock().expect("cancellation mutex poisoned") = None;
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
                Err(error) => {
                    *pending_password_path
                        .lock()
                        .expect("password-path mutex poisoned") = None;
                    display_operation_error(&ui, "Open archive", error);
                }
            }
        });
    });
}

pub(super) fn progress_title_with_filename(operation: &str, path: &Path) -> String {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.display().to_string());
    format!("{operation} - {file_name}")
}

pub(super) fn start_extract(
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

pub(super) fn start_create(
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

pub(super) fn start_test(
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

pub(super) fn extraction_destination(archive: &Path, mode: i32) -> Result<Option<PathBuf>, String> {
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

pub(super) fn set_create_sources(ui: &AppWindow, state: &AppState, paths: Vec<PathBuf>) {
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

pub(super) fn archive_directory_name(archive: &Path) -> String {
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

pub(super) fn default_archive_name(sources: &[PathBuf], format: CreateFormat) -> String {
    let stem = sources
        .first()
        .and_then(|path| path.file_stem())
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "archive".to_owned());
    format!("{stem}.{}", format.default_extension())
}

pub(super) fn close_archive(ui: &AppWindow, state: &AppState) {
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

pub(super) fn update_selection_ui(weak: &slint::Weak<AppWindow>, state: &AppState) {
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

fn display_operation_error(ui: &AppWindow, title: &str, error: ArchiveError) {
    if matches!(error, ArchiveError::Cancelled) {
        ui.set_status_text("Operation cancelled".into());
        return;
    }
    let message = archive_error_message(&error, ui.get_language_selection());
    show_ui_error(&ui.as_weak(), title, message);
}

pub(super) fn archive_error_message(error: &ArchiveError, language: i32) -> String {
    match (error, language) {
        (ArchiveError::InvalidArchive(path), 1) => format!(
            "지원하는 압축파일이 아니거나 파일이 손상되었습니다.\n{}",
            path.display()
        ),
        (ArchiveError::InvalidArchive(path), 2) => format!(
            "対応しているアーカイブではないか、ファイルが破損しています。\n{}",
            path.display()
        ),
        _ => error.to_string(),
    }
}

pub(super) fn error_title(title: &str, language: i32) -> &str {
    match (title, language) {
        ("Open archive", 1) => "압축파일 열기 실패",
        ("Extract archive" | "Extract archives", 1) => "압축 풀기 실패",
        ("Create archive" | "Create archives", 1) => "압축파일 만들기 실패",
        ("Test archive", 1) => "압축파일 검사 실패",
        ("Open archive", 2) => "アーカイブを開けません",
        ("Extract archive" | "Extract archives", 2) => "展開に失敗しました",
        ("Create archive" | "Create archives", 2) => "アーカイブの作成に失敗しました",
        ("Test archive", 2) => "アーカイブの検証に失敗しました",
        _ => title,
    }
}

pub(super) fn show_ui_error(weak: &slint::Weak<AppWindow>, title: &str, message: String) {
    if let Some(ui) = weak.upgrade() {
        ui.set_status_text(message.clone().into());
        ui.set_error_title(error_title(title, ui.get_language_selection()).into());
        ui.set_error_message(message.into());
        ui.set_error_visible(true);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::Instant;

    #[test]
    fn invalid_archive_error_keeps_ui_responsive() {
        slint::platform::set_platform(Box::new(i_slint_backend_testing::TestingBackend::new(
            i_slint_backend_testing::TestingBackendOptions {
                threading: true,
                renderer_name: Some("software".into()),
                ..Default::default()
            },
        )))
        .unwrap();
        let directory =
            std::env::temp_dir().join(format!("archive-rclick-error-ui-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let invalid = directory.join("not-an-archive.zip");
        fs::write(&invalid, b"not an archive").unwrap();
        let valid = directory.join("valid.zip");
        let engine = load_engine().unwrap();
        engine
            .create(
                &valid,
                std::slice::from_ref(&invalid),
                &CreateOptions::default(),
                &|_: ProgressSnapshot| {},
                &CancellationToken::new(),
            )
            .unwrap();

        let ui = AppWindow::new().unwrap();
        ui.set_language_selection(1);
        ui.set_esc_close_main_window(true);
        ui.on_main_window_close_requested(|| panic!("dismissing an error must not close the app"));
        ui.window().set_size(slint::PhysicalSize::new(960, 640));
        let state = Rc::new(AppState::new());
        ui.set_archive_rows(ModelRc::from(Rc::clone(&state.rows)));
        ui.show().unwrap();
        start_listing(
            &ui,
            Rc::clone(&state),
            Arc::clone(&engine),
            invalid.clone(),
            None,
            0,
            PathBuf::new(),
        );

        let phase = Rc::new(Cell::new(0));
        let phase_in_timer = Rc::clone(&phase);
        let error_rendered = Cell::new(false);
        let weak = ui.as_weak();
        let timer = slint::Timer::default();
        let started = Instant::now();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(20),
            move || {
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "listing/error UI stalled"
                );
                let ui = weak.upgrade().unwrap();
                if ui.get_busy() {
                    return;
                }
                assert!(state.cancellation.lock().unwrap().is_none());
                match phase_in_timer.get() {
                    0 | 2 => {
                        assert!(ui.get_error_visible());
                        assert!(
                            ui.get_error_message()
                                .contains("지원하는 압축파일이 아니거나")
                        );
                        assert_eq!(ui.get_has_archive(), phase_in_timer.get() == 2);
                        // Rendering also instantiates the overlay and its focus scope.
                        let snapshot = ui.window().take_snapshot().unwrap();
                        // Let the deferred focus run between instantiating
                        // the overlay and sending the dismissal key.
                        if !error_rendered.replace(true) {
                            return;
                        }
                        if let Some(path) = std::env::var_os("ARCHIVERCLICK_ERROR_SNAPSHOT") {
                            image::save_buffer(
                                path,
                                snapshot.as_bytes(),
                                snapshot.width(),
                                snapshot.height(),
                                image::ColorType::Rgba8,
                            )
                            .unwrap();
                        }
                        let key = if phase_in_timer.get() == 0 {
                            slint::platform::Key::Return
                        } else {
                            slint::platform::Key::Escape
                        };
                        ui.window()
                            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                                text: key.into(),
                            });
                        ui.window()
                            .dispatch_event(slint::platform::WindowEvent::KeyReleased {
                                text: key.into(),
                            });
                        assert!(
                            !ui.get_error_visible(),
                            "Enter/Escape should dismiss the error"
                        );
                        error_rendered.set(false);
                        if phase_in_timer.get() == 2 {
                            assert_eq!(ui.get_archive_title(), "valid.zip");
                            phase_in_timer.set(3);
                            slint::quit_event_loop().unwrap();
                            return;
                        }
                        phase_in_timer.set(1);
                        start_listing(
                            &ui,
                            Rc::clone(&state),
                            Arc::clone(&engine),
                            valid.clone(),
                            None,
                            0,
                            PathBuf::new(),
                        );
                    }
                    1 => {
                        assert!(ui.get_has_archive());
                        assert_eq!(ui.get_archive_title(), "valid.zip");
                        phase_in_timer.set(2);
                        start_listing(
                            &ui,
                            Rc::clone(&state),
                            Arc::clone(&engine),
                            invalid.clone(),
                            None,
                            0,
                            PathBuf::new(),
                        );
                    }
                    _ => {}
                }
            },
        );
        slint::run_event_loop().unwrap();
        assert_eq!(phase.get(), 3);
        timer.stop();
        ui.hide().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }
}
