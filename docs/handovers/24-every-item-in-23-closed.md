# Handover 24 — the two defects are fixed, the question is answered, and what the deferrals were actually worth

**Date:** 2026-08-03
**Predecessor:** [`23-what-is-left-after-the-performance-work.md`](./23-what-is-left-after-the-performance-work.md)
is the list this closes. Read it first only if you want the *before*; everything
it left open is accounted for below.

## The one-paragraph answer

Handover 23 left two correctness defects, four deferred performance items and
one design question. **Both defects are fixed**, and both turned out to be worse
than 23 described: the recursion guard's failure reproduces as SIGABRT on the
stack the *test suite itself* runs on, and the debugger's use-after-free
reproduces not as a dangling read but as a well-formed, wrong value rendered
under the user's own variable name. **The design question is answered and
closed** — pages stay segregated by size class, and for a reason nobody had
written down. Of the four deferrals, three are done and one is partly done. Nine
new decision records land with them: ADR-105 through ADR-113.

The largest measured win came from the item handover 23 ranked last and
recommended against.

## What it is worth, end to end

The `54d2d9a` binary and this one, run interleaved in palindromic order
(A,B,B,A, so a session's linear drift cancels rather than landing on whichever
ran second), full `sizes.json` sizes, best of five, **every checksum identical**:

| benchmark | time | peak RSS | Rust drift |
|---|---:|---:|---:|
| `primes` | **1.26×** | 13.4× less | +0.9% |
| `mandelbrot` | **1.15×** | 44.9× less | −0.6% |
| `collatz` | **1.23×** | 7.0× less | +6.8% |
| `vm` | 1.01× | 14.3× less | +0.4% |
| `hashwork` | **1.30×** | 26.1× less | +1.9% |
| `tree` | 1.03× | 23.0× less | −0.0% |
| `pipeline` | 1.00× | 25.4× less | −0.4% |
| `bfs` | 1.08× | 11.2× less | −4.9% |
| **geometric mean** | **1.13×** | **17.8× less** | |

**Read the last column before the first.** It is the *same Rust binary* measured
in both arms — `run.py` caches the rustc output by mtime, so nothing about it
changed — which makes it a free drift meter for the machine underneath. It says
`collatz`'s 1.23× is if anything understated (the machine was 6.8% slower during
the arm that won) and `bfs`'s 1.08× is flattered by about the same. Handover 23
§5 warns that this laptop drifts several percent over a few minutes; this is what
that looks like, and it is why the ordering is a palindrome.

Against CPython 3.14 the suite moves from 0.9× to **0.8×**, and against Rust from
35× to **32×**.

---

## 1. The two defects

### D-1 — fixed (ADR-105)

`MAX_RECURSION_DEPTH` counted calls; what runs out is bytes. Measured on this
backend by bisecting the abort depth under `ulimit -s`, a native frame is
`99 + 1.06 × gc_locals` bytes — 86 B for a minimal frame, 294 B for one holding
twenty-two live collections. A count calibrated for the first is not safe for the
second.

**23 understated the reach.** It framed the failure as needing `ulimit -s 2048`.
But `praxis run` calls the JIT entry on the process main thread (8 MiB on macOS)
while `cargo test` runs every JIT test on a libtest thread that passes no
`stack_size` — std's 2 MiB default. The smaller of the two stacks Praxis actually
runs on is the one the whole test suite lives on.

The prologue now spends a *byte* budget it finds in the context, counting down:

```
load    left, [ctx + STACK_LEFT_OFFSET]
icmp    left < COST            ; this function's cost, folded at compile time
brif    -> stack_overflow
iadd    left, -COST
store   [ctx + STACK_LEFT_OFFSET], left
```

Same instruction count as the call counter it replaces. Counting *down* is what
makes `Runtime::context()` the one door a stack size enters through — the
backend never learns it — which is the answer to the question 23 called the hard
part. `StackBudget` is a sealed newtype so a host can lower the budget and cannot
raise it, because the shadow-stack reservation is sized from it.

