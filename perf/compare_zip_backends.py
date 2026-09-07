"""Compare the CLI's three ZIP backends and verify every output against its source."""

import argparse
import hashlib
import json
import platform
import random
import statistics
import subprocess
import time
import zipfile
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, default=Path("target/release/archive-rclick-cli.exe"))
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--threads", type=int, default=4)
    parser.add_argument("--level", type=int, choices=range(10), default=5)
    parser.add_argument("--extract", action="store_true", help="also measure selected-backend extraction")
    parser.add_argument("--case", choices=["text", "incompressible", "small-files"], action="append")
    args = parser.parse_args()
    if args.runs < 1 or not 1 <= args.threads <= 1024:
        parser.error("runs must be positive; threads must be 1..1024")
    cli = args.cli.resolve(strict=True)
    root = Path(__file__).resolve().parent / "results" / f"zip-backends-{time.time_ns()}"
    root.mkdir(parents=True)
    rng = random.Random(20260907)
    fixtures = {
        "text": [
            (f"{i}.txt", (f"record={i}; compression backend benchmark; value=123456789\n".encode() * 45000))
            for i in range(8)
        ],
        "incompressible": [(f"{i}.bin", rng.randbytes(2 * 1024 * 1024)) for i in range(8)],
        "small-files": [(f"{i}.txt", (f"record={i}; archive\n".encode() * 64)) for i in range(2000)],
    }
    result = {"platform": platform.platform(), "cli": str(cli),
              "cli_sha256": hashlib.sha256(cli.read_bytes()).hexdigest(), "level": args.level,
              "threads": args.threads, "runs": args.runs, "cases": []}
    for case, entries in fixtures.items():
        if args.case and case not in args.case:
            continue
        source = root / case
        source.mkdir()
        expected = {}
        for name, data in entries:
            (source / name).write_bytes(data)
            expected[f"{case}/{name}"] = hashlib.sha256(data).digest()
        samples = {backend: [] for backend in ("7z", "zlib-ng", "zlib-rs")}
        for run in range(args.runs):
            # Rotate execution order so one engine does not always get the first run.
            backends = list(samples)
            backends = backends[run % 3:] + backends[:run % 3]
            for backend in backends:
                output = root / f"{case}-{backend}-{run}.zip"
                started = time.perf_counter()
                subprocess.run([str(cli), "a", str(output), str(source),
                                f"-zip-backend={backend}", f"-mx={args.level}",
                                f"-mmt={args.threads}", "-bd"], check=True,
                               stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                elapsed = time.perf_counter() - started
                with zipfile.ZipFile(output) as archive:
                    actual = {entry.filename: hashlib.sha256(archive.read(entry)).digest()
                              for entry in archive.infolist() if not entry.is_dir()}
                    assert actual == expected, f"content mismatch: {output}"
                sample = {"seconds": elapsed, "bytes": output.stat().st_size}
                if args.extract:
                    extracted = root / f"extract-{case}-{backend}-{run}"
                    started = time.perf_counter()
                    subprocess.run([str(cli), "x", str(output), f"-o{extracted}",
                                    f"-zip-backend={backend}", f"-mmt={args.threads}", "-y", "-bd"],
                                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                    sample["extract_seconds"] = time.perf_counter() - started
                    actual = {p.relative_to(extracted).as_posix(): hashlib.sha256(p.read_bytes()).digest()
                              for p in extracted.rglob("*") if p.is_file()}
                    assert actual == expected, f"extracted content mismatch: {extracted}"
                samples[backend].append(sample)
        for backend, runs in samples.items():
            row = {"case": case, "backend": backend,
                   "median_seconds": statistics.median(r["seconds"] for r in runs),
                   "archive_bytes": runs[0]["bytes"], "samples": runs}
            if args.extract:
                row["median_extract_seconds"] = statistics.median(r["extract_seconds"] for r in runs)
            result["cases"].append(row)
            print(f"{case:16} {backend:7} {row['median_seconds']:.3f}s {row['archive_bytes']:,} bytes", flush=True)
            if args.extract:
                print(f"  extract: {row['median_extract_seconds']:.3f}s", flush=True)
    report = root / "results.json"
    report.write_text(json.dumps(result, indent=2), encoding="utf-8")
    print(report)


if __name__ == "__main__":
    main()
