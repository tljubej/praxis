# ADR-152: A brace a block cannot explain is a record, and a shape has one field order

**Date:** 2026-08-10
**Status:** accepted
**Milestone:** 12

## Context

§5.6 gives the language anonymous records — a type that *is* its field set,
identified by that set and nothing else. Until now only the input parser could
produce one: a named-capture template derives `{ x: Int, y: Int }`, and the
[book chapter](../book/src/types/structural-records.md) said in as many words
that the kind was not something a program could write, because a `{ … }` in
expression position is a block.

That left a shape you can be handed but cannot build. `{foo: 1, bar: "asd"}` was
four diagnostics, none of which was about the thing that was wrong.

Underneath it, the two halves of §5.6 had never been reconciled. "Field order in
source does not affect type identity" makes two spellings of one field set the
**same type**, and `unify` implements that: two anonymous defs with matching
field names unify pairwise by name and the later def-id is rewritten to the
earlier one. But a field read compiles to a **slot index** against whichever def
won, while the value was assembled in whatever order its own producer wrote —
the runtime builds a named-capture template's `RecordSchema` from the plan's
capture order. So:

```praxis
fn width_of(r) { r.w }
var a = parse("3x4", `{w:int}x{h:int}`)
var b = parse("4x3", `{h:int}x{w:int}`)
out(width_of(b))                            // 4 — that is `b.h`
```

`b.w` is 3 and the program printed 4, with no diagnostic anywhere, because
`RecordSchema::same_type` compares field names *positionally*: the runtime
treats field order as part of a record's identity, and the type system says it
is not. The chapter recorded this as a known compiler bug and told readers to
write each shape in one order.

## Decision

Two rules, and the second is what makes the first safe.

### 1. A `{` where an expression must begin is a record literal when a block cannot explain it

`{ x: 1, y: 2 }` and `{ x, y }` are anonymous record literals. The decision is
made from two tokens after the `{`:

- `Ident` then `:` — no block's first statement is a name followed by a colon.
- `Ident` then `,` — no block's first statement is a name followed by a comma.

Anything else is the block it already was. The literal is the **same AST node**
as the headed form (`RECORD_LIT_EXPR`) with its `PATH_EXPR` child absent, which
is the shape a headless record *pattern* already has (ADR-091), and it reaches
the same inference and lowering.

Two cases fall out of the rule rather than being carved out of it:

- **`{ x }` stays a block.** It is a well-formed block whose value is `x` *and*
  a well-formed one-field punned literal, and blocks-as-values had the spelling
  first. `{ x: x }` is the record. This is the one thing the rule cannot have
  both ways, and it is stated in the chapter rather than left to be discovered.
- **`{ x:bp }` stays a block**, holding the statement `x` with a §9.8 marker on
  it. `{ x: bp }` with a space is the record whose field is the binding `bp` —
  the adjacency rule that already tells `min=` from `min =`.

The [`StructLit`](../../crates/praxis-parser/src/parse.rs) suppression the four
keyword heads set is **not** consulted. What that flag protects is `p { … }` —
a *name* followed by the brace that could be the keyword's block (ADR-050) — and
a `{` where an operand is still required cannot be that brace, because the block
comes after a complete head. `if { hit: true }.hit { … }` reads both braces
correctly.

### 2. An anonymous shape has one field order: the one it was first written in

`TypeDb` keeps the canonical field order of each anonymous shape, keyed by its
field names sorted. The first spelling anywhere in the program registers its own
order; `register_record` permutes every later one into it. A nominal record is
untouched — its order is its declaration's.

Every producer of a value then lays it out that way:

- A **literal** already did. Lowering pairs each initializer with its index in
  the canonical def and sorts, so this needed nothing.
- A **parser template**, `sections(...)` and `block(...)` each carry the
  canonical order on their plan node (`field_order`), filled at lowering from
  the arena through the [`FieldOrder`](../../crates/praxis-input-parser/src/plan.rs)
  trait, and `alloc_record` assembles into it.

So `width_of(b)` above is 3, and `b` prints `{ w: 3, h: 4 }`.

## Reason

**One node, not two.** Everything after the head — the fields, punning, the
duplicate check, the lowering, the codegen schema — is the same for both
spellings; `name()` is the one question whose answer differs. A separate
`ANON_RECORD_LIT_EXPR` would have been a second copy of all of it for every
exhaustive match downstream to keep in step.

**The tie-break is what a block cannot be, not what a record looks like.** A
rule phrased the other way ("a `{` followed by a name is a record") would have
had to except blocks, and the exception list is unbounded. Phrased as "a block
cannot explain this", the two forms it admits are exactly the two shapes no
statement has, `{ x }` needs no special case to keep working, and the whole rule
is two tokens of lookahead with no parser mode.

**The order has to be decided by something that has seen every spelling.** It
cannot be a property of the producer building the value, because the whole point
of §5.6 is that two producers make one type. Within a compile the only thing
that has seen them all is the type arena, which registered a definition for
each — so that is where it lives, and the plan carries the answer outward rather
than the runtime trying to derive one.

**First-written rather than sorted by name.** Sorting is a pure function of the
name set, so both the compiler and the runtime could compute it with nothing
threaded between them — considerably less code. It was refused because it
changes what every anonymous record *displays* as:
``read lines(`{x1:int},{y1:int} -> {x2:int},{y2:int}`)`` would print
`{ x1: 0, x2: 5, y1: 9, y2: 9 }`. §5.6 promises display follows what was
written, and for the program with one spelling of each shape — which is nearly
all of them — first-written keeps that promise exactly.

The cost, stated rather than hidden: a value's display order now depends on
where the *first* spelling of its shape appears in the file, so adding a
differently-ordered literal above an existing `read` changes how the read's rows
print. Only a program that already writes one shape two ways can see it, and
that program was previously reading the wrong field.

## Consequences

- **`{ x: 1 }` compiles**, reads and assigns fields, matches a headless record
  pattern, compares structurally, and works as a `Map` key or `Set` element —
  the chapter's "an anonymous record is an ordinary record" is now true from
  both directions.
- **The field-order bug is closed**, and the caveat it earned in
  [Records without names](../book/src/types/structural-records.md) is gone. The
  example that documented it (`docs/book/examples/types-a/field-order.px`) now
  prints the right answer and stays as the gate.
- **§5.6's "display and construction preserve source order" is amended**: an
  anonymous shape's order is its *first* spelling's, in layout and display
  alike, because a type has one field order.
- `lower_to_plan` takes a `&mut dyn FieldOrder`. `SourceOrder` is the
  implementation for a plan lowered outside a compile (the teardown and plan
  tests), and it is correct on its own terms — with one spelling, the first one
  *is* canonical.
- `PlanNode::Template` carries an **empty** `field_order` for the shapes that
  are not records. A tuple's element order is its capture order and nothing
  reorders it, so there is no second opinion to record.
- `Attached::Stopped` is boxed. `TypeDb` grew by the order map and
  `StoppedHost` holds one by value, which pushed the enum past
  `clippy::large_enum_variant` — the other large variant was already boxed.
