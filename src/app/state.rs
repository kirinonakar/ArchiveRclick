use std::{
    cell::{Cell, RefCell},
    cmp::Ordering,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
};

use slint::{Model, ModelNotify, ModelTracker};

use crate::{
    ArchiveRow,
    archive::{ArchiveEntryKind, ArchiveListing, ConflictChoice},
    tasks::CancellationToken,
};

#[derive(Debug, Clone)]
pub(crate) struct DisplayEntry {
    // Inferred folders have no archive entry of their own, so they must not use
    // a descendant's metadata merely to obtain a model row.
    pub(crate) source_index: Option<usize>,
    pub(crate) name: String,
    pub(crate) relative_path: PathBuf,
    pub(crate) is_folder: bool,
}

// Column indices come from the UI. This sentinel keeps the first view in stable
// archive order and avoids sorting a large flat archive during model install.
const ARCHIVE_ORDER: usize = usize::MAX;

pub(crate) struct ArchiveRowModel {
    listing: Rc<RefCell<Option<ArchiveListing>>>,
    display: Rc<RefCell<Vec<DisplayEntry>>>,
    selected: Rc<RefCell<HashSet<PathBuf>>>,
    selection_anchor: Rc<RefCell<Option<PathBuf>>>,
    current_folder: Rc<RefCell<PathBuf>>,
    sort_column: Rc<Cell<usize>>,
    sort_ascending: Rc<Cell<bool>>,
    notify: ModelNotify,
}

impl ArchiveRowModel {
    pub(crate) fn new(
        listing: Rc<RefCell<Option<ArchiveListing>>>,
        display: Rc<RefCell<Vec<DisplayEntry>>>,
        selected: Rc<RefCell<HashSet<PathBuf>>>,
        selection_anchor: Rc<RefCell<Option<PathBuf>>>,
        current_folder: Rc<RefCell<PathBuf>>,
        sort_column: Rc<Cell<usize>>,
        sort_ascending: Rc<Cell<bool>>,
    ) -> Self {
        Self {
            listing,
            display,
            selected,
            selection_anchor,
            current_folder,
            sort_column,
            sort_ascending,
            notify: ModelNotify::default(),
        }
    }

    /// Toggles one row, or selects the contiguous range from the previous
    /// anchor when Shift is held. The anchor is a path instead of a row
    /// number so sorting and folder navigation cannot make it point at a
    /// different entry.
    pub(crate) fn select(&self, row: usize, extend: bool) {
        let display = self.display.borrow();
        let Some(entry) = display.get(row) else {
            return;
        };
        let path = entry.relative_path.clone();
        let mut selected = self.selected.borrow_mut();
        let anchor = extend
            .then(|| self.selection_anchor.borrow().clone())
            .flatten()
            .and_then(|anchor| {
                display
                    .iter()
                    .position(|candidate| candidate.relative_path == anchor)
            });

        let range_selected = if let Some(anchor) = anchor {
            selected.clear();
            let (start, end) = if anchor <= row {
                (anchor, row)
            } else {
                (row, anchor)
            };
            selected.extend(
                display[start..=end]
                    .iter()
                    .map(|candidate| candidate.relative_path.clone()),
            );
            true
        } else if !selected.insert(path.clone()) {
            selected.remove(&path);
            false
        } else {
            false
        };
        drop(selected);
        *self.selection_anchor.borrow_mut() = Some(path);
        // Keep ordinary clicks as a row-level update so the list item remains
        // stable long enough for TouchArea to recognize a second click.
        if range_selected {
            self.notify.reset();
        } else {
            self.notify.row_changed(row);
        }
    }

    /// Selects the row targeted by a context menu.  A right-click on an
    /// unselected row replaces the current selection; right-clicking a row
    /// that is already selected keeps an existing multi-selection intact.
    pub(crate) fn select_for_context(&self, row: usize) -> bool {
        let display = self.display.borrow();
        let Some(entry) = display.get(row) else {
            return false;
        };
        let path = entry.relative_path.clone();
        let mut selected = self.selected.borrow_mut();
        if !selected.contains(&path) {
            selected.clear();
            selected.insert(path.clone());
        }
        drop(selected);
        *self.selection_anchor.borrow_mut() = Some(path);
        self.notify.reset();
        true
    }

