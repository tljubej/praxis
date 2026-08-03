# ADR-118: A `Vec[T]`'s three words are the compiler's to read, and a `VecDeque`'s are not

**Date:** 2026-08-03
**Status:** accepted — implemented (part 1 of 2)
**Milestone:** post-M11 performance
([handover 25](../handovers/25-two-mallocs-per-runtime-call.md) §5 F-2,
[handover 26](../handovers/26-ten-packages-six-waves-and-the-five-things-25-got-wrong.md)
§4 W4a, wave 1)
**Amends:** nothing. ADR-013 chose `Vec<GcRef>` as `Vec[T]`'s storage and that
choice stands — what changes is only *which* growable vector, and the block's
size class, its `owned_bytes` charge and its descriptor callbacks are all
unmoved.

**This record is part 1 of 2.** W4a is the runtime half: it makes the layout
readable. W4b, in wave 3, is the backend half that reads it, and it appends to
this file rather than opening a new number — decisions 1 through 5 below are
about the container and are complete; the inlining decisions are 6 onward and
are not written yet. Nothing here changes generated code, the ABI version, MIR,
or any observable program behaviour.

## Context

Handover 25 §5 F-2 proposed inlining the collection primitives — `praxis_vec_get`
is a call with root spills in front of it, a `catch_unwind` inside it and an
`Inst::CheckFault` after it, for a bounds compare and a load — and gave a
one-sentence reason to believe it was cheap:

> The payload layouts are `#[repr(C)]` and their offsets are already exported for
> other reasons.

**The first clause is true of the payload and false of the field that matters.**
`VecPayload` is `#[repr(C)]`, and always has been. But its hot field is a
`std::Vec<GcRef>`, and `std::Vec` is `#[repr(Rust)]`: its pointer and capacity
live inside a private `RawVec`, `offset_of!` cannot name them from outside `std`,
and the order of the three words is not stable across compiler versions. The
second clause is false outright — handover 26 §1 correction 4 ran
`rg -n 'offset_of!' crates/`, found 19 hits, and **not one is on a collection
payload**.

So a backend that wants to answer `v.len()` inline has nothing it is permitted to
read. It is not a matter of finding the number: there is no number to find that
the language guarantees will still be there next toolchain.

That is the whole of W4a. It is a prerequisite, not a win.

## Decision 1: a pinned `#[repr(C)]` container, not a side table and not an accessor

`crates/praxis-runtime/src/repr_c_vec.rs` introduces `ReprCVec<T>` — pointer,
length, capacity, in that order, `#[repr(C)]` — and `VecPayload.items` becomes a
`ReprCVec<GcRef>`.

Three alternatives were considered and each fails on a property the vector
already has.

**A side table of lengths, keyed by payload address.** Every `push` would have to
write two places, and the two could disagree; a `Vec[T]` that reached the table
by one route and the payload by another is exactly the "two answers to one
question" shape MIR-10 exists to prevent. It also adds a lookup to the inline
path, which is the thing being removed.