Two things fell out that were not the goal:

- **The shadow-stack reservation is now exact and 2.6× smaller** (12.29 MiB →
  4.77 MiB). Its old sizing multiplied "the deepest recursion" by "the widest
  frame" as if a program could have both at once. It cannot, and the guard is now
  what says so.
- **The byte model is audited, not trusted.** `audit_frame_cost` compares
  Cranelift's own `frame_to_fp_offset` against what the prologue charged, on
  every function of every program a debug build compiles — the entire test suite.
  It caught its own first calibration error before the change left the branch,
  which is the entire argument for having it.

Verified end to end on release binaries either side of the change, under
`ulimit -s 2048`: `54d2d9a` exits 134 with "thread 'main' has overflowed its
stack"; the same program on the same stack now exits 1 with a clean
`StackOverflow`. An ordinary recursive program keeps its full depth.

### D-2 — fixed (ADR-106)

`RuntimeRoots` has a sixth arm now, and it is **weak**: never traced, so it
retains nothing `RootSlots::dead` dropped, but scanned once per collection
immediately after the sweep, nulling every debug slot whose block that sweep
reclaimed. A debug value is therefore always a live object or an absence.

The timing is the whole design. "Reclaimed" is only recognisable between the
sweep and the next allocation; a filter applied later — at the snapshot, at the
render — cannot tell a block that died from one that died and came back.

**And that is not hypothetical.** With the scan disabled, the end-to-end test
shows `xs` — a 200-element `Vec` of `2000..2199` — rendering as `Some([40748])`:
a different `Vec` the block had been reissued as. Not a crash, not a null. A
well-formed, false answer under the user's own variable name. Handover 23 called
this "a snapshot can copy a dangling `GcRef`", which is the mild reading.

Cost: no measurable time, ±0.02% peak RSS, no ABI bump.
`a_dead_local_stops_being_reachable_from_its_frame` — the test a *strong* arm
would have broken on purpose — is untouched and passing.

---

## 2. The question (Q-1) — answered, ADR-109

**Pages stay segregated by size class. Descriptor segregation is rejected, not
deferred.**

The argument everyone reaches for first is memory stranding, and it does not
carry: the 22 built-in descriptors reach only eight distinct rungs, so
per-descriptor pages cost 704 KiB against the 256 KiB reachable today — 448 KiB,
0.09% of a suite peaking between 507 MiB and 3.19 GiB. Recorded so nobody
re-derives it and rejects the idea for a reason that is not true.

What rejects it is **provenance ordering**. `Heap::mark` reads `header.heap_id()`
and refuses a foreign reference *before* it masks the address to a page.
Descriptor segregation deletes the header, so there is no word left to read
first: `page_of` must be applied to an unvalidated `GcRef` and the result
dereferenced. That is exactly the unconditional mask ADR-103 Decision 5 already
weighed and called "a strictly weaker guarantee", arriving as a consequence of
the layout rather than as a proposal — which makes it harder to notice, not
easier.

Two smaller costs, both on paths recently made fast on purpose: ADR-102's inline
type proof moves off the object's own cache line, and the poisoned-descriptor
refusal loses its subject, because a per-page descriptor cannot record that one
block is filed storage.

So P-3 is **not** wasted work, and it is much smaller than 23 describes — see
below.

---

## 3. The four deferrals

### P-4 — the allocation hot spots

**`Char` interning (ADR-107) — done.** ASCII `0..=127`, 128 objects on the
24-byte rung, 3 KiB — one extra immortal page, which is a figure that moved under
it while it was being written; see the P-3 section. 23 named two parser sites;
there are four, and the one it missed — `praxis_text_get`, which is both `t[i]`
and `for c in t` — is probably the biggest for real code. A 3×3 `grid(char)`
costs 1 object instead of 10.

23 said this was "exactly the shape ADR-100 built for `Int`". It is the runtime
half only: the language has no character literal, so ADR-100's `Inst::ConstGc`
decision has no analogue and generated code never reads the new table.

