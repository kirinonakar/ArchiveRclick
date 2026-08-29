use super::super::*;

pub(super) fn wire(
    ui: &AppWindow,
    state: Rc<AppState>,
    engine: Engine,
    operation_progress_window: Rc<RefCell<Option<ProgressWindow>>>,
) {
    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        let engine = Arc::clone(&engine);
        let progress_window = Rc::clone(&operation_progress_window);
        ui.on_password_response(move |password, accepted| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            ui.set_password_visible(false);
            if !accepted || password.is_empty() {
                *state
                    .pending_password_path
                    .lock()
                    .expect("password-path mutex poisoned") = None;
                *state
                    .pending_test_password_path
                    .lock()
                    .expect("test-password-path mutex poisoned") = None;
                ui.set_status_text("Password entry cancelled".into());
                return;
            }
            let password = password.to_string();
            let pending_test = state
                .pending_test_password_path
                .lock()
                .expect("test-password-path mutex poisoned")
                .take();
            if let Some(path) = pending_test {
                start_test(
                    &ui,
                    Rc::clone(&state),
                    Arc::clone(&engine),
                    path,
                    Some(password),
                    Rc::clone(&progress_window),
                );
                return;
            }
            let pending = state
                .pending_password_path
                .lock()
                .expect("password-path mutex poisoned")
                .take();
            let Some(path) = pending.or_else(|| {
                state
                    .listing
                    .borrow()
                    .as_ref()
                    .map(|listing| listing.archive_path.clone())
            }) else {
                return;
            };
            let same_archive = state
                .listing
                .borrow()
                .as_ref()
                .is_some_and(|listing| listing.archive_path == path);
            let directory = if same_archive {
                state.current_folder.borrow().clone()
            } else {
                PathBuf::new()
            };
            start_listing(
                &ui,
                Rc::clone(&state),
                Arc::clone(&engine),
                path,
                Some(password),
                pathname_codepage(ui.get_encoding_selection()),
                directory,
            );
        });
    }

    {
        let cancellation = Arc::clone(&state.cancellation);
        let pending_conflict = Arc::clone(&state.pending_conflict);
        let weak = ui.as_weak();
        ui.on_cancel_requested(move || {
            if let Some(token) = cancellation
                .lock()
                .expect("cancellation mutex poisoned")
                .as_ref()
            {
                token.cancel();
            }
            if let Some(sender) = pending_conflict
                .lock()
                .expect("conflict mutex poisoned")
                .take()
            {
                let _ = sender.send(ConflictChoice::Cancel);
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_status_text("Cancelling…".into());
                ui.set_conflict_visible(false);
            }
        });
    }

    {
        let pending_conflict = Arc::clone(&state.pending_conflict);
        let weak = ui.as_weak();
        ui.on_conflict_response(move |response| {
            let choice = conflict_choice_from_response(response);
            if let Some(sender) = pending_conflict
                .lock()
                .expect("conflict mutex poisoned")
                .take()
            {
                let _ = sender.send(choice);
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_conflict_visible(false);
            }
        });
    }
}
