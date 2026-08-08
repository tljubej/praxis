#!/usr/bin/env python3
"""A/B two `praxis` binaries under handover 26 §6's measurement protocol.

**The baseline arm is not the previous commit.** It is *this* tree with the
package's single toggle point reverted — the one lowering arm, the one constant,
the one `if` the package added — rebuilt, and saved aside. That is why this tool
takes two binary paths and builds neither: only the package's author knows where
its toggle is, and no flag can guess it.
[ADR-113](../docs/decisions/113-an-int-box-is-a-table-read-behind-a-pacing-branch.md)
records what the other baseline costs. The inline `Int` box read **−14.4%** on
`mandelbrot` against the previous commit and **−0.8%** against this tree with the
two lowering arms restored to `call_symbol`; the 13.6 points in between were four
unrelated changes (ADR-109's 16-byte header, ADR-110, ADR-111, ADR-112) that had
landed since. A comparison that cannot say what it held constant is not a
measurement.

Everything else this tool does is a clause of §6, and every clause is there
because a previous measurement was wrong:

* **An exclusive lock at `/tmp/praxis-measure.lock`**, and a nonzero exit if it
  cannot be taken. Absolute, and outside every worktree: six worktrees exist
  under `.claude/worktrees/`, each with its own `benchmarks/`, so a repo-relative
  lock file resolves to a different inode in each and excludes nobody.
* **A quiescence gate** — no competing `cargo`/`rustc`/`praxis` process, and a
  1-minute load average under 0.5. One agent's `cargo build` saturates every
  core, and a benchmark timed during it is measuring the compiler. The numbers
  still look plausible, which is what makes this worse than a crash. The gate is
  re-asked **between benchmarks**, not only at the top: the lock excludes another
  measurement, not another agent deciding to build halfway through a sweep.
* **Both binaries copied out of `target/`** before the first run, so a rebuild
  mid-sweep cannot swap an arm out from under the palindrome.
* **A,B,B,A per rep with the leading arm alternating**, minimum 5 reps. The
  palindrome exists so that a monotone drift is charged to both arms equally,
  and **the statistic below reads it the way it was collected: paired.**
* **`sizes.json` is frozen**: its sha256 is asserted. A benchmark whose size
  moved is not comparable to any earlier number, including the wave baseline.
* **Every run's stdout is compared byte-for-byte between the arms**, and against
  `results.json`'s recorded checksum. An arm that printed something else is a
  failure, not a result.
* **Controls the package must not move.** `--controls collatz,primes` for an
  allocator package: if a control moves outside the noise — the larger of the 2%
  floor and the paired dispersion that control's own ratios showed — the whole
  sweep is void and says so, because whatever moved it also moved the benchmark
  you care about.

**What the quiescence gate does not see.** macOS exec-scans a freshly linked
binary through XProtect, and that work runs in `syspolicyd`, not in anything
named `cargo`, `rustc` or `praxis` — a "no compiler alive" check is blind to it,
and it costs ~30 s on the first execution of a new inode. Staging the binaries
creates two new inodes, so this tool pays the scan deliberately in an untimed
warm-up run before the first rep. It cannot pay for a `just ci` that is still
scanning its own 28 test binaries in the background; that is what the load
average is for.

**The statistic is paired, because the palindrome already paired the runs.**
Each rep is four runs and contains exactly two A/B adjacencies — runs 1-2 and
runs 3-4, whichever arm led; runs 2-3 are the same arm twice and are not a pair.
So every rep yields **two ratios**, each formed from two runs *seconds* apart,
`2 × reps` of them in a sweep and ten at the protocol minimum. Pairing is the
entire reason the order is a palindrome; collapsing each arm to one number over
the whole sweep before comparing throws it away and then measures the drift the
palindrome had already cancelled.

* The **headline** is the **median of those ratios**. Robust to the one run a
  Spotlight index landed on, and it never divides a run from minute 1 by a run
  from minute 12.
* The **resolution bar** is the **scaled median absolute deviation of the same
  ratios** — a robust scale to go with a robust centre, both computed on the
  same sample. Its predecessor used `min` for the estimate and the arms' full
  `max − min` range for the bar, which cannot be right at the same time: the
  range is fixed entirely by the single worst sample that `min` was chosen to
  ignore, and what it actually measures is how far the machine drifted over the
  sweep — the thing the palindrome had already cancelled. Run over all eight
  benchmarks with **one binary in both arms**, that bar came out **5-23%, median
  15%**, where the paired dispersion of the very same runs was 0.7-4.0%. A
  control could have regressed 20% and passed.
* `min(A) / min(B)` is **still reported**, as `speedup_min`. Handover 25 and
  ADR-113 quote that number, and a new statistic nobody can line up against the
  old ones is a different way to lose a measurement. It is the second line, not
  the headline.

**And a sub-2% single-benchmark delta on this machine is not a result** (§6).
The tool says so itself rather than leaving it to a hopeful reader, and it says
the same of a delta smaller than the paired dispersion its own ratios showed.
Both are printed because they disagree usefully: the 2% floor is §6's flat claim
about this machine, and the dispersion is what *these* twenty runs actually did.
ADR-113's +1.4%/+2.0% costs on `tree`/`pipeline` are believed only because they
reproduced to the tenth of a percent across two independent passes; that is the
bar for anything the clock could not resolve in one.

Usage:
    ./ab.py --label W6 --arm-a /tmp/praxis-baseline --arm-b /tmp/praxis-w6
    ./ab.py --label W4b --arm-a … --arm-b … --controls collatz,primes
    ./ab.py --label W1 --arm-a … --arm-b … --only vm,bfs,hashwork --reps 7
    ./ab.py --check-only          # gates only: is this machine ready to measure?
    ./ab.py --smoke --label x …   # exercise the harness; output is stamped VOID
"""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent

# One import rather than a copy: `die`'s prefix and `BENCHMARKS`' membership are
# the same facts here as in the harness that produced the wave baseline.
sys.path.insert(0, str(HERE))
from run import BENCHMARKS, PILOT_SIZES, die  # noqa: E402

# Absolute, and deliberately not `benchmarks/.measure.lock`. Handover 26 §7
# trap 2: six git worktrees exist under `.claude/worktrees/`, each carrying its
# own `benchmarks/` directory, so a repo-relative lock path is a *different file*
# in each one and the mutual exclusion silently does nothing.
LOCK_PATH = Path("/tmp/praxis-measure.lock")

# sha256 of `benchmarks/sizes.json` as of e4f42e6. §6 freezes the sizes: a size
# change moves a benchmark to a different rung of the pacer's power-of-two ladder
# (benchmarks/README.md), so a sweep at moved sizes is not comparable to the wave
# baseline it is supposed to be priced against. Update this constant only in the
# same commit that deliberately re-tunes `sizes.json`, and re-baseline everything
# when you do.
SIZES_SHA256 = "194ad251f4c2387ffc36a7586572fbea2c81a06bdc592d03939fa5fe87f6927a"

# §6's quiescence definition, verbatim: no competing build, 1-minute load < 0.5.
#
# The two halves are not the same claim and only one of them is waivable.
#
# **A competing build is always fatal.** `cargo`/`rustc`/`praxis` saturating the
# cores is the failure §6 opens by naming, and no flag turns it into a warning.
#
# **The load ceiling is a proxy**, and its job is to catch what the process match
# cannot see — chiefly XProtect's `syspolicyd` exec-scan of a freshly linked
# binary, which is ~30 s per binary and matches none of those names. 0.5 assumes
# a machine with nothing on it. That assumption fails on a laptop whose owner is
# *watching the measurement happen*: the editor and its renderer alone hold this
# one at ~3, indefinitely, with not one build process alive. Waiting for 0.5 then
# waits forever, and the honest options are to raise the ceiling or to not
# measure at all.
#
# So `--max-load` raises it, and the waiver is recorded in the JSON, printed at
# the top of the run and repeated in the verdict — because the number's caveat
# has to travel with the number. This is sound in a way that skipping the process
# check would not be: a steady UI load is *stationary*, and the palindrome puts
# the two arms seconds apart specifically so that whatever is stationary is
# charged to both. It is also self-limiting — pair-to-pair noise widens the MAD
# bar, so a contaminated sweep reports "cannot resolve" rather than a wrong
# number. What it cannot absorb is a *step change* mid-sweep, which is what the
# between-benchmarks re-check is for and which stays armed at the raised ceiling.
MAX_LOAD_1MIN = 0.5
QUIET_PATTERN = "cargo|rustc|praxis"

# §6: "a sub-2% single-benchmark delta on this machine is not a result". Used
# twice — to annotate a delta as unresolvable, and to decide that a control moved.
NOISE_FLOOR = 0.02

# §6: "minimum 5 reps". Each rep is four runs (A,B,B,A), so 5 reps is 10 samples
# per arm and — because each rep contains two A/B adjacencies — 10 paired ratios.
MIN_REPS = 5

# The reciprocal of the standard normal's 0.75 quantile. It is what turns a
# median absolute deviation into an estimate of the same quantity a standard
# deviation estimates, and it is applied because the result is compared against
# NOISE_FLOOR, which is a σ-shaped claim about this machine. An unscaled MAD is
# ~2/3 of that and the two would not be the same kind of number. It also errs in
# the direction this protocol wants: the wider of two candidate bars is the one
# that refuses to call a marginal delta a result.
MAD_TO_SIGMA = 1.4826


