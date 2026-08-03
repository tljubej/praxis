# Ten packages, six waves, and the five things handover 25 got wrong

**Date:** 2026-08-03
**Tree:** `e4f42e6`, unmodified.
**Predecessor:** [`25-two-mallocs-per-runtime-call.md`](./25-two-mallocs-per-runtime-call.md)
is the investigation and the ranking. This is the execution plan: the same work
decomposed into packages that separate agents can build in parallel without
clobbering each other.

**How this was produced.** Ten agents scoped one package each, read-only, against
this tree; one built the conflict matrix and the waves from their file lists; one
was told to refute the result and did. Everything below that carries a file and a
line was verified by reading the code, not inferred. What was *not* verified is
flagged in §9 — the scoping agents were forbidden from building or running, so
every timing claim in here is still handover 25's, and every claim about what the
compiler will accept is still a claim.

## The one-paragraph answer

The work splits into **ten packages across six waves**, three of which run three
agents wide. **A wave 0 comes first and is not optional**: five packages verify
their central claim by counting emitted instructions and there is no dump hook in
the tree, ten packages independently named their ADR `114-*.md` (which in separate
worktrees is a silent overwrite, not a merge conflict), and four want to write the
same ABI constant. Wave 0 builds the shared tooling, assigns the ADR numbers, and
installs the measurement lock. **The scoping corrected handover 25 in five places
and three of them widen the plan**: W1 does not touch `abi.rs` at all, so W2 and
W4a run beside it rather than behind it; W5 and W8's first stage are the same
transform and only one should be built; and W4 has a prerequisite handover 25 did
not know existed. Two packages should not be built as framed — W11 rests on a
premise its own scope read the repair log and refuted, and W12 cannot deliver its
win through any flag that exists today.

---

## 1. The five corrections to handover 25

**1. W1 does not write `abi.rs`.** Four scopes independently sequenced themselves
behind W1 on the assumption that it edits the 42 `NativeScope::new` sites there.
It does not: if `NativeScope`'s signature is preserved byte-for-byte — and it can
be, because with the roots living in the context's array the scope holds only a
header pointer, a `usize` and a `PhantomData` — then **zero of the 60 call sites
and zero of the 70 `scope.root(…)` sites are edited**. W1's whole footprint is
`roots.rs`, `context.rs`, `lib.rs`. This is the single most load-bearing
correction in the set; it is what makes wave 1 three wide.

**2. W5 and W8 Stage 0 are one transform. Build W8-S0; delete W5.**
`lower_while` (`build.rs:2197-2216`) emits `Materialize{Bool}` immediately
followed by `ExtractScalar{Bool}` in the same block — which is exactly the pair a
block-local box/unbox forwarding removes in general. The same builder shape emits
it for every intermediate node of a float expression tree
(`lower_float_binop:1980` → `lower_materialize_float:2005`), so the general
version also removes **8 of `mandelbrot`'s 10 `Materialize{Float}` per
iteration**. Landing W5 as a narrow special case and W8 as the general rule later
means the second has to unpick the first.

**3. W8 is far cheaper than handover 25 priced it, and it needs none of ADR-108's
four missing analyses.** §6 called it "the big one — a week+, and it needs the CFG
work ADR-108 declined". Stage 0 is block-local forwarding: no predecessor map, no
dominator tree, no loop detection. Its real cost is not analysis, it is the crash
debugger — an elided temp has no `GcRef` to store, and
`crates/praxis-cli/tests/run.rs:752` asserts on exactly such a temp
(`@ "a + b"` in `debug_temps.px`).

