# ADR-114: The native roots are one store, and it grows because only their depth is bounded

**Date:** 2026-08-03
**Status:** accepted — implemented
**Milestone:** post-M11 performance (handover 25 finding F-1, handover 26 package W1)
**Amends:** ADR-012's fifth arm. `NativeRootFrame` — a `Box`ed frame with a
`parent` pointer and its own `Vec` — is deleted and replaced by one
`NativeRootStore` the `Runtime` owns. ADR-012's `RootSet` seam, P0-07's `Rooted`
proof and its `&mut Payload` accessors, and ADR-044's two-slot-set split are all
preserved verbatim; `NativeScope`'s public signature does not change at all.
This is **ADR-101 applied to a fifth region**, and the interesting part is the
one place the copy does not go through: ADR-101 could make exhaustion
unrepresentable and this cannot.

## Context

`crates/praxis-runtime/src/roots.rs`, before:

```rust
let mut frame = Box::new(NativeRootFrame {   // malloc #1
    parent,
    roots: RefCell::new(Vec::new()),         // malloc #2, on the first root()
});
```

**Every runtime wrapper that roots a reference did two `malloc`s and two `free`s
to do it.** `NativeScope::new` has 60 call sites — 42 in `abi.rs`, 17 in the
parser interpreter, one in the debugger's `p EXPR` — and it is how a wrapper
keeps its arguments reachable across an allocation, so it sits on the path of
`praxis_vec_push`, `praxis_deque_push_back`, `praxis_deque_pop_front`,
`praxis_bitset_insert`, `praxis_set_insert` and `praxis_map_insert`: every
mutating collection primitive in the language. Handover 25 §4 profiled `bfs`
spending **~24% of its time inside `libsystem_malloc`**, with the chain named
leaf-first:

```
praxis_bitset_insert
  └ RawVec<GcRef>::grow_one → finish_grow → _xzm_xzone_malloc_tiny
praxis_deque_push_back
  └ RawVec<GcRef>::grow_one → finish_grow → _malloc_zone_malloc
```

The collector side matched. `<NativeRootFrame as RootSet>::push_roots` walked
the parent chain and did an `extend_from_slice` per link, once per collection —
and the parser interpreter nests those, so a collection taken inside a parse
paid for the walk at every level.

This is the same defect class as ADR-101's `Box<ShadowFrame>` and handover 21's
SipHash-keyed free list: **a data structure allocated per operation on the
hottest path.** ADR-101's Consequences already pointed at this arm — it built
`SlotStack<T>` generic "because the mechanism is not specific to GC roots" —
and the frames here nest with the Rust stack for exactly the reason a shadow
frame nests with the Praxis stack.

## Decision 1: one store, and a scope is a watermark

`Runtime` owns a `NativeRootStore` — a `RefCell<Vec<GcRef>>` and nothing else —
and `RuntimeContext.native_roots` points at it for the life of the runtime,
keeping its position and its width. A scope stops being an object:

```rust
pub struct NativeScope<'c> {
    store: *const NativeRootStore,
    watermark: usize,
    _ctx: PhantomData<&'c mut RuntimeContext>,
}
```

`new` reads `store.len()`. `root` is a bounds-checked store and an increment.
`Drop` is `truncate(watermark)`. `push_roots` is **one** `extend_from_slice` over
`[0, len)`, which yields exactly the set the chain walk yielded for ADR-101's
reason: scopes nest with the Rust stack, each occupies exactly the run between
its own watermark and the next one's, and the runs partition `[0, len)`.

No box, no per-scope `Vec`, no thread-local, and no linked list for the collector
to walk. The `RefCell` stays, because `root` takes `&self` so several `Rooted`
values can be live at once — the common shape, since a helper roots its result
and then roots each intermediate it builds. Nothing can collect inside the
`borrow_mut`: the only thing a push can call is the *system* allocator, on a
growth, and that is not a safepoint.

### The signature is preserved byte-for-byte, and that is what made wave 1 three wide

