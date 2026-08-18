# The object heap and the collector

Every value in a running Praxis program is a heap object behind a pointer. An
`Int` is an object. A `Bool` is an object. A `Vec[Int]` is an object holding
pointers to objects. There is one reference type, `GcRef`, and generated code
treats it as opaque.

The collector behind that is precise, non-moving, single-threaded mark-and-sweep
over size-class pages, with the roots supplied by the compiler. None of which you
can observe from a Praxis program, and the point of this chapter is largely to
say exactly what "none of which" covers.

## What it means for a program

Three things, and then a fourth that is not quite nothing.

**A call copies the reference, never the object.** Passing a collection to a
function does not copy it, and mutating it through the parameter is visible to
the caller. Rebinding the parameter is not — that changes one slot in one frame.

```praxis
// A call copies the reference, never the object. `xs` and `v` name
// one Vec, so a push through either is visible through both.
fn push_two(xs: Vec[Int]) {
    xs.push(1)
    xs.push(2)
}

// Rebinding the parameter changes only this function's slot.
fn rebind(xs: Vec[Int]) {
    xs = [9, 9, 9]
    xs.push(9)
}

var v = Vec[Int]()
var alias = v
push_two(v)
out(v)
out(alias)

rebind(v)
out(v)
```

```text
[1, 2]
[1, 2]
[1, 2]
```

**There is nothing to run when an object dies.** No finalizers, no destructors,
no `close`, no scope-exit hook. The collector calls internal drop functions
during sweep so that a `Vec`'s Rust backing allocation is released, and that is
invisible: nothing you wrote runs, and nothing you can print changes.

**There is no way to ask about identity.** The language has no `is`, no `===`,
no `ref_eq`. `==` on scalars compares payloads and `==` on composites compares
structurally, so two separately built vectors with the same elements are equal.
That is what makes it safe for the runtime to hand out one shared object for
every `true`, every `Unit`, every code point below 128 and every integer from
−256 to 1024: an optimization that would be observable in a language with
reference equality is unobservable here.

The fourth thing is memory. The collector cannot change what a program prints,
but it decides how much memory the program holds while printing it — and its
schedule is tunable, which is a convenient way to show that the schedule is not
part of the program's meaning:

```praxis
// One live vector, and a great deal of garbage made around it.
var kept = Vec[Int]()
kept.push(7)

var total = 0
var i = 0
while i < 200000 {
    var scratch = [i, i + 1, i + 2]
    total = total + scratch.sum()
    i = i + 1
}

out(total)
out(kept)
```

```text
60000300000
[7]
```

`PRAXIS_GC_PACER` replaces the collector's schedule. `doubling` is the unbounded
rule the language used to have; `bounded:64K:1` collects as often as it possibly
can. On one machine that program peaks at 21 MiB, 12 MiB and 7 MiB of resident
set under `doubling`, the default, and `bounded:64K:1` — and prints the same two
lines in all three.

```console
$ PRAXIS_GC_PACER=doubling praxis run gc-is-invisible.px
60000300000
[7]
$ PRAXIS_GC_PACER=bounded:64K:1 praxis run gc-is-invisible.px
60000300000
[7]
```

That variable is the only environment variable the runtime reads, and it exists
for A/B measurement rather than for tuning your program.

## An object

An allocation is a `GcHeader` followed by its payload in the same block. The
header is 16 bytes and has three fields, each with a reader on a hot path:

| field | width | what it is for |
|---|---|---|
| `descriptor` | 8 | pointer to the `TypeDescriptor`; null means *swept* |
| `payload_offset` | 2 | where the payload starts, as the allocator laid it out |
| `heap_id` | 4 | which heap owns this block |

Two fields that used to be there are not. The mark colour moved into the page,
because a colour byte in the header costs a random-access store per surviving
object per collection. The payload size was deleted outright because nothing read
it, which took the header from 24 bytes to 16 and every block in the heap down by
eight. Adding a field back is not a local decision: it moves the size-class
ladder and the immediate that generated code folds to reach a payload, so it owes
an ABI version bump.

`payload_offset` is the single layout authority — written by the allocator from
the same calculation that produced the address it initialized, and read by
everything else. Nothing downstream re-derives it, so nothing downstream can
derive it differently. `heap_id` is allocation provenance, and it is the one
field two rounds of shrinking refused to spend: the mark phase reads it *before*
it dereferences anything the header points at, so a reference from another heap
or into swept storage is rejected rather than followed.

Everything payload-aware is centralized in the descriptor rather than scattered
across type switches: `trace`, `drop_value`, `format`, and optional `equals`,
`hash`, `compare` and `owned_bytes` callbacks. There are exactly 22 built-in
descriptors — the six scalars, `Text`, the nine collections, `Range`, and the
generic `Record`, `Tuple`, `Enum`, `Closure` and `VarCell` shapes. They are
`static`, because descriptor *pointer* identity is what the runtime compares.

