//! Bounded, best-effort diagnostics for the standalone GUI and CLI processes.
use std::{
    fs::{self, OpenOptions},
    io::Write,
    sync::{Mutex, Once},
    time::{SystemTime, UNIX_EPOCH},
};

pub fn log(message: &str) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let Some(base) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let directory = std::path::PathBuf::from(base).join("ArchiveRclick/Logs");
    if fs::create_dir_all(&directory).is_err() {
        return;
    }
    let path = directory.join("diagnostics.log");
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        if file.metadata().is_ok_and(|m| m.len() > 1024 * 1024) {
            let _ = file.set_len(0);
        }
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let message: String = message.chars().take(65536).collect();
        let _ = writeln!(file, "[{seconds} pid={}] {message}", std::process::id());
    }
}

/// Install only in our executables. A DLL must not replace Explorer's global hook.
/// The hook records an aborting release panic too; it cannot recover from one.
pub fn install_panic_hook() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            log(&format!(
                "PANIC: {info}\n{}",
                std::backtrace::Backtrace::force_capture()
            ));
            previous(info);
        }));
    });
}
