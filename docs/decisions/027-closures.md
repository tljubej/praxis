# ADR-027: Closures — Approach B calling convention, capture analysis, VarCell

**Date:** 2026-07-25 · **Status:** accepted

## Context

§4.10 requires closures that "capture values automatically," with mutable
captures using "GC-managed environment cells." The M7 Part 2 handover landed the
closure frontend (parse, resolve, infer) and runtime object model
(`ClosurePayload`, `CLOSURE` descriptor, ABI wrappers), but left the
HIR→MIR→codegen bridge as a Unit placeholder. Part 3 (WS7a + WS7b) closes that
gap end-to-end.

Two design forks needed resolving before implementation:

1. **Calling convention.** A closure value bundles code (`fn_ptr`) and an
   environment. The `closures.rs` module comment described an "env as trailing
   params" scheme (Approach A); the handover §3 prescribed "load captures at
   entry" (Approach B). These are mutually exclusive.
2. **Mutable captures.** §4.10 mandates GC-managed cells for `var` captures,
   but the M5 `VarCell` carryover was unimplemented.

## Decision

### Calling convention: Approach B (closure passed as a hidden first arg)

The synthetic closure function's MIR signature is
`fn(ctx, closure_self, params...)` (ctx is the implicit hidden ABI param, as for
every Praxis function). At entry, a **prologue** loads each captured value via
`praxis_closure_capture(ctx, self, idx)` and binds it to a local. The call site
reads `fn_ptr` via `praxis_closure_fn_ptr` (which takes `ctx` for ABI uniformity,
even though unused), then emits a Cranelift `call_indirect` with the signature
`fn(ctx, closure, args...) -> i64`, passing the closure value as the hidden first
explicit arg.

**Why B over A (env as trailing params):** the call site knows nothing about
captures — it reads `fn_ptr`, passes the closure, and is done. This keeps the
single highest-risk piece (Cranelift indirect calls: signature + raw address +
matching `CallConv::Fast`) as simple as possible. The cost is a mechanical
capture-loading prologue per synthetic function, which is easy to generate and
test in isolation. B also yields a uniform indirect-call signature per-arity (one
`SigRef` per arity, reusable across all call sites of any same-arity closure),
keeps the closure value self-contained (better fault snapshots, foundation for
future borrow/move semantics), and scales to closures-in-collections
(`vec.get(0)(x)`) and recursive/curried closures. A's marginal win (a trivially
simpler body) was outweighed by pushing complexity into every call site.

### Capture analysis (`praxis-hir/src/capture.rs`)

Walk the closure's body subtree scanning every resolved `NAME` token (not just
`PathExpr`s — the lhs of `+=` is a bare `NAME` token, so a PathExpr-only walk
misses assigned-to captures). A name is a capture iff its symbol is a value
binding (`Let`/`Var`/`Param`) declared *outside* the closure node's range (params
and closure-local bindings are declared inside, so they're filtered). Dedup by
symbol; first-seen order is the env slot index, shared by the allocation site, the
synthetic function prologue, and (for `var`) the `VarCell`.

**The scan covers nested closure literals too, and the "outside the closure
node's range" test is the whole predicate.** A name a nested closure references
that resolves outside the *enclosing* closure is a capture of the enclosing
closure as well as of the nested one — it has to be, because the nested
closure's environment is filled from the enclosing frame at the point the nested
literal is allocated, so the enclosing closure must be holding the value in
order to hand it over. Nothing extra is needed to keep the nested closure's own
params and locals out: they are declared at ranges *inside* the enclosing
closure's node, so the same `contains_range` test that filters the enclosing
closure's own bindings filters them.

This sentence is here because the implementation once disagreed with it. A guard
that returned early when the walked body *was* a nested closure made
`|a| |b| b + base` capture nothing, and the inner environment was then filled
from an empty one — a silently wrong answer for a captured `Text`, a panic for
an `Int`, a SIGSEGV for a reassigned `Int`. The braced spelling
`|a| { |b| b + base }` was correct throughout, which is how it survived. See
[handover 31](../handovers/31-what-an-aoc-solve-found.md) item 1.

