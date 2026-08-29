//! Pure progress snapshot formatting plus Slint/taskbar presentation updates.

use super::super::*;

pub(in crate::app::commands) struct ProgressUiText {
    pub(in crate::app::commands) file_percent: String,
    pub(in crate::app::commands) percent: String,
    pub(in crate::app::commands) elapsed: String,
    pub(in crate::app::commands) remaining: String,
    pub(in crate::app::commands) total: String,
    pub(in crate::app::commands) detail: String,
    pub(in crate::app::commands) file_value: f32,
    pub(in crate::app::commands) value: f32,
}

fn initial_progress_text() -> ProgressUiText {
    let snapshot = ProgressSnapshot::new(crate::tasks::ProgressPhase::Opening);
    progress_ui_text(&snapshot)
}

pub(in crate::app::commands) fn set_initial_progress_window(ui: &ProgressWindow) {
    let text = initial_progress_text();
    ui.set_progress_file_percent(text.file_percent.into());
    ui.set_progress_percent(text.percent.into());
    ui.set_progress_elapsed(text.elapsed.into());
    ui.set_progress_remaining(text.remaining.into());
    ui.set_progress_total(text.total.into());
    ui.set_progress_detail(text.detail.into());
    ui.set_progress_file_value(text.file_value);
    ui.set_progress_value(text.value);
    platform::taskbar::show_indeterminate(ui.window());
}

pub(in crate::app::commands) fn progress_ui_text(snapshot: &ProgressSnapshot) -> ProgressUiText {
    let file_value = snapshot.current_file_fraction().unwrap_or(-1.0);
    let value = if snapshot.total_bytes.is_some() || snapshot.total_entries.is_some() {
        snapshot.fraction()
    } else {
        -1.0
    };
    let file_percent = if file_value < 0.0 {
        "—%".to_owned()
    } else {
        format!("{:.0}%", file_value * 100.0)
    };
    let percent = if value < 0.0 {
        "—%".to_owned()
    } else {
        format!("{:.0}%", value * 100.0)
    };
    let entry_detail = if let Some(total_entries) = snapshot.total_entries {
        format!(
            "Files {} / {}",
            snapshot.entries_processed.min(total_entries),
            total_entries
        )
    } else {
        format!("Files {}", snapshot.entries_processed)
    };
    ProgressUiText {
        file_percent,
        percent,
        elapsed: format!("Elapsed {}", format_duration(Some(snapshot.elapsed))),
        remaining: format!(
            "Remaining {}",
            format_duration(snapshot.estimated_remaining)
        ),
        total: format!("Total {}", format_duration(snapshot.estimated_total)),
        detail: format!(
            "{entry_detail}  •  {} processed",
            compact_bytes(snapshot.bytes_processed)
        ),
        file_value,
        value,
    }
}

fn format_duration(duration: Option<Duration>) -> String {
    let Some(duration) = duration else {
        return "—".to_owned();
    };
    let seconds = duration.as_secs();
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

pub(in crate::app::commands) fn compact_bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = value as f64;
    let mut unit = 0usize;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}
