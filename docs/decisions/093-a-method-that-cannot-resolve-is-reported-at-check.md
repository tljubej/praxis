# ADR-093: A method that cannot resolve is reported at `check` — one emitter for `Y110`, and it is inference's

**Date:** 2026-08-01
**Status:** accepted — implemented
**Milestone:** Repair (register REP-33, half (b))
**Amends:** ADR-057 Decision 5, at the `HasMethod` door. ADR-057 was already
amended once, by REP-28, at the `HasField` door; this is the same amendment at
its sibling.

## Context

Appendix D — the design document's own "first end-to-end demo target" — passed
`praxis check` with exit 0 and no output, and then died at `praxis run` with
eight `error[Y110]`s. That is the shape of the defect, and Appendix D is only
where it was noticed. Three smaller programs reproduce it with nothing else
going on:

```praxis
let v = Vec[Int]()      # (1) concrete receiver, no such row
v.push(1)
out(v.nope())
```

```praxis
fn f(x) { x.nope() }    # (2) receiver a parameter, pinned by the call
out(f(3))
```

```praxis
fn f(x) { x.nope() }    # (3) receiver a parameter, nothing ever pins it
out(1)
```

Each was `check` exit 0 and silent, `run` exit 1 with one `Y110`. So was
`out(a.wrapping_sub(1))` (REP-46's measurement, filed onto the same row), and so
was every other missing method in the language.

**The cause is a division of labour, not a bug in a check.** `praxis check` runs
parse and `praxis_hir::analyze_root` and stops. `praxis run` runs those, and
*then* `praxis_hir::lower`. `lower` was the sole emitter of `Y110`. So every
`Y110` was invisible to `check` by construction — and invisible to the LSP too,
which never calls `lower` either.

Inference declined to report at both of its doors, deliberately and with the
reason written down at each:

- `infer_catalog_call`, on a concrete receiver with no matching row: "lowering
  owns `Y110` and has the name span."
- `resolve_deferred_method`, on a deferred receiver that resolved to a type with
  no row: "A *method* requirement is still left alone: lowering reports it, and
  it has the name span."

**The stated reason is false on its own terms.** The `key` that
`infer_catalog_call` is given *is* the method-name token's `TextRange`, and
`Constraint::at` on the deferred path is the same range. Inference has exactly
the span lowering claimed as its title to the report. `diagnostics::unknown_method`
already existed and built a better message — it named the receiver type, which
lowering's never did — and its arm in `report_cap_failure` was documented as
unreachable dead code.

There was a third leak, and it is the one that made Appendix D report eight
things instead of two. A receiver that is still a type *variable* becomes a
pending `HasMethod` constraint. `TypeDb::take_dischargeable` only ever returns
constraints whose variable has resolved, so a constraint on a variable nothing
pinned stays pending forever and is never looked at again. `infer_catalog_call`
then hands back a fresh variable — which is what `left` binds to in Appendix D,
so the later `.zip`, `.map` and `.sum` each see a receiver that is still a
variable, each defer, each are dropped, and each were reported by lowering. Six
of Appendix D's eight `Y110`s were cascade off `sorted`. `zip`, `map`-on-a-
pipeline and `sum` are all registered and all work.

## Decision

**Inference reports a method call that cannot resolve — either because the
receiver is known and has no such row, or because no receiver in the catalog has
that name at that arity — and lowering reports nothing.**

That is the whole rule. It has three consequences worth stating separately.

### 1. Both of inference's doors report

The concrete-receiver miss in `infer_catalog_call` and the deferred-receiver
miss in `resolve_deferred_method` both push `Y110`. This is REP-28's move at
`HasField`, made at `HasMethod`: two doors, one builder, one code, and the pass
that both `check` and `run` execute.

### 2. A name the catalog holds nowhere is refused before the receiver is known

`MethodCatalog::has_name_at_arity` is the predicate, and it lives on the catalog
because its justification is a fact about the catalog: **it is the complete
method universe of this language.** A record carries no rows (`p.len()` on
`struct P { len: Int }` is already a missing method, not a field read), an enum
carries none, and there is no user `impl`. So a name the table does not hold at
that arity can never resolve against *any* receiver, and deferring it is
deferring forever.

The predicate is spelled "no row holds this name at this arity" and not "no row
matches this receiver", and the difference is load-bearing. §5.2's
`fn total(values) { values.sum() }` must still infer with no call site: `sum`
exists on `Vec[T]` at arity 0, so the requirement is deferred as before and
answered when a call site says what `values` is. Only `x.nope()` is refused
early.

**This is a rule the field door does not have and could not have.** A field name
is a fact about a record the program may yet declare, three lines further down.
A method name is a fact about a fixed table compiled into the compiler. If this
language ever grows user-defined methods, this half of the rule loses its
justification and must go with it — `has_name_at_arity`'s doc comment says so
where the next person will read it.

**It applies to method calls only, not to subscripts.** REP-16 routes `m[k]`
through the same dispatch function under the catalog name `[]`, and gave it its
own diagnostic — "values of type `Set[Int]` cannot be indexed with 1 index(es)"
— precisely so that a user never reads ``no method `[]` ``. A subscript on a
receiver nothing has pinned therefore keeps deferring exactly as before. That
leaves one pre-existing gap untouched and unclaimed: `fn f(g, x, y, z) { g[x, y,
z] }` matches no row at any arity and is still silently accepted, because there
is no receiver to name in the sentence the subscript wording wants.

### 3. Lowering's report is deleted, not kept as a backstop

Two emitters for one code is what ADR-057 Decision 5 got wrong and REP-28
corrected. A second emitter would also be *invisible*: it fires only where the
first did not, so nothing observes it until it is the only report, and by then
it is the wrong one.

What is left at lowering is the `TypedExpr::MethodCall { lowering_symbol: None }`
fallback node, unchanged, and two things can still reach it:

- the body of an **uncalled** function, whose receiver no call site pinned. This
  bullet used to say `monomorphize` drops uncalled polymorphic originals so it
  never reaches MIR, and **that is false**: ADR-057 decision 5 pins the receiver
  to the declaration group's level, so the function generalizes to a *monotype*,
  `Scheme::is_polymorphic()` is false, and mono's drop filter keeps it.
  `fn f(v) { v.len() }` with `out(1)` reached MIR and ICEd for as long as this
  bullet stood. The real answer is
  [ADR-137](./137-a-deferred-receiver-resolves-in-rounds-and-the-channel-runs-to-a-fixpoint.md)
  decision 3: MIR recognizes a receiver that is still an unbound variable and
  lowers the call to an unconditional `panic`. The body is unreachable by
  construction — every call, including one through a value, unifies the argument
  and pins the receiver — so the `panic` is a statement of that invariant and not
  a behaviour a program can observe.
- a chain that somehow does reach MIR *with a concrete receiver*, which is a
  **compiler bug**. It surfaces as the MIR builder's existing ICE naming the
  method — the REP-40 preference: a compiler bug should read as a compiler bug
  report, not as a wrong answer and not as a user-facing type error.

### 4. One message, and it says both halves

```
no method `sorted` on type `Vec[Int]` taking 0 argument(s)
```

The type is what inference's builder said and lowering's could not; the arity is
what lowering's said and inference's did not. §5.4 asks for concrete language,
and the receiver type is the concrete half.

The one shape with no receiver to name — case (3) above, where the call is
refused because the catalog holds the name nowhere — gets its own wording:

```
no type has a method `nope` taking 0 argument(s)
```

Rendering the receiver there would print `?T`, a type variable's leaked internal
name, into a message required to be concrete, and it would be the least useful
half of the sentence. The name and the arity are the whole answer.

## Consequences

**What is bought.** All three reproduced programs now report identically at
`check` and at `run`. Appendix D reports at `check`. The LSP surfaces `Y110` for
the first time. `praxis check` stops being a command that can pass a program
`praxis run` will refuse — for this code, which was the largest remaining
instance of it.

**What is paid.** A generic body calling a name the catalog lacks is now
rejected without a call site. That is a behaviour change for programs nobody has
today, and it is the price of case (3) reporting at all.

**What improved by accident, and is now pinned by a test.** §5.2's uncalled
`fn total(values) { values.sum() }` used to emit a `Y110` at `run` — lowering
saw an unresolved receiver and reported it. It is clean at both commands now,
because lowering no longer reports and inference correctly defers a name the
catalog holds. The document's own example type-checks for the first time.

*Amended (ADR-137).* "By accident" turned out to be the whole of it: `sum` is a
**pipeline sink**, so MIR's `recognize_pipeline` claims the node before it can
ask for a catalog row, and the general case was not clean at all — the same
program with `len`, `push`, `get` or `contains` in place of `sum` reached MIR
and ICEd. ADR-137 decision 3 makes the general case hold for the reason this
paragraph claims.

**What is now cascade-free.** Appendix D's eight diagnostics became three — two
`sorted` and one `frequencies`, measured — and that is the honest intermediate
state, reported at `check` where the old eight were reported at `run`. The six
that vanished were never separate failures. (REP-33's other half then registered
the three §6.3 barrier rows, and the three became zero: the demo runs and prints
`11` / `31`.)

**Register correction.** REP-33's row and handover 17 both list five missing
names for Appendix D — `sorted`, `frequencies`, `zip`, `map` and `sum`. That
list is a transcription of the eight diagnostics rather than of the gap. Three
of the five are implemented and working, and Appendix D needs exactly `sorted`
and `frequencies`. REP-46's paragraph on the same row states that "`praxis
check` performs no method resolution at all"; that is not what the code did.
`check` ran full inference and full method resolution — `infer_catalog_call`
resolved and recorded `method_refs` — and what it did not do was *report* the
miss.
