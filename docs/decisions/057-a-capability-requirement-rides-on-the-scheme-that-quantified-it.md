# ADR-057: A capability requirement rides on the scheme that quantified it, and a key is hashable *and* immutable

**Date:** 2026-07-29
**Status:** Accepted — implemented
**Milestone:** Repair (stage S17 — F10's constraint channel, TY-25…TY-29, TY-32, RT-08)

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

## Consequences

- **`Y013`, `Y014`, `Y015` and `Y016` are spent.** ADR-051 reserved all four for
  exactly these findings and they were unused.
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
- **`HasMethod` has the shape and one consumer short of a fix.** `check` answers
  it through the method catalog, but nothing *emits* one yet: a method call on an
  unresolved receiver still returns a fresh variable. That is **TY-30**, and it
  is what `collection_method_constrains_unannotated_receiver_parameter` gates.
- **`Iterable`'s `item` is not unified at discharge.** `check` answers the
  yes/no; which item type a receiver yields is established where the `for` is
  inferred. A constraint that resolves to a *differently*-itemed iterable would
  not be caught — no finding asks for it, and `iter_item` is a function of the
  receiver alone.
- **Catalog rows still declare no bounds.** `TypePattern::Var("T")` is
  unconstrained at all 74 sites, so `Vec[Bool].sum()` still typechecks. That is
  **TY-31**, whose fix the plan sizes as one commit across those sites.
