# ADR-064: A subscript is a catalog row, and a store is a different row from a read

**Date:** 2026-07-30
**Status:** Accepted — implemented
**Milestone:** Repair (stage S25 — REP-16)

## Context

```praxis
counts[point] += 1
```

`P002`. And `let value = map[key]` was a `P001` at the `[`: the postfix loop in
`parse.rs` had arms for `L_PAREN` and `DOT` and none for `L_BRACK`, so **there was
no subscript syntax at all** — in a language whose design doc writes
`map[key]` (§4.7), `counts[key] += 1` (§6.2), `grid[x, y]` (§6.4), and
`counts[point] += 1` inside §3.3's representative program, which is S25's own
acceptance criterion.

Four pieces were missing: a read form, a store form, an lvalue in the assignment
grammar, and a per-collection lowering. This ADR records the four decisions they
turned into.

## Decision 1: dispatch through the method catalog, under names no program can spell

A subscript is dispatched on the receiver's shape *and its arity* — `Map[K, V]` at
one index, `Grid[T]` at two — which is exactly what §5.7's closed table is keyed
by. So the rows live there, under `[]` and `[]=`
(`praxis_stdlib::catalog::INDEX_READ` / `INDEX_STORE`), and inference resolves a
subscript through the same `infer_catalog_call` a method call goes through.

The names are **not identifiers**, which is what keeps them out of source: the
parser accepts only an `Ident` after `.`, so `m.[](k)` cannot be written and the
subscript grammar is the rows' only caller. A gate asserts the property rather
than trusting it.

This is not only economy. The shared path carries four things that are each
load-bearing, and a second dispatch table would be a second place for one of them
to be missing:

- **Bidirectional argument inference**, so a store's value unifies with the
  collection's element type before it is inferred.
- **The `HasMethod` deferral** for a receiver that is still a variable, so
  `fn first(m, k) { m[k] }` infers and the requirement is answered by whatever the
  call site puts in `m`'s place (TY-30, ADR-057). A subscript is therefore exactly
  as generic as a method call and no more: the requirement *pins* its receiver, so
  one function serves one receiver kind, and two kinds through one function is the
  same `Y001` that `fn size(c) { c.len() }` gives.
- **TY-31's bounds** and **TY-32's collection invariants**, so a key stored
  through `m[k] = v` must still be hashable.
- **Monomorphization**, because the resolved row rides on the `MethodRef` map that
  lowering reads and nothing else (F15/HIR-02).

`MethodRef`s are keyed by `TextRange`. A method call keys on its name token; a
subscript has no name token and keys on **the whole `INDEX_EXPR` node**, whose
range always ends in `]` and so can never collide with an identifier's.

## Decision 2: a store is its own row, and the two surfaces are not the same set

Six collections read: `Vec`, `Deque`, `Text`, `Map`, `Counter`, `Grid`. **Three
store:** `Map`, `Counter`, `Grid`.

> **Superseded in part (ADR-124, 2026-08-04).** The feature this decision names
> below was built: `Vec` and `Deque` have `INDEX_STORE` rows now, so **five**
> store and the one reader left without one is `Text`, which is immutable. The
> decision here — that a store is its own row, not the read written backwards —
> is what made adding them two rows rather than a second dispatch.

The store rows are not the read rows written backwards, because the sets differ and
the semantics differ:

- **A `Vec` reads through `v[0]` and has no element store anywhere in the
  language** — no `v[0] = x`, and no `.set` method either. So `v[0] = x` is a
  report (`Y020`) rather than a silently invented operation. Giving `Vec` and
  `Deque` element stores is a *feature*, registered as its own finding rather than
  smuggled in here.
- **A `Counter`'s read is zero-defaulting and its store is not.** §6.2's "absent
  values read as zero" is what makes `counts[key] += 1` work on a key never seen,
  and it is a property of the read alone.

`Map`'s two *reads* are also two different wrappers, and that is §4.7's own choice
rather than an implementation detail: "indexing a missing map key faults instead of
returning an option… the user chooses between explicit absence with `.get` and
assertion-like access with indexing." So `praxis_map_index` is a new wrapper beside
`praxis_map_get`, differing in one line. Pointing both rows at one wrapper would
take the choice away from the user, and a gate asserts they are distinct.

The fault it raises is `IndexOutOfBounds` — an index the collection does not hold,
which is what that kind's doc already describes. A dedicated `MissingKey` would
read better and is owed to the next stage that spends an ABI bump, exactly as
`clamp`'s empty range is (ADR-058).

`praxis_counter_set` is the one other new wrapper: `praxis_counter_inc` adds
exactly one, so it can express neither `c[k] = n` nor `c[k] += n`.

