# ADR-077: A zero-argument accessor is a call, and a bare `.name` is a field

**Date:** 2026-07-31
**Status:** Accepted — the tree already behaved this way; this fixes the design
doc and pins the rule
**Milestone:** Repair (REP-31)

## Context

The design doc writes the zero-argument accessors two ways. §4.2's first example
of a `let` is `let width = grid.width`; §6.4's required Grid API opens with
`grid.width` and `grid.height` and then writes `grid.get(x, y)`,
`grid.positions()`, `grid.cells()` with parentheses; §5.7's example catalog rows
are `Vec[T].push(T) -> Unit` and `Vec[T].len -> Int` — one with, one without.

The tree supports exactly one of the two. `len`, `width` and `height` are
`MethodEntry` rows of arity zero, and every test and every corpus program calls
them: `g.width() * 10 + g.height()`. A bare `v.len` reaches `lower_field_get`,
finds no record, and is `Y112` "no field `len` on this type".

So the doc's property spellings name a syntax that has never existed, and REP-31
is the question of which side moves.

## Decision

**They are always calls, and the doc is corrected.** `v.len()`, `grid.width()`,
`grid.height()`. There is no property form and none is added.

Three reasons, in the order they bind.

**1. A bare `.name` already means something else, and it lowers differently.**
`p.x` is a field read: `TypedExpr::FieldGet` carrying a slot **index** taken from
the record's definition, which MIR turns into a `LoadField`. A method call is a
catalog row and a runtime call. Letting one syntax mean either would need a rule
for which wins, and the only available rule is "whichever the receiver's type
happens to have" — so adding a field to a `struct` whose name matches a catalog
row would silently change what an existing expression does.

**2. A receiver whose type is not yet known cannot tell them apart.** This is the
reason that makes the rule load-bearing rather than merely tidy. REP-28 put a
field read on the constraint channel: `fn dist(a) { a.x + a.y }` emits
`Capability::HasField { name, ty }` against `a` and discharges it by asking the
record the call site pinned what the field holds. `fn f(v) { v.len }` under a
property form would emit a requirement with **two** possible discharges — a field
of that name, or a nullary row of that name — and nothing at the read site could
choose. The channel's three capabilities each resolve to one thing; a fourth that
resolves to one of two is not a capability, it is a coin flip.

**3. §5.7 says the catalog is the dispatch table.** A row has a receiver pattern,
parameters and a result. A property read would be a second dispatch surface over
the same names, keyed differently, and the language server reads the same table
for completion and signature help — it would have to render one row two ways.

## Consequences

- **Four lines of the design doc change**: §4.2's `let width = grid.width`,
  §5.7's `Vec[T].len -> Int`, and §6.4's `grid.width` / `grid.height`. Every other
  accessor in those lists already had its parentheses. No implementation changes.
- **§5.7 states the rule** rather than leaving it to be inferred from the examples,
  and says why a bare `.name` cannot be a call.
- **`Y112` is what a property spelling gets**, from lowering's one emitter. That is
  the same division REP-28 kept for a resolved receiver with no such field, and it
  carries the same known asymmetry: `praxis check` does not run lowering, so
  `v.len` is clean under `check` and reported under `run`. Moving `Y112` into
  inference is the general fix for that asymmetry and belongs with `Y110`'s, not
  here — the finding is the doc's, and a doc that spells a construct the language
  does not have is what REP-31 was.
- **A record field may be named `len`** and is unaffected: `p.len` reads the field,
  `p.len()` looks for a row. The gate pins both.
