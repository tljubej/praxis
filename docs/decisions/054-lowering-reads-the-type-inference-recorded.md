# ADR-054: Lowering reads the type inference recorded, and a specialization substitutes the scheme's own binders

**Date:** 2026-07-29
**Status:** Accepted — implemented
**Milestone:** Repair (stage S15 — F15, HIR-01, HIR-02, MONO-01, MONO-02)

## Context

ADR-014 says HIR lowering may "re-derive in read mode" against the finalized
`TypeDb`. It did not re-derive. It **re-inferred**, at a second, independent
instantiation:

- `lower_call` instantiated the callee's scheme afresh, so `id(1.5)` lowered as
  an unbound variable however firmly the call site had pinned it to `Float`, and
  `let values: Vec[Float] = Vec()` reached codegen with no element descriptor.
- `lower_method_call` repeated the catalog lookup against a receiver type of its
  own and then read the result off the entry's *pattern* — a fresh `?T` for each
  `Var("T")` in it — so `values.get(0)` on a `Vec[Float]` lowered as `?T`.
- `lower_if` and `lower_loop` recomputed joins inference had already computed,
  and `lower_match` did not even do that: it took the first arm's type, which is
  `Never` whenever that arm diverges.
- Nineteen `db.fresh_var()` fallbacks stood behind all of it. A fresh variable
  agrees with whatever the next use wants, so a lowering mistake surfaced three
  passes later as a missing descriptor and never as a diagnostic.

Monomorphization inherited the same split. `specialize` instantiated the callee's
scheme, unified *that copy's* parameters with the call site's argument types, and
then `follow`ed every type in the clone — but the copy and the clone shared no
variables. The copy's were the ones unification pinned; the clone's were the ones
lowering had written, from a *third* instantiation. `follow` found them exactly
as unbound as it left them, and the "specialized" clone was the generic original
with a new name (MONO-01).

## Decision 1: one map, filled at one place, holding every expression's type

`praxis_hir::Analysis::expr_types: HashMap<NodeKey, Type>` records **every**
inferred expression's type.

`NodeKey` is `(TextRange, SyntaxKind)` and is a distinct *type* from a token's
range. This is load-bearing rather than tidy: a `PATH_EXPR` node and the `Ident`
token inside it occupy the same range, so a range-keyed map beside `ref_types`
would have had one silently displace the other, precisely where a name reference
and its expression meet.

It is filled at `Inferer::infer_expr`, which every path that infers an expression
goes through — plus two entries for expressions inference reaches *without*
evaluating them:

- `infer_block_inner`, because a branch body, a loop body and a function body are
  `infer_block` calls, not `infer_expr` calls;
- a call's callee name and a record literal's head, which are `PATH_EXPR` nodes
  resolved through `refs` rather than evaluated.

"Inference visited this node" and "there is a recorded type for this node" are
therefore one statement, which is what lets the reader treat a miss as a bug.

## Decision 2: a miss is `Y099`, never a fresh variable

`Lowerer::node_ty` reports `DiagCode::InternalMissingType` and answers `Unit`.
It does not mint a variable. A fresh variable is the silent lie this whole repair
is tracking; a diagnostic naming the node kind and the span is a compiler bug
reported as one.

The type read is **deep-resolved**, memoized per handle. `follow` answers the
top-level representative only, so a `Vec[?T]` whose element a later `push` pinned
would still reach the backend with a variable where its element descriptor should
be. Lowering runs after every link is final, so resolving there is safe and the
typed tree comes out concrete.

## Decision 3: a function's type is its scheme's body, not an instantiation of it

`lower_fn` writes `scheme.body()` — the type inference arrived at, binders and
all — and `lower_param` writes the parameter's own monotype. Those are the same
variables the body's expressions carry, because they all come from the one map.

This is what makes Decision 4 possible, and it is why `TypedFn.fn_type` for a
generic function now *renders* with type variables in it. It did before, too;
they were merely a different set from the ones its own parameters used.

## Decision 4: a specialization substitutes binders; it does not follow an original

