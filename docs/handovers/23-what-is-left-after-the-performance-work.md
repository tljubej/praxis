# What is left — two defects, four deferrals, and one question that decides two of them

**Date:** 2026-08-02
**Tree:** `024ed7f` (everything below was measured or read there) · **1841 tests, 0 ignored** · `just ci` green
**Predecessors:** [`21-where-the-time-goes.md`](./21-where-the-time-goes.md) is the
investigation; [`22-every-finding-closed.md`](./22-every-finding-closed.md) is what
landed and where 21 was wrong. **Read 22 first**; this document is only the work
that is still open.

## The one-paragraph answer

The six performance findings are closed and the suite is at parity with CPython.
What is left divides cleanly in three. **Two are correctness defects**, both
pre-existing, both found by this work rather than caused by it, and neither can
produce a wrong answer silently — one aborts, one is a use-after-free the
collector's own provenance check would reject. **Four are deferred performance
items**, each deferred for a stated reason rather than for lack of time. **One is
a design question** — whether pages are segregated by size class or by descriptor
— and it decides two of the four deferrals, so it should be answered before either
is started.

Nothing below is blocking anything. The tree is in a good state to stop at.

---

## 1. The two defects

### D-1 — `MAX_RECURSION_DEPTH` is a call count, not a byte budget

**Severity: aborts the host.** Defined behaviour is a clean `StackOverflow`
fault; actual behaviour on a wide frame is SIGABRT.

`MAX_RECURSION_DEPTH = 8000` (`crates/praxis-runtime/src/context.rs`) exists so
deep recursion faults cleanly instead of overflowing the native stack — its own
doc says "chosen with headroom under the native stack's abort threshold." But it
counts *calls*, and what runs out is *bytes*. A function with 20 live collections
compiles to 112 `Gc` locals, all spilled to its native frame, and 2700 of those
exhaust a 2 MiB thread stack — the exact failure the guard exists to prevent, at
a third of the depth the guard allows.

Measured on both the pre- and post-§3.3 binaries under `ulimit -s 2048`: ok at
2500, dead at 3000, identically. **§3.3 neither caused nor worsened it.** Recorded
in ADR-101's Consequences.

The shape of a fix: the depth guard already runs inline in the prologue and
already knows the function's `slot_count`. Charging a *frame cost* rather than a
count — bumping by something proportional to the frame's width, against a budget
derived from the real stack limit — is the same instruction sequence with a
different addend. The hard part is not the codegen, it is deciding what the budget
should be derived from, since `getrlimit` on the main thread and a spawned
thread's stack size are different numbers.

**Related, and worth knowing before anyone tries to lower it:**
`MAX_SHADOW_SLOTS = 192` allows only about **34 user-level collections per
function**, because one `let v = Vec()` expands to several `Gc` locals. It is a
compile-time error, not a runtime one, so it is not a defect — but it is a smaller
ceiling than the number suggests.

### D-2 — the debug values are not a `RuntimeRoots` arm

**Severity: a snapshot can copy a dangling `GcRef`.** Latent — no test reaches
it, and the collector's provenance check rejects a swept reference rather than
tracing it — but it is a real hole.

`RootSlots::dead` nulls a shadow slot the moment its local dies (MIR-01), while
the debug view deliberately keeps the value renderable (MIR-16, ADR-044). So
between the null and the fault, a value the debugger still names is unreachable
for GC. A collection in that window frees it, and `praxis_snapshot_debug_chain`
then copies a dangling reference into `CrashSnapshot`, which *is* a root set.

True on `main` before the performance work, for exactly the same values. §3.2
altered neither the values nor their lifetimes.

**Why it was not fixed as part of §3.2.** The contiguous debug value stack makes
it a two-line change — a sixth `RuntimeRoots` arm walking `[base, top)`. But that
arm is the set-merge ADR-044 exists to refuse: rooting the debug values *strongly*
makes the GC root set the over-approximate one again, which is what MIR-16 was
written to prevent, and it would break
`a_dead_local_stops_being_reachable_from_its_frame` on purpose. The honest fix is
a **weak** arm — the values are kept valid but do not keep anything alive — and
that is a new concept in this collector, so it needs its own ADR and its own
measurement. Recorded in ADR-104's Consequences.

---

## 2. The four deferred performance items

Ranked by what they are worth, which is not the order they became available in.

### P-1 — `Inst::Alloc` / `Materialize`: the inline fast path

The fourth row of handover 21 §3.4's table, and the only one not done. Today an
`Int` box is `call praxis_alloc_int` → `catch_unwind` → build `RuntimeRoots` →
`maybe_collect` → `Heap::alloc`.

**It was deferred because §3.6 was in flight and changes every offset it would
bake in.** That is no longer true, and the change got *easier*: allocation is now
`PageHeader::claim_free_block`, six instructions on a bitmap, against a
size-class-indexed `partial` list. The inline sequence is a load of
`partial[class]`, a bitmap claim, and a header write, with the slow path (no
partial page, or the pacing counter at threshold) out of line.

**The thing that makes it more than a codegen change**: it puts ADR-040's
`Safepoint` token into generated code. The token exists so "allocate on the paced
path without pacing" has no spelling — obtaining it *is* the pacing. An inline
fast path that skips `Heap::pace` when the counter is under threshold is
reproducing that logic outside the type that enforces it. That needs a decision
record saying what the generated code's obligation is and how it is checked, not
a comment.

It also unlocks **§3.7 for the third time**: `opt_level = "speed"` has now
measured as noise twice, and the standing explanation is that allocation is an
opaque call and a memory clobber, so Cranelift's mid-end cannot move anything
across a loop body. Inlining the fast path is precisely what removes that. Expect
§3.7 to stay negative until then, and re-test it immediately after.

### P-2 — bound the collector's pacer

**Untouched by all six findings.** `collect_threshold` doubles after every paced
collection and nothing lowers it except `reset`, so peak resident set is a
function of how long a program has run rather than of how much it is holding.
§3.6 changed the constant — an object costs less and an emptied page is re-classed
rather than being dead capital for its old layout, so the suite peaks at 0.5–3.2
GiB where it used to peak near 6 — but the curve is still unbounded in time.

`benchmarks/REPORT.md`'s "what would move these numbers" §2 has the standing
proposal: `max(live × k, previous × k)` with a hard ceiling.

**The one measurement that exists says the naive fix is a loss**, and it should
be re-run before anyone trusts it. The old appendix experiment — pacing purely off
the live set — cut peak memory by a large factor and cost *more* time, because
collecting against a live set this small means collecting constantly. That
measurement was taken against `fd70374`, before any of the six findings, and the
file is now `benchmarks/gcfix-pre-perf-fixes.json`, deliberately renamed so
`report.py` cannot silently compare it against a current `results.json`. **Re-run
it against `024ed7f` before designing anything**: sweep is now six times cheaper
than it was when that trade was measured, so the trade may have inverted.

### P-3 — `GcHeader` at 8 bytes (§3.6's C4)

`GcHeader` is `{descriptor, size, payload_offset, mark-byte-removed, heap_id}` =
20 bytes of fields in 24 bytes of `#[repr(C)]`. C4 would reduce it to
`{descriptor}` — `size()` becomes `descriptor().size()`, `payload<T>()` derives
its offset from `descriptor().align()`, and `heap_id()` becomes a page lookup.
An `Int` block goes 32 → 16 bytes.

Costs an ABI bump and moves the payload-offset immediate that
`Inst::EnumTag` bakes in at compile time (`lower.rs` calls
`GcHeader::payload_offset_for` as a `const fn`, so the source does not change but
the emitted constant does, 24 → 8).

**Do not start this before answering Q-1 below.** If pages become
descriptor-segregated, `GcHeader` disappears entirely and C4 is wasted work.

### P-4 — the remaining allocation hot spots

Small, independent, each worth doing on its own:

- **`Char` interning.** `praxis_alloc_char` allocates per call, and the input
  parser allocates **one `Char` per grid cell**
  (`crates/praxis-runtime/src/parser.rs:473`, `:1342`). An ASCII/BMP intern table
  in `Immortals`, exactly the shape ADR-100 built for `Int`, is a cheap win on
  every grid-shaped puzzle input. The identity argument ADR-100 makes for `Int`
  carries verbatim.
- **`Text` literals in a loop.** Still an allocation *and* a `CheckFault` per
  evaluation. `crates/praxis-mir/src/build.rs` already registers the right fix in
  a comment: `praxis_alloc_text` is `AllocatesAndFaults` because it validates its
  bytes, but a literal's bytes came from a Rust `String` and cannot fail — moving
  the validation out of the wrapper makes the row `Allocates` and the instruction
  genuinely non-faulting. That is ADR-017 territory (it changes what a violated
  compiler precondition *does*), so it wants a decision, not a patch.
- **`run_parser_plan`'s boxed `PlanId`.** Left on the allocating path by §3.5
  because it runs once per parse. Trivial to move onto `Inst::ConstGc`; near-zero
  value. Listed for completeness.
- **LICM on MIR.** Deferred by §3.5 and still the right call: MIR is a flat
  `Vec<Block>` with no predecessor map, no dominator tree and no loop detection,
  so a correct pass means building all four. Its remaining value after ADR-100 is
  out-of-range `Int` literals and `AllocKind::Text` in a loop — and the second is
  the one where hoisting is most delicate, because a faulting `Alloc` and its
  `CheckFault` are paired *positionally within one block* by `verify.rs` and must
  move together or not at all. Do the `Text` validation fix first; it may remove
  the reason.

**Not deferred, just absent**: the other two things §4.3 reserves — tagged
pointers and escape analysis. Neither exists. Escape analysis is the one that
reaches the loop-local accumulator, which is now the dominant remaining cost.

---

## 3. The question that decides P-1 and P-3

### Q-1 — are pages segregated by size class, or by descriptor?

ADR-103 chose **size class**: 14 rungs from 24 to 128 bytes, `GcHeader` retained
at 24 bytes, no ABI bump, no codegen change, and all three of ADR-039's Decisions
literally intact.

Its own Consequences name the alternative and say it is the natural end state:
segregating by **descriptor** makes `descriptor`, `payload_offset` and `size` all
page constants, takes `GcHeader` to nothing, and makes `payload::<T>()` the
identity. The descriptor set is closed and small — 22 built-ins, and a record,
enum, tuple or closure boxes its fields behind a `Vec<GcRef>`, so arity does not
widen it.

It is rejected in ADR-103 because it changes what a `GcRef` points at and touches
all 122 `payload::<T>()` sites. But the decision is not "never" — it is "not in
that change." Two open items hang off it:

- **P-3 is wasted work if the answer is descriptor.** ADR-103 says so explicitly.
- **P-1 bakes page-metadata offsets into generated code**, so the answer changes
  what those offsets are and how many there are.

Answer Q-1 before starting either.

**Two smaller tuning questions sit under it**, both one constant and both
measurable rather than arguable: `PAGE_SIZE` is 32 KiB (16 KiB recycles emptied
pages at a finer grain and costs 3.2% metadata; 64 KiB is better for `collatz`
and worse for a REPL), and a heap currently retains emptied pages forever, so it
never shrinks. `madvise(MADV_FREE)` on the block region while keeping the header
mapped would return physical pages *and* keep `page_of`'s mask sound, but it is
platform-specific and needs its own soundness argument.

---

## 4. Things that are fine and should not be re-litigated

Recorded so the next reader does not spend an afternoon rediscovering them.

- **`opt_level = "speed"` is a negative result, measured twice.** Once in
  handover 21 §3.7 and once after §3.4 landed, which is the point 21 nominated
  for the re-test. Every workload within ±3% both directions; the floor pass
  (codegen time) regressed on all eight benchmarks, `bfs` by 4.7 ms. There is a
  comment at `Jit::in_generation` saying not to try it a third time. Try it again
  only after P-1.
- **Reconstructing the debugger view from the shadow frame cannot work.**
  `liveness::block_roots` computes roots as live-*before* an instruction, so an
  `Alloc`'s destination is by construction excluded from its own safepoint's root
  set. Handover 22 §3.1 has the full argument.
- **`ScalarKind::Byte`'s `load_symbol()` is `IntLoad`** — an eight-byte read of a
  one-byte payload. This is *not* a live defect: `Byte` is reserved and not wired,
  the arm is documented as defensive, and §3.4 deliberately left it as a call
  rather than inlining it, so no generated code reaches it. It becomes a defect
  the day `Byte` is wired. Fix it in the same change.
- **`benchmarks/sizes.json` has not been re-tuned.** It was chosen for a 6 GiB
  Praxis peak; the suite now peaks at 0.5–3.2 GiB, so every Rust column in
  `REPORT.md` is smaller than it needs to be. Raising the sizes would make the
  Rust baselines less noisy without changing any ratio.
- **`benchmarks/REPORT.md` is generated.** `report.py` holds the prose as well as
  the tables, and the two had come apart before `024ed7f`. Edit `report.py`, never
  the report.

## 5. Caveats on everything above

- One machine: Apple M2 Pro / 16 GiB / macOS 26.5.2. Best-of-3, no CPU pinning,
  no frequency locking. Every A/B in the work this describes interleaved its two
  binaries; the laptop drifts by several percent over a few minutes.
- **`bfs` at reduced size is too noisy to attribute anything to** — observed
  spread 5.2–7.6 s on a single unchanged binary. Its `sizes.json` row in
  `REPORT.md` is from `run.py` and is sound; a reduced-size `bfs` delta is not.
- **The page allocator has not been run under a sanitizer.** No nightly toolchain
  on this machine, so ASan was not attempted. The net in debug builds is the
  `debug_assert`s in `block_index` (the Lemire reciprocal against plain division),
  `block_ptr` and `mark`; the release suite runs without them. This is the single
  largest untested surface in the tree and the first thing to do if anything
  mysterious shows up.
- **"Sweep is six times cheaper" is measured on `collatz` only.** The other
  benchmarks' collector deltas are inferred from wall-clock and RSS.
