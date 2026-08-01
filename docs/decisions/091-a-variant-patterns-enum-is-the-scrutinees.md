# ADR-091: A variant pattern's enum is the scrutinee's, and a record pattern needs no head

**Date:** 2026-08-01
**Status:** Accepted — implemented
**Milestone:** Repair (stage S26 — REP-56, REP-57, REP-66)

## Context

```praxis
let ms = read scan(choice(Mul: `mul({a:int},{b:int})`, Do: `do()`))
for m in ms { match m { Mul(p) => out(p.a), Do(_) => {} } }
```

`praxis check` said nothing. `praxis run` printed `Unit`. Written the way C.9
actually wants it — `p.a * p.b` — the same program aborted the process:

```
panicked at crates/praxis-runtime/src/abi.rs: int_payload wants a `Int` payload;
this value is a `Unit` (REP-56)
```

The register said this row "needs a real payload-record type — a feature, not a
repair". **It does not, and this ADR exists partly to say so.** The payload record
type is real and has been all along: `synthesize.rs` builds a genuine anonymous
`TypeData::Record` from the capture list, `FieldSet` preserves capture order,
`plan.rs` assigns the runtime slots by that same order so the type's field index
and the runtime slot agree by construction, and `unify.rs` has unified anonymous
records and anonymous enums structurally since ADR-024/ADR-025. `out(p)` on the
whole payload already printed `{ a: 2, b: 3 }`. Nothing below HIR was wrong.

What was wrong is smaller and worse. **Inference and lowering disagreed about
where a variant pattern's enum comes from.** Lowering reads it off the
*scrutinee*. Inference read it off the constructor's resolved *symbol* — and an
anonymous enum from `choice(...)` has no declaration, so name resolution records
no reference for the constructor and there is no symbol. `infer_pattern`'s whole
`Variant` arm was guarded by that lookup, so for every anonymous enum it did
nothing at all: the scrutinee was never unified, the payload was never asked for,
and the payload binding never got a type. `p` kept an unbound variable, `p.a` took
`infer_field_get`'s REP-28 tolerance, lowering answered `Unit` instead of emitting
the field load, and the runtime aborted one instruction later. The arm's own
comment asserted the constructor was "resolution already resolved" — false for
every anonymous enum in the language.

Two more defects were found in the same few lines and are fixed here because they
are the same mistake seen from other sides.

- `Mul({a, b})` — the payload record spelled out in the pattern — did not parse
  (**REP-57**). A pattern beginning with `{` was "expected a pattern", and
  `is_pattern_start` omitted `L_BRACE`, so the arm list stopped there and the rest
  of the function left the tree: 21 diagnostics from one token.
- `P {}` was read as a **binding named `P`** (**REP-66**), because
  `Pattern::kind()` decided the record shape from the presence of a
  `PATTERN_FIELD` child and `P {}` has none. `match q { P {} => 1 }` where `q` is
  a `Q` ran the arm and returned 1 — HIR-07's defect at the one pattern shape
  HIR-07 did not reach.

## Decision 1: inference reads a variant pattern's enum off the scrutinee

`infer_variant_pattern` follows `expected`. If it is a `TypeData::Enum`, that def
and those arguments are the pattern's enum, and the payload comes from
`variant_payload_of` — which is, word for word, what `lower_pattern` has always
done. One rule, stated in both halves, instead of two halves that agreed only by
accident on the nominal case.

**The constructor symbol stays as the fallback**, consulted only when the
scrutinee is *not* already an enum. That is the case that runs the other way: when
nothing has pinned the scrutinee, the constructor is the only thing that can pin
it, and it is why

```praxis
fn score(m) { match m { Step(n) => n, Stay => 0 } }
```

infers `(Move) -> Int` for a nominal enum. Scrutinee-first rather than
constructor-first is a real choice, and it changes behaviour in one case: a
variant name that resolves to *some* enum's constructor while the scrutinee is a
different concrete enum carrying a variant of that name. Scrutinee-first is
correct there — it is what lowering does, so the two now agree instead of the
pattern unifying the scrutinee with the wrong enum — and it is what makes the
`Y122` in Decision 5 possible at all.

## Decision 2: a record pattern's head is optional, and a headless one pins from the scrutinee

```text
pattern := … | Ident "{" [pattern_field ("," pattern_field)*] "}"
             | "{" pattern_field ("," pattern_field)* "}"
```

The head becomes optional and `PatternKind::Record` carries an `Option<String>`.
This is ADR-069 Decision 1's production with one thing removed, so it costs one
parser arm and no new token — and it is legal in *every* pattern position, because
it is one production:

```praxis
match m { Mul({a, b}) => a * b, Do(_) => 0 }
for {x, y} in points { … }
|{x, y}: Point| x + y
```

