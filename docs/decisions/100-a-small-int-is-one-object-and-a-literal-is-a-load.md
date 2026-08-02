# ADR-100: A small `Int` is one object per value, and a literal is a load

**Date:** 2026-08-02
**Status:** accepted — implemented
**Milestone:** performance repair (handover 21 §3.5)
**Amends:** §4.3's interning reservation, which this exercises for `Int`.
Extends ADR-011's non-moving-address argument to a third immortal kind, and
ABI v14 → v15.

## Context

`lower_lit_gc` emitted `ConstInt` + `Alloc { AllocKind::Int }` per `Lit::Int`, so
`i + 1` inside a loop heap-allocated the `1` on **every iteration**. Hoisting
that literal into a `let` by hand — no compiler change at all — made a
10M-iteration loop 36% faster (docs/handovers/21-where-the-time-goes.md §3.5).

The `Alloc` was not one cost but four: a call into `praxis_alloc_int`, the
`catch_unwind` guard around it (ADR-080), a pacing check, and — because
`liveness::is_gc_safepoint` matches `Inst::Alloc` unconditionally — a store of
every live root into the shadow frame before the call. All of it to produce an
object whose value the compiler already knew.

§4.3 reserves the cure in so many words: *"This uniform model is normative even
if later optimizations intern small integers, use tagged pointers, or eliminate
allocations through escape analysis. Such optimizations must preserve reference
and aliasing semantics."* The clause has been exercised twice already — `Unit`
since M3 and `Bool` since ABI v10 are interned singletons, so every `true` in
every program is one object and always has been.

The condition is the interesting part: *preserve reference and aliasing
semantics*. For `Int` there are none to preserve, and that is a fact about the
language rather than an assumption:

- **There is no identity operator.** `praxis_hir`'s `BinOp` is arithmetic, the
  six comparisons and the two logical connectives; `UnaryOp` is `Neg`/`Not`.
  There is no `is`, no `===`, no `ref_eq`.
- **`==` on `Int` compares payloads.** `compare_kind` answers
  `CompareVia::Scalar(ScalarKind::Int)`, which lowers to `Inst::IntCmp` over
  extracted `i64`s. The structural fallback, `praxis_struct_eq`, has no pointer
  fast path either — it checks descriptor identity and dispatches `int_equals`.
- **Keyed collections are structural.** `Map`, `Set` and `Counter` are keyed by
  `DynamicKey`, whose `eq` *does* open with a pointer comparison — but that is a
  fast path **for** structural equality, and `int_equals` is reflexive, so
  sharing an object can only make it fire more often, never change the answer.
- **An `Int` payload is never written after allocation.** `Inst::StoreScalar`
  has no builder site and the backend's arm for it is a documented no-op.
- **Nothing hashes an address into anything language-visible.** `impl Hash for
  GcRef` exists and has no users.

## Decision

### 1. The runtime interns `-256..=1024`, minted once, in the one place immortals are minted

`Immortals::new` builds a `Box<[GcRef]>` of `SMALL_INT_COUNT` immortal `Int`s
alongside `unit`/`true_`/`false_`, through the same `ImmortalWitness` seal. RT-03
— an immortal is minted exactly once, and only from `immortal.rs` — is preserved
rather than worked around: nothing else may mint one, and `int_ref` only reads
the table.

The bounds live in **one module**, `praxis-runtime::small_int`, because three
crates ask the same question: `praxis-mir` asks whether a literal is in range
while lowering, `praxis-runtime` asks again on the allocation path, and the
Cranelift backend derives its element offset from the same `SMALL_INT_MIN`. Two
spellings of the range would let the compiler emit a table read for a slot the
table does not have. This is `ScalarKind::alloc_symbol`'s rationale applied to a
constant.

`Box<[GcRef]>` and not `[GcRef; N]`: generated code reads the table through a raw
pointer parked in the context, and that pointer must survive a move of the
`Runtime` that owns the `Immortals`.

