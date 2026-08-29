use super::super::*;

pub(super) fn wire(ui: &AppWindow, state: Rc<AppState>, engine: Engine) {
    {
        let weak = ui.as_weak();
        ui.on_main_window_close_requested(move || {
            if let Some(ui) = weak.upgrade() {
                prepare_main_window_close(&ui);
                let _ = ui.hide();
            }
        });
    }

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
            if let Err(error) = platform::save_column_boundaries(&boundaries)
                && let Some(ui) = weak.upgrade()
            {
                ui.set_status_text(format!("Could not save column widths: {error}").into());
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
}
