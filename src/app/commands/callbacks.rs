//! Composition root for main-window event controllers.

use super::*;

mod archive_actions;
mod archive_view;
mod drop_input;
mod operation_prompts;
mod settings;

/// Connects UI events to focused controllers. Each controller owns one family
/// of user intents; this function only supplies their shared dependencies.
pub(super) fn wire_callbacks(
    ui: &AppWindow,
    state: Rc<AppState>,
    engine: Engine,
    writable_formats: Vec<CreateFormat>,
    operation_progress_window: Rc<RefCell<Option<ProgressWindow>>>,
) {
    archive_view::wire(ui, Rc::clone(&state), Arc::clone(&engine));
    settings::wire(ui);
    archive_actions::wire(
        ui,
        Rc::clone(&state),
        Arc::clone(&engine),
        writable_formats,
        Rc::clone(&operation_progress_window),
    );
    operation_prompts::wire(
        ui,
        Rc::clone(&state),
        Arc::clone(&engine),
        operation_progress_window,
    );
    drop_input::wire(ui, state, engine);
}
