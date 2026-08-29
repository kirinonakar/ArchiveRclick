//! Worker/UI synchronization for password and overwrite decisions.

use super::super::*;

/// Coordinates a password dialog shown by the progress-only Explorer window
/// with the worker thread that is waiting to retry the current archive.
pub(super) struct ProgressPasswordPrompt {
    response: Mutex<Option<Option<String>>>,
    wake: Condvar,
}

impl ProgressPasswordPrompt {
    pub(super) fn new() -> Self {
        Self {
            response: Mutex::new(None),
            wake: Condvar::new(),
        }
    }

    pub(super) fn respond(&self, password: Option<String>) {
        let mut response = self
            .response
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *response = Some(password);
        self.wake.notify_all();
    }

    pub(super) fn wait(
        &self,
        weak: &slint::Weak<ProgressWindow>,
        archive: &Path,
        cancel: &CancellationToken,
    ) -> Option<String> {
        {
            let mut response = self
                .response
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            *response = None;
        }
        let operation = format!("Enter the password for {}", archive.display());
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_password_operation(operation.into());
            ui.set_password_value("".into());
            ui.set_password_visible(true);
        });

        let mut response = self
            .response
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while !cancel.is_cancelled() {
            if let Some(password) = response.take() {
                return password;
            }
            response = self
                .wake
                .wait_timeout(response, Duration::from_millis(50))
                .unwrap_or_else(|poison| poison.into_inner())
                .0;
        }
        None
    }
}

/// Coordinates an existing-file prompt in the progress-only Explorer window
/// with the extraction worker waiting for the user's decision.
pub(super) struct ProgressConflictPrompt {
    response: Mutex<Option<ConflictChoice>>,
    wake: Condvar,
}

impl ProgressConflictPrompt {
    pub(super) fn new() -> Self {
        Self {
            response: Mutex::new(None),
            wake: Condvar::new(),
        }
    }

    pub(super) fn respond(&self, choice: ConflictChoice) {
        let mut response = self
            .response
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *response = Some(choice);
        self.wake.notify_all();
    }

    pub(super) fn wait(
        &self,
        weak: &slint::Weak<ProgressWindow>,
        destination: &Path,
        cancel: &CancellationToken,
    ) -> ConflictChoice {
        {
            let mut response = self
                .response
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            *response = None;
        }
        let display = destination.display().to_string();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_conflict_path(display.into());
            ui.set_conflict_visible(true);
        });

        let mut response = self
            .response
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while !cancel.is_cancelled() {
            if let Some(choice) = response.take() {
                return choice;
            }
            response = self
                .wake
                .wait_timeout(response, Duration::from_millis(50))
                .unwrap_or_else(|poison| poison.into_inner())
                .0;
        }
        ConflictChoice::Cancel
    }
}

pub(super) struct ProgressConflictResolver {
    pub(super) weak: slint::Weak<ProgressWindow>,
    pub(super) prompt: Arc<ProgressConflictPrompt>,
    pub(super) cancel: CancellationToken,
    pub(super) selected_policy: Mutex<Option<ConflictChoice>>,
}

impl ConflictResolver for ProgressConflictResolver {
    fn resolve(&self, destination: &Path) -> ConflictChoice {
        if let Some(choice) = *self
            .selected_policy
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
        {
            return choice;
        }
        let choice = self.prompt.wait(&self.weak, destination, &self.cancel);
        if choice == ConflictChoice::Cancel {
            self.cancel.cancel();
            return choice;
        }
        if matches!(
            choice,
            ConflictChoice::OverwriteAll | ConflictChoice::SkipAll
        ) {
            *self
                .selected_policy
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = Some(choice);
        }
        choice
    }
}

pub(in crate::app::commands) fn conflict_choice_from_response(response: i32) -> ConflictChoice {
    match response {
        0 => ConflictChoice::Overwrite,
        1 => ConflictChoice::Skip,
        2 => ConflictChoice::OverwriteAll,
        3 => ConflictChoice::SkipAll,
        _ => ConflictChoice::Cancel,
    }
}

/// The right-drag "extract here" flow asks only on the first conflict. Its
/// three visible answers become the policy for all remaining conflicts.
pub(in crate::app::commands) fn first_conflict_choice_from_response(
    response: i32,
) -> ConflictChoice {
    match response {
        0 => ConflictChoice::OverwriteAll,
        1 => ConflictChoice::SkipAll,
        _ => ConflictChoice::Cancel,
    }
}
