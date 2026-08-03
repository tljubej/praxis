# ADR-118: A `Vec[T]`'s three words are the compiler's to read, and a `VecDeque`'s are not

**Date:** 2026-08-03
**Status:** accepted — implemented (complete: part 1 W4a, part 2 W4b)
**Milestone:** post-M11 performance
([handover 25](../handovers/25-two-mallocs-per-runtime-call.md) §5 F-2,
[handover 26](../handovers/26-ten-packages-six-waves-and-the-five-things-25-got-wrong.md)
§4 W4a/W4b, waves 1 and 3)
**Amends:** part 1 amends nothing. ADR-013 chose `Vec<GcRef>` as `Vec[T]`'s
storage and that choice stands — what changes is only *which* growable vector,
and the block's size class, its `owned_bytes` charge and its descriptor
callbacks are all unmoved. **Part 2 amends one sentence of ADR-113** — see
decision 6 — and carries an amendment to
[ADR-122](./122-a-descriptor-the-compiler-wrote-is-provable-and-a-parameter-is-not.md)'s
census, which is recorded in that file.

**This record is in two parts.** W4a is the runtime half: it makes the layout
readable, and decisions 1 through 5 are about the container. W4b, in wave 3, is
the backend half that reads it, and it appends here rather than opening a new
number: **decisions 6 through 10, and everything below them, are part 2.**
Nothing in part 1 changed generated code, the ABI version, MIR, or any
observable program behaviour; part 2 changes the first three and none of the
fourth.

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
untimed in the build phase; that is a correctness check, not a measurement).

### What the clock said, after the wave merged

Five A,B,B,A reps with the leading arm alternating, the median of the per-pair
ratios against a bar of the scaled MAD of the same ratios:

| | paired | |
|---|---:|---|
| `mandelbrot` | 1.018× | unresolved |
| `tree` | 1.005× | unresolved |
| `primes` | 0.998× | unresolved |
| `vm` | 0.996× | unresolved |
| `collatz` | 0.995× | unresolved |
| `bfs` | 0.990× | unresolved |
| *control* `hashwork` | 1.020× | unresolved |
| *control* `pipeline` | 1.012× | unresolved |
| **geometric mean** | **1.000×** | |

**Six of six non-control deltas the clock could not resolve, and the geometric
mean is 1.000×.** That is the result this record predicted and it is the right
one: the prediction above was that a measurable speedup would be evidence of a
mistake, and there is not one. Note in particular that `vm` and `bfs` — the two
benchmarks that reach a `Vec` hardest, and the two this section nominated as
having something to say — are at 0.996× and 0.990×, both inside their own spread.
Both controls also stayed inside theirs.

Taken with the 1-minute load at 2.2–3.1 and no competing build; §6's 0.5 ceiling
is unreachable on this machine and was explicitly waived. For a null result that
is a weaker caveat than it would be for a win — a wider bar makes "no difference"
easier to report, so the honest statement is that this measurement **can rule out
a regression larger than roughly 2%, and cannot rule out a smaller one.** Nothing
in the design suggests one: the container holds the same three words the `Vec`
held, in an order the compiler no longer has to guess.

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

---

# Part 2 (W4b): the backend half, which reads what part 1 pinned

**Date:** 2026-08-03
**Status:** accepted — implemented
([handover 26](../handovers/26-ten-packages-six-waves-and-the-five-things-25-got-wrong.md)
§4 W4b, wave 3, plus
[handover 27](../handovers/27-the-five-gates-and-what-26-got-wrong.md) §6, the
W4 orphan)

Part 1 ends by saying the prerequisite is discharged: there is now a legal
number at `payload+8` and `payload+16`. Part 2 emits the load. Three collection
reads — `bs.contains(x)`, `v[i]`, `v.len()` — get an inline arm behind ADR-102's
descriptor proof, with the wrapper kept exactly where it was as the cold block
both bail-outs branch to. On the path a loop takes there is now **no call at
all**: one, one and one before, zero, zero and zero after, measured off the
dumps rather than asserted (see "What the counts say").

## Decision 6: `bs.contains(x)` is its own MIR instruction, and *that* is what stops it being a safepoint

