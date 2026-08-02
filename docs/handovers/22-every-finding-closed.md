# Every finding closed — 4.6× CPython becomes parity, and the report was wrong twice

**Date:** 2026-08-02
**Status:** all six findings of
[`21-where-the-time-goes.md`](./21-where-the-time-goes.md) are implemented and on
`main`
**Scope:** the runtime and the code the JIT generates
**Supersedes:** handover 21's §4 ordering and its §1 estimate
**Regenerates:** `benchmarks/REPORT.md` from a real `run.py` at `sizes.json`
sizes

> Handover 21 predicted that its two prototyped changes were worth ~2.7×, and
> that "4.6× CPython becomes roughly 1.9× and 185× Rust roughly 75×". All six
> findings landed. The suite is now **1.0× CPython and 39× Rust**, and peak
> resident set fell by 2–13× per benchmark. The estimate was low because it
> priced two of six items.

## 1. What landed

Six commits, each with `just ci` green, each measured on its own. The test count
went 1780 → 1841 with no test deleted or weakened.

| | commit | ADR | ABI |
|---|---|---|---|
| §3.1 free list | `fe8a7af` | — | — |
| §3.5 literal re-boxing | `4125471` | 100 | 15 |
| §3.3 shadow stack | `488330f` | 101 | 16 |
| §3.4 inline per-op calls | `771a8d8` | 102 | 17 |
| §3.6 `live` registry | `29f7e75` | 103 | — |
| §3.2 debug frame | `1786148` | 104 | 18 |

Nothing in §4.3 changed. No language semantic changed. The debugger's rendered
output is byte-identical before and after, and `crates/praxis-debugger` has no
changes at all.

## 2. The suite, at `sizes.json` sizes

`benchmarks/results.json` before and after, both produced by `run.py --reps 3`,
which refuses to time a benchmark whose three implementations disagree.

| Benchmark | before | after | | vs CPython | vs Rust | peak RSS |
|---|---:|---:|---:|---:|---:|---:|
| `primes` | 13.24 s | **1.54 s** | 8.6× | 3.94 → **0.44** | 205 → **23** | 6.67 → **0.94** GiB |
| `collatz` | 16.26 s | **1.30 s** | 12.6× | 6.17 → **0.48** | 388 → **30** | 6.51 → **0.49** GiB |
| `mandelbrot` | 13.63 s | **3.00 s** | 4.5× | 5.64 → **1.19** | 227 → **48** | 7.02 → **3.19** GiB |
| `tree` | 15.75 s | **3.28 s** | 4.8× | 4.31 → **0.80** | 182 → **36** | 5.18 → **2.06** GiB |
| `vm` | 19.90 s | **7.10 s** | 2.8× | 2.51 → **0.86** | 176 → **62** | 5.88 → **1.03** GiB |
| `pipeline` | 17.93 s | **4.05 s** | 4.4× | 6.92 → **1.52** | 282 → **60** | 6.10 → **3.13** GiB |
| `hashwork` | 13.99 s | **6.51 s** | 2.1× | 2.94 → **1.30** | 44 → **19** | 6.06 → **2.05** GiB |
| `bfs` | 27.18 s | **9.64 s** | 2.8× | 6.85 → **2.19** | 194 → **62** | 6.13 → **1.14** GiB |
| **geomean** | | | **3.8×** | **4.62 → 0.96** | **185 → 39** | |

Per-call overhead went from 120 ns to roughly 10.

## 3. Where handover 21 was wrong

Its measurements all stand. Three of its *claims* did not.

### 3.1 §3.2's premise is false, and not for the reason it anticipated

Handover 21 §3.2 proposed reconstructing the crash debugger's view from the
shadow frame at fault time: "the shadow frame already holds the live values at
every safepoint. The debug frame's `value` slots are a second copy of the same
words."

They are not. It is not only that `RootSlots::dead` nulls slots the debugger
still needs — the fidelity loss the handover half-anticipated. It is that
`liveness::block_roots` computes the root set as live-*before* an instruction, so
an `Alloc`/`Materialize`/`Call` destination is **by construction** excluded from
its own safepoint's root set. `liveness.rs` says so in its module doc: "the
destination is written after the collection so it is not rooted." A temp
materialized and consumed before the next safepoint lives only in a Cranelift
register and in the debug frame. Reconstruction cannot see it. Ever.

What worked instead is in ADR-104: store once per *definition* rather than once
per safepoint, and make the frame two `SlotStack` claims off the mechanism §3.3
had already built. Zero fidelity loss, and it is *more* renderable than before,
not less.

### 3.2 §3.2's "18–24%" is two costs on two program shapes

Neither of the two changes alone reaches the band.

`collatz.px` is a file of top-level statements, and `praxis_hir::lower` folds
them into one synthetic entry function — so the whole 6.5M-iteration run performs
**one** debug-frame push and one pop. Its share cannot be malloc; it is ~170
stores per inner iteration at a 48-byte stride through a pointer reloaded at
every site. The 5M-no-op-call row is the mirror image: 68 ns per call, all
prologue. Measured after the fact, stage one moved the no-op call by 0.00 s over
20M calls, and stage two moved `collatz` by 0.02 s.

