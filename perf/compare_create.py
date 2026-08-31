"""Compare two release create_perf executables on identical, warm-cache ZIP inputs."""

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


def prepare(root):
    root.mkdir(parents=True, exist_ok=True)
    small = root / "small-files"
    small.mkdir(exist_ok=True)
    for index in range(20_000):
        path = small / f"entry-{index:05d}.txt"
        if not path.exists():
            path.write_bytes((f"record={index:05d}; category=archive; value=123456789\n".encode() * 48)[:2048])
    sql = root / "large.sql"
    if not sql.exists():
        block = b"".join(
            f"INSERT INTO records VALUES ({i}, 'customer-{i % 997}', '2026-08-31', {i * 17});\n".encode()
            for i in range(16_384)
        )
        with sql.open("wb") as output:
            remaining = 128 * 1024 * 1024
            while remaining:
                chunk = block[:remaining]
                output.write(chunk)
                remaining -= len(chunk)
    random_file = root / "incompressible.bin"
    if not random_file.exists():
        rng = random.Random(20260831)
        with random_file.open("wb") as output:
            for _ in range(64):
                output.write(rng.randbytes(1024 * 1024))
    return [small, sql, random_file]


def expected_contents(source):
    is_directory = source.is_dir()
    files = sorted(source.rglob("*")) if is_directory else [source]
    return {
        (path.relative_to(source).as_posix() if is_directory else path.name):
        hashlib.sha256(path.read_bytes()).hexdigest()
        for path in files if path.is_file()
    }


def verify_zip(output, expected):
    # Read every member: zipfile checks CRC, and SHA-256 checks against source data.
    with zipfile.ZipFile(output) as archive:
        assert len(archive.infolist()) == len(expected), "entry count changed"
        assert set(archive.namelist()) == set(expected), "archive paths changed"
        for name, digest in expected.items():
            assert hashlib.sha256(archive.read(name)).hexdigest() == digest, name


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--candidate", type=Path)
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--level", type=int, default=6)
    parser.add_argument("--threads", default="auto")
    parser.add_argument("--source", type=Path, action="append",
                        help="use this existing file/folder instead of synthetic fixtures (repeatable)")
    for name in ["baseline", "candidate"]:
        parser.add_argument(f"--{name}-backend", choices=["composite", "7zip", "libarchive"], default="composite")
        parser.add_argument(f"--{name}-dll", type=Path)
        parser.add_argument(f"--{name}-level", type=int, help="override level for a size/speed tradeoff comparison")
    args = parser.parse_args()
    base = Path(__file__).resolve().parent
    sources = [path.resolve() for path in args.source] if args.source else prepare(base / "work" / "compression-inputs")
    if args.prepare_only:
        print("Prepared:", *sources, sep="\n")
        return
    if not args.baseline or not args.candidate or args.runs < 1:
        parser.error("--baseline, --candidate and --runs >= 1 are required")
    output_dir = base / "results" / f"create-{time.time_ns()}"
    output_dir.mkdir(parents=True)
    report = {
        "platform": platform.platform(),
        "processor": platform.processor(),
        "cache_policy": "warm-up per executable; alternating order; no cache purge or disk flush",
        "executables": {name: str(path.resolve()) for name, path in
                        [("baseline", args.baseline), ("candidate", args.candidate)]},
        "backends": {name: getattr(args, f"{name}_backend") for name in ["baseline", "candidate"]},
        "libraries": {name: str(getattr(args, f"{name}_dll").resolve()) if getattr(args, f"{name}_dll") else None
                      for name in ["baseline", "candidate"]},
        "results": [],
    }
    for source in sources:
        print("Hashing input:", source.name, flush=True)
        expected = expected_contents(source)
        samples = {"baseline": [], "candidate": []}
        for run in range(args.runs + 1):
            order = list(samples) if run % 2 == 0 else list(reversed(samples))
            for name in order:
                executable = getattr(args, name).resolve()
                output = output_dir / f"{source.name}-{name}-{run}.zip"
                level = getattr(args, f"{name}_level")
                command = [str(executable), "zip", str(source), str(output),
                           str(args.level if level is None else level), args.threads]
                backend = getattr(args, f"{name}_backend")
                library = getattr(args, f"{name}_dll")
                if backend != "composite" or library:
                    command.append(backend)
                    if library:
                        command.append(str(library.resolve()))
                completed = subprocess.run(
                    command,
                    check=True, capture_output=True, text=True,
                )
                sample = json.loads(completed.stdout)
                verify_zip(output, expected)
                sample["sha256_verified"] = True
                output.unlink()
                if run:
                    samples[name].append(sample)
                print(source.name, name, "warmup" if not run else run, sample, flush=True)
        medians = {name: statistics.median(s["seconds"] for s in values)
                   for name, values in samples.items()}
        result = {"source": source.name, "samples": samples, "median_seconds": medians,
                  "speedup": medians["baseline"] / medians["candidate"],
                  "equal_output_sizes": len({s["output_bytes"] for v in samples.values() for s in v}) == 1}
        report["results"].append(result)
        (output_dir / "results.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print("Results:", output_dir / "results.json")


if __name__ == "__main__":
    main()