`praxis_bitset_contains` is 12.6% of `bfs` (handover 25 §2) for a word load and
a shift. Handover 26 §4 read the spill in front of it as unavoidable, and its
reasoning had two true premises: `liveness::is_gc_safepoint` matches every
`Inst::Call` whatever the symbol's `Effect`, and ADR-113 settled that a backend
arm may not narrow a spill from what it happens to emit. **Both are true and the
conclusion does not follow.** `is_gc_safepoint` is a *shape* match over `Inst`,
and `Inst::ValueCmp` is deliberately absent from it. A `Pure` primitive given
`ValueCmp`'s shape is therefore not a safepoint — not because a backend arm
decided it was cheap, but because MIR classified the instruction, in the pass
that states the safepoint property.

So `bs.contains(x)` gets its own instruction:

- the manifest row moves from `-> Gc` to `-> RawI64`, so the wrapper answers the
  `0`/`1` it used to box;
- `Inst::BitsetContains` carries a `Scalar(Bool)` dst and **no `RootSlots` field
  at all**, which is the "make illegal states unrepresentable" form of the
  claim: there is no slot set to leave empty by accident, because the variant
  cannot hold one;
- the builder re-boxes through `Materialize{Bool}`, so `lower_expr_gc`'s
  contract is untouched and every consumer of `contains` sees what it always
  saw.

`Inst::StructEq` and `Inst::ValueCmp` are the precedent for each of those three
independently. Nothing here is new machinery; what is new is noticing that a
catalog row could ask for it.

**This is the amendment to ADR-113, and it is a narrowing rather than a
loosening.** ADR-113's rule reads that a spill may not be narrowed by a backend
arm on the strength of what that arm emits — because the arm is downstream of
the pass that decided, and two arms emitting the same call would then disagree
about rooting. Giving the primitive a MIR instruction **satisfies** that rule
rather than evading it: the decision moves *upstream*, into the pass whose job
is to state which instructions are safepoints, and every backend arm for the new
instruction inherits one answer. A reading under which this violates ADR-113
would also forbid `Inst::ValueCmp`, which predates it.

### `MethodLowering::ScalarPrimitive`, and why a test rather than taste made it a third variant

The catalog needed a third `MethodLowering` arm to express the row, and the
reason is a test:
`a_non_faulting_row_with_a_value_result_cannot_answer_the_unit_sentinel` sweeps
every `RuntimeSymbol` row and requires a value result to be `AbiRet::Gc`. That
sweep is **right** about wrappers that answer a `GcRef` — the alternative is a
row that returns the unit sentinel through the value channel, which is the class
of confusion it was written for — and **wrong** about one that answers the
scalar channel on purpose. Loosening it would have cost the check on the forty-
odd rows it *is* right about — the sweep asserts its own reach, precisely so
that skipping rows cannot silently empty it — so `ScalarPrimitive` is the
distinction made representable, and it owes the sweep a companion, which it has.

Two facts follow from the variant that `RuntimeSymbol` cannot express, and they
are written on the variant rather than in this record: the answer is not a
`GcRef`, and the call site's safepoint status is the instruction's rather than
`Inst::Call`'s.

**The variant is not what MIR dispatches on**, and that is deliberate.
`build::lower_method_call` asks the *manifest* whether the row answers
`AbiRet::RawI64`, so there is no second symbol list to keep in step with the
catalog's. Two refusals close the gap from both ends:
`a_scalar_primitive_row_answers_the_scalar_channel_and_a_scalar_type` refuses a
`ScalarPrimitive` row whose wrapper still answers a `GcRef` — the state that
would put a raw `0`/`1` into a rootable slot, which is P0-03 — and
`lower_scalar_primitive`'s fallthrough is an ICE naming any `RawI64` wrapper
reachable from a method call that no `Inst` produces. Neither is a test that
compares two lists, because there is only one list.

### Measured on the shape alone, this commit was five CLIF instructions *worse*

At the moment the orphan landed, before any inlining, a one-`contains` loop went
**361 → 366 CLIF instructions**: the box moved from inside the wrapper, where it
was out of line, to `emit_inline_bool` at the call site. The root spills did not
vanish either — they moved to the `Materialize`, so four root stores sat *after*
the call rather than before it. That is what the dump said and it is not what
handover 27 §6 predicted.

