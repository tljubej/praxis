# ADR-146: A collection constructor's arity is its shape

**Date:** 2026-08-06
**Status:** accepted
**Milestone:** 12
**Amends:** [ADR-089](./089-a-name-has-one-signature.md) decision 1 (a narrow
carve-out for the collection constructors, stated below);
[ADR-041](./041-bounded-extents-fault-instead-of-aborting.md) decision 1
(`VecExtent` joins `GridExtent` and `BitIndex` as a validated size newtype)

## Context

No collection could be built at a size.
[Handover 31](../handovers/31-what-an-aoc-solve-found.md) item 8 measured what
that costs on a real program. AoC 2025 day 12 is a 2D packing search, and the
board it packs into is a *working* grid — one the algorithm allocates for
itself, not one it reads. `Grid()` builds a 0×0 grid and takes no arguments, so
the board could not be a `Grid[Bool]`. It was a `Vec[Bool]` with the row stride
written out at all four use sites:

```praxis
if occ[ny * w + nx] { return false }
occ[(y + c.1) * w + (x + c.0)] = v
```

That is `Grid[Bool]` reimplemented by hand, minus the bounds checking, in the
one program that most wanted the real thing. Filling it cost a four-line
`while` loop, because there was no `Vec(n, fill)` either.

The capability was never missing. `praxis_grid_new` **is** a fill constructor —
`GridExtent::new(width, height)` then `vec![fill; extent.cells()]` — and the
codegen calls it with two `iconst 0`s because that is the only extent a source
program can ask for. `(0..n).map(|_| false)` builds a filled `Vec` today, which
is what day 7 used. Only the spelling was missing, and that makes the omission
harder to justify rather than easier.

The reason it stayed missing is a real one, and it is the whole difficulty of
this decision: **`Vec()` and `Vec(n, fill)` is an arity overload**, and ADR-089
decision 1 is titled "A name has exactly one signature" and reads "No
arity-based overloading, no optional or default parameters". Its closing
consequence says so in as many words: "This closes the precedent, not just the
case."

## Decision

### 1. A collection constructor's argument count selects its shape, from a closed compiler-owned table

Two rows, and the table is not open to anything else:

| Spelling | Type | Wrapper |
|---|---|---|
| `Vec(n, fill)` | `forall T. (Int, T) -> Vec[T]` | `praxis_vec_filled` |
| `Grid(w, h, fill)` | `forall T. (Int, Int, T) -> Grid[T]` | `praxis_grid_filled` |

`Vec()` and `Grid()` are unchanged and still build the empty collection;
`Grid()` is still the 0×0 grid. The arity chooses between them, and that is the
entire mechanism: `SIZED_CTORS` in `praxis_stdlib::prelude` is a two-row `const`
naming the source name, the wrapper, and how many of the leading arguments are
extents. Inference reads it, MIR reads it, and neither carries its own copy.

The table is keyed on the name, so the seven other constructors — `Deque`,
`Map`, `Set`, `Counter`, `MinHeap`, `MaxHeap`, `BitSet` — gain nothing. `Set(3,
0)` is still `Y024`, naming zero arguments. That is not an oversight to fix
later: a sized `Set` would be `n` copies of one element in a *set*, which is one
element, and a sized `Map` has no answer at all for what its keys are. `Vec` and
`Grid` are the two whose contents are addressed by position, which is the
property that makes "n of them" mean something.

### 2. This narrows ADR-089 decision 1; it does not reverse it

ADR-089 gave two grounds for the general rule, and **neither reaches a
collection constructor**. That is why this is a carve-out rather than a
repudiation.

- **"Overload resolution needs the argument types before it can pick a scheme;
  inference needs the scheme before it can check the arguments."** The
  circularity is real for a general overload and does not arise here, because
  the choice is made on the argument *count* — a syntactic fact, available
  before any argument is typed. `infer_call` collects the arguments, counts
  them, and builds the callee type from the count; the arguments are then
  unified against it exactly as they always were. Nothing is resolved by
  looking at a type.
- **"A bare `fn` name in value position is a closure value; an overloaded name
  has no single function value."** A collection constructor has no function
  value *at all*. `var f = Vec` is `Y022` — "`Vec` is a builtin, so it has no
  function value; call it: `Vec()`" — and has been since before this decision.
  So there is no `map(Vec)` for a second dispatch rule to disambiguate, and
  [ADR-061](./061-a-fn-name-in-value-position-is-a-closure.md) does not reopen.