Handover 26 §1's most load-bearing correction was that this package need not
touch `abi.rs`, and it does not. **Zero of the 60 `NativeScope::new` sites and
zero of the ~70 `scope.root(…)` sites are edited.** `new` is still an `unsafe fn`
taking `*mut RuntimeContext` and returning by value; `root` still takes `&self`
and returns a `Rooted<'_>` borrowing the scope; `Rooted` is unchanged. What
changed is one field's pointee type and the three functions that read it.

The only edits outside `roots.rs`/`context.rs`/`lib.rs` are comments that had
become false — 15 identical blocks in `parser.rs` describing a chain that no
longer exists, and two in `abi.rs`. Leaving them would have been the cheaper
diff and the worse one.

## Decision 2: the store grows, so the watermark is a `usize` index — this is the whole package

ADR-101 can say "shadow-stack exhaustion is unrepresentable rather than handled"
because **both** of its factors are constants: depth is capped by ADR-105's byte
budget, width by the `SlotCount` type. Multiply, add a frame of headroom, assert
it in a `const` block, and there is no bounds check in the prologue because there
is nothing left to check.

**Here only the depth half holds**, and that asymmetry is the decision.

### Depth is bounded, and could have been reserved for

Every nested scope sits under a Praxis call or a parser-plan node, so ADR-105's
budget bounds it the same way it bounds generated frames. Measured, with a
counter on `NativeScope::new`/`Drop`:

| program | peak scope depth |
|---|---:|
| every benchmark in `benchmarks/praxis` | **1** |
| `read lines(int)` | 2 |
| ``read lines(`{a:int},{b:int}`)`` | 3 |

A fixed reservation would have covered this half comfortably.

### Width is the program's input, and no constant covers it

A single scope's root count is bounded by nothing at all. Two shapes reach it,
and **the one handover 25 and handover 26 both named is the rarer of the two**:

| program | peak roots held at once |
|---|---:|
| every benchmark in `benchmarks/praxis` | **1** |
| `read lines(int)`, 200,000 lines | **200,001** |
| ``read lines(`{a:int},{b:int}`)``, 200,000 lines | **200,002** |
| `bfs(start, nbrs)` builtin, 40,000 nodes | **119,997** |

Handovers 25 and 26 attribute the unbounded case to the graph builtins and
`ClosureOracle::retain` — true, and not the common case. The common case is
`parser::walk_lines` (`crates/praxis-runtime/src/parser.rs:835`), which opens
**one** scope at line 846 for a whole `lines(…)` walk and calls `scope.root` once
per line at 858. Every program that reads line-shaped input goes through it, and
its root count *is* the input's line count. The correction matters because it
moves the unbounded population from "an exotic builtin" to "the ordinary shape
this language exists for".

So the two populations are **1** and **the input**, with nothing observed in
between.

### What a hard limit would have cost

An `assert!(len < CAP)` — the mechanism that makes ADR-101's reservation
airtight — turns a 200,000-line input into a **process abort**, in the parser,
before the program's first statement runs. That is precisely the failure ADR-105
exists to prevent ("a size the host cannot serve is a fault, not an abort" is
ADR-041's title, and ADR-105's guard is the stack version of it). Covering the
real workload instead means reserving for an input nobody has bounded, which is
not a reservation, it is a refusal to decide.

### And therefore the watermark must be an index

The store reallocs. A `*mut GcRef` watermark is the natural shape — it is what
`SlotStackHeader` uses, and copying ADR-101 faithfully would produce it — and it
**passes every small test and dies only on a growth**: an outer scope saves an
address, an inner scope's `root()` moves the array, and the outer `Drop`
publishes a pointer into freed storage as the store's new end. The next
collection reads it. The failure needs a scope that outgrows the reservation
*while another is live*, which no unit test writes by accident.

Two tests exist for exactly that. `a_scope_survives_the_growth_its_own_roots_force`
roots past the reservation in one scope, asserts the capacity actually moved (so
it cannot silently degrade into a test that fits), and compares the whole run
element-for-element afterwards.
`an_inner_scopes_growth_leaves_the_outer_scopes_watermark_valid` forces the
growth from the *inner* scope, which is the shape that kills a pointer, and
checks the outer scope's roots are intact and the inner's are gone.
`a_parse_that_outgrows_the_native_root_reservation_still_answers`
(`crates/praxis-cli/tests/run.rs`) reaches the same state the way a program does.

