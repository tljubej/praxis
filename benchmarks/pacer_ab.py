#!/usr/bin/env python3
"""A/B the collector's pacer against itself, in one binary.

Both pacers ship in the same `praxis` build behind `PRAXIS_GC_PACER`
(ADR-112), so the two arms of this measurement are the *same executable* run
twice — no second target directory, no second link, and no chance that a
difference in optimization, layout or link order is read as a difference in
pacing.

Praxis only. The Rust and Python columns are pacer-independent by construction,
and running them would cost ~34 s a pass for three numbers that cannot move.

## What the methodology is defending against

`docs/handovers/23-what-is-left-after-the-performance-work.md` §5: one machine,
no CPU pinning, no frequency locking, and a laptop that drifts by several
percent over a few minutes. So:

* the two arms run **back to back** within a rep, never one arm's five runs
  followed by the other's;
* **which arm goes first alternates** on successive reps, so a monotone drift
  is shared by both arms rather than charged to whichever always ran second;
* the reported figure is the **minimum** per arm, the statistic least
  contaminated by a machine that only ever gets slower under interference.

Peak RSS is one `/usr/bin/time -l` run per (benchmark, arm), taken **after** the
timing runs so it cannot perturb them, through `run.py`'s own `peak_rss_bytes`
— the same function, so the macOS-reports-bytes / Linux-reports-kbytes
convention cannot come apart between the two files.

Correctness is gated on `results.json`'s per-benchmark `checksum`, which is the
same guarantee `run.py` gives: a "fast" arm that printed something else is a
failure, not a result.

Usage:
    ./pacer_ab.py --arm-b bounded:64M:2                  # all eight, 5 reps
    ./pacer_ab.py --only collatz,tree,hashwork --reps 3  # a screening sweep
    ./pacer_ab.py --arm-b bounded:8M:2 --out sweep-8M.json
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
PRAXIS_BIN = ROOT / "target" / "release" / "praxis"

# One import rather than a copy, so the RSS convention cannot drift from the
# harness that produced the numbers this is compared against.
sys.path.insert(0, str(HERE))
from run import BENCHMARKS, die, peak_rss_bytes  # noqa: E402


def timed_run(cmd: list[str], size: int, pacer: str) -> tuple[float, list[str]]:
    """One run under one pacer, returning (wall seconds, stdout lines)."""
    start = time.perf_counter()
    proc = subprocess.run(
        cmd,
        input=f"{size}\n".encode(),
        capture_output=True,
        env={**os.environ, "PRAXIS_GC_PACER": pacer},
    )
    elapsed = time.perf_counter() - start
    if proc.returncode != 0:
        die(
            f"PRAXIS_GC_PACER={pacer} {' '.join(cmd)} exited {proc.returncode}\n"
            f"{proc.stderr.decode(errors='replace')}"
        )
    # A pacer spec the runtime could not read prints one line to stderr and
    # falls back (ADR-112 decision 4). Catching it here is the whole reason it
    # is not silent: a typo would otherwise make this arm measure the other one.
    if b"ignoring PRAXIS_GC_PACER" in proc.stderr:
        die(f"the runtime rejected PRAXIS_GC_PACER={pacer}: {proc.stderr.decode()}")
    return elapsed, proc.stdout.decode().strip().splitlines()


def rss_under(cmd: list[str], size: int, pacer: str) -> float | None:
    """`run.py`'s `peak_rss_bytes`, with the pacer selected for the child."""
    previous = os.environ.get("PRAXIS_GC_PACER")
    os.environ["PRAXIS_GC_PACER"] = pacer
    try:
        return peak_rss_bytes(cmd, size)
    finally:
        if previous is None:
            os.environ.pop("PRAXIS_GC_PACER", None)
        else:
            os.environ["PRAXIS_GC_PACER"] = previous


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--arm-a", default="doubling", help="the control pacer spec")
    ap.add_argument("--arm-b", default="bounded:64M:2", help="the candidate pacer spec")
    ap.add_argument("--only", default="", help="comma-separated benchmark subset")
    ap.add_argument("--reps", type=int, default=5)
    ap.add_argument("--out", default="gcfix.json")
    args = ap.parse_args()

    if not PRAXIS_BIN.exists():
        die(f"{PRAXIS_BIN} not found — run `cargo build --release` first")
    results_path = HERE / "results.json"
    if not results_path.exists():
        die("results.json not found — it is where the checksums come from")
    baseline = json.loads(results_path.read_text())["benchmarks"]
    sizes = json.loads((HERE / "sizes.json").read_text())

    names = [n.strip() for n in args.only.split(",") if n.strip()] or list(BENCHMARKS)
    if unknown := [n for n in names if n not in BENCHMARKS]:
        die(f"unknown benchmark(s): {', '.join(unknown)}")

    arms = {"a": args.arm_a, "b": args.arm_b}
    print(f"arm A: PRAXIS_GC_PACER={args.arm_a}", file=sys.stderr)
    print(f"arm B: PRAXIS_GC_PACER={args.arm_b}", file=sys.stderr)
    print(f"{args.reps} reps, interleaved, alternating which arm goes first\n", file=sys.stderr)

    out: dict[str, dict] = {}
    for name in names:
        size = sizes[name]
        cmd = [str(PRAXIS_BIN), "run", str(HERE / "praxis" / f"{name}.px")]
        expected = baseline[name]["checksum"]
        best = {"a": float("inf"), "b": float("inf")}
        ok = True

        for rep in range(args.reps):
            order = ("a", "b") if rep % 2 == 0 else ("b", "a")
            for arm in order:
                elapsed, lines = timed_run(cmd, size, arms[arm])
                ok = ok and lines == expected
                best[arm] = min(best[arm], elapsed)

        rss = {arm: rss_under(cmd, size, spec) for arm, spec in arms.items()}

        out[name] = {
            # The three keys `report.py` reads, describing arm B — the same
            # schema `gcfix-pre-perf-fixes.json` holds.
            "min": best["b"],
            "peak_rss": rss["b"],
            "checksum_ok": ok,
            # Both arms recorded side by side, because they were measured in one
            # interleaved session and only against each other do they mean
            # anything. Comparing arm B against a `results.json` from another
            # session is exactly the trap README.md's `gcfix.json` paragraph
            # was written to close.
            "size": size,
            "reps": args.reps,
            "arm_a": {"pacer": args.arm_a, "min": best["a"], "peak_rss": rss["a"]},
            "arm_b": {"pacer": args.arm_b, "min": best["b"], "peak_rss": rss["b"]},
        }
        speedup = best["a"] / best["b"]
        mem = rss["a"] / rss["b"] if rss["a"] and rss["b"] else float("nan")
        print(
            f"{name:11s} A {best['a']:7.3f}s  B {best['b']:7.3f}s  {speedup:5.3f}×   "
            f"RSS A {rss['a'] / 2**20:8.1f}  B {rss['b'] / 2**20:8.1f} MiB  "
            f"{mem:6.1f}× less  {'ok' if ok else 'CHECKSUM MISMATCH'}",
            file=sys.stderr,
        )

    (HERE / args.out).write_text(json.dumps(out, indent=2) + "\n")
    print(f"\nwrote {HERE / args.out}", file=sys.stderr)

    times = [out[n]["arm_a"]["min"] / out[n]["arm_b"]["min"] for n in names]
    mems = [
        out[n]["arm_a"]["peak_rss"] / out[n]["arm_b"]["peak_rss"]
        for n in names
        if out[n]["arm_a"]["peak_rss"] and out[n]["arm_b"]["peak_rss"]
    ]
    time_gm = statistics.geometric_mean(times)
    print(
        f"geometric mean: time {time_gm:.3f}× "
        f"({(time_gm - 1) * 100:+.1f}% — positive means arm B is faster), "
        f"peak RSS {statistics.geometric_mean(mems):.1f}× less",
        file=sys.stderr,
    )
    if not all(out[n]["checksum_ok"] for n in names):
        die("at least one arm printed the wrong answer")


if __name__ == "__main__":
    main()
