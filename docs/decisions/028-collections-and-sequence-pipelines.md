# ADR-028: Collections, DynamicKey, and the sequence pipeline (M8)

**Date:** 2026-07-25  
**Status:** accepted

## Context

Milestone 8 (§19.8) delivers the full collection set (`Map`/`Set`/`Counter`/
`Deque`/`MinHeap`/`MaxHeap`/`BitSet`/complete `Grid`), the §6.3 sequence
pipeline, the §4.11 control-flow surface, and the `Iterable`/`SupportsOrd`
capabilities. Several representation decisions were not pinned down by the spec
and needed resolving before implementation.

## Decisions

### 1. `DynamicKey` bridges Praxis values into Rust's `HashMap`/`HashSet`

Per §11.3, `Map[K,V]`/`Set[T]`/`Counter[T]` reuse Rust's hash collections behind
opaque GC objects. Rust needs `Hash`+`Eq` on its key type; a Praxis key is a
uniform `GcRef` whose structural identity is defined by the *value's* type
descriptor (§5.5). `DynamicKey` (praxis-runtime/src/dynamic_key.rs) is the
bridge: it stores the rooted `GcRef` plus its descriptor, and its Rust `Hash`/
`Eq` delegate to the descriptor's `hash`/`equals` callbacks. This closes the
§19.7 "tuples and records as keys" criterion: ADR-026's structural eq/hash
machinery flows into Rust's hash collections through `DynamicKey`.

**Identity by `TypeId`, not pointer.** Rust `const` descriptors can be
duplicated across crate boundaries (each reference to a `pub const INT` may
inline a fresh copy), so two `INT` copies can have distinct addresses. The
`DynamicKey`/`Vec` push-descriptor-adoption logic compares descriptors by
`TypeId` (canonical type identity, descriptor.rs:14), never by pointer.

### 2. The sequence pipeline is eagerly materialized (Vec), not lazy Seq

The spec (§6.3) describes a lazy `Seq[T]` with fusion. M8-WS8 ships an
**eager** version: each combinator (`map`/`filter`/`sum`/`count`/`collect`)
lowers to a fused loop that materializes a `Vec[T]` (for the streaming ones) or
a scalar (for sinks). The catalog `CollectionCtor::Seq` exists and is
internal-only (never user-named), but the delivered combinators return `Vec[T]`.

This satisfies the user's seamless-experience requirement ("no `.collect()`",
"it should just work"): `v.map(f).sum()` and `v.map(f).len()` chain without an
explicit collect. The tradeoff is allocation (each streaming step allocates a
Vec) rather than the spec's ideal single-fused-loop. **True lazy `Seq[T]` with
cross-combinator fusion is the M8-WS8 continuation**, documented in the handover;
the remaining 20 combinators (reduce/any/all/find/position/enumerate/zip/take/
skip/take_while/flat_map/filter_map/sorted/unique/frequencies/min/max/min_by/
max_by/chunks/windows) ship alongside it.

### 3. `for` lowers to an index loop; the loop counter is a Gc Int slot

`for x in coll {}` (§4.11) lowers to `i=0; while i < coll.len() { x = coll.get(i); ...; i++ }`.
The counter lives in a **Gc Int slot** (not a transient scalar) so it persists
across the loop's block boundaries — MIR scalars are transient between
extract/materialize and don't survive block transitions, but Gc slots do. The
iterator's `len`/`get` symbols are dispatched by the collection ctor
(`praxis_vec_len`/`praxis_deque_len`, etc.). Map/Set/Grid iteration via `for`
is a follow-up (the index model assumes integer-indexable collections).

### 4. The loop-context stack enables `break`/`continue`

The MIR `Builder` carries a `loop_stack: Vec<LoopCtx { continue_target,
break_target }>`. `while`/`for`/`loop` push on entry, pop on exit; `break`/
`continue` read the top frame and jump. `return` writes the function's
`return_local` and terminates with `Terminator::Return`. This reuses the
existing `Terminator::{Branch, Jump, Return}` — no new terminator or instruction
was needed for control flow.

## Consequences

- All §6.1 collections are end-to-end (TypeIds 6–19); the GC is unchanged
  (descriptor-driven, no type switches).
- The §19.7 keying criterion is closed and evidenced by tests (tuple/text keys
  in Maps and Sets).
- The seamless pipeline works for the core combinators; full fusion + the
  remaining combinators are the documented continuation.
- A pre-existing parser bug (method chaining after a method-with-args) was found
  and fixed during WS6 — the postfix-loop checkpoint no longer advances past
  the receiver.