**Only `Int`.** `Float` fails the reflexivity argument that carries
`DynamicKey`'s fast path — `float_equals` is IEEE, so NaN ≠ NaN — and interning
it would make two separately-written NaN literals equal as map keys. It does not
bite *today* only because NaN has no literal spelling, which is a lexer rule
someone could change; the win is nil either way. `Text` fails a different test:
`TextPayload::Owned(Box<str>)` is not `Copy`, and `Heap::alloc_immortal` demands
`Copy` *because* an immortal is invisible to `Heap`'s `Drop` — an immortal `Text`
leaks its `Box<str>` at teardown, which is RT-02 exactly.

### 2. Minting an immortal is pacing-neutral (RT-04)

`Heap::alloc_immortal` snapshots `bytes_since_collect` and restores it. Pacing
measures *the pressure a program is putting on the collector*, and an object no
collection can reclaim exerts none. Charging it would have made the table a
hidden GC-schedule change: ~40 KiB against a 64 KiB initial threshold means every
program's first real allocation would have arrived with two thirds of its budget
spent, and widening the range later would silently move the first collection of
every program in the language.

### 3. `int_ref` still paces, even when it answers from the table

The manifest declares `VecLen`, `MapLen`, `EnumTag`, `TextLen`, `CounterGet` and
two dozen more `Effect::Allocates`, which is generated code's contract that the
call site is a GC safepoint. Returning before `Heap::pace` would make a loop
whose only allocations are small `Int`s never offer the collector a turn — the
pacing counter is the collector's only trigger, and nothing else in such a loop
touches it. So the token is minted and then explicitly dropped: `Safepoint` is
`#[must_use]`, so which of the two happened is stated rather than implied.

`AllocInt`'s manifest row stays `Allocates` for the same reason — it still
allocates for an out-of-range value.

### 4. `Inst::ConstGc` — a `GcRef` constant is a load, not a call

Interning removes the *allocation*. The call, the guard, the pacing check and the
spill are removed by a new MIR instruction: `Inst::ConstGc { dst, konst }`, where
`GcConst` is `SmallInt(i64) | Unit | Bool(bool)` — the three kinds of reference
the runtime has already minted and can name without allocating.

It carries no `RootSlots` and no `DebugSlots`, and that is the decision: it is
**not a GC safepoint**. It calls no wrapper, allocates nothing and cannot fault,
so nothing here could trigger a collection and there is nothing to show the
collector. `Inst::fault_reason` answers `None` — which the exhaustive match
forced someone to decide — so ADR-088's rule applies in both directions and a
`CheckFault` after one is rejected as redundant.

The backend emits two loads for `SmallInt` (the table base out of the context,
then the element at a compile-time byte offset) and one for `Unit`/`Bool`, which
are cached on the context directly.

**The address is read out of the live context, never baked into code.** An
`iconst` of a heap address was the obvious alternative and is wrong four ways:
there is no heap at compile time (the CLI builds the `Jit` before the `Runtime`);
`DebugSession::reload` replaces its `Jit` while keeping its `Runtime`, so
per-generation constants would grow a session without bound; `Heap::reset` mints
a fresh `HeapId`, which would make a baked address fail the mark phase's
provenance check rather than fault loudly; and it would invert ADR-043's
ownership direction, putting a pointer from compile-time metadata into the heap
the arena is required to outlive.

### 5. `Unit` and `Bool` literals take the same instruction

Neither ever allocated — `praxis_alloc_unit` and `praxis_alloc_bool` answer from
the context and their manifest rows say `Effect::Pure` — but lowering went
through `Inst::Alloc`, which `is_gc_safepoint` treats as a safepoint whatever the
manifest says. Two answers to one question is exactly the drift the
`AllocKind::constructor`/`symbols` design exists to prevent. Folding them into
`GcConst` deletes an extern call *and* a spurious full-frame spill per literal,
and leaves one answer.

## Consequences

