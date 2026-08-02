# Where the time actually goes — 2.5× before any unboxing

**Date:** 2026-08-02
**Status:** investigation only; no code changed on `main` by this document
**Scope:** the code the JIT generates and the runtime it calls, on the
`benchmarks/` suite
**Contests:** `benchmarks/REPORT.md` §"What would move these numbers" and its
closing sentence

> `benchmarks/REPORT.md` closes with: *"So the gap in the main table is not the
> collector's to give back. It is the boxing, and only item 1 above reaches
> it."* That conclusion does not survive a profile. Its **measurements** are not
> in dispute — the ratios, the floor column and the pacer appendix all stand.
> What is wrong is the attribution.

## 1. The finding

Boxing is what makes Praxis *allocate*. It is not what makes each allocation
cost ~40 ns. Sampling `collatz` — the benchmark whose inner loop is arithmetic
and nothing else, the one the report calls the purest measurement of boxing —
puts **only 19% of runtime inside JIT-generated code**. The other 81% is
runtime bookkeeping, and most of it is not required by a uniform boxed
representation.

Two changes, neither touching §4.3, neither altering the value model, neither
changing a single language semantic, were prototyped and measured:

| | baseline | + free-list fix | + debug frame off | both | speedup |
|---|---:|---:|---:|---:|---:|
| `collatz` @ 60,000 | 2.35 s | 0.86 s | 1.93 s | **0.87 s** | 2.7× |
| `primes` @ 300,000 | 1.25 s | 0.60 s | 1.20 s | **0.60 s** | 2.1× |
| 10M × `i = i + 1` | 0.78 s | 0.34 s | 0.75 s | **0.35 s** | 2.2× |
| 5M × no-op call | 1.43 s | — | 1.09 s | **0.58 s** | 2.5× |

Per-call overhead falls from **120 ns to 52 ns**. If the factor holds across the
suite, 4.6× CPython becomes roughly 1.9× and 185× Rust roughly 75× — before one
line of unboxing, interning, tagging or escape analysis.

The report's item 1 (stop allocating for scalars) is still the largest single
win available. It is not the *first* one available, and the suite as it stands
does not measure what the report says it measures: `collatz` at 388× Rust is
about one third boxing and two thirds a hash lookup and a debugger.

## 2. Method

Release build at `fd70374`, Apple M2 Pro / 16 GiB / macOS 26.5.2. Timing is
`/usr/bin/time -p`, three runs, minimum reported; workload sizes reduced from
`sizes.json` so a run fits in seconds rather than gigabytes. Profiles are
`/usr/bin/sample` at 1 ms over a 3–4 s window, attributed leaf-first.

Each experiment was applied alone, measured, and **reverted** before the next.
Nothing from this investigation is in the tree.

## 3. The findings, ranked by measured value

### 3.1 The free list is a SipHash-keyed `HashMap`, probed on every allocation

**~34% of runtime.** `heap.rs:83` declares

```rust
free: RefCell<std::collections::HashMap<BlockLayout, Vec<NonNull<u8>>>>,
```

with the default `RandomState`. `Heap::alloc_raw` probes it per object
(`heap.rs:366`) and `Heap::sweep` files every reclaimed block back through it
(`heap.rs:554`) — so every boxed integer pays a SipHash of a 16-byte key
**twice**, plus a hashbrown probe, plus the `RefCell` borrow.

The profile is unambiguous: `core::hash::sip::Hasher::write` and
`hash_one::<BlockLayout>` together are 34% self time on `collatz` and 33% on
`primes`, ahead of everything else including the generated code.

A program uses single digits of distinct block layouts. The hash buys nothing.

**Fix.** Index by size class, or linear-scan a small `Vec<(BlockLayout,
Vec<_>)>`. The prototype was the latter, four lines, and produced the
free-list column above. RT-01's soundness argument is untouched: it rests on
the exact `(size, align)` match, not on how the bucket is found.

### 3.2 The crash debugger's bookkeeping runs in every `praxis run`

**~18–24%.** Every generated function call pushes a *second* heap frame.
`praxis_push_debug_frame` (`debug.rs:80`) builds a `Vec<DebugLocal>`, converts
it with `into_boxed_slice` + `mem::forget`, then `Box::new(DebugFrame)` — two
to three mallocs — and the prologue makes a third extern call to
`praxis_set_frame_source_span`. Then `SpillCtx::spill_debug`
(`lower.rs:380`) emits a **second** store sequence at every safepoint, over a
deliberately over-approximate local set, into that frame.

None of it is read unless the program faults. Removing it entirely takes
`collatz` 2.35 → 1.93 s and 68 ns off every call.