`compare` is carried by exactly the eleven a `Map` key or `Set` member can be —
the scalars, `Text`, `Range`, `Record`, `Tuple` and `Enum` — and is `None` on the
nine collections, `Closure` and `VarCell`, none of which can ever be a key. That
is what makes the order a container walks its keys in total, and it is a
deliberately wider set than the source language's `<`.

An `Int` is therefore 24 bytes: 16 of header and 8 of payload.

## Pages

Every block lives on a **page**: one 32 KiB allocation, aligned to 32 KiB, whose
first bytes are a `PageHeader` and whose remainder is an array of equal-sized
blocks. Because the base is aligned to the page size, finding an address's page
is a mask and finding its block index is a multiply-shift. No side table, no hash
lookup.

The size-class ladder is 8-byte granular from 16 bytes (a bare header, which is
what a `Unit` is) to 128 bytes: 15 rungs. Deliberately not powers of two — the
ladder exists to make composites smaller, and a power-of-two ladder would round a
`Vec`'s block up to 64 and a `Map`'s to 128. A payload larger than 128 bytes, or
aligned more strictly than a header, gets a page to itself; no descriptor in the
language takes that path.

A page carries two bitmaps. `allocated` says which blocks hold an initialized
object, and `mark` says which the mark phase reached this cycle. `allocated` is
the free list as well as the liveness record: allocation claims the lowest clear
bit at or above a cursor. It is a bitmap rather than a list threaded through dead
blocks specifically because threading a next-pointer through a dead block would
overwrite its `descriptor`, and swept storage that claims to be a typed object is
the failure mode the whole design is arranged to avoid.

An emptied page goes back to the heap's own pool and is re-classed on demand, so
storage is reusable across layouts. A page is never returned to the operating
system while its heap lives — that is soundness, not policy, because the story
"a stale reference masks to a page that is still mapped and is rejected there"
requires the page to still be mapped.

## Mark and sweep

The collector is precise (it knows exactly which words are references, because
the compiler tells it), non-moving (an object's address is fixed for its
lifetime), single-threaded, and needs no write barrier.

**Mark** starts from the roots and drains a worklist. Per object: check
`heap_id` against this heap's; mask to the page; test-and-set the mark bit;
if it was clear, call the descriptor's `trace`, which pushes children onto the
worklist. The worklist *is* the grey set — a third colour would say nothing extra
in a collector with no concurrency, which is why the header never had a byte for
one.

**Sweep** walks the pages a word of bitmap at a time. `allocated & !mark` is the
dead set; each dead block gets its payload finalized, its header poisoned
(descriptor nulled, `heap_id` zeroed) and only then its bit cleared. A page in
which nothing died costs two tests and at most one store per 64 blocks, and
**sweep never touches a survivor**. That property is what makes measuring the live
set free: sweep also accumulates `live_count × block_size` per page, one multiply
per page and nothing per object.

Non-moving is the load-bearing choice. Stable addresses are why the Rust
collection wrappers can be simple, why spilled roots never need updating, and why
a crash snapshot can hold references safely.

## Roots

Roots come from six places, and the set is exhaustive by construction: the type
the collector accepts is built only from a live runtime context and destructures
all six arms, so "collect against a partial root set" does not compile.

Five are strong — the collector keeps them alive:

1. **The shadow stack**, which is where generated code puts its live locals.
2. **The process input buffer.**
3. **A failed parse's partial value**, so the crash debugger can show it.
4. **The crash snapshot**, once one has been taken.
5. **Native scopes** — the run of entries a runtime helper claims while it builds
   a value across an allocation. They live in one growable store whose *depth*
   is what is bounded rather than its size, and holding a payload reference
   across a safepoint without rooting its owner does not type-check.

The sixth is **weak**: the crash debugger's per-call value slots. The collector
never traces them — that would merge the two slot sets the compiler deliberately
keeps apart — but it scans them once per collection, immediately after the sweep,
and turns every entry naming reclaimed storage into an absence. So a debug value
is always a live object or nothing, never a dangling reference.

### The shadow stack

The compiler computes, per safepoint, the minimal set of live `Gc` locals, and
generated code stores exactly those into a frame before the safepointing call.

A frame is not an object. The runtime owns one contiguous region of slots for the
whole program; a function's frame is the run between the `top` it found on entry
and the `top` it left behind. The prologue loads `top`, zeroes exactly the slots
it claims, and stores the bumped `top`; the epilogue stores the saved base back.
No call, no allocation, no `catch_unwind`. The
collector scans `[base, top)` in one linear pass, skipping nulls, which yields
exactly what a walk of per-frame objects yielded and allocates nothing.

A slot is a **live range**, not a name. Locals that are never live at the same
safepoint share a slot, assigned by colouring the interference relation, so a
frame's width is its peak simultaneous liveness rather than its count of locals.
Over the AoC corpus that took the summed declared width from 1925 slots to 216.

