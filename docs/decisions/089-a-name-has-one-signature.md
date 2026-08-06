# ADR-089: A name has one signature, so `assert` takes a condition and the arity mismatch gets a code

**Date:** 2026-08-01
**Status:** accepted
**Milestone:** Repair (answers open decision **D16**, S25)
**Amended by:**
[ADR-146](./146-a-collection-constructors-arity-is-its-shape.md) — a collection
constructor's arity is its shape (decision 1 gains one narrow carve-out, below)

## Context

D16 was registered as "does `assert` take a message?" The repair plan attached a
warning to it that is the load-bearing half:

> `assert`'s message is the cheapest possible motivating case, so answering it in
> isolation sets the precedent by accident.

The precedent is the real question: **does Praxis get arity-based overloading,
optional parameters, or default arguments?** A two-argument `assert` beside a
one-argument `assert` requires one of the three. So the general question is
answered first and `assert` follows from it, rather than the other way round.

The apparent counterexample is the parser DSL, which already has optional and
named arguments — `chars(one_of("^v<>"), skip: whitespace)`, `grid(P, ragged,
fill: "x")`, a variadic `block(item, …)`. §7.5 is explicit that this is a
different grammar:

> a labelled argument … belongs to the parser-expression grammar and has no
> meaning in an ordinary call, where it is a parse error at the `:`.

Those fourteen constructors are a closed hand-written table with per-constructor
type synthesis. Not one is a `Func` scheme, and no user program can declare one.

## Decision

### 1. A name has exactly one signature

No arity-based overloading, no optional or default parameters, no named
arguments outside the parser-expression sublanguage.

- **§5.1's model has no room for it.** "An HM-inspired inference engine." A
  symbol carries one scheme, and `unify` on two `Func`s of different length is a
  mismatch. Overload resolution needs the argument types before it can pick a
  scheme; inference needs the scheme before it can check the arguments. Optional
  parameters additionally need a defaulting rule, a lowering for the absent
  argument, and a monomorphization witness for a parameter no call site
  supplies — four mechanisms, and no section asks for any of them.
- **ADR-061 would reopen.** A bare `fn` name in value position is a closure
  value; an overloaded name has no single function value, so `map(f)` would need
  a second dispatch rule keyed on the closure's expected arity.
- **The language already spells "one operation, two shapes" as two names** —
  §6.3's `min`/`min_by`, `max`/`max_by`, `find`/`position`, and §5.4's own hint
  text `sort_by(|value| …)`. That answer is established and costs nothing.
- **§2.2's non-goals are the same instinct**: no user-defined operator
  overloads, no traits, no macros.

**Amended by [ADR-146](./146-a-collection-constructors-arity-is-its-shape.md):
the collection constructors `Vec` and `Grid` select their shape on their
argument count.** `Vec()` beside `Vec(n, fill)`, and `Grid()` beside `Grid(w, h,
fill)`, from a closed two-row table the compiler owns. The rule above is
otherwise unchanged, and it is unchanged *because neither ground it rests on
reaches those two names*: the choice is made on the argument count — a syntactic
fact available before any argument is typed — so the circularity the first
bullet names does not arise; and a collection constructor has no function value
for the second bullet's `map(f)` rule to be ambiguous about, because `var f =
Vec` is `Y022`. This is the same shape of exception as §7.5's parser
constructors, carved out in the Context above for the same reason: a closed
hand-written table with per-constructor type synthesis, which no user program
can declare. Nothing else in the language gains an overload, an optional
parameter or a default argument, and a third row in that table is a request to
reopen ADR-146.

### 2. `assert` keeps one argument, and the name that carries words is `panic`

`(Bool) -> Unit` stands (ADR-056 Decision 1, unchanged).

**Why not a mandatory message** (`forall T. (Bool, T) -> Unit`, which needs no
overloading and which decision 1 therefore does not forbid): it makes
`assert(x > 0)` illegal — the spelling every existing test and every reader
expects — with nothing shorter than an `if` to replace it; it collapses `assert`
into `panic` with an inverted condition, leaving §16.1 two prelude names for one
operation; and ADR-056 is accepted and implemented, so changing it needs more
than "other languages have one".

**Why not a second name** (`assert_with`): two spellings for one operation is
what ADR-077 refused for accessors and ADR-085 refused for `Text`. The second
spelling already exists and is built from constructs the language has:
`if !cond { panic(msg) }`.

**The measurement, which is stronger than any of the arguments.** A failed
`assert` already prints the condition's own source text beside its evaluated
value — `<tmp#4: Bool> @ "total == 42" = false` — and every local in the frame
(`total: Int = 41`), because ADR-056 made it a fault and §9.3's crash machinery
hangs off the fault path. **A hand-written message is strictly less information
than the crash report already contains.** `assert`'s message is the cheapest
motivating case precisely because the thing it would buy has already been
bought.

### 3. The rule ships with a diagnostic, `Y024`

A rule that a name has one signature is only a good rule if the compiler says so
plainly when you break it. Today it does not: `assert(cond, "why")`, `f(1, 2)`
and `g(1, 2)` all report `Y001` — *expected `(Bool) -> Unit`, found
`(Bool, Text) -> ?T`* — a whole-function-type mismatch that reads like an
inference accident, sitting next to a `Y007` that names collection arity and a
`Y110` that names method arity.

`TypeDb::unify` **already knows** (`if ps_a.len() != ps_b.len()`) and discards
the fact. So a new `UnifyError::ArityMismatch` raises it where the knowledge is,
and every `Func`-vs-`Func` unification benefits rather than just `infer_call`:

> `error[Y024]: this function takes 1 argument(s), but 2 were given`

No `assert`-specific text — a rule stated in four places goes stale in three.

## Consequences

- **The language changes not at all; the report does.** Nothing that compiled
  stops compiling, and nothing that failed starts succeeding — a call with the
  wrong arity was already an error and remains one, with a message that names
  what is wrong instead of showing two function types to diff by eye.
- **The parser DSL is untouched and the boundary is reaffirmed.** `skip:`,
  `fill:`, `ragged` and variadic `block` keep working, because §7.5 already says
  they are a sublanguage. Answering D16 the other way would have silently
  reversed ADR-084 and REP-34, both of which drew this same line.
- `assert`'s catalog row, `prelude_assert_requires_bool`, and every test spelling
  `assert(cond)` are unaffected.
- **This closes the precedent, not just the case.** Anyone later wanting an
  optional parameter is asking to reopen decision 1, which is the outcome the
  plan's warning was written to force.
