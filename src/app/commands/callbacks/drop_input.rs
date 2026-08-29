use super::super::*;

pub(super) fn wire(ui: &AppWindow, state: Rc<AppState>, engine: Engine) {
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