It pays twice afterwards, and both are now measured rather than promised. The
next commit deletes the call. And W8-S0's block-local box/unbox forwarding
deletes the pair — see "The interaction with W8-S0" below, where the block
holding `b.contains(k)` ends with a census of exactly `{BitsetContains: 1}`.

## Decision 7: `BitSetPayload.words` migrates too, because part 1 missed the one primitive that had nothing to read

Handover 26 §4 calls `praxis_bitset_contains` "the cleanest" of W4b's three
primitives, and it is: `Pure`, no allocation, no fault, no pacing obligation,
one word load and one shift. Part 1 migrated `VecPayload.items` to `ReprCVec`
and left `BitSetPayload.words` a `std::Vec<u64>` — whose three words live in a
private `RawVec` in no guaranteed order. **The cleanest of the three was the one
with nothing legal to load.**

So `words` migrates, at the same size (24 bytes), the same alignment and the
same size class, so no descriptor, `owned_bytes` charge or page density moves.
`const _: () = assert!(...)` says so at the three places that could falsify it,
rather than this paragraph saying it.

The interesting consequence is not the migration but what it licenses.

### The inline membership test emits no range check, and that is a decision

The wrapper is `BitIndex::new(i).is_some_and(|b| p.contains(b))` — a range test
against `0..=BitIndex::MAX`, then a word probe. The inline form emits **only the
word probe**, because the probe subsumes the range test exactly, for every value
of the type:

- `BitIndex::MAX_WORDS` is `2^26`, and `BitSetPayload::insert` — the only thing
  in the tree that grows `words` — resizes to `word + 1` for a `BitIndex`, so
  `words.len() <= 2^26` always;
- `x < 0` reinterpreted unsigned is `>= 2^63`, so `word >= 2^57`;
- `x > MAX` reinterpreted unsigned is `>= 2^32`, so `word >= 2^26`.

Both out-of-range classes land at or above every possible word count, so
`word >= words.len()` refuses precisely the values `BitIndex::new` refuses, and
accepts precisely the ones it accepts.

That is an identity, which is to say it is the kind of thing that gets *believed*
rather than checked — the values where the two forms could disagree are the ones
no program produces on purpose.
`the_word_probe_generated_code_emits_answers_contains` checks it against
`contains` over both extremes of the type, every boundary and a dense sweep, in
the module that owns the range. It is ADR-113's
`the_unsigned_range_test_generated_code_emits_answers_index_of` in a second
place, and the reason to write it is the same one.

### `absent` is inline, and is not a bail-out

A word past the end is a **correct answer** — an empty or short `BitSet` answers
`false` — not a refusal. Routing it to the cold block would make the common
early state pay a call, and in `bfs` the visited set is short for the whole
first level of every search. So the inline form has two hot exits and one cold
one.

That has a consequence for measurement which is recorded under "What the counts
say": it is the first loop in the round whose hot path is not unique.

## Decision 8: three primitives, three `Effect`s, three different discharges

The three rows are `Pure`, `Faults` and `Allocates`, and the fast arm has to
answer for each. None of the three is discharged by assertion.

**`praxis_bitset_contains` is `Effect::Pure`.** Nothing to discharge: no
safepoint, no spill, no `CheckFault`, and decision 6 is what makes MIR agree.

