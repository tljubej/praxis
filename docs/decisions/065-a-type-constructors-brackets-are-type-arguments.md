# ADR-065: A type constructor's brackets are type arguments, and every other name's are a subscript

**Date:** 2026-07-30
**Status:** Accepted — implemented
**Milestone:** Repair (stage S25 — REP-09)

## Context

```praxis
let counts = Counter[(Int, Int)]()
```

`P002` at the `[`. The element type is inferred from use instead, so `Counter()`
worked and **the design doc's own spelling did not** — and §3.3's representative
program, which is S25's acceptance criterion, writes the explicit form.

REP-16 landed a subscript grammar one commit earlier, which turned the parse error
into a `Y020`: `Counter[(Int, Int)]` read as a subscript of the `Counter`
constructor function. So the two forms now genuinely collide, and REP-09 is not
"add a form" but "resolve an ambiguity".

## Decision: the name in front decides, from a closed list the parser holds

`Ident [ … ]` is a **type-argument list** when the identifier is one of the
compiler-owned type constructors, and a **subscript** otherwise.

Nothing else can decide it:

- **Not the brackets**, which are identical.
- **Not the contents.** `Int` is a legal expression, `(Int, Int)` a legal tuple of
  two names, `Vec[Int]` a legal subscript. Every type that can appear as an
  argument also parses as an expression.
- **Not "followed by `(`".** `m[k](7)` — calling a closure stored in a collection —
  is a legal M8 form (§4.10), and that rule would silently reparse it. Silently
  reparsing a legal program is the defect class this repair exists to remove.

So `praxis-parser` holds `TYPE_CONSTRUCTOR_NAMES`: the ten §6.1 collections plus
`Option`. `parse`'s own special case (§7.1, `parse(text, parser_expr)` is syntax
rather than a call) is the precedent for a name-driven decision in this parser.

**The cost, stated rather than hidden:** a binding that shadows a type
constructor's name cannot be subscripted. `Counter[0]` reads as a type-argument
list and then reports that a `(` is missing. That is the whole price, and it buys
`m[k](7)`.

**There are therefore two copies of the list**, because `praxis-parser` does not
depend on `praxis-stdlib` and cannot ask. `the_parsers_type_constructors_are_the_compilers`
is what keeps them from drifting — a name in only one is either a constructor whose
type arguments do not parse, or a binding name that can never be subscripted. It is
the same shape as `every_graph_helper_is_a_prelude_name` (ADR-060).

## Decision: the arguments belong to the call, and they *unify*

`TYPE_ARG_LIST` is a sibling of the `ARG_LIST` under `CALL_EXPR`, not part of the
callee path: `Counter` alone is still just a name, and the arguments say what the
one call it heads constructs.

Inference applies them to the callee type **instantiated at this call site**, so
the constraint lands on this call's own variables rather than on the constructor's
scheme. And it **unifies** rather than substitutes, which is what makes a
disagreement a report at the use that disagrees:

```praxis
let c = Counter[Text]()
c.inc(1)                  // Y001: expected Text, found Int
```

Substituting would have made the annotation win silently, and inferring-then-
comparing would have reported at the constructor, which is not where the mistake
is.

Three consequences fall out of using the existing machinery rather than new
checks:

- **The wrong number of arguments is `Y007`**, the code a written `Vec[Int, Text]`
  annotation already gets. It is the same mistake in a second position, so it is
  the same code — no new code is spent by this row.
- **A type argument is an annotation**, so `Vec[Nope]()` is `N002` and `Vec[n]()`
  (a value in type position) is `N003`, reported by *resolution* at the annotation
  before inference has an opinion. `resolve_call` checks them exactly as `let`'s
  `: Type` is checked.
- **The result's arguments are read generically** — a collection's or an enum's —
  so the mechanism is not `Counter`-shaped. It reaches any nullary generic
  constructor the prelude grows.

A type-argument list belongs to a *call*, so the `(` after it is required:
`Counter[Int]` alone names a type in value position, which no expression grammar
accepts. An empty `Counter[]()` reports too — a constructor with no type arguments
is spelled `Counter()`, and that spelling already works.

## Consequences

- **No new diagnostic code.** `Y007`, `N002` and `N003` cover every mistake this
  form can make, and ADR-051 needs no amendment.
- **`peek_text` returns `Option<&'t str>`** rather than a `&str` borrowed from the
  parser. The elided lifetime made a second decision from the same text a borrow
  error, which is a trap rather than a design: the text outlives the parser.
- **§3.3's representative program type-checks now** — `praxis check` exits 0 — and
  still does not *run*: `counts.values()` does not exist in the catalog. That is
  the register's newest row, and it was invisible until this one landed.
- **The corpus program from REP-16 uses the explicit spelling**, so the form is
  executed by `every_corpus_program_runs_and_prints_the_answer_it_documents` and
  not only by unit tests.