**4. W4's stated premise is false.** §5 F-2 says "the payload layouts are
`#[repr(C)]` and their offsets are already exported for other reasons." `rg -n
'offset_of!' crates/` returns 19 hits and **not one is on a collection payload**,
and the hot field is inside a `std::Vec<GcRef>`, whose field order Rust does not
guarantee at all. W4 therefore splits: **W4a** introduces a `ReprCVec` with a
pinned layout (runtime-only, no backend change), **W4b** does the inlining.
`praxis_deque_len` drops out of scope entirely — `DequePayload.items` is a
`VecDeque` (`collections.rs:224`), whose layout is even less specified.

**5. `opt_level` stays where §3 left it.** Nothing in the scoping bears on it. It
is closed.

Two smaller ones: `payload::<T>()` has **190** sites, not 122 (handover 23) or 187
(handover 24); and `praxis_vec_len` is `Effect::Allocates`, so inlining it means
delegating to ADR-113's intern path, not just reading a `usize`.

---

## 2. Wave 0 — INFRA. One agent, alone, before anything branches.

Not optional, and it is the wave that prevents silent damage rather than loud
damage.

| item | why |
|---|---|
| **`PRAXIS_DUMP_CLIF` / `PRAXIS_DUMP_VCODE`**, permanent, in `module.rs::Jit::compile` | Verified absent: no `PRAXIS_*` env var in the codegen or CLI crates, no `set_disasm`, and `lower_function` consumes its `codegen::Context` via `define_function` + `clear_context`, so an integration test cannot read the IR after the fact. Handover 25's 156-CLIF/216-instruction figures came from a hand-added `eprintln` that was reverted. **W4b, W6, W7, W10 and W12 all verify their headline by counting instructions**; without this, five agents each add and revert the same hook in the same file. |
| **`lowered_function_ir(src) -> String`** in `lower.rs`'s test module | The existing `emitted_ir` wraps a single emit closure, not a whole `lower_function`, so any package asserting "the hot path contains no `call`" has to hand-build MIR today. |
| **`benchmarks/ab.py`** — the palindromic harness | Five packages describe it in prose, slightly differently. See §7. |
| **`scripts/asan.sh`** | The `--target` is load-bearing and four packages independently rediscovered that. Record the `e4f42e6` baseline (1911 passed, 0 reports) in a comment. |
| **The ADR number registry** | Pre-write `114..123` into `docs/decisions/README.md` with owner and placeholder title, so each package **edits its own pre-existing line** instead of appending. Assignment: **114 W1, 115 W2, 116 W6, 117 W7, 118 W4, 119 W10, 120 W8-S0/S0b, 121 W8-S1, 122 W11, 123 W12.** Nobody picks their own number. |
| **A MIR-shape test helper** | `lower_src_to_mir(src)` plus counters by `Inst` variant. W8-S0's gate is "`mandelbrot`'s inner loop goes from 10 `Materialize{Float}` to 2"; W11's census needs the same machinery. |
| **A dominator assertion helper** | `assert_dominates(func, pred_block, store_block)` over `DominatorTree::with_function`. W10 needs it — every store to a claimed block must be dominated by the pacing branch, which is strictly stronger than ADR-113's "is it the entry block's terminator". |

Wave 0 must be **committed to main before wave 1 branches are cut**, so every
worktree inherits the hook and the reserved ADR block.

---

## 3. The waves

| wave | packages | width | notes |
|---|---|---:|---|
| **0** | INFRA | 1 | main tree, alone |
| **1** | **W1**, W2, W4a | 3 | runtime only; no generated code changes |
| **2** | W6, W7, W8-S0 | 3 | the backend wave; W6 takes the ABI bump |
| **3** | W4b, W10 | 2 | both consume W6's descriptor table |
| **4** | W8-S0b | 1 | sole ownership of `SpillCtx` |
| **5** | DECISION-GATE | 1 | re-measure and re-rank; not a build wave |

**Worktree isolation is mandatory from wave 1 on.** Six worktrees already exist
under `.claude/worktrees/`, each with its own `target/`; **cap concurrent cargo
builds at 3** regardless of how many agents a wave nominally has — 16 GiB will
swap above that against a 9560-line `abi.rs`. Run `just ci` **once per merged
wave**, not once per package: it is ~17 minutes and ~14 of those are macOS
XProtect exec-scanning freshly-linked binaries, a cost that multiplies with every
parallel run.

### Wave 1 — W1, W2, W4a

Regions verified disjoint by grep, not asserted: `context.rs` — W1 stops at
`clear_for_rerun` (~1073), W2 is at `alloc_text` (1194), W4a at `alloc_vec`
(1239); `heap.rs` — W2 at 2508/2589/2662, W4a at 1794/1842/1859; `abi.rs` — no
pair within 3 lines; `parser.rs` — 308 vs 324-345; `lib.rs` — different lines of
one sorted `pub use` block. **Merge order W1 → W4a → W2**: W1 owns the largest
`context.rs` region, so it lands first and the other two rebase onto a settled
file.

### Wave 2 — W6, W7, W8-S0

W6 goes first among the `lower.rs` packages because it mints the descriptor table
sized for all 22 built-ins, which is what W4b and W10 want in wave 3, and it takes
the round's single v19→v20 ABI bump. W7's regions (the raise diamonds at
2431/2484, the `IntBinOp` arm 1286-1441, the block loop 440-465) do not intersect
W6's (the const block, `inline_scalar_load_of:2274`, the `iconst` at 2378).
**Merge order W6 → W7.**

**One thing must be resolved before this wave is cut.** W8-S0's stated transform
is "rewrite each `ExtractScalar { dst: e }` to a scalar copy of `s` into `e`" —
and **there is no scalar-to-scalar move instruction in MIR.** The exhaustive
variant list is `ir.rs:252-483`; `MoveGc` is Gc-only and its own doc at
`ir.rs:438-440` says "`Materialize` is the one legal `Scalar` → `Gc` transition",
while `StoreScalar` writes into an existing box. If W8-S0 adds a `MoveScalar`
variant it must edit `lower_inst` in `lower.rs` — which W6 and W7 are both
rewriting in this wave — and it moves out. If it does whole-function `LocalId`
substitution instead, it stays in `praxis-mir` and the wave holds. **Resolve the
mechanism first.**

### Wave 3 — W4b, W10

Verified disjoint in `lower.rs`: W4b owns the single `CallTarget::Runtime` arm at
1542 plus new fns after 2429; W10 owns the `Alloc` arm at 984, the `Materialize`
arm at 1264, and a new `emit_inline_claim` beside `emit_inline_intern:2114`.
Neither touches `emit_scalar_load`, the raise diamonds, or `SpillCtx`. **Merge
W4b → W10.** Re-run ASan on the merged result specifically: W4b hands generated
code a raw buffer pointer and W10 has generated code writing `GcHeader`s, and
**ASan does not instrument JIT-emitted code** — so this pair is the highest-risk
memory-safety point in the plan and the argument has to be written down as well as
run.

### Wave 4 — W8-S0b, alone

Sole ownership of `SpillCtx` and `store_debug_defs`/`store_debug_local`; adds
`scalar_kind` to the `#[repr(C)] DebugLocalMeta` generated code reads; teaches
`clear_reclaimed` (`debug.rs:314`) and `copy_stack` (`crash_snapshot.rs:240`) to
skip a scalar slot. Its failure mode is *the collector dereferences an f64 bit
pattern as a `GcHeader`*. This is the one wave where a shared tree is actively
wrong: the debugger-fidelity gate can only be read as a signal if nothing else is
moving the debugger.