**`praxis_vec_get` is `Effect::Faults` (`IndexOutOfBounds`), and the fast arm
cannot fault.** An index the bounds test rejects goes to the cold block, which
calls the wrapper, which raises exactly as it always did. The `Inst::CheckFault`
MIR emits after the call is unchanged and reads a flag the fast arm never
writes — so ADR-088's rule is satisfied by construction rather than by a second
path through it. The bounds test is *one* unsigned compare covering both the
sign and the bound, which is `praxis_vec_get`'s own `idx < 0 || idx as usize >=
len` and ADR-113's identity in its third place.

**`praxis_vec_len` is `Effect::Allocates`, and this is the one that could have
been done dishonestly.** The obvious fast arm reads the length word and boxes
it. Boxing is where the allocation is, and the allocation is where the
*collection pacing* is: ADR-112's pacer advances on allocation, so a `v.len()`
in a loop that never reached an allocator would defer the collector
indefinitely. That is ADR-113 decision 1's rejected alternative arrived at from
a different direction, and it is a bug that would not show up in any test that
does not measure heap growth.

The fast arm therefore **delegates to `emit_inline_intern`** rather than reading
a `usize` and pretending. The pacing predicate is tested first, so a collection
that was due still reaches `praxis_alloc_int`, and the schedule is bit-for-bit
what the `int_ref` inside `praxis_vec_len` would have produced.
`an_inline_vec_length_paces_before_it_answers_from_the_intern_table` is that
stated as a shape: both `Heap` words loaded and compared on the path that
answers inline, and two cold blocks rather than one, because folding the "not a
`Vec`" bail-out into the "length outside the intern table" bail-out would mean
calling `praxis_vec_len` for a length this path already has in a register.

**The diff does not touch `emit_inline_intern`'s body** — it is byte-identical
to its pre-W4b text — which matters beyond tidiness: handover 27 §9 registered
that if both W4b and W10 edit that function, the planned `W4b → W10` merge order
is the wrong direction. W4b calls it and does not edit it, so the question is
W10's alone.

### `praxis_deque_len` and `praxis_deque_get` get no arm, and cannot

Decision 5 refused to migrate `DequePayload` because a `VecDeque` is a ring
buffer: element *i* lives at `(head + i) % cap` and the storage wraps. That
refusal is now load-bearing rather than tidy — there is no two-load sequence to
emit, and an arm that read a `VecDeque`'s buffer as a slice would be wrong for
every deque that has ever popped from the front. This is why `vm`, whose stack
is a `Deque`, is untouched by part 2.

## Decision 9: the inline arm proves a descriptor before every payload read, and the proof is a dominance claim

Every payload read in the three arms is dominated by the proof of the object it
reads from. The member's `Int` payload is read only where the member's
descriptor has been proved; the words and the length are read only where the
receiver's has. This is REP-56 exactly — an eight-byte read off an object that
is not an `Int` is an out-of-bounds read of a zero-width `Unit` — and it is the
whole of the soundness argument for handing generated code a raw buffer pointer.

`a_membership_test_proves_a_descriptor_before_every_payload_read` states it as
**dominance over the emitted CFG**, not as "the load is in a later block". The
second is a claim about block numbering, which is the builder's business and can
change without the safety property changing; the first is the property. The test
sweeps every block that reads at each of the three displacements rather than
asserting there is one, because `+16` is *both* the `Int` payload offset and —
since `words` is at `BitSetPayload+0` — the words pointer.

`descriptor_address` is one function rather than the two lines it used to be
inside `emit_scalar_load`, for ADR-116's reason: part 2 adds three more proof
sites, and a second spelling of "where a descriptor address comes from" is
exactly what ADR-116 removed. The `adr116-arm-a` toggle is still those two lines
and still reverts every proof site in the tree.

`INLINE_VEC_SITE` and `INLINE_BITSET_SITE` are minted in the runtime beside the
payloads whose alignment and field offsets they name, with
`InlineSliceSite::new` `pub(crate)` — so **the set of payloads generated code
may walk is a list this crate wrote**, not a displacement the backend computed.
`GridPayload` is the reason that matters: it is also a leading word followed by
a growable vector, at a different offset and behind a different descriptor, and
a `Grid` walked as a `Vec` would read its width as an element pointer.

### The interaction with W8-S0, measured

Decision 6's re-box is a `Materialize{Bool}` whose only reader is the
`ExtractScalar{Bool}` that `lower_if` emits immediately, in the same block —
which is exactly the shape ADR-120's block-local forwarding deletes. W4b was
written before W8-S0 was in the tree and predicted it; the merge tests it.

**It happens.** On the merged tree the block holding `b.contains(k)` in a
`while`/`if` loop has a whole census of `{BitsetContains: 1}` — no
`Materialize`, no `ExtractScalar`, no `Call`.
`a_bitset_membership_test_is_its_own_instruction_and_never_a_call` asserts the
two zeros rather than deleting the assertions, because the zero is the claim: a
change that reintroduces the round trip must fail rather than pass quietly.

This is the reason decision 6's five-instruction regression at the site was
worth taking, and it is the second time in the round that a package's cost was
paid by a package it was measured against rather than by itself.

## Decision 10: an inline arm in the backend buys the descriptor analysis nothing, and this was expected to be otherwise

W4b was filed as the counterweight to W8-S0 in ADR-122's census: a package that
moves descriptors *out* of `Inst::Call`, where the provable-descriptor analysis
is blind, and into MIR's own emissions, where it is not. The census measures it
and the answer is **one site**:

| | before W4b | with W4b |
|---|---:|---:|
| suite, whole module | 219 sites, 30 literal (13.7%), 140 chased (63.9%) | **218**, 30 (13.8%), 140 (64.2%) |
| `bfs` alone | 63 sites, 4 literal, 49 chased | **62**, 4, 49 |
| three inner loops | 29 sites, 2 literal (6.9%), 27 chased (93.1%) | **unchanged, all four numbers** |

One site, in `bfs`, with both columns unchanged. The reason is the whole of the
finding: **only `bs.contains(x)` becomes a MIR instruction.** `praxis_vec_get`
and `praxis_vec_len` inline in the *backend* and keep their `Inst::Call` in MIR
— the wrapper is the cold arm, not the absent arm — so a MIR-level census cannot
see them, and the descriptor of the `GcRef` they answer is exactly as unprovable
as it was.

The general statement is the useful one, and it is now in ADR-122 as an
amendment: **inlining a call in the backend does not make a descriptor
provable.** What does is giving the primitive its own `Inst`, which costs a
variant, a verifier arm, a liveness arm and a backend arm apiece. Anyone hoping
to raise the chased column by inlining more wrappers should price that, not the
inlining.

The inner loops do not move at all because `collatz`, `primes` and `mandelbrot`
are arithmetic loops that touch no `BitSet` and no `Vec`. That is a null result
with a reason, which is worth more than a null result without one.

## What the counts say, and why a count is a cost here rather than the headline

**This is where part 2 differs from W6 and W7, and it is the most important
paragraph in the record.** Those packages removed instructions from a loop, so
an instruction count was their result. This one removes a **call**, and the
callee — `abi_guard!`'s `catch_unwind` region, the payload reads, the box, the
`BitIndex` construction — was never in the count. A count of the caller cannot
price an inlining. Every number below is *up*, and every one of them is a cost
this package is paying rather than a result it is claiming.

Three one-primitive loops, `PRAXIS_DUMP_CLIF` / `PRAXIS_DUMP_VCODE`, arm A
(`adr118-arm-a`, the wrapper call) against arm B (this tree):

| loop, per iteration | | arm A | arm B | Δ | calls A → B |
|---|---|---:|---:|---:|---|
| `if b.contains(k) { … }`, member present | vcode | 112 | 128 | **+16** | 1 → **0** |
| …member absent | vcode | 74 | 90 | **+16** | 1 → **0** |
| `acc = acc + v[0]` | vcode | 131 | 139 | **+8** | 1 → **0** |
| `acc = acc + v.len()` | vcode | 122 | 133 | **+11** | 1 → **0** |
| `if b.contains(k) { … }`, member present | CLIF | 104 | 128 | +24 | 1 → 0 |
| …member absent | CLIF | 65 | 89 | +24 | 1 → 0 |
| `acc = acc + v[0]` | CLIF | 115 | 132 | +17 | 1 → 0 |
| `acc = acc + v.len()` | CLIF | 107 | 128 | +21 | 1 → 0 |

Whole function, machine code:

| | arm A | arm B |
|---|---|---|
| `contains` loop | 343 instructions in 40 blocks, 1500 bytes | 369 in 48, 1592 bytes |
| `v[0]` loop | 364 in 42, 1592 bytes | 379 in 50, 1636 bytes |
| `v.len()` loop | 351 in 40, 1544 bytes | 374 in 49, 1632 bytes |

**The one row that is a result and not a cost is the last column**, and it is
the same on all three primitives and in both IRs: one call per iteration before,
zero after. That is the package. Everything to its left is the price of the
proof, the bounds test and the cold-block bookkeeping, and it is charged against
a `bl` whose body does not appear in the table.

**So this is the package whose result must come from a clock**, and none is
claimed here. Handover 26 §6 discards build-phase timings on a shared machine
and this tree was measured with other agents compiling beside it. The
measurement arms are staged; the numbers are the measurement phase's.

### Two corrections to how these numbers were produced

**The counts in this package's own commit message are not reproducible and
should not be quoted.** `3804eeb` reports 226 → 236, 157 → 173 and 173 → 170 per
iteration. Re-measured here, arm A against arm B on loops written to the same
description, all three grow — there is no primitive whose per-iteration count
falls, so the third figure has the wrong sign as well as the wrong magnitude.
The loops that produced the original figures were not recorded, so the two are
not strictly comparable; what is not in doubt is that **no arm of this package
makes a loop body smaller**, and a record that implied otherwise would be
claiming the one thing the design says is impossible.

**The round's shared per-iteration helper has a defect in its vcode column.** It
walks the CFG parsed out of the *CLIF* dump while summing instruction counts
taken from the *vcode* header. Those two numberings coincide only when the
backend adds no blocks, and it always adds some: for these loops the CLIF has 35
to 42 blocks against the vcode's 40 to 50, and part 2's inline arms widen the
gap. On the handover-25 baseline loop on this tree the two methods answer 130
and 115 for the same quantity. The figures above are from a walk built
separately for each IR out of its own dump, with coldness taken from the CLIF's
`cold:` marker for CLIF and from emission order for vcode. This is registered as
a follow-up rather than fixed here: the helper lives outside the repository, and
correcting it would silently restate numbers other records own.

**dump.rs's per-iteration rule has an exception now, and part 2 is what
introduced it.** The rule says "at each branch, take the successor that is
inside the component and is not cold", which assumes exactly one such successor.
`bs.contains(x)`'s `absent` arm is hot and in the loop beside the `read` arm
(decision 7), so the arm-B `contains` loop has **four** hot cycles rather than
one — two membership outcomes times two `if` arms. The table above names which
path each row is, rather than picking one and calling it "the" iteration.

## What the sanitizer says, and why green is necessary and not sufficient

`./scripts/asan.sh` on this tree:

| | part 1's run | this tree |
|---|---:|---:|
| passed | 1967 | **2061** |
| failed | 0 | **0** |
| `AddressSanitizer` reports | 0 | **0** |
| executables, all verified instrumented | 30 | **32** |

The count rose because five packages have landed since, not because anything was
skipped: the release-profile suite is 2061 against `cargo test --workspace`'s
2063, the same two-test gap part 1 reported (1967 against 1969) for the same
reason — a `debug_assertions`-gated pair.

**That run does not cover this package's new reads, and the argument has to be
written down rather than run.** Handover 26 §7 trap 6: ASan does not instrument
JIT-generated code. Cranelift emits machine code into a mapped region; there is
no compilation unit for rustc to instrument and no `-Z` flag that reaches it. So
the sanitizer sees `ReprCVec`, `BitSetPayload::insert`, the descriptor
callbacks and every runtime path the suite drives — and is blind to the three
loads part 2 emits, which are precisely the ones that read a raw buffer pointer.
Part 1 could say its `unsafe` was genuinely covered; part 2 cannot, and says so.

What stands in for the sanitizer is the structure, in four parts:

1. **Every payload read is dominated by a descriptor proof** (decision 9),
   asserted as dominance over the emitted CFG rather than as block order. A
   receiver whose descriptor is not `Vec`/`BitSet`, or an index that is not an
   `Int`, reaches the wrapper and never a load.
2. **Every displacement is minted in the crate that owns the layout**, from
   `offset_of!` against the real type, and is `pub(crate)` to construct — so the
   backend names a site rather than computing an offset, and a third payload
   cannot be walked by accident.
3. **The bounds test is total.** For `v[i]` it is one unsigned compare that
   rejects negatives with the same branch; for `bs.contains(x)` it is the word
   probe, whose sufficiency is an identity checked against `contains` over the
   whole `i64` range rather than argued (decision 7).
4. **The layout claims are `const _` assertions, not prose.** If `std::Vec`
   grows a word, or `BitSetPayload` gains a field, the build fails at the
   assertion rather than at an emitted load.

The honest summary is that the green run rules out a regression in the runtime
half and says nothing about the emitted half, and that the emitted half is
defended by construction and by unit tests over the emitted CFG.

## The measurement, and the number that must not be promised

**Arm B is this tree. Arm A is this tree with `adr118-arm-a` on**, a cargo
feature on `praxis-codegen-cranelift` whose whole body is the
`INLINE_COLLECTION_PRIMITIVES` pair in `lower.rs`; the two emitters read it and
nothing else in the crate does. That is handover 26 §6's shape — this tree with
this package's single toggle point reverted, never the previous commit, which
would fold in W6, W7, W8-S0 and W11 and report them as W4b's.

```
arm B (candidate) cargo build --release -p praxis-cli
arm A (baseline)  cargo build --release -p praxis-cli \
                      --features praxis-codegen-cranelift/adr118-arm-a
