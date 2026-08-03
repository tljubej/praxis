# Two mallocs per runtime call, four useful instructions in 216, and the sanitizer that was always available

**Date:** 2026-08-03
**Tree:** `e4f42e6` — everything below was measured or read there.
**Predecessor:** [`24-every-item-in-23-closed.md`](./24-every-item-in-23-closed.md).
This is a fresh investigation plus the plan for what is left; 24's §5 list is
carried forward and re-ranked at the end.

## The one-paragraph answer

**Handover 23's largest stated caveat was false, and so was the reason for it.**
A nightly toolchain has been installed on this machine the whole time; the whole
workspace runs clean under AddressSanitizer — **1911 tests, 0 failures, 0
sanitizer reports** — which closes the item 23 called "the single largest
untested surface in the tree". With that out of the way, a fresh profile finds
something bigger than anything on 24's list: **every runtime wrapper that roots a
reference does two `malloc`s and two `free`s to do it.** `NativeScope::new` boxes
a frame and then grows an empty `Vec` on the first root. A prototype that removes
both is **2.70× on `vm`, 1.50× on `bfs`, 1.24× on `hashwork`**, 1.22× on the
geometric mean, with all 1913 tests passing and every checksum identical — and
the prototype's own profile says a proper fix is worth more still. This is the
same defect class as the SipHash-keyed free list handover 21 found: a data
structure allocated per operation on the hottest path.

Separately, and answering the standing question directly: **on a pure loop Praxis
is already 3–6× faster than CPython 3.14.** The suite's 0.8× geometric mean is
not the arithmetic. It is the runtime-call path, and the two mallocs above are
most of it.

`opt_level = "speed"` is settled and it is **negative, permanently** — see §3.

---

## 1. The sanitizer, and the caveat that was not true

Handover 23 §5: *"The page allocator has not been run under a sanitizer. No
nightly toolchain on this machine, so ASan was not attempted."*

`rustup toolchain list` has had `nightly-aarch64-apple-darwin` installed
throughout; it currently resolves to `rustc 1.99.0-nightly (11177f223
2026-08-02)`. The command is one line:

```bash
RUSTFLAGS="-Zsanitizer=address" ASAN_OPTIONS=detect_leaks=0 cargo +nightly test --workspace --target aarch64-apple-darwin --release
```

Result: **1911 passed, 0 failed, 0 `AddressSanitizer` reports**, across 28 test
binaries. Instrumentation was verified rather than assumed — the test binaries
link `@rpath/librustc-nightly_rt.asan.dylib` and carry 37 undefined `__asan_*`
symbols, so the pass is a real pass and not a silently uninstrumented build.

`--target` is load-bearing: without it, build scripts and proc macros are
instrumented too and the build fails. That is the whole trick, and it is the
reason to write the command down here.

**What this does and does not cover.** It covers the runtime: `claim_free_block`,
`block_index`, `relink_pages`, the sweep, every payload accessor, the parser
interpreter. It does **not** cover JIT-generated code, which Cranelift emits raw
and which no `-Z` flag can instrument. So the shadow-stack writes and the inline
`Int` fast path are still unchecked by this. But the surface 23 nominated — the
page allocator under recycling pressure, which ADR-112's bounded pacer made the
common case — is now checked and clean.

**Add it to CI as a nightly-only job**, not to `just ci`: the instrumented build
is a second full compile and `just ci` is already 17 minutes on this laptop.

---

## 2. Where the time goes now

Release build at `e4f42e6`, `/usr/bin/sample` at 1 ms, attributed leaf-first.
`???` is JIT-generated code, which has no symbols.

| | `collatz` | `mandelbrot` | `pipeline` | `bfs` |
|---|---:|---:|---:|---:|
| JIT-generated code | **51.8%** | 17.3% | 9.8% | **28.6%** |
| `Heap::claim_block` | 9.3% | **22.0%** | **19.0%** | 2.1% |
| `Heap::alloc_raw` | 8.1% | 17.3% | 11.7% | — |
| `praxis_alloc_float` | — | 14.0% | — | — |
| `praxis_alloc_int` | 3.9% | — | 7.8% | — |
| `Heap::collect_inner` | 4.3% | 9.3% | 17.1% | — |
| `Heap::mark`/`trace` | — | — | 4.8% | — |
| **system `malloc`/`free`** | — | — | — | **~24%** |
| `praxis_bitset_contains` | — | — | — | 12.6% |

Three things to read out of that table.

**Handover 21's headline has inverted.** It measured 19% of `collatz` inside
generated code and called the other 81% "runtime bookkeeping". `collatz` is now
**52%** generated code. The six findings did what they were for.

**`mandelbrot` and `pipeline` are 60%+ allocator.** For `mandelbrot` the reason
is structural and has no existing mitigation: ADR-100 interns `Int` in
`[-256, 1024]`, and **there is no analogous table for `Float`** — the value space
is not enumerable. Every float temporary in that inner loop is a real block
claim. `praxis_alloc_float` alone is 14%.

**`bfs` spends a quarter of its time in `libsystem_malloc`.** That is not the GC.
It is §4 below.

---

## 3. What the generated code actually emits

Measured by dumping Cranelift IR and vcode for

```praxis
while i < limit {
    acc = acc + i * 3
    i = i + 1
}
```

Per iteration: **156 CLIF instructions, 216 aarch64 instructions.** Four of them
are the arithmetic — one `imul`, two `iadd`, one `icmp`. The other 152 break down
as:

| what | CLIF instrs | why it is there |
|---|---:|---|
| runtime type proofs | **42** (27%) | ADR-102: 7 sites × `load descriptor; iconst DESC; icmp; brif; load payload; jump` |
| allocation fast path | ~30 | ADR-113: pacing test, intern-range test, table read |
| overflow checks | 20 | §4.12, normative |
| shadow-stack root spills | ~17 | ADR-019/101, required before a safepoint |
| `Inst::CheckFault` | 9 | ADR-088: 3 sites × `load ctx.pending_fault; load flag; brif` |
| crash-debugger value stores | 7 | ADR-104 |
| the arithmetic | **4** | |

Six findings fall out of that, listed in §5 with what each is worth.

### `opt_level = "speed"` is settled, and the answer is no

Handover 21 §3.7 has three measurements and ends "try it again after P-1b". It
can be closed now without waiting, because the *code* can be compared rather than
the clock. Same program, same binary, `opt_level` toggled:

| | hot loop | whole function |
|---|---:|---:|
| `none` | 216 instrs | 473 |
| `speed` | **216 instrs** | 471 |

