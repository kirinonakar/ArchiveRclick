# ArchiveRclick third-party notices

ArchiveRclick includes unmodified Windows x64 runtime binaries for libarchive
3.8.9, zlib 1.3.2, bzip2 1.0.8, XZ Utils/liblzma 5.8.3, LZ4 1.10.0,
Zstandard 1.5.7, and 7-Zip 26.03 (7z.dll). Their complete license texts are
in the adjacent `licenses` directory in the source distribution.

The portable package also includes Microsoft's unmodified
`vcruntime140.dll` 14.51.36247.0 from the Visual Studio 2026 redistributable
payload. Redistribution is governed by the applicable Microsoft Visual Studio
license and its REDIST list: <https://aka.ms/vs/18/redistribution>.

Source and project information:

- libarchive: <https://github.com/libarchive/libarchive/tree/v3.8.9>
- zlib: <https://zlib.net/>
- bzip2: <https://sourceware.org/bzip2/>
- XZ Utils: <https://tukaani.org/xz/>
- LZ4: <https://github.com/lz4/lz4>
- Zstandard: <https://github.com/facebook/zstd>
- 7-Zip: <https://www.7-zip.org/>

## Statically linked ZIP backends

ZIP creation additionally uses zip 8.2.0, flate2 1.1.9, libz-ng-sys 1.1.29
(with zlib-ng), and zlib-rs 0.6.7. License texts are in `licenses/zip.txt`,
`licenses/flate2-mit.txt`, `licenses/flate2-apache.txt`, `licenses/zlib-ng.txt`,
and `licenses/zlib-rs.txt`. See the main THIRD-PARTY-LICENSES.md for supporting
Rust dependencies and vendor/README.md in source distributions for local changes.