```

**The toggle deliberately does not revert the MIR shape change.** In both arms
`bs.contains(x)` is `Inst::BitsetContains` with a `Scalar(Bool)` dst and is not
a safepoint, because that decision is MIR's and a backend feature cannot express
it. What the clock will see is the inlining; decision 6's evidence is the
instruction census and the block-level census above. A reader comparing the two
arms is comparing "call the wrapper" against "inline it", not "before W4b"
against "after".

**Do not promise 12.6%.** Handover 26 §4 says it in those words, and it is
right. The 12.6% of `bfs` attributed to `praxis_bitset_contains` in handover 25
§2 is a profile bucket that includes the root spills and the fault check
*around* the call, not just the callee. Decision 6 removes some of those, which
moves the arithmetic in this package's favour rather than against it — but a
profile bucket is not a speedup, the eight benchmarks are not one benchmark, and
`bfs` is the only one of the eight where `bs.contains` is hot. The number is the
measurement phase's to supply and this record does not anticipate it.

The staged arms, and the check that the toggle bites:

```
/tmp/praxis-arms/W4b-a  a75e8d3ba10f074b9ec01fa6cd3401229f0d90745aa640987e7595012bc41be6
/tmp/praxis-arms/W4b-b  3ad32cd6e2b04bfc43f46aba87cabf80f1014bad9115566a0dc394f5725026f0
```

Equal hashes would mean the feature compiled to the same program and the
measurement was of nothing. They are not equal. The unit tests that pin the
inline shapes are `#[cfg(not(feature = "adr118-arm-a"))]` for the same reason
ADR-116's are — under the feature the emitted code contains the call they assert
is absent — so the arms differ in what the suite *checks* as well as in what it
emits.