**Zero difference in the loop.** Not "within noise" — the same count, and
spot-checking the blocks, the same instructions. That retires the explanation
that has been carried since 21 ("the mid-end has nothing to work with when the
loop body is a chain of opaque calls") and replaces it with a sharper one: what
the lowering emits is not redundant *to Cranelift*. The 31 `movz`/`movk` pairs
that re-materialize descriptor addresses are not CSE-able because the register
allocator rematerializes constants on purpose rather than keeping them live; the
loads are through memory Cranelift cannot prove non-aliasing; the type proofs
compare against addresses it cannot fold.

**Recommendation: set the flag to `none` explicitly, delete the "try again after
P-1b" note, and record this as the last measurement.** Redundancy in the loop is
the *lowering's* to remove, not the mid-end's, and every item in §5 removes it at
the lowering.

### The suite average is not the arithmetic

Best of 3–5, same machine, CPython 3.14.2 (the Homebrew PGO+LTO build the report
uses):

| microbenchmark | Praxis | CPython | Praxis is |
|---|---:|---:|---:|
| 20M-iteration `Int` loop, values interned | 0.32 s | 1.89 s | **5.9× faster** |
| 20M-iteration `Int` loop, values outside the intern range | 0.49 s | 1.97 s | **4.0× faster** |
| 5M-iteration `Float` multiply loop | 0.12 s | 0.41 s | **3.4× faster** |
| 5M-iteration `Vec` index + `%` loop | 0.19 s | 0.61 s | **3.2× faster** |

So the language *is* several times CPython on straight-line loops, and
`REPORT.md`'s 0.8× geometric mean is being set by something the microbenchmarks
do not reach. The difference between the two `Int` rows is one real allocation
per iteration: **8.5 ns**, which is what a block claim plus its share of the
sweep costs today.

---

## 4. F-1 — `NativeScope` mallocs twice per rooting runtime call

**This is the finding.** `crates/praxis-runtime/src/roots.rs`:

```rust
let mut frame = Box::new(NativeRootFrame {   // malloc #1
    parent,
    roots: RefCell::new(Vec::new()),         // malloc #2, on the first root()
});
```

`NativeScope::new` is called by **60 sites** — 42 in `abi.rs`, 17 in `parser.rs`.
It is how a runtime wrapper keeps its arguments reachable across an allocation,
so it is on the path of `praxis_vec_push`, `praxis_deque_push_back`,
`praxis_deque_pop_front`, `praxis_bitset_insert`, `praxis_set_insert`,
`praxis_map_insert` — every mutating collection primitive in the language. Each
call boxes a frame, then heap-grows a zero-capacity `Vec` on the first
`scope.root(…)`, then frees both on the way out.

`bfs`'s profile names the exact chain:

```
praxis_bitset_insert
  └ RawVec<GcRef>::grow_one → finish_grow → _xzm_xzone_malloc_tiny
praxis_deque_push_back
  └ RawVec<GcRef>::grow_one → finish_grow → _malloc_zone_malloc
praxis_deque_pop_front
  └ ... and _xzm_free directly beneath each of them
```

**Measured.** Two prototypes, each an A/B of two binaries run in palindromic
order (A,B,B,A) so the laptop's drift is shared, best of 5, every benchmark's
output verified byte-identical to the baseline's:

| benchmark | inline roots only | + pooled frame (both mallocs gone) |
|---|---:|---:|
| `vm` | 1.660× | **2.623×** |
| `bfs` | 1.125× | **1.482×** |
| `hashwork` | 1.070× | **1.220×** |
| `pipeline` | 1.013× | 1.018× |
| `tree` | 1.016× | 1.013× |
| `primes` | 1.020× | 1.001× |
| `mandelbrot` | 1.017× | 1.003× |
| `collatz` | 1.007× | 0.998× |
| **geometric mean** | **1.101×** | **1.220×** |

Re-run at full `sizes.json` sizes: `vm` **2.695×**, `bfs` **1.496×**, `hashwork`
**1.236×**. The whole workspace test suite passes on the prototype: **1913
passed, 0 failed.**

`vm` is the benchmark `REPORT.md` lists furthest from Rust (61×) and second
worst against Python (0.8×). This alone takes it to roughly 23× Rust.

**And the prototype understates the fix.** Profiling `vm` *with* the prototype
applied, `NativeScope::new` is still 9.2%, its `Drop` 8.9%, and `_tlv_get_addr`
2.3% — 20% of the remaining runtime is the thread-local frame pool the prototype
used to dodge the box. A real fix has no pool.

**The real fix is ADR-101 applied to the fifth root arm.** The frames already
nest exactly with the Rust stack, which is the property that made the shadow
frame a contiguous stack. So: a `native_roots` slot stack in `RuntimeContext`,
`NativeScope::new` bumps a pointer, `root()` writes one word and bumps, `Drop`
restores the saved top. No box, no `Vec`, no `RefCell`, no thread-local, and
`push_roots` becomes a contiguous `[base, top)` scan instead of a linked-list
walk with an `extend_from_slice` per frame. This is a copy of a design that is
already in the tree and already has its ADR; it needs an ADR of its own only for
the reservation-sizing argument (how deep the native chain can get, which is
bounded by the parser interpreter's recursion, not by user code).

The one API question it raises: `NativeScope::new(ctx)` returns the scope by
value today, and a stack-allocated frame cannot move after it is linked. Either
the frame is in the context's array (no address stability problem at all — the
preferred shape) or the constructor splits in two. Prefer the first.

---

## 5. The other findings, ranked

### F-2 — every collection primitive is an out-of-line call

`v[j]` lowers to `call praxis_vec_get(ctx, vec, idx)` — with root spills before
it, a `catch_unwind` inside it, and an `Inst::CheckFault` after it — for a body
that is a bounds compare and a load. `praxis_bitset_contains` is **12.6% of
`bfs`** for a word load and a shift. `praxis_vec_len` is 3.0% of `bfs` and 1.2%
of `pipeline` for reading a `usize`.

This is exactly the shape ADR-102 and ADR-113 already established: prove the
descriptor inline, do the work inline, branch to the existing call on the cold
arm. The payload layouts are `#[repr(C)]` and their offsets are already exported
for other reasons. Candidates in value order: `vec_get`, `vec_len`,
`bitset_contains`, `deque_len`, `vec_push` (fast arm: capacity available).

Worth: `bfs` and `pipeline` are the suite's only two losses against Python, and
this is what they are made of.

### F-3 — a comparison boxes a `Bool` and then unboxes it to branch

`while i < limit` emits, in order: `icmp slt`, `uextend`, a `select` between
`ctx.true_ref` and `ctx.false_ref` (the box), a debug store, a descriptor load, a
compare against the `Bool` descriptor's address, a branch, a `uload8` of the
payload, a compare against zero, a `uextend`, and finally the branch that was
wanted. **15 CLIF instructions to produce a condition that `icmp; brif` already
had.**

A peephole in the lowering — when a `Materialize { Bool }` has exactly one use
and that use is the block's terminator, branch on the predicate directly — is
local, needs no new IR, and cannot change semantics because the boxed `Bool` is
an immortal singleton that nothing else observes. Every conditional in every
program pays this today.

### F-4 — the type proof costs 7 instructions and 3 of them are one constant

ADR-102's inline proof is `load descriptor; iconst DESC_ADDR; icmp eq; brif; load
payload`, and on aarch64 the `iconst` is three instructions (`movz` + two
`movk`) because the descriptor lives above 2³². Seven sites in the sample loop,
**31 `movz`/`movk` per iteration** — 14% of the loop's machine instructions
materializing addresses that never change.

Two independent fixes, in increasing order of what they need:

1. **Put the built-in descriptor addresses in `RuntimeContext`.** The proof
   becomes `load ctx+N; icmp`, one load instead of three constant halves, and the
   load is from a line that is already hot. Purely mechanical.
2. **Elide the proof when the static type is known.** The front end has already
   proved `i: Int`; ADR-102's proof exists to keep REP-56 (a `check`-clean
   program extracting an `Int` from a `Unit`) a refusal rather than an
   out-of-bounds read — that is, to defend against a *compiler* bug, not a
   program one. It is worth keeping as a `debug_assert`-shaped thing, not as
   release-mode code, if the argument for the front end's guarantee is written
   down. **This is a decision, not a patch** — it is the one item here that trades
   a stated safety property for speed, and it should be its own ADR with the
   trade priced.

### F-5 — `Float` has no interning and no escape analysis, so `mandelbrot` is 63% allocator

ADR-100's table cannot be built for `Float`. The two things §18.2 already
sanctions that *do* reach it are **tagged pointers** and **allocation elimination
for non-escaping temporary scalar results**, and `mandelbrot`'s inner loop is the
canonical shape for the second: ten `Float` temporaries per iteration, every one
of them dead before the loop's back edge.

Note for whoever costs tagged pointers: **the design document does not need to
change.** §18.2 reserves "tagged or interned small scalar objects", "allocation
elimination for non-escaping temporary scalar results", and "stack promotion of
non-escaping objects with debugger-safe materialization" by name, and §4.3's
normative sentence is explicitly written to survive all three ("This uniform
model is normative even if later optimizations intern small integers, use tagged
pointers, or eliminate allocations through escape analysis"). No language
semantics are on the table for the top of this list.

### F-6 — `Inst::CheckFault` re-reads a flag the previous branch already decided

```
brif overflowed, block_raise, block_next
block_raise: call praxis_raise_int_overflow_if(ctx, 1); jump block_next
block_next:  load ctx.pending_fault; load flag; brif flag, fault_epilogue, …
```

The only way the flag is set on that path is the block immediately above, which
already branched on the same predicate. Three instructions per fallible
operation, three fallible operations in the sample loop. ADR-088 ("a faulting
instruction is observed by the next one") is the contract and it should stand;
what can change is that the *lowering* specializes when the producing instruction
is inline and its fault path is a block it emitted itself — the raise block jumps
straight to the fault epilogue instead of falling through into a re-read.

### F-7 — the crash debugger still costs 3.4%

Measured as a deletion, the way handover 21 §3.2 measured it: one binary, the
debugger's continuous bookkeeping compiled out behind an env var, palindromic
interleave, best of 5, outputs identical.

| | speedup with the debug view compiled out |
|---|---:|
| `collatz` | 1.077× |
| `tree` | 1.071× |
| `primes` | 1.028× |
| `mandelbrot` / `pipeline` | 1.026× |
| `hashwork` | 1.009× |
| `vm` | 1.006× |
| **geometric mean** | **1.034×** |

ADR-104 did most of the work — 21 measured this at 18–24%. What is left is one
store per `Gc`-local definition, a second slot-stack push and pop per call, and
the zeroing of the value region in the prologue (19 stores in the sample
program). 3.4% is not free but it is not next either. **The honest options are
(a) leave it, (b) compile two variants and pick at `Jit::new` from
`--debug never`.** Do not re-litigate reconstructing the view from the shadow
frame; handover 22 §3.1 settled that and ADR-106 now depends on it.

### F-8 — `for c in text` and `t[i]` are O(n²)

Carried from 24 §6 and confirmed: `praxis_text_len` is `chars().count()` and
`praxis_text_get` is `chars().nth(i)`, and `iter_plan` gives `Text` an
`InPlace { TextLen, TextGet }` plan, so `for c in t` walks the string from the
start for every character. This is a **correctness-of-complexity** bug on the
shape this language exists for — the user's own `test.px` in the working tree is
`for c in b`.

The fix that covers both `t[i]` and `for c in t` in one move: record
`is_ascii` on the text payload at construction (it is one pass over bytes that
already happen to be in cache) and make both primitives index bytes directly when
it is set. Puzzle input is ASCII essentially always. Non-ASCII text gets a
one-entry `(char_index, byte_offset)` cursor cached on the payload, which makes
sequential access O(1) amortized and leaves random access O(n) — the same
guarantee Python gives, arrived at differently.

---

## 6. The plan

Ordered by measured value per unit of work. Each row is a landable change with
its own ADR where one is owed.

| # | work | worth | cost | owes an ADR |
|---|---|---|---|---|
| **W1** | **`NativeScope` → contiguous native root stack in `RuntimeContext`** (F-1) | **1.22× geomean measured, `vm` 2.70×, and the prototype understates it** | half a day; the design is ADR-101's, already in the tree | yes — reservation sizing |
| **W2** | `for c in text` / `t[i]` in O(1) (F-8) | unbounded on text-shaped programs | half a day | yes — the ASCII flag is an observable representation choice |
| **W3** | ASan as a nightly CI job (§1) | it is already green; this keeps it green | an hour | no |
| **W4** | Inline the collection primitives: `vec_get`, `vec_len`, `bitset_contains`, `deque_len` (F-2) | `bfs` and `pipeline`, the two rows behind Python | 1–2 days | no — ADR-102's argument covers it |
| **W5** | Branch on the predicate instead of boxing a `Bool` (F-3) | 15 CLIF instructions per conditional, every program | a day | no |
| **W6** | Descriptor addresses into `RuntimeContext` (F-4.1) | 14% of the sample loop's instructions | a day | no |
| **W7** | Fold `CheckFault` into the inline fault branch (F-6) | 3 instructions × every fallible op | 1–2 days | no — ADR-088 stands, the lowering specializes |
| **W8** | **Escape analysis on MIR for non-escaping scalar temporaries** (F-5) | `mandelbrot` is 63% allocator; this is the only thing that reaches it | **the big one — a week+**, and it needs the CFG work ADR-108 declined | yes |
| **W9** | **Tagged pointers for `Int`** | removes boxing, root spills *and* safepoints from integer loops | the largest single change in the tree: every `payload::<T>()`, `Heap::mark`, every descriptor read | yes, several |
| **W10** | P-1b — the inline bitmap claim (24 §5) | the other half of P-1 | ADR-113 built the hard parts | yes |
| **W11** | Elide the static type proof (F-4.2) | 6 of 7 instructions per scalar read | a day, plus the argument | **yes — this one trades a safety property** |
| **W12** | Two code variants, `--debug never` picks the lean one (F-7) | 3.4% | a day | yes |

**Do W1 first and alone.** It is a 22% geometric-mean win measured on a
prototype that passes the whole suite, it touches one file plus 60 mechanical
call sites, and it changes no generated code — so it can be measured against
`e4f42e6` with nothing else moving.

**W1 through W7 are all "the runtime call path is too expensive", and together
they are worth more than W8 or W9.** None of them needs a language decision. The
right shape for the next round is to land them, re-run `run.py`, and *then* price
escape analysis against a suite where the bookkeeping is gone — because that is
exactly the mistake handover 21 §3.6 recorded: a percentage from an earlier
section of the same document had expired by the time it was acted on.

### On budging for performance

You offered to. **The top eight items do not need it.** §4.3's uniform boxed
representation and §18.2's optimization list are already written to permit
interning (shipped), tagged pointers, escape analysis, and stack promotion with
debugger-safe materialization. What is expensive is not the language's model — it
is a `Box::new` per collection insert.

Three places where budging would actually buy something, in case they become
attractive later:

- **W11**, the runtime type proof. Not a language semantic — a defense against
  compiler bugs, in release code, on every scalar read. 6 of 7 instructions.
- **W12**, the crash debugger being unconditional. §9 makes it always available;
  making it a compile-time variant costs a flag at `praxis run` and buys 3.4%.
- **§4.12's checked arithmetic**, which is 20 of the sample loop's 156 CLIF
  instructions. **Do not touch this one.** The branches are perfectly predicted,
  the real cost is F-6's redundant re-read rather than the check, and it is a
  genuine safety property of the language rather than an implementation
  accident.

---

## 7. Carried forward from 24 §5, re-ranked

- **P-1b — the inline bitmap claim.** Still open, still unblocked. Now **W10**,
  and it moved *down*: `collatz` is already 52% generated code, so the remaining
  allocator time on the benchmark it helps most is 25%.
- **Escape analysis.** Still the largest structural item. Now **W8**, and §5's
  `mandelbrot` profile is the argument for it.
- **Tagged pointers.** **W9.**
- **`opt_level = "speed"`.** **Closed, negative.** §3 has the instruction-count
  comparison that makes it a fact rather than a timing. Set it explicitly to
  `none` and delete the standing note.
- **`k` in the pacer.** Untouched. ADR-112 prices both sides; nothing in this
  investigation bears on it.
- **`PAGE_SIZE`.** Untouched.
- **Returning emptied pages to the OS.** Untouched. ADR-109 Decision 3 has the
  soundness argument and the `mmap`/`munmap` shape.
- **`for c in text` is O(n²).** Promoted to **W2** — it is a complexity bug on
  this language's core shape, not a performance item.
- **The page allocator under a sanitizer.** **Done, clean.** §1.

## 8. Caveats

- One machine: Apple M2 Pro / 16 GiB / macOS 26.6. Every A/B here interleaves its
  two arms in palindromic order and takes best of 5, and every one verified
  output identity against the baseline — but there is no CPU pinning and no
  frequency locking, and this laptop drifts several percent over a few minutes.
- The A/B benchmark sizes are **reduced** from `sizes.json` so a sweep fits in
  minutes. F-1 was re-confirmed at full sizes; nothing else was. Nothing here
  should reach `REPORT.md` without `run.py`.
- The instruction counts in §3 are from one synthetic loop. It is representative
  of `collatz`/`primes` and not of `bfs`/`vm`, which are dominated by §4.
- The F-1 prototype is a **prototype**: a small-vector plus a thread-local box
  pool. It was written to price the fix, not to be the fix, and its own profile
  says so. It is not in the tree; the tree is unmodified at `e4f42e6`.
- `praxis_bitset_contains` at 12.6% of `bfs` is attributed by sample, and part of
  that is the root spills and fault check *around* the call rather than inside
  it. F-2 removes all three together, so the attribution does not need splitting
  before acting on it.