§7.5 wrote this shape as `.Number { … }` — a leading-dot variant form with the
payload record's fields flattened into the variant's own braces. That surface does
not exist and is not built. It would need a new token *and* a flattening rule, and
the dot exists only to dodge a collision with ADR-069 Decision 4's nominal record
pattern that nesting does not have. §7.5's sentence predates ADR-069 and describes
a language that was never written; it is amended to the spelling that works.

**A headless pattern needs a record it can see.** Unlike a tuple pattern, which
pins an open scrutinee from its own arity (ADR-069 Decision 4), a record pattern
cannot: field names alone do not determine a record type, because the language has
no row variables. So a headless pattern against a scrutinee nothing has pinned is
`Y123`, with a message that names the two ways out — head the pattern, or annotate
the value.

Silence there was the first thing tried, by analogy with `infer_field_get`'s
REP-28 tolerance for an unpinned receiver. It was measured and **it is not the same
trade**: `let f = |{x, y}| x + y` passed `praxis check` and aborted under `praxis
run` with REP-56's own panic, because inference had bound `x` and `y` to fresh
variables while lowering — which reads the record off the scrutinee and by then
knows it — stored the fields at `Int`. A field *read* can be silent because
lowering answers `Unit` too, consistently. A *binding* cannot: the binding's type
and the body's disagree, and a disagreement between inference and lowering is
exactly the defect this row exists to close.

## Decision 3: a pattern is a record pattern because of its brace

`Pattern::kind()` decides the record shape from a direct `L_BRACE` token child,
before it looks at the name. It used to decide from a `PATTERN_FIELD` child, after
finding a direct `Ident`, and that is silently wrong at both ends — with the same
consequence each time, a pattern that matches everything:

- `{a, b}` has no direct `Ident`, so it would fall through to the final
  `PatternKind::Wildcard` and become an irrefutable arm. This is what a
  grammar-only fix for REP-57 would have shipped.
- `P {}` has no `PATTERN_FIELD`, so it read as `PatternKind::Name("P")` — a
  binding, which matches anything (REP-66).

The brace is what the *parser* used to open the fields, so it is what the reader
of the tree should ask about. `P {}` stays legal and is a record pattern that
binds nothing: it names the record it tests for, so it is refutable, and it is
`Some` beside `Some(_)`.

## Decision 4: a headless `{}` is a parse error, and an anonymous enum renders its variants

`{}` is rejected where `()` is (ADR-069 Decision 1), and for the same reason: it
binds nothing and names no record, so it tests nothing and covers everything. A
pattern that matches anything is spelled `_`, and accepting a second spelling lets
a half-written pattern become an irrefutable arm by accident.

An anonymous enum renders as `{ Mul({ a: Int, b: Int }) | Do(Unit) | Dont(Unit) }`
— braces to match the anonymous record's own structural form (§5.6), `|` and
payload parentheses to keep the two unambiguous, and *every* payload written so
the rendering is total. It used to render as **nothing**, so this row's own
neighbouring diagnostics read "expected `Int`, found " and "`` has no variant
`Bogus`". A type a message cannot name is a type the reader cannot fix.

## Decision 5: an unknown variant is reported at inference

When the scrutinee is a concrete enum and the pattern names a variant it does not
have, `Y122` is reported in inference. It was lowering's alone, and lowering runs
only on a program analysis accepted — so `praxis check` was clean on a misspelled
variant while `praxis run` exited 1 on the same file. That is REP-12's asymmetry,
for *every* enum and not only the anonymous ones this row is about. Inference does
not fall back to the constructor symbol in that case: a name that happens to be
some other enum's variant would unify the scrutinee with that enum and bury the
report under a mismatch about a type the program never mentions.

## Consequences

- **No new type-system machinery, and no new diagnostic code.** `Y112`, `Y114`,
  `Y115`, `Y122` and `Y123` already name every mistake here. ADR-024/ADR-025's
  anonymous structural record was right end to end; what this row changed is which
  question inference asks.
- **`for {x, y} in points` and `|{x, y}: Point| x + y` arrive with it**, which is
  the header half ADR-069's Consequences left "to a row of its own". REP-25 had
  already routed both through `parse_pattern`, so they cost nothing extra.
- **A helper function taking a `choice` value is still unspellable**, and this is
  the honest boundary of what the row delivers. A `match` inside a function whose
  parameter nothing pinned lowers every variant pattern to a wildcard, so arms
  2..n are `Y121` and arm 1 would run for every value. For a nominal enum the
  constructor symbol pins the parameter and this never fires; for an anonymous
  enum nothing can, because the type has no name to annotate with. Decision 2's
  `Y123` closes the record half of this — a headless pattern in that position now
  *reports* instead of running wrong — but the variant half is registered as its
  own row rather than fixed here.
- **`P {}` changes meaning.** It was a catch-all binding and is now a record
  pattern. No program or fixture in the tree relied on it, which is why it can be
  fixed rather than deprecated — but it is a behaviour change beyond the row's
  own reproduction, and it is stated here because a silent one would be worse.
