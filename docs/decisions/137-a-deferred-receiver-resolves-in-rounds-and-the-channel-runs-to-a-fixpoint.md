# ADR-137: A deferred receiver resolves in rounds, and the channel runs to a fixpoint

**Date:** 2026-08-06
**Status:** accepted
**Milestone:** 12

## Context

Handover 31 item 11 reports a program that `praxis check` accepts in silence and
`praxis run` refuses with an `internal compiler error`:

```praxis
fn pick(t, i, j) { t[i][j] }
out(pick([[7, 8]], 0, 0))
```

```console
$ praxis run pick.px --input /dev/null
thread 'main' panicked at crates/praxis-mir/src/build.rs:1437:17:
internal compiler error: the pipeline recognizer declined `[]`, and it carries
no runtime symbol. … every unresolved method call must have been reported by
inference and dropped before here (ADR-093).
```

The assertion names the invariant it is enforcing, and the invariant is the one
being broken: the unresolved call was **not** reported by inference. The program
is well typed — `fn pick(t: Vec[Vec[Int]], i, j)` compiles and prints `7` — so
this is inference failing to resolve something it can resolve.

What matters is whether the receiver is the parameter itself or a value
*derived* from it. Every row below is a single call site at one concrete type,
so none of them asks for polymorphism:

| Body of `fn f(v)` | Before |
|---|---|
| `v.len()` — method on the parameter | works |
| `v[0]` — one subscript, result returned | works |
| `v[0].map(\|x\| x * 2)` — a *pipeline stage* on a derived value | works |
| `v[0][1]` — subscript of a subscript | ICE |
| `v[0].len()` — catalog method on a subscript result | ICE |
| `v.get(0).len()` — catalog method on a method result | ICE |
| `for row in v { row.len() }` — catalog method on the `for` item | ICE |

Binding the intermediate first — `var row = t[i]` then `row[j]` — ICEs
identically, so it is not about expression chaining.

### What the round-counting experiment showed

`Inferer::discharge_constraints` had exactly two callers: the end of `infer_fn`
and the end of `infer_declaration_group`. So a program gets one drain per
function body, and moving the call site into a later function *buys an extra
drain*. That is the whole diagnosis, and it is measurable:

| Program | Links | Drains | Result |
|---|---|---|---|
| `fn f(v){v[0].len()}` / `fn g()->Int{f([[1,2,3]])}` / `out(g())` | 2 | 2 | prints `3` |
| `fn f(v){v[0][0].len()}` / `fn g()->Int{f([[[1,2,3]]])}` / `out(g())` | 3 | 2 | ICE |
| the same, plus `fn h()->Int{g()}` / `out(h())` | 3 | 3 | prints `3` |

Every ICE row in the table above is fixed by exactly one extra drain. So the
resolution machinery was already correct and already produced the right types;
only the iteration count was wrong.

### Why one drain is one round

`HasMethod`, `Iterable` and `HasField` are the three capabilities discharged by
*producing* a type ([ADR-057](./057-a-capability-requirement-rides-on-the-scheme-that-quantified-it.md)
decision 5, [ADR-062](./062-an-iterated-parameter-is-generic-in-the-iterable-and-not-its-element.md)
decision 4, REP-28). `resolve_deferred_method` unifies the catalog row's result
with the variable the call site is holding; `resolve_deferred_iterable` unifies
the item; `resolve_deferred_field` unifies the field's type. **That unification
is what makes the next link's constraint dischargeable** — and the next link is
not in the batch `take_dischargeable` already handed back, because when the
batch was taken its receiver was still a variable. `for c in
self.db.take_dischargeable()` therefore resolved `t[i]` and dropped `t[i][j]` on
the floor, and whatever was left at the declaration group's final sweep was
silently discarded.

`v[0].map(…)` survives for a reason that is not resolution at all: its
constraint is never discharged either, but MIR's `recognize_pipeline` claims the
node before the assertion is reached, so a pipeline stage needs no `method_refs`
entry. The same accident is why an uncalled `fn total(values) { values.sum() }`
looks clean — `sum` is a pipeline sink.

## Decision 1: the constraint channel discharges to a fixpoint

`Inferer::discharge_constraints` takes batches until `take_dischargeable`
returns empty. One batch is one round, and a derived receiver is exactly one
round deeper than the parameter it came from.

**The termination argument is written into the doc comment**, because a compiler
that hangs is worse than one that ICEs, and because an arbitrary iteration cap
would hide the question rather than answer it. Every round strictly removes
constraints from the pending set — `take_dischargeable` removes what it returns
— so the loop can only run forever if a round can *mint* a constraint that a
later round discharges into another one. It cannot. The only channel writes any
discharge path makes are `Capability::Kind` requirements, from `apply_bounds`'s
`Bound::Kind` arm and from `require_collection_invariants`, both through
`require_cap`; a pending `Kind` constraint is answered by `capability::check`,
which emits nothing at all. No round can produce a *resolving* constraint, so
the fixpoint is reached in at most one round per link of the longest chain.