---

## 4. The packages

### W1 — the native root stack. **Do this first.** ADR-114. ~1 day.

One contiguous `GcRef` array owned by `Runtime`, reachable through
`RuntimeContext.native_roots`. `NativeScope` becomes a saved watermark, `root()`
one store and one increment, `Drop` one store, `push_roots` one
`extend_from_slice` over `[0, top)`.

**The decision the package turns on: the store must GROW, and the watermark must
therefore be a `usize` index, not a `*mut GcRef`.** ADR-101 can make shadow-stack
exhaustion unrepresentable because both factors are constants; here **only the
depth is bounded**. 15 of the 60 scopes root inside a loop whose trip count is the
program's input — the graph builtins (`abi.rs` 5836/5867/5897/5937/5970/6018) open
one scope for a whole search and `ClosureOracle::retain` (`abi.rs:5779`) roots
every newly discovered state, roughly two roots per edge explored. No constant
covers that; a hard `assert!` turns a large puzzle input into a process abort. So
the store reallocs, and **a pointer watermark passes every small test and dies
only on realloc**. That is what the ASan run and a
`a_scope_survives_the_growth_its_own_roots_force` test exist to catch.

No ABI bump: the field keeps its position and its width, and both its writer and
its only reader are inside `praxis-runtime`. None of the 11 `offset_of!`s in
`lower.rs` names `native_roots`.