## Decision 3: a compound store is a read-modify-write that evaluates its place once

`m[k] += v` is **not** desugared into `m[k] = m[k] + v`. The desugared form names
the receiver and every index twice, and MIR lowers each `TypedExpr` where it
stands, so `m[f()] += 1` would call `f` twice — a silently wrong answer of exactly
the kind this repair exists to remove.

So `TypedStmt::IndexAssign` carries the receiver, the indices, the value, the store
symbol and (for a compound operator) the read symbol, and MIR lowers the receiver
and indices **once** into locals that both the read and the write use. Two gates
hold the line: one counts the instructions, and one observes the side effect of an
index expression and a receiver expression that log when they run.

The read symbol comes from the catalog looked up against **the receiver type
inference resolved the store against**, not one lowering derived itself — the
HIR-02 mistake, and the same mistake `get_symbol_for` makes for `for` (REP-15).

The arithmetic is `Int`'s, through the same `lower_int_binop` a compound assignment
to a local uses, and the value goes through the channel as `Numeric` for
`infer_assign`'s reason (TY-31): `fn bump(m, k) { m[k] += 1 }` leaves the value a
variable, and answering "not numeric" about a variable is wrong.

## Decision 4: an assignment target is an expression, and whether it names storage is inference's answer

`ASSIGN_STMT`'s target is a *token*, and its single expression child is the value —
so a target that is itself an expression cannot be told from the value. Rather than
widen it (and revisit every consumer), a subscript target gets a second statement
kind, `PLACE_ASSIGN_STMT`, whose two expression children are the target and the
value in source order. `parse_stmt` still routes `name = …` to `parse_assign_stmt`
on the token after the name, so the bare-name path is untouched.

The parser wraps **whatever expression** precedes the operator, and does not try to
decide whether it is a place. That is deliberate:

- `f() = 1` and `p.x = 1` are well-formed *shapes* whose mistake is that they name
  no storage. The parser's answer was "expected a statement separator", which says
  nothing about it; inference's is `Y021`, "the left side of an assignment must be
  a name or an index".
- The alternative is a lookahead over a balanced bracket group, or threading the
  top-level node kind back out of `parse_expr_bp`. Both encode "is this a place" in
  the grammar, where it is a question about what the target *denotes*.

An empty subscript `m[]` is the one thing the parser does reject, because a
subscript selects something: an empty index list is a syntax error rather than an
arity the catalog happens to have no row for.

## Consequences

- **`Y020` and `Y021` are spent, both in inference.** `Y020` covers three shapes: a
  receiver with no subscript, a receiver with no *store*, and the right receiver at
  the wrong arity (`grid[x]` — §6.4 spells it `grid[x, y]`). Emitting them in
  inference rather than lowering is REP-12's asymmetry: `praxis check` does not run
  lowering, so a program reported only there is clean under `check` and fails under
  `run`. **`Y022` and `N007` are the next free codes**; ADR-051 is amended with
  both rows.
- **A deferred subscript that resolves to a non-indexable receiver is reported
  where a deferred *method* is not.** `resolve_deferred_method` deliberately leaves
  a missing method alone because lowering owns `Y110` and has the name span. A
  subscript has no such report at lowering, so `fn first(m, k) { m[k] }` applied to
  a `Set` would have been accepted and then silently dropped. It reports `Y020` at
  the use site instead.
- **`stmt_exprs` exists**, next to F20's `TypedExpr::children`, and for the same
  reason: three walks over `TypedStmt` — MIR's closure collection, MIR's
  function-value collection, and the debugger's purity check — named the fields by
  hand, and `IndexAssign` is the first statement with three expressions. Two test
  walkers use it too.
- **`p` rejects an indexed assignment** (ADR-034), with its own message: the write
  outlives the expression, which is a stronger reason than a local assignment's.
- **`adv_map_index_missing_key_does_not_fault_current_behavior` is replaced.** It
  asserted that `m["missing"]` does *not* fault, and it passed for a reason that had
  nothing to do with maps: with no subscript grammar, `let v = m["missing"]` parsed
  as `let v = m` plus a recovered statement, and the `v` it compared was the map.
  The test now asserts §4.7's sentence, both halves.
- **`Counter[(Int, Int)]()` moves from a `P002` to a `Y020`.** `Ident [ … ] (` is
  now genuinely ambiguous between a subscript and REP-09's explicit constructor
  type arguments, and the postfix arm claims it. It was an error before and is an
  error now, but REP-09 must resolve the ambiguity rather than simply add a form.
- **§3.3 still does not compile**, and REP-16 was not the last thing in its way:
  `counts.values()` does not exist either. That is a new finding (the register's
  row), and it was not visible before because the program failed earlier.