### `Rooted` survives the growth because it holds a value, not an address

A `Rooted<'s>` carries the `GcRef` by value plus a `PhantomData<&'s ()>`. The
tempting shape is a `*mut GcRef` into the store — it would let `Drop` clear its
own entry — and it is unsound here for the same reason the pointer watermark is,
except worse: the read would land in freed-then-reused storage and answer *a*
`GcRef` rather than crashing.

What the store owes a `Rooted` is not an address but a promise — *this reference
is somewhere in `[0, len)` for as long as the scope lives* — and moving the array
does not break that promise. The collector re-reads the slice at every
collection; nothing outside the store ever holds the array's address. That is
also the reason this region can be a `Vec` at all where ADR-101's had to be a
`Box<[T]>`: generated code holds a shadow frame's base pointer in a Cranelift
`Variable` for the duration of a call, and nothing holds anything here.
`a_rooted_handed_out_before_a_growth_still_names_its_object` pins it.

### The reservation, and why nothing turns on it

`NATIVE_ROOT_RESERVATION = 1024` — 8 KiB, one `malloc` per `Runtime`. It is
three orders of magnitude above every bounded program measured, and it is about
what a *single* old-style scope's two allocations cost, of which the suite made
tens of millions.

It is deliberately not sized to demand, because there is no demand curve to size
to: it buys the bounded population "one allocation per runtime, ever", and for
the unbounded population no starting capacity helps, because `Vec`'s doubling
puts the total bytes copied within a factor of two of the final size whatever the
start. Even `Vec::new()` would cost the bounded population exactly one
allocation, at the first `root()` of the program's life. **Raising this number is
a memory decision, not a speed one**, and saying so is better than pretending a
figure was derived.

## Decision 3: release restores an absolute, and cannot raise the watermark

`Drop` calls `truncate(self.watermark)` — an absolute, not a subtraction, for
ADR-101's "absolutes, not increments" reason: it cannot underflow, and an
imbalance introduced below this scope is corrected here rather than propagated
upward.

`truncate`'s own no-op-when-already-shorter rule buys something the chain did not
have. Under the parent-pointer design, releasing scopes out of order wrote a
stale head pointer into the context — a dangling `*mut NativeRootFrame` the next
collection would walk. Under a watermark, the *only* thing an out-of-order
release can do is leave the store where it is. **A pop that could raise the
length has no spelling**, and a raise is the dangerous direction: it would
republish entries a live scope had already released, handing the collector
references to storage a sweep may have reclaimed and RT-01 may have reissued as
an object of another type.

Scopes still nest with the Rust stack, and nothing in the runtime breaks that.
The point is that the consequence of breaking it fell from *memory unsafety* to
*a root released early*. `a_scope_dropped_out_of_order_cannot_raise_the_watermark`
is the assertion.

## Decision 4: one store per `Runtime`, not one chain per context

`Runtime::context()` used to wire `native_roots: null` on every fresh context,
with the comment "no native frame is on the Rust stack when the context is
minted". It now wires the runtime's one store, so every context this runtime
mints sees every scope that is open.

That is a fix, not a side effect, and it is ADR-101's `collect_now` consequence
arriving one arm later: `Runtime::collect_now` mints a fresh context, so under
the old wiring a host-driven collection **could not see the native roots at
all**. The debugger's `p EXPR` (`evaluate.rs`) mints one too, and roots the crash
snapshot's values into a scope on it — that path happened to work only because
nothing else was open at the time. `every_context_this_runtime_mints_sees_the_same_store`
pins the new behaviour.

`clear_for_rerun` gains the fourth `debug_assert` — the store must be empty
between runs, which for this region is a sharper statement than for the other
three, because a `NativeScope` is RAII on the *Rust* stack and there is no frame
left that could hold a claim. It resets the length and deliberately **not** the
capacity: a `restart` re-parses the same input.

## `RUNTIME_ABI_VERSION` stays at 19 and gains a paragraph