### W2 — `Text` in O(1). ADR-115. ~1 day.

**It costs zero bytes, which is the finding.** `SourceSlice` is 24 bytes and
`Box<str>` is 16, so the `#[repr(C)]` union already reserves 8 bytes the `Owned`
variant never uses — `size_of::<TextPayload>()` stays 32, the block stays 48, the
size class does not move, and nothing in `praxis-mir` or the codegen crate
changes.

Spend them on **one** field: `char_count: Cell<u64>` with `u64::MAX` as
`NOT_COUNTED`. It gives the ASCII test exactly and for free — `char_count ==
bytes.len()` iff every scalar is one byte, no separate flag — and makes `t.len()`
O(1) after the first call even for non-ASCII. **Lazy, not at construction**,
against handover 25's phrasing: `praxis_get_input`'s stdin buffer can be tens of
MB and a program that never indexes it would pay the whole scan.

Note that `for c in t` is O(n²) **twice**: `iter_plan`'s `len` call sits in the
loop *header*, so it is re-evaluated per iteration. Recommendation is to decline
the non-ASCII cursor and record the refusal with its numbers (block 48→56, page
density 670→574, +16.7% pacing charge).

### W4a / W4b — inline the collection primitives. ADR-118. 2-3 days.

**W4a** (wave 1, runtime only): a `ReprCVec` with a pinned `#[repr(C)]` layout,
because `std::Vec` is `#[repr(Rust)]` and `offset_of!` cannot reach its private
fields. ~20 mutating `.items.*` sites migrate. **Add
`crates/praxis-runtime/src/dynamic_key.rs:419-420` to the file list** — it is a
multi-line `.items.push` that the single-line census grep missed, inside
`#[cfg(test)]`, and it is still a hard compile error once the field type changes.

**W4b** (wave 3, the backend): three primitives, not five.
`praxis_bitset_contains` is the cleanest (`Pure`, no allocation, no fault, no
pacing obligation); `praxis_vec_get` next (`Faults`, but the fast arm cannot);
`praxis_vec_len` third (`Allocates`, discharged by delegating to
`emit_inline_intern`). `praxis_deque_len` is **not inlinable** as the payload
stands. `praxis_vec_push` is deferred.

**The inline arm keeps the root spills.** `liveness::is_gc_safepoint` (L377)
treats *every* `Inst::Call` as a safepoint regardless of the symbol's `Effect`, and
ADR-113 already settled the question in the words to reuse: the spill "is a
MIR-level property about which instructions the collector may run at, not a
backend arm's to narrow from what it happens to emit." **So the realized win on
`bfs` will be less than the 12.6% handover 25 attributes to
`praxis_bitset_contains`** — part of that 12.6% is the spills, and they stay. Do
not promise 12.6% in the ADR; measure it.

### W6 — descriptor addresses into `RuntimeContext`. ADR-116. ~1 day.

Verified: `praxis_runtime::scalars::INT` is at `0x10053b788` in
`target/release/praxis`, so the `iconst` at `lower.rs:2378` really is
`movz`+`movk`+`movk`. Proof goes from 5 instructions to 3.

**Take the by-value array indexed by `BuiltinTypeId`, not four plain fields and
not a base pointer.** A base pointer costs two dependent loads (reject); four
fields cost the backend a second hand-written `ScalarKind → which field` mapping
alongside the one that already exists. The array is one load at a compile-time
displacement and makes the slot index and the descriptor one value.

**A trap the agent must be told about up front.** This will break
`a_bool_extract_reads_one_byte_and_a_char_four` (`lower.rs:3391`) and it will
present as a panic in a test about payload *widths*. The cause is the test helper
`payload_load` (`lower.rs:3316`) matching the displacement by **substring**: with
the table appended at offset 136, the `Char` slot prints as `v0+168`, and
`"+168".contains("+16")` is true. `Int` lands at 152 and `Float` at 176, neither
of which collides — so exactly one of three sub-cases fails, which is the most
confusing possible presentation. The fix is to match the displacement as a whole
token. Hours will be lost to a phantom offset bug otherwise.

