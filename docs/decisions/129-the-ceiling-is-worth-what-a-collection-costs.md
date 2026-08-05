# ADR-129: The ceiling is worth what a collection costs, and collections got cheaper

**Date:** 2026-08-05
**Status:** Accepted — implemented
**Milestone:** post-M11 performance
**Amends:** ADR-112's `MAX_COLLECT_THRESHOLD` only — the constant moves from
64 MiB to 4 MiB. **ADR-112's rule is not touched**, and neither is its decision
2: the ceiling still clamps the ratchet term and never the whole expression, so
a program whose live set exceeds the ceiling still gets its `live × k` headroom
and still cannot thrash. `LIVE_HEADROOM` stays at 2, and the Measurements say
why it was considered and rejected rather than left alone by omission.

**ADR-112's Measurements are not amended and not wrong.** They priced 8 MiB at
4% against the build that ran them, and that build no longer exists. This record
is the same sweep re-run, not a correction of it.

## Context

### The complaint

Peak resident set. Across the eight benchmarks Praxis peaked at **3.6× CPython's
resident set** while being 3× faster than it — 44 to 130 MiB where CPython used
15 to 55.

### Where it was, which is not where it looks

Peak RSS decomposes as **`floor + live set + collection threshold`**, and only
the third term is large:

* **The floor is 7.3 MiB.** `praxis run` on a size-0 program, whole process,
  including the JIT. CPython's floor — `python3 -c pass` — is **15.2 MiB**. The
  process Praxis starts is less than half the size of the one CPython starts.
* **The live sets are smaller than CPython's.** Driving the collector's headroom
  to its floor (`bounded:64K:1`, where the threshold is `live` and peak RSS is
  therefore `floor + 2 × live`) and backing the model out: `tree` holds 16.0 MiB
  against CPython's ~18.6, `bfs` 18.0 against ~20.3, `hashwork` 6.1 against
  ~8.3, `pipeline` 38.6 against ~39.8. §4.3's uniform boxing costs *less* here
  than CPython's 28-byte `int`, and this is the first measurement in the record
  that says so.
* **The threshold is the whole gap.** The four benchmarks that hold nothing at
  all — `primes`, `mandelbrot`, `collatz`, `vm` — peaked at 44, 73, 72 and
  74 MiB, which is the 64 MiB ceiling plus the process and nothing else. They
  were holding 64 MiB of garbage because the pacer had been told to.

So the memory was never in the representation. It was in one constant.

### Why the constant was 64 MiB, and why that stopped being right

ADR-112 swept 8/16/64/256 MiB and put the knee at 64: 8 MiB cost 4% and 64 MiB
cost 1.2%, for a 14.4× memory saving against the unbounded doubling pacer it was
replacing. That was a correct reading of a correct measurement.

It went stale for the reason ADR-112 itself named. Its prediction — written down
before its numbers were read — was that *total sweep-and-relink work is
independent of the ceiling*, because collections scale as `total/ceiling` while
pages walked per collection scale as `ceiling/PAGE_SIZE`. What is left is the
**per-collection fixed cost**, and that is the only thing a ceiling prices.

Two of the packages since then cut exactly that cost:

* **ADR-114** took two `malloc`s out of the rooting call.
* **ADR-128** narrowed the frame that `push_roots` scans *at every collection*,
  taking `tests/aoc-corpus`'s summed declared width from 1925 slots to 216.

Neither was aimed at the pacer. Both make a collection cheaper, and a ceiling is
a bet about how much a collection costs. Halving the price of collecting halves
what the ceiling is buying, so the knee moved — and nothing in the tree would
have said so, because the constant carries its own justification and that
justification cited a sweep nobody re-ran.

**That is the generalizable finding, and it is worth more than the constant:** a
constant chosen at a knee is only valid while the curve it sat on is. This one
outlived its curve by two packages.

## Decision: `MAX_COLLECT_THRESHOLD` is 4 MiB

`INITIAL_COLLECT_THRESHOLD << 6` rather than `<< 10`. One constant; every
reference to it in the workspace is symbolic, so nothing else moves.

**Why 4 MiB and not 8.** 8 MiB costs 1.6% and lands the suite at 1.28× CPython;
4 MiB costs 1.9% and lands it at **1.09×**. The marginal 0.3% buys the whole
remaining distance to parity. The time is being spent either way.

**Why 4 MiB and not lower.** Below 4 MiB the ceiling stops being the binding
term: the only benchmarks still above CPython are the three whose *live set*
dominates (`pipeline`, `bfs`, `tree`), and by decision 2 of ADR-112 the ceiling
cannot touch those — their threshold is `live × 2` and the ceiling is not in the
expression. 2 MiB therefore keeps raising collection frequency for every program
in the language while buying almost nothing: ~0.98× CPython for a further ~2%,
where the four ceiling-bound benchmarks move only 11.4 → 9.3 MiB.

**Why `LIVE_HEADROOM` stays 2.** It is the other knob and it is the worse one.
Measured at a fixed 8 MiB ceiling, `k = 2 → 1` costs **4.3%** — `pipeline` alone
−18.1% — and moves the geometric mean of peak RSS by 1.1×, because for the four
low-live benchmarks `live × k` never binds and their memory does not move at
all. It buys ~0.1× of the CPython ratio for more than twice what the entire
ceiling change costs. ADR-112 priced `k` upward (2 → 4) and this prices it
downward; both directions are now measured and 2 stands.