All eight benchmarks produce byte-identical stdout under **all three** of
`main`'s binary, arm A and arm B, at the frozen sizes, checked untimed:
`bfs`, `collatz`, `hashwork`, `mandelbrot`, `pipeline`, `primes`, `tree`, `vm`.
That is a correctness gate, not a measurement — and it is the gate that matters
most here, because the three inline arms reimplement three wrappers' semantics
in a second place, and `vm` and `bfs` between them drive `v[i]`, `v.len()` and
`bs.contains(x)` millions of times per run.

### Part 1's arm is retired, and in the loud direction

Part 1's toggle was `praxis-runtime/std-vec-payload`, which replaced the pinned
triple with a `#[repr(transparent)]` newtype over `std::Vec`. **It no longer
builds against the backend, on purpose.** With loads emitted at those
displacements, that arm is not a slower build of the same program — it is a
miscompile, reading a capacity where a length is wanted. So
`REPR_C_VEC_ELEMENTS_OFFSET` and the two `InlineSliceSite`s built on it —
`INLINE_VEC_SITE`, `INLINE_BITSET_SITE` — do not exist under the feature, and
`praxis-codegen-cranelift` names the two sites unconditionally, which turns the
combination into a compile-time refusal:

```
$ cargo check -p praxis-cli --features praxis-runtime/std-vec-payload
error[E0425]: cannot find value `INLINE_BITSET_SITE` in module `praxis_runtime::bitset`
error[E0425]: cannot find value `INLINE_VEC_SITE` in module `praxis_runtime::collections`
```