### W7 — fold `CheckFault` into the inline raise. ADR-117. ~1.5 days.

Checked `Int` arithmetic is **the only faulting instruction in the language whose
fault path the lowering emits itself**. When an `IntBinOp { overflow: Checked }`
is immediately followed by a `CheckFault` — which ADR-088 guarantees it always is
— the raise cold blocks jump straight to `blocks[on_fault]` and the `CheckFault`
emits nothing. ADR-088 is a MIR rule and is untouched; no MIR, no verifier, no
runtime behaviour changes.

Verified structurally available: `Terminator::Fault`'s epilogue takes no block
parameters (`lower.rs:1856-1873`), so the direct jump has somewhere to land. Fold
**both** Div/Rem diamonds — the two conditions are mutually exclusive, which
`lower.rs:1405-1408` already documents.

**Handover 25 said this owes no ADR. Its scope disagrees and is right.** ADR-102
wrote a doc section literally titled *"# ADR-088 is untouched"* whose closing
sentence — "both arms of the diamond converge at `cont`" — is exactly what W7
falsifies. Rewriting a previous ADR's stated invariant in a code comment, with no
record, is the thing the decision log exists to prevent.

**Scope honestly:** on `bfs` and `vm` — the two benchmarks dominated by runtime
calls — this reaches almost nothing. It is a `collatz`/`primes` change.

### W8-S0 / W8-S0b — block-local box/unbox forwarding. ADR-120. 2-3 days for the pair.

Stage 0 is a `praxis_mir::forward` pass invoked from inside `lower_module`, run
**before `annotate`** (it deletes safepoints, and `RootSlots`/`DebugSlots` are
computed per safepoint). Gate on `!can_fault()`; verified satisfiable —
`Inst::fault_reason` (`ir.rs:529`) answers `faulting(scalar.alloc_symbol())` for
`Materialize`, `AllocFloat` is `Allocates` (`praxis-stdlib/src/abi.rs:217`), and
`Effect::faults()` is true only for `Faults`/`AllocatesAndFaults`.

**Stage 0 lands `crates/praxis-cli/tests/run.rs:752` RED on purpose.** That is its
measurement signal, not a failure. Two dangers: an agent "fixes" it by editing the
test, which silently narrows a shipped §9 debugger guarantee; or another package
in flight cannot tell whose change broke the debugger. **Never run W8-S0
concurrently with W12, and never merge it to main without W8-S0b.**

Stage 0b is the scalar debug slot that makes 752 green again *unedited*. Note
`liveness::uses`, `live_out_of` and `live_in_fixpoint` are bare `fn`, private to
the module — `liveness.rs` is in the file list whether the scope said so or not.

### W10 — the inline bitmap claim, aimed at `Float`. ADR-119. 2-3 days.

**Re-priced downward, and the edge is not in handover 25.** W10's entire
justification is `praxis_alloc_float` at 14% of a 63%-allocator `mandelbrot`, and
`mandelbrot` is the only float-allocating benchmark in the suite. **W8-S0 removes
8 of its 10 float boxes.** By the time W10 is measured in wave 3 it is inlining a
call that happens ~5× less often, for 2-3 days plus a 350-450 line ADR. Either
re-price it at the wave-2 boundary or move it ahead of W8-S0.

**The crux is the `Safepoint` obligation, and it must be the ADR's first
Decision.** ADR-113 could say "the inline path forges no token *because* it
allocates nothing — it reads an immortal." W10 breaks that clause. The replacement
is three parts, of which the first is now checkable by **dominance over the
emitted CFG** rather than by "is it the entry block's terminator". Order:
header → payload → `allocated` bit → counters, so that a hypothetical collection
mid-sequence sees a block that is simply not allocated rather than an allocated
block with a garbage header. Pin it with an IR test, not a comment.

### W11 — **do not build as framed.** ADR-122 if authorised at all.