`native_roots` keeps its position and its width and points at something entirely
different. That is the shape v15 (`roots` → `shadow`) and v18 (`debug_top` →
`debug_frames`) each bumped for, and this one does not, because **which side
reads it is the whole difference**: those two fields are loaded and
bump-allocated by every generated prologue, and this one has no reader outside
`praxis-runtime` at all. Its writers are `Runtime::context` and `NativeScope`;
its only reader is `RuntimeRoots::from_context`; and none of the eleven
`offset_of!`s in `crates/praxis-codegen-cranelift/src/lower.rs` names it —
checked, not assumed. The struct's size is unchanged too, so the v9 rule that put
`native_roots` in the version history in the first place does not fire either.

v19 is the open window ADR-105, ADR-107, ADR-109, ADR-111 and ADR-113 are already
in. This is one more paragraph in it, and the paragraph exists so that the
*absence* of a bump is on the record rather than inferred from silence.

## Measurements

**This record was produced in a build phase and deliberately contains no
timing.** Handover 26 §6 is explicit that a number produced while two other
agents are compiling measures their build, and that such numbers are discarded.
What is here instead is the deterministic evidence, which does not drift, plus
the two staged binaries the measurement phase compares.

### The allocator traffic, which is the claim

`crates/praxis-runtime/tests/native_root_allocator_traffic.rs` installs a
counting `#[global_allocator]`, arms it, opens 10,000 scopes rooting two
references each, and disarms:

| | allocations | frees |
|---|---:|---:|
| ADR-012's boxed frame | 20,000 | 20,000 |
| this record | **0** | **0** |

Exact, machine-independent, and it fails on the line that would put a per-scope
allocation back rather than showing up as two percent on a benchmark somebody
re-runs next quarter. The harness was verified to count — a `Box::new` added to
the loop reports 10,000 and 10,000 — because a counter that never counts is
worth less than no test.

### What the prototype said, and why it understates this

Handover 25 §4 measured a prototype of this change (a small-vector plus a
thread-local box pool) in a palindromic A/B, best of 5, every checksum verified
identical: **`vm` 2.70×, `bfs` 1.50×, `hashwork` 1.24×, geometric mean 1.22×**,
whole suite passing. Profiling `vm` *with* the prototype applied, 20% of the
remaining runtime was the pool itself — `NativeScope::new` 9.2%, its `Drop`
8.9%, `_tlv_get_addr` 2.3%. **This has no pool**, so the prototype's number is a
floor rather than a ceiling. It is quoted here as provenance for why the package
was ranked first, not as this change's result.

### The arms

Arm A is **not** the previous commit and **not** `main` — ADR-113 records
measuring against the wrong baseline reporting 14.4% where the right one gave
0.8%. Arm A is this branch with this package's mechanism reverted and nothing
else, which for W1 is exactly three files:

```
git checkout 5cbede3 -- crates/praxis-runtime/src/roots.rs \
                        crates/praxis-runtime/src/context.rs \
                        crates/praxis-runtime/src/lib.rs
cargo build --release
```

Those three files are the whole mechanism; every other file this branch touches
holds comments, tests, an ADR, or `scripts/asan.sh`, none of which reach the
binary. A cargo feature carrying both implementations was considered and
rejected: it would mean a `#[cfg]` on a field of `#[repr(C)] RuntimeContext`,
which is a worse thing to ship permanently than a three-file revert is to
document.

## What was deliberately *not* done

**The store is not a `SlotStack<GcRef>`.** Reusing ADR-101's type would have been
the tidier diff and it is the wrong shape twice: `SlotStack` never resizes, which
is the one property this region cannot have, and its slot type must have a
meaningful zero (`null` for the shadow stack, `None` for the debug values)
because generated code zeroes a claimed run. Nothing zeroes here — a claim is a
`usize` read — and `GcRef` is `NonNull`, so it has no zero to be. The two regions
share an argument, not a type.

**`root` is not branch-free.** It tests `store.is_null()`, because
`NativeScope::new`'s contract has always accepted a null or placeholder context
and the defensive paths in `abi.rs` rely on getting a `Rooted` back anyway. One
predicted-not-taken compare replaces two `malloc`s; making it unrepresentable
would need a `'static` empty store, which `RefCell` is not `Sync` enough to be.

**The capacity is never shrunk.** A store that grew to hold one root per line of
a 200,000-line input retains 2 MiB for the life of the `Runtime`. Shrinking on
release would put a `free` back on the path of `praxis_vec_push`, which returns
the store to empty on every call — the exact traffic this record removes. See
Consequences.