Shadow-stack exhaustion is unrepresentable rather than handled, and the argument
runs through the recursion guard. Every prologue refuses before it pushes
anything if the remaining stack budget will not cover this frame's cost, where
the cost is a measured floor of 160 bytes plus 2 bytes per `Gc` local past the
eleventh — the floor being the high-water mark across both targets the backend
supports, so a program faults at the same depth on either. The budget is 8000
reference-width frames' worth. The shadow-stack reservation is sized from that
same arithmetic plus one frame of headroom, so there is no bounds check in the
prologue, because there is nothing left to check.

This is the one place the machinery becomes a fault you can hit:

```text
error: program faulted: stack overflow (recursion limit)
```

A wide frame reaches it sooner than a narrow one, because it costs more, which is
the whole reason the guard charges by shape rather than counting calls.

## Safepoints and pacing

A safepoint is a point where a collection may happen, and that is exactly a point
where something may allocate: an `Alloc`, a `Materialize`, or a call to a runtime
wrapper whose manifest row says it allocates. The compiler spills roots
immediately before each one.

On the runtime side, allocation is gated by a token. `Heap::alloc` takes a
`Safepoint`, and the only way to obtain one is `Heap::pace`, which is where the
collection test runs. Obtaining the token *is* the pacing, and the token is
neither `Copy` nor `Clone` — one token, one allocation. So "allocate on the paced
path without pacing" has no spelling.

The test itself is two words: has this heap allocated more bytes since the last
collection than its current threshold. After each collection the threshold is
recomputed as

```text
max( min(previous × 2, ceiling), live × 2, 64 KiB )
```

Three terms, each there for its own reason. The doubling ratchet keeps
allocations per collection amortized constant. The ceiling — 4 MiB — bounds
speculative growth, because being wrong about the future costs only a collection
that finds nothing. `live × 2` is not speculative: those bytes are provably
reachable now, so a program legitimately holding more than the ceiling must be
allowed to exceed it, or every collection would prove it can reclaim nothing and
the next allocation would trigger another. The ceiling clamps the doubling term
and never the whole expression, which is what keeps those two rules from
fighting.

The resident set a program holds is therefore `floor + live + that threshold`,
where the floor is what the process costs before the program does anything, JIT
included: `out(1)` costs 5.5 MiB on the machine the numbers above come from, and
that is a figure which moves with the host. The ceiling is a tuned constant of
the same kind. It is a bet on what a collection costs — hold more garbage and
you collect less often — so it moves whenever a collection gets cheaper.

Generated code does not call the pacing function. It transcribes it: two loads
and a compare, with the allocation's fast path on the not-due side and the
out-of-line wrapper on the other. Which means the collector runs on a branch that
generated code took, not on a call it made. What makes that sound is that
nothing between the pacing branch and the last store into the new object can
collect, so there is no window in which a half-initialized block is reachable.

## Where it does show through

Four places, all of them about memory rather than meaning.

**A `Text` slice keeps its owner alive.** A `Text` is either an owned UTF-8
payload or a zero-copy `(owner, start, length)` view into another `Text`. The
input parser produces slices into the immutable input buffer, so holding one word
out of a 10 MB input holds the 10 MB. Non-moving addresses are what make that
representation sound in the first place.

**Interning is bounded and permanent.** The interned integers, characters and
singletons live on pages flagged immortal: no bit of their `allocated` bitmap is
ever cleared and nothing on them is ever finalized. They are never collected and
never intended to be.

**Deep recursion faults.** See above; the program reports and the debugger opens,
rather than the process dying on a native stack overflow.

**Running out of memory is not modelled.** A size the host cannot serve — a
`BitSet` insert of 10^18, say — is a fault checked before anything is allocated,
and reads `program faulted: size or extent out of range`.
But a page the operating system refuses is not: the process aborts. Nothing in
the language observes heap exhaustion.

## Current choice versus permanent property

Permanent, in the sense that the language is defined around it:

- Every value is a reference to a heap object, with reference semantics on
  assignment and argument passing.
- There is no identity operator, and equality is structural.
- There are no user-visible finalizers.
- Object addresses are stable for an object's lifetime, and no interior pointer
  is ever exposed as a long-lived value.

A current implementation choice, and most of these have already changed once:

- Mark-and-sweep, non-moving, single-threaded, no write barrier. It was chosen
  for stable addresses and a small surface; a generational collector would take
  the stable addresses with it, and everything above that leans on them.
- Size-class pages of 32 KiB with a 15-rung ladder. This replaced a bump arena
  with a side registry, and the alternative of segregating by descriptor was
  weighed and rejected on provenance grounds rather than on memory: it deletes
  the header, and with no per-object word there is nothing left to read before
  masking an unvalidated reference to a page.
- Which values are interned, and the 4 MiB pacing ceiling.
- The 16-byte header. Two fields have left it, and the language reserves the
  right to intern, tag or eliminate small objects entirely, provided reference
  and aliasing semantics survive — and for `Int` there are none to survive, which
  is a fact about the language rather than an assumption about the program.