Its own scope read the repair log and refuted the premise. **REP-56 was not a
type-system hole**: inference and lowering disagreed about where a variant
pattern's enum comes from, so a `praxis check`-clean program emitted
`ExtractScalar { Int }` against a genuine `Unit`. Three more of that class are in
the tree — REP-49, TY-31's catalog bound, REP-54's parser/synthesizer drift — the
most recent fixed **two days before handover 25 was written**. And the `Unit`
sentinel makes the front end's guarantee routinely false *by design* at every
fault path, held only by ADR-088's positional `CheckFault` rule.

**The counter-proposal, which trades no safety property at all:** elide the proof
only where the descriptor follows from *MIR's own emissions* — the `src` local's
every definition is a `Materialize { scalar: K }`, an `Alloc { AllocKind::K }`, or
a `ConstGc` of a K-typed immortal. That is the whole of the arithmetic loop and
none of the four historical defects.

**Gate it on a census as step 0.** If fewer than half the hot-loop
`ExtractScalar` sites come out provable, land only the MIR pass and the verifier
rule — which turn REP-49 and REP-56 into *build failures* and are a pure safety
win worth having regardless — and report the backend half as not worth its risk.

**Never build W6 and W11 blind and add their results.** At today's only proof site
they overlap almost completely: if W11 elides for all four wired scalars, W6's win
goes to zero, because the load W6 introduces disappears along with the `icmp` it
feeds. That is the exact double-count handover 21 recorded.

### W12 — **defer.** The crux resolves badly.

