# ADR-055: Exhaustiveness and reachability are one usefulness question, asked at every position a value has

**Date:** 2026-07-29
**Status:** Accepted — implemented
**Milestone:** Repair (stage S16 — HIR-06, which closes the stage)

## Context

`praxis-hir`'s `exhaustive.rs` answered two questions with two independent
walks, and each could only see the top level of a pattern:

- `uncovered_constructors` collected the **top-level** variant indices the arms
  named and compared them against the enum's variants. For
  `enum Wrapped { Wrap(Flag) }`, `match w { Wrap(On) => 1 }` covers `Wrap`, and
  nothing asked whether the payload's `Off` was covered. A one-variant enum made
  every match on it exhaustive.
- `pattern_catches_all` reported an arm unreachable only when an **earlier** arm
  was a bare `_` or a bind. A repeated constructor — `match e { A => 1, A => 2,
  B => 3 }` — was silently dead code, and so was a nested pattern an earlier arm
  already subsumed.

Both are the same question — *does this pattern match a value the ones above it
do not?* — and both walks were flattened versions of it. The file's own doc
comment said as much: "intentionally simpler than a full Maranget usefulness
matrix". That is HIR-06.

## Decision 1: one usefulness matrix answers both

`useful(P, q, types)` returns the value shapes `q` matches that no row of the
matrix `P` does; empty means not useful. It is Maranget's algorithm in its
standard three cases — `q`'s head is a constructor; `q`'s head is a wildcard and
the first column's constructors are complete; `q`'s head is a wildcard and some
constructor is missing.

Both diagnostics are then one call each:

- **`Y121`**: arm *i* is unreachable when its own pattern is not useful against
  arms `0..i`.
- **`Y120`**: the match is non-exhaustive when a bare `_` is still useful against
  every arm.

An unreachable arm still goes **into** the matrix. It covers what it names
whether or not it can run, and dropping it would make `{ _ => 1, A => 2 }`
report a missing `B` on account of the arm it had just rejected.

The recursion terminates because the wildcard-complete case only fires when
every constructor appears *literally* in the first column, so it can recurse no
deeper than real constructor patterns nest in the source.

## Decision 2: a variant pattern is padded to its payload arity

`Some`, `Some(_)` and `Some(n)` are one test. Lowering now emits exactly one
sub-pattern per payload slot: a pattern that names fewer is padded with
wildcards, and one that names *more* is truncated after its extras have been
lowered (so anything wrong inside them still reports).

The matrix pairs each pattern column with a type, and a row narrower than its
payload would pair them off by one. Padding is also the safer MIR: an
over-supplied sub-pattern used to make `emit_subpattern_tests` read a payload
index the object does not have.

This makes `TypedPattern::Wildcard` reachable from source in a **payload slot**
for the first time — before, MIR's decision tree only ever saw one from a
synthesized top-level fallback. `a_padded_payload_wildcard_selects_its_arm_at_runtime`
(`jit.rs`) is that path, end to end.

A wrong sub-pattern count is **not** a new diagnostic. It is a shape error the
register does not name, `Y122`/`Y123` already cover the two neighbouring
mistakes, and truncating is strictly safer than what it replaces. It is recorded
in the progress note as an open shape rather than allocated a code here.

## Decision 3: only enums and `Bool` have a closed signature

A type whose values can be enumerated needs no `_` arm; anything else does.
`Closed` is an enum's variants and `Bool`'s two literals. **Everything else is
`Open`** — `Int`, `Float`, `Text`, `Char` because they have too many values, and
`Unit`, records, tuples, functions and an unresolved type variable because the
checker has no pattern syntax to enumerate them with. This preserves what the
old `_ =>` arm did for every one of them: a `_` is required.

`Unit` is deliberately in the open set even though it has exactly one value.
There is no `Unit` pattern syntax to write, so calling its signature complete
would make a `_`-less match on it exhaustive with no arm that could ever run.

A `Float` pattern is keyed by its **bits**, so two spellings of a value are one
constructor and `NaN` is its own — which is right, since a `NaN` pattern matches
nothing and therefore covers nothing.

## Decision 4: `Y120` names the shape that is missing

The witness is a value, rendered as a pattern: ``missing `Wrap(Off)` `` rather
than ``missing variant(s): Wrap``. Naming only the outer constructor is the most
a top-level check could say, and it sends the reader to the arm that is already
there.

At most **three** witnesses are named, and the recursion stops as soon as it has
them: a match over a wide enum has no use for forty. When the only witness is a
bare `_` — an open signature — the message stays ``missing a `_` catch-all
arm``, because there is no value to name and the fix is an arm.

## Consequences

- **`exhaustive::check` takes `&mut TypeDb`.** A payload's type under the
  scrutinee's arguments comes from `variant_payload_of`, which interns.
  `Some(n)` against an `Option[Int]` therefore recurses at `Int` and not at the
  def's own parameter (F12).
- **A `Ctor` carries its `EnumDefId`.** A variant pattern of *some other* enum —
  which only an already-reported `Y122`/`Y123` can produce — matches nothing,
  rather than colliding with this column's variant of the same index.
- **The matrix borrows; it never owns a pattern the lowering did not write.**
  Rows are slices of `&TypedPattern`, and the one wildcard every padded position
  points at is a `static`.
- **Nothing in the corpus changes**, for the eighth time in the repair. No `.px`
  under `tests/` and no CLI fixture contains a `match` at all, so the stage's
  predicted `Y120`/`Y121` churn was zero by construction.
- **Records and tuples are still `Open`.** Praxis has no record or tuple pattern
  syntax; when it gets one, they become `Closed` with a single constructor and
  the matrix already handles that shape.
