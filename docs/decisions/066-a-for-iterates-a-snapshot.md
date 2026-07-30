# ADR-066: A `for` iterates a snapshot, and the snapshot is where the order is decided

**Date:** 2026-07-30
**Status:** Accepted — implemented
**Milestone:** Repair (REP-15)

## Context

```praxis
let s = Set()
s.insert(1)
for x in s { … }
```

hung the process. `for i in b` over a `BitSet`, `for kv in c` over a `Counter`
and `for kv in m` over a `Map` killed it. A `MinHeap` over `[3, 1, 2]` summed to
`4349199564` — a silently wrong answer, which is worse than the crashes because
nothing reported it.

`capability::iter_item` has answered "yes, and here is the item type" for all ten
collections since M8. MIR's two symbol pickers had arms for `Vec`, `Deque` and
`Range` and a `_ => VecGet` / `_ => VecLen` default, so a `Set`'s payload was
read through `praxis_vec_get` — a `HashSet<DynamicKey>` reinterpreted as a
`VecPayload`. Nothing ever lowered six of the ten, and **no test ran a `for` over
one**, which is why three milestones went by.

This is not a missing match arm. The runtime had no accessor for the missing six
to select: `MapGet` and `CounterGet` take a *key*, `GridGet` takes `(x, y)`, and
`Set`, `BitSet` and the heaps had nothing at all. So the row needed a protocol
before it needed code.

## Decision 1: the loop walks a snapshot, not the collection

A `for` over a collection that cannot index itself calls **one** runtime wrapper
before the loop header, which answers a `Vec` of the members, and then walks that
`Vec` with the `praxis_vec_len` / `praxis_vec_get` pair the `Vec` case already
uses. Four new wrappers exist for the four collections that had none —
`praxis_set_items`, `praxis_bitset_items`, `praxis_min_heap_items`,
`praxis_max_heap_items` — and `Grid` uses `praxis_grid_cells`, which already
existed.

The alternative, an *nth-member* accessor per collection, is what the shape of
the existing code suggests and it does not survive contact with the collections:

- A `HashSet` and a `HashMap` have no nth member. Answering one is a linear scan,
  so every loop over a hashed collection would be quadratic.
- A `BinaryHeap`'s backing array is heap-ordered only at its root. `[3, 1, 2]`
  and `[3, 2, 1]` are both valid layouts for the same heap, so an indexed read of
  the array answers in insertion-history order — a *different* wrong answer from
  the one the defect had, not a fix.

A cursor object is the third alternative and costs the most: a new GC-visible
type, its rooting, and an invalidation rule for a collection mutated mid-walk.

The snapshot also *decides* the mutation question rather than deferring it: the
loop walks what the collection held when the loop began. `for x in s { s.insert(…) }`
is well-defined and terminates. A live cursor over a Rust hash table could not
offer that at all.

The cost is honest and bounded: one `Vec` per loop, O(n) space, and a mutation
during the walk is not seen by that walk.

## Decision 2: a keyed collection snapshots twice and pairs in MIR

`Map` and `Counter` yield `(K, V)` pairs. Their snapshot is REP-18's existing
`keys()` and `values()` rows — two calls, **index-aligned** because they share
one ordering (`maps::ordered_entries`) — and the loop body builds the pair from
`keys[i]` and `values[i]` with the same `AllocKind::Tuple` a `(a, b)` literal
lowers to.

The pair is built in **MIR** and not in the runtime, because the tuple's schema
is the compiler's answer already: `AllocKind::Tuple` resolves it from the item
type on the `For` node. Building it in the runtime would need a schema interner
there, and — the deciding reason — it would have to build the schema from the
*payload's* descriptors, and a `Map`'s value descriptor is always `INT` (it is
recorded for formatting and never specialized). A `Map[Text, Text]` would get a
pair that reads its value as an `i64`.

So no new runtime wrapper exists for `Map` or `Counter`: REP-18's rows are the
protocol, and `keys()`/`values()`/`for` are three callers of one order.

## Decision 3: `for (k, v) in m` stays REP-10's

The binding is one name. A `Map`'s member is a `(K, V)` tuple read with `kv.0`
and `kv.1` (REP-08). Destructuring in binding position is a *pattern*, and
patterns for records and tuples are REP-10 — the same grammar, and no reason for
two.

## Decision 4: the plan is total, and each iterable's order is its accessors'

`IterPlan` is an exhaustive match over `CollectionCtor` with no default arm. The
`_ => VecGet` default **is** the defect: it turned "this collection was never
wired" into "this collection reads like a `Vec`". A new collection constructor is
a compile error until someone says how it iterates.

Each order is the one that collection's own accessors already promise, so `out(c)`
and `for x in c` never disagree:

| Iterable | Order | Shared with |
|---|---|---|
| `Vec`, `Deque`, `Range` | in place, by index | — (no snapshot) |
| `Set` | ascending by rendered member | `set_format` (`maps::ordered_members`) |
| `Map`, `Counter` | ascending by rendered key | `keys()`, `values()`, formatting |
| `BitSet` | ascending bit | `bitset_format` |
| `MinHeap`, `MaxHeap` | pop order | `pop`, formatting |
| `Grid` | row-major | `cells()` |

Determinism is a correctness property here and not a tidiness one, which is
RT-16 with teeth for the third time: Rust randomizes hash-table order **per
process**, and a `for` that concatenated its members would answer differently on
two runs of the same program. The rendered-form sort keys are still D3's to
replace; `write_sorted`, `ordered_entries` and `ordered_members` are the three
places that change when `TypeDescriptor::compare` is populated.

A heap's is the one order here that is *meaningful* rather than merely fixed, and
it waits on nothing: a heap carries an ordering by construction.

## Decision 5: an unknown tuple slot is null, and the value answers for it

`let m = Map()` generalizes at the `let`, so a `for kv in m` whose body never
opens the pair leaves `K` and `V` unresolved — and `tuple_schema_for` required
every slot to resolve, so that program failed to compile.

A `TupleSchema` slot may now be **null**, meaning "the compiler had no static
type here", and the runtime reads the value's own descriptor off its header for
such a slot. This is the same answer `collection_arg_descriptor` already gives an
unresolved element type, for the same reason (HIR-01/MONO-01, hazard H10):
refusing to compile rejects a working program. The header is what makes it safe
rather than merely permissive — an object always knows what it is, and a slot
whose two values disagree about their type is *unequal* rather than one being
read as the other.

Arity is unaffected, which matters: degrading to the zero-element schema — the
other thing `tuple_schema_for` already does, for `MirType::Opaque` — drops every
element on the floor, because `praxis_alloc_tuple` sizes the payload from the
schema.

## Consequences

- **Ten iterables have a `for` lowering**, and `a_for_reaches_every_member_of_every_iterable`
  is the gate that did not exist. Two of its failure modes are not assertions:
  the test process used to hang and to die.
- **No new diagnostic code** and **no ABI version bump**: four additive wrappers,
  no `#[repr(C)]` change that generated code reads.
- **`for` is the only caller of the four new wrappers.** They are not catalog
  rows, so no program can spell them — `s.items()` is `Y110`. If a `.items()`
  method is ever wanted, it is a catalog row over the same wrapper.
- **One source `for` body still serves every iterable it is given** (ADR-062):
  the iterator stays quantified, monomorphization makes one clone per iterable
  kind, and each clone picks its own plan from a concrete ctor. The corpus
  program's `digits(c)` walks six different collections.
- **A `Grid` is iterable and yields cells**, which `iter_item` has always said and
  nothing had executed.
