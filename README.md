# ArchiveRclick

<p align="center">
  <img src="app.png" alt="ArchiveRclick" width="100" height="100" />
</p>

ArchiveRclick is a small, portable Windows x64 archive manager written in Rust.
It uses Slint for the native UI, bundled 7-Zip (`7z.dll`) for ZIP/7z operations,
and libarchive for broader archive-format support.

<img src="screenshot.png" alt="ArchiveRclick screenshot" width="70%">

<a href="https://slint.dev/"><img src="https://raw.githubusercontent.com/slint-ui/slint/v1.17.1/logo/MadeWithSlint-logo-whitebg.png" alt="Made with Slint" width="160"></a>

## Features

- Open and browse ZIP, 7z, RAR/RAR5, TAR, CAB, ISO/IMG, LHA/LZH, CPIO, AR, XAR,
  WARC, and other formats supported by the bundled libarchive runtime. Formats
  are detected from archive contents rather than filename extensions.
- Navigate folders, sort and resize columns, select individual entries or a
  contiguous range with Shift-click, drag and drop to extract.
- Extract all or selected entries to a chosen directory, the current directory,
  or an archive-name directory. Choose how existing files are handled, enter a
  password when needed, cancel long operations, and monitor progress.
- Create ZIP or 7z archives from files and folders with selectable compression
  level and CPU threads. Password protection is supported; 7z can also encrypt
  file names (headers).
- Create split ZIP/7z archives from the create dialog with volume-size presets
  ranging from 10 MB to 92.4 GB. Parts are written as `.zip.001` or `.7z.001`
  sequences; open the first part to browse or extract the complete archive.
- Drag selected entries from the archive to Explorer. Drop a single archive onto
  the window to open it, or drop multiple files/a folder to prepare a new archive.
- Optional Explorer right-click commands for extraction, one-archive creation,
  and one-archive-per-folder batch creation.
- English, Korean, and Japanese UI; system/light/dark themes; configurable
  fonts, legacy filename code pages, 7z thread count, and header-encryption
  defaults.

## Download

Download the portable ZIP from the [Releases page](https://github.com/kirinonakar/ArchiveRclick/releases).

- [Microsoft Store](https://apps.microsoft.com/detail/9PDZJM0TSVPK?hl=ko-kr&gl=KR&ocid=pdpshare) version supports the modern Windows right-click menu.

## Requirements

- 64-bit Windows 10 or 11.
- Rust 1.92 or newer with the MSVC Windows target for building.

The supported libarchive 3.8.9 runtime and its codec dependencies are bundled;
no separate archive-runtime installation is required.

## Build and use

```powershell
cargo build --release
target\release\archive-rclick.exe example.7z
target\release\archive-rclick.exe --check-runtime
```

The build copies the required native DLLs and runtime notices beside the
executable. Keep them together when copying the application. To create a clean
portable package with hashes and complete license files, run:

```powershell
.\package-release.ps1
```

Command-line operations:

```text
ArchiveRclick [archive|command]
  extract <archive>...  Extract each archive into its own subfolder
  zip <path>...         Create one ZIP archive
  7z <path>...          Create one 7z archive
  zip-each <folder>...  Create one ZIP archive per folder
  7z-each <folder>...   Create one 7z archive per folder
  --register            Register as an available Windows archive handler
  --unregister          Remove that registration
```

`--register` only adds a per-user handler that can be selected under Windows
Default apps; it does not replace the user's existing defaults. The Explorer
right-click extension can be registered or removed from the application's
Settings screen.

## Security

Archive paths are treated as untrusted. Extraction rejects absolute paths,
parent traversal, reserved Windows names, links, special files, and reparse
points. Listing, testing, and extraction apply entry/size limits and verify
destination handles before installing output. `runtime/SHA256SUMS` records the
expected hashes of the bundled native files.

## License and third-party components

ArchiveRclick source code is licensed under the [MIT License](LICENSE).

The portable ZIP is a combined distribution and is not wholly MIT-licensed;
the bundled components listed below retain their own license terms.

The portable Windows build also redistributes these native components under
their own terms:

| Component | Version | License summary |
| --- | --- | --- |
| libarchive | 3.8.9 | Mixed per-file notices, mainly BSD-style |
| zlib | 1.3.2 | zlib license |
| bzip2 | 1.0.8 | bzip2 license |
| XZ Utils / liblzma | 5.8.3 | 0BSD for liblzma; see package notice |
| LZ4 | 1.10.0 | BSD 2-Clause |
| Zstandard | 1.5.7 | BSD or GPLv2 |
| 7-Zip (`7z.dll`) | 26.02 | LGPL with the 7-Zip unRAR restriction and BSD portions |
| Microsoft VC runtime | 14.51.36247.0 | Microsoft Visual Studio REDIST terms |

The complete notices and license texts are included in
[`runtime/THIRD-PARTY-NOTICES.md`](runtime/THIRD-PARTY-NOTICES.md) and
[`runtime/licenses/`](runtime/licenses/). Microsoft redistribution terms are
also listed at the [Visual Studio REDIST page](https://aka.ms/vs/18/redistribution).
Rust dependency license metadata is listed in
[`THIRD-PARTY-LICENSES.md`](THIRD-PARTY-LICENSES.md).

## Architecture

- `src/archive`: archive API, metadata, options, path safety, and native FFI.
- `src/tasks`: cancellation and throttled progress primitives.
- `src/app`: Slint state and command wiring.
- `src/platform`: Windows dialogs, settings, file associations, and Explorer integration.
- `ui`: Slint components.