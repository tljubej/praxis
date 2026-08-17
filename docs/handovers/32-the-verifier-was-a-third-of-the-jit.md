# A third of every compile was Cranelift checking our work

**Date:** 2026-08-17, benchmark suite re-run 2026-08-18
**Status:** two changes landed, three hypotheses measured and rejected, one
trade priced and left alone. `just ci` green — 2570 tests, clippy clean,
`book-verify` 408 ok, and all 36 corpus programs byte-identical against the
release binary. `benchmarks/results.json` and `REPORT.md` are regenerated; §5 is
what the suite said, and §5.1 is the row that looked like a regression and is
not one.
**Scope:** the time `praxis run` spends *compiling*, from source text to a
callable entry point. Nothing here touches the code that gets generated or the
runtime that executes it — which is what every performance document before this
one is about ([21](./21-where-the-time-goes.md),
[25](./25-two-mallocs-per-runtime-call.md),
[28](./28-five-numbers-only-a-later-package-caught.md)).
**Build:** release, Apple M2 Pro / macOS 26.6, baseline `2d8d328` built from a
pristine `git archive` of that commit so the two binaries differ only by this
work. Wall clock is the minimum of 11 runs per program, arms interleaved per
program so drift hits both equally.

## The one-paragraph answer

`Jit::compile` is **83% of the compile pipeline** on the largest corpus program,
and **29% of that was the Cranelift IR verifier** — a check Cranelift enables by
default and whose own documentation says it is "useful during development". It
was running in the optimized binary a user runs programs with, and in the one
place it cannot catch anything a gate would not have caught first. Turning it
off in release builds is **1.47× on `Jit::compile`** and **1.19× on the wall
clock of the whole corpus**. A second change — a loop-invariant recomputation in
MIR liveness — takes `annotate` from 1.67 ms to 0.86 ms. Every corpus program is
faster, the slowest-improving by 1.028×, and every one still prints the same
bytes. On the benchmark suite the only column either change can reach is the
size-0 floor, and it falls **1.133×** on the geometric mean of the eight; the
timed columns are untouched, provably, because the two binaries emit identical
machine code for every one of those programs (§5).

## 1. Where the time goes

Per-phase timing of the full pipeline, `tests/aoc-corpus/aoc2025_day12.px` (180
lines, 17 functions), minimum of 30 iterations, before this work:

| phase | ms | share |
|---|---:|---:|
| `parse` | 0.10 | 0.5% |
| `analyze` | 0.39 | 1.9% |
| `hir_lower` | 0.16 | 0.8% |
| `monomorphize` | 0.05 | 0.3% |
| `mir_lower` | 0.95 | 4.7% |
| `mir_annotate` | 1.67 | 8.3% |
| `mir_verify` | 0.10 | 0.5% |
| `Jit::new` | 0.002 | 0.0% |
| **`Jit::compile`** | **16.75** | **83.0%** |
| total | 20.18 | |

Two things in that table are worth saying out loud before the finding.

**`Jit::new` is free.** The 64 MiB `ArenaMemoryProvider` reservation
([ADR-153](../decisions/153-a-modules-code-is-one-reservation.md)) costs 2 µs,
because it is `PROT_NONE` address space and not memory. The reservation is not a
thing to optimize away, and the debugger minting a `Jit` per `p EXPR` is not
paying for it either.

**The front end is not the problem.** Parse through MIR verify is 3.4 ms against
16.8 ms of backend. A compile-time investigation that starts anywhere but
Cranelift is looking in the wrong place.

Sampling that compile at 1 ms over an 8 s window attributes `Jit::compile` as:

| | samples | share of `Context::compile` |
|---|---:|---:|
| register allocation (`regalloc2`) | 1489 | 33% |
| **CLIF verifier** (`verify_context`) | **1317** | **29%** |
| egraph mid-end + the rest of `optimize` | ~800 | 18% |
| lowering, emission, everything else | ~890 | 20% |

Everything *outside* `Context::compile` — this crate building CLIF, plus
cranelift-frontend's SSA construction under `seal_all_blocks` — is about 420 of
the 4915 samples in `Jit::compile`, and under the node that holds
`define_function` it is 12 of 4510. The backend is Cranelift, all of it; the
only lever this tree has on the first, third and fourth rows is emitting less
IR.

## 2. The finding: `enable_verifier` defaults to on

`Context::compile` runs `verify_context` after the legalizer, after each mid-end
pass, and again before lowering. Cranelift's setting is declared with a default
of `true` and this comment:

> "This makes compilation slower but catches many bugs. The verifier is always
> enabled by default, which is useful during development."

`CRANELIFT_FLAGS` set `opt_level` explicitly and left everything else at its
default, so every `praxis run` ever measured has been paying for it.