**That deletion is a measurement, not a proposal** — it removes a shipped
feature. The fix is to stop maintaining the view continuously:

- The shadow frame already holds the live values at every safepoint. The
  debug frame's `value` slots are a second copy of the same words.
- `DebugLocalMeta` is static per function and already interned in the
  generation arena.

So the debugger's view can be **reconstructed** from the shadow frame at fault
time rather than maintained ahead of a fault that almost never comes. Failing
that, gate the whole chain on `--debug` and compile two variants. MIR-16 is the
reason the two spills are separate today; it argues for two *sets*, not for
paying for both at every safepoint.

### 3.3 The shadow frame is a 1552-byte `Box`, malloc'd and zeroed per call

**~32 ns/call.** `MAX_SHADOW_SLOTS = 192` (`shadow_frame.rs:46`), and
`ShadowFrame::new` (`shadow_frame.rs:73`) allocates `[*mut GcHeader; 192]` —
1536 bytes memset to null on every call, whatever the function's real
`slot_count`, freed on return. Dropping the constant to 24 alone took the
no-op-call benchmark 1.43 → 1.27 s.

**Fix.** A contiguous shadow *stack* in `RuntimeContext`: bump a pointer by the
function's real `slot_count`, zero only those slots, restore on return. The
frame stops being an allocation. Generated code can do the bump inline, which
also removes two of the ~5 extern calls per Praxis call and their
`catch_unwind` landing pads.

### 3.4 Per-op, the JIT emits calls where one or two instructions would do

The arithmetic core is already right: `Inst::IntBinOp` emits an inline `iadd`
and an inline overflow predicate. Everything *around* it is an out-of-line call
into Rust, and each callee is wrapped in `std::panic::catch_unwind` by
`abi_guard!` (`abi.rs:464`):

| Site | Emitted today | Should be |
|---|---|---|
| `Inst::ExtractScalar` (`lower.rs:711`) | call `praxis_int_load` | `load [ref + payload_offset]` — a `const fn` the backend **already** inlines for enum tags at `lower.rs:1200` |
| overflow report (`lower.rs:1441`) | **unconditional** call to `praxis_raise_int_overflow_if`, per op | `brif` to a cold block |
| `Inst::CheckFault` (`lower.rs:1096`) | call `praxis_check_fault` to read one flag out of the context | inline load + `brif` |
| `Inst::Alloc`/`Materialize` | call `praxis_alloc_int` → `catch_unwind` → build `RuntimeRoots` → `maybe_collect` → `Heap::alloc` | inline fast path (counter under threshold, free block available), slow path out of line |

The comment at `lower.rs:1441` states the unconditional-call choice
deliberately — "the call is unconditional and the wrapper decides" — and its
stated benefit, that arithmetic stays one basic block, is real. It is also what
a `brif` to a cold block buys without the call.

### 3.5 Every literal is re-boxed on every evaluation

`lower_lit_gc` (`build.rs:1481`) emits `ConstInt` + `AllocKind::Int` per
`Lit::Int`, so `i + 1` inside a loop heap-allocates the `1` on **every
iteration**.

Measured directly in source, no compiler change: hoisting the literal into a
`let` outside the loop makes a 10M-iteration loop **36% faster** (0.78 →
0.48 s).

**Fix.** LICM on MIR for invariant scalar boxes, or intern constants once per
generation. §4.3 already reserves interning small integers. Worth first
confirming nothing in the language can observe `Int` identity — if nothing can,
this is free.

### 3.6 After those, the `live` registry is what is left

With 3.1 and 3.2 applied, the `collatz` profile is `Heap::alloc_raw` 36% and
`Heap::collect_inner` 24%. That is the side registry: a
`Vec<NonNull<GcHeader>>` pushed per allocation, grown to hundreds of millions
of entries under the doubling pacer, walked in full by every sweep. It is also
8 bytes of overhead per object — 25% on top of a 32-byte `Int`.

**Fix.** Segregated size-class pages with mark bitmaps. Allocation becomes a
free-pointer pop with no registry write; sweep becomes a bitmap scan; `size`,
`payload_offset` and `heap_id` move to per-page metadata, taking `GcHeader`
from 24 bytes to 8 and an `Int` from 32 bytes to 16.

That halves peak RSS as a side effect, which is the *other* half of the
report's problem — and unlike the pacer experiment in the report's appendix, it
does not trade time for it.

**Implemented (ADR-102), and the prediction was half right.** Size-class pages
with allocated/mark bitmaps landed; `bumpalo`, the `live` registry and the mark
byte are gone. The header shrink was deliberately *not* done — it costs an ABI
bump and moves an immediate generated code bakes in, and descriptor-segregated
pages would make it wasted work.

