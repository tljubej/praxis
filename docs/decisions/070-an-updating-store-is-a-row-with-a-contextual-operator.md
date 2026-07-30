# ADR-070: An updating store is a catalog row, and its operator is decided contextually

**Date:** 2026-07-30
**Status:** Accepted — implemented
**Milestone:** Repair (stage S25 — REP-21)

## Context

```praxis
distance[key] min= candidate
best[key] max= score
```

§6.2 writes both, and §19's milestone criteria list "`min=` and `max=` map
updates work". Neither parsed. Three of the four pieces already existed and had
been waiting:

- `praxis_map_update_min` and `praxis_map_update_max` in the runtime, complete,
  **with no caller**.
- `INDEX_STORE_MIN` and `INDEX_STORE_MAX` reserved in the catalog (ADR-064),
  with no rows.
- The `PLACE_ASSIGN_STMT` grammar and the whole subscript-store path (REP-16).

What was missing is the operator itself, and it is missing for a reason: **`min`
is an identifier.** `min=` is two tokens, and the prelude's `min`/`max` helpers
(ADR-058) are names §3.3's own representative program calls.

## Decision 1: the operator is contextual, and adjacency is the rule

The parser decides it, at the one position where an identifier cannot otherwise
appear: after a complete expression, in statement position, where an assignment
operator is what may follow. An `Ident` spelling `min` or `max` **immediately**
followed by `=` is the operator; anything else is two tokens that mean what they
say.

A lexer rule was the alternative and it is wrong: it would take `min` away from
every program that calls the helper, or it would need the same contextual
knowledge one layer lower, where the parser's state is not available.

Adjacency is checked against the **raw** token stream rather than through
`nth_kind`, which skips trivia — that difference *is* the rule. `d[k] min = 3`
with a space is therefore not an update, and it reports (as two statements run
together), which is the same answer `+ =` gets for `+=`. `==` cannot be mistaken
for the operator at all: the lexer's max-munch makes it one `EQ2`.

## Decision 2: the two tokens are a node, so no walk can read the `=` alone

The pair is wrapped in an `UPDATE_OP` node. Without it the `=` would be a direct
child of `PLACE_ASSIGN_STMT`, where every existing walk looking for the
assignment operator would find it and read `d[k] min= v` as a plain store — the
difference between "keep the smaller value" and "overwrite it", silently.

`PlaceAssignStmt::op()` was changed to return a **`PlaceAssignOp`** rather than
the operator token, and the token accessor was removed rather than kept beside
it. That is what made the two call sites a compile error instead of a judgement
call, and it is the shape to keep: there is no way to ask for the token and miss
the update.

## Decision 3: `min=` and `max=` are rows, not desugarings

They cannot be a read-modify-write over `[]` and `[]=`, and the reason is
§6.2's own sentence: "an absent entry accepts the first value". A subscript
**read** of an absent key *faults* (§4.7), so `d[k] = min(d[k], c)` faults on
every key the program reaches for the first time — which for a shortest-path
relaxation is every key.

So they are two catalog rows, dispatched exactly as REP-16's store is, and the
whole of ADR-064 decision 1 carries over: bidirectional argument inference, the
`HasMethod` deferral (so `fn relax(d, k, v) { d[k] min= v }` works and is
answered at its call site), TY-32's invariants, and monomorphization.

In HIR they lower with `AssignOp::Assign` — *no read* — and a `set` symbol that
is the update wrapper. MIR needed **no change at all**: it already emits
`set(receiver, indices…, value)` for a non-compound store, which is exactly
`praxis_map_update_min(ctx, map, key, value)`.

## Decision 4: the receiver is `Map[K, V]` with `V` bound to `Int`

The wrappers compare through `int_payload`, so a `Map[Text, Text]` would read its
values as `i64`s. The row states that as a **bound** (`TypePattern::is_scalar`)
rather than as a literal `Int` argument, for TY-31's reason: a bound *pins* an
unresolved value type instead of merely permitting it, so `let d = Map()`
followed by `d[k] min= 1` gives `d` a `Map[?K, Int]` rather than reporting.

`Map` only. `Counter`, `Grid`, `Vec` and the rest have no updating store, because
those are the two wrappers that exist — and a `Counter`'s `min=` would have to
answer what an absent key's zero means, which §6.2 does not.

That receiver-shaped miss gets **its own message** under `Y020`: "values of type
`Counter[?T]` cannot be updated with `min=`". The plain-store wording would be
false about the receiver the mistake is most likely to be written for — a
`Counter` *can* be assigned through one index.

## Consequences

- **No new diagnostic code, no ABI bump.** The wrappers and the fault kinds
  already existed; `Y020` covers the receiver miss and `Y001` the value type.
- **`min`/`max` remain ordinary prelude functions**, including as a subscript
  receiver (`min[0]`) and as a value (`let m = min`), and a gate asserts it — the
  contextual rule exists precisely to keep that true.
- **The five arithmetic compounds are untouched**, and a gate counts their reads:
  a shared path would have been the regression to fear, since `+=` must still
  read before it writes.
