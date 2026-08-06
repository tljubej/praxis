# ADR-041: A size the host cannot serve is a fault, not an abort

**Date:** 2026-07-28
**Status:** Accepted
**Milestone:** Repair (stage S7 — RT-07, RT-09)
**Amends:** ADR-017's fault list (`FaultKind` gains `InvalidSize`); ADR-028's
`DynamicKey` contract (equality now requires descriptor identity)
**Amended by:**
[ADR-146](./146-a-collection-constructors-arity-is-its-shape.md) — `VecExtent`
joins decision 1's validated newtypes, and decision 2's cap gains
`VecExtent::MAX_ITEMS`

## Context

`Grid[T](width, height)` and `BitSet.insert(n)` took a user `Int` straight to a
Rust allocation size:

```rust
let cells = vec![unit; (width as usize) * (height as usize)];   // praxis_grid_new
self.words.resize(word + 1, 0);                                 // BitSetPayload::insert
```

`width = -1` casts to `usize::MAX`. `width = 2^40, height = 2^40` overflows the
multiplication — a debug panic, a wrapped nonsense length in release.
`bs.insert(10^18)` asks `Vec::resize` for 10^16 words. Each of these ends in a
panic or an OOM abort raised *inside* an `extern "C"` function, which is
undefined behaviour on the way out and a dead process at best. The program that
caused it gets no diagnostic, because the fault channel was never reached.

Guarding at each call site is what the code already tried: `praxis_bitset_insert`
had `if i >= 0`, which is why negatives were silently dropped rather than
crashing — a smaller lie in place of a bigger one, and no help at all against
`10^18`.

## Decision 1: the validated newtype is the only route from `Int` to a size

`GridExtent::new(width: i64, height: i64) -> Option<GridExtent>` and
`BitIndex::new(value: i64) -> Option<BitIndex>` are the only constructors, and
they are what the payload methods take:

```rust
impl BitSetPayload {
    fn insert(&mut self, i: BitIndex);      // not usize
    fn contains(&self, i: BitIndex) -> bool;
    fn remove(&mut self, i: BitIndex);
}
```

An unbounded resize is therefore not something a caller forgot to guard — it is
something a caller cannot spell. `GridExtent` additionally carries the product
it proved (`cells()`), so the multiplication happens once, where it is checked.

**A third constructor, `VecExtent::new(len: i64) -> Option<VecExtent>`
([ADR-146](./146-a-collection-constructors-arity-is-its-shape.md)).** `Vec(n,
fill)` takes a user `Int` to a `vec![fill; n]`, which is this ADR's opening
example with one dimension instead of two, so it takes this ADR's answer: the
newtype is the only route, and `praxis_vec_filled` cannot reach the allocation
without one.

**The Context above describes a path that is live again.** `Grid[T](width,
height)` was unreachable from source for as long as `Grid()` was nullary — the
codegen passed two `iconst 0`s — and ADR-146's `Grid(w, h, fill)` reopens it
with a user-supplied extent. Nothing about that is a repeat of RT-07, and the
reason is this decision: `praxis_grid_filled` calls the same `GridExtent::new`,
so the negative width and the overflowing product are refused before an
allocation size exists.

## Decision 2: the bound is a cap, not merely "fits in a `usize`"

`GridExtent::MAX_CELLS = 2^28` (2 GiB of `GcRef` before a single cell object
exists). `BitIndex::MAX = 2^32 - 1` (a 512 MiB word vector).
`VecExtent::MAX_ITEMS = GridExtent::MAX_CELLS` (ADR-146) — the same number for
the same reason, because a `Vec` cell and a `Grid` cell are the same eight bytes
and a program that wants more of one plausibly wants more of the other.

A `checked_mul` alone is not enough: `Grid[Int](2^40, 2)` multiplies cleanly and
then aborts the process anyway. The numbers are a judgement about what a Praxis
program plausibly asks for. A program that wants more gets a fault it can catch
rather than a SIGKILL it cannot.

## Decision 3: `FaultKind::InvalidSize`, and only where the request cannot be honoured

A new kind rather than reusing `IndexOutOfBounds`: a negative *extent* is not an
index, and a crash debugger that says "index out of bounds" for
`Grid[Int](-1, 4)` is lying about the program.

Which operations fault is decided by whether the request can be honoured at all:

| | |
|---|---|
| `Grid[T](w, h)` with a bad extent | **faults** — there is no grid to return |
| `BitSet.insert(n)` out of range | **faults** — the caller asked the set to contain something, and it will not |
| `BitSet.contains(n)` out of range | `false` — a value the set cannot hold is a value it does not contain |
| `BitSet.remove(n)` out of range | no-op — likewise |

Queries stay total. Only the two mutations that cannot deliver what was asked
raise. This is a behaviour change for negative `insert`, which used to vanish.

Adding a `FaultKind` variant needs no ABI bump: generated code never switches on
the kind, and the `#[repr(C)]` enum's width is unchanged. S7's one bump (H17) is
still RT-17's.

## Decision 4: neighbour offsets are `checked_add`

`(i64::MAX, 0).neighbors4()` overflowed `px + dx`. A coordinate that overflows
is outside every grid — `GridExtent` bounds the extents far below `i64::MAX` —
so the overflow case *is* the out-of-bounds case, and one `grid_neighbor` helper
answers both.

## Decision 5: `DynamicKey` equality requires descriptor identity (RT-09)

`DynamicKey::eq` dispatched the left key's `equals` callback against the right
key's payload without first checking that the two agreed on a type. Two objects
of different types could therefore be compared through one of their layouts —
and the result could disagree with `Hash`, which is the invariant a `HashMap`
is built on.

Equality now short-circuits on `ptr::eq` between the descriptors (pointer, not
id: ADR-038 makes the address the authoritative identity), so a foreign payload
is never handed to a callback at all. `Hash` leads with the descriptor id, which
keeps the two consistent in the same direction.

`DynamicKey`'s fields are now private. The descriptor was always *derived* from
the value's own header inside `new`, but the public fields let a caller pair any
descriptor with any value; that pairing is now unrepresentable.

## Consequences

- A `Grid` or `BitSet` operation the host cannot serve is observable to the
  program instead of fatal to the process. This is a strict improvement over the
  status quo but is not a general panic-across-FFI policy: **D12 remains open**,
  and other wrappers still reach Rust panics on malformed input.
- The caps are visible in the language: a legitimate 2^29-cell grid now faults.
  If a real program ever wants one, raise `MAX_CELLS` — do not delete it.
- `praxis_grid_new` and `praxis_bitset_insert` moved from `Effect::Allocates` to
  `Effect::AllocatesAndFaults` in the manifest. Every method call already emits a
  `CheckFault`, so no lowering changed.