The invariant is anchored to `apply_bounds`'s exhaustive `match`, which is where
a future `Bound` arm routing to `require_method` would have to be written.

## Decision 2: this changes *when* a constraint is examined, not *which* variables are pinned

ADR-057 decision 5 and ADR-062 decisions 2 and 3 are untouched, and deliberately
so. Nothing in this change goes near `pin_to_level`, `claim_constraints`,
`generalize_at`, `reemit_constraints` or `infer_call`.

- `fn total(values) { values.sum() }` called at `Vec[Int]` and then at
  `Vec[Float]` is still a `Y001` at the second call site. The receiver is still
  pinned to the declaration group's level, `total` is still the monotype
  `(Vec[Int]) -> Int`, and the report is produced by unifying that monotype in
  `infer_call` — not by the constraint channel, so no number of rounds can reach
  it.
- A `for`'s iterator is still quantified while its item is still pinned.
- A parameter no method was called on still generalizes: `fn id(x) { x }` is
  still `forall T. (T) -> T`.

And the derived receiver was pinned **all along**: `require_method` pins the
result variable alongside the receiver and every parameter, precisely so a call
site cannot instantiate a fresh result while discharge unifies the original.
What was missing was the second round, not a second pin. The observable proof is
that `pick` called at `Vec[Vec[Int]]` and at `Vec[Vec[Text]]` is a `Y001` for
exactly the reason `total` is.

## Decision 3: a `MethodCall` whose receiver is still a variable at MIR is an unreachable body, and lowers to a `panic`

A second, distinct defect reaches the same assertion, with no chaining involved:

```praxis
fn f(v) { v.len() }
out(1)
```

`f` is never called, so nothing ever pins `v`, so the `HasMethod` constraint
survives the fixpoint and is dropped — and the body still reaches MIR, which
finds a method call with no catalog row and ICEs. The same for `v[0]`,
`v.push(1)`, `v.get(0)`, `v.contains(1)`; clean for `v.sum()` and `v.map(…)`,
which are pipeline rows.

**[ADR-093](./093-a-method-that-cannot-resolve-is-reported-at-check.md) §3's
first bullet is wrong about why this cannot happen.** It says `monomorphize`
drops uncalled polymorphic originals so such a body never reaches MIR. It does
not: ADR-057 decision 5's `pin_to_level(receiver, decl_site)` makes the function
a **monotype**, `Scheme::is_polymorphic()` is `!binders.is_empty()` and is
therefore false, and mono's drop filter keeps it. Dropping it instead is not an
option either — `fn f(v){v.len()}` with `var g = f` and no call still needs the
symbol.

So MIR guards it. A `MethodCall` whose receiver's static type is still an
unbound type variable after `recognize_pipeline` has declined lowers to an
unconditional `panic` carrying the method name, and the assertion is kept
verbatim for every other decline: a receiver that *is* concrete with no symbol
and no recognizer arm is still a compiler bug and must still read as one
(REP-40).

The body is unreachable by construction. Any call unifies the argument and pins
the receiver — including a call through a value, which is why
`fn f(v){v.len()}` with `var g = f` and `out(g([1,2]))` prints `2` — so the only
way to reach the guard is a function nothing ever calls. Answering the `Unit`
singleton instead is refused by REP-40, and reporting a diagnostic is refused by
ADR-093's own consequences, which promise an uncalled deferred body is clean at
both commands.

## Consequences

- `fn pick(t, i, j) { t[i][j] }` compiles and prints `7`, and so does every
  other row of the table. Stripping the annotations off a real solve — day 12's
  `search`, whose only load-bearing annotation was the parameter its body
  double-subscripts — no longer changes whether the program runs.
- **ADR-062's own claim becomes observable with a method.** It says an iterated
  parameter's item is pinned "for the same reason a method receiver is", and the
  book example chosen for it does arithmetic on the item. `for row in v {
  row.len() }` is the combination the example does not cover, and it was an ICE
  until now.
- **A `Y110` on a derived receiver is reported at `check` for the first time.**
  `fn f(v) { v[0].len() }` with `out(f([1, 2, 3]))` resolves the element to
  `Int`, which has no `len`; before the fixpoint the constraint was never
  re-examined, so `check` exited 0 and `run` ICEd. This is
  [ADR-133](./133-every-diagnostic-a-well-formed-program-can-earn-is-analysiss.md)'s
  rule reaching one more code, and it means the fix does not only accept more
  programs — it also reports programs that used to be silent. A sweep of
  `tests/aoc-corpus`, `tests/input-parsers` and `docs/book/examples` found no
  existing program whose `check` output moves.
- **No new diagnostic code is allocated.** Every report this reaches is a `Y110`
  or a `Y001` that ADR-093 and ADR-057 already own.
- ADR-093's §3 first bullet and the two lowering comments that repeat it are
  corrected rather than deleted: the reasoning they record is the reasoning the
  next reader would otherwise re-derive.
