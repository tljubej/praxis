# Milestone 7 Part 1 report & Milestone 7 Part 2 handover

**Project:** Praxis
**Date:** 2026-07-24
**Status:** M7 Part 1 complete (WS1–WS5). All §19.7 deliverables for records,
enums, and pattern matching are landed and green. Ready to begin Part 2 (WS6–WS10:
structural equality/hashing, closures, monomorphization, carryovers, docs).

> **For a fresh context:** read this document, then `praxis_technical_design.md`
> (the contract — §4.5 records, §4.6 enums, §4.10 closures, §5.5 equality/hashing,
> §13.6 monomorphization, §19.7 Milestone 7). The rest of this file tells you
> what exists, where, and what to do next.

---

## 1. What landed in M7 Part 1

M7 Part 1 delivered the core data-modeling layer: **nominal records, enums,
pattern matching, and three carryover fixes**. All work is on `main`, committed
as `M7-WS1` through `M7-WS5` (plus one fix commit). **404 tests passing**, `just ci`
clean.

### What Part 1 built, by workstream

| Workstream | What landed | Key commits |
|---|---|---|
| **WS1: Type-system foundation** | `TypeData::Record`/`Enum` via def-id indirection (ADR-025). Side-tables on `TypeDb` (`record_defs`, `enum_defs`). Unify/generalize/occurs/instantiate extended to recurse into record fields and enum payloads. `Collection` added to `instantiate_walk` (latent M5 bug fix). Two-pass resolver (register top-level names first). Provisional anon records migrated from interleaved-tuple to real `Record` type. `SymbolKind::Struct`/`Enum` added. | `1644488` |
| **WS2: Short-circuit carryover** | `\|\|` short-circuits via MIR branches (rhs not evaluated when lhs is true — verified by div-by-zero-in-rhs test). `!` implemented as eq-with-false. | `1caf57e` |
| **WS3: Nominal records** | `struct Point { x: Int, y: Int }`, `Point { x: 3, y: 4 }` (with punning `Point { x, y }`), `p.x` field access. Full vertical slice: SyntaxKinds, parser (with `no_struct_literal` flag for if/while conditions), AST wrappers, resolver, inference (struct type registration, literal field-type unification), HIR (`TypedExpr::RecordLit`/`FieldGet`), MIR (`AllocKind::Record` + `Inst::LoadField`), codegen (compile-time `RecordSchema` build/leak/cache), runtime ABI (`praxis_alloc_record`/`set_field`/`record_field`). Records survive GC. | `8b973b9` |
| **WS4: Enums** | `enum Tile { Empty, Wall, Number(Int) }`, `Number(42)` payload-variant construction, bare `Empty` zero-payload-variant-as-value. Runtime `EnumPayload { tag: u32, items: Vec<GcRef> }` + `ENUM` descriptor (TypeId 9). ABI wrappers `praxis_alloc_enum`/`enum_set_payload`/`enum_tag`/`enum_payload`. Enums survive GC. | `cb77399` |
| **WS5: Pattern matching** | `match t { Empty => 1, Number(n) => n, _ => 0 }`. Parser (newline-separated arms, `is_pattern_start` helper), AST (`MatchExpr`/`MatchArm`/`Pattern`/`PatternKind`), resolver (pattern-variable bindings in child scopes), inference (scrutinee/pattern unification, variant payload types), HIR (`TypedExpr::Match` + `TypedMatchArm` with `variant_idx` + `bindings`), MIR (`lower_match` as tag-compare branch chain + `Inst::EnumTag`/`EnumPayloadGet`), codegen (`uload32` tag read, native `icmp` comparison, ABI-wrapped payload extraction). **Exhaustiveness checking (Y120/Y121) is NOT yet implemented** — it's scaffolded in the HIR but the checker pass is not wired. | `ce8255b`, `9389581` |

### §19.7 acceptance criteria — partial status