**`RootScope` is untouched.** It is ADR-012's host-facing scope, it holds its own
`Vec`, and it is not on any hot path; folding it into the store would couple the
host's rooting to a runtime the host may not have.

## Consequences

- **The native root store is the one region of the four that can move**, and the
  reason is that nothing outside it holds its array's address. That is a
  property to check before adding a reader, not a licence — a future
  `emit_native_root_claim` in the backend would make it false, and the
  reservation argument does not close for a growable region.
- **A program's peak native root count is now a retained memory cost.** The
  capacity is a high-water mark for the life of the `Runtime`: 8 bytes per root
  at the peak, so 1.6 MB for a 200,000-line parse. It is bounded by the same
  input that bounds the parsed values themselves — which occupy at least four
  times that in the heap — and it was previously freed with each frame. Named
  here rather than glossed.
- **`Runtime::collect_now` improves**, exactly as it did in ADR-101. A
  host-driven collection now sees the native scopes that are open; before, it
  minted a context with a null chain head and saw none of them.
- **`Runtime` gained a fourth thing that must not move after `context()` is
  called.** `native_roots` joins `heap`, `pending_fault`, `fault_message`,
  `parse_detail` and `crash_snapshot` as a pointer *into* the `Runtime` rather
  than into a separately boxed header the way `shadow` and the two debug stacks
  are. The distinction is deliberate — only generated code needs an address that
  survives a move — and it is the kind of thing a test fixture discovers as a
  `SIGBUS`, which is how the one in `roots.rs` came to box its runtime.
- **`NativeScope::root_count` answers a different number.** It was "this frame
  only"; it is now "this scope and everything nested inside it". Those agree for
  every caller that asks before opening an inner scope, which is every caller
  there is — the method has no non-test users in the tree — but the change is
  real and is documented at the method.
- **`NativeRootFrame` is gone from `praxis-runtime`'s public surface**, replaced
  by `NativeRootStore` and `NATIVE_ROOT_RESERVATION`. Both matches are
  compiler-checked; nothing outside the crate named the old type.
- **`scripts/asan.sh` could not pass, and this record found it.** Its
  instrumentation check was `! nm "$exe" | grep -q '__asan_'` under
  `set -o pipefail`: `grep -q` exits at the first match, `nm` takes SIGPIPE, the
  pipeline reports 141, and `!` turns that into "not instrumented" — on every
  binary, always, including one carrying 25,435 `__asan_*` symbols. `grep -c`
  reads to the end and does not signal. The 1911/0 baseline in the script's own
  header was carried from handover 25 §1 and had never been reproduced by the
  script. Two lines, fixed here because this package's stated verification is a
  clean sanitizer run.

## Open questions

- **Should the two graph-shaped scopes release earlier?** `praxis_bfs` opens one
  scope for an entire search and never releases a root until the search ends, so
  the store's peak is the whole visited set even though the walk's own visited
  set already keeps those references alive on the Rust side. Rooting per *step*
  and releasing per step would make the peak the frontier rather than the
  closure. It is a change to `graph.rs`'s oracle protocol, not to this record's
  mechanism, and it wants the person who owns ADR-060.
- **Is the `RefCell` still earning its keep?** Its dynamic check is what lets
  `root` take `&self`, and the argument that a collection cannot occur inside a
  `borrow_mut` is written above rather than enforced. A `Cell<usize>` length
  beside an `UnsafeCell<[GcRef]>` would be branch-free and would move that
  argument into an `unsafe` block, which is a worse place for it. Left as is;
  re-open only with a measurement that says the borrow flag is visible.
- **What does the fifth arm cost the collector now?** `push_roots` copies the
  whole store into the collector's scratch `Vec` on every collection, and for a
  parse holding 200,000 roots that is a 1.6 MB memcpy per collection. The chain
  walk copied the same bytes in more pieces, so this is not a regression — but it
  is the first time the number is large enough to ask whether `RootSet` should
  hand out a slice rather than push into an out-parameter. That is an ADR-012
  seam change and belongs with whoever next touches `Heap::mark`.
