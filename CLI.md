# ArchiveRclick CLI

A console program that uses the same `7z.dll` / libarchive engine as the GUI.
It works without installing `7z.exe`. Keep the DLL shipped with the executable in the same folder.

```powershell
.\archive-rclick-cli.exe a backup.7z .\my-folder -mx=5
.\archive-rclick-cli.exe a backup.zip .\file1.txt .\file2.txt
.\archive-rclick-cli.exe a secret.7z .\my-folder -pMyPassword -mhe=on
.\archive-rclick-cli.exe a split.7z .\my-folder -v100m -mmt=8
.\archive-rclick-cli.exe x backup.7z "-o.\output" -y
.\archive-rclick-cli.exe e backup.zip "-o.\flat" -aos
.\archive-rclick-cli.exe l backup.7z
.\archive-rclick-cli.exe l backup.7z -slt
.\archive-rclick-cli.exe t backup.7z
.\archive-rclick-cli.exe x backup.7z "*.txt" -r "-o.\text"
.\archive-rclick-cli.exe --help
```

- The basic syntax is `archive-rclick-cli <command> [options] <archive> [files...]`.
- `a` creates a new archive. If the extension is missing, `.7z` is appended.
  When you specify a folder, the folder name itself is included in the archive. If no input is given, the contents of the current folder are used.
- `x` extracts while preserving the folder structure, while `e` extracts using file names only.
  The default output location is the current folder. Conflicts are confirmed in the terminal;
  for automated runs, specify overwrite `-y`/`-aoa` or skip `-aos`.
- `l` lists contents, `t` performs a full integrity check, and `i` prints the engine version and the formats that can be created.
- For `.tar.gz`, the outer compression is decompressed first to obtain the `.tar` (like the existing GUI engine), and then `x` is run again on that `.tar` to extract the inner files.
  `.tar.xz` and `.tar.zst` extract the inner files directly.
- Creation formats: `-t7z`, `-tzip`, `-ttar`, `-ttar.gz`, `-ttar.xz`, `-ttar.zst`.
  Password, header encryption, and splitting are only possible in combinations supported by the corresponding engine and format.
- Supports `-mx=0`~`-mx=9`, `-mmt=on|off|<number>`, `-mhe=on|off`, `-v100m`, etc.
- Supports `*`, `?`, and UTF-8 `@listfile`. When creating, use wildcards only in the last path element.
  Directory inputs include their sub-items, and `-r` expands the wildcard search scope.
- Arguments after `--` are not interpreted as options. Wrap paths containing spaces in quotes.
- Output is UTF-8. You can disable progress with `-bd` / `-bsp0`.

This is not a full command-compatible implementation of 7-Zip. Adding to/updating/deleting existing archives (update via `a`, `u`, `d`), standard input/output compression streams, SFX, etc. are not supported. Running `a` on an existing archive returns an error without modifying the files. If the same file/folder name exists across different input paths, specify the common parent folder as the input.
`t` checks the entire archive, not selected files.

Exit codes: `0` success, `1` warning, `2` operation failed, `7` invalid command/option, `255` canceled.

## Build and MSIX

```powershell
.\package-msix.ps1
```

The GUI, `archive-rclick-cli.exe`, the shared DLLs, and this document are all included in the same MSIX.
It uses the existing output path `dist\msix` and does not create a CLI-only deployment folder.
You can run the package configuration files directly from `dist\msix\package`.
After installing the MSIX, you can call it from a terminal using the execution alias `archive-rclick-cli.exe`.
