use std::ffi::{OsStr, OsString};

use crate::archive::CreateFormat;

use super::context_menu::ContextMenuCommand;

/// Selects the UI mode without loading an archive engine or creating a window.
/// This keeps argument interpretation deterministic and independently testable.
pub(super) enum LaunchRequest {
    MainWindow(Option<OsString>),
    ContextMenu(ContextMenuCommand),
}

pub(super) fn parse_launch_request(mut args: impl Iterator<Item = OsString>) -> LaunchRequest {
    let first = args.next();
    let elevated_retry = first.as_deref() == Some(OsStr::new("--elevated-retry"));
    let command = if elevated_retry { args.next() } else { first };
    let remaining = || args.collect::<Vec<_>>();

    match command.as_deref().and_then(|value| value.to_str()) {
        Some("extract") => LaunchRequest::ContextMenu(ContextMenuCommand::Extract {
            args: remaining(),
            elevated_retry,
        }),
        Some("extract-to") => LaunchRequest::ContextMenu(ContextMenuCommand::ExtractTo {
            args: remaining(),
            extract_here: false,
        }),
        Some("extract-here-to") => LaunchRequest::ContextMenu(ContextMenuCommand::ExtractTo {
            args: remaining(),
            extract_here: true,
        }),
        Some("zip-to") => LaunchRequest::ContextMenu(ContextMenuCommand::CreateTo {
            args: remaining(),
            format: CreateFormat::Zip,
        }),
        Some("7z-to") => LaunchRequest::ContextMenu(ContextMenuCommand::CreateTo {
            args: remaining(),
            format: CreateFormat::SevenZip,
        }),
        Some("zip-each-to") => LaunchRequest::ContextMenu(ContextMenuCommand::CreateEachTo {
            args: remaining(),
            format: CreateFormat::Zip,
        }),
        Some("7z-each-to") => LaunchRequest::ContextMenu(ContextMenuCommand::CreateEachTo {
            args: remaining(),
            format: CreateFormat::SevenZip,
        }),
        Some("zip") => LaunchRequest::ContextMenu(ContextMenuCommand::Create {
            args: remaining(),
            format: CreateFormat::Zip,
            elevated_retry,
        }),
        Some("7z") => LaunchRequest::ContextMenu(ContextMenuCommand::Create {
            args: remaining(),
            format: CreateFormat::SevenZip,
            elevated_retry,
        }),
        Some("zip-each") => LaunchRequest::ContextMenu(ContextMenuCommand::CreateEach {
            args: remaining(),
            format: CreateFormat::Zip,
            elevated_retry,
        }),
        Some("7z-each") => LaunchRequest::ContextMenu(ContextMenuCommand::CreateEach {
            args: remaining(),
            format: CreateFormat::SevenZip,
            elevated_retry,
        }),
        _ => LaunchRequest::MainWindow(command),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevated_retry_is_attached_to_the_context_command() {
        let request = parse_launch_request(
            ["--elevated-retry", "zip", "--output", "out.zip", "input"]
                .into_iter()
                .map(OsString::from),
        );

        let LaunchRequest::ContextMenu(ContextMenuCommand::Create {
            args,
            format,
            elevated_retry,
        }) = request
        else {
            panic!("expected a create context command");
        };
        assert!(elevated_retry);
        assert_eq!(format, CreateFormat::Zip);
        assert_eq!(args, ["--output", "out.zip", "input"]);
    }

    #[test]
    fn an_archive_path_remains_a_main_window_startup_argument() {
        let request = parse_launch_request([OsString::from("sample.zip")].into_iter());
        let LaunchRequest::MainWindow(Some(argument)) = request else {
            panic!("expected a main-window request");
        };
        assert_eq!(argument, "sample.zip");
    }
}
