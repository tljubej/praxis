# ADR-057: A capability requirement rides on the scheme that quantified it, and a key is hashable *and* immutable

**Date:** 2026-07-29
**Status:** Accepted — implemented; Decision 5 extended 2026-07-31 by REP-28
(`HasField`, the third capability discharged by resolving)
**Milestone:** Repair (stage S17 — F10's constraint channel, TY-25…TY-32, RT-08)

## Context

`praxis-hir`'s `capability.rs` held four free predicates deciding whether a type
supports equality, hashing, ordering or iteration. Two had **zero non-test
callers**. One — `supports_hash` — was literally `supports_eq`. And all four
answered the same way about an unresolved type variable: **yes, optimistically**.

That optimism is correct for a question asked about a specific type, and it is
what made the whole system unsound anyway, because inference asked the question
about *variables* and then threw the answer away:

```praxis
fn equal(a, b) { a == b }          // needs Eq(?a); ?a is then quantified
fn main() -> Bool { equal(f, g) }  // instantiated at a function type — accepted
```

The requirement was decided against `?a` (yes), discarded at generalization, and
never re-asked at the use site. Same shape for `for x in xs` (`iter_item` yields
an unresolved iterator *itself*, so `drain(1)` compiled) and for every `Map` key.

This ADR is the channel that fixes it, plus the five findings that consume it
and land with it. ADR-010's final consequence deferred §5.4's capability system
to M7; M8 pipelines, M9 `Option` and ADR-037 floats have all landed since, and
ADR-026 assumes hashability is enforced. **That deferral is superseded here.**

## Decision 1: a constraint is deferred, carried, and re-emitted

Three types, in three crates, each at the lowest level that can name what it
holds:

- `praxis_stdlib::capability::CapKind` — the payload-free vocabulary: `Eq`,
  `Ord`, `Hash`, `HashStable`, `Numeric`. `praxis-stdlib` cannot name a `Type`,
  and should not: the method catalog's type *patterns* live here and a pattern
  is not a type.
- `praxis_types::constraint::{Capability, Constraint}` — `Capability` adds the
  two arms that carry types (`Iterable { item }`, `HasMethod { name, params,
  result }`); a `Constraint` pairs one with a `VarId` and the spans.
- `praxis_hir::capability::check` — the one decision function, `Result<(),
  Type>` where the error names the *component* that failed.

The lifecycle is four steps and each has exactly one home:

1. **Emit.** `Inferer::require_cap` splits on whether the type is already
   concrete. Concrete is decided on the spot, exactly as before. A variable goes
   onto `TypeDb`'s pending list.
2. **Claim.** `generalize_at` takes the pending constraints about the variables
   it just quantified, and only those. One about a variable the enclosing scope
   still owns is not this scheme's to carry.
3. **Re-emit.** `instantiate_with_mapping` pushes each of the scheme's
   constraints again, re-pointed at the fresh variable this use put in the
   binder's place — including the types a `Capability` carries, which are
   written in the generic body's terms and would otherwise be shared between
   unrelated call sites.
4. **Discharge.** `take_dischargeable` drains the constraints whose variable has
   since **resolved**. What survives all four belongs to a variable nothing
   pinned, which inference has already reported as itself.

**Where discharge happens is the discipline, not an implementation detail.**
Inference drains after a function body and *before* that function generalizes,
so what is still pending at that moment is precisely what the scheme is about to
own. Draining after would report a generic body's requirement against a variable
nothing had pinned yet; draining before the body is finished would report it
against a variable the body had not finished constraining.

## Decision 2: the report goes to the use site, with the requirement as a note

A constraint carries **two** spans. `at` is where the program asked — the `==`,
the `for`, the `insert`. `via` is the instantiation that violated it.

```text
error[Y004]: values of type `(Int) -> Int` cannot be compared with `==`
  4 | fn main() -> Bool { equal(f, g) }
    |                     ^^^^^^^^^^^
note: this is the operation that requires it
  1 | fn equal(a, b) { a == b }
    |                      ^^
```

Reporting at `a == b` alone would name code that is correct for every other
instantiation of `equal`. Reporting at the call alone would leave the user
asking why. `TypeDb::instantiate_at` is what fills in the second span;
`Diagnostic::with_note` is what lets a wording helper's output gain a span it
could not have known about.

## Decision 3: a key is hashable **and immutable** (D4)

`CapKind::Hash` and `CapKind::HashStable` are two capabilities, not one.

`supports_hash` really *is* `supports_eq` — a descriptor's `hash` and `equals`
callbacks are defined together, so "can be hashed" and "can be compared" are one
question about the representation. That is exactly why it is the **wrong**
question for a `Map` key. A `Vec` hashes fine. What it cannot do is stay
findable: `key.push(2)` after `table.insert(key, v)` moves the entry's bucket
without moving the entry.

The rule is **mutability, not container-ness**:

- Out: `Vec`, `Map`, `Set`, `Deque`, `Grid`, `Counter`, `MinHeap`/`MaxHeap`,
  `BitSet`.
- In, structurally: scalars, `Text`, tuples, records, enums — a tuple or record
  is a key iff every component is. That is Python's `tuple` rule, and it is
  already how `supports_eq` recurses.

The precedent D4 turned on: **Python rejects mutable containers outright** —
`list`, `dict` and `set` set `__hash__ = None`. **Rust is the counterexample
that does not transfer**: `HashMap<Vec<i32>, V>` is legal only because the borrow
checker makes mutating a held key impossible, and Praxis has `var` mutation and
no borrow checker.

The requirement is asked **at the method call**, where a program actually puts a
value into a collection, and after the arguments have unified — so
`m.insert(key, 1)` has pinned `K` by then. An unresolved key rides the channel,
which is the common shape: `let m = Map()` mints two variables and the first
`insert` is what says what they are.

Wording follows §5.4 — never name the capability, never say "trait". `Y014` says
what the program did and why it cannot work: *a value of type `Vec[Int]` can
change after it is stored, so it cannot be used as a key.*

## Decision 4: `Numeric` and `Ord` are different sets

`Text` is orderable and is not a number. `Char` likewise. `Bool` is neither.
`CapKind::Numeric` is `Int`/`UInt`/`Byte`/`Float` and nothing else, and `%` is
narrower still — it is undefined for `Float` (TY-27), so it is not a capability
at all but a rule at one operator.

This is why `numeric_scalars_are_orderable` was rewritten (plan §8.2, H18's last
entry). Its assertions were still true — ADR-045 gave `Text` and `Char` real
runtime `compare` callbacks — but its *name* claims the two sets are one, and
this stage is where they stop being.

## Decision 5: a receiver a method was called on is pinned, not quantified (TY-30)

`HasMethod` is the one requirement that *produces* something when it holds, and
that is what makes it different from the other five.

A method call whose receiver is still a variable used to constrain nothing at
all. `catalog::lookup` needs a catalog-representable receiver, and a variable is
not one, so `fn total(values) { values.sum() }` gave up: the result was a fresh
variable, no `method_refs` entry was recorded, and lowering later reported a
method it could not find on a type nobody had named. §5.2 promises this exact
program needs no annotations and infers `total: Vec[Int] -> Int`.

The requirement goes on the channel — `HasMethod { name, params, result }` on the
receiver's variable — and discharge **resolves** it: look the method up against
the receiver the program pinned, unify the entry's receiver, parameters and result
with the ones the call site holds, and record the `MethodRef` lowering reads. The
result unification is what pins the call; the parameter unifications are what
give `fn add(v, x) { v.push(x) }` its `(Vec[Int], Int) -> Unit`.

**The receiver is pinned to the declaration group's level, so generalization
cannot quantify it.** `TypeDb::pin_to_level` is Pottier's level-lowering rule
applied deliberately rather than as a consequence of a link, and the reason is not
inference at all — it is lowering. There is **one lowered body per source
function**: monomorphization clones a tree lowering has already resolved. So one
method call site carries one catalog entry and one receiver type. A quantified
receiver would be N receiver types at one call site with no way to lower any of
them, which is why `total(vec_of_int)` and `total(vec_of_float)` in one program is
a disagreement about `total`'s signature and not two instantiations. §5.2 states
the same answer from the other end: `total` is `Vec[Int] -> Int`, a monotype.

The capability's own types are pinned with it. The result variable in particular:
quantifying it would let a call site instantiate a *fresh* result while discharge
unified the original, and the call would come out unconstrained.

**A failure is lowering's to report, not the channel's.** `Y110` has one emitter,
it has the method-name span, and it fires for both shapes that reach it — a
receiver the program pinned to a type without the method, and a receiver nothing
ever pinned. Reporting from the channel as well would be the same mistake twice.
So `HasMethod` resolves or stays silent; it never vetoes.

## Decision 6: a catalog type variable may be bounded, and the bound is a scalar identity, not a capability (TY-31)

`Vec[Bool].sum()` typechecked. So did `Vec[Float].sum()`, which returned
`9222246136947933184` — the float's bits, added as an integer. The catalog had no
way to say what a method requires of its own type variables:
`TypePattern::Var("T")` was unconstrained at all 75 occurrences.

`TypePattern::Var` carries an optional `Bound` now.
`MethodEntry::bounds()` sweeps the receiver, the parameters and the result and
reports each name once, because a bound is a fact about the *variable* and not
about the position it is written in — `sum` declares its requirement on the
receiver's element because there is nowhere else in the row for it to live.
`MethodCatalogBuilder::finish` refuses an entry that bounds one name two
different ways, so "whichever the checker read first" is not a thing that can
happen.

**The bound is `Bound::Is(ScalarType)`, and that is against expectation.** The
plan asked for `CapKind` bounds and the finding is worded that way ("numeric or
orderable element types"). Writing it found the opposite: `sum`, `product`, `min`
and `max` each lower to an `ExtractScalar` at `ScalarKind::Int` followed by an
`IntBinOp` or an `IntCmp`. `CapKind::Numeric` is `Int`, `UInt`, `Byte` **and**
`Float`, so a `Numeric` bound would have documented a width the lowering does not
have and blessed the `Float` case — the same shape of mistake as declaring `Text`
orderable while no `Text` comparison was lowered (P0-12). A capability is the
wrong *width* for an Int-only operation.

It is discharged by **unification**, not through the constraint channel, and that
is what makes it more than a check: a pipeline's intermediate element type is a
fresh variable when the sink is looked up, so unifying *pins* it. `v.map(|x|
"s").sum()` is rejected at the closure, and `v.map(|x| x * 2).sum()` is clean.
A deferred yes/no would have answered "optimistically capable" for both.

There is one `Bound` arm. The capabilities a row might otherwise want are already
enforced from the receiver's **type** rather than per row, which is strictly
stronger — a `Map` key must be hash-stable wherever the map is built, not only
when `insert` is called (Decision 3). The match on `Bound` in `apply_bounds` is
exhaustive, so a capability arm — which would have to route through `require_cap`,
because a capability about an unresolved variable must be deferred and not
unified — is a compile error to add halfway.

## Consequences

- **`Y013`, `Y014`, `Y015` and `Y016` are spent.** ADR-051 reserved all four for
  exactly these findings and they were unused. `Y015` is the *deferred* numeric
  failure and `Y010` is the immediate one: a compound assignment against a known
  `Bool` is reported at the operation and can name it, while the same requirement
  discharged after some later use pinned the target has left that operation
  behind. `Inferer::require_cap_as` is where the caller chooses.
- **A compound assignment's numeric requirement survives generalization.** S13
  reported it only for a target whose type was already known, deliberately: `a +=
  1` inside a generic function says nothing about `a`, and pinning it to `Int`
  would narrow every unannotated numeric binding. Deferring is the third option.
- **`enumerate` and `zip` say what they build.** Both rows declared `result:
  Vec[T]` — the receiver's element type — so `v.enumerate()` on a `Vec[Int]` came
  out `Vec[Int]`; `zip` additionally required the other sequence to have the
  receiver's element type. They are `Vec[(Int, T)]` and `Vec[(T, U)]` now. S15
  found this and recorded it as a finding the register does not have; this is the
  stage that touches the sequence rows. **`alloc_empty_vec` still does not read
  its element type from the chain's result** — that needs MIR-05's per-stage item
  types (S21), and H10's verifier rule still waits on it.
- **Generalization has a second rule now**, and it is a rule about lowering:
  level and *pinning*. `pin_to_level` is the only way to state it, and TY-30 is
  its only caller. A future stage that gives mono its own method resolution could
  lift the pin; nothing else can. *(ADR-062 gives it a second caller: a `for`'s
  item variable, for the same reason — mono substitutes from the call site's
  argument types and does not run this channel.)*
- **A requirement the receiver's own type carries reaches through a deferred
  method.** `resolve_deferred_method` calls `require_collection_invariants` on the
  receiver it just learned, so `fn store(m, k) { m.insert(k, 1) }` refuses a
  mutable key (`Y014`) even though `m` was never annotated. Decision 3 and
  Decision 5 compose without either knowing about the other.
- **`Diagnostic::with_note` is new** on the finished diagnostic, not only on the
  builder. §8.2 asks for "related spans when inference connects distant
  expressions", and this is the first inference that connects two.
- **`claim_constraints` follows before comparing.** The map's own key variable is
  what the requirement names; `insert` links it to the parameter's, and that is
  the one generalization quantifies. Comparing unfollowed ids leaves the
  constraint pending forever — a silent no-op, which is the failure mode the
  channel exists to prevent.
- **A duplicate requirement is recorded once.** The same `(var, cap, span)`
  discovered twice is one requirement; a loop body would otherwise push one per
  pass over the tree.
- **`HasMethod` is emitted and resolved (Decision 5, TY-30)**, and it is the one
  capability whose discharge writes to `method_refs` rather than answering a
  yes/no. Its gate is
  `collection_method_constrains_unannotated_receiver_parameter`.
- ~~**`Iterable`'s `item` is not unified at discharge.** `check` answers the
  yes/no; which item type a receiver yields is established where the `for` is
  inferred. A constraint that resolves to a *differently*-itemed iterable would
  not be caught — no finding asks for it, and `iter_item` is a function of the
  receiver alone.~~ **Superseded by ADR-062** (REP-04). Two findings asked for it
  in the end, and they are one fix: `iter_item` answering an unresolved receiver
  with *itself* is why a `for` over an unannotated parameter pinned that parameter
  to its own element type (REP-03), and unifying the item at discharge is what
  makes the fresh variable that replaces it resolvable. `Iterable` is discharged
  by `Inferer::resolve_deferred_iterable` now, and is the second capability that
  resolves rather than answers.
- **`HasField` is the third capability, and the third discharged by resolving**
  (REP-28, 2026-07-31). A field read on a receiver that was still a variable
  constrained nothing at all — the same defect Decision 5 fixed for a method call,
  at the other member syntax. §4.9's own example is the reproduction:
  `fn dist(a) -> Int { a.x + a.y }` passed `praxis check` and failed under `praxis
  run` with `Y112`. `Capability::HasField { name, ty }` rides the channel,
  `Inferer::resolve_deferred_field` asks the resolved record what the field holds
  and **unifies** it with the type the read handed back, and the receiver and the
  field's type are `pin_to_level`'d for Decision 5's reason: `lower_field_get`
  reads one record definition for the field's index, so one read site carries one
  record type. `pin_to_level` has three callers now.

  The division of reports is Decision 5's, unchanged: a receiver that resolves to a
  record without the field is left to lowering, which owns `Y112` and has the
  field-name span. So a never-called `fn dist(a) { a.x }` still reports there,
  exactly as a never-called `fn total(v) { v.sum() }` reports `Y110` there.
- **A `Bound::Cap` arm has no catalog row (Decision 6).** Nothing in the
  catalog needs one, because the receiver's type already answers those
  questions. The next row that genuinely does — a registered `sorted`, a
  `unique`, a `Vec.contains` — adds the arm and the `require_cap` route
  together.