| Criterion | Status | Notes |
|---|---|---|
| Store parser-generated records in vectors and maps | ✅ vectors; ❌ maps (M8) | Records store in `Vec` fine; `Map`/`Set` are M8 |
| Use tuples and records as set/map keys | ❌ | Needs WS6 (eq/hash descriptors) + M8 containers |
| Compile closure pipelines with captured values | ❌ | Needs WS7 (closures) |
| Reject non-exhaustive matches | ❌ | Needs exhaustiveness checker (WS5 follow-up) |

### Three bugs found and fixed during WS5

These are documented so the next agent doesn't re-discover them:

1. **Match arm separation**: §4.6 grammar uses newline separation (no commas).
   Fixed `parse_match` to check `is_pattern_start(peek())` after each arm.
2. **Payload-less variant ambiguity**: `Empty` in a pattern is a bare Ident,
   indistinguishable from a variable bind `x`. Fixed `lower_pattern` to check
   the scrutinee's enum type and resolve bare names as variants.
3. **Cranelift 0.134 `uload32`**: returns I64 (not I32) — no `uextend` needed.
   Also, `EnumPayloadGet` must use the ABI wrapper because `Vec`'s `repr(Rust)`
   field order is not stable across Rust versions.

---

## 2. Architecture summary (what exists now)

```
COMPILE TIME                                  RUNTIME
praxis-types:                                 praxis-runtime:
  TypeData::Record { def: RecordDefId }         RecordSchema/RecordPayload/RECORD descriptor
  TypeData::Enum { def: EnumDefId }             EnumPayload { tag, items }/ENUM descriptor
  TypeDb.record_defs / enum_defs               praxis_alloc_record/set_field/record_field
  register_record/register_enum/anon_record    praxis_alloc_enum/set_payload/tag/payload
  unify/generalize/instantiate recurse         praxis_enum_tag returns boxed Int (unused now)
  into record/enum children
praxis-hir:                                   praxis-codegen-cranelift:
  two-pass resolver                            record_schema_for (Box::leak + cache)
  infer_struct/infer_enum                      AllocKind::Record/Enum lowering
  TypedExpr::RecordLit/FieldGet/EnumVariant/   Inst::LoadField/EnumTag/EnumPayloadGet
    Match                                      native icmp for IntCmp (avoids alloc safepoints)
  lower_match (tag-compare branch chain)       compile() takes &TypeDb for schema building
```

### ABI version: 3 (bumped from 2 in WS3)

New runtime symbols: `praxis_alloc_record`, `praxis_record_set_field`,
`praxis_record_field`, `praxis_alloc_enum`, `praxis_enum_set_payload`,
`praxis_enum_tag`, `praxis_enum_payload`. All registered in `symbols.rs` and
`module.rs` sym_names array.

### Key design decisions baked in (from the M7 plan, ADR-025)

1. **Def-id indirection**: `TypeData::Record`/`Enum` carry only a `u32` index
   into side-tables on `TypeDb`. Keeps `Type` cheap and avoids recursive size.
2. **Closures reuse `TypeData::Func`** (planned for WS7) — the capture
   environment is a runtime concern, not a type-system one.
3. **Closure syntax = `|params| expr`** claiming bare `PIPE`. `||` stays
   logical-or. Or-patterns deferred.
4. **Record field access = `p.x`** disambiguated from method calls in the DOT
   loop (`parse.rs`): IDENT not followed by `(` → field; followed by `(` → method.
5. **Compile-time schema registry**: the codegen builds `RecordSchema` from the
   def-id, `Box::leak`s to `&'static` (cached in a process-wide `OnceLock`),
   and embeds the address as an immediate.
6. **Scope boundary**: M7 Part 1 lands the eq/hash *capability infrastructure*
   (planned WS6). End-to-end "use as set/map keys" closes in M8 when containers
   exist.

---

## 3. Known limitations / deferred

### Must-do for M7 completion (Part 2 workstreams)

