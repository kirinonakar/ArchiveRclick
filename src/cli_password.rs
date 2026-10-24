use std::io::{self, Write};

#[cfg(windows)]
pub fn prompt() -> io::Result<String> {
    use windows::Win32::{
        Foundation::HANDLE,
        System::Console::{
            CONSOLE_MODE, ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
            SetConsoleMode,
        },
    };
    struct RestoreMode(HANDLE, CONSOLE_MODE);
    impl Drop for RestoreMode {
        fn drop(&mut self) {
            // SAFETY: borrowed standard-input handle; restore on every return.
            let _ = unsafe { SetConsoleMode(self.0, self.1) };
        }
    }
    let console_error = |error: windows::core::Error| io::Error::other(error.to_string());
    // SAFETY: standard handle is borrowed, and mode is a live output parameter.
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.map_err(console_error)?;
    let mut mode = CONSOLE_MODE::default();
    unsafe { GetConsoleMode(handle, &mut mode) }.map_err(console_error)?;
    unsafe { SetConsoleMode(handle, CONSOLE_MODE(mode.0 & !ENABLE_ECHO_INPUT.0)) }
        .map_err(console_error)?;
    let restore = RestoreMode(handle, mode);
    eprint!("Password: ");
    io::stderr().flush()?;
    let mut text = String::new();
    let read = io::stdin().read_line(&mut text);
    drop(restore);
    eprintln!();
    if read? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "password input closed",
        ));
    }
    Ok(text.trim_end_matches(['\r', '\n']).to_owned())
}

#[cfg(not(windows))]
pub fn prompt() -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "hidden password input requires Windows",
    ))
}
