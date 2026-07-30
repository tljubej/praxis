# ADR-062: An iterated parameter is generic in the iterable and monomorphic in its element

**Date:** 2026-07-30
**Status:** Accepted — implemented
**Milestone:** Repair (stage S26 — REP-03, REP-04)

## Context

```praxis
fn total(r) {
    var t = 0
    for i in r { t = t + i }
    t
}
```

```text
error[Y005]: values of type `Int` cannot be iterated
  3 |   for i in r { t = t + i }
    |           ^^
```

A legal program, rejected, and rejected in terms of a type it never wrote.
`capability::iter_item` answered an unresolved receiver with **itself**:

```rust
TypeData::Var(_) => return Some(t),   // ← the whole defect
```

so the loop variable and the iterator came back as one variable, and `t + i`
pinned the iterator to `Int`. Identically for `Vec`, `BitSet` and `Range` — which
is why every one of TY-34's gates annotates its iterated parameter, with a comment
saying so.

That is REP-03's *reported* half. Its silent half is worse:

```praxis
fn copy(vs) { var o = Vec()
  for v in vs { o.push(v) }
  o }
```

Nothing here pins the element, so nothing reports. `v` was typed as the
*collection*, `o` came out `Vec[Vec[Int]]`, and the program — accepted by `praxis
check` — faulted at run time with "value does not have the declared type".

REP-04 is the same defect from the other end. ADR-057 recorded it as a consequence
and said no finding asked for it:

> **`Iterable`'s `item` is not unified at discharge.** `check` answers the
> yes/no; which item type a receiver yields is established where the `for` is
> inferred. A constraint that resolves to a *differently*-itemed iterable would
> not be caught — no finding asks for it, and `iter_item` is a function of the
> receiver alone.

**That consequence is superseded here.** The two rows are one fix, and neither is
fixable alone: a fresh item variable is what gives the deferred `Iterable { item }`
constraint two things to relate, and unifying at discharge is what makes the fresh
variable resolvable. On the unfixed tree the differently-itemed program cannot even
be *observed* — it reports REP-03's `Y005` first.

## Decision 1: an unresolved receiver yields a fresh item variable

`iter_item` answers `Some(db.fresh_var())`. The optimism is unchanged — an
unresolved receiver is still iterable, as every other capability predicate is still
optimistic about a variable — but the item is now a *different* variable, so
pinning one no longer pins the other.

`capability::check`'s `Iterable` arm gains the early-out `HasMethod` already has:
a variable receiver answers yes without minting the item it will not use.

## Decision 2: the item variable is pinned to the declaration group's level

This is ADR-057 Decision 5's rule at a second door, for exactly its reason and no
other.

There is **one lowered body per source function**. Monomorphization clones a tree
lowering has already resolved and substitutes each binder from the call site's
*argument types*; it does not run the constraint channel. So an item variable that
only the channel can resolve would reach MIR unbound — and MIR reads the item type
to type the loop variable's slot and the debugger's view of it.

`TypeDb::pin_to_level` is how the rule is stated, and REP-03 is its second caller.
It is applied only to the fresh variable Decision 1 mints: a *concrete* iterator's
item is whatever the collection's argument is, and pinning that would clamp
`fn f() { var v = Vec()\n for x in v { … }\n v }`'s own element type for no reason.

## Decision 3: the **iterator** stays quantified

The obvious way to make the item resolvable is to pin the receiver instead, which
is what TY-30 does for a method receiver. It is the wrong answer here, and MIR
says why: `len_symbol_for` and `get_symbol_for` pick the runtime symbols from the
iterator's **static collection ctor**. A single lowered body cannot serve a `Vec`
and a `Range`, because one of them would read a length out of the wrong payload
word. One clone per iterable kind is not an optimization — it is the only way the
symbols can be right, and a quantified iterator is what provides the clones.

So `total` is `forall T. (T) -> Int`: **any iterable, of `Int`**. `total(v)` and
`total(0..4)` in one program are two clones, with `praxis_vec_*` and
`praxis_range_*` respectively.

Two call sites that disagree about the *element* are a disagreement about `total`'s
signature, exactly as two receivers at one method call site are (ADR-057 D5). Two
that disagree about the *ctor* are two instantiations. That asymmetry is the whole
decision, and it falls straight out of which facts lowering needs concrete.

## Decision 4: `Iterable` discharges by resolving, not by checking

`Inferer::resolve_deferred_iterable` joins `resolve_deferred_method` as the second
constraint discharged by *producing* something rather than answering a yes/no. It
asks the resolved receiver what it yields and **unifies** that with the item the
constraint carries.

`capability::check` could not do this. Its failure shape is `Err(offending type)`,
and "iterates, but not at that element type" is a **mismatch** — `Vec[Text]` is
perfectly iterable, so `Y005`'s wording would be a lie. The report is a `Y001` at
the use site with the requirement as its note (ADR-057 decision 2), because `for i
in r { t = t + i }` is correct for every other instantiation of `total`:

```text
error[Y001]: expected Int, found Text
  10 |   out(total(names))
     |       ^^^^^^^^^^^^
note: this is the operation that requires it
  3 |   for i in r { t = t + i }
    |           ^^
```

A receiver that is not iterable **at all** is still the channel's own `Y005`, from
the same function. `Diagnostic::with_note` on a finished diagnostic is what lets a
unification failure gain the second span, so `diag_unify` was split: it builds the
diagnostic through `unify_diagnostic` and pushes it, and this one caller takes the
unpushed value first.

## Consequences

- **`for` over an unannotated parameter works**, and works against each iterable
  the call site chooses. `total(v)`, `total(0..4)` and `total(d)` in one program
  are three clones with three sets of symbols.
- **`copy`'s loop variable is the element**, so `Vec[Vec[Int]]` — and the runtime
  fault it produced — is gone.
- **A differently-itemed receiver is reported**, which is the half that has never
  had a test.
- **A body that does not pin its element still typechecks and still runs**, at one
  element type per program. `fn each(v) { for x in v { out(x) } }` is `forall T.
  (T) -> Unit` with the item free; the first call decides it. That is the same
  narrowing Decision 5 already imposes through any method call on the item, and a
  `for` body that does nothing with the item is the only shape it can be observed
  in.
- **ADR-057's "`item` is not unified at discharge" is superseded**, and its
  Decision 5 now has two callers rather than one. The sentence in `pin_to_level`'s
  doc comment that says TY-30 is "the one caller today" is amended with it.
- **`iterable_is_answered_by_iter_item` and
  `unresolved_var_is_optimistically_iterable` still hold** — the optimism did not
  change, only what it answers *with*.
- **A `for` over a `Set`, `Map`, `Counter`, `Grid`, `BitSet` or heap still has no
  lowering** and segfaults, whether or not the parameter is annotated. That is
  pre-existing, independent of this ADR, and is registered as **REP-15**:
  `get_symbol_for` has arms for `Vec`, `Deque` and `Range` only, and the runtime
  has no indexed accessor for the other six to select. The exit criterion's
  `BitSet` case is satisfied at inference, which is what it asks for; the run half
  belongs to REP-15.