- **Exhaustiveness checking** (WS5 follow-up): the `exhaustive.rs` module is not
  yet created. A `match` on an enum must cover all variants or have a `_` arm;
  non-exhaustive → `Y120`, unreachable arm → `Y121`. The MIR `lower_match`
  already falls through to Unit for unmatched cases, but the compile-time check
  is missing.
- **Structural equality & hashing** (WS6): `RECORD`/`ENUM` descriptors have
  `equals: None`/`hash: None`. Records/tuples/enums cannot be compared with `==`
  or used as keys yet. The `==` operator only works on Int/Bool. Need: implement
  `equals`/`hash` callbacks on RECORD/ENUM descriptors; add `SupportsEq(T)`/
  `SupportsHash(T)` capability resolution (§5.4/§5.5); extend `==`/`!=` to
  equatable records/tuples/enums; lower to `praxis_struct_eq` runtime call.
- **Closures & GC environments** (WS7): no closure syntax, no capture analysis,
  no `VarCell` (M5 carryover — "only needed for closure capture"). Need: claim
  bare `PIPE` in `parse_prefix`; capture analysis (`praxis-hir/src/capture.rs`);
  `ClosurePayload { fn_ptr, env: Vec<GcRef> }` + `CLOSURE` descriptor; ABI
  wrappers; MIR (`AllocKind::Closure`); mutable captures via `VarCell`.
- **Monomorphization** (WS8): generics still rejected with `Y100`
  (`lower.rs:387`). `catalog.rs::pattern_matches` still uses `Var("T")` string
  wildcard. Need: `praxis-hir/src/mono.rs` pass between `lower` and
  `lower_module`; instantiate polymorphic callees at call sites; cache by
  `FunctionId + canonical type args`; retire `pattern_matches` wildcard.
  Also: `Vec[T]()` constructor still ignores the type arg (M5 carryover).
- **Input-parser carryovers** (WS9): `child_descriptor` doesn't recurse for
  nested constructors (M6 carryover); `walk_template` is a stub (M6 follow-up).
- **Docs & corpus** (WS10): no M7 handover doc; ADRs 026/027 not written;
  README still says "Milestone 6 complete".

### Carryover items from earlier milestones (some addressed, some deferred)

- ✅ **Short-circuit `||`/`!`** (M4 carryover) — done in WS2.
- ✅ **`child_descriptor` recursion** — deferred to WS9.
- ❌ **`Vec[T]()` type arg honored** (M5 carryover) — deferred to WS8 (ties into
  monomorphization).
- ❌ **Standalone template-literal matching** (M6 follow-up) — deferred to WS9.

---

## 4. What Milestone 7 Part 2 should do

**Remaining workstreams, dependency-ordered:**

### WS6 — Structural equality & hashing (§5.5, §5.4)
- Implement `equals`/`hash` on `RECORD` descriptor (element-wise through schema,
  recursing via field descriptors; short-circuit on first non-equal field).
- Add eq/hash to tuple and enum descriptors (tag-equal then payload-equal).
- Compile-time capability check: `SupportsEq(T)`/`SupportsHash(T)` recursive —
  record/tuple/enum equatable iff all fields are; functions never (§5.5).
- Extend `==`/`!=` to equatable records/tuples/enums; lower to `praxis_struct_eq`.
- Tests: `Point{1,2} == Point{1,2}`, tuple eq, enum eq, non-equatable → diag.
- ADR: `026-structural-equality-hashing.md`.

### WS7 — Closures & GC environments (§4.10)
- Syntax: `CLOSURE_EXPR` kind; claim bare `PIPE` in `parse_prefix` → `parse_closure`.
- Capture analysis (`praxis-hir/src/capture.rs`): detect free vars; mutable
  (`var`) captures via `VarCell` (M5 carryover).
- Inference: closure type = `Func`; NOT generalized if mutable capture.
- HIR: `TypedExpr::Closure { params, body, captures, fn_type }`.
- MIR: `AllocKind::Closure { fn_name, captures }`; synthetic nested MIR function.
- Runtime: `ClosurePayload { fn_ptr, env: Vec<GcRef> }` + `CLOSURE` descriptor;
  `praxis_alloc_closure`/`praxis_closure_call`; `VarCell` for mutable captures.
