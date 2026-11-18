# Rebuilding the native runtime

The checked-in `runtime/x64` directory is the release payload. To reproduce
the open-source DLLs, use Microsoft vcpkg commit
`de36db04550a8625e3154b1a758c60035901f244` and run from the repository root:

```powershell
vcpkg install "libarchive[core,bzip2,crypto,lz4,lzma,zstd]:x64-windows" `
  --overlay-ports=packaging/vcpkg-overlay `
  --x-install-root=target-vcpkg/installed
```

The overlay pins libarchive 3.8.11 by SHA-512 and enables Windows CNG for crypto.
This vcpkg snapshot builds zlib 1.4 as `libz.dll`, so use it together with the
rebuilt `archive.dll`. Copy the listed open-source DLLs from
`target-vcpkg/installed/x64-windows/bin`, refresh `runtime/SHA256SUMS` and
the matching licenses, then run `./package-release.ps1` and the full Rust test suite.