**Loop-invariant literal hoisting (ADR-108) — done, and it is the largest win in
this handover.** Handover 23 filed LICM last, recommended continuing to defer it,
and listed what it would be worth — omitting `Lit::Float`, whose `AllocFloat` row
is already plain `Allocates` and therefore unconditionally hoistable.

The general pass is still declined, and now permanently: a correct one needs a
predecessor map, a dominator tree, loop detection and a CFG that is not a flat
`Vec<Block>`, all four confirmed missing. But `lower_while` / `lower_for` /
`lower_loop` each already hold their own preheader, which is the only one of the
four a *literal* hoist needs. A preheader stack in the builder gets it.

| | before | after | |
|---|---:|---:|---:|
| 20M × `x = x + 2.0` | 0.726 s | **0.512 s** | 1.42× |
| `hashwork` @ 800,000 | 0.458 s | **0.374 s** | 1.22× |
| `mandelbrot` @ 400 | 2.475 s | **2.390 s** | 1.04× |

**The headline is `hashwork`, not `mandelbrot`, and that inverts the
expectation.** `hashwork`'s generator loop allocates five out-of-range `Int`
literals per step against a body that does little else. `mandelbrot`'s inner loop
allocates ~10 `Float` boxes per iteration from the *arithmetic* and only 2 from
literals, so its ceiling was never more than about 20%. The rest is escape
analysis's problem.

One design note worth keeping: the preheader must be tracked on its own stack,
not on `LoopCtx`. `LoopCtx` is pushed *after* a `while`'s condition is lowered,
and `mandelbrot`'s `4.0` lives in a condition.

**`run_parser_plan`'s boxed `PlanId` — done, and 23's patch was wrong.** It calls
the fix "trivial to move onto `Inst::ConstGc`". `PlanId`s come from a
process-wide arena bounded at `1 << 20`; `GcConst::SmallInt` is valid only for
`-256..=1024`, and the backend `expect`s that. The 1025th plan a process
registered would have panicked the JIT. The correct patch is smaller — call the
existing `lower_lit_gc`, which already asks `small_int::index_of` and falls back.

**`Text` literals (ADR-111) — done, and it turned out to matter more than 23
priced it.** 23 asks for the validation to move out of `praxis_alloc_text` so the
row becomes `Allocates`, and prices the win at "one `CheckFault` per
evaluation" — which ADR-102 had already made two loads and a `brif` the day
after that sentence was written. There is also not a single `Text` literal in the
whole benchmark suite, so on its own this could not move a measured number.

What changed the value is ADR-108, which landed hours earlier. Its hoist is gated
on `Inst::can_fault()`, i.e. on the manifest — so a non-faulting `Text` literal
is admitted to the preheader, and a literal in a loop stops costing a `Box<str>`
malloc, a memcpy, a GC block and a sweep-time drop **per iteration**. That is the
real win, and neither 23 nor ADR-108 could have named it.

One correction to the obvious version of that claim: flipping the row is *not*
sufficient. ADR-108 states its hoistable set at the call sites, and `Lit::Text`
called `b.alloc` directly rather than going through `box_invariant_literal`. The
gate itself needed no edit; the call site did.

A violated precondition now **aborts** through `abi_guard!`, exactly as
`praxis_int_load`'s does, rather than faulting. `debug_assert` was rejected
because it compiles out and leaves a `Box<str>` of non-UTF-8 that `text_str`
later hands out as `&str` — REP-56's exact shape. The one caller that holds bytes
it did not author, `praxis_get_input`, validates them itself, so `InvalidText`
still lands at the `read`.

**`ScalarKind::Byte` — fixed rather than re-documented.** 23 §4 records
`load_symbol()` answering `IntLoad` for `Byte` as "not a live defect; fix it the
day `Byte` is wired". The mirror arm is the same defect and 23 does not mention
it: `alloc_symbol()` answers `AllocInt`, minting an `INT`-descriptor object over
a byte. No correct symbol exists to point either at, so both now refuse. The
day-`Byte`-is-wired trap is a failed build instead of a silently wrong program.