def sha256_of(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def take_measurement_lock() -> int:
    """Take the exclusive measurement lock, or exit nonzero.

    The lock lives on the open descriptor and is released when it closes, so this
    is `os.open` and not `open()`: a raw fd is not reference-counted shut the way
    a file object is, so the lock cannot be dropped by a stray rebinding halfway
    through a sweep. It is returned rather than discarded so the fd has a name at
    the call site saying it is alive on purpose.

    Does not block. A measurement that waited would start whenever the other one
    finished — unattended, hours later, against a `target/` that had moved on,
    which is a worse outcome than the refusal §6 asks for.
    """
    fd = os.open(LOCK_PATH, os.O_RDWR | os.O_CREAT, 0o644)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        # Read before reporting, not before locking: opening for truncation would
        # destroy the holder's identity in the failure path that needs it.
        held_by = os.read(fd, 4096).decode(errors="replace").strip() or "(unrecorded)"
        os.close(fd)
        die(
            f"{LOCK_PATH} is held by another measurement — refusing to run.\n"
            f"  holder: {held_by}\n"
            "  Two measurements on one machine measure each other. Wait for it, "
            "or if it is stale, confirm the pid is gone before removing the file."
        )
    os.ftruncate(fd, 0)
    os.write(fd, f"pid {os.getpid()} · {' '.join(sys.argv)} · {time.ctime()}\n".encode())
    return fd


def load_average_1min() -> float:
    """The 1-minute load average.

    macOS `sysctl -n vm.loadavg` answers `{ 1.2 3.4 5.6 }` — braces and all — so
    it is stripped rather than split blindly. `os.getloadavg()` would do on both
    platforms, but §6 names the `sysctl` command and an operator checking this by
    hand should be reading the same number this does.
    """
    if sys.platform == "darwin":
        out = subprocess.run(
            ["sysctl", "-n", "vm.loadavg"], capture_output=True, text=True
        ).stdout
        fields = out.strip().strip("{}").split()
        if fields:
            return float(fields[0])
    return os.getloadavg()[0]


def _ancestors(pid: int) -> set[int]:
    """This process and every process that spawned it."""
    seen: set[int] = set()
    while pid > 1 and pid not in seen:
        seen.add(pid)
        out = subprocess.run(
            ["ps", "-o", "ppid=", "-p", str(pid)], capture_output=True, text=True
        ).stdout.strip()
        if not out.isdigit():
            break
        pid = int(out)
    return seen


def competing_processes() -> list[str]:
    """Live `cargo`/`rustc`/`praxis` processes that are not this measurement.

    `pgrep -f` matches the whole command line, and this script's own path
    contains "praxis", so it matches itself and every shell that invoked it. Own
    pid and ancestors are excluded: the process that started this measurement is
    by construction not competing with it. Nothing else is filtered — a sibling
    agent's `cargo test` is exactly what this is for.
    """
    proc = subprocess.run(
        ["pgrep", "-fl", QUIET_PATTERN], capture_output=True, text=True
    )
    mine = _ancestors(os.getpid())
    out = []
    for line in proc.stdout.splitlines():
        pid, _, cmd = line.partition(" ")
        if pid.isdigit() and int(pid) not in mine:
            # One `rustc` command line is ~2600 characters of `--extern` flags.
            # Eight of them buries the load average this message also carries.
            out.append(f"{pid} {cmd[:110]}…" if len(cmd) > 110 else f"{pid} {cmd}")
    return out


def check_quiescent(max_load: float = MAX_LOAD_1MIN) -> tuple[bool, float, list[str]]:
    """(is the machine quiet, 1-minute load, the processes that say it is not).

    `max_load` is the ceiling in force; a competing process is fatal at any
    ceiling. See `MAX_LOAD_1MIN` for why only one of the two is waivable.
    """
    busy = competing_processes()
    load = load_average_1min()
    return (not busy and load < max_load, load, busy)


def stage(binary: Path, into: Path, arm: str) -> Path:
    """Copy an arm's binary out of `target/`, so a rebuild cannot swap it.

    §6: "Copy both binaries **out** of `target/` first." A concurrent
    `cargo build --release` relinks `target/release/praxis` in place, and a sweep
    that started against one build and finished against another reports the
    difference between them as this package's win.
    """
    if not binary.exists():
        die(f"arm {arm.upper()}: {binary} not found")
    dest = into / f"praxis-{arm}"
    shutil.copy2(binary, dest)
    return dest


def timed_run(binary: Path, src: Path, size: int) -> tuple[float, bytes]:
    """One run to completion, returning (wall seconds, raw stdout bytes)."""
    start = time.perf_counter()
    proc = subprocess.run(
        [str(binary), "run", str(src)], input=f"{size}\n".encode(), capture_output=True
    )
    elapsed = time.perf_counter() - start
    if proc.returncode != 0:
        die(
            f"{binary.name} run {src.name} exited {proc.returncode}\n"
            f"{proc.stderr.decode(errors='replace')}"
        )
    return elapsed, proc.stdout


def warm_up(binary: Path, src: Path) -> None:
    """One untimed run, to pay XProtect's exec scan before the clock starts.

    Staging created a new inode, and macOS scans a never-before-executed binary
    on its first exec — ~30 s on this machine, charged entirely to whichever arm
    happened to run first. The palindrome cannot absorb that; nothing can. So it
    is paid here, at size 0, and discarded.
    """
    timed_run(binary, src, 0)


def sweep_drift(samples: list[float]) -> float:
    """One arm's full `max − min` range over the whole sweep, relative to its min.

    **Reported, never gated on.** This is a diagnostic about the *machine* —
    how much slower the sweep's worst run was than its best, across minutes —
    and that is nearly all drift and interference, which the palindrome pairs
    away before the arms are compared. It was the previous version's resolution
    bar, and as such it was hopeless: 5-23% across the suite with one binary in
    both arms. Keep reading it to see whether the machine behaved; do not read it
    as an error bar on the delta.
    """
    return (max(samples) - min(samples)) / min(samples)


def scaled_mad(values: list[float]) -> float:
    """A robust scale for `values`: the median absolute deviation, σ-scaled.

    Paired with `statistics.median` as the centre, which is the point — an
    estimator and a dispersion measure have to be the same kind of statistic or
    the pair of them says nothing. Zero is a legitimate answer (more than half
    the ratios identical); `max(NOISE_FLOOR, …)` at the call site is what stops
    that from becoming a claim of infinite resolution.
    """
    if not values:
        return float("nan")
    centre = statistics.median(values)
    return MAD_TO_SIGMA * statistics.median([abs(v - centre) for v in values])


def main() -> None:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--label", default="", help="the package this measures, e.g. W6")
    ap.add_argument("--arm-a", default="", help="baseline binary: this tree, toggle reverted")
    ap.add_argument("--arm-b", default="", help="candidate binary: this tree")
    ap.add_argument("--only", default="", help="comma-separated benchmark subset")
    ap.add_argument(
        "--controls",
        default="",
        help="benchmarks this package must NOT move; one moving voids the sweep",
    )
    ap.add_argument("--reps", type=int, default=MIN_REPS, help=f"A,B,B,A reps (min {MIN_REPS})")
    ap.add_argument(
        "--max-load",
        type=float,
        default=MAX_LOAD_1MIN,
        help=f"1-minute load ceiling (default {MAX_LOAD_1MIN}). Raising it WAIVES "
        "the load half of the quiescence gate and stamps every result with the "
        "waiver; a competing cargo/rustc/praxis process stays fatal regardless. "
        "Raise it only for a load you have identified and know to be steady",
    )
    ap.add_argument("--out", default=None, help="default ab-<label>.json")
    # Mutually exclusive rather than checked: `--check-only` reports whether the
    # machine passes the gates, and `--smoke` waives one of them, so a run that
    # was both would answer its own question wrongly.
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument(
        "--check-only",
        action="store_true",
        help="run every gate — lock, frozen sizes, quiescence, PRAXIS_GC_PACER, "
        "PRAXIS_DUMP_*, a results.json checksum for all eight benchmarks — and "
        "exit; time nothing",
    )
    mode.add_argument(
        "--smoke",
        action="store_true",
        help="exercise the harness at pilot sizes without the quiescence gate; "
        "every number it prints and writes is stamped VOID and is not a measurement",
    )
    args = ap.parse_args()

    # The lock is taken before anything else is inspected, including the
    # arguments' own files: a run that validated first and locked second would
    # spend that window racing the measurement it is about to refuse to disturb.
    lock_fd = take_measurement_lock()  # noqa: F841 — held for the process lifetime

    sizes_path = HERE / "sizes.json"
    if not sizes_path.exists():
        die("sizes.json not found — it is the frozen workload and there is no default")
    actual = sha256_of(sizes_path)
    if actual != SIZES_SHA256:
        die(
            "sizes.json has moved and the workload is supposed to be frozen.\n"
            f"  expected sha256 {SIZES_SHA256}\n"
            f"  found           {actual}\n"
            "  Every A/B in this round, and the wave baseline they are priced "
            "against, were measured at the old sizes. Restore it, or re-baseline "
            "the whole suite and update SIZES_SHA256 in the same commit."
        )

    if args.max_load < MAX_LOAD_1MIN:
        die(f"--max-load {args.max_load} is below the protocol's {MAX_LOAD_1MIN}")
    load_gate_waived = args.max_load > MAX_LOAD_1MIN

    quiet, load, busy = check_quiescent(args.max_load)
    if not quiet and not args.smoke:
        detail = "\n".join(f"    {p}" for p in busy) or "    (none)"
        die(
            "the machine is not quiescent — refusing to measure.\n"
            f"  1-minute load {load:.2f} (must be < {args.max_load})\n"
            f"  competing processes matching /{QUIET_PATTERN}/:\n{detail}\n"
            "  Note this gate is blind to XProtect: a freshly linked binary is "
            "exec-scanned by `syspolicyd`, which matches none of those names, so "
            "a `just ci` that has just finished linking is still costing you "
            "cores for ~30 s per binary. The load average is what sees that.\n"
            "  If the load is steady background you have identified and no build "
            "is running, `--max-load` raises the ceiling and stamps the result."
        )
    if load_gate_waived and busy:
        # Belt and braces: `check_quiescent` already refuses, but this is the one
        # combination someone reaching for `--max-load` is most likely to want to
        # force, so it gets its own sentence rather than a generic refusal.
        die(
            "--max-load waives the load ceiling, not the competing-build check. "
            f"{len(busy)} process(es) matching /{QUIET_PATTERN}/ are running."
        )
    # Every *environment* gate is asked before `--check-only` may answer, because
    # what it answers is "is this machine ready to measure?" and these are part
    # of that question. Below the early return, a shell with `PRAXIS_DUMP_CLIF`
    # exported would get "ready to measure" and then a nonzero exit from the
    # sweep `--check-only` had just green-lit.
    #
    # `PRAXIS_GC_PACER` changes the collector's schedule and nothing else
    # (ADR-112), so a stale export moves both arms without moving one character of
    # their output. The ratio might survive it; the claim "this is what the
    # workspace ships" does not. `run.py` refuses for the same reason.
    if (pacer := os.environ.get("PRAXIS_GC_PACER")) is not None:
        die(f"PRAXIS_GC_PACER={pacer!r} is set — unset it, or use pacer_ab.py")
    # A dump hook prints IR to stderr from inside the compile the floor includes.
    if dumps := sorted(k for k in os.environ if k.startswith("PRAXIS_DUMP_")):
        die(f"{', '.join(dumps)} set — a dump hook is not a thing to time under")

    results_path = HERE / "results.json"
    if not results_path.exists():
        die("results.json not found — it is where the expected checksums come from")
    baseline = json.loads(results_path.read_text())["benchmarks"]
    if args.check_only:
        # The whole suite, not a subset: `--check-only` does not know which
        # benchmarks a later sweep will ask for, so the readiness it reports has
        # to cover all of them.
        if missing := [n for n in BENCHMARKS if not baseline.get(n, {}).get("checksum")]:
            die(
                f"results.json has no checksum for {', '.join(missing)} — a sweep "
                "over them could not tell two identically wrong arms apart. "
                "Re-run run.py at frozen sizes."
            )
        print(
            f"ready to measure: 1-minute load {load:.2f}, no competing build, "
            f"sizes.json frozen at {SIZES_SHA256[:12]}…, no PRAXIS_GC_PACER or "
            f"PRAXIS_DUMP_* in the environment, results.json has a checksum for "
            f"all {len(BENCHMARKS)} benchmarks",
            file=sys.stderr,
        )
        return

    if not args.label:
        die("--label is required: it records which package the sweep belongs to")
    if not args.arm_a or not args.arm_b:
        die(
            "--arm-a and --arm-b are both required. Arm A is this tree with the "
            "package's toggle reverted, not the previous commit — see the module "
            "docstring and ADR-113."
        )
    if args.reps < MIN_REPS:
        die(f"--reps {args.reps} is below the protocol's minimum of {MIN_REPS}")

    all_sizes = json.loads(sizes_path.read_text())

    names = [n.strip() for n in args.only.split(",") if n.strip()] or list(BENCHMARKS)
    controls = [n.strip() for n in args.controls.split(",") if n.strip()]
    if unknown := [n for n in [*names, *controls] if n not in BENCHMARKS]:
        die(f"unknown benchmark(s): {', '.join(unknown)}")
    # A control that was not run cannot void anything, so naming one adds it.
    names += [n for n in controls if n not in names]
    # The suite-checksum gate exists because *both* arms can be identically
    # wrong, and a benchmark `results.json` has never recorded is exactly the
    # case where it cannot say so. A missing entry is refused here, before the
    # clock starts, rather than left to yield `expected = None` — which the
    # comparison reads as a pass, failing the gate open on its one real input.
    # `--smoke` is the documented exception and is handled below: pilot sizes
    # compute a different, equally correct answer.
    if not args.smoke and (
        missing := [n for n in names if not baseline.get(n, {}).get("checksum")]
    ):
        die(
            f"results.json has no checksum for {', '.join(missing)}, and a "
            "measurement whose output nothing can be checked against is not one. "
            "Re-run run.py at frozen sizes, or pass --smoke, which waives this "
            "gate and stamps everything VOID."
        )

    staging = Path(tempfile.mkdtemp(prefix="praxis-ab-"))
    try:
        arms = {
            "a": stage(Path(args.arm_a).resolve(), staging, "a"),
            "b": stage(Path(args.arm_b).resolve(), staging, "b"),
        }
        digests = {arm: sha256_of(p) for arm, p in arms.items()}

        print(f"label   {args.label}", file=sys.stderr)
        for arm in ("a", "b"):
            src = args.arm_a if arm == "a" else args.arm_b
            print(f"arm {arm.upper()}   {src}  sha256 {digests[arm][:12]}…", file=sys.stderr)
        if digests["a"] == digests["b"]:
            print(
                "\n  both arms are byte-identical: this is a harness self-test, and any\n"
                "  difference it reports is this machine's noise and nothing else.\n",
                file=sys.stderr,
            )
        if args.smoke:
            print(
                "\n  VOID — --smoke: pilot sizes, quiescence gate skipped. Exercising the\n"
                "  harness, not measuring anything. Do not quote a number from this run.\n",
                file=sys.stderr,
            )
        if load_gate_waived:
            print(
                f"\n  LOAD GATE WAIVED — ceiling raised {MAX_LOAD_1MIN} → "
                f"{args.max_load}, observed {load:.2f}, no competing build.\n"
                "  Steady load is charged to both arms by the palindrome and\n"
                "  widens the MAD bar; a resolved delta is real, an unresolved\n"
                "  one may be this machine. Not comparable to a 0.5 number.\n",
                file=sys.stderr,
            )
        print(
            f"{args.reps} reps of A,B,B,A, leading arm alternating; "
            f"controls: {', '.join(controls) or 'none'}\n",
            file=sys.stderr,
        )

        first_src = HERE / "praxis" / f"{names[0]}.px"
        for arm in ("a", "b"):
            warm_up(arms[arm], first_src)

        out_benchmarks: dict[str, dict] = {}
        void_reasons: list[str] = []
        # A caveat is not a void reason. Void means "these numbers must not be
        # quoted"; a caveat means "quote them with this attached".
        caveats: list[str] = []

        for name in names:
            size = PILOT_SIZES[name] if args.smoke else all_sizes[name]
            src = HERE / "praxis" / f"{name}.px"
            samples: dict[str, list[float]] = {"a": [], "b": []}
            # (arm A seconds, arm B seconds) for two runs adjacent in time.
            pairs: list[tuple[float, float]] = []
            stdouts: set[bytes] = set()

            for rep in range(args.reps):
                # Palindromic within the rep so that a monotone drift is charged
                # to both arms equally, and the leading arm alternates between
                # reps so that neither arm always pays the rep's cold start.
                order = ("a", "b", "b", "a") if rep % 2 == 0 else ("b", "a", "a", "b")
                rep_runs: list[tuple[str, float]] = []
                for arm in order:
                    elapsed, stdout = timed_run(arms[arm], src, size)
                    samples[arm].append(elapsed)
                    rep_runs.append((arm, elapsed))
                    stdouts.add(stdout)
                # The palindrome's two A/B adjacencies, and the reason it is a
                # palindrome: runs 1-2 and runs 3-4 are each one A and one B a
                # single run apart, whichever arm led. Runs 2-3 are the same arm
                # twice — the B,B or A,A at the centre — and are not a pair.
                # Building the ratio here, from the two times themselves, is what
                # keeps the statistic paired; anything that compares `samples`
                # arm-wise afterwards has already thrown the pairing away.
                for left, right in ((rep_runs[0], rep_runs[1]), (rep_runs[2], rep_runs[3])):
                    # KeyError if `order` ever stops alternating at these
                    # positions, which is the loud failure this deserves: a
                    # silently mis-paired ratio is a wrong measurement that looks
                    # like a right one.
                    by_arm = {left[0]: left[1], right[0]: right[1]}
                    pairs.append((by_arm["a"], by_arm["b"]))

            # Byte-for-byte, and before any timing is believed. Two gates, not
            # one: the arms must agree with each other, and they must agree with
            # the suite's recorded answer — two arms can be identically wrong.
            # The second gate is frozen-size-only: `results.json`'s checksum is
            # the answer at `sizes.json`'s size, and a pilot size computes a
            # different, equally correct one.
            arms_agree = len(stdouts) == 1
            # Non-None in every non-smoke run: the gate above refused to start a
            # sweep over a benchmark `results.json` has no checksum for, so there
            # is no path here on which a missing entry reads as a pass.
            expected = None if args.smoke else baseline[name]["checksum"]
            printed = next(iter(stdouts)).decode(errors="replace").strip().splitlines()
            matches_suite = args.smoke or printed == expected
            if not arms_agree:
                void_reasons.append(f"{name}: the two arms printed different bytes")
            elif not matches_suite:
                void_reasons.append(
                    f"{name}: both arms disagree with results.json's checksum "
                    f"({printed} vs {expected})"
                )

            # The headline, and it is paired: one ratio per A/B adjacency, two
            # per rep, ten at the protocol minimum. Each divides two runs
            # seconds apart, so whatever the machine was doing that minute is
            # very nearly common to both and divides out.
            ratios = [t_a / t_b for t_a, t_b in pairs]
            speedup = statistics.median(ratios)
            delta = speedup - 1.0
            # A robust centre needs a robust scale, and this is the same sample's.
            # It replaces `max(max−min over each arm)`, which was the previous
            # bar: outlier-driven, drift-driven, and measured at 5-23% with the
            # same binary in both arms. The dispersion of the ratios is what it
            # should have been all along — it asks "how much do these ten
            # independent estimates of the ratio disagree", which is exactly the
            # question "can this delta be resolved" needs answered.
            dispersion = scaled_mad(ratios)
            # Handover 25 and ADR-113's statistic, kept so their numbers and
            # these are comparable. Second line, not the headline: `min` over a
            # whole sweep compares a best-case A minutes away from its best-case
            # B, which is the pairing the palindrome exists to preserve, thrown
            # away.
            best = {arm: min(s) for arm, s in samples.items()}
            speedup_min = best["a"] / best["b"]
            drift = {arm: sweep_drift(s) for arm, s in samples.items()}
            is_control = name in controls
            # §6 says a control voids the sweep when it moves "outside noise" —
            # not outside 2%. Whichever of the two is larger is the honest bar;
            # with the noise term being the paired dispersion rather than the
            # sweep's range, "larger" is 2% on any quiet machine and a 5%
            # control move voids as it should.
            bar = max(NOISE_FLOOR, dispersion)
            if is_control and abs(delta) > bar:
                void_reasons.append(
                    f"control {name} moved {delta * 100:+.1f}%, outside its "
                    f"{bar * 100:.1f}% noise bar — whatever moved it is not "
                    "this package's toggle, so the whole sweep is void"
                )

            out_benchmarks[name] = {
                "size": size,
                "is_control": is_control,
                "checksum_ok": arms_agree and matches_suite,
                # median of the paired ratios; >1 means arm B is faster
                "speedup": speedup,
                "delta": delta,
                "paired_dispersion": dispersion,
                "resolution_bar": bar,
                "below_noise_floor": abs(delta) < NOISE_FLOOR,
                "inside_paired_dispersion": abs(delta) < dispersion,
                # min(A)/min(B): handover 25 and ADR-113's statistic, reported
                # so their figures and these can be lined up.
                "speedup_min": speedup_min,
                "delta_min": speedup_min - 1.0,
                # In execution order, so a reader can see drift for themselves.
                "paired_ratios": ratios,
                "arm_a": {
                    "min": best["a"],
                    "median": statistics.median(samples["a"]),
                    "max": max(samples["a"]),
                    "sweep_drift": drift["a"],
                    "samples": samples["a"],
                },
                "arm_b": {
                    "min": best["b"],
                    "median": statistics.median(samples["b"]),
                    "max": max(samples["b"]),
                    "sweep_drift": drift["b"],
                    "samples": samples["b"],
                },
            }
            row = out_benchmarks[name]
            notes = []
            if not row["checksum_ok"]:
                notes.append("CHECKSUM MISMATCH")
            if row["below_noise_floor"]:
                notes.append(f"under the {NOISE_FLOOR * 100:.0f}% floor")
            if row["inside_paired_dispersion"]:
                notes.append("inside the paired spread")
            print(
                f"{'VOID ' if args.smoke else ''}{'control ' if is_control else ''}"
                f"{name:11s} "
                f"A {best['a']:7.3f}s  B {best['b']:7.3f}s  "
                f"paired {speedup:5.3f}× {delta * 100:+5.1f}% "
                f"±{dispersion * 100:4.1f}%  "
                f"min {(speedup_min - 1) * 100:+5.1f}%  "
                f"drift {drift['a'] * 100:4.1f}/{drift['b'] * 100:4.1f}%  "
                f"{'; '.join(notes)}",
                file=sys.stderr,
            )

            # The lock excludes other *measurements*; it does not exclude another
            # agent starting a `cargo build`. A full sweep is minutes long, so
            # the quiescence gate is re-asked between benchmarks rather than only
            # at the top — otherwise a build that began at benchmark three is
            # charged to benchmarks three through eight and nothing says so.
            still_quiet, now_load, now_busy = check_quiescent(args.max_load)
            if not still_quiet and not args.smoke:
                who = f"; first competitor: {now_busy[0]}" if now_busy else ""
                void_reasons.append(
                    f"the machine stopped being quiescent after {name} — 1-minute "
                    f"load {now_load:.2f}, {len(now_busy)} competing process(es)"
                    f"{who}. Everything after this would have measured the "
                    "interference, so the sweep stops here."
                )
                break

        measured = [n for n in out_benchmarks if n not in controls]
        geomean = (
            statistics.geometric_mean([out_benchmarks[n]["speedup"] for n in measured])
            if measured
            else float("nan")
        )
        # The same aggregate over the min-based ratios, so a reader comparing
        # against handover 25's geometric means is comparing like with like.
        geomean_min = (
            statistics.geometric_mean(
                [out_benchmarks[n]["speedup_min"] for n in measured]
            )
            if measured
            else float("nan")
        )
        if args.smoke:
            void_reasons.append("--smoke: pilot sizes, quiescence gate skipped")
        # Both ends recorded, so a reader can see whether the machine was as
        # quiet at the end as it was asked to be at the start. Read before the
        # caveat is composed, because the caveat quotes it.
        _, end_load, _ = check_quiescent(args.max_load)
        if load_gate_waived:
            # NOT a void reason. A void sweep is one whose numbers must not be
            # quoted; this one's may be, with the caveat attached — which is why
            # the caveat is a first-class field and is printed twice rather than
            # left in the JSON for a reader who may never open it.
            caveats.append(
                f"the load ceiling was raised from {MAX_LOAD_1MIN} to "
                f"{args.max_load} (--max-load); observed {load:.2f} at the start "
                f"and {end_load:.2f} at the end, with no competing build at "
                "either point. The palindrome charges steady load to both arms "
                "equally and the MAD bar widens with what it cannot absorb, so a "
                "resolved delta here is real and an unresolved one may only be "
                "this machine. Do not compare against a number taken at 0.5."
            )
        out = {
            "meta": {
                "label": args.label,
                "when": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
                "machine": platform.platform(),
                "arm_a": {"path": str(Path(args.arm_a).resolve()), "sha256": digests["a"]},
                "arm_b": {"path": str(Path(args.arm_b).resolve()), "sha256": digests["b"]},
                "same_binary": digests["a"] == digests["b"],
                "reps": args.reps,
                "runs_per_arm": args.reps * 2,
                # Two A/B adjacencies per rep, so as many ratios as runs per arm.
                "paired_ratios_per_benchmark": args.reps * 2,
                "statistic": (
                    "speedup = median of the per-pair A/B ratios; "
                    "resolution_bar = max(2% floor, 1.4826 × MAD of those ratios); "
                    "speedup_min = min(A)/min(B), reported for comparability with "
                    "handover 25 and ADR-113"
                ),
                "controls": controls,
                "sizes_sha256": SIZES_SHA256,
                "load_1min_at_start": load,
                "load_1min_at_end": end_load,
                "quiescent": quiet,
                "load_ceiling": args.max_load,
                "load_gate_waived": load_gate_waived,
                "benchmarks_completed": len(out_benchmarks),
                "noise_floor": NOISE_FLOOR,
                "smoke": args.smoke,
            },
            "benchmarks": out_benchmarks,
            "geometric_mean": geomean,
            "geometric_mean_min": geomean_min,
            # The verdict is a field rather than something the reader infers,
            # because a void sweep whose per-benchmark numbers look fine is
            # exactly the artefact §6 exists to stop being quoted.
            "verdict": "void" if void_reasons else ("ok, with caveats" if caveats else "ok"),
            "void_reasons": void_reasons,
            "caveats": caveats,
        }
        out_path = Path(args.out or HERE / f"ab-{args.label}.json")
        out_path.write_text(json.dumps(out, indent=2) + "\n")
        print(f"\nwrote {out_path}", file=sys.stderr)

        print(
            f"geometric mean over {len(measured)} non-control benchmarks: "
            f"{geomean:.3f}× ({(geomean - 1) * 100:+.1f}% — positive means arm B is "
            f"faster), paired; {geomean_min:.3f}× ({(geomean_min - 1) * 100:+.1f}%) "
            "on min(A)/min(B), which is handover 25's statistic",
            file=sys.stderr,
        )
        unresolved = [
            n
            for n in measured
            if out_benchmarks[n]["below_noise_floor"]
            or out_benchmarks[n]["inside_paired_dispersion"]
        ]
        if unresolved:
            print(
                f"{len(unresolved)} of {len(measured)} deltas the clock could not "
                f"resolve ({', '.join(unresolved)}) — either under the "
                f"{NOISE_FLOOR * 100:.0f}% floor handover 26 §6 sets for this machine, "
                "or smaller than the dispersion of the benchmark's own paired ratios. "
                "Report the instruction count as the result and say plainly that the "
                "clock could not tell, or reproduce the figure in a second independent "
                "pass the way ADR-113's +1.4%/+2.0% costs were.",
                file=sys.stderr,
            )
        if caveats:
            # Last thing on the terminal, after the numbers, because that is
            # where it will actually be read.
            print("\nCAVEATS:\n  " + "\n  ".join(caveats), file=sys.stderr)
        if void_reasons:
            die("VOID measurement:\n  " + "\n  ".join(void_reasons))
    finally:
        shutil.rmtree(staging, ignore_errors=True)


if __name__ == "__main__":
    main()
