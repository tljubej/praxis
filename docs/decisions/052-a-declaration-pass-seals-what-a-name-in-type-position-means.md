# ADR-052: A declaration pass seals what a name in type position means

**Date:** 2026-07-29
**Status:** Accepted
**Milestone:** Repair (S13 — TY-08…TY-15; the part of F19 those findings need)

## Context

Six of S13's eight findings are one question asked at six sites: *what type does
this written annotation denote?* Each site answered it differently, and every
wrong answer had the same shape — a fresh type variable, which unifies with
anything and therefore reports nothing.

- **TY-08.** `praxis_ast::TypeRef::cast` accepted only `SyntaxKind::TYPE_REF`.
  The parser emits `TUPLE_TYPE` for `(Int, Text)` and `FN_TYPE` for
  `(Int) -> Int`, so six accessors — `let`, `var`, parameter, return type,
  struct field, enum payload — answered `None` for those two shapes. The
  annotation was not rejected; it was invisible.
- **TY-09.** A user `enum` in type position resolved by name and then hit
  `scalar_from_name`, whose fallback asked for a `SymbolKind::Struct` and
  answered `None` for anything else. `lookup_enum_type` existed, was correct,
  and was never called.
- **TY-10.** `struct`/`enum` types were registered when inference *reached* the
  declaration, in source order. Resolution has been two-pass since M7 so the
  *name* is visible above its declaration — the *type* was not.
- **TY-11.** Annotation validation asked only whether the name resolved. `out`
  resolves.
- **TY-13.** `infer_assign` looked the assignment target up in a `ScopeTree` the
  inferer had moved out of resolution and then pushed *empty* child scopes onto.
- **TY-12** turned out to be S11's `Y007` and needed no code.

The plan's F19 sketch answers all of this with a sealed `TypeEnv` plus a
`DeclGroup` driver. What S13 needs is the environment; the SCC-ordered binding
groups that F19 also describes are for mutual-recursion *generalization*, which
no S13 finding names.

## Decision

### 1. The three annotation node kinds are one predicate, and the wrapper accepts all of them

`SyntaxKind::is_type_node` is `TYPE_REF | TUPLE_TYPE | FN_TYPE`, written once.
`TypeRef::cast` reads it, and so does the recursion in type resolution — which
previously spelled the list out twice more.

Resolving a `TYPE_REF` then tells its three shapes apart by *what the node
holds*, not by where it was found: a direct `Ident` token is a name, one type
child is a parenthesized group, and two or more are a constructor followed by
its arguments. The group case had no reading at all before, because a group's
`Ident` sits inside a nested `TYPE_REF` — so `(Int)` was a fresh variable. `()`
is `Unit`, which is what makes `() -> Int` nullary rather than a function of one
invented argument.

### 2. A name in type position resolves to the symbol *resolution* chose

Name resolution records every annotation name in a new
`NameResolution::type_refs`, keyed by range. Inference reads the answer; it does
not repeat the lookup.

`type_refs` is a **separate map from `refs`** rather than an addition to it. An
annotation name is not a value reference: nothing instantiates a scheme for it,
hover has nothing to say about it, and everything that walks `refs` would have
had to learn to skip it.

### 3. Type declarations are registered in dependency order, before any expression

`praxis_hir::decl::declare` runs first, inside the declaration group's level. A
declaration is *ready* when no annotation inside it names a type still pending;
readiness is asked by walking the item's `Ident` tokens through `type_refs`,
which only annotation names are in, so the declaration's own name and its field
and variant names never match.

**A cycle registers anyway, in source order.** `struct A { b: B }` /
`struct B { a: A }` has no fixpoint in a type system without equirecursive
types, and unifying placeholders into one would fail the occurs check. What is
left when nothing more becomes ready is registered with what is known, and a
member that still has no type becomes a fresh variable — which is what every
unresolvable annotation has always done. The alternative, reporting the cycle,
is a language decision about recursive data that S13 has no mandate to make.

### 4. The environment is sealed, and it is what makes the lookup total

`TypeEnv` has no mutator outside `praxis_hir::decl`. `declare` is its only
constructor. So "a type name the resolver accepted, whose `Type` does not exist
yet" is not a state expression inference can observe — it was the *normal* state
before, for every forward reference.

