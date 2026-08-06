# ADR-083: A `Float` prints as a `Float`

**Date:** 2026-07-31
**Status:** Accepted — implemented
**Milestone:** Repair (register REP-44)

## Context

`out(1.0)` printed `1`. So did `out(1.0.to_text())`, `out(1e10)` printed
`10000000000`, and a `Vec[Float]` of `[3.0, 5.0]` printed `[3, 5]` — the same
characters a `Vec[Int]` of `[3, 5]` prints.

The rendering came from one line in `scalars::float_format`:

```rust
// Rust's default `{}` formatting renders finite values in the shortest
// round-trippable form, and `inf`/`-inf`/`NaN` as those literals (§4.12).
let _ = write!(out, "{v}");
```

The comment's premise is true about Rust and false about Praxis. Rust's `{}` is
shortest-**round-trippable for Rust**, where the text `1` parses back as an
`f64` whenever the context asks for one. Praxis has no such context: §4.12's own
typing rule is that `42` is strictly an `Int` literal, that a `Float` operand
makes an operation `Float` and an `Int` operand makes it `Int`, and that there is
no implicit widening. `1` is not a `Float` in this language, so the text `1` does
not read back as the value it came from.

§4.12 asks for "the shortest round-trippable form" and the implementation
answered a different language's version of that sentence.

## Decision: the rendered form is the shortest text that reads back as the same `Float`

A finite value whose shortest digits contain no `.` and no exponent gets `.0`
appended. Everything else is unchanged: `2.5` stays `2.5`, `0.1 + 0.2` stays
`0.30000000000000004`, and `inf` / `-inf` / `NaN` — which §4.12 names as those
literals — take no suffix, because they are not decimal literals and appending
one would produce text that reads back as nothing at all.

One function, `scalars::write_float`, and `praxis_float_to_text` calls it rather
than restating the rule: `out(x)` and `x.to_text()` disagreeing would be a defect
of its own, and the two used to share only a `format!("{f}")` by coincidence.

### Why this and not "print the type alongside"

The alternative was to leave the digits alone and let the *context* say what the
value is — a debugger-style `1 : Float`. It is rejected because the rendered
form has to be unambiguous on its own: it is what `Float.to_text()` answers, and
it is what a reader compares against a written-down expected output. A rendering
that needs external context to say which type it is cannot do either job.
(This argument originally leaned on §16.3 making the rendered form a container's
*sort key* as well. That leg is gone —
[ADR-138](./138-a-container-orders-by-the-value-and-not-by-its-printing.md)
orders a container by the value — and the decision does not need it.)

### Why not exponent notation for large values

Rust's `{}` never emits an exponent, so `1e10` renders `10000000000` and takes
`.0` like any other whole number. Choosing `1e10` instead would be a second
decision — which magnitudes switch notation — and §4.12 asks for the shortest
*round-trippable* form, not the shortest form. Both spellings read back as the
same value; only one of them is already implemented, and adding the other would
change existing output for no correctness gain.

## Consequences

- `run_pass_float_methods` asserted `"9"` for `sqrt(16.0) + 5.to_float()`. It
  **pinned this defect** and is rewritten, not deleted (§8.2): the arithmetic and
  the type were right and the text was wrong.
- The descriptor round-trip test in `scalars.rs` checks `2.5`, which carries a
  `.` already and therefore passes under either rule. The new gate
  `a_whole_numbered_float_renders_as_a_float` is whole numbers, an exponent and
  the three non-finite values — the only places the two rules differ.
- Sort keys moved: a `Map[Float, _]` ordered by the rendered form, so `1.0`
  sorted where `"1.0"` falls rather than where `"1"` did — the same
  lexicographic-not-numeric limitation D3 owned.
  [ADR-138](./138-a-container-orders-by-the-value-and-not-by-its-printing.md)
  removed it: a `Set[Float]` now prints `{1.5, 2.0, 10.25}`, and NaN sorts last
  per decision 2 above rather than wherever the string `"NaN"` happened to fall.
- §4.12's formatting sentence is rewritten to state the rule and its reason.
