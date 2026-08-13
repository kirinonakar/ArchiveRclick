use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use crate::archive::ProgressSink;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressPhase {
    Opening,
    Listing,
    Extracting,
    Compressing,
    Testing,
    Finished,
}

impl ProgressPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Opening => "Opening",
            Self::Listing => "Reading archive",
            Self::Extracting => "Extracting",
            Self::Compressing => "Creating archive",
            Self::Testing => "Testing archive",
            Self::Finished => "Finished",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProgressSnapshot {
    pub phase: ProgressPhase,
    pub current_file: String,
    pub current_file_bytes_processed: u64,
    pub current_file_total_bytes: Option<u64>,
    pub entries_processed: u64,
    pub total_entries: Option<u64>,
    pub bytes_processed: u64,
    pub total_bytes: Option<u64>,
    pub elapsed: Duration,
    pub estimated_remaining: Option<Duration>,
    pub estimated_total: Option<Duration>,
}

impl ProgressSnapshot {
    pub fn new(phase: ProgressPhase) -> Self {
        Self {
            phase,
            current_file: String::new(),
            current_file_bytes_processed: 0,
            current_file_total_bytes: None,
            entries_processed: 0,
            total_entries: None,
            bytes_processed: 0,
            total_bytes: None,
            elapsed: Duration::ZERO,
            estimated_remaining: None,
            estimated_total: None,
        }
    }

    pub fn fraction(&self) -> f32 {
        // A finished operation is complete even when it had no entries or
        // bytes.  Without this special case an empty archive would end at
        // 0%, which is misleading in the progress UI.
        if self.phase == ProgressPhase::Finished {
            1.0
        } else if let Some(total) = self.total_bytes.filter(|total| *total > 0) {
            (self.bytes_processed as f64 / total as f64).clamp(0.0, 1.0) as f32
        } else if let Some(total) = self.total_entries.filter(|total| *total > 0) {
            (self.entries_processed as f64 / total as f64).clamp(0.0, 1.0) as f32
        } else {
            0.0
        }
    }

    /// Returns the current file's progress when its size is known. A missing
    /// total means the caller should display an indeterminate bar.
    pub fn current_file_fraction(&self) -> Option<f32> {
        if self.phase == ProgressPhase::Finished {
            return Some(1.0);
        }
        let total = self.current_file_total_bytes?;
        if total == 0 {
            return Some(1.0);
        }
        Some((self.current_file_bytes_processed as f64 / total as f64).clamp(0.0, 1.0) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::{ProgressPhase, ProgressSnapshot};

    #[test]
    fn finished_empty_operation_is_one_hundred_percent() {
        let snapshot = ProgressSnapshot::new(ProgressPhase::Finished);

        assert_eq!(snapshot.fraction(), 1.0);
    }

    #[test]
    fn byte_progress_takes_precedence_over_entry_progress() {
        let mut snapshot = ProgressSnapshot::new(ProgressPhase::Compressing);
        snapshot.total_entries = Some(10);
        snapshot.entries_processed = 9;
        snapshot.total_bytes = Some(1_000);
        snapshot.bytes_processed = 250;

        assert!((snapshot.fraction() - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn current_file_progress_is_independent_of_overall_progress() {
        let mut snapshot = ProgressSnapshot::new(ProgressPhase::Compressing);
        snapshot.bytes_processed = 900;
        snapshot.total_bytes = Some(1_000);
        snapshot.current_file_bytes_processed = 25;
        snapshot.current_file_total_bytes = Some(100);

        assert!((snapshot.fraction() - 0.9).abs() < f32::EPSILON);
        assert!((snapshot.current_file_fraction().unwrap() - 0.25).abs() < f32::EPSILON);
    }
}

pub struct ThrottledProgress<'a> {
    inner: &'a dyn ProgressSink,
    interval: Duration,
    started_at: Instant,
    last_report: Mutex<Option<Instant>>,
}

impl<'a> ThrottledProgress<'a> {
    pub fn new(inner: &'a dyn ProgressSink, interval: Duration) -> Self {
        Self {
            inner,
            interval,
            started_at: Instant::now(),
            last_report: Mutex::new(None),
        }
    }

    pub fn report(&self, mut snapshot: ProgressSnapshot, force: bool) {
        let mut last = self.last_report.lock().expect("progress mutex poisoned");
        let now = Instant::now();
        snapshot.elapsed = now.duration_since(self.started_at);
        let fraction = snapshot.fraction();
        if snapshot.phase == ProgressPhase::Finished || fraction >= 1.0 {
            snapshot.estimated_remaining = Some(Duration::ZERO);
            snapshot.estimated_total = Some(snapshot.elapsed);
        } else if fraction > 0.0 {
            let elapsed_seconds = snapshot.elapsed.as_secs_f64();
            let total_seconds = elapsed_seconds / f64::from(fraction);
            let total =
                Duration::from_secs_f64(total_seconds.max(0.0).min(Duration::MAX.as_secs_f64()));
            snapshot.estimated_total = Some(total);
            snapshot.estimated_remaining = total.checked_sub(snapshot.elapsed);
        }
        if force || last.is_none_or(|previous| now.duration_since(previous) >= self.interval) {
            *last = Some(now);
            self.inner.report(snapshot);
        }
    }
}
