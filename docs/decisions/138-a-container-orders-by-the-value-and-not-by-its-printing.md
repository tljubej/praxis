# ADR-138: A container orders by the value, and not by its printing

**Date:** 2026-08-06
**Status:** accepted
**Milestone:** 13

## Context

An Advent-of-Code solve walked a `Set[Int]` and got the wrong answer. Nothing
faulted, nothing printed suspiciously, and the program was correct:

```praxis
var s = Set()
s.insert(9)
s.insert(10)
s.insert(100)
s.insert(2)
out(s)            // {10, 100, 2, 9}
out(s.sorted())   // [2, 9, 10, 100]
```

Two orders out of one collection, and the wrong one is the one a `for` walks —
so this is an **answer**, not a formatting wart. A fold over the members computes
a different number, and the only signal is a reader noticing that the two lines
disagree.

The cause is a debt taken deliberately and then left. RT-16 established that a
hash-backed collection must be printed and iterated in a fixed order, because
Rust randomizes hash-table iteration per process and a program whose expected
output cannot be written down does not have one. ADR-066 decision 4 named the
order and the three functions that impose it — `maps::write_sorted`,
`maps::ordered_entries`, `maps::ordered_members` — and picked the *rendered
form* as the sort key, "the only total order available today, because
`TypeDescriptor::compare` is `None` on every descriptor".

That sentence stopped being true a day later. ADR-045 populated `compare` on
`Int`, `Byte`, `Char`, `Float` and `Text`, which is what `sorted()`, a heap's
`Ord` and `praxis_value_cmp` have gone through ever since. The three call sites
never asked. So the language shipped with two orders — one numeric, one
lexicographic — reachable from the same collection, and a book chapter
documenting the second as a limitation.

There is a second, quieter split inside the first. A printed `Map` sorted the
whole rendered *entry* (`"key: value"`) while `keys()`, `values()` and a `for`
sorted the rendered *key*. `':'` is 0x3A and `'1'` is 0x31, so `{a: 1, a1: 2}`
printed as `{a1: 2, a: 1}` while `m.keys()` answered `[a, a1]`. One `Map`, two
orders, and `collections.md` documented the disagreement rather than fixing it.

## Decision 1: `compare` is populated on every type a key can be

A `Map` key and a `Set` member are exactly what `capability::supports_hash_stable`
admits: the scalars `Int`, `UInt`, `Byte`, `Char`, `Float`, `Text` and `Bool`,
plus `Unit`, `Range`, and tuples, records and enums recursing over those. (`UInt`
has no runtime object, so it has no descriptor.) Every one of those descriptors
now carries a `compare`:

| Descriptor | Order |
|---|---|
| `Int`, `Byte`, `Char`, `Float`, `Text` | ADR-045's, unchanged |
| `Bool` | `false` before `true` |
| `Unit` | `Equal` — a singleton |
| `Range` | start, then end, over the normalized payload (ADR-059) |
| `Tuple` | arity, then element-wise left to right |
| `Record` | type identity, then arity, then field name and value, in schema order |
| `Enum` | type identity, then tag in **declaration** order, then payload-wise |

`compare` stays `None` on the eleven descriptors a key can never be: the nine
collections (a mutable collection is refused as a key by ADR-057 D4), `Closure`
(not even equatable) and `VarCell` (never a user value). So "has no container
order" and "can never be a key" are now the same set, which is what makes the
ordering total everywhere it is asked for.

The enum's tag is declaration order rather than variant name because that is the
order the type was written in: it is the order `enum_format` names the variants
in and the order a `match`'s arms are read in. Sorting the names would impose an
alphabet the declaration never mentioned, so a reader of the enum could not
predict the order without sorting it themselves.

## Decision 2: the order is total and deterministic by construction

`ordering::container_cmp` applies three rules in sequence, and each is total on
its own domain:

1. **Different descriptors order by descriptor id** — by the id, never by the
   descriptor's *address*. Addresses are assigned by the loader and differ
   between runs; ordering by one would put the per-process nondeterminism back
   in a place no in-process test could see.
2. **Same descriptor with a `compare` orders through it.** By decision 1 this is
   the rule that answers for every key, and it is the same callback `sorted()`
   uses — which is what makes `out(s)` and `out(s.sorted())` one sequence.