`Annotations` — the resolver from decision 1 — borrows the environment rather
than owning it, so the declaration pass (building it) and inference (reading a
sealed one) run the same code. Two near-duplicate copies is how
`lookup_struct_type` and `lookup_enum_type` came to disagree.

### 5. The prelude holds values and types, and only one may appear in type position

`SymbolKind::BuiltinType` is the seeded scalar type names; `SymbolKind::Builtin`
is `out`, `panic`, `Vec` and the rest. `SymbolKind::is_type` — `BuiltinType |
Struct | Enum` — is the one place the set is written.

A name in type position that resolves to a *value* is `N003`, not `N002`. `N002`
would be a lie about which mistake was made: the name is known, and it is the
wrong sort of thing.

The collection constructors and `Option` are checked *before* the lookup and
skipped, because they are compiler-owned type names with no scope symbol — and
`Vec` is also a prelude *value*, so a kind check that reached it would reject
`Vec[Int]`.

### 6. `ScopeTree` belongs to the resolver

The `Inferer` has no scope tree and no `scope: ScopeId` parameter. Every binding
question is answered by the range-keyed maps: `refs` for a reference, `decls`
for a declaration site, `type_refs` for a name in type position. `Inference`
still carries the tree through to `Analysis`, untouched, for the LSP.

### 7. A compound assignment reports only against a known type

`x += e` is arithmetic, so its target must be `Int` or `Float` (§4.12) — `Y010`.
But the check fires only when the target's type *resolves to something*: an
unbound variable is a type a later use may still pin, and reporting it would
turn `fn f(a) { a += 1 }` into an error about a function that is fine. Both
halves are gated.

## Alternatives considered

- **Make `TypeRef` an enum, like `Expr`.** `Expr` is an enum because its
  consumers dispatch on the variant. Every `TypeRef` consumer passes it straight
  to one resolver, which dispatches on `node.kind()` — so the enum would add a
  second dispatch and a second place for the kind list to drift.
- **Add annotation names to `refs`.** Rejected under decision 2.
- **Order type declarations by a topological sort with an explicit cycle
  report.** A sort needs the same dependency edges the readiness check already
  computes, and then needs a decision about what a cycle *means*. Rounds
  terminate, need no edge set, and leave the language question open.
- **A fixpoint over placeholder variables for recursive types.** This is what
  would make `struct Node { next: Node }` work, and it is a language feature
  (equirecursive or iso-recursive types), not a bug fix.
- **Pin an unconstrained compound-assignment target to `Int`.** It would make
  the check total, and it would silently change inference for every unannotated
  numeric parameter. Deferred to S17, where TY-31's `Y015` gives numeric
  constraints a channel.
- **Land F19's SCC binding groups here.** No S13 finding needs them. They are
  what mutual-recursion *generalization* needs — the residue S11 recorded — and
  they belong with whichever stage takes that on.

## Consequences

- **A `let`, a parameter, a `for` binding and a pattern binding are immutable,
  and now say so** (`Y009`). This is the largest source of newly-rejected
  programs in the stage. The corpus triage found none: every compound assignment
  and every reassignment in `tests/`,
  `crates/praxis-cli/tests/fixtures` and `crates/praxis-codegen-cranelift/tests/jit.rs`
  targets a `var` holding an `Int`.
- **A closure parameter's annotation is checked**, which it was not — the
  resolver never walked it.
- **A variant pattern's constructor is recorded as a reference.** It is one, and
  inference needs the symbol now that it cannot look the text up. It is
  deliberately *not* reported when it fails to resolve: naming a variant the
  scrutinee's type does not have is `Y122` (HIR-07, S16), not `N001`.
- **`empty_vec_float_has_the_float_element_descriptor_before_any_push` is not
  TY-08's after all.** Inference does pin the element type — pushing an `Int`
  into a `let values: Vec[Float] = Vec()` is a `Y001`. What loses it is
  `lower_call`, which re-instantiates the callee's scheme instead of using the
  type inferred at the call site. That is F15/MONO-01 in S15, and the test's
  `#[ignore]` reason now names it.
- **Adding a declaration form means declaring it in the pass.** `infer_top_stmt`
  no longer dispatches `struct`/`enum`; a new type declaration that only teaches
  the inferer about itself will silently have no type.
