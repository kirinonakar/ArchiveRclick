# ArchiveRclick third-party notices

ArchiveRclick includes Windows x64 runtime binaries for libarchive 3.8.11,
zlib 1.4, bzip2 1.0.8, XZ Utils/liblzma 5.8.4, LZ4 1.11.1,
Zstandard 1.5.7, and 7-Zip 26.06 (7z.dll). Their complete license texts are
in the adjacent `licenses` directory in the source distribution.

The portable package also includes Microsoft's unmodified
`vcruntime140.dll` 14.51.36247.0 from the Visual Studio 2026 redistributable
payload. Redistribution is governed by the applicable Microsoft Visual Studio
license and its REDIST list: <https://aka.ms/vs/18/redistribution>.

Source and project information:

- libarchive: <https://github.com/libarchive/libarchive/tree/v3.8.11>
- zlib: <https://zlib.net/>
- bzip2: <https://sourceware.org/bzip2/>
- XZ Utils: <https://tukaani.org/xz/>
- LZ4: <https://github.com/lz4/lz4>
- Zstandard: <https://github.com/facebook/zstd>
- 7-Zip: <https://www.7-zip.org/>

## Slint

ArchiveRclick uses Slint 1.17.1 for its user interface under the Slint
Royalty-free Desktop, Mobile, and Web Applications License, version 2.0.
The [license text](licenses/LicenseRef-Slint-Royalty-free-2.0.md) is bundled
with the app. Slint also provides the [official license PDF](https://slint.dev/agreements/slint-royalty-free-license.pdf).

## Statically linked ZIP backends

ZIP creation additionally uses zip 9.6.1, flate2 1.1.10, libz-ng-sys 1.1.29
(with zlib-ng), and zlib-rs 0.6.8. License texts are in `licenses/zip.txt`,
`licenses/flate2-mit.txt`, `licenses/flate2-apache.txt`, `licenses/zlib-ng.txt`,
and `licenses/zlib-rs.txt`. See the main THIRD-PARTY-LICENSES.md for supporting
Rust dependencies and vendor/README.md in source distributions for local changes.
