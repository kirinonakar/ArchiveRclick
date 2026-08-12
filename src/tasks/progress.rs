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
    pub entries_processed: u64,
    pub total_entries: Option<u64>,
    pub bytes_processed: u64,
    pub total_bytes: Option<u64>,
}

impl ProgressSnapshot {
    pub fn new(phase: ProgressPhase) -> Self {
        Self {
            phase,
            current_file: String::new(),
            entries_processed: 0,
            total_entries: None,
            bytes_processed: 0,
            total_bytes: None,
        }
    }

    pub fn fraction(&self) -> f32 {
        if let Some(total) = self.total_bytes.filter(|total| *total > 0) {
            (self.bytes_processed as f64 / total as f64).clamp(0.0, 1.0) as f32
        } else if let Some(total) = self.total_entries.filter(|total| *total > 0) {
            (self.entries_processed as f64 / total as f64).clamp(0.0, 1.0) as f32
        } else {
            0.0
        }
    }
}

pub struct ThrottledProgress<'a> {
    inner: &'a dyn ProgressSink,
    interval: Duration,
    last_report: Mutex<Option<Instant>>,
}

impl<'a> ThrottledProgress<'a> {
    pub fn new(inner: &'a dyn ProgressSink, interval: Duration) -> Self {
        Self {
            inner,
            interval,
            last_report: Mutex::new(None),
        }
    }

    pub fn report(&self, snapshot: ProgressSnapshot, force: bool) {
        let mut last = self.last_report.lock().expect("progress mutex poisoned");
        let now = Instant::now();
        if force || last.is_none_or(|previous| now.duration_since(previous) >= self.interval) {
            *last = Some(now);
            self.inner.report(snapshot);
        }
    }
}
