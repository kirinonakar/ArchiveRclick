//! Native and Slint drag/drop adaptation, including temporary extraction staging.

use super::*;
use crate::platform::explorer_drag::{DragFile, PrepareFiles};

pub(super) const DRAG_STAGING_PREFIX: &str = platform::explorer_drag::STAGING_PREFIX;

pub(super) fn start_archive_drag(
    ui: &AppWindow,
    state: &Rc<AppState>,
    engine: &Engine,
    row: usize,
    progress_window: &Rc<RefCell<Option<ProgressWindow>>>,
) {
    start_archive_drag_with(
        ui,
        state,
        engine,
        row,
        progress_window,
        platform::explorer_drag::is_drag_button_down,
        platform::start_file_drag,
    );
}

fn start_archive_drag_with(
    ui: &AppWindow,
    state: &Rc<AppState>,
    engine: &Engine,
    row: usize,
    progress_window: &Rc<RefCell<Option<ProgressWindow>>>,
    gesture_active: impl Fn() -> bool + Send + 'static,
    drag: impl FnOnce(
        Vec<DragFile>,
        Vec<u8>,
        Option<Vec<u8>>,
        PrepareFiles,
        CancellationToken,
        mpsc::SyncSender<Result<bool, String>>,
    ) -> Result<(), String>
    + Send
    + 'static,
) {
    if ui.get_busy()
        || ui.get_error_visible()
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

    let Some(mut listing) = state.listing.borrow().as_ref().cloned() else {
        return;
    };
    let archive = listing.archive_path.clone();
    let current_folder = state.current_folder.borrow().clone();
    let selection = state.selected_paths();
    if selection.is_empty() {
        return;
    }
    let password = state
        .open_password
        .lock()
        .expect("password mutex poisoned")
        .clone();
    let options = ExtractOptions {
        copy_zone_identifier: platform::load_copy_zone_identifier_preference(),
        zip_backend: ZipBackend::from_registry_key(&platform::load_zip_backend_preference()),
        threads: ThreadCount::from_registry_key(&platform::load_thread_preference()),
        selection: ExtractSelection::Paths(selection),
        password,
        conflict_policy: InitialConflictPolicy::OverwriteAll,
        pathname_codepage: pathname_codepage(ui.get_encoding_selection()),
        ..ExtractOptions::default()
    };
    let title = progress_title_with_filename("Extracting selected items", &archive);
    let (cancel, weak_progress) = match operations::begin_progress_window_operation_with_visibility(
        ui,
        state,
        progress_window,
        &title,
        &archive.display().to_string(),
        false,
    ) {
        Ok(operation) => operation,
        Err(error) => {
            show_ui_error(&ui.as_weak(), "Drag selected items", error);
            return;
        }
    };
    let weak = ui.as_weak();
    let engine = Arc::clone(engine);
    let cancellation = Arc::clone(&state.cancellation);
    std::thread::spawn(move || {
        if !gesture_active() {
            cancel.cancel();
        }
        let metadata_pending = Arc::new(Mutex::new(true));
        let pending = Arc::clone(&metadata_pending);
        let metadata_cancel = cancel.clone();
        std::thread::spawn(move || {
            loop {
                let is_pending = *pending.lock().unwrap();
                if !is_pending {
                    break;
                }
                if !gesture_active() {
                    // Serialize this check with the transition into native OLE:
                    // releasing after a valid drop must not cancel extraction.
                    if *pending.lock().unwrap() {
                        metadata_cancel.cancel();
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let warning = Arc::new(Mutex::new(None::<String>));
        let result = (|| {
            // ISO browsing intentionally retains only immediate children. Read
            // its complete metadata before offering a selected folder so the
            // virtual-file descriptors include every descendant. No files are
            // extracted or staged by this metadata-only operation.
            if listing.format_name.to_ascii_lowercase().contains("iso")
                && listing.entries.iter().any(|entry| {
                    entry.kind == crate::archive::ArchiveEntryKind::Directory
                        && options.selection.includes(&entry.path)
                })
            {
                listing = Arc::new(
                    engine
                        .list(
                            &archive,
                            options.password.as_deref(),
                            options.pathname_codepage,
                            &|_: ProgressSnapshot| {},
                            &cancel,
                        )
                        .map_err(|error| error.to_string())?,
                );
            }
            let files = drag_files(&listing, &options.selection, &current_folder, &cancel)?;
            if files.is_empty() {
                return Err("No regular files or folders were selected".to_owned());
            }
            let descriptors = platform::explorer_drag::prepare_descriptors(&files, &cancel)?;
            let origin = if options.copy_zone_identifier {
                platform::explorer_drag::source_zone_metadata(&archive)?
            } else {
                None
            };
            let updates = weak_progress.clone();
            let extract_cancel = cancel.clone();
            let extract_warning = Arc::clone(&warning);
            let prepare: PrepareFiles = Box::new(move || {
                if extract_cancel.is_cancelled() {
                    return Err(ArchiveError::Cancelled.to_string());
                }
                let _ = updates.clone().upgrade_in_event_loop(|ui| {
                    let _ = ui.show();
                });
                let staging = create_drag_staging_directory()?;
                let result = engine
                    .extract(
                        &archive,
                        staging.path(),
                        &options,
                        &move |snapshot| update_progress_window_details(&updates, snapshot),
                        &OverwriteAllResolver,
                        &extract_cancel,
                    )
                    .map_err(|error| error.to_string());
                let summary = match result {
                    Ok(summary) => summary,
                    Err(error) => {
                        platform::explorer_drag::cleanup_staging(staging);
                        return Err(error);
                    }
                };
                *extract_warning.lock().unwrap() = summary.warning;
                Ok(staging)
            });
            let (sender, receiver) = mpsc::sync_channel(1);
            *metadata_pending.lock().unwrap() = false;
            let drag_cancel = cancel.clone();
            weak.clone()
                .upgrade_in_event_loop(move |_ui| {
                    if drag_cancel.is_cancelled() {
                        let _ = sender.try_send(Ok(false));
                    } else if let Err(error) = drag(
                        files,
                        descriptors,
                        origin,
                        prepare,
                        drag_cancel,
                        sender.clone(),
                    ) {
                        let _ = sender.try_send(Err(error));
                    }
                })
                .map_err(|error| error.to_string())?;
            let dragged = receiver.recv().map_err(|error| error.to_string())??;
            Ok::<_, String>(dragged)
        })();
        *metadata_pending.lock().unwrap() = false;
        let cancelled = cancel.is_cancelled();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if let Some(progress) = weak_progress.upgrade() {
                platform::taskbar::clear(progress.window());
                let _ = progress.hide();
            }
            *cancellation.lock().expect("cancellation mutex poisoned") = None;
            ui.set_busy(false);
            match result {
                _ if cancelled => ui.set_status_text("Drag cancelled".into()),
                Ok(false) => ui.set_status_text("Drag cancelled".into()),
                Ok(true) => {
                    let status = warning.lock().unwrap().take().map_or_else(
                        || "Selected items were sent to Explorer".to_owned(),
                        |warning| format!("Selected items were sent to Explorer — {warning}"),
                    );
                    ui.set_status_text(status.into());
                }
                Err(error) => show_ui_error(&ui.as_weak(), "Drag selected items", error),
            }
        });
    });
}

fn create_drag_staging_directory() -> Result<tempfile::TempDir, String> {
    tempfile::Builder::new()
        .prefix(&format!("{DRAG_STAGING_PREFIX}{}-", std::process::id()))
        .tempdir()
        .map_err(|error| format!("Could not create the drag staging directory: {error}"))
}

fn drag_files(
    listing: &crate::archive::ArchiveListing,
    selection: &ExtractSelection,
    folder: &Path,
    cancel: &CancellationToken,
) -> Result<Vec<DragFile>, String> {
    use crate::archive::{ArchiveEntryKind, safe_relative_path};
    let mut files = std::collections::BTreeMap::new();
    let selected = match selection {
        ExtractSelection::All => None,
        ExtractSelection::Paths(paths) => Some(
            paths
                .iter()
                .map(PathBuf::as_path)
                .collect::<std::collections::HashSet<_>>(),
        ),
    };
    for entry in &listing.entries {
        if cancel.is_cancelled() {
            return Err(ArchiveError::Cancelled.to_string());
        }
        let path = match safe_relative_path(&entry.path) {
            Ok(path) => path,
            Err(ArchiveError::IncompatibleEntryPath(_)) => continue,
            Err(error) => return Err(error.to_string()),
        };
        if !selected
            .as_ref()
            .is_none_or(|paths| path.ancestors().any(|parent| paths.contains(parent)))
            || !matches!(
                entry.kind,
                ArchiveEntryKind::File | ArchiveEntryKind::Directory
            )
        {
            continue;
        }
        let name = path
            .strip_prefix(folder)
            .map_err(|error| error.to_string())?
            .to_owned();
        if name.as_os_str().is_empty() {
            continue;
        }
        let directory = entry.kind == ArchiveEntryKind::Directory;
        files.insert(
            name.clone(),
            DragFile {
                archive_path: path,
                name: name.clone(),
                size: entry.size,
                directory,
                modified_unix_seconds: entry.modified_unix_seconds,
            },
        );
        let mut parent = name.parent();
        while let Some(path) = parent.filter(|path| !path.as_os_str().is_empty()) {
            files.entry(path.to_owned()).or_insert_with(|| DragFile {
                archive_path: folder.join(path),
                name: path.to_owned(),
                size: None,
                directory: true,
                modified_unix_seconds: None,
            });
            parent = path.parent();
        }
    }
    Ok(files.into_values().collect())
}

pub(super) fn handle_file_drop(
    ui: &AppWindow,
    state: &Rc<AppState>,
    engine: &Engine,
    paths: Vec<PathBuf>,
) {
    if ui.get_busy()
        || ui.get_error_visible()
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

pub(super) fn parse_dropped_path(text: &str) -> Option<PathBuf> {
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

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::archive::{
        ArchiveEntry, ArchiveEntryKind, ArchiveListing, ArchiveResult, OperationSummary,
        ProgressSink,
    };
    use std::{
        cell::Cell,
        sync::atomic::{AtomicBool, Ordering},
        time::Instant,
    };

    struct SlowExtraction {
        ui_thread: std::thread::ThreadId,
        staging: Arc<Mutex<Option<PathBuf>>>,
        ready: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
        fail: bool,
    }

    impl ArchiveEngine for SlowExtraction {
        fn version(&self) -> String {
            "test".into()
        }
        fn writable_formats(&self) -> Vec<CreateFormat> {
            vec![]
        }
        fn list(
            &self,
            _: &Path,
            _: Option<&str>,
            _: u32,
            _: &dyn ProgressSink,
            _: &CancellationToken,
        ) -> ArchiveResult<ArchiveListing> {
            unreachable!()
        }
        fn create(
            &self,
            _: &Path,
            _: &[PathBuf],
            _: &CreateOptions,
            _: &dyn ProgressSink,
            _: &CancellationToken,
        ) -> ArchiveResult<OperationSummary> {
            unreachable!()
        }
        fn test(
            &self,
            _: &Path,
            _: Option<&str>,
            _: &dyn ProgressSink,
            _: &CancellationToken,
        ) -> ArchiveResult<OperationSummary> {
            unreachable!()
        }
        fn extract(
            &self,
            _: &Path,
            destination: &Path,
            options: &ExtractOptions,
            progress: &dyn ProgressSink,
            _: &dyn ConflictResolver,
            cancel: &CancellationToken,
        ) -> ArchiveResult<OperationSummary> {
            assert_ne!(std::thread::current().id(), self.ui_thread);
            *self.staging.lock().unwrap() = Some(destination.to_owned());
            let ExtractSelection::Paths(paths) = &options.selection else {
                panic!()
            };
            assert_eq!(paths.len(), 512);
            for path in paths {
                fs::write(destination.join(path), b"payload").unwrap();
            }
            let mut snapshot = ProgressSnapshot::new(ProgressPhase::Extracting);
            snapshot.current_file = "selected-511.txt".into();
            snapshot.entries_processed = 512;
            snapshot.total_entries = Some(512);
            progress.report(snapshot);
            self.ready.store(true, Ordering::Release);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !self.release.load(Ordering::Acquire) && !cancel.is_cancelled() {
                assert!(Instant::now() < deadline, "UI did not release the worker");
                std::thread::sleep(Duration::from_millis(5));
            }
            if cancel.is_cancelled() {
                return Err(ArchiveError::Cancelled);
            }
            if self.fail {
                return Err(ArchiveError::Worker("simulated extraction failure".into()));
            }
            Ok(OperationSummary {
                warning: Some("test warning".into()),
                ..Default::default()
            })
        }
    }

    fn run_slow_drag(cancel: bool, fail: bool, cancel_before_drop: bool, cancel_metadata: bool) {
        let ui = AppWindow::new().unwrap();
        let state = Rc::new(AppState::new());
        let mut listing = ArchiveListing {
            archive_path: PathBuf::from("test.zip"),
            format_name: "ZIP".into(),
            filter_name: None,
            warning: None,
            entries: vec![],
            total_uncompressed_size: 0,
        };
        for index in 0..512 {
            let path = PathBuf::from(format!("selected-{index}.txt"));
            state.selected.borrow_mut().insert(path.clone());
            listing.entries.push(ArchiveEntry {
                index,
                path: path.clone(),
                display_path: path.to_string_lossy().into_owned(),
                size: Some(7),
                compressed_size: None,
                modified_unix_seconds: None,
                kind: ArchiveEntryKind::File,
                encrypted: false,
            });
            state
                .display
                .borrow_mut()
                .push(crate::app::state::DisplayEntry {
                    source_index: None,
                    name: path.to_string_lossy().into_owned(),
                    relative_path: path,
                    is_folder: false,
                });
        }
        *state.listing.borrow_mut() = Some(Arc::new(listing));
        ui.set_has_archive(true);
        ui.show().unwrap();
        let staging = Arc::new(Mutex::new(None::<PathBuf>));
        let ready = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let ui_thread = std::thread::current().id();
        let engine: Engine = Arc::new(SlowExtraction {
            ui_thread,
            staging: Arc::clone(&staging),
            ready: Arc::clone(&ready),
            release: Arc::clone(&release),
            fail,
        });
        let drag_called = Arc::new(AtomicBool::new(false));
        let called = Arc::clone(&drag_called);
        let released = Arc::new(AtomicBool::new(false));
        let release_for_drag = Arc::clone(&released);
        let staging_for_drag = Arc::clone(&staging);
        let progress_window = Rc::new(RefCell::new(None));
        start_archive_drag_with(
            &ui,
            &state,
            &engine,
            0,
            &progress_window,
            move || !cancel_metadata,
            move |files, _descriptors, _origin, prepare, _cancel, completion| {
                assert_eq!(
                    std::thread::current().id(),
                    ui_thread,
                    "OLE must stay on the UI STA"
                );
                assert_eq!(files.len(), 512);
                assert!(
                    staging_for_drag.lock().unwrap().is_none(),
                    "extracted before drag confirmation"
                );
                called.store(true, Ordering::Release);
                std::thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !release_for_drag.load(Ordering::Acquire) {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    if cancel_before_drop {
                        drop(prepare);
                        completion.send(Ok(false)).unwrap();
                        return;
                    }
                    let directory = match prepare() {
                        Ok(directory) => directory,
                        Err(error) => {
                            completion.send(Err(error)).unwrap();
                            return;
                        }
                    };
                    let paths: Vec<_> = files
                        .iter()
                        .map(|file| directory.path().join(&file.archive_path))
                        .collect();
                    assert!(paths.iter().all(|path| path.is_file()));
                    std::thread::sleep(Duration::from_millis(80));
                    assert!(paths.iter().all(|path| path.is_file()));
                    drop(directory);
                    completion.send(Ok(true)).unwrap();
                });
                Ok(())
            },
        );
        assert!(ui.get_busy());
        let ticks = Rc::new(Cell::new(0));
        let tick_count = Rc::clone(&ticks);
        let weak = ui.as_weak();
        let state_for_timer = Rc::clone(&state);
        let staging_for_timer = Arc::clone(&staging);
        let drag_for_timer = Arc::clone(&drag_called);
        let hover_ticks = Rc::new(Cell::new(0));
        let hovering = Rc::clone(&hover_ticks);
        let timer = slint::Timer::default();
        let started = Instant::now();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(10),
            move || {
                assert!(
                    started.elapsed() < Duration::from_secs(15),
                    "drag UI stalled"
                );
                let ui = weak.upgrade().unwrap();
                if !ui.get_busy() {
                    assert!(state_for_timer.cancellation.lock().unwrap().is_none());
                    slint::quit_event_loop().unwrap();
                    return;
                }
                if drag_for_timer.load(Ordering::Acquire) && !released.load(Ordering::Acquire) {
                    hovering.set(hovering.get() + 1);
                    assert!(
                        staging_for_timer.lock().unwrap().is_none(),
                        "hover must not extract files"
                    );
                    assert!(
                        !progress_window
                            .borrow()
                            .as_ref()
                            .unwrap()
                            .window()
                            .is_visible(),
                        "hover must not show extraction progress"
                    );
                    if hovering.get() == 5 {
                        released.store(true, Ordering::Release);
                    }
                }
                if ready.load(Ordering::Acquire) {
                    tick_count.set(tick_count.get() + 1);
                    if tick_count.get() == 5 {
                        let progress = progress_window.borrow();
                        let progress = progress.as_ref().unwrap();
                        assert_eq!(progress.get_progress_file(), "selected-511.txt");
                        if cancel {
                            progress.invoke_cancel_requested();
                        } else {
                            release.store(true, Ordering::Release);
                        }
                    }
                }
            },
        );
        slint::run_event_loop().unwrap();
        timer.stop();
        assert!(cancel_metadata || hover_ticks.get() >= 5);
        assert!(
            cancel_before_drop || cancel_metadata || ticks.get() >= 5,
            "UI must keep processing events during extraction"
        );
        if cancel_before_drop || cancel_metadata {
            assert!(staging.lock().unwrap().is_none());
        } else {
            assert!(
                !staging.lock().unwrap().as_ref().unwrap().exists(),
                "staging leaked"
            );
        }
        assert_eq!(drag_called.load(Ordering::Acquire), !cancel_metadata);
        assert_eq!(ui.get_error_visible(), fail);
        if cancel || cancel_before_drop || cancel_metadata {
            assert_eq!(ui.get_status_text(), "Drag cancelled");
        }
        if !cancel && !fail && !cancel_before_drop && !cancel_metadata {
            assert!(ui.get_status_text().contains("test warning"));
        }
        ui.hide().unwrap();
    }

    #[test]
    fn selected_drag_stays_responsive_and_cleans_up_success_cancel_and_failure() {
        // Slint's event-loop proxy can be installed only once per process.
        // Isolate this harness from the other UI tests' backend instances.
        const CHILD: &str = "ARCHIVERCLICK_DRAG_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            for scenario in [
                "success",
                "cancel",
                "failure",
                "cancel_before_drop",
                "cancel_metadata",
            ] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "app::commands::drag_drop::tests::selected_drag_stays_responsive_and_cleans_up_success_cancel_and_failure", "--nocapture"])
                .env(CHILD, scenario)
                .output().unwrap();
                assert!(
                    output.status.success(),
                    "{scenario}: {}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }
        slint::platform::set_platform(Box::new(i_slint_backend_testing::TestingBackend::new(
            i_slint_backend_testing::TestingBackendOptions {
                threading: true,
                renderer_name: Some("software".into()),
                ..Default::default()
            },
        )))
        .unwrap();
        let scenario = std::env::var(CHILD).unwrap();
        run_slow_drag(
            scenario == "cancel",
            scenario == "failure",
            scenario == "cancel_before_drop",
            scenario == "cancel_metadata",
        );
    }

    #[test]
    fn folder_drag_metadata_keeps_descendants_and_current_folder_without_unselected_files() {
        let entry = |path: &str, kind| ArchiveEntry {
            index: 0,
            path: path.into(),
            display_path: path.into(),
            size: Some(7),
            compressed_size: None,
            modified_unix_seconds: None,
            kind,
            encrypted: false,
        };
        let listing = ArchiveListing {
            archive_path: "test.zip".into(),
            format_name: "ZIP".into(),
            filter_name: None,
            warning: None,
            total_uncompressed_size: 21,
            entries: vec![
                entry("parent/chosen/sub/data.txt", ArchiveEntryKind::File),
                entry("parent/chosen/empty", ArchiveEntryKind::Directory),
                entry("parent/other.txt", ArchiveEntryKind::File),
            ],
        };
        let files = drag_files(
            &listing,
            &ExtractSelection::Paths(vec!["parent/chosen".into()]),
            Path::new("parent"),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(
            files
                .iter()
                .map(|file| file.name.clone())
                .collect::<Vec<_>>(),
            vec![
                PathBuf::from("chosen"),
                PathBuf::from("chosen/empty"),
                PathBuf::from("chosen/sub"),
                PathBuf::from("chosen/sub/data.txt")
            ]
        );
        assert!(files[..3].iter().all(|file| file.directory));
        assert!(!files[3].directory);
        assert_eq!(
            files[3].archive_path,
            PathBuf::from("parent/chosen/sub/data.txt")
        );
    }
}
