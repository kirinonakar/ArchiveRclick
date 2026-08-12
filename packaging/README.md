# Rebuilding the native runtime

The checked-in `runtime/x64` directory is the release payload. To reproduce
the open-source DLLs, use Microsoft vcpkg commit
`aae277acf4e7de287ddb5e208b5316614de6aad7` and run from the repository root:

```powershell
vcpkg install "libarchive[core,bzip2,crypto,lz4,lzma,zstd]:x64-windows" `
  --overlay-ports=packaging/vcpkg-overlay `
  --x-install-root=target-vcpkg/installed
```

The overlay pins libarchive 3.8.9 by SHA-512, enables Windows CNG for crypto,
and avoids an OpenSSL runtime dependency. Copy only the DLL allowlist recorded
in `runtime/SHA256SUMS`, update the matching license files, then run
`scripts/package-release.ps1` and the full Rust test suite.