The last line of this paragraph used to read "immutable captures (`let`/`param`)
are `ByValue`; `var` captures are `ByCell`", which is stale since
[ADR-125](./125-a-binding-is-a-binding-and-the-compiler-decides-its-storage.md):
what picks a cell is whether anything *reassigns* the binding, not which keyword
declared it. `lower.rs`'s `Symbol::reassigned` check is the code of record — this
module deliberately does not read it.

### HIR → MIR → codegen bridge

- **`TypedExpr::Closure`** carries `{ params, body, captures, fn_type, fn_name }`
  with a synthesized unique `__closure_N` name.
- **`lower_module`** appends one synthetic `Function` per closure literal
  (collected via a typed-tree visitor), with the capture-loading prologue.
- **`AllocKind::Closure { fn_name, captures }`** allocates the value via
  `praxis_alloc_closure(ctx, fn_ptr, n)` + `praxis_closure_set_capture`. The
  codegen takes the synthetic fn's address via `func_addr` on a declared `FuncRef`
  (the synthetic fn is declared in `Jit::compile`'s first pass, so the relocation
  resolves at finalize).
- **`Inst::CallIndirect { callee, args, .. }`** dispatches indirect calls. The
  `TypedExpr::Call` arm detects a closure-value callee by checking whether the
  callee symbol is in `b.locals` (top-level `fn`s never are) and emits
  `CallIndirect` instead of `Call`.

### Mutable captures via `VarCell` (WS7b)

**Escape analysis** (`TypedModule.escaping_vars`): collect every `var` symbol
captured by some closure (`ByCell`), computed during `lower()`. Threaded to the
MIR builder.

For an escaping `var`, the binding site allocates a `VarCell`
(`praxis_alloc_var_cell`) and the **local holds the cell**, not the value.
Reads (`Path`) deref via `praxis_var_cell_get`; writes (`Assign`, plain and
compound) store via `praxis_var_cell_set`. A `ByCell` capture stores the cell in
the closure's env; the synthetic fn prologue loads the cell and binds the symbol
to it; the body's reads/writes then route through the cell automatically (the
symbol is in `escaping_vars`). The binding function and every capturing closure
share the **same** cell, so a mutation in one frame is visible to the other.
Uncaptured `var`s stay ordinary mutable locals — no cell overhead.

`VarCell` is `TypeId(12)`, an internal type (never equatable/hashable, not
first-class).

## Consequences

- Closures work end-to-end: immutable and mutable captures, indirect calls,
  currying, returned closures whose env outlives the frame (GC'd), closures in
  collections. Closes the §19.7 criterion "compile closure pipelines with
  captured values."
- Currying works for a closure body that *is* a closure, not only for one that
  contains one, and a mutable capture threaded through several environments is
  still a single shared cell. `|a| |b| b + base` and `|a| { |b| b + base }`
  print the same `<closure:N>` and the same answer, which is the assertable form
  of the capture-analysis rule above.
- The `closures.rs` doc comment was updated to describe Approach B (it previously
  described the stale Approach-A sketch).
- `praxis_closure_fn_ptr` gained a `ctx` param (unused) for ABI uniformity — every
  `praxis_*` wrapper is called as `fn(ctx, args...)` from generated code.
- ABI bumped to 6 (`praxis_alloc_var_cell` / `praxis_var_cell_get` /
  `praxis_var_cell_set`).
- The `VarCell` from M5 ("deferred to M7, only needed for closure capture") is
  finally load-bearing, as the M5 handover anticipated.
- Follow-ups (not in M7 scope): closures as `Map`/`Set` values needn't be
  equatable (§5.5: functions never are); `.0`/`.1` tuple field access; recursive
  named closures via `let rec`.
