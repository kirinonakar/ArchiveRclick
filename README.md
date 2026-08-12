# ArchiveRclick

ArchiveRclick is a Windows-focused archive browser written in Rust. It uses
Slint for the native desktop UI and libarchive for archive format support.

The project is intentionally small: open an archive, inspect it, extract all or
selected entries, test it, or create a new archive.

## Requirements

- Rust 1.92 or newer with the MSVC Windows target.
- A 64-bit libarchive 3.8.9 or newer DLL. Put `archive.dll`, `libarchive.dll`, or
  `libarchive-13.dll` beside `archive-rclick.exe`, or set
  `ARCHIVERCLICK_LIBARCHIVE` to its absolute path.

Use a libarchive build that includes zlib, liblzma, and zstd if ZIP, TAR.GZ,
TAR.XZ, and TAR.ZST creation are required. The application detects read formats
from archive contents rather than filename extensions.

## Build

```powershell
cargo build --release
```

Open an archive from the UI or pass it on the command line:

```powershell
target\release\archive-rclick.exe example.7z
```

Register ArchiveRclick as an available handler in Windows Default Apps (per
user, without overriding the user's existing choices):

```powershell
target\release\archive-rclick.exe --register
```

Use `--unregister` to remove that registration.

Explorer files and folders can be dropped onto the window. A single dropped
file is opened as an archive; multiple files or a folder are staged for archive
creation. Dragging entries from inside an archive back to Explorer is not part
of the initial build.

## Architecture

- `src/archive`: UI-independent archive API, metadata, options, and libarchive FFI.
- `src/tasks`: standard-thread cancellation and throttled progress primitives.
- `src/app`: Slint-facing state and command wiring.
- `src/platform`: Windows dialogs, dynamic-library lookup, and shell integration.
- `ui`: Slint components.

Archive entry paths are treated as untrusted. Extraction rejects absolute paths,
parent traversal, reserved Windows names, links, special files, and reparse-point
ancestors beneath the selected destination.

Listing and integrity testing apply entry, metadata, and decompressed-byte caps
to resist resource-exhaustion archives. Extraction also verifies opened root,
parent, and temporary-file handles remain under the selected destination before
installing output. The final Windows rename uses `MoveFileExW`; an attacker who
already has concurrent write access to the chosen destination can still create
a very narrow reparse-point swap race between the last check and that rename.
Fully removing that race requires NT handle-relative create/rename operations.

Runtime round-trip tests require a supported libarchive DLL and fail clearly if
none is available. CI and release packaging should supply libarchive 3.8.9 or
newer—ideally the exact DLL intended for distribution—through the application
directory or `ARCHIVERCLICK_LIBARCHIVE`.
