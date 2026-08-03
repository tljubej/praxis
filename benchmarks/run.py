#!/usr/bin/env python3
"""Run the Praxis benchmark suite and emit results.json.

Every benchmark exists three times — `praxis/<name>.px`, `rust/<name>.rs`,
`python/<name>.py` — implements the same algorithm, reads one integer workload
size on stdin, and prints the same integer checksum lines. The harness refuses
to time a benchmark whose three implementations disagree, so a "fast" number
can never come from a program that did less work.

Four passes per benchmark:

  correctness  one run per language; the three outputs must be identical
  floor        one run per language at size 0 — compile, start, and whatever
               fixed setup the program does before its measured loop
  memory       one run per language under `/usr/bin/time -l`, for peak RSS
  timing       `--reps` runs per language; min and median are reported

The floor pass is what answers "is JIT time influencing the result?": it is the
whole fixed cost of `praxis run` on that exact source file, so the ratio of the
floor to the timed run is the honest upper bound on compile-time contamination.

Usage:
    ./run.py                     # full suite, sizes from sizes.json
    ./run.py --pilot             # tiny sizes, correctness only
    ./run.py --only primes,vm    # a subset
    ./run.py --reps 5            # repetitions in the timing pass
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
PRAXIS_BIN = ROOT / "target" / "release" / "praxis"
BUILD_DIR = HERE / ".build"

LANGS = ("rust", "python", "praxis")

BENCHMARKS = [
    "primes",
    "mandelbrot",
    "collatz",
    "vm",
    "hashwork",
    "tree",
    "pipeline",
    "bfs",
]

# Sizes used by --pilot: small enough to run everywhere in well under a second,
# large enough that every code path is exercised at least once.
PILOT_SIZES = {name: 200 for name in BENCHMARKS}
PILOT_SIZES["mandelbrot"] = 60
PILOT_SIZES["bfs"] = 3
PILOT_SIZES["tree"] = 3
PILOT_SIZES["pipeline"] = 500

RUSTC_FLAGS = ["-C", "opt-level=3", "-C", "codegen-units=1", "-C", "target-cpu=native"]


def cpu_name() -> str:
    """A human-readable CPU name; `platform.processor()` says only "arm" here."""
    if sys.platform == "darwin":
        r = subprocess.run(
            ["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True
        )
        if r.returncode == 0 and r.stdout.strip():
            return r.stdout.strip()
    return platform.processor() or platform.machine()


def memory_gib() -> int | None:
    """Installed RAM in GiB — the ceiling that decides how large a Praxis run can be."""
    try:
        if sys.platform == "darwin":
            out = subprocess.run(
                ["sysctl", "-n", "hw.memsize"], capture_output=True, text=True
            ).stdout
            return round(int(out) / 2**30)
        return round(os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES") / 2**30)
    except (ValueError, OSError, AttributeError):
        return None


def die(msg: str) -> None:
    print(f"benchmarks: {msg}", file=sys.stderr)
    sys.exit(1)


def build_rust(names: list[str]) -> None:
    BUILD_DIR.mkdir(exist_ok=True)
    for name in names:
        src = HERE / "rust" / f"{name}.rs"
        exe = BUILD_DIR / name
        if exe.exists() and exe.stat().st_mtime >= src.stat().st_mtime:
            continue
        print(f"  rustc {name}", file=sys.stderr)
        r = subprocess.run(
            ["rustc", *RUSTC_FLAGS, "-o", str(exe), str(src)],
            capture_output=True,
            text=True,
        )
        if r.returncode != 0:
            die(f"rustc failed for {name}:\n{r.stderr}")


def commands(name: str) -> dict[str, list[str]]:
    return {
        "rust": [str(BUILD_DIR / name)],
        "python": [sys.executable, str(HERE / "python" / f"{name}.py")],
        "praxis": [str(PRAXIS_BIN), "run", str(HERE / "praxis" / f"{name}.px")],
    }


def run_once(cmd: list[str], size: int) -> tuple[float, str]:
    """Run one process to completion, returning (wall seconds, stdout)."""
    payload = f"{size}\n".encode()
    start = time.perf_counter()
    proc = subprocess.run(cmd, input=payload, capture_output=True)
    elapsed = time.perf_counter() - start
    if proc.returncode != 0:
        die(
            f"{' '.join(cmd)} exited {proc.returncode}\n"
            f"--- stdout ---\n{proc.stdout.decode(errors='replace')}\n"
            f"--- stderr ---\n{proc.stderr.decode(errors='replace')}"
        )
    return elapsed, proc.stdout.decode().strip()


RSS_MACOS = re.compile(r"(\d+)\s+maximum resident set size")
RSS_LINUX = re.compile(r"Maximum resident set size \(kbytes\):\s*(\d+)")


def peak_rss_bytes(cmd: list[str], size: int) -> float | None:
    """Peak RSS of one run, in bytes, or None if /usr/bin/time can't report it."""
    if not Path("/usr/bin/time").exists():
        return None
    flag = "-l" if sys.platform == "darwin" else "-v"
    proc = subprocess.run(
        ["/usr/bin/time", flag, *cmd], input=f"{size}\n".encode(), capture_output=True
    )
    err = proc.stderr.decode(errors="replace")
    if m := RSS_MACOS.search(err):
        return float(m.group(1))
    if m := RSS_LINUX.search(err):
        return float(m.group(1)) * 1024
    return None