**`Box<[T]>` plus a separate `len` field.** This is what M3 had, and M5 replaced
it *because* `push` had to reallocate and re-seal the payload (see
`collections.rs`'s module header). Re-splitting the length out means either
reimplementing amortized growth by hand — a second allocator in the tree, with
its own capacity-overflow arithmetic — or keeping a `Vec` beside the length and
having two capacities. Rejected on the same grounds as the side table, plus the
`RawVec` reimplementation.

**Exporting an accessor the backend calls.** That is `praxis_vec_len`, which is
the call being removed.

`ReprCVec` avoids the reimplementation entirely, which is the point of its shape:
it holds *only* the three words a `Vec` handed it. Every construction goes through
`from_vec`, and every mutation goes through `vec_mut`, which reconstitutes a real
`Vec`, lets `Vec` do the work, and takes the parts back. `RawVec`'s growth
policy, its capacity-overflow checks and its allocation-failure handling are all
still the ones running. What is new is only that the three words sit somewhere
the compiler is allowed to look.

The size claim handover 26 §9 lists as *"asserted from reading and never
checked"* is now checked, in the tree, in both toggle arms:

```rust
const _: () = assert!(size_of::<ReprCVec<GcRef>>() == size_of::<Vec<GcRef>>());
const _: () = assert!(align_of::<ReprCVec<GcRef>>() == align_of::<Vec<GcRef>>());
const _: () = assert!(size_of::<ReprCVec<GcRef>>() == 24);
```

They hold. **`VecPayload` is still 32 bytes**, so the block is still 48 with
ADR-109's header, the size class does not move, and the pacer's page density is
unchanged — which is why this record needs no ADR-112 interaction at all. That is
asserted too, beside the payload.

## Decision 2: `ptr` at 0, `len` at 8, `cap` at 16 — and the order is an ABI decision made a wave early

```rust
#[repr(C)]
pub struct ReprCVec<T> {
    ptr: NonNull<T>,   // offset 0
    len: usize,        // offset 8
    cap: usize,        // offset 16
    _owns: PhantomData<T>,
}
```

`ptr` and `len` are first and adjacent because they are the pair generated code
reads: `praxis_vec_get` wants both, `praxis_vec_len` wants the second. Combined
with `element_descriptor` staying at offset 0 of `VecPayload`, the reachable set
is `payload+0` (descriptor), `payload+8` (element pointer) and `payload+16`
(length) — three small displacements off one base register, all inside the same
32-byte payload and therefore the same cache line as whatever brought the object
in.

`cap` is last because no generated code has a reason to read it. Capacity is the
allocator's business; its one reader is `vec_owned_bytes`, which charges the
pacer for the buffer's real footprint rather than its occupancy (RT-04).
Putting the word nothing bakes at the end means that if a fourth field is ever
needed it lands past every displacement W4b will have emitted.

**No ABI constant changes this wave and the order is still an ABI decision.**
`RUNTIME_ABI_VERSION` stays at 19 and gains no paragraph here, because nothing in
generated code reads any of these offsets yet — W4b owns that bump's paragraph.
But the moment a backend emits a load at a displacement, the displacement is the
contract, and choosing it after the loads exist is choosing it under duress. So
it is chosen now and pinned now:

```rust
const _: () = assert!(offset_of!(ReprCVec<GcRef>, ptr) == 0);
const _: () = assert!(offset_of!(ReprCVec<GcRef>, len) == 8);
const _: () = assert!(offset_of!(ReprCVec<GcRef>, cap) == 16);
const _: () = assert!(offset_of!(VecPayload, element_descriptor) == 0);
const _: () = assert!(offset_of!(VecPayload, items) == 8);
```

In the tree, not in a sentence — the house rule for `#[repr(C)]` layout claims.
`a_backend_can_read_the_length_and_the_elements_out_of_a_live_payload` is the
rehearsal: it allocates a real `Vec[Int]` on a real heap, reads the length as a
`usize` at `payload+16` and the element buffer as a pointer at `payload+8`, and
walks the elements through it. That test is what W4b's first emitted load has to
agree with.

## Decision 3: the container can only ever hold what a `Vec` gave it, and a forgotten guard leaks rather than dangles

The dangerous state for a decomposed vector is *stale parts*: `ptr` naming a
buffer `RawVec` has since grown away from. `vec_trace` walks `items` on every
mark, so a stale pointer here is a wild read inside the collector rather than a
wrong answer, and it appears only after a reallocation — which is to say, it
passes every small test.

Two mechanisms make it unrepresentable rather than merely absent.

**There is one constructor.** `ptr`, `len` and `cap` are private and the only
function that writes them is `from_vec`, which reads them off a live `Vec`. So
"`cap` is the *allocated* capacity and not the length" — `Vec::from_raw_parts`'s
one easy contract to break, and a heap corruption rather than a wrong answer when
broken — is an invariant of the type, not an obligation on callers. It is also a
test: `a_vec_survives_the_round_trip_with_its_capacity_intact` over-reserves
(`with_capacity(17)` for 3 elements), decomposes, reconstitutes and asserts the
capacity survives, because a round trip that quietly substituted the length would
pass a contents-only assertion.

**`vec_mut` empties the container rather than aliasing it.** The guard takes the
three words out and leaves `ReprCVec::default()` behind; its `Drop` writes the
(possibly reallocated) parts back. The interesting case is the one that is not a
borrow-checker question: `std::mem::forget` on the guard. Because the container
was emptied on entry, forgetting the guard **leaks the buffer**, which is safe,
instead of leaving a container the collector will trace pointing at storage the
`Vec` may already have freed. `a_forgotten_mutation_guard_leaves_the_container_empty_rather_than_stale`
asserts exactly that, and it is the reason to pay three stores on a path that is
about to call `RawVec::grow`.

The guard's `Drop` also runs on the unwind path, which is not incidental: a
runtime wrapper whose mutation panics is caught by `abi_guard!` (ADR-080), and
the elements must still be there for the fault epilogue and the crash debugger to
walk. `a_mutation_that_panics_still_hands_the_elements_back` pins it.

**Ownership and `Drop` are one free, in the same place they always were.**
`VEC`'s `drop_value` is unchanged — it is still `drop_in_place` on the whole
`VecPayload` — and what that now runs is `ReprCVec`'s `Drop`, which hands the
three words to a `Vec` and lets `Vec` free the buffer. One allocation, one free,
reached by the same route as before. Three drop-accounting tests cover it: a
container drops every element exactly once, a round trip through `into_vec` drops
nothing, and a popped element is dropped by its new owner and not by the
container.

There are five `unsafe` blocks in the module — `from_vec`'s `NonNull`, the
`Vec::from_raw_parts` in `into_vec`, the two slice reconstitutions, and
`ManuallyDrop::take` in the guard's `Drop` — and each carries a `SAFETY` comment
naming the invariant and where it is established. Everything else, including all
of the mutating API and both `IntoIterator` impls, is safe code written on top of
those five.

## Decision 4: the reading API is the slice API, so the migration is eight lines

Most `.items` uses are reads. `Deref<Target = [T]>` and `DerefMut` give them
`len`, `is_empty`, indexing, slicing, `get`, `first`, `last`, `contains`, `iter`,
`sort_unstable_by`, `swap` and the rest with no edit at all. `IntoIterator` is
implemented for `&ReprCVec<T>` and `&mut ReprCVec<T>` explicitly, because trait
selection does not autoderef and `for item in &p.items` would otherwise fail at
every site.

The mutating API keeps `Vec`'s names and signatures — `push`, `pop`, `insert`,
`remove`, `swap_remove`, `clear`, `truncate`, `retain`, `reserve`, `append`,
`extend_from_slice`, `Extend`, `FromIterator` — each one line over `vec_mut`.

**The census, done by renaming the field and reading the compiler's answer rather
than by grepping.** 43 sites name `VecPayload.items` (35 field accesses and 6
struct literals in `praxis-runtime`, 2 reads in
`praxis-codegen-cranelift/tests/adversarial_audit.rs`). **Eight needed editing**,
and none of them is a mutation:

- six `VecPayload { items: … }` constructions, which now take a `ReprCVec`
  (`Vec::new()` → `ReprCVec::new()`, or `.into()` on a `Vec` the caller built);
- two `p.items.clone()` sites annotated `let items: Vec<GcRef>`, which become
  `.to_vec()` — `Clone` on the container answers a container.

**Twelve mutating sites compiled untouched**: ten `push`, one `extend`, one
`extend_from_slice`. Handover 26 §4 estimated "~20 mutating `.items.*` sites
migrate"; the real number of mutating sites that needed migrating is **zero**,
because the API kept its names. `Runtime::alloc_vec` and the parser's
`alloc_vec` also keep their `Vec<GcRef>` parameter and convert inside, so all of
their callers are unchanged too — the conversion is a decomposition of three
words, not a copy of the buffer.

That includes `crates/praxis-runtime/src/dynamic_key.rs`, which handover 26
singles out (at 419-420; it is at 418-420) as *"a multi-line `.items.push` that
the single-line census grep missed … and it is still a hard compile error once
the field type changes"*. **It is not a compile error.** `ReprCVec::push` accepts
it verbatim. The warning was right about the site being invisible to a grep and
wrong about the consequence, and the difference is a design choice this record
made rather than a fact about the tree: a container without a `push` method would
have broken it, along with eleven others.

## Decision 5: `DequePayload` is not migrated, and neither is `GridPayload` — for opposite reasons

**`Deque[T]` is declined, permanently for this shape.** `DequePayload.items` is a
`std::collections::VecDeque<GcRef>`: a ring buffer with a head index, where
element *i* is at `(head + i) % cap` and the storage wraps. Even with the fields
pinned, `praxis_deque_len` inline is `load head; load len` at best and
`praxis_deque_get` is a modulo — and the wrap means a `Deque` is not addressable
as a contiguous slice at all, so the `Deque` half of W4b's `vec_get` shape has no
inline arm to write. Handover 26 §8 drops it from W4 entirely and that is
correct. Reproducing a `VecDeque` as a pinned container would mean reimplementing
the ring arithmetic — the one thing decision 1 refused to do for growth — and
buying a `len` load on a benchmark (`bfs`) whose deque cost is `push_back` and
`pop_front`, not `len`. If it is ever wanted, the honest route is to make
`Deque[T]`'s payload a pinned ring with its own ADR, not to smuggle a
`ReprCVecDeque` in behind this one.

**`Grid[T]` is not migrated, and that is scope rather than judgement.**
`GridPayload.items` is a `Vec<GcRef>` and would migrate mechanically — it is
row-major and contiguous, and `g[y][x]` is exactly the shape `vec_get` is. It is
left alone because W4b's three primitives are `bitset_contains`, `vec_get` and
`vec_len`, none of which touch a `Grid`, and migrating a field with no reader
would be an unmeasured change to a payload in the same wave as a measured one.
This is a registered follow-up, not a refusal: it needs no new decision, only a
reason.

## The measurement, and why it is a regression check

**W4a should be performance neutral.** It changes a container's layout, not an
algorithm: the same buffer, the same growth policy, the same one allocation and
one free, the same 32-byte payload in the same size class. There is no win to
claim this wave, and a number that showed one would be evidence of a mistake
somewhere rather than of a success. What the measurement phase is looking for is
the *absence* of a slowdown, and the benchmarks with something to say are the
ones that reach a `Vec` in a hot loop: **`vm`** (38 `push` sites, an interpreter
whose stack is a `Vec`), **`bfs`** (7, and its adjacency structure is a
`Vec[Vec[Int]]` it subscripts in the inner loop) and **`tree`** (2). `hashwork`
has none and `pipeline` one, so those two are controls: if either moves, the
number is not this package's.

**The toggle is the `std-vec-payload` cargo feature on `praxis-runtime`**, and it
switches exactly one thing: `ReprCVec<T>` becomes a `#[repr(transparent)]`
newtype over `std::Vec<T>` with the same public API. Every caller in the tree,
every construction site, every `.push`, `VecPayload` itself and all 43 `.items`
uses are byte-for-byte identical in both arms. That is the toggle handover 26 §6
asks for — *this tree with this package's single toggle point reverted*, not the
previous commit, which would fold in W1 and W2 and report them as W4a's.

```
arm B (candidate) cargo build --release -p praxis-cli
arm A (baseline)  cargo build --release -p praxis-cli \
                      --features praxis-runtime/std-vec-payload
```

The toggle is verified to bite rather than assumed:
`a_backend_can_read_the_length_and_the_elements_out_of_a_live_payload` is
`#[cfg(not(feature = "std-vec-payload"))]` **because it fails under the feature** —
the payload then holds a `std::Vec`, whose word order is exactly the thing
nothing is allowed to assume, and the test reads a pointer where it expects a
length. A toggle whose test suite is indifferent to it is not a toggle.

All seven benchmarks produce byte-identical stdout under both arms (checked
untimed; this is a correctness check, not a measurement — no timing was taken in
the build phase, per handover 26 §6).

## What the sanitizer says

This package writes a container, so a green ASan run is a gate rather than a
nicety. `./scripts/asan.sh`, this tree:

| | `e4f42e6` baseline | this tree |
|---|---:|---:|
| passed | 1911 | **1967** |
| failed | 0 | **0** |
| `AddressSanitizer` reports | 0 | **0** |
| executables, all verified instrumented | — | 30 |

The count rose because wave 0 and wave 1 added tests, not because anything was
skipped; the release-profile suite is 1967 against `cargo test --workspace`'s
1969, the same two-test gap the baseline had (1911 against 1913) for the same
reason — a `debug_assertions`-gated pair.

**ASan reaches all of this and that is unusual for this round.** Handover 26 §7
trap 6 warns that a green run is necessary and not sufficient for W4b, W10 and
W8-S0b, because ASan does not instrument JIT-generated code. W4a emits no
generated code: every `unsafe` block it adds is compiled by rustc, on the paths
the suite exercises tens of thousands of times, and is therefore genuinely
covered. The sufficiency caveat starts at W4b, and part 2 of this record owes it.

**`scripts/asan.sh` had to be repaired to produce that run at all**, and the
defect is worth recording because it is silent in the safe direction only by
accident. The instrumentation check was `nm "$exe" | grep -q '__asan_'` under
`set -o pipefail`: `grep -q` exits at the first match, `nm` dies of SIGPIPE with
141, and `pipefail` makes that the pipeline's status — so the script reports "not
instrumented" for precisely the binaries large enough that `nm` is still writing.
It is a race, which is why it passed at `e4f42e6` and failed here against a
`praxis` binary carrying 25,429 `__asan_*` symbols. The fix is `grep` without
`-q`, which consumes its input. Had the comparison gone the other way the script
would have blessed an uninstrumented build.

## Consequences

- **The prerequisite W4b was blocked on is discharged**, and the thing it was
  blocked on was not "write some code" but "there is no legal number". There is
  now: `payload+8` is the element pointer and `payload+16` is the length, pinned
  by four `const _` assertions and one test that reads them off a live heap
  object.
- **`VecPayload`'s size, alignment, size class and `owned_bytes` charge are
  unchanged**, so ADR-109's page segregation and ADR-112's pacer see nothing.
  This is asserted, not observed.
- **The workspace has one more container type**, which is a real cost: `ReprCVec`
  is 24 bytes of layout the tree now owns and must keep agreeing with `Vec`. The
  `const _` pair is what keeps that agreement from rotting silently — if a future
  `std::Vec` grows a word, the build fails at the assertion rather than at W4b's
  emitted load.
- **`praxis-runtime` has a cargo feature, and it is the first one.** It exists
  for measurement and is never built by `just ci`. A second feature on this crate
  should be viewed with suspicion: the reason this one is acceptable is that it
  changes a private representation and nothing observable, so the two arms cannot
  disagree about behaviour, only about layout.
- **The two arms are not quite identical in what they permit.** `Vec<T>` is
  `Send`/`Sync` when `T` is; a `NonNull<T>` is unconditionally neither. The
  `#[repr(C)]` arm therefore carries `unsafe impl<T: Send> Send` and
  `unsafe impl<T: Sync> Sync` with `Vec`'s own reasoning — sole ownership of the
  elements — so that the toggle changes layout and *only* layout. `GcRef` is
  neither, so nothing in the tree exercises this today; it is there so that the
  arms cannot silently diverge in a second dimension later.
- **`Grid[T]` is a registered follow-up** (decision 5), and it should be taken by
  whoever first wants an inline `g[y][x]`.

## Open questions, for part 2

- **Does `praxis_vec_get`'s inline arm want the length load at all?** The bounds
  check needs it, but if W4b's fast arm keeps the root spills (handover 26 §4
  says `liveness::is_gc_safepoint` forces them; handover 27 §6 says the
  `ValueCmp` shape is the door out) the load may be free next to what remains.
  Measure before assuming the three-load sequence is the cost.
- **Does the `element_descriptor` at offset 0 buy anything for W4b?** It is what
  `vec_get` would need to prove the element type inline, and it is already on the
  same cache line. Nothing in W4b's stated scope reads it; noted so that part 2
  says yes or no rather than not noticing.
- **Should `ReprCVec` grow a `push_within_capacity`?** Handover 26 §8 defers
  `praxis_vec_push` because its fast arm needs a capacity check and a length
  write, which is a mutation in generated code. If that is ever revived, the
  runtime-side half is a method that fails rather than grows, and the ordering
  question — length written last, so a collection mid-sequence sees a shorter
  vector rather than an uninitialized slot — is W10's decision-1 argument in a
  second place. It is not written here because nothing needs it yet.