### 3.3 §3.6's profile was stale before it was written

Handover 21 §3.6 says that with §3.1 and §3.2 applied, `Heap::alloc_raw` is 36%
and `Heap::collect_inner` 24%. Re-sampled on the tree §3.6 actually branched from
— after §3.1, §3.3, §3.5 *and* §3.4 — they were **6.1% and 6.6%**.

So the page allocator's wall-clock return is modest: flat on `collatz`, ~1.1× on
`hashwork` and `mandelbrot`, 1.16× on `bfs`, with the collector's own share down
31% (sweep 6× cheaper, the bitmap claim ~25% dearer than popping a `Vec`). The
half of that finding that arrived in full is the memory ceiling — and it arrived
**without** the `GcHeader` shrink the plan said it would need.

The general lesson: a ranked list of findings is measured against one tree, and
every item you fix invalidates the ranking of the items below it. Re-profile
between items.

## 4. §3.7 is still negative, and now has a price

`opt_level = "speed"` was tried again after §3.4, which is the point handover 21
nominated. Every workload landed within ±3% in both directions — noise — and the
floor pass (size 0, i.e. codegen time) regressed on all eight benchmarks: `bfs`
+4.7 ms, `vm` +4.4 ms, `tree` +3.5 ms. Reverted, with a comment at
`Jit::in_generation` and the measurement in §3.7 so it is not tried a third time.

The remaining explanation: allocation is still `praxis_alloc_*`, still an opaque
call and still a memory clobber, so Cranelift's mid-end still cannot move
anything across a loop body. Expect this to stay negative until an inline
allocation fast path exists.

## 5. What is deliberately not done

- **`Inst::Alloc`'s inline fast path** (handover 21 §3.4, row 4). It bakes
  `Heap`'s field offsets into generated code, and §3.6 changed every one of them.
  It is also the change that puts ADR-040's `Safepoint` token into generated
  code, which needs its own decision record. Now cheap to do: allocation is a
  bitmap claim, six instructions.
- **`GcHeader` at 8 bytes** (§3.6's C4). Costs an ABI bump and moves an immediate
  generated code bakes in. The natural end state is pages segregated by
  *descriptor* — the descriptor set is closed at 22 — which deletes `GcHeader`
  entirely and would make C4 wasted work. Decide *that* before doing this.
- **Bounding the pacer.** Untouched. `collect_threshold` still doubles without
  limit, so peak RSS is still a function of runtime rather than of live data.
  The constant moved; the shape did not.

## 6. Two things found on the way that are not performance

Both are pre-existing and neither was caused by this work.

- **`MAX_RECURSION_DEPTH` takes no account of native frame size.** It is a call
  count. A function with 20 live collections (112 Gc locals, all spilled to its
  native frame) aborts the host with SIGABRT at depth ~2700 under a 2 MiB stack —
  the exact failure the guard exists to prevent. Measured identical on pre- and
  post-§3.3 binaries. Recorded in ADR-101's Consequences. Relatedly,
  `MAX_SHADOW_SLOTS = 192` allows only ~34 user-level collections per function.
- **The debug values are not a `RuntimeRoots` arm.** A value `RootSlots::dead`
  nulled but a debug slot still names can be swept before the fault, leaving the
  snapshot to copy a dangling `GcRef`. True on `main` before this work for exactly
  the same values. The two-line fix — a sixth arm over `[base, top)` — is the
  set-merge ADR-044 deliberately refuses, unless the values are rooted *weakly*,
  so it needs its own decision and its own measurement against
  `a_dead_local_stops_being_reachable_from_its_frame`. Recorded in ADR-104's
  Consequences.

Four tests were also found to be passing for the wrong reason: each detected a
collection by watching the live registry shrink (`after < before + 1`), which an
interned `Int` satisfies on iteration zero without any collection having run.
Repaired in `4125471`.

## 7. Caveats

- One machine (Apple M2 Pro / 16 GiB / macOS 26.5.2), best-of-3, no CPU pinning,
  no frequency locking. Every A/B in this document interleaved its two binaries.
- `bfs` at reduced size is too noisy for min-of-3 to be meaningful (observed
  spread 5.2–7.6 s on one binary). Its `sizes.json` row in §2 is from `run.py`
  and is sound; do not read a reduced-size `bfs` delta as a signal.
- `benchmarks/gcfix.json` has been renamed `gcfix-pre-perf-fixes.json`. It was
  measured against `fd70374` and `report.py` renders the appendix by comparing it
  row-for-row against whatever is in `results.json` — so leaving it in place would
  have the report attribute this suite's changes to a pacer experiment that never
  ran against this build. See `benchmarks/README.md`.
- `sizes.json` was tuned for a 6 GiB peak. These workloads now peak at 0.5–3.2
  GiB and the sizes have not been re-tuned, so every Rust column in `REPORT.md` is
  smaller than it needs to be.