`auto` *is* resolvable before codegen — it is a pure function of TTY state, and
`run::run` holds the `DebugMode` at line 33, 98 lines before `Jit::new()` at 131.
But resolving it and going lean under a pipe is **exactly what §9.6 forbids**,
because the *noninteractive* diagnostic prints a backtrace and top-frame locals
out of the same debug view (`render.rs:101-118`). And explicit `--debug never`
promises the same thing (`debug_mode.rs:7`: "always print the noninteractive
diagnostic and exit nonzero"). So the package needs a **fourth mode nobody will
type**, plus an edit to normative prose in `praxis_technical_design.md` §9.6, to
buy a 3.4% that W1 partly eats.

If it is ever revived: **measure `FramesOnly` before `None`.** Keeping the
frame-entry claim preserves the backtrace, loses only §9.6 item 3, and by handover
25's own breakdown should capture most of the win.

### W8-S1 and W9 — behind the gate.

W8-S1 (Gc→Scalar demotion for loop-carried locals) reaches `mandelbrot`'s `x` and
`y` and takes the inner loop to **zero** float allocations.

**W9 (tagged pointers) has no version worth its price today, and this reverses
handover 25's ranking.** Low-bit tagging narrows `Int` below §4.3's normative
"signed 64-bit payload". NaN-boxing makes two NaNs one word and thereby breaks
`DynamicKey`'s pointer fast path for `Map`/`Set`/`Counter` keys. Either rewrites
190 `payload::<T>()` sites, 133 `descriptor()` sites and both production
`heap_id()` sites — to buy the ~17 root-spill instructions per loop **that W8-S1
also removes**. Measure after W8-S1 before spending 3-6 weeks. Current
recommendation: **decline**.

**Confirmed, so nobody re-derives it:** `praxis_technical_design.md:2308` (§18.2)
lists "Tagged or interned small scalar objects", "Allocation elimination for
non-escaping temporary scalar results" and "Stack promotion of non-escaping
objects with debugger-safe materialization" verbatim, and §4.3 at line 265 says
the uniform model "is normative even if later optimizations intern small integers,
use tagged pointers, or eliminate allocations through escape analysis."
**No design-document change is needed for any package in this plan.** One textual
distinction is load-bearing and an ADR must use it: §18.2 attaches "with
debugger-safe materialization" to *stack promotion of objects*, not to *allocation
elimination for temporary scalar results*.

---

## 5. The conflict matrix, condensed

Full matrix in the workflow output; these are the rows that decide scheduling.

| file | packages | verdict |
|---|---|---|
| `codegen/src/lower.rs` (3804 lines) | W4b, W6, W7, W10, W11, W12, W8-S0b | **The bottleneck.** Hard pairs, never parallel: W6/W11, W11/W8-S0b, W12/W8-S0b, W7/W12, W4b/W6. Soft (worktree + ordered merge): W6/W7, W6/W10, W7/W4b, W7/W10, W4b/W10. Merge order within a wave: **W6 → W7 → W4b → W10.** |
| `runtime/src/context.rs` | W1, W2, W4a, W6, W8-S0b | **Hard W1/W6** — both rewrite the struct field list, `placeholder()` and `Runtime::context()`'s body. W2 (1194) and W4a (1239) are >120 lines below anything W1 touches: trivial, parallel-safe. |
| `runtime/src/abi.rs` — `RUNTIME_ABI_VERSION:227` | W4b, W6, W8-S0b, W10 | **Exactly one v19→v20 bump per round, taken by W6**, which owes it twice. The others append a *paragraph* under the v20 heading and touch no numeral. Three satellite readers will each fail loudly and confusingly if this is not enforced: `gc.rs:472`, `abi.rs:6117`, `lib.rs:54`. **Pre-stub owned lines under the v20 heading** the way ADR numbers are pre-allocated — W4b and W10 are in the same wave and would otherwise append adjacent prose to one doc-comment block. |
| `mir/src/build.rs` | W5, W11, W8-S0 | W5 deletes the `ExtractScalar` at 2149 and 2212; W11 adds a field to all 22 constructions *including those two*. Resolved by deleting W5. |
| `docs/decisions/114-*.md` (the filename) | all ten | **The worst conflict in the set, because git will not flag it** — two unrelated documents at one path in two worktrees, and the merge keeps one. Wave 0 assigns numbers. |

---

## 6. The measurement protocol

**Concurrent measurement is the single failure mode that silently invalidates
everything.** One agent running `cargo build` saturates every core; a second agent
timing a 5-second `bfs` during that build is measuring the compiler. The numbers
still look plausible, which is why this is worse than a crash.

**Phase separation, not locking-and-hoping.** Each wave has three phases and they
never overlap. **(1) Build/test:** all agents concurrent; `cargo test`, `just ci`,
ASan. **Nobody times anything** — any number produced here is discarded.
**(2) Quiesce:** every agent reports done and stops; the orchestrator verifies
`pgrep -f 'cargo|rustc|praxis'` is empty and 1-minute load is below 0.5.
**(3) Measure:** *one* agent owns the machine exclusively and measures every
package in the wave, back to back.

Mechanical enforcement, because a rule agents can forget is not a rule:
`benchmarks/ab.py` takes an exclusive lock and **exits nonzero** if it cannot get
it. **The lock must live at an absolute path outside any worktree** —
`/tmp/praxis-measure.lock`, not `benchmarks/.measure.lock`. Six worktrees already
exist, each with its own `benchmarks/`, so a repo-relative lock succeeds against a
different inode in each and the mutual exclusion silently does nothing. Note also
that macOS `sysctl -n vm.loadavg` returns `{ 1.2 3.4 5.6 }` and needs parsing, and
that XProtect's exec-scan runs as `syspolicyd`, which a "no rustc alive" check
does not see.

**The per-package A/B.** The baseline is **not the previous commit** — it is *this
tree with this package's single toggle point reverted*. ADR-113 records measuring
against the wrong baseline giving 14.4% where the right one gave 0.8%. Copy both
binaries **out** of `target/` first. Run palindromically A,B,B,A with the leading
arm alternating, minimum 5 reps, best per arm. `sizes.json` is **frozen** — assert
its sha256. Diff every run's stdout byte-for-byte between arms before believing
any timing; a differing checksum voids the measurement. Include **controls the
package should not move** — `collatz`/`primes` for the allocator packages — and if
a control moves, the number is not yours.

**Prefer the deterministic evidence where it exists.** For W4b, W6, W7, W10 and
W12 the instruction count is the honest headline and it does not drift. W6 in
particular predicts 7 sites × 2 instructions = 14 fewer per iteration of the
sample loop; if the count does not move by roughly that, the change did not land,
and no amount of A/B tells you that. **Report the instruction count as the result
and say plainly when the clock could not resolve the difference** — a sub-2%
single-benchmark delta on this machine is not a result.

**Re-baseline at every wave boundary**: a full `run.py` at frozen sizes plus a
`sample` profile of `mandelbrot`, `bfs` and `vm` into `results-wave-N.json`. That
is the denominator the next wave prices itself against. Nothing reaches
`REPORT.md` except through `run.py` — it is generated; edit `report.py`.

---

## 7. The traps, in one list

1. **Ten packages want ADR 114.** In separate worktrees this is a silent
   overwrite, not a merge conflict. Wave 0 assigns.
2. **`benchmarks/.measure.lock` as specified is a no-op** across the six existing
   worktrees. Absolute path.
3. **W6 will break `a_bool_extract_reads_one_byte_and_a_char_four`** via a
   substring match on `"+16"` inside `"+168"`. Expected update; tell the agent.
4. **W8-S0 lands `run.rs:752` red on purpose.** Not a failure. Do not edit the
   test. Do not merge without S0b. Do not run alongside W12.
5. **Four packages want `RUNTIME_ABI_VERSION`.** One bump per round, W6 takes it;
   the rest append prose to pre-stubbed lines.
6. **ASan does not instrument JIT-generated code**, and W4b, W10 and W8-S0b all
   put new unsafe behaviour exactly there. A green run is necessary, not
   sufficient — the argument must be written as well as run.
7. **The ranking expires as the waves land.** Every percentage in handover 25 was
   measured at `e4f42e6`, before W1's 1.22× moves the denominator. Handover 25 §6
   already warns about this and handover 21 §3.6 records the same mistake being
   made once. Treat wave 5 as a re-ranking, not a backlog to drain.
8. **Cap builds at 3; run `just ci` once per merged wave.**

---

## 8. What was deleted, deferred and declined

- **W5 — deleted.** Subsumed by W8-S0; building both gives two mechanisms for one
  shape.
- **`praxis_deque_len` — dropped from W4.** `VecDeque`'s layout is unspecified.
- **`praxis_vec_push` — deferred.** Its fast arm needs a capacity check and a
  length write, which is a mutation in generated code.
- **W12 — deferred.** Needs a fourth CLI mode and a §9.6 edit to buy 3.4%.
- **W11 as framed — declined**, counter-proposal offered, gated on a census.
- **W9 (tagged pointers) — declined for now**, and this reverses handover 25's
  ranking. Re-measure after W8-S1.
- **`opt_level = "speed"` — closed**, permanently, by handover 25 §3.
- **W4's registered follow-up, currently unowned:** after W4b + W8-S0, an
  `if bs.contains(x)` condition still pays the full box/unbox round trip, because
  its Gc local is defined by `Inst::Call` (`lower.rs:1512-1546`) and W8-S0's
  producer set is `Materialize`/`Alloc`/`ConstGc`. Register it rather than leaving
  it as an orphan reference to a package that no longer exists.

## 9. What is not verified

The scoping agents were read-only and forbidden to build or run, so:

- **Every timing in this document is handover 25's.** Nothing here was
  re-measured.
- **`size_of::<TextPayload>() == 32` with 8 dead bytes** was measured by compiling
  a faithful copy of the declarations standalone, not by building this tree.
  Re-check at the keyboard with a `const _` assertion.
- **W4a's claim that `size_of::<ReprCVec<T>>() == size_of::<Vec<T>>()`** and that
  reconstituting a `Vec` from raw parts round-trips without a capacity surprise is
  asserted from reading. Pin both with `const _` blocks and a round-trip test.
- **How many `CheckFault`s in the corpus are actually foldable** (W7) was not
  measured. Statically known: 2 of the 4 MIR `check_fault()` producers are
  foldable, and handover 25 §3 reports 3 of 3 in one synthetic loop.
- **W11's elided fraction is not measured**, and everything about whether the
  package is worth doing turns on it. The census is step 0 and it is a gate.
- **Whether W6 is a wall-clock win at all.** It is certainly a machine-instruction
  win (5 → 3 per proof). Whether trading three independent ALU uops for one L1
  load-use dependency is faster on an M2 Pro cannot be settled by reading.
- **Whether the `Materialize`→`ExtractScalar` pair is as dominant across the whole
  suite as in `mandelbrot`** (W8-S0). Counted by hand-walking one inner loop.
