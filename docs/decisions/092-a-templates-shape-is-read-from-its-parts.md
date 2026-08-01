# ADR-092: A template's shape is read from its parts, in one place, and there is no tuple node

**Date:** 2026-08-01
**Status:** Accepted — implemented
**Milestone:** Repair (REP-54)

## Context

```praxis
fn main() -> Vec[(Int, Int)] { read lines(`{int},{int}`) }
```

over `"1,2\n3,4\n"` printed **`[Unit, Unit]`** and exited 0. `praxis check` was
silent, and correctly so: the annotation typechecks, §7.8's own type-derivation
table says `` `{int},{int}` `` is `(Int, Int)`, and §7.3's example says
``lines(`{int},{int}`)`` is `Vec[(Int, Int)]`. Three facts, two of them already
right:

| | |
|---|---|
| the static type | `Vec[(Int, Int)]` — right (`synthesize::template_type`) |
| the value | a real `Tuple` with a real `TupleSchema` — right (`walk_template`) |
| the `Vec`'s element tag | `Unit` — **wrong** (`template_result_descriptor`) |

The value was never wrong. `rows.get(0)` printed `(1, 2)` the whole time,
because reaching an element reads the descriptor out of the object's own header.
It is the tag the *collection* carries for its elements that lied, and that tag
is what `vec_format`, `vec_equals` and `vec_hash` dispatch through.

**So this was not only a rendering defect — it was a wrong `Bool`.** A parsed
`Vec[(Int, Int)]` compared *unequal* to an identical `Vec` built with `push`:
`vec_equals` compares the two element descriptors first and bails before it
looks at a single element, so `same_element(UNIT, TUPLE)` ended the comparison.
The `false`-expecting sibling program also answered `false`, correctly — both
spellings agreed with each other and with nothing, which is the silent kind.

This is ADR-078 Decision 5's own class — "a collection's element descriptor is
derived, never defaulted" — surviving in the one shape that decision did not
reach, and it survived thirty-three corpus programs because none of them used a
multi-anonymous-capture template.

**Why it stayed hidden is the more useful half.** `template_result_descriptor`'s
comment said the multi-capture shape "lowers to a `Tuple` node, handled above".
It does not, and there was no "above": `lower_template` pushes
`PlanNode::Template` for *every* template shape, so `walk_tuple` was unreachable
from source and the descriptor path had nothing to defer to. §7.3's three-way
rule was restated in four places — the static type, the lowering's comment, the
interpreter's assembly of the value, and the interpreter's element tag — and
three of the four had gone stale.

## Decision 1: the tuple tag is the uniform `TUPLE` descriptor, not a constructed one

`template_result_descriptor` answers `&crate::tuples::TUPLE` for two or more
anonymous captures.

The register estimated this row as wide, and named the reason: the fix "is a
tuple descriptor built from the child descriptors, which is `alloc_tuple`'s
runtime `TupleSchema` interner reached from a static-descriptor path that has no
tuple constructor today". **There is no such thing to build, and recording that
the estimate was wrong is the point of writing it down.** `TUPLE` is a *uniform*
descriptor, exactly like `VEC`, `GRID`, `RECORD` and `ENUM`: one descriptor
describes every tuple of every arity and element mix, and the per-shape
`TupleSchema` lives in the **payload** (`tuples.rs`), where `tuple_format`,
`tuple_equals` and `tuple_hash` read it from. `VecPayload::element_descriptor` is
a `*const TypeDescriptor` — it could not hold a schema if one were built. The
schema this path was thought to need was already interned, at the value site, by
`alloc_tuple`.