## Measurements

Apple M2 Pro, 16 GiB, macOS 26.5.2, release build. **One binary**, both arms
selected by `PRAXIS_GC_PACER` (ADR-112 decision 4), through
`benchmarks/pacer_ab.py`: 5 reps, the two arms back to back within a rep, which
arm leads alternating between reps, minimum per arm, every run's stdout gated on
`results.json`'s checksum. All arms `ok`.

**One caveat, stated because the protocol asks for it.** These ran at a 1-minute
load average of ~2.3, above `ab.py`'s 0.5 quiescence gate — a browser, not a
compiler. The interleaved palindrome is what that gate's absence is survived by,
and the two ceiling arms were measured in independent sweeps that agreed (1.6%
and 1.9%, with the same per-benchmark shape). **Peak RSS is load-independent and
is the deliverable here**; the percentages are the ones to re-take on a quiet
machine if anything turns on them.

### The two ceiling arms, against today's 64 MiB

| benchmark | 64 MiB (A) | 8 MiB | 4 MiB | time 8M | time 4M | RSS 4M |
|---|---:|---:|---:|---:|---:|---:|
| `primes` | 43.7 MiB | 15.3 | 11.4 | 1.014× | 1.011× | 3.8× less |
| `mandelbrot` | 73.0 MiB | 15.9 | 11.8 | 1.026× | 1.035× | 6.2× less |
| `collatz` | 72.4 MiB | 15.1 | 10.9 | 1.025× | 1.037× | 6.6× less |
| `vm` | 73.8 MiB | 16.5 | 12.4 | 1.028× | 1.030× | 5.9× less |
| `hashwork` | 84.0 MiB | 24.9 | 23.3 | 0.936× | 0.882× | 3.6× less |
| `tree` | 93.8 MiB | 51.9 | 51.9 | 0.946× | 0.943× | 1.8× less |
| `pipeline` | 130.4 MiB | 109.3 | 109.4 | 0.940× | 0.930× | 1.2× less |
| `bfs` | 110.5 MiB | 60.9 | 60.5 | 0.966× | 0.998× | 1.8× less |
| **geometric mean** | | | | **0.984×** | **0.982×** | **3.3× less** |

Above 1.00× the lower ceiling is faster.

**The cost is 1.9% and its distribution is the result.** The four benchmarks that
hold nothing are *faster* at 4 MiB — 1.1% to 3.7% — as well as ~6× smaller,
which is the first-touch page-fault saving ADR-112 measured at the 64 MiB step,
continued down the same curve. Every one of the 1.9% is paid by the four that
hold something, and paid during their *growth* phase, where live is still small
and the ceiling rather than `live × 2` is setting the schedule.

### Against CPython 3.14, which is what opened this

| | `primes` | `mandelbrot` | `collatz` | `vm` | `hashwork` | `tree` | `pipeline` | `bfs` | **geo** |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| CPython | 14.6 | 14.7 | 14.6 | 15.0 | 23.5 | 33.8 | 55.0 | 35.5 | |
| was (64 MiB) | 2.99× | 4.97× | 4.96× | 4.92× | 3.58× | 2.78× | 2.37× | 3.11× | **3.57×** |
| now (4 MiB) | 0.78× | 0.80× | 0.75× | 0.83× | 0.99× | 1.53× | 1.99× | 1.70× | **1.09×** |

Five of the eight now peak **below** CPython. The three that do not are the three
holding a large live set, and what is above CPython there is the collector's
`live × 2` headroom against a refcounting implementation that frees at zero —
which is a property of tracing collection, not a defect, and is priced above.

### The knob that was rejected

`k = 2 → 1`, ceiling fixed at 8 MiB, same protocol:

| | `primes` | `mandelbrot` | `collatz` | `vm` | `hashwork` | `tree` | `pipeline` | `bfs` | **geo** |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| time | 1.001× | 1.002× | 1.004× | 0.998× | 0.999× | 0.896× | 0.819× | 0.970× | **0.959×** |
| RSS | 1.0× | 1.0× | 1.0× | 1.0× | 1.0× | 1.3× | 1.4× | 1.3× | **1.1×** |

Four benchmarks do not move at all in either column, which is `live × k` not
binding, exactly as decision 2 says. **4.3% for 1.1×** is the trade, against the
ceiling's 1.9% for 3.3×.

## What this does not change

* **The rule.** `max(min(previous × 2, ceiling), live × k, INITIAL)` is
  unchanged, and so is every test that pins its shape.
* **Decision 2.** The ceiling clamps the ratchet only. A program holding more
  than 4 MiB is paced by `live × 2` and never sees this constant — which is what
  makes lowering it safe by construction rather than by measurement, and is why
  `a_bounded_pacer_gives_a_large_live_set_its_headroom` needed no change.
* **`Pacer::Doubling`** remains in the tree as the named, tested statement of the
  rule ADR-112 amended.
* **§4.3.** No language semantic moves. Boxing is untouched, and the live-set
  measurement above is an argument that boxing is not what the memory column was
  ever about.