`cargo test -p praxis-runtime --features std-vec-payload` still passes (403
tests): the crate on its own is unaffected, and part 1's measurement — a null
result, recorded above — was taken before part 2 landed. What is gone is the
*whole workspace* built that way, and it should stay gone. A feature that
produces a wrong program is worse than a feature that has been deleted, and the
compile error is the deletion made visible at the one place someone would try
it.

## Part 1's open questions, answered

- **"Does `praxis_vec_get`'s inline arm want the length load at all?"** It emits
  it, and it must: the bounds test is the fault check, and without it the arm
  would read past the end for an out-of-range index. What has changed since the
  question was asked is the second half of it — the arm *does* keep its root
  spills, because MIR still emits `Inst::Call` for `praxis_vec_get` and
  `is_gc_safepoint` matches every call (decision 10). Whether the load is free
  next to the spill is not separable in a static count and is a question for the
  clock.
- **"Does the `element_descriptor` at offset 0 buy anything for W4b?"** **No.**
  `v[i]`'s inline arm answers a `GcRef` and proves nothing about it; the
  element's descriptor is proved later, by whichever `emit_scalar_load` consumes
  it, exactly as it was when the wrapper answered. Reading
  `element_descriptor` would let the arm prove the element type once per *read*
  instead of once per *use*, which is only a win where a read has several uses —
  and that is a different optimization, over a different scope, that would need
  the proof's result to be a value the consumer can see. Noted as answered, not
  as deferred.