3. **Same descriptor without one falls back to the rendered forms.**

Rule 3 is load-bearing rather than defensive, and it is the reason ADR-066's
rendered-form sort is retained rather than deleted. A schema slot may be null
(ADR-066 decision 5): `var s = Set()` whose elements are never inspected leaves
the element type an unresolved variable, and `supports_hash_stable` answers such
a variable optimistically. So a value inside a composite key can be of a type the
compiler never resolved. Answering `Equal` there — the lazy alternative — would
leave ties in the sort whose resolution is the hash table's own randomized
iteration order, which is precisely the defect ADR-066 bought off.

`Float` NaN keeps ADR-045 decision 2's rule: it sorts last and equals itself.
This is the first time that rule reaches the three call sites; before, a
`Set[Float]` ordered by the string `"NaN"`, which happens to fall between `"1.5"`
and `"inf"`.

## Decision 3: the container order and the source order are different sets

`TypeDescriptor::compare` is the order a **container** walks and prints its keys
in. `praxis_hir::capability::supports_ord` is the **source language's** order —
`<`, `>`, `sorted()`, a heap element. They are deliberately different, and a
tuple is the case where they come apart: a `Map[(Int, Int), V]` walks its keys
element-wise, and `(1, 2) < (1, 3)` is still `Y006`.

`supports_ord` does not move. ADR-045 decision 1 rejected lexicographic products
as a *language* feature on the grounds that nobody had picked a semantics for
them, and that argument is untouched by the need to walk a hash table in a
reproducible order. The negative gate is
`supports_ord_is_the_source_order_and_not_the_container_order`, which exists so
that a future reader cannot "fix the inconsistency" by widening `supports_ord`
and quietly legalising the comparison ADR-045 refused.

The rejected alternative is to keep `compare` scoped to the source order and give
the containers a second, private comparison function. That is two orders to keep
in agreement, and the whole defect being fixed here is two orders that were not.

## Decision 4: a keyed collection prints in the order it iterates

`map_format`, `set_format` and `counter_format` now call `ordered_entries` /
`ordered_members` **first** and render in that order. `write_sorted` is renamed
`write_ordered` and no longer sorts; its only remaining job is the punctuation.

This deletes the rendered-entry sort, and with it the `{a1: 2, a: 1}` versus
`[a, a1]` disagreement. A printed collection is now a promise about its iteration
order, which is what a reader assumed it was.

## Consequences

- The AoC defect is gone: `out(s)`, `for x in s` and `out(s.sorted())` are three
  readings of one sequence for every key type.
- **Two book expectation files move**, both from lexicographic to numeric:
  `docs/book/examples/collections/order.out` and
  `docs/book/examples/control-flow/for-snapshot.out`. `order.px` is rewritten
  rather than re-blessed — its subject *was* the defect, so blessing it would
  have produced an example whose output contradicts its own comments.
- `ordered_entries` and `ordered_members` stop allocating a `String` per key on
  every `keys()`, `values()`, `for` and `out()`. That is a strict win, and if a
  benchmark regresses the cause is a composite `compare` walking a schema per
  comparison, not this.
- **A defensive posture is weakened, and it is worth naming.** `praxis_value_cmp`,
  `praxis_vec_sorted` and `praxis_vec_sorted_by_key` raise `TypeMismatch` when the
  element type has no `compare`. Composites now have one, so a *miscompile* that
  lowered `(1, 2) < (1, 3)` to one of those wrappers would be answered instead of
  faulted. No well-typed program reaches them — `supports_ord` and the catalog's
  `Ord` bound refuse it at `praxis check`, and
  `docs/book/examples/pipelines/pair-not-orderable.err` is the gate — so this is
  a lost backstop rather than a language change.
- No ABI bump. `compare` is a field on a `static TypeDescriptor` in
  `praxis-runtime`; no `#[repr(C)]` layout, no MIR, no codegen and no descriptor
  emission changes. One `TUPLE` descriptor serves every tuple shape, and the
  per-shape knowledge was already in the schema.
- ADR-045's "populated on five descriptors and stays `None` on the other sixteen"
  and ADR-066's "the rendered-form sort keys are still D3's to replace" are both
  discharged.