`mono::specialize` calls `instantiate_with_mapping`, which says which fresh
variable stands for which binder. Unifying that copy against the call site's
argument types **and its result type** decides them. What the clone is then
rewritten by is `TypeDb::substitute_params(t, scheme.binders(), chosen)` — a
substitution over the variables the clone actually contains.

The result is a witness in its own right, not a convenience. A callee whose
quantified variable appears only in its result — `fn empty() { Vec() }` is
`forall T. () -> Vec[T]` — has no argument to tell two instantiations apart. The
guard that skipped zero-argument call sites therefore dropped the generic
original without emitting any clone at all, and the cache key that ignored the
result would have collided `Vec[Int]` with `Vec[Text]` (MONO-02).

## Decision 5: a method name is not a name reference

`Analysis::method_refs: HashMap<TextRange, MethodRef>` holds the catalog entry
inference selected, the receiver type it selected it against, and the result type.

A method name resolves to a `MethodEntry`, not to a `SymbolId`, so it has no
entry in `refs` and never will. Its result used to be smuggled into `ref_types`
at the same range — a map only reference consumers read, whose entry was in fact
the *receiver*'s type, and which `hover` could never reach because `hover` asks
`refs` first. Hover over `v.len()`'s `len` answered nothing at all (HIR-02).

## Consequence: a function's signature is pinned before its body is inferred

`infer_fn` used to unify the signature placeholder with `(params) -> result`
*after* the body. A recursive call inside the body therefore unified a bare
variable with `(args) -> ?r`, and the result stayed unknown until the whole
function was done — so in

```praxis
fn build(n: Int) -> Vec[Int] {
  if n == 0 { Vec() } else { let v = build(n - 1); v.push(n); v }
}
```

`v` had no type but a variable when `v.push(n)` was inferred, and the catalog
could not resolve `push`. Nothing reported it, because lowering resolved the
method later, against a type inference had since pinned.

That is the re-derivation Decision 1 removes, so the ordering has to be right
here instead: the placeholder is unified with the signature **before** the body
is inferred. This is strictly more checking than before — a recursive call's
arguments are now checked where they are written rather than at the end.

## Alternatives rejected

**Key the map by `TextRange` alone.** Cheapest, and unrepresentable-collision is
the whole reason `NodeKey` carries a kind. `ref_types` already occupies token
ranges.

**Let lowering fall back to its own catalog lookup when `method_refs` misses.**
This is what the old code did unconditionally, and it is the bug: two passes
answering one question, the later one winning silently. Lowering now reports
`Y110` on a miss and keeps the receiver and argument subtrees, which are
well-formed in their own right — discarding them, as it used to, lost every
closure and capture inside them.

**Defer unresolved method calls to a second inference pass.** The principled fix
for a receiver whose type is pinned only later by an unrelated constraint. Not
needed: pinning the signature first covers the recursive case, which is the one
the corpus has, and a retry pass has to run before generalization or it can link
a variable a scheme has already quantified. F19's SCC-ordered binding groups are
where the general case belongs.

**Have `lower_block` read the block node's type.** A block reached as an `else
if` body has no `BLOCK_EXPR` node at all — lowering synthesizes it — so the tail's
type is the only answer available for every block. It is also provably the same
answer since TY-16 aligned the two tail rules.

## Consequences

- The typed tree is concrete. Every `TypedExpr.ty` is the type inference
  computed at that node, resolved to its leaves.
- `mono::resolve_expr` is a fold over F20's child walker; the second of the three
  hand-written 29-arm walks over `TypedExpr` is gone, and pattern types — which
  it never touched — are specialized too.
- `Lowerer::loop_results` is deleted. It existed to recompute a join inference
  had already computed (TY-21); reading it is one line.
- `symbol_type`, `call_result_type` and `param_type` are gone, with the five
  `db.instantiate` re-instantiations and all nineteen `db.fresh_var()` fallbacks.
- **Not done:** the MIR verifier's no-`Opaque`-at-a-descriptor-site rule (H10)
  and the `MirType::Opaque` sites in `praxis-mir`'s builder. A fused pipeline's
  items and accumulators still have no static type, and that is MIR-05 in S21 —
  see the progress note.