    pub(crate) fn clear_selection(&self) {
        if self.selected.borrow().is_empty() {
            self.selection_anchor.borrow_mut().take();
            return;
        }
        self.selected.borrow_mut().clear();
        self.selection_anchor.borrow_mut().take();
        self.notify.reset();
    }

    pub(crate) fn select_all_visible(&self) {
        let display = self.display.borrow();
        let mut selected = self.selected.borrow_mut();
        selected.extend(display.iter().map(|entry| entry.relative_path.clone()));
        drop(selected);
        self.notify.reset();
    }

    pub(crate) fn prepare_listing(listing: ArchiveListing) -> (ArchiveListing, Vec<DisplayEntry>) {
        let display = build_display_entries(&listing, Path::new(""), ARCHIVE_ORDER, true);
        (listing, display)
    }

    pub(crate) fn set_prepared_listing(&self, listing: ArchiveListing, display: Vec<DisplayEntry>) {
        *self.listing.borrow_mut() = Some(listing);
        *self.display.borrow_mut() = display;
        self.current_folder.borrow_mut().clear();
        self.selected.borrow_mut().clear();
        self.selection_anchor.borrow_mut().take();
        self.notify.reset();
    }

    pub(crate) fn clear_listing(&self) {
        *self.listing.borrow_mut() = None;
        self.display.borrow_mut().clear();
        self.current_folder.borrow_mut().clear();
        self.selected.borrow_mut().clear();
        self.selection_anchor.borrow_mut().take();
        self.notify.reset();
    }

    pub(crate) fn rebuild_display(&self) {
        rebuild_display(
            &self.listing,
            &self.display,
            &self.current_folder,
            self.sort_column.get(),
            self.sort_ascending.get(),
        );
        self.notify.reset();
    }
}

impl Model for ArchiveRowModel {
    type Data = ArchiveRow;