### P-2 — bounded pacer (ADR-112), and the old measurement's verdict inverted

23 records one measurement, and it says the fix is a loss: pacing off the live
set cut memory by a large factor and cost *more* time. It also says to re-run it
before designing anything, because sweep is now six times cheaper than when that
trade was measured. That instruction was right, and the reason it was right is
sharper than "sweep got cheaper".

**The mechanism behind the −14% no longer exists.** At `fd70374` the free list was
a `RefCell<HashMap<BlockLayout, Vec<NonNull<u8>>>>` with the default SipHash
hasher, probed once per allocation and once per swept block, over a `bumpalo`
arena that never reused anything. Under the doubling pacer collections were rare,
so nearly every allocation was a *fresh* bump and paid none of that; pacing off
the live set made nearly every allocation a *reused* one, so it paid two SipHash
probes and a borrow flag. ADR-103 deleted that free list. Reuse is now
`PageHeader::claim_free_block` — the same six instructions whether the block is
fresh or recycled.

The rule is `max(min(previous × 2, 64 MiB), live × 2, INITIAL)`. **The ceiling
clamps only the speculative ratchet, never the mandatory `live × k` term** — a
program whose live set legitimately exceeds the ceiling must not collect on every
allocation. That is the one line that stops someone "simplifying" the formula
into a thrash bug, and it is a numbered Decision rather than a comment for
exactly that reason.

Measured across all eight at full sizes, one binary, interleaved with alternating
order, best of 5: **0.6% faster overall while peaking 15.6× lower.** The memory
column's *shape* is the result — the four benchmarks that hold nothing all land
at 72–73 MiB (the ceiling plus the process) and the rest come out ordered by how
much they hold.

**`pipeline` is the one that pays** (0.795× at k=2), and it is a finding the
recommended screening set could not have produced: it holds a 1M-element `Vec`
live, so `live × k` binds over the ceiling and every collection marks all of it.
`k = 4` recovers most of it (0.913×) and costs every other program a 3×
retention guarantee instead of 5×, so `k = 2` stands — same growth factor as
Go's `GOGC=100`. Both prices are on record.

### P-3 — `GcHeader`, and it is one field

23 asks for 8 bytes. ADR-109 stops at **16**, and the difference is not timidity:
reaching 8 means deleting `heap_id`, which forfeits the read-before-mask ordering
Q-1 just refused to give up for a much larger prize, and deleting
`payload_offset`, which adds a dependent load to every one of the 187
`payload::<T>()` sites.

Sixteen costs one deletion. `GcHeader.size` has **zero readers in the
workspace** — every `.size()` call is `TypeDescriptor::size`. `MIN_BLOCK` and
`BLOCK_GRANULE` are already derived from the header, so the ladder, the block
count, the bitmap width and the page header all follow without an edit to
`page.rs` at all.

Every block lost exactly eight bytes and every descriptor kept its class index:
`Int` 32 → 24, `Vec`/`Text` 56 → 48, `Map` 88 → 80. The derived constants moved
with it — `NUM_CLASSES` 14 → 15, `MAX_BLOCKS` 1365 → 2048, `BITMAP_WORDS`
22 → 32, and `PageHeader` 424 → 584 bytes, from 1.29% of a page to 1.78%. That
last one is a real counter-cost and it is bought back many times over: an `Int`
page now holds **1340 blocks instead of 1010**.

Two things fell out that are worth keeping.

`small_char.rs`'s cost test is derived from `BlockLayout::of(&CHAR)` rather than
restating a constant, so it caught ADR-107's figures going stale the same day
they were written — and it caught something a prose reading would not have:
ADR-107's "**it costs zero additional pages**" claim **inverted**. It rested on
1010 blocks per page, so the 1,281 interned `Int`s spilled onto a second immortal
page with room to spare. At 1340 the `Int`s fit on one page with 57 free, and the
128 `Char`s now force a second. The free ride is gone.

