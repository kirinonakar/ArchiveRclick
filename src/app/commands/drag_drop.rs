//! Native and Slint drag/drop adaptation, including temporary extraction staging.

use super::*;

pub(super) const DRAG_STAGING_PREFIX: &str = "ArchiveRclick-drag-";

pub(super) fn start_archive_drag(
    ui: &AppWindow,
    state: &Rc<AppState>,
    engine: &Engine,
    row: usize,
) {
    if ui.get_busy()
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
    let mut selection = state.selected_paths();
    if selection.is_empty() {
        return;
    }
    selection.sort();

    let staging = match create_drag_staging_directory() {
        Ok(path) => path,
        Err(error) => {
            show_ui_error(&ui.as_weak(), "Extract selected items", error);
            return;
        }
    };

    ui.set_busy(true);
    ui.set_status_text("Preparing selected items for Explorer…".into());

    let password = state
        .open_password
        .lock()
        .expect("password mutex poisoned")
        .clone();
    let options = ExtractOptions {
        selection: ExtractSelection::Paths(selection.clone()),
        password,
        conflict_policy: InitialConflictPolicy::OverwriteAll,
        pathname_codepage: pathname_codepage(ui.get_encoding_selection()),
        ..ExtractOptions::default()
    };
    let cancel = CancellationToken::new();
    let progress = |_: ProgressSnapshot| {};
    let extraction = engine.extract(
        &archive,
        &staging,
        &options,
        &progress,
        &OverwriteAllResolver,
        &cancel,
    );

    let result = match extraction {
        Ok(_) => staged_drag_paths(&staging, &selection)
            .and_then(|paths| platform::start_file_drag(&paths).map(|()| paths)),
        Err(error) => Err(error.to_string()),
    };
    let _ = fs::remove_dir_all(&staging);
    ui.set_busy(false);

    match result {
        Ok(_) => ui.set_status_text("Selected items were sent to Explorer".into()),
        Err(error) => show_ui_error(&ui.as_weak(), "Drag selected items", error),
    }
}

fn create_drag_staging_directory() -> Result<PathBuf, String> {
    let temp = std::env::temp_dir();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("Could not create a drag staging name: {error}"))?
        .as_nanos();
    let process = std::process::id();
    for attempt in 0..100u32 {
        let path = temp.join(format!(
            "{DRAG_STAGING_PREFIX}{process}-{timestamp}-{attempt}"
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "Could not create the drag staging directory {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Err("Could not create a unique drag staging directory".to_owned())
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

/// Removes this process's temporary Explorer-drag folders. The explicit
/// process prefix prevents one running instance from deleting another
/// instance's active drag payload.
pub(super) fn cleanup_drag_staging_directories() {
    let process_prefix = format!("{DRAG_STAGING_PREFIX}{}-", std::process::id());
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_owned = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(&process_prefix));
        if is_owned && path.is_dir() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

pub(super) fn handle_file_drop(
    ui: &AppWindow,
    state: &Rc<AppState>,
    engine: &Engine,
    paths: Vec<PathBuf>,
) {
    if ui.get_busy()
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