    fn row_count(&self) -> usize {
        self.display.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<Self::Data> {
        let display = self.display.borrow();
        let display_entry = display.get(row)?;
        let listing = self.listing.borrow();
        let entry = display_entry
            .source_index
            .and_then(|index| listing.as_ref()?.entries.get(index));
        let (size, packed, modified, path) = if let Some(entry) = entry {
            (
                format_size(entry.size),
                format_size(entry.compressed_size),
                format_timestamp(entry.modified_unix_seconds),
                entry.display_path.clone(),
            )
        } else {
            (
                String::new(),
                String::new(),
                String::new(),
                display_entry
                    .relative_path
                    .to_string_lossy()
                    .replace('\\', "/"),
            )
        };
        Some(ArchiveRow {
            selected: self
                .selected
                .borrow()
                .contains(&display_entry.relative_path),
            is_folder: display_entry.is_folder,
            name: display_entry.name.clone().into(),
            size: size.into(),
            packed: packed.into(),
            modified: modified.into(),
            path: path.into(),
        })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

pub(crate) struct AppState {
    pub(crate) listing: Rc<RefCell<Option<ArchiveListing>>>,
    pub(crate) display: Rc<RefCell<Vec<DisplayEntry>>>,
    pub(crate) selected: Rc<RefCell<HashSet<PathBuf>>>,
    pub(crate) current_folder: Rc<RefCell<PathBuf>>,
    pub(crate) sort_column: Rc<Cell<usize>>,
    pub(crate) sort_ascending: Rc<Cell<bool>>,
    pub(crate) rows: Rc<ArchiveRowModel>,
    pub(crate) cancellation: Arc<Mutex<Option<CancellationToken>>>,
    pub(crate) pending_conflict: Arc<Mutex<Option<std::sync::mpsc::SyncSender<ConflictChoice>>>>,
    pub(crate) pending_create_sources: Rc<RefCell<Vec<PathBuf>>>,
    pub(crate) open_password: Arc<Mutex<Option<String>>>,
    pub(crate) pending_password_path: Arc<Mutex<Option<PathBuf>>>,
    pub(crate) pending_test_password_path: Arc<Mutex<Option<PathBuf>>>,
}

impl AppState {
    pub(crate) fn new() -> Self {
        let listing = Rc::new(RefCell::new(None));
        let display = Rc::new(RefCell::new(Vec::new()));
        let selected = Rc::new(RefCell::new(HashSet::new()));
        let selection_anchor = Rc::new(RefCell::new(None));
        let current_folder = Rc::new(RefCell::new(PathBuf::new()));
        let sort_column = Rc::new(Cell::new(ARCHIVE_ORDER));
        let sort_ascending = Rc::new(Cell::new(true));
        let rows = Rc::new(ArchiveRowModel::new(
            Rc::clone(&listing),
            Rc::clone(&display),
            Rc::clone(&selected),
            Rc::clone(&selection_anchor),
            Rc::clone(&current_folder),
            Rc::clone(&sort_column),
            Rc::clone(&sort_ascending),
        ));
        Self {
            listing,
            display,
            selected,
            current_folder,
            sort_column,
            sort_ascending,
            rows,
            cancellation: Arc::new(Mutex::new(None)),
            pending_conflict: Arc::new(Mutex::new(None)),
            pending_create_sources: Rc::new(RefCell::new(Vec::new())),
            open_password: Arc::new(Mutex::new(None)),
            pending_password_path: Arc::new(Mutex::new(None)),
            pending_test_password_path: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn rebuild_display(&self) {
        self.rows.rebuild_display();
    }

    pub(crate) fn clear_archive(&self) {
        self.rows.clear_listing();
        self.sort_column.set(ARCHIVE_ORDER);
        self.sort_ascending.set(true);
    }

    pub(crate) fn activate_row(&self, row: usize) -> Option<PathBuf> {
        let display = self.display.borrow();
        let entry = display.get(row)?;
        entry.is_folder.then(|| entry.relative_path.clone())
    }

    pub(crate) fn selected_paths(&self) -> Vec<PathBuf> {
        self.selected.borrow().iter().cloned().collect()
    }
}

fn rebuild_display(
    listing: &RefCell<Option<ArchiveListing>>,
    display: &RefCell<Vec<DisplayEntry>>,
    current_folder: &RefCell<PathBuf>,
    column: usize,
    ascending: bool,
) {
    let current = current_folder.borrow().clone();
    let listing_ref = listing.borrow();
    *display.borrow_mut() = listing_ref
        .as_ref()
        .map(|listing| build_display_entries(listing, &current, column, ascending))
        .unwrap_or_default();
}

fn build_display_entries(
    listing: &ArchiveListing,
    current: &Path,
    column: usize,
    ascending: bool,
) -> Vec<DisplayEntry> {
    let mut folders = Vec::<DisplayEntry>::new();
    let mut files = Vec::<DisplayEntry>::new();
    let mut folder_indices = HashMap::<String, usize>::new();

    for (source_index, entry) in listing.entries.iter().enumerate() {
        let relative = normalize_archive_path(&entry.path);
        let Ok(rest) = relative.strip_prefix(current) else {
            continue;
        };
        let mut components = rest.components();
        let Some(first) = components.next() else {
            continue;
        };
        let name = first.as_os_str().to_string_lossy().into_owned();
        let has_descendants = components.next().is_some();
        let is_folder = has_descendants || entry.kind == ArchiveEntryKind::Directory;
        let relative_path = if current.as_os_str().is_empty() {
            PathBuf::from(&name)
        } else {
            current.join(&name)
        };
        if is_folder {
            let key = name.to_ascii_lowercase();
            if let Some(&folder_index) = folder_indices.get(&key) {
                if !has_descendants
                    && folders[folder_index].source_index.is_none()
                    && folders[folder_index].name == name
                {
                    folders[folder_index].source_index = Some(source_index);
                }
                continue;
            }
            folder_indices.insert(key, folders.len());
            folders.push(DisplayEntry {
                source_index: (!has_descendants).then_some(source_index),
                name,
                relative_path,
                is_folder: true,
            });
        } else {
            files.push(DisplayEntry {
                source_index: Some(source_index),
                name,
                relative_path,
                is_folder: false,
            });
        }
    }

    if column != ARCHIVE_ORDER {
        let compare = |left: &DisplayEntry, right: &DisplayEntry| {
            let ordering = compare_entries(listing, left, right, column);
            if ascending {
                ordering
            } else {
                ordering.reverse()
            }
        };
        folders.sort_by(compare);
        files.sort_by(compare);
    }

    if folders.is_empty() {
        files
    } else {
        folders.extend(files);
        folders
    }
}

fn compare_entries(
    listing: &ArchiveListing,
    left: &DisplayEntry,
    right: &DisplayEntry,
    column: usize,
) -> Ordering {
    let left_entry = left
        .source_index
        .and_then(|index| listing.entries.get(index));
    let right_entry = right
        .source_index
        .and_then(|index| listing.entries.get(index));
    match column {
        1 => left_entry
            .and_then(|entry| entry.size)
            .cmp(&right_entry.and_then(|entry| entry.size)),
        2 => left_entry
            .and_then(|entry| entry.compressed_size)
            .cmp(&right_entry.and_then(|entry| entry.compressed_size)),
        3 => left_entry
            .and_then(|entry| entry.modified_unix_seconds)
            .cmp(&right_entry.and_then(|entry| entry.modified_unix_seconds)),
        4 => compare_ascii_case_insensitive(
            display_path(listing, left).as_ref(),
            display_path(listing, right).as_ref(),
        ),
        _ => compare_ascii_case_insensitive(&left.name, &right.name),
    }
    .then_with(|| left.name.cmp(&right.name))
}

fn display_path<'a>(
    listing: &'a ArchiveListing,
    display: &'a DisplayEntry,
) -> std::borrow::Cow<'a, str> {
    display
        .source_index
        .and_then(|index| listing.entries.get(index))
        .map(|entry| std::borrow::Cow::Borrowed(entry.display_path.as_str()))
        .unwrap_or_else(|| display.relative_path.to_string_lossy())
}

fn compare_ascii_case_insensitive(left: &str, right: &str) -> Ordering {
    left.bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .cmp(right.bytes().map(|byte| byte.to_ascii_lowercase()))
}

fn normalize_archive_path(path: &Path) -> PathBuf {
    path.to_string_lossy()
        .replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect()
}

fn format_size(value: Option<u64>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else if size >= 100.0 {
        format!("{size:.0} {}", UNITS[unit])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn format_timestamp(value: Option<i64>) -> String {
    let Some(seconds) = value else {
        return String::new();
    };
    // Civil date conversion adapted from Howard Hinnant's public-domain algorithm.
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    let hour = day_seconds / 3_600;
    let minute = day_seconds % 3_600 / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use slint::Model;

    use crate::archive::{ArchiveEntry, ArchiveEntryKind, ArchiveListing};

    use super::AppState;

    fn install_listing(state: &AppState, listing: ArchiveListing) {
        let (listing, display) = super::ArchiveRowModel::prepare_listing(listing);
        state.rows.set_prepared_listing(listing, display);
    }

    fn entry(
        path: &str,
        kind: ArchiveEntryKind,
        size: Option<u64>,
        modified_unix_seconds: Option<i64>,
    ) -> ArchiveEntry {
        ArchiveEntry {
            index: 0,
            path: PathBuf::from(path),
            display_path: path.to_owned(),
            size,
            compressed_size: size.map(|value| value / 2),
            modified_unix_seconds,
            kind,
            encrypted: false,
        }
    }

    fn listing(entries: Vec<ArchiveEntry>) -> ArchiveListing {
        ArchiveListing {
            archive_path: PathBuf::from("test.zip"),
            format_name: "ZIP".to_owned(),
            filter_name: None,
            entries,
            total_uncompressed_size: 0,
        }
    }

    fn row_names(state: &AppState) -> Vec<String> {
        (0..state.rows.row_count())
            .map(|row| state.rows.row_data(row).unwrap().name.to_string())
            .collect()
    }

    #[test]
    fn default_view_preserves_archive_order_with_folders_first() {
        let state = AppState::new();
        install_listing(
            &state,
            listing(vec![
                entry("zeta.txt", ArchiveEntryKind::File, Some(1), None),
                entry("folder/second.txt", ArchiveEntryKind::File, Some(2), None),
                entry("Alpha.txt", ArchiveEntryKind::File, Some(3), None),
                entry("folder/first.txt", ArchiveEntryKind::File, Some(4), None),
                entry("beta.txt", ArchiveEntryKind::File, Some(5), None),
            ]),
        );

        assert_eq!(
            row_names(&state),
            ["folder", "zeta.txt", "Alpha.txt", "beta.txt"]
        );
    }

    #[test]
    fn activating_a_folder_rebuilds_the_view_inside_that_folder() {
        let state = AppState::new();
        install_listing(
            &state,
            listing(vec![
                entry("folder/child.txt", ArchiveEntryKind::File, Some(1), None),
                entry("top.txt", ArchiveEntryKind::File, Some(2), None),
                entry(
                    "folder/nested/grand.txt",
                    ArchiveEntryKind::File,
                    Some(3),
                    None,
                ),
            ]),
        );

        let folder = state.activate_row(0).expect("first row is a folder");
        *state.current_folder.borrow_mut() = folder;
        state.rows.clear_selection();
        state.rebuild_display();

        assert_eq!(row_names(&state), ["nested", "child.txt"]);
    }

    #[test]
    fn shift_selection_selects_the_range_from_the_previous_anchor() {
        let state = AppState::new();
        install_listing(
            &state,
            listing(vec![
                entry("one.txt", ArchiveEntryKind::File, Some(1), None),
                entry("two.txt", ArchiveEntryKind::File, Some(2), None),
                entry("three.txt", ArchiveEntryKind::File, Some(3), None),
                entry("four.txt", ArchiveEntryKind::File, Some(4), None),
            ]),
        );

        state.rows.select(1, false);
        state.rows.select(3, true);

        let mut selected = state.selected_paths();
        selected.sort();
        assert_eq!(
            selected,
            [
                PathBuf::from("four.txt"),
                PathBuf::from("three.txt"),
                PathBuf::from("two.txt"),
            ]
        );
    }

    #[test]
    fn explicit_name_sort_is_case_insensitive_and_keeps_folders_first() {
        let state = AppState::new();
        install_listing(
            &state,
            listing(vec![
                entry("zeta.txt", ArchiveEntryKind::File, Some(1), None),
                entry("folder/child.txt", ArchiveEntryKind::File, Some(2), None),
                entry("Alpha.txt", ArchiveEntryKind::File, Some(3), None),
                entry("beta.txt", ArchiveEntryKind::File, Some(4), None),
            ]),
        );

        state.sort_column.set(0);
        state.rebuild_display();

        assert_eq!(
            row_names(&state),
            ["folder", "Alpha.txt", "beta.txt", "zeta.txt"]
        );
    }

    #[test]
    fn synthetic_folder_has_its_own_path_and_no_child_metadata() {
        let state = AppState::new();
        install_listing(
            &state,
            listing(vec![entry(
                "folder/child.bin",
                ArchiveEntryKind::File,
                Some(8 * 1024),
                Some(86_400),
            )]),
        );

        let row = state.rows.row_data(0).unwrap();
        assert!(row.is_folder);
        assert_eq!(row.name.as_str(), "folder");
        assert_eq!(row.size.as_str(), "");
        assert_eq!(row.packed.as_str(), "");
        assert_eq!(row.modified.as_str(), "");
        assert_eq!(row.path.as_str(), "folder");
    }

    #[test]
    fn explicit_directory_metadata_replaces_synthetic_metadata() {
        let state = AppState::new();
        install_listing(
            &state,
            listing(vec![
                entry(
                    "folder/child.bin",
                    ArchiveEntryKind::File,
                    Some(8 * 1024),
                    Some(86_400),
                ),
                entry("folder", ArchiveEntryKind::Directory, Some(0), Some(0)),
            ]),
        );

        let row = state.rows.row_data(0).unwrap();
        assert!(row.is_folder);
        assert_eq!(row.size.as_str(), "0 B");
        assert_eq!(row.packed.as_str(), "0 B");
        assert_eq!(row.modified.as_str(), "1970-01-01 00:00");
        assert_eq!(row.path.as_str(), "folder");
    }
}
