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


## ZIP backend comparison (CLI only)

After `cargo build --release --locked`, run:

```powershell
python perf/compare_zip_backends.py --runs 3 --threads 4 --level 5
```

This compares `7z`, `zlib-ng`, and `zlib-rs` on text, incompressible data,
and many small files, rotating backend order between runs. Every archive is
read by Python's independent ZIP reader and checked against source SHA-256
hashes. JSON contains raw timings, medians, archive sizes, and platform details
under `perf/results/zip-backends-*`. Measurements include CLI startup and file
I/O, use warm caches, and are not a general CPU or compression ranking.

Add `--extract` to time and SHA-256-verify extraction with the same selected
backend, or `--case small-files` to focus on scheduling/allocation overhead.
Keep a baseline CLI and its runtime DLLs and pass `--cli <baseline-path>` to run
the same deterministic fixtures against the previous implementation. Benchmark
without builds or other disk-heavy jobs running in parallel.

## Native 7z stream comparison

Build `cargo build --release --locked --example create_perf --example extract_perf`
before and after the change. Preserve the baseline executables and runtime DLLs
in a separate directory, then run:

```powershell
python perf/compare_sevenzip.py --baseline target/perf-sevenzip-baseline `
  --candidate target/release/examples --runs 3 --mib 32 --data random `
  --output perf/results/sevenzip-random.json
```

Repeat with `--data text` and a different output filename for compressible data.
The probe compares levels 0 and 5 with four threads, single-volume creation, and
single/split-volume extraction. Split inputs are identical archive bytes divided
into 4 MiB parts. Every extraction is SHA-256 checked against its source. It
excludes one warm-up, alternates executable order, and saves raw measurements and
medians. Timings measure the engine call with warm OS caches; they do not measure
GUI startup, cold storage, or split-volume creation. Fixtures live in a private
temporary directory and are removed after the run.