ADR-089 already carved out §7.5's parser constructors on exactly this reasoning:
"Those fourteen constructors are a closed hand-written table with
per-constructor type synthesis. Not one is a `Func` scheme." The sized
constructors are the same shape of exception for the same reason — a closed
table the compiler owns, not a scheme a user program can declare — and
[ADR-073](./073-a-constructor-call-is-a-shape-checked-before-it-is-built.md)'s "the table states the shape,
not a count" is the same instinct one layer down.

**What stays closed.** No user function gets an overload. No optional or default
parameters anywhere, including here: `Vec(n)` and `Grid(w, h)` are `Y024`, and
the fill has no default, because "the element type's zero value" is exactly the
rule `praxis_grid_new` implements and exactly the rule that has no answer for a
composite cell (decision 6). No named arguments outside the parser sublanguage.
Anyone wanting a third row is asking to reopen this decision, which is the same
forcing function ADR-089's closing line installed.

### 3. The element type comes from the fill, and a written type argument still constrains it

`Vec(3, false)` is `Vec[Bool]` with no annotation, because the callee type
built at the call site is `(Int, ?T) -> Vec[?T]` with one fresh `?T` shared
between the fill parameter and the result. Unifying the arguments against it
pins `?T` from the fill, and the result carries it out.

This is built **at the call site's level**, not seeded as a second `Scheme`.
`seed_builtin_schemes` still stores exactly one scheme per name — the empty
form, `forall T. () -> Vec[T]` — so nothing downstream of a symbol's scheme
sees two of anything, and a symbol still carries one scheme as §5.1's model
requires.

[ADR-065](./065-a-type-constructors-brackets-are-type-arguments.md)'s bracket form composes
unchanged, because `apply_written_type_args` reads the callee's *result*:
`Vec[Bool](3, false)` agrees, and `Vec[Int](3, false)` is `Y001` on the fill.
That composition is why the sized form is a second arity of the existing name
rather than a second lowercase name like `vec_filled` — the bracket rule only
works on a type-constructor name, so a separate name would have cost it.

A `var Vec = …` binding in scope makes `Vec(3, 0)` an ordinary call of that
binding, not a construction: the table is consulted only for a
`SymbolKind::Builtin` resolution, which is HIR-03's rule about shadowing
applied where it already was.

### 4. The fill is one value stored `n` times, not `n` values

`Grid(2, 2, Vec())` gives four cells that are the *same* `Vec`, and a push into
one is visible from the other three.

This is not a new hazard; it is the language's existing reference semantics
stated at a new site. `var b = a` already aliases, and `outer.push(a)` twice
followed by `outer[0].push(9)` already prints `[[1, 2, 9], [1, 2, 9]]`. A
deep copy is also not something the runtime could do generically — cloning an
arbitrary `GcRef` means cloning through descriptor callbacks that do not exist
— so the alternative to stating the rule is not a better rule but a missing
feature.

For the scalar fills this decision exists to serve (`false`, `0`, `'.'`) the
distinction is unobservable, because a scalar box is immutable. It is stated,
documented in the book, and pinned by a test so that a later change to deep-copy
is a failing test rather than a silent semantic change.

### 5. A bad size is `FaultKind::InvalidSize`, and `VecExtent` is why it is not an abort

`Vec(-1, 0)` and `Grid(-1, 2, 0)` fault with *size or extent out of range*, and
so does any extent past 2^28 cells.

It cannot be a check-time refusal: the size is a runtime `Int`, and `Vec(n,
false)` for a computed `n` is the whole point of the feature. So it is the
answer ADR-041 already established for exactly this input —
`RaisedFault::INVALID_SIZE`, raised where the request cannot be honoured,
because there is no collection to return.

ADR-041 decision 1 requires that the route from an `Int` to an allocation size
be a validated newtype, so **`VecExtent::new(len: i64) -> Option<VecExtent>`
joins `GridExtent::new` and `BitIndex::new`** with the same cap
(`VecExtent::MAX_ITEMS = GridExtent::MAX_CELLS = 2^28`) for the same reason
decision 2 gave: a `checked_mul` that merely fits in a `usize` is still an
allocation no host can serve. `Grid(w, h, fill)` needs no new validation at all
— `GridExtent` already covers it, which is the point of having built it.

