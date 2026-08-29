//! Main-window construction, lifetime, and geometry persistence.

use super::*;

/// Dependencies retained for the lifetime of the interactive main window.
/// Construction details and writable-format setup stay private to this module.
pub(super) struct MainWindowSession {
    pub(super) ui: AppWindow,
    pub(super) state: Rc<AppState>,
    pub(super) engine: Engine,
    pub(super) initial_show_error: InitialWindowShowError,
}

/// Builds the main application window and wires it up.
pub(super) fn open_main_window(
    operation_progress_window: Rc<RefCell<Option<ProgressWindow>>>,
) -> Result<MainWindowSession, String> {
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
    ui.set_esc_close_main_window(platform::load_esc_close_main_window_preference());
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
    ui.set_language_preference_selection(language_preference_selection_index(&language_preference));
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
        if let Some(ui) = weak_for_close.upgrade() {
            prepare_main_window_close(&ui);
        } else {
            cleanup_drag_staging_directories();
        }
        slint::CloseRequestResponse::HideWindow
    });

    // AppWindow creation queues a hidden native window. Wait until the event
    // loop has created it, then apply the saved physical size and native theme
    // before mapping it. Showing first and resizing from this callback leaves
    // the default-size frame visible briefly on some launches.
    let initial_show_error = Arc::new(Mutex::new(None));
    let initial_show_error_for_callback = Arc::clone(&initial_show_error);
    let weak = ui.as_weak();
    let theme_selection = ui.get_theme_selection();
    slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if let Some(geometry) = saved_geometry {
            ui.window()
                .set_size(slint::PhysicalSize::new(geometry.width, geometry.height));
        }
        platform::apply_window_theme(ui.window(), theme_selection);
        if let Err(error) = ui.show() {
            let message = format!("Could not show the UI: {error}");
            match initial_show_error_for_callback.lock() {
                Ok(mut slot) => *slot = Some(message),
                Err(poisoned) => *poisoned.into_inner() = Some(message),
            }
            let _ = slint::quit_event_loop();
        }
    })
    .map_err(|error| format!("Could not schedule the initial window show: {error}"))?;
    Ok(MainWindowSession {
        ui,
        state,
        engine,
        initial_show_error,
    })
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

/// Performs the same cleanup and geometry persistence for both the native
/// title-bar close request and the Escape shortcut.
pub(super) fn prepare_main_window_close(ui: &AppWindow) {
    cleanup_drag_staging_directories();
    if should_save_window_geometry(ui.window()) {
        let size = ui.window().size();
        let position = ui.window().position();
        let _ = platform::save_window_geometry(&platform::WindowGeometry {
            x: position.x,
            y: position.y,
            width: size.width,
            height: size.height,
        });
    }
}

// ---------------------------------------------------------------------------
// Explorer context-menu operations. The shell extension launches the app with
// these verbs and the work runs in a small progress-only window: no main
// window appears, and the window closes by itself once the work is finished.
// ---------------------------------------------------------------------------