`praxis-repr`'s forward map (ADR-042 Decision 1's "one exhaustive `Type ->
BuiltinTypeId` match") has always answered `Tuple` for every `TypeData::Tuple`,
and that is the map codegen goes through for a source-level `Vec[(Int, Int)]`.
The parser answering `Unit` was the only place the two producers disagreed —
which is why the gate for this decision compares a parser-built `Vec` against a
`push`-built one rather than against a printed string.

Two behaviour changes ride along, both intended:

- such a `Vec` now compares **equal** to an equally-shaped one built with
  `push`, where it compared unequal before. Any program relying on the old
  `false` was relying on a wrong answer.
- hashing changes for the same values, since `vec_hash` mixes the element
  descriptor id and `unit_hash` hashed `()` for every element. `Eq` and `Hash`
  stay in agreement, because both now route through `TUPLE`.

## Decision 2: `PlanNode::Tuple` is deleted, because it cannot represent a tuple

The variant, its `walk` dispatch arm, `walk_tuple`, and `child_descriptor`'s arm
for it are gone. It had been documented as "retained defensively (the `Tuple`
node is reserved)" since the M7 handover, nine milestones ago, and nothing ever
constructed one — not the lowering, not a test, not a fixture.

"Nothing emits it" is the weak form of the argument, and it invites the answer
"so emit it". **The strong form is that the variant cannot represent the
construct it is named for.** `Tuple { elements: &'static [u32] }` holds child
node indices and nothing else, and a multi-capture template's separators are
`TemplatePartNode::Literal`s *between* its captures: lowered to that variant,
`` `{int},{int}` `` loses its comma. `walk_tuple` said as much in its own
comment — "There is no literal between two elements here to be bounded by". The
only shape it could express is back-to-back captures with no separator, and even
that lowers to `Template` today and is handled correctly there. Widening
`elements` to carry the literals makes it `PlanNode::Template` again.

So it was a state the plan type permitted, the lowering could not produce, and
the language could not mean — and it cost this defect once already, by giving a
stale comment somewhere plausible to point. A reserved variant is a promise the
type system makes on the compiler's behalf that the compiler never keeps.
Deleting it turns "unreachable, trust the comment" into "unnameable, checked by
rustc".

What makes the deletion safe rather than hopeful is the standing lowering test
in `plan.rs` asserting that a two-anonymous-capture template lowers to
`PlanNode::Template`; its doc now says that this is what it is for. If a
`tuple(P, Q)` parser combinator is ever wanted, it is a new construct with its
own §7.5 row, not a variant restored from cold storage — §7.5 lists no such
constructor and §7.3 writes tuples only as a template result.

## Decision 3: §7.3's shape rule is stated where it is enforced

`TemplateShape` and `TemplateShape::of(parts)` live in `plan.rs`, beside the
`TemplatePartNode`s they classify. `template_result_descriptor` is a total match
on it, `walk_template`'s assembly matches on
`(TemplateShape::of(parts), captures.as_slice())`, and `lower_template`'s two
byte-identical trailing branches — distinguished by their comments and by
nothing else — are one branch.

This is the fix for the *recurrence* rather than the defect. The tag and the
value can no longer disagree about what shape a template is, because they ask
the same function; a rule stated in four places goes stale in three, and this
one had.

`synthesize::template_type` is deliberately **not** folded in. It answers the
same question one step earlier, over AST `TemplatePart`s rather than lowered
ones, and it has always been right — forcing one classifier across both would
mean threading a generic through a crate boundary to fix nothing. Its doc points
at `TemplateShape` instead of restating the rule.

One implementation note, because it is the kind of thing that gets "simplified"
back: `walk_template`'s scalar arm binds its single value by **slice pattern**,
and the combination the classifier cannot produce falls to the `Unit` arm. No
`expect`, no `unwrap`. This code runs beneath an `extern "C"` entry point, where
a panic is undefined behaviour (ADR-080), and `parser.rs` already carries one
scar from bridging an invariant that way.

## Consequences

- `` `{int},{int}` `` and friends render, compare and hash as the tuples they
  always were, at any arity, any element mix, and any collection nesting.
- No new language surface. `(Int, Int)` is already a type, and the program
  already parsed, checked and evaluated; only the runtime tag changed.
- ADR-078 Decision 5's "derived, never defaulted" now has no exception. Its
  closing paragraph still names REP-54 as the one place the rule is unkept and
  predicts a tuple-descriptor constructor; that paragraph needs amending.
- Named-capture templates are untouched: `alloc_record` takes each field's
  descriptor from the captured *value's* header, never from `child_descriptor`.

## Gates

- `parser::tests::multi_anonymous_template_captures_are_a_tuple` — the sibling
  of the one-capture test beside it. Hand-builds an `Int`/`Word` template (mixed
  on purpose: a fix that reached for the first child's descriptor stays red) and
  asserts the element tag is `TUPLE`. Observed red with Decision 1 reverted:
  `left: TypeId(0)` against `right: TypeId(16)`.
- `tests/input-parsers/template_multi_capture_tuple` — §7.3's own example, end
  to end, with `pairs == same` as the load-bearing line. Observed red:
  `[Unit, Unit]` and `false`.
- `tests/input-parsers/template_multi_capture_mixed_arity` — arity 3, mixed
  element types, `lines` inside `sections`. Observed red: `[[Unit, Unit],
  [Unit]]`. Its second line prints `(3, 4, beta)` on both binaries and must
  stay that way; that is the fixture's own evidence that only the tag lied.
- Decision 2 needs no test and should not be given one. Deleting the variant
  makes naming it a compile error, which is the whole point of deleting rather
  than documenting.
