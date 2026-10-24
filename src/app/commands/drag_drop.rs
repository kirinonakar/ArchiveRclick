//! Native and Slint drag/drop adaptation, including temporary extraction staging.

use super::*;

pub(super) const DRAG_STAGING_PREFIX: &str = "ArchiveRclick-drag-";

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
        platform::start_file_drag,
    );
}

fn start_archive_drag_with(
    ui: &AppWindow,
    state: &Rc<AppState>,
    engine: &Engine,
    row: usize,
    progress_window: &Rc<RefCell<Option<ProgressWindow>>>,
    drag: impl FnOnce(&[PathBuf], mpsc::SyncSender<Result<bool, String>>) -> Result<(), String>
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

    let Some(archive) = state
        .listing
        .borrow()
        .as_ref()
        .map(|listing| listing.archive_path.clone())
    else {
        return;
    };
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
        selection: ExtractSelection::Paths(selection.clone()),
        password,
        conflict_policy: InitialConflictPolicy::OverwriteAll,
        pathname_codepage: pathname_codepage(ui.get_encoding_selection()),
        ..ExtractOptions::default()
    };
    let title = progress_title_with_filename("Preparing selected items for Explorer", &archive);
    let (cancel, weak_progress) = match operations::begin_progress_window_operation(
        ui,
        state,
        progress_window,
        &title,
        &archive.display().to_string(),
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
        let mut selection = selection;
        selection.sort();
        // The worker owns staging until the target finishes the transfer.
        // Even a large recursive cleanup must never run on the UI thread.
        let result = (|| {
            let staging = create_drag_staging_directory()?;
            let updates = weak_progress.clone();
            let summary = engine
                .extract(
                    &archive,
                    staging.path(),
                    &options,
                    &move |snapshot| update_progress_window_details(&updates, snapshot),
                    &OverwriteAllResolver,
                    &cancel,
                )
                .map_err(|error| error.to_string())?;
            if cancel.is_cancelled() {
                return Ok((false, summary.warning));
            }
            let paths = staged_drag_paths(staging.path(), &selection)?;
            let (sender, receiver) = mpsc::sync_channel(1);
            let drag_cancel = cancel.clone();
            let drag_progress = weak_progress.clone();
            weak.clone()
                .upgrade_in_event_loop(move |_ui| {
                    if let Some(progress) = drag_progress.upgrade() {
                        platform::taskbar::clear(progress.window());
                        let _ = progress.hide();
                    }
                    // OLE stays on the window's STA thread. Its modal drag loop is
                    // entered only after the expensive archive work is finished.
                    if drag_cancel.is_cancelled() {
                        let _ = sender.try_send(Ok(false));
                    } else if let Err(error) = drag(&paths, sender.clone()) {
                        let _ = sender.try_send(Err(error));
                    }
                })
                .map_err(|error| error.to_string())?;
            let dragged = receiver.recv().map_err(|error| error.to_string())??;
            Ok::<_, String>((dragged, summary.warning))
            // `staging` is removed here on the worker, after Explorer has
            // finished consuming the synchronous CF_HDROP payload.
        })();
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
                Ok((false, _)) => ui.set_status_text("Drag cancelled".into()),
                Ok((true, warning)) => {
                    let status = warning.map_or_else(
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
    use crate::archive::{ArchiveListing, ArchiveResult, OperationSummary, ProgressSink};
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

    fn run_slow_drag(cancel: bool, fail: bool) {
        let ui = AppWindow::new().unwrap();
        let state = Rc::new(AppState::new());
        *state.listing.borrow_mut() = Some(ArchiveListing {
            archive_path: PathBuf::from("test.zip"),
            format_name: "ZIP".into(),
            filter_name: None,
            warning: None,
            entries: vec![],
            total_uncompressed_size: 0,
        });
        for index in 0..512 {
            let path = PathBuf::from(format!("selected-{index}.txt"));
            state.selected.borrow_mut().insert(path.clone());
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
        let progress_window = Rc::new(RefCell::new(None));
        start_archive_drag_with(
            &ui,
            &state,
            &engine,
            0,
            &progress_window,
            move |paths, completion| {
                assert_eq!(
                    std::thread::current().id(),
                    ui_thread,
                    "OLE must stay on the UI STA"
                );
                assert_eq!(paths.len(), 512);
                assert!(
                    paths.iter().all(|path| path.is_file()),
                    "payload removed before drop"
                );
                called.store(true, Ordering::Release);
                let paths = paths.to_vec();
                std::thread::spawn(move || {
                    // Delay the transfer acknowledgement. Staging must survive
                    // until it arrives, while the UI keeps processing events.
                    std::thread::sleep(Duration::from_millis(80));
                    assert!(paths.iter().all(|path| path.is_file()));
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
        assert!(
            ticks.get() >= 5,
            "UI must keep processing events during extraction"
        );
        assert!(
            !staging.lock().unwrap().as_ref().unwrap().exists(),
            "staging leaked"
        );
        assert_eq!(drag_called.load(Ordering::Acquire), !cancel && !fail);
        assert_eq!(ui.get_error_visible(), fail);
        if cancel {
            assert_eq!(ui.get_status_text(), "Drag cancelled");
        }
        if !cancel && !fail {
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
            for scenario in ["success", "cancel", "failure"] {
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
        run_slow_drag(scenario == "cancel", scenario == "failure");
    }
}
