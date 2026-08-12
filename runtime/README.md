# Native runtime bundle

ArchiveRclick dynamically loads `archive.dll` from the application directory.
The files in `x64` are the complete non-system dependency closure for the
Windows x64 build; `build.rs` copies them beside every Windows Cargo binary.

## Provenance

| File | Component | Version |
| --- | --- | --- |
| `archive.dll` | libarchive | 3.8.9 |
| `z.dll` | zlib | 1.3.2 |
| `bz2.dll` | bzip2 | 1.0.8 |
| `liblzma.dll` | XZ Utils / liblzma | 5.8.3 |
| `lz4.dll` | LZ4 | 1.10.0 |
| `zstd.dll` | Zstandard | 1.5.7 |
| `vcruntime140.dll` | Microsoft Visual C++ Runtime | 14.51.36247.0 |

The open-source libraries were built for `x64-windows` using Microsoft vcpkg
commit `aae277acf4e7de287ddb5e208b5316614de6aad7` and the overlay port in
`packaging/vcpkg-overlay/libarchive`. The libarchive source is pinned to the
GitHub `v3.8.9` tag snapshot with the SHA-512 recorded in the portfile. The VC
runtime is copied unmodified from Visual Studio's redistributable directory.

`SHA256SUMS` is the release allowlist. Do not add OpenSSL DLLs: this libarchive
build reports `archive_openssl_version = NULL` and uses Windows CNG instead.

Licenses and notices are preserved in `licenses` and
`THIRD-PARTY-NOTICES.md`.