- **Measured, three runs, minimum, against the same tree without this change:**

  | | before | after | |
  |---|---:|---:|---:|
  | 10M × `i = i + 1` | 0.31 s | **0.22 s** | 1.4× |
  | `collatz` @ 60,000 | 0.82 s | **0.30 s** | 2.7× |
  | `primes` @ 300,000 | 0.57 s | **0.31 s** | 1.8× |
  | `vm` @ 400,000 | 1.64 s | **1.22 s** | 1.3× |
  | `hashwork` @ 800,000 | 0.74 s | **0.58 s** | 1.3× |
  | `mandelbrot` @ 200 | 1.02 s | **0.88 s** | 1.2× |
  | `tree` @ 60 | 2.14 s | **1.89 s** | 1.1× |

  `collatz` is the largest because its inner loop is arithmetic over `/ 2`,
  `% 2`, `* 3`, `+ 1` and comparisons against `0` and `1` — every literal
  interned, and `n % 2` answers `0` or `1`, which is interned too.

- **The interned range is a tuning knob, and it is 40 KiB of permanently
  resident arena.** `SMALL_INT_COUNT` × 32 bytes (a `GcHeader` is 24, an
  `IntPayload` is 8). That is a floor on RSS for every program, which matters
  because handover 21 §3.6 is explicitly about the memory ceiling. It is one
  constant in one module with the benchmark suite named as its arbiter.

- **Four tests were repaired *before* the change was measured, because interning
  turned them into false passes rather than failures.**
  `allocate_until_automatic_collection`,
  `checked_int_add_is_an_automatic_gc_safepoint`, `vec_push_many_survive_collection`
  and `every_scalar_boxing_wrapper_paces_the_collector` all detect a collection
  by watching the live registry *shrink* — and an interned `Int` never enters the
  registry, so `after < before + 1` was true on the first iteration and each
  reported success without a collection ever running. Every one now allocates
  above the range, through a shared `UNINTERNED` constant that says why.
  `dynamic_key_equal_for_identical_scalar_values` was the one honest failure: its
  `assert_ne!(a, b, "distinct allocations")` stopped being true.

- **A `Bool`/`Unit` literal is no longer spilled at its defining instruction, and
  neither is a small `Int`.** MIR-16 says `DebugSlots` must not shrink when
  `RootSlots` becomes exact, and this is the same hazard from the other side. It
  is discharged rather than argued: the temp is still spilled at the next
  `CheckFault`, whose `DebugSlots` includes every `Gc` local defined so far in
  the block, and the verifier guarantees a `CheckFault` immediately precedes
  every fault diversion. A crash-snapshot test pins it.

- **`Heap::reset`'s documentation obligation grew.** Its doc already said
  "Immortal singletons must be re-allocated afterwards"; that now means the whole
  `Immortals` value, because a `RuntimeContext` minted before a reset holds a
  `small_ints` pointer as well as three references. `reset` has no production
  caller, so this is documentation, not a bug.

- **LICM on MIR was considered and deferred.** It was the handover's first
  suggestion, and MIR has none of the prerequisites — `Function` is a flat
  `Vec<Block>` with no predecessor map, dominator tree, back-edge detection or
  pre-header notion. After this change the residual win on literals is
  second-order (two loads per iteration instead of zero), and what would remain
  is `Alloc { AllocKind::Text }` in a loop — which is where hoisting is *most*
  delicate, because a faulting alloc and its `CheckFault` are paired positionally
  within one block and must move as a unit, and hoisting a faulting alloc out of
  a zero-trip loop would raise a fault the program would not have raised. A test
  records the one fact such a pass would need from the verifier: an `Alloc`
  hoisted out of the block that uses its result verifies, because MIR is
  deliberately non-SSA and has no def-dominates-use rule.

- **ABI v14 → v15.** `RuntimeContext` gained `small_ints`, appended after
  `fault_message` so every generated-code-read offset above is unchanged. Unlike
  `parse_detail`, `crash_snapshot` and `fault_message`, generated code *does*
  read this one, so the version bump is load-bearing rather than bookkeeping.