`Jit::compile`, minimum of 25 iterations, both arms from **one binary** (an
environment override appended to `CRANELIFT_FLAGS`), so the two differ by the
flag and by nothing else:

| program | verifier on | verifier off | ratio |
|---|---:|---:|---:|
| `aoc2025_day09.px` | 21.40 ms | 15.26 ms | 1.403 |
| `aoc2025_day12.px` | 17.19 ms | 11.44 ms | 1.503 |
| `adr127_pipeline_over_every_iterable.px` | 19.21 ms | 13.13 ms | 1.463 |
| `rep15_iterating_every_collection.px` | 8.27 ms | 5.31 ms | 1.559 |
| `aoc2025_day11.px` | 3.65 ms | 2.49 ms | 1.468 |
| `day10_bfs_shortest_distance.px` | 2.61 ms | 1.75 ms | 1.494 |

**It costs no generated code.** The verifier reads the IR and never rewrites it,
and `PRAXIS_DUMP_VCODE=all` is identical between the arms once the immediates
holding ASLR'd runtime addresses are normalized — a normalization two runs of
the *same* arm also need, which is the control that says the difference found is
address nondeterminism and not the flag.

### Why the gate is `debug_assertions` and not `false`

What the verifier buys is a net that catches malformed CLIF *this crate* emits
before Cranelift miscompiles it. That net is worth keeping everywhere it could
fire:

- `cargo test --workspace` builds with debug assertions on — the whole suite,
  including `praxis-codegen-cranelift`'s 10.5k-line `jit.rs`, still verifies.
- `just ci` runs `book-verify` against `target/debug/praxis`, so the book's 408
  executed examples still verify.
- `scripts/asan.sh` runs `cargo test --release`, which would have made the
  nightly sanitizer job the one gate compiling *unverified*. It now exports
  `-C debug-assertions` alongside `-Zsanitizer=address`. That is a change this
  finding forced, and it also restores `debug_assert!` and integer-overflow
  checking to that job, neither of which `[profile.release]` was giving it.

Only the optimized binary a user runs a program with skips it.
`the_clif_verifier_is_on_exactly_when_debug_assertions_are` reads the flag back
off the ISA rather than off the constant, for the reason
`the_opt_level_flag_is_accepted_and_takes_effect` does: a stringly-typed setting
pair is accepted or rejected at run time, so the constant says what was asked
for and only the ISA says what governs the compilation.

## 3. The second finding: liveness recomputed a loop invariant

`dirty_in_fixpoint` called `block_roots` for every block on **every round** of
its fixpoint, and `compute_fixpoint` then called it once more per block for
itself. `block_roots` is a full backward walk that allocates one `BTreeSet` per
instruction, and its arguments — `live_in` and `gc_locals` — are both fixed
before the dirty fixpoint starts. It is computed once now, ahead of the
fixpoint, and shared.

`mir_annotate` on `aoc2025_day12.px`: **1.667 ms → 0.858 ms, 1.94×**. The MIR
suite (158 tests) passes unchanged, which is the assertion that the answer is
the same one.

This is 4% of the compile pipeline, not 29% — it is in this document because it
was free and because the profile put `block_roots` at 167 of `annotate`'s 231
samples, i.e. exactly where the arithmetic said it would be.

## 4. Before and after, end to end

Whole corpus, `praxis run` including process startup and the program's own
execution, minimum of 11 runs, arms interleaved, **stdout compared on every
program**:

| | HEAD `2d8d328` | this tree | ratio |
|---|---:|---:|---:|
| corpus total | 370.51 ms | 311.66 ms | **1.189×** |
| geometric mean of per-program ratios | | | **1.165×** |
| best (`adr149_grouping_a_sequence.px`) | 17.94 ms | 13.05 ms | 1.375 |
| worst (`m9_grid_and_commands.px`) | 3.96 ms | 3.85 ms | 1.028 |

Nothing regressed. The spread is Amdahl and nothing else: `aoc2025_day09.px`
gains 1.122× because it *runs* for 60 ms, and `m9_grid_and_commands.px` gains
1.028× because ~2.5 ms of its 4 ms is process startup neither change touches.

## 5. The benchmark suite: the floor moved, the runtime provably did not

`run.py` was re-run in full and `REPORT.md` regenerated. The suite's headline is
unchanged — **7× Rust, 0.2× CPython**, geometric means to the same figures as
before — which is the expected answer for a change that emits the same
instructions.

### The floor is the only column this change can reach

`run.py`'s floor pass is one run of the benchmark source at size 0: compile,
process start, and whatever size-independent setup the program does. Startup and
setup are identical between two binaries of the same tree, so the *difference* of
two floors is the compile-time difference and nothing else. Paired against a
pristine `2d8d328` build, `A,B,B,A` per rep with the leading arm alternating, 14
reps, minimum per arm:

| benchmark | HEAD | this tree | saved | ratio |
|---|---:|---:|---:|---:|
| `primes` | 5.41 ms | 4.96 ms | 0.45 ms | 1.091 |
| `mandelbrot` | 6.03 ms | 5.19 ms | 0.84 ms | 1.162 |
| `collatz` | 4.75 ms | 4.33 ms | 0.41 ms | 1.095 |
| `vm` | 10.30 ms | 8.44 ms | 1.87 ms | 1.221 |
| `hashwork` | 7.26 ms | 6.18 ms | 1.08 ms | 1.174 |
| `tree` | 23.97 ms | 22.72 ms | 1.24 ms | 1.055 |
| `pipeline` | 9.13 ms | 7.73 ms | 1.40 ms | 1.181 |
| `bfs` | 37.26 ms | 34.08 ms | 3.18 ms | 1.093 |
| **total** | **104.11 ms** | **93.64 ms** | **10.47 ms** | **1.112** |
| geometric mean | | | | **1.133** |

The ratios rank by how much of a floor is *compile* rather than setup, which is
why `vm` — 123 lines, the largest program in the suite — leads at 1.221× and
`tree` trails at 1.055× while still saving 1.24 ms. Absolute savings rank by
program size, and `bfs` saves the most at 3.18 ms.

### No `ab.py` sweep, because a deterministic answer was available

The timed columns were not A/B'd, and the reason is better than a clock: the two
binaries **emit identical machine code for all eight benchmark programs**.
`PRAXIS_DUMP_VCODE=all` compared byte for byte, after normalizing the immediates
that encode ASLR'd runtime addresses — with the same-binary comparison as the
control, which needs that normalization too. Identical instructions cannot run
differently, and a paired sweep could only have added noise around zero.

(`ab.py`'s quiescence gate would not have passed anyway: this machine idles at a
1-minute load average of ~1.9 against the tool's ceiling of 0.5, most of it the
desktop application this session runs in. Waiving the gate is what `--max-load`
is for and it is not what a measurement wants.)

### The sweep-to-sweep deltas are the machine, and the controls say so

`results.json` had not been regenerated since 2026-08-07, so the new file differs
by every commit in between as well as by this work. Four rows look slower than
the old ones. **They are not**, and the suite's own controls are the evidence —
the Rust binaries were not rebuilt (`run.py` rebuilds only on a newer source) and
CPython is the same interpreter, so any movement in those two columns is the
machine:

| benchmark | Rust | Python | Praxis |
|---|---:|---:|---:|
| `tree` | +4.0% | +4.7% | +5.4% |
| `pipeline` | +4.1% | +1.9% | +5.7% |
| `bfs` | +7.3% | +18.9% | +25.8% |
| `hashwork` | +0.4% | +1.9% | **+9.8%** |

The first three move together across all three languages, which is what
`README.md` warns about in as many words: with one binary in both arms this
machine's sweep-to-sweep drift ran 5–23% where the paired dispersion of the very
same runs was 0.7–4.0%. That is the whole reason per-package credit is assigned
by `ab.py` and not by subtracting one `run.py` table from the last.

**`hashwork` is the one row the controls do not explain** — its Praxis column
moved five times what either control did. It was chased, and it is **not a
regression**; §5.1 is the chase.

### 5.1 `hashwork` did not get slower, and the Aug 7 binary proves it

`b1887c4` is the commit that *wrote* the old `results.json`, so it is the exact
baseline rather than an approximation of one, and `rustc` is the same build
(1.97.1, `8bab26f4f`) in both `meta` blocks — a rebuild of it today is faithful.
Three answers, in increasing order of how little they depend on a clock.

**The machine code is identical.** `hashwork.px` compiles to byte-identical
vcode at `b1887c4` and at `e8e63d2`, normalized the way §5 describes. Ten days
of commits changed nothing about the program that runs, so whatever moved is
either the runtime library or the machine.

**Two independent paired passes say flat.** `ab.py`, palindromic, arm A the Aug 7
build and arm B this tree: **+0.4% ± 1.4%** over 6 reps and **+0.7% ± 1.4%** over
8, `primes` clean as a control in the second. Both are inside their own paired
dispersion and under the 2% floor — the clock cannot resolve them, and they
agree on the sign being *positive*, which is arm B faster. (The first pass is
stamped `void`: load crossed the raised ceiling after `hashwork`, so the sweep
stopped. It corroborates and is not quoted as the result. The second is `ok, with
caveats`, the caveat being the same waiver.)

**The Aug 7 binary is slower today than it was on Aug 7.** This is the one that
settles it, because it holds the code fixed and varies only the date:

| benchmark | peak RSS | Aug 7 record | same source, today | delta |
|---|---:|---:|---:|---:|
| `primes` | 12 MiB | 0.225 s | 0.232 s | +2.9% |
| `mandelbrot` | 11 MiB | 0.252 s | 0.267 s | +6.0% |
| `collatz` | 7 MiB | 0.123 s | 0.128 s | +3.9% |
| `vm` | 13 MiB | 0.968 s | 0.987 s | +2.0% |
| `hashwork` | 23 MiB | 1.759 s | **1.975 s** | **+12.3%** |

+12.3% on code that has not changed, against the +9.8% the sweep-to-sweep
comparison attributed to ten days of work. The whole delta is in the baseline
arm, and then some.

**What `hashwork` actually is, is the noisiest row in the suite.** Its paired
dispersion is ±1.4% where every other benchmark's is ±0.3–0.6%, and its
within-sweep drift ran **4.2% and 9.4%** in the first pass and **10.5% and 4.5%**
in the second — a spread the size of the "regression", in one arm, in one sitting.
`speedup_min` swung **+3.9% then −4.0%** across the two passes on the same pair of
binaries, which is the cleanest possible demonstration of why the headline here is
a median of paired ratios and not a `min`. `run.py`'s min-of-5 does not tame it:
the Aug 7 samples were 1.759–1.776 s and today's 1.932–1.990 s, each tight within
its own sitting and 10% apart between them.

**The carry-forward is the method, not the row.** A `run.py` sweep is a snapshot
of a machine on a day; `README.md` already says its drift ran 5–23% against the
paired 0.7–4.0%, and this is that sentence arriving as a concrete false alarm.
`hashwork` is the row where it will keep arriving, because it is the one with a
23 MiB working set and a SipHash in its inner loop on a laptop with a compositor
and an Electron application resident. A number quoted off two sweeps is a
hypothesis; `ab.py` is what turns it into a measurement.

## 6. Three hypotheses that did not survive measurement

Recorded so nobody spends the afternoon again.

**Reusing one Cranelift `Context` across functions: 0.0%.** `lower_function`
takes `module.make_context()` per function and drops it; holding one in `Jit` is
the idiom `cranelift-jit-demo` uses, and `clear_context` was already being
called, so the change was four lines. Measured from one binary with a toggle,
interleaved: 17.193 vs 17.224, 21.403 vs 21.526, 19.212 vs 19.099, 8.271 vs
8.101 ms — noise, in both directions. An earlier cross-build reading of "2–3%"
was drift, and is the reason the toggle exists. **The change was reverted.**

**The crash debugger's per-definition stores: 1.04–1.07×.** `store_debug_defs`
emits one store per `Gc` local per definition (ADR-104), and stores are 2127 of
~7000 CLIF instructions on `aoc2025_day12.px` — the largest single opcode by a
factor of 1.5. Suppressing them entirely measures 11.45→10.99 ms, 14.74→13.81 ms
and 5.36→5.06 ms. Cranelift handles a store cheaply. A `--no-debug` *compile*
mode would buy ~5% of compile time, which is not a feature's worth; §8 of
[28](./28-five-numbers-only-a-later-package-caught.md) already priced what the
debugger costs at *run* time (2.4% of the suite), and that remains the number
that matters.

**Anything in the front end.** 3.4 ms of 20.2, of which the biggest item is the
one §3 already fixed.

## 7. One trade, priced and not taken

`opt_level = "none"` is worth a further **1.14–1.23×** on `Jit::compile` against
the verifier-off baseline (`day12` 11.47→9.38 ms, `day09` 14.76→12.21 ms,
`rep15` 5.35→4.34 ms, `adr127` 12.75→11.18 ms).

It stays `"speed"`. `CRANELIFT_FLAGS`' own note prices the other side — suite
geometric mean 1.025×, `collatz` +16.5%, `tree` +4.2% — and a compile is paid
once while a loop is paid every iteration. The break-even is a program that runs
for about as long as it takes to compile, and the corpus is mostly on the other
side of that line. What *has* changed is that the flag's compile cost is no
longer "+0.1 ms on a 6.9 ms floor": against a floor that is now 40% lower, the
mid-end is ~2 ms of an 11.4 ms compile. If a `praxis run --fast` ever wants to
exist for scripts that compile more than they run, this is the knob, and these
are its numbers.

## 8. What is left, ranked

1. **Register allocation is 33% of Cranelift's time** and this tree cannot reach
   it except by handing Cranelift less IR. §6's store experiment says the
   debugger's stores are not the way in. Nothing else has been measured.
2. **Function-level parallelism.** `aoc2025_day12.px` is 17 functions and
   `Context::compile` is per-function; wasmtime compiles functions in parallel
   for exactly this reason. The blockers here are structural rather than
   algorithmic — `JITModule` is not `Sync`, `Generation` is behind an `Rc` with
   `RefCell` caches, and lowering takes `&mut TypeDb` — and the ceiling is set by
   the largest single function, which has not been measured.
3. Nothing else in the profile is above 5%.