- **"Should `ReprCVec` grow a `push_within_capacity`?"** Not answered here,
  because part 2 emits no mutation. It remains W10's or a later package's, with
  the ordering argument (length written last) unchanged.

## Consequences (part 2)

- **Generated code now depends on two payload layouts**, which is ABI v20's
  W4b paragraph. `VecPayload`'s element pointer and length and `BitSetPayload`'s
  words pointer and word count are baked into emitted loads. Repacking either
  payload, or reordering `ReprCVec`'s three words, is a generated-code change
  from here — part 1 explicitly did *not* bump for the layout because nothing
  read it, and that sentence is now false.
- **`praxis_bitset_contains` changed its signature**, `GcRef` to `i64`, which is
  the strongest break in the ABI changelog: a v19 caller against a v20 runtime
  dereferences a `0`/`1` as a pointer. It is also the least subtle, which is the
  only comfortable thing about it.
- **MIR has one more instruction**, and ADR-044's five exhaustive matches each
  grew an arm. The merge with ADR-122 removed one of the five —
  `liveness::defs` is now `verify::defines` with its answer wrapped — so the new
  variant had to be added in four places rather than five, and the two that
  would have had to agree cannot disagree.
- **The catalog has a third `MethodLowering` variant** with one row in it today.
  That is a real cost and the bar for a fourth should be the same as the bar for
  this one was: an existing invariant that is right and that the new row
  genuinely falsifies.
- **The hot path of three collection reads calls nothing**, which is what the
  package is for, and **every one of them is bigger than it was**, which is what
  it costs. Both are measured above.
- **`emit_inline_intern` is called and not edited**, so handover 27 §9's
  sequencing question resolves in favour of the planned `W4b → W10` order.

## Open questions (part 2)

- **Should `praxis_vec_get` and `praxis_vec_len` get their own `Inst` too?**
  Decision 10 says that is what would make their results provable and what would
  drop their root spills. The obstacle is not effort: `v[i]` answers a `GcRef`,
  so it needs a `Gc`-dst instruction rather than a scalar one, and a `Gc` dst is
  a rooting question rather than a shape question — `is_gc_safepoint` excludes
  `ValueCmp` because nothing it produces needs rooting. That argument does not
  transfer, and the package that wants it has to make a new one.
- **Is `absent`-inline right for a `BitSet` that is mostly full?** The arm was
  chosen for `bfs`, where the visited set is short early. A workload whose
  queries are almost always in range pays one extra branch for the case that
  never happens. Both arms are one branch; there is no measurement that
  distinguishes them and no reason to expect one.
- **Does `Grid[T]` want the same treatment?** Part 1 registered it as a
  follow-up; part 2 adds the reason it is not free. `INLINE_VEC_SITE`'s
  `pub(crate)` constructor exists specifically so that a `Grid` cannot be walked
  as a `Vec`, so a `Grid` arm means a third site and a third descriptor proof,
  not a reuse.
- **How much of the emitted half can be sanitized at all?** Nothing in this
  round has an answer. A `debug_assertions`-gated bounds check emitted *into*
  generated code would be one shape of answer, and it would need a decision about
  what a violated check does at a point with no fault protocol.