And the fixture fix the mapping proposed was wrong in a way worth recording:
`#[repr(C, align(16))] Aligned16([u64; 3])` rounds *up* to 32 bytes, so it does
not restore the block-size parity the test's precondition asserts. Giving both
fixtures a `[u64; 4]` payload does, and makes the parity structural — it holds
for any header size that is a multiple of 16, where the old pairing worked only
at exactly 24.

### P-1 — the inline allocation fast path

23 describes the target as the bitmap claim. The mapping found the starting point
had moved: `AllocKind::Int` is now rare (ADR-100 routes in-range literals to
`Inst::ConstGc`), the hot instruction is `Inst::Materialize`, and for `Int` its
wrapper usually returns an *interned* object without allocating at all. So the
largest single win is not the claim — it is deleting the call, the
`catch_unwind`, the `RuntimeRoots::from_context` and the `maybe_collect` standing
in front of a two-load table read.

That splits P-1 in two. **P-1a** bakes only `RuntimeContext` and `Heap` scalar
offsets, none of which Q-1 moves. **P-1b** bakes eight `PageHeader` offsets and
three `GcHeader` ones — now safe, since Q-1 says size class, but still the larger
half.

**P-1a is done, in two records.** ADR-110 first, for the free part:
`AllocKind::Bool`, `Materialize { Bool }` and `AllocKind::Unit` become loads
rather than calls. All three wrappers' rows are already `Effect::Pure` —
`praxis_alloc_bool`'s body has been `ctx.true_ref`/`ctx.false_ref` since ADR-040
Decision 4 — so those call sites were never safepoints and never allocated.

Then ADR-113, for the one that mattered: an `Int` box is a table read behind a
pacing branch. Four loads, two compares, two never-taken branches, one cold block
that still calls `praxis_alloc_int` unchanged.

| | before | after | |
|---|---:|---:|---:|
| `collatz` @ 340,000 | 1.233 s | **1.003 s** | **−18.7%** |
| `primes` @ 1,600,000 | 1.430 s | **1.231 s** | **−13.9%** |

`tree` and `pipeline` pay about 2%, reproducibly: their values mostly leave the
interned range, so they get the pacing test in front of a call they were making
anyway.

**How the `Safepoint` obligation was made checkable rather than documented.**
ADR-040 exists so that "allocate on the paced path without pacing" has no
spelling, and an inline fast path that skips `Heap::pace` reproduces that logic
outside the type enforcing it. The resolution: the inline path forges no token
*because the token is permission to collect*, and the path it takes is exactly
the branch on which `maybe_collect` would have returned `false` — it allocates
nothing at all, it reads an immortal. So the obligation is only "take this branch
only when a collection was not due", and that is enforced by making the predicate
one `pub` statement (`Heap::collection_is_due`), exporting exactly the two
`offset_of!` displacements it reads, and welding those to the table's own bounds
in an `InlineInternSite` the backend cannot assemble. A test reads a live `Heap`
*through the exported displacements* and asserts the two words compare as the
predicate answers — so an offset drift, a lapsed `#[repr(C)]`, or a third term
added to the predicate all fail there.

The mapping's larger `InlineAllocSite` was rejected: every field on it is for the
*claim* path, which P-1a does not take, so they would have gone stale before
their first reader.

**Still open: P-1b**, the bitmap claim itself. Q-1 no longer blocks it.

---

## 4. Things that are fine and should not be re-litigated

Carried forward from 23 §4, with corrections.

- **`opt_level = "speed"` is no longer a negative result, and this is the one
  entry in 23 §4 that inverted.** It was measured twice and both times moved
  nothing, on the standing explanation that allocation is an opaque call and a
  memory clobber so the mid-end cannot move anything across a loop body. ADR-113
  removes exactly that for the commonest allocation in the language, and the
  third measurement takes **6.3% off `collatz`** (reproduced) and 1.6% off
  `primes`, with the other five inside ±0.5% and the compile-time floor costing
  0.2–0.9 ms instead of 4.7. **The flag is still not set**: one benchmark is not
  a result, and `collatz` is the most allocation-dense program in the suite, so
  its number is the best case rather than the average. What is retired is the
  comment at `Jit::in_generation` saying not to try a third time. Try it again
  after P-1b.