ADR-041's context paragraph — "`Grid[T](width, height)` … took a user `Int`
straight to a Rust allocation size" — describes a path that source could not
reach for as long as `Grid()` was nullary. It is live again from source now, and
`GridExtent` is precisely what makes that safe rather than a repeat of RT-07.

### 6. An explicit fill lifts the composite-element restriction

`praxis_grid_filled` does **not** call `default_cell`, and that is the point.

`praxis_grid_new` invents the cell type's zero value and has none for a
composite — `B::Vec | B::Grid | B::Map | … => None` — so it raises
`TypeMismatch` rather than filling a `Grid[Vec[Int]]` with the Unit sentinel
under a `Vec` descriptor, which would be a mislabelled element descriptor one
level down (P0-11). An explicit fill removes the question: the caller supplied a
value of the cell type, so there is nothing to invent. `Grid[Vec[Int]]` becomes
constructible from source for the first time — the input parser could already
build such a grid, so no other layer is surprised — and the descriptor is
reconciled through `adopt_or_reject`, the same helper `push` uses, so a null
static descriptor adopts the fill's and a genuine mismatch is still
`TypeMismatch` rather than a retag.

### 7. The extents cross the ABI boxed, unlike `praxis_grid_new`'s

`praxis_grid_filled` takes `(Ctx, Ptr, Gc, Gc, Gc)` where `praxis_grid_new`
takes `(Ctx, Ptr, RawI64, RawI64)`. A reader will notice, so: the extents are
boxed `Int`s because that is what MIR already has. Lowering an argument
expression yields a `Gc` local; a `RawI64` would need an `ExtractScalar` per
extent in the builder and a second shape in the codegen's allocation arm. The
cost is one `int_payload` read per construction, against a `vec![fill; n]` that
is about to run. `praxis_grid_new`'s raw extents are not wrong — its two are
`iconst 0` immediates, with no local to unbox.

The extents and the fill are carried as `LocalId` operands on
`AllocKind::Collection`'s new `init` field, spelled as a `CollectionInit` enum
whose *variant* is the arity: `Empty`, `Filled { count, fill }`, `FilledGrid {
width, height, fill }`. An operand list of the wrong length for the constructor
is therefore not something a builder forgot to check — it is something it cannot
spell. `AllocKind::constructor()` matches on `init` to answer the filled
wrapper, so `Inst::fault_reason` and the MIR verifier's `CheckFault`
requirement get the new answer with no second statement of the fact (MIR-10).
`liveness::uses` returns those operands, which is what roots the fill across the
allocating call.

## Consequences

- **Day 12's board becomes a `Grid`.** `tests/aoc-corpus/aoc2025_day12.px` drops
  its fill loop and all four hand-written `y * w + x` sites, its `fits` gains
  `contains` in place of four comparisons, and its answers are byte-identical.
  That is the measurement the handover asked for.
  `tests/aoc-corpus/adr146_sized_collections.px` is the feature's own fixture
  beside it.
- **The book stated the absence as a design fact in five places** —
  `grid-and-graphs.md`'s "it takes no arguments, so there is no way to ask for a
  sized one", `prelude.md`'s "All of them take no arguments" and its
  "neither can any collection constructor", and `faults.md`'s fault table row and
  its "no Praxis source reaches it". Every one is now false, and every one is
  corrected. There is still no `to_grid` on a sequence; that clause was the only
  true one in the passage.
- **`Y024` gets sharper, not blunter.** `Vec(3)` now reports that the function
  takes 2 arguments rather than 0, because the arity it is unified against is
  the one the count selected.
- **Nothing that compiled stops compiling.** Every existing `Vec()` and `Grid()`
  takes the `Empty` path unchanged, including the list-literal lowering, the
  pipeline collect target, and `Grid()`'s two `iconst 0`s.
- **A sized construction carries its source span and a nullary one still does
  not**, so `Vec(-1, 0)`'s crash snapshot names `Vec(-1, 0)` and `Vec()`'s temp
  reads as it always has. The asymmetry is the fault: a nullary construction
  cannot be refused for anything it was given, so a span on it would add a row to
  a report that is never about it — and the snapshot has a window, so an added
  row costs a real one. Giving every construction its span is a defensible
  separate change; it moves six debugger examples in the book, which is not this
  ADR's argument to make.
- **The `repeat` name from the handover is not added.** One operation, one
  spelling — which is ADR-089's own "the language already spells 'one operation,
  two shapes' as two names" read in the other direction.
