"""Compare baseline/candidate release examples using identical 7z data and SHA-256."""

import argparse
import hashlib
import json
import platform
import random
import statistics
import subprocess
import tempfile
from pathlib import Path


def run(exe, *args):
    result = subprocess.run([str(exe), *map(str, args)], check=True,
                            capture_output=True, text=True)
    return json.loads(result.stdout)


def hashes(root):
    return {p.relative_to(root).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in root.rglob("*") if p.is_file()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True,
                        help="directory containing create_perf.exe and extract_perf.exe")
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--mib", type=int, default=32)
    parser.add_argument("--data", choices=["random", "text"], default="random")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.runs < 1 or args.mib < 1:
        parser.error("runs and mib must be positive")
    executables = {k: getattr(args, k).resolve() for k in ("baseline", "candidate")}
    report = {"platform": platform.platform(), "executables": {k: str(v) for k, v in executables.items()},
              "policy": "warm-up excluded; alternating order; warm OS cache; engine timing; SHA-256 after extraction",
              "runs": args.runs, "mib": args.mib, "data": args.data, "results": []}
    # TemporaryDirectory only removes fixtures owned by this invocation.
    with tempfile.TemporaryDirectory(prefix="archive-7z-perf-") as temporary:
        root = Path(temporary)
        source = root / "input"
        source.mkdir()
        rng = random.Random(20260908)
        text = b"".join(f"INSERT INTO records VALUES ({i}, 'customer-{i % 997}', {i * 17});\n".encode()
                        for i in range(16384))
        text = (text * 2)[:1024 * 1024]
        with (source / "payload.bin").open("wb") as f:
            for _ in range(args.mib):
                f.write(rng.randbytes(1024 * 1024) if args.data == "random" else text)
        expected = hashes(source)
        for level in (0, 5):
            samples = {name: [] for name in executables}
            for iteration in range(args.runs + 1):
                order = list(executables)
                if iteration % 2:
                    order.reverse()
                for name in order:
                    directory = executables[name]
                    archive = root / f"{name}-{level}-{iteration}.7z"
                    created = run(directory / "create_perf.exe", "7z", source, archive, level, "4", "7zip")
                    extracted = {}
                    for split in (False, True):
                        archive_input = archive
                        if split:
                            with archive.open("rb") as f:
                                index = 1
                                while chunk := f.read(4 * 1024 * 1024):
                                    archive.with_suffix(f".7z.{index:03d}").write_bytes(chunk)
                                    index += 1
                            archive_input = archive.with_suffix(".7z.001")
                        destination = root / f"out-{name}-{level}-{iteration}-{split}"
                        extracted[str(split)] = run(directory / "extract_perf.exe", archive_input, destination)
                        assert hashes(destination) == expected, str(destination)
                    sample = {"create": created, "extract": extracted}
                    if iteration:
                        samples[name].append(sample)
                    print(f"level={level} run={iteration} {name}: "
                          f"create={created['seconds']:.4f}s "
                          f"extract={extracted['False']['seconds']:.4f}s "
                          f"split={extracted['True']['seconds']:.4f}s", flush=True)
            medians = {name: {
                "create": statistics.median(s["create"]["seconds"] for s in values),
                "extract": statistics.median(s["extract"]["False"]["seconds"] for s in values),
                "split_extract": statistics.median(s["extract"]["True"]["seconds"] for s in values),
            } for name, values in samples.items()}
            report["results"].append({"level": level, "samples": samples, "median_seconds": medians})
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(args.output)


if __name__ == "__main__":
    main()