- **Reconstructing the debugger view from the shadow frame cannot work.**
  Unchanged; handover 22 §3.1 has the argument, and ADR-106 now depends on it.
- **`benchmarks/sizes.json` should NOT be re-tuned**, which reverses 23's advice.
  The Rust column is the *least* noisy of the three (0.5–3.3% spread, tighter
  than Praxis on six of eight rows), so raising sizes buys nothing a report that
  prints whole-number ratios can show — and it costs about 21 minutes of suite
  time. Worse, it moves every benchmark to a different rung of the pacer's
  power-of-two ladder, so peak RSS is a log2 staircase where a sub-2× size change
  reads as either 0% or 100%. Freeze it across any A/B.
- **`benchmarks/REPORT.md` is generated.** Confirmed mechanically: `report.py`
  run against the committed `results.json` reproduces the committed report
  byte-for-byte. Edit `report.py`, never the report.

## 5. What is left

Named so the next reader has a list rather than an archaeology exercise.

- **P-1b — the inline bitmap claim.** The other half of P-1. Q-1 no longer blocks
  it, and ADR-113 has already built the parts that were hard: the pacing
  predicate is one `pub` statement, the offsets are exported, and the
  `Safepoint`-token argument is on record. What it adds is eight `PageHeader`
  offsets and three `GcHeader` ones in generated code, and a header write.
- **Escape analysis**, which is now clearly the largest single item. ADR-108 took
  the *literals* out of `mandelbrot`'s inner loop and left the ten `Float`
  temporaries the arithmetic itself creates. §4.3 reserves it explicitly.
- **Tagged pointers.** Still absent, still reserved by §4.3.
- **`opt_level = "speed"`** — retest after P-1b, on a suite where more than one
  row should move. See §4.
- **`k` in the pacer.** `pipeline` pays 20% at `k = 2` and recovers most of it at
  `k = 4`, at the cost of every other program's retention bound going 3× → 5×.
  The trade is measured and on record in ADR-112; nobody has to re-derive it to
  revisit it.
- **`PAGE_SIZE`.** Genuinely one constant, everything downstream derived, both
  derivation-pinning tests re-derive automatically. ADR-109 Decision 3 has the
  corrected metadata table. The 16 KiB direction is now the one that costs more,
  not less — which is the opposite of what 23 implies.
- **Returning emptied pages to the OS.** ADR-109 Decision 3 writes the soundness
  argument 23 asks for and finds it is not the blocker: `MADV_FREE` leaves the
  bytes reading back as either their previous contents or zeros, and *both*
  encode poisoned. The blocker is that pages come from `std::alloc::alloc` and
  madvising the system allocator's memory is outside its contract. The honest
  version moves page allocation to `mmap`/`munmap`; half a day and its own ADR.
- **`for c in text` is O(n²)** — see below.

## 6. New, and not in 23

- **`for c in text` is O(n²).** `praxis_text_get` is `chars().nth(i)` and
  `praxis_text_len` is `chars().count()`. Found while measuring `Char` interning,
  which it will mask on any text-iteration shape.
- **The page allocator has still never been run under a sanitizer**, and the
  bounded pacer turns "most allocations are fresh pages" into "most allocations
  are recycled blocks" — which is the change most likely to find a latent bug in
  `claim_free_block` / `block_index` / `relink_pages` if one exists.
- **Escape analysis and tagged pointers are still absent**, and escape analysis is
  now clearly the largest remaining item: ADR-108 took the literals out of
  `mandelbrot`'s inner loop and left the ten `Float` temporaries the arithmetic
  itself creates.