The memory half came in as predicted, without the header shrink: peak RSS falls
**1.3× to 1.8×** across the suite (`mandelbrot` 1032 → 573 MiB, `vm` 253 → 138,
`tree` 787 → 518, `bfs` 764 → 577, `collatz` 111 → 72).

The time half did not, because **this section's profile had already expired when
3.1, 3.3 and 3.5 landed.** Re-profiled on the tree this change went onto,
`collatz` reads `Heap::alloc_raw` **6.1%** and `Heap::collect_inner` **6.6%** —
not 36% and 24%. Against that baseline the rewrite takes `collect_inner` to
**1.1%** (six times cheaper: a word of `allocated & !mark` per 64 blocks instead
of a walk over every live object, twice) and puts allocation up to **7.7%** (a
bitmap claim is a little dearer than popping a `Vec`). Net collector time on
`collatz` falls 31%, and wall clock moves 0–16%: `bfs` 1.16×, `hashwork` 1.10×,
`mandelbrot` 1.10×, `10M i = i + 1` 1.11×, `primes` 1.04×, `collatz`/`tree`/
`pipeline`/`call` unchanged. The lesson for whoever reads this next: **re-take
the profile before believing a percentage from an earlier section of the same
document.**

### 3.7 Negative result: `opt_level` is `none`, and raising it does nothing

`JITBuilder::new` is used with no flags set anywhere in the workspace, so
Cranelift runs at its default `opt_level = "none"`. Setting `opt_level =
"speed"` produced **no measurable change** on any benchmark tried.

Worth writing down so it is not tried twice: the mid-end has nothing to work
with when a loop body is a chain of opaque calls into Rust. This becomes worth
revisiting only *after* §3.4.

**Re-tested after §3.4 landed (ADR-102), and the result is unchanged.** The
premise above was the explanation, so removing the opaque calls should have
changed the answer. It did not. With `ExtractScalar`, the overflow report and
`CheckFault` all inlined, `opt_level = "speed"` against the same tree, all eight
benchmarks plus the two microbenchmarks, interleaved, min of three:

| | none | speed |
|---|---:|---:|
| `collatz` @ 60,000 | 0.227 s | 0.230 s |
| `primes` @ 300,000 | 0.191 s | 0.192 s |
| 10M × `i = i + 1` | 0.300 s | 0.305 s |
| 5M × no-op call | 0.342 s | 0.332 s |
| `tree` @ 60 | 1.398 s | 1.387 s |
| `hashwork` @ 800,000 | 0.727 s | 0.716 s |
| `vm` @ 400,000 | 1.211 s | 1.214 s |
| `mandelbrot` @ 200 | 0.901 s | 0.905 s |
| `pipeline` @ 200,000 | 1.381 s | 1.407 s |
| `bfs` @ 80 | 5.551 s | 5.527 s |

Every row is within ±3%, in both directions, which is this laptop's noise.
Meanwhile the floor pass (size 0 — compile, start, fixed setup) grows on every
benchmark: `bfs` +4.7 ms, `vm` +4.4 ms, `tree` +3.5 ms, the rest under 1 ms. So
`speed` is a small, strictly one-directional cost. **Reverted; the flag is not
set and `module.rs` now says why.**

What is left of the original explanation is that the *remaining* calls still act
as memory clobbers — every allocation is still `praxis_alloc_*`, and `collatz`'s
loop still allocates — so the mid-end still cannot move much across a loop body.
That predicts this stays negative until §3.6 changes what allocation costs, and
it should not be tried again before then.

## 4. What to do first

1. **§3.1** — a few lines, ~2.5× on allocation-heavy code, no design question.
2. **§3.3** and **§3.5** — small, self-contained, no semantics affected.
3. **§3.2** — the biggest design question here. A shipped feature is being paid
   for continuously by every program that never uses it.
4. **§3.4** — the most work, and what unlocks §3.7.
5. **§3.6** — a collector rewrite, and the one that also fixes the memory
   ceiling.

## 5. Caveats

- Measured on `collatz`, `primes` and microbenchmarks at **reduced sizes**, not
  the full suite at `sizes.json` sizes. Nothing here should be transcribed into
  `benchmarks/REPORT.md` without re-running `run.py` against a real patch.
- Three runs per configuration on an otherwise-idle laptop; no CPU pinning.
- The 2.1–2.7× column pairs a legitimate fix (§3.1) with a feature deletion
  (§3.2). It is an upper bound on what those two areas are worth, not a patch
  anyone should apply as written.
- Sample-based attribution across the JIT boundary is imperfect; frames inside
  generated code are reported as `???`. The ranking was stable across
  benchmarks and the A/B timings corroborate each attribution independently.
