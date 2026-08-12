//! File-drop bridge from the winit backend to application commands.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
};

use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};

pub type FileDropHandler = Box<dyn Fn(Vec<PathBuf>)>;

pub fn install_file_drop_handler(window: &slint::Window, handler: FileDropHandler) {
    let mut hovered_paths = Vec::new();
    let mut dropped_paths = Vec::new();
    let mut drop_started = false;
    let mut rejected = false;

    window.on_winit_window_event(move |_window, event| match event {
        WindowEvent::HoveredFile(path) => {
            // Windows reports the complete CF_HDROP payload as one HoveredFile
            // event per path before it reports the corresponding DroppedFile
            // events. The first hover therefore also starts a fresh batch.
            if drop_started {
                hovered_paths.clear();
                dropped_paths.clear();
                drop_started = false;
                rejected = false;
            }
            if hovered_paths.len() >= 4096 {
                rejected = true;
            } else if !rejected {
                hovered_paths.push(path.clone());
            }
            EventResult::PreventDefault
        }
        WindowEvent::HoveredFileCancelled => {
            hovered_paths.clear();
            dropped_paths.clear();
            drop_started = false;
            rejected = false;
            EventResult::PreventDefault
        }
        WindowEvent::DroppedFile(path) => {
            drop_started = true;
            if rejected {
                return EventResult::PreventDefault;
            }

            if hovered_paths.is_empty() {
                let path = path.clone();
                rejected = true;
                let _ = catch_unwind(AssertUnwindSafe(|| handler(vec![path])));
                return EventResult::PreventDefault;
            }

            let index = dropped_paths.len();
            if hovered_paths.get(index) != Some(path) {
                hovered_paths.clear();
                dropped_paths.clear();
                rejected = true;
                return EventResult::PreventDefault;
            }

            dropped_paths.push(path.clone());
            if dropped_paths.len() == hovered_paths.len() {
                hovered_paths.clear();
                let paths = std::mem::take(&mut dropped_paths);
                rejected = true;
                let _ = catch_unwind(AssertUnwindSafe(|| handler(paths)));
            }
            EventResult::PreventDefault
        }
        _ => EventResult::Propagate,
    });
}
