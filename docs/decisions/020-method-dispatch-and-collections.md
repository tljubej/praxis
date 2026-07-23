# ADR-020: Method-call dispatch through the built-in catalog

**Date:** 2026-07-23 · **Status:** accepted

## Context

§16.2 requires a single structured method catalog that every compiler stage and
the LSP consume (rule 20.3: "never duplicate type or method knowledge"). ADR-010
shipped the catalog schema (`MethodCatalog`), the `TypePattern` shape language,
and the `Type → TypePattern` bridge in M2 — but expression-level `.method()`
dispatch was deferred to M5.

M4's JIT could only execute scalar arithmetic: `Text`/`Tuple`/`Vec` allocated
but had no method surface, and `expr.method(args)` had no parser, inference, or
lowering support at all. The §19.5 deliverable "basic collection methods and
structural formatting" requires the full vertical slice.

## Decision

Wire the ADR-010 catalog bridge into every stage of the pipeline:

1. **Type system:** add `TypeData::Collection { ctor, args }` to the inference
   engine (`praxis-types/src/data.rs`), with a `TypeDb::collection()`/`vec()`
   constructor, unify rule (element-wise), occurs check, level lowering, and
   pretty-printing. The catalog bridge (`type_to_pattern`) now handles
   `Collection` patterns so `Vec[Int]` maps to the catalog's `Vec[T]` entry.

2. **Parser:** add `.method(args)` postfix syntax with left-associative chaining
   (`v.push(1).len()`). The `METHOD_CALL_EXPR` node wraps the receiver as its
   first child (via the rowan checkpoint trick). Also add `Vec[T]` type-arg
   annotation parsing.

3. **AST:** `MethodCallExpr` wrapper with `receiver()`/`method_name()`/
   `arg_list()`, registered in the `Expr` enum and `cast_from_child`.

4. **Inference:** `infer_method_call` infers the receiver, looks up the catalog
   via `crate::catalog::lookup(db, catalog, receiver_ty, name, arity)`, unifies
   param types with arg types, and returns the result type. The catalog is
   threaded into the `Inferer` via a process-wide `OnceLock`.

5. **HIR lowering:** `lower_method_call` resolves the method (same catalog
   lookup), records the runtime lowering symbol (e.g. `praxis_vec_push`), and
   emits `TypedExpr::MethodCall`. Unknown methods produce a `Y110` diagnostic.

6. **MIR:** `TypedExpr::MethodCall` lowers to `Inst::Call` with
   `CallTarget::Runtime(symbol)`. The `Vec()` and `out()` builtins also lower to
   runtime calls (`praxis_vec_new` / `praxis_write_stdout`).

7. **Codegen:** `CallTarget::Runtime(name)` resolves through the JIT's
   registered symbol table with a variadic signature.

8. **Catalog (`praxis-stdlib/src/builtins.rs`):** `builtin_catalog()` returns the
   finalized, duplicate-free table with M5's entries: `Vec.push/len/get/is_empty`
   and `Text.len/is_empty/get`.

## Reason

- The catalog is the single source of truth (rule 20.3): the compiler never
  hardcodes method names or signatures — it reads the catalog. Adding a method
  is one catalog entry + one runtime wrapper.
- `VecPayload` changed from `Box<[GcRef]>` (M3) to `Vec<GcRef>` (M5) so `push`
  mutates in place (§11.1: `push -> Unit`, receiver mutated). The non-moving GC
  keeps the `VecPayload` address stable even as its internal buffer grows.
- Threading the catalog via `OnceLock` (not a constructor parameter) keeps the
  `analyze`/`lower` public signatures unchanged.

## Consequences

- A bug in the catalog bridge (e.g. a missing `Collection` arm) silently breaks
  method resolution; the `Y110` diagnostic is the user-visible symptom.
- The `Vec()` builtin defaults to `Int` elements (the descriptor arg is null);
  a real `Vec[T]()` type-arg construction is a follow-up (the type annotation
  parses, but the constructor doesn't read the element type from it yet).
- The HIR block-ordering bug fixed along the way (a trailing expression before
  an assignment was mis-ordered) was a latent M4 bug exposed by the
  `v.push(i); i = i + 1` pattern.
