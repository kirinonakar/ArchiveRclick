use super::super::*;

pub(super) fn wire(ui: &AppWindow) {
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
                ui.set_esc_close_main_window(platform::load_esc_close_main_window_preference());
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
        ui.on_third_party_notices_requested(move || {
            let result = third_party_notices_path().and_then(|path| platform::open_file(&path));
            if let Some(ui) = weak.upgrade() {
                match result {
                    Ok(()) => ui.set_status_text("Third-party notices opened".into()),
                    Err(error) => {
                        ui.set_status_text(error.clone().into());
                        platform::show_error("Open third-party notices", &error);
                    }
                }
            }
        });
    }

    {
        let weak = ui.as_weak();
        ui.on_third_party_licenses_requested(move || {
            let result = third_party_runtime_licenses_path()
                .and_then(|path| platform::reveal_in_explorer(&path));
            if let Some(ui) = weak.upgrade() {
                match result {
                    Ok(()) => ui.set_status_text("Runtime licenses opened".into()),
                    Err(error) => {
                        ui.set_status_text(error.clone().into());
                        platform::show_error("Open runtime licenses", &error);
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
        ui.on_file_associations_register_requested(move || {
            let result = std::env::current_exe()
                .map_err(|error| format!("Could not locate ArchiveRclick: {error}"))
                .and_then(|executable| platform::register_file_associations(&executable));
            if let Some(ui) = weak.upgrade() {
                match result {
                    Ok(()) => ui.set_status_text(
                        "ArchiveRclick이 지원 확장자의 파일 연결 앱으로 등록되었습니다.".into(),
                    ),
                    Err(error) => {
                        ui.set_status_text(error.clone().into());
                        platform::show_error("Register file associations", &error);
                    }
                }
            }
        });
    }

    {
        let weak = ui.as_weak();
        ui.on_file_associations_unregister_requested(move || {
            let result = platform::unregister_file_associations();
            if let Some(ui) = weak.upgrade() {
                match result {
                    Ok(()) => {
                        ui.set_status_text("ArchiveRclick 파일 연결 등록이 제거되었습니다.".into());
                    }
                    Err(error) => {
                        ui.set_status_text(error.clone().into());
                        platform::show_error("Remove file associations", &error);
                    }
                }
            }
        });
    }

    {
        let weak = ui.as_weak();
        ui.on_default_apps_requested(move || {
            let result = platform::open_url("ms-settings:defaultapps");
            if let Some(ui) = weak.upgrade() {
                match result {
                    Ok(()) => ui.set_status_text("Windows 기본 앱 설정을 열었습니다.".into()),
                    Err(error) => {
                        ui.set_status_text(error.clone().into());
                        platform::show_error("Open Default apps", &error);
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
                  header_encryption,
                  esc_close_main_window| {
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
                    platform::save_esc_close_main_window_preference(esc_close_main_window)
                {
                    failure = Some(format!("Could not save settings: {error}"));
                } else if let Err(error) =
                    platform::save_theme_preference(theme_registry_key(theme_selection))
                {
                    failure = Some(format!("Could not save settings: {error}"));
                } else if let Err(error) = platform::save_language_preference(
                    language_registry_key(language_preference_selection),
                ) {
                    failure = Some(format!("Could not save settings: {error}"));
                }
                if let Some(message) = failure {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_status_text(message.into());
                    }
                    return;
                }
                platform::shell_ext::refresh_context_menu();
                let family = platform::resolve_font_family(preference);
                if let Some(ui) = weak.upgrade() {
                    ui.set_font_family(family.into());
                    ui.set_settings_header_encryption(header_encryption);
                    ui.set_esc_close_main_window(esc_close_main_window);
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
}