def praxis_frontend(name: str, reps: int) -> float:
    """`praxis check` on the same source: lex + parse + infer, then stop.

    A lower bound on `run`'s fixed cost — it leaves out MIR lowering and
    Cranelift codegen, which the size-0 floor covers.
    """
    cmd = [str(PRAXIS_BIN), "check", str(HERE / "praxis" / f"{name}.px")]
    best = float("inf")
    for _ in range(reps):
        start = time.perf_counter()
        subprocess.run(cmd, capture_output=True)
        best = min(best, time.perf_counter() - start)
    return best


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--pilot", action="store_true", help="tiny sizes; correctness check only")
    ap.add_argument("--only", default="", help="comma-separated benchmark subset")
    ap.add_argument("--reps", type=int, default=5, help="timed repetitions per pair")
    # A pilot writes somewhere else by default: its numbers are correctness
    # evidence at a toy size, and clobbering a real suite's results.json with
    # them would leave REPORT.md describing measurements that no longer exist.
    ap.add_argument("--out", default=None)
    args = ap.parse_args()
    out_path = Path(
        args.out or (HERE / ("results-pilot.json" if args.pilot else "results.json"))
    )

    if not PRAXIS_BIN.exists():
        die(f"{PRAXIS_BIN} not found — run `cargo build --release` first")
    if shutil.which("rustc") is None:
        die("rustc not on PATH")
    # `PRAXIS_GC_PACER` (ADR-112) changes the collector's schedule and nothing
    # else, so a stale export would move every Praxis number in results.json
    # without moving one character of its output — and REPORT.md would then
    # describe a collector this workspace does not ship. Refusing is the same
    # guard `gcfix-pre-perf-fixes.json`'s deliberate filename is: a measurement
    # that cannot say which build it came from must not be recorded. Use
    # `pacer_ab.py`, which sets the variable per run and writes both arms.
    if (pacer := os.environ.get("PRAXIS_GC_PACER")) is not None:
        die(
            f"PRAXIS_GC_PACER={pacer!r} is set — results.json must measure the "
            "collector this workspace ships. Unset it, or use pacer_ab.py."
        )

    names = [n.strip() for n in args.only.split(",") if n.strip()] or list(BENCHMARKS)
    unknown = [n for n in names if n not in BENCHMARKS]
    if unknown:
        die(f"unknown benchmark(s): {', '.join(unknown)}")

    if args.pilot:
        sizes = {n: PILOT_SIZES[n] for n in names}
    else:
        sizes_path = HERE / "sizes.json"
        if not sizes_path.exists():
            die("sizes.json not found — run with --pilot, or create it")
        all_sizes = json.loads(sizes_path.read_text())
        missing = [n for n in names if n not in all_sizes]
        if missing:
            die(f"sizes.json has no entry for: {', '.join(missing)}")
        sizes = {n: all_sizes[n] for n in names}

    print("building rust benchmarks", file=sys.stderr)
    build_rust(names)

    results = {
        "meta": {
            "machine": cpu_name(),
            "platform": platform.platform(),
            "memory_gib": memory_gib(),
            "python": sys.version.split()[0],
            "rustc": subprocess.run(["rustc", "-V"], capture_output=True, text=True).stdout.strip(),
            "rustc_flags": " ".join(RUSTC_FLAGS),
            "praxis_bin": str(PRAXIS_BIN),
            "reps": args.reps,
            "pilot": args.pilot,
        },
        "benchmarks": {},
    }

    for name in names:
        size = sizes[name]
        print(f"\n{name} (size={size})", file=sys.stderr)
        cmds = commands(name)

        # Correctness gate: one run of each, outputs must agree exactly.
        outputs = {lang: run_once(cmd, size)[1] for lang, cmd in cmds.items()}
        if len(set(outputs.values())) != 1:
            detail = "\n".join(f"  {lang}: {out!r}" for lang, out in outputs.items())
            die(f"{name}: implementations disagree\n{detail}")
        checksum = outputs["rust"].splitlines()
        print(f"  checksum {checksum}", file=sys.stderr)

        entry: dict = {
            "size": size,
            "checksum": checksum,
            "times": {},
            "floor": {},
            "peak_rss": {},
        }

        # Floor: size 0. Compile, start, and any size-independent setup.
        for lang, cmd in cmds.items():
            best = min(run_once(cmd, 0)[0] for _ in range(3))
            entry["floor"][lang] = best

        # Memory: one run each under /usr/bin/time.
        for lang, cmd in cmds.items():
            entry["peak_rss"][lang] = peak_rss_bytes(cmd, size)

        # Timing.
        for lang, cmd in cmds.items():
            samples = sorted(run_once(cmd, size)[0] for _ in range(args.reps))
            entry["times"][lang] = {
                "min": samples[0],
                "median": samples[len(samples) // 2],
                "max": samples[-1],
                "samples": samples,
            }
            rss = entry["peak_rss"][lang]
            rss_s = f"{rss / 2**30:6.2f} GiB" if rss else "     n/a"
            print(
                f"  {lang:7s} min {samples[0]:9.3f}s  median {entry['times'][lang]['median']:9.3f}s"
                f"  peak {rss_s}  floor {entry['floor'][lang] * 1000:7.1f}ms",
                file=sys.stderr,
            )

        entry["praxis_frontend"] = praxis_frontend(name, 3)
        results["benchmarks"][name] = entry

    out_path.write_text(json.dumps(results, indent=2) + "\n")
    print(f"\nwrote {out_path}", file=sys.stderr)


# Guarded so `pacer_ab.py` can import `peak_rss_bytes` rather than copy it: the
# macOS-reports-bytes / Linux-reports-kbytes convention must be one function, or
# the A/B's memory column and this file's would silently differ by 1024×.
if __name__ == "__main__":
    main()
