# ADR-029: Pipeline fusion and the chain-recognition pass (M8-WS11)

**Date:** 2026-07-26  
**Status:** accepted

## Context

ADR-028 §2 shipped the §6.3 sequence pipeline as an **eager** per-combinator
lowering: each combinator (`map`/`filter`/`sum`/`count`/`collect`) emitted its
*own* loop and materialized an intermediate `Vec`. The user-visible behavior was
correct (`v.map(f).sum()` returned the right answer with no mandatory
`.collect()`), but the spec's "fuse common chains into loops" clause (§6.3) was
not met — `v.map(f).filter(p).sum()` compiled to three loops and two throwaway
Vecs. The M8 handover (§7) identified cross-combinator fusion as the top
follow-up and sketched a plan to hook a recognition pass into HIR's
`lower_method_call`.

## Decisions

### 1. Recognition happens in the MIR builder, not HIR

The handover proposed hooking `lower_method_call` in HIR. That is unnecessary:
the entire chain is already visible as a `TypedExpr::MethodCall` tree at
MIR-lowering time — each combinator's `receiver` is itself a `MethodCall` (or a
collection leaf). The recognizer (`recognize_pipeline` in
`praxis-mir/src/build.rs`) walks that tree at the single dispatch site
(`lower_expr_gc`'s `MethodCall` arm, when `lowering_symbol` is empty). This
keeps the change localized to one crate and one function family, and requires
no typed-tree or HIR changes.

The dispatch is **incrementally safe**: if recognition declines (the chain
contains a non-pipeline method, or an unrecognized combinator), the call falls
back to the verbatim M8-WS8 eager per-combinator lowerer
(`lower_pipeline_combinator`). A regression in the recognizer can never break
the eager path.

### 2. Combinators classify into `Stage`s (streaming) and `Sink`s (terminal)

A `PipelinePlan` is `source · Stage[] · Sink`. The 27 §6.3 combinators sort into
four buckets:

- **Streaming** (fuse into the loop body as a `Stage`): `map`, `filter`,
  `filter_map`, `flat_map`, `take`, `skip`, `take_while`, `enumerate`, `zip`.
- **Aggregating sink** (terminate, scalar/element result): `sum`, `product`,
  `count`, `min`, `max`, `min_by`, `max_by`, `any`, `all`, `find`, `position`,
  `fold`, `reduce`.
- **`collect`** — explicit sink → Vec.
- **Barrier** (materialize, then resume): `sorted`, `unique`, `frequencies`,
  `chunks`, `windows`. These need the whole sequence and require new runtime
  sort/dedup helpers; they are **deferred** (not registered in the catalog, so
  they Y110 until a separate workstream lands).

When the outermost call is a streaming stage rather than a sink (e.g.
`let out = v.map(f)`), the recognizer appends an implicit `Collect` so the chain
yields a Vec — mirroring the eager M8-WS8 behavior where `map`/`filter` produced
Vecs directly.

### 3. Stages emit branches inline; control flow is real, not enum-encoded

Each stage emits its MIR branches directly into the current block and returns
`(item_after_stage, still_live)`. `still_live == false` means the stage already
emitted a jump to the loop's increment block (`filter` dropped the element,
`skip` is in the prefix) or exit (`take`/`take_while`/`zip` stopped the loop).
This is essential: a Rust `bool` cannot represent a runtime branch, so a
control-flow-enum return value (the first draft's model) was unsound. The
inline-emission model mirrors how the rest of the builder (`if`/`while`/`match`)
lowers straight-line MIR.

The fused loop pushes its own `LoopCtx { continue_target: incr_blk,
break_target: exit }` so `filter`-skips jump to the increment (not the header —
avoiding an infinite loop) and short-circuit sinks (`any`/`all`/`find`) jump to
exit. `flat_map` is special-cased before the stage loop: it invokes its closure,
runs the sink in a nested inner loop over the result, then continues the outer
loop (consuming the outer element).

### 4. Barriers split to collect; lazy `Seq[T]` remains deferred

Barriers are not registered in the catalog. When the recognizer walks past one
(a `MethodCall` whose name is a barrier), it simply stops the walk and treats
that inner call as the source — the inner chain (below the barrier) fuses and
collects to a Vec, the barrier itself Y110s, and a future workstream wires it.
This makes adding barriers later purely additive.

The delivered combinators still return `Vec[T]` at the chain's end (eager
*materialization at the sink*); fusion removes the *intermediate* Vecs between
combinators, which is the performance goal. True first-class lazy `Seq[T]`
values flowing through non-pipeline contexts (the handover's step 5) remain a
later refinement — not needed for the perf win or any current fixture.

### 5. `find`/`position` return -1 on miss; `enumerate`/`zip` are forward-compatible

Praxis has no `Option` type yet, so `find`/`position` return `-1` on miss (the
Rust `i64::MAX`-as-sentinel convention would be surprising; -1 is the
least-surprising "not found" index). `enumerate` and `zip` produce `(i, item)`
and `(a, b)` tuples respectively, but **tuple field access `.0`/`.1` is still
deferred** (ADR-026), so these combinators are fully useful only once that lands
— they ship now for forward-compatibility and because `enumerate().count()` /
`zip(b).count()` / `.sum()` (via closures that destructure in their params) work
today. `filter_map` is modeled as "map and keep everything": the catalog types
it `(T) -> U` with non-Unit `U`, and a precise Unit-drop needs a runtime tag
check (deferred).

## Consequences

- The §6.3 "fuse common chains into loops" clause is **met**: `v.map(f).filter(p).sum()`
  compiles to one loop, zero intermediate Vecs. The pipeline grows from 6 → 23
  combinators (only the 5 barriers remain).
- The M8 `fold` stub is closed — `fold` now threads its accumulator through the
  closure via `CallIndirect`.
- The eager M8-WS8 lowerers are retained verbatim as the fallback path; any
  chain the recognizer declines is bit-for-bit identical to M8 behavior.
- The GC-rooting risk flagged in the handover (live GcRefs mid-loop across
  fused stages) is structurally mitigated: every call/materialize carries
  `live_roots`, but the liveness pass (`crate::annotate`) recomputes the precise
  minimal root set, so the builder only needs to emit correct instruction
  *shape*. The `pipeline_fused_chain_survives_gc_stress` test (300 elements
  through a fused map+filter+sum) verifies this end-to-end.
- 620 tests pass (up from 591 at M8 close); `just ci` clean.
