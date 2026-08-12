# ArchiveRclick performance probe

Run from the repository root on Windows:

```powershell
powershell -ExecutionPolicy Bypass -File perf/ArchiveRclick.Perf.ps1 `
  -EntryCount 100000 -PayloadMiB 128 -StartupRuns 5
```

The script creates a deterministic TAR fixture under `perf/work`, builds the
release application and `perf_engine` example, then writes JSON results to
`perf/results/latest.json`. It measures:

- process launch to visible main-window handle;
- idle working-set and private memory after a configurable settling interval;
- libarchive listing time for a large metadata-only archive;
- root model preparation time and row count;
- extraction time and throughput for the configured payload.

Use a supported runtime with `-LibArchivePath C:\path\to\archive.dll`. For local
development only, `-AllowUnsupportedRuntime` permits the Windows
`archiveint.dll` fallback. That fallback is not suitable for release validation.

Generated fixtures, extracted data, and result JSON are ignored by Git. `Prepare`,
`Engine`, and `App` modes are available when only one stage needs to be rerun.