- Tests: `let o=10; let f=|x| x+o; f(5)`→15; mutable capture; closure in Vec.
- ADR: `027-closure-representation.md`.

### WS8 — Monomorphization (ADR-018)
- New pass `praxis-hir/src/mono.rs` (between `lower:264` and `lower_module`):
  instantiate polymorphic callees, cache by `FunctionId + canonical type args`.
- Remove Y100 gate (`lower.rs:387`). Replace `pattern_matches` `Var("T")` wildcard.
- `Vec[T]()` — honor type arg.
- Tests: `fn id(x){x}` runs; distinct instances; cache-hit.

### WS9 — Input-parser carryovers
- `child_descriptor` recursion for nested constructors.
- `walk_template` real standalone template matching.

### WS10 — Docs, handover, corpus
- `docs/handovers/07-milestone-7-handover.md` (M7 report & M8 handover).
- ADRs 026/027; update 018 (done), 024 (superseded); `docs/decisions/README.md`.
- `README.md` → "Milestone 7 complete".
- Corpus fixtures: records-in-Vec, enum match, closure pipeline.

### Exhaustiveness checking (WS5 follow-up)
- New `praxis-hir/src/exhaustive.rs`: usefulness algorithm. Enum → closed
  variant set must be covered; Bool → true/false; other types require `_`.
  Non-exhaustive → `Y120`, unreachable → `Y121`.

---

## 5. Test inventory

| Suite | Count | Location |
|---|---|---|
| Parser (incl. read/parse/match grammar) | ~60 | `praxis-parser/src/parse.rs` |
| HIR (incl. type synthesis, match lowering) | ~65 | `praxis-hir/src/*.rs` |
| Runtime (incl. descriptors, GC, enum layout) | ~75 | `praxis-runtime/src/*.rs` |
| Input parser DSL | ~26 | `praxis-input-parser/src/*.rs` |
| JIT end-to-end (incl. records, enums, match, short-circuit) | ~44 | `praxis-codegen-cranelift/tests/jit.rs` |
| Types (incl. record/enum unify/generalize) | ~44 | `praxis-types/src/types_tests.rs` |
| CLI | ~10 | `praxis-cli/tests/` |
| Other | ~80 | various |
| **Total** | **404** | `cargo test --workspace` |

---

## 6. Where to start for Part 2

**WS6 (structural equality & hashing) is the natural next step** — it's
self-contained, doesn't depend on closures or monomorphization, and closes
one of the four §19.7 acceptance criteria ("Use tuples and records as set/map
keys" — the eq/hash machinery; end-to-end keying needs M8 containers).

**Starting files:**
- `praxis-runtime/src/records.rs:91` — `RECORD` descriptor (`equals: None`,
  `hash: None`). Implement the callbacks.
- `praxis-runtime/src/descriptor.rs:139` — `EqualsFn`/`HashFn` types.
- `praxis-hir/src/infer.rs` — `infer_bin` for `==`/`!=` (currently Int/Bool only).
- `praxis-mir/src/build.rs` — comparison lowering (needs a `praxis_struct_eq` path).

**Key M7 design point for WS6:** the capability check (§5.4) is internal —
diagnostics must never mention "trait" or "capability" names. A record is
equatable iff all fields are equatable; functions are never equatable (§5.5).

**Key M7 design point for WS7:** closures capture values automatically (§4.10).
Mutable captures use GC-managed environment cells (`VarCell`). There are no move
closures, borrow captures, or lifetime rules. The `VarCell` from M5 ("deferred
to M7, only needed for closure capture") finally becomes load-bearing.

**Key M7 design point for WS8:** monomorphization inserts between `lower`
(typed HIR) and `lower_module` (MIR), per ADR-018. The user never writes type
arguments. Cache instances by `FunctionId + canonical type arguments` (§13.6).
