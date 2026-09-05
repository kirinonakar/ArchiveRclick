use super::super::*;

pub(super) fn wire(
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
                  volume_custom,
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
                let default_folder = sources[0].parent().unwrap_or(&sources[0]);
                let destination =
                    match platform::save_archive(&default_name, format.default_extension(), default_folder) {
                        Ok(Some(path)) => path,
                        Ok(None) => return,
                        Err(error) => {
                            show_ui_error(&weak, "Create archive", error);
                            return;
                        }
                    };
                let supports_split = matches!(format, CreateFormat::Zip | CreateFormat::SevenZip);
                let split_size = if !supports_split {
                    None
                } else if volume_index == VOLUME_CUSTOM_UI_INDEX {
                    match parse_volume_size(volume_custom.as_str()) {
                        Some(size) => Some(size),
                        None => {
                            let message = if volume_custom.trim().is_empty() {
                                "Enter a split size, for example 30MB or 1.5GB".to_owned()
                            } else {
                                format!(
                                    "Invalid split size \"{volume_custom}\". Use values like 30MB, 500KB, or 1.5GB"
                                )
                            };
                            show_ui_error(&weak, "Create archive", message);
                            return;
                        }
                    }
                } else {
                    VolumeSizePreset::from_ui_index(volume_index).bytes()
                };
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
}
