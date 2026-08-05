# ADR-127: A pipeline's source is the `for` loop's, and a collection converts by naming what it becomes

**Date:** 2026-08-04
**Status:** Accepted — implemented

## Context

Every one of these is `Y110`, measured against `target/debug/praxis check` on
this tree:

```praxis
var s = Set()
s.insert(1)
out(s.map(|x| x * 2).sum())      // no method `map` on type `Set[Int]`

var m = Map[Text, Int]()
out(m.items().len())             // no method `items` on type `Map[Text, Int]`

out((0..10).map(|x| x * 2).sum())      // no method `map` on type `Range`
out(deque.map(|x| x + 1).sum())        // no method `map` on type `Deque[Int]`
out("hello".count(|c| c == "l"[0]))    // no method `count` on type `Text`
```

And every one of these works today:

```praxis
for x in s { … }
for kv in m { out(kv.0) }
for c in "hello" { … }
for i in 0..10 { … }
```

So the language already has a complete, tested answer to "what can I iterate and
what does it yield" — `capability::iter_item` answers for eleven collections
plus `Text`, and MIR's `IterPlan` walks all of them, in a deterministic order,
through the accessors each one actually has. It has a *second*, much smaller
answer to the same question, and that one is the pipeline: §6.3's combinators
are registered on `Vec[T]` and on `Seq[T]`, and nothing else.

The gap is not a missing feature per collection. It is one feature registered
against one receiver.

Two more facts about the current shape, both of which this decision uses:

- **Twenty-three of the forty-nine pipeline rows are dead.** ADR-126 recorded it
  and declined to act: "No catalog row answers a `Seq[T]` … so all twenty
  `Seq`-receiver rows are dead … they become reachable the day a row answers a
  `Seq`." Nothing has answered a `Seq` in three milestones, and this decision is
  what makes the day never come — the generic receiver subsumes what the pair
  was for.
- **There is no route out of a keyed collection and no route back in.** `Set`
  has no member accessor at all in the language: `praxis_set_items` exists and
  is reachable only from `for`. `Map`/`Counter` have `keys()` and `values()`
  (REP-18) but no pair accessor. In the other direction there is exactly one row
  — `Vec.frequencies() -> Counter[T]` — and it *counts* rather than converts, so
  it cannot round-trip anything.

## Decision 1: one receiver pattern, `Iterable`, replaces the `Vec`/`Seq` pair

A new arm of the catalog's shape language:

```rust
/// A receiver the pipeline walks: any of the ten iterables in
/// `PIPELINE_RECEIVERS`, binding what it yields to `item`.
Iterable { item: Box<TypePattern> },
```

`map` becomes **one** row instead of two:

```rust
MethodEntry {
    receiver: TypePattern::Iterable { item: Box::new(TypePattern::var("T")) },
    name: "map",
    params: vec![t_to_u()],
    result: vec_of_u(),
    …
}
```

**The accepted receivers are named in one place**, `PIPELINE_RECEIVERS`, and the
list is the `for` loop's minus two:

| Receiver | Item |
| --- | --- |
| `Vec[T]` | `T` |
| `Deque[T]` | `T` |
| `Set[T]` | `T` |
| `MinHeap[T]`, `MaxHeap[T]` | `T` |
| `Range`, `BitSet` | `Int` |
| `Text` | `Char` |
| `Map[K, V]` | `(K, V)` |
| `Counter[T]` | `(T, Int)` |

**`Grid[T]` is excluded, and `grid.map` is why.** §6.4 requires `grid.map(fn)`
and it means the shape-preserving one — `Grid[T] -> Grid[U]`, cells in place. It
is unimplemented today (`g.map(|c| c)` is `Y110`, measured), so there is nothing
to break, but a generic row would claim the name and answer `Vec[U]` instead.
A grid's pipeline entry is `grid.cells()` and `grid.positions()`, which already
exist and already answer `Vec`s. The exclusion is enforced rather than intended:
`MethodCatalogBuilder::finish` gains a check that no concrete-receiver row shares
a `(name, arity)` with a generic row on a receiver in `PIPELINE_RECEIVERS`, so
the collision is a build failure and not a silent precedence.

**`Seq[T]` is excluded because it has no values.** The twenty-three `*_on_seq`
rows are deleted. `praxis-repr` already says a `Seq` has no runtime
representation; after this nothing produces one, nothing consumes one, and
retiring `CollectionCtor::Seq` itself becomes a mechanical follow-up rather than
a decision.

### How the item binds

Everywhere else, a catalog receiver is instantiated and **unified** with the
actual receiver — that is what pins `T` in `Vec[T].push(T)`. An `Iterable`
receiver cannot be: it accepts ten different constructors, and unifying against
any one of them pins the other nine out. So the row's receiver is not unified at
all. What is unified is the *item*:

```rust
// `iter_item` is the `for` loop's own answer to "what does this yield", and
// binding the row's item pattern to it is what makes `Iterable { item: (K, V) }`
// mean "a Map or a Counter".
let item_ty = capability::iter_item(&mut self.db, receiver_ty).expect("lookup accepted it");
let item_param = pattern_to_type_named(&mut self.db, item_pattern, &mut names);
let _ = self.db.unify(item_param, item_ty);
```

That is the whole of it, and it has one consequence worth stating: a row can
constrain *which* iterables it accepts by writing a shape into `item`.
`Iterable { item: Tuple[K, V] }` is "a `Map` or a `Counter`", because those are
the two whose item is a pair — which Decision 4 uses and nothing else has to.

Three implementation points, each of which is a place the rule could be
half-applied:

- **`pattern_matches` and `receiver_accepts` are the same rule written twice.**
  `praxis_hir::catalog::pattern_matches` decides dispatch and
  `praxis_lsp::completion::receiver_accepts` decides what `set.` offers; the
  LSP's own comment says it is "the same rule … restated". The `Iterable` arm
  must not be restated a third time, so the rule moves into `praxis-stdlib`
  beside `TypePattern` and both call it. It stays a pure pattern-level test —
  ctor membership in `PIPELINE_RECEIVERS`, or `Scalar(Text)` — so it needs no
  `TypeDb` and the immutable-borrow shape of `lookup` is unchanged.
- **`infer_catalog_call` and `resolve_deferred_method` instantiate the entry
  twice**, in two copies of the same eight lines. The `Iterable` path has to
  reach both — `fn top(v) { v.map(f) }` resolves through the deferred one — so
  the block is extracted into one `bind_receiver` both call. A second copy is a
  second place for this to be missing.
- **`MethodEntry::bounds` sweeps the receiver**, so `TypePattern::Iterable` needs
  a `collect_bounds` arm. `sum`'s `Bound::Is(Int)` lives on the item variable and
  keeps working unchanged — including its load-bearing half, that an item type
  nothing has pinned yet is *pinned* to `Int` rather than merely permitted.

### The receiver generalizes; two *parameters* must not

`zip`'s argument is `Vec[U]` and `flat_map`'s closure answers `Vec[U]`, and both
stay that way. The fused loop indexes each of them with `praxis_vec_len` and
`praxis_vec_get` directly — `Step::Zip` walks its second source with its own
dense counter, and a splice walks the inner `Vec` — and neither has an
`IterPlan` in scope, because neither is the source. Generalizing those two
patterns to `Iterable` would put a `SetPayload` under `praxis_vec_get`, which is
the exact wrong-type read `IterPlan` exists to prevent.

So `m.zip(s)` on a `Set` is a unification failure at the argument, and the
spelling is `m.zip(s.to_vec())`. That is a second thing `to_vec` earns its place
for, and it is the honest boundary: the pipeline generalizes over what it
*walks*, not over every sequence a row mentions. If those parameters should
generalize later, the fix is to give the two emitters an `IterPlan` too, and it
is its own decision.

### What this costs and what it buys

Forty-nine rows become twenty-six — 23 fused stages and sinks, plus the 3
barriers — and ten receivers get all twenty-six instead of one receiver getting
them twice. `sum`'s `Int` bound, `sorted`'s `Ord` bound and `unique`'s
`HashStable` bound are written once instead of once per receiver, so they cannot
drift.

## Decision 2: the pipeline's source protocol is `IterPlan`

`lower_pipeline` opens the source with a hardcoded pair:

```rust
emit_bounds_check(b, src, idx, body_blk, exit);   // praxis_vec_len
b.call_runtime(item, RuntimeSymbol::VecGet, vec![src, idx]);
```

`lower_for` opens it with `iter_plan`, and `iter_plan`'s own doc says what the
hardcoded pair would do to a `Set`: "reading a `Set`'s payload through
`praxis_vec_get` was a wrong-type read that hung or killed the process, and a
`MinHeap`'s was a silently wrong answer."

So the prologue and the body-head come out of `lower_for` into one pair of
helpers — `emit_iter_source` (the snapshot, or the receiver itself) and
`emit_iter_item` (the indexed read, plus the `AllocKind::Tuple` that pairs a
`Paired` plan's two aligned snapshots) — and `lower_pipeline` calls them. One
function decides how a collection is walked, and `for` and the pipeline cannot
disagree about the order or about what a member is.

Three things follow, and the third is the reason this is a decision and not a
refactor:

- **Vec, Deque, Range and Text stay allocation-free.** They are
  `IterPlan::InPlace`; nothing about `v.map(f).sum()` changes.
- **Set, the heaps, BitSet, Map and Counter take one snapshot `Vec`** before the
  loop — the same one `for` takes, one call per pipeline rather than per step.
  A fused chain over a `Map` is one loop over two aligned snapshots, and
  `m.filter(p).count()` allocates two vectors and no more.
- **The order is the program's answer, not its printing.** `praxis_set_items`
  and the `keys`/`values` pair go through `maps::ordered_members`, which is
  deterministic and seed-independent. `s.map(f)` therefore answers the same
  `Vec` on every run, which a `HashSet` iteration order would not.

## Decision 3: a barrier materializes its receiver, rather than staying `Vec`-only

`sorted`, `unique` and `frequencies` are `RuntimeSymbol` rows, not intrinsics —
they need the whole sequence before they can answer, so they are wrapper calls
over a real `VecPayload`. Their receiver becomes `Iterable` like everything
else, and the lowering inserts the same `IterPlan` snapshot in front:

> A row whose receiver pattern is `Iterable` and whose lowering is a
> `RuntimeSymbol` is called on `emit_iter_source(receiver)`, never on the
> receiver local.

The alternative — leaving the three on `Vec[T]` — was rejected because it makes
`set.map(f).sorted()` legal and `set.sorted()` a `Y110`, which is a rule nobody
can hold in their head. Getting it wrong in the other direction is the defect
`IterPlan` was built out of, so the snapshot is not optional and the rule above
is what a test asserts: for each of the three, a non-`Vec` receiver emits its
plan's snapshot symbol before the wrapper.

## Decision 4: `to_vec` is the one materializer, and `to_set`/`to_map`/`to_counter` are fused sinks

Nine new rows, all on the `Iterable` receiver:

| Row | Receiver item | Result | Per-element wrapper |
| --- | --- | --- | --- |
| `to_vec()` | `T` | `Vec[T]` | `praxis_vec_push` |
| `to_set()` | `T` (hash-stable) | `Set[T]` | `praxis_set_insert` |
| `to_map()` | `(K, V)` | `Map[K, V]` | `praxis_map_insert` |
| `to_counter()` | `(T, Int)` | `Counter[T]` | `praxis_counter_set` |
| `to_deque()` | `T` | `Deque[T]` | `praxis_deque_push_back` |
| `to_min_heap()` | `T` (orderable) | `MinHeap[T]` | `praxis_min_heap_push` |
| `to_max_heap()` | `T` (orderable) | `MaxHeap[T]` | `praxis_max_heap_push` |
| `to_bitset()` | `Int` | `BitSet` | `praxis_bitset_insert` |
| `sorted_by_key(f)` | `T` | `Vec[T]` — see Decision 5 | — |

**The set is closed at "every collection with a constructor", and that is the
point.** The ask named `Set`/`Map`/`Counter`; the last four are here because
leaving them out is what creates the asymmetry Decision 6 then has to defend —
`deque.filter(p)` would answer a `Vec[T]` with no way back to a `Deque`, and
`v.to_min_heap()` is the only way to build a heap from a sequence without a
`while` loop. Each is one row over a wrapper that already exists and a
`CollectInto` sink that is generic in the ctor, so the marginal cost of the four
is a table entry apiece. `Grid` is the one collection with no row, because a
grid needs a width and a flat item sequence does not carry one.

`to_map` and `to_counter` say "a pair" in their receiver pattern rather than in
prose, so `[1, 2].to_map()` is a unification failure at the method name —
"expected `(K, V)`, found `Int`" — and not a row that resolves and then faults.
`to_bitset` says `Int` the same way, and `praxis_bitset_insert` still faults on a
negative or oversized member, which is a value question no type can answer.

### They are sinks, so they fuse

`Sink::CollectInto { ctor, args }` sits beside `Sink::Collect`. The accumulator
*is* the target collection, allocated before the loop through
`AllocKind::Collection` — which already resolves a `Map`'s key descriptor and a
`Set`'s element descriptor from the static type args — and the per-element step
is one wrapper call that already exists: `praxis_set_insert`,
`praxis_map_insert`, `praxis_counter_set`. A pair item is taken apart with the
`praxis_tuple_get` MIR already emits.

So `v.map(f).to_set()` is **one** loop with no intermediate `Vec`, which is what
makes these sinks rather than barriers. `Sink::Collect`'s own vector is
allocated with `args: vec![MirType::Opaque]` and adopts a descriptor on first
push; `CollectInto` should pass the real static args instead, because it has
them.

### `to_vec` on a `Vec` is the identity, and that is not `collect` coming back

ADR-126 deleted `collect` because it "named a step the compiler takes anyway" —
a chain ending on a stage already materializes, so `v.map(f).collect()` and
`v.map(f)` built the same plan. `to_vec` is not that. For nine of its ten
receivers it names a step the compiler does **not** take: `s.to_vec()` is the
only way to get a `Vec[T]` out of a `Set`, and `m.to_vec()` is the only way to
get `Vec[(K, V)]` out of a `Map` — `keys()` and `values()` answer two aligned
halves and nothing joins them.

On a `Vec` receiver it degenerates to the identity, and it answers **the same
reference**, not a copy. That is the second half of ADR-126 decision 2 kept:
that decision declined to leave a shallow copy behind "under a name that does
not mention it", and `to_vec` does not mention one either. If `Vec` should be
copyable it still wants its own decision, on every collection with the same
need.

### `to_counter` and `frequencies` are different questions

`v.frequencies()` counts occurrences of each element; `pairs.to_counter()`
assigns the count each pair carries. They are the two directions of the same
type change and neither expresses the other, so both stay. Duplicate keys in
`to_map`/`to_counter` resolve last-wins, which is `insert`'s existing rule.

`v.to_set()` and `v.unique()` are also not the same question, for one reason
worth writing next to both: `unique` answers a `Vec` in **first-occurrence
order**, and a `Set` has no order to preserve.

## Decision 5: `sorted_by_key`, because a keyed collection cannot order its own items

```praxis
var pairs = m.keys().zip(m.values())
out(pairs.sorted())          // error[Y006]: values of type `(Text, Int)` cannot be ordered
```

Measured on this tree, and correct: ADR-045 decided that no composite is
orderable, because MIR had one integer compare and `(1, 2) < (1, 3)` would have
compared two schema pointers. So the moment a pipeline's item is a pair — which
is the moment its source is a `Map` or a `Counter` — `sorted` is unavailable,
and "the five most common values" has no spelling.

`sorted_by_key(|item| key)` is the spelling. The closure extracts an orderable
key; the sort is decorate–sort–undecorate through the same `praxis_value_cmp`
`sorted` already uses, so the `Ord` bound moves from the element to the
extracted key and the composite-ordering question ADR-045 deferred stays
deferred.

**Not `sorted_by(|a, b| a < b)`.** `min_by`/`max_by` already own the
less-than-predicate shape, so a second row in it would be one shape answering
two questions; and a comparator is O(n log n) calls back into JIT'd code where a
key extractor is n. The callback mechanism itself is not new — §6.5's graph
helpers already call closures from runtime wrappers.

**This is the one row here that the ask did not name**, and it is separable: drop
it and Decisions 1–4 still stand. Without it, `counts.to_vec().max_by(|a, b| a.1 < b.1)`
gets the single most common element and nothing gets the top five.

## Decision 6: `map_values` and the other shape-preserving conveniences are declined

`m.map_values(f) -> Map[K, U]` is the obvious next row. So is `m.filter_keys(p)`,
a `Set`-preserving `filter`, a `Counter`-preserving `map_values`, a
`Deque`-preserving `take`. **The reason to decline is not that any one of them is
bad; it is that they are not one row, they are a rule** — and it is a second rule
about what a combinator answers, running parallel to the one Decision 4 just
bought.

The rule this ADR keeps instead, stated once so the twenty-six combinators do not
each have to: **a pipeline's currency is `Vec`.** Every stage answers a `Vec`,
whatever the source was, and a program that wants a collection back says which
one. `set.filter(p)` is a `Vec[T]`; `set.filter(p).to_set()` is a `Set[T]`. One
sentence answers "what does `filter` return" for all ten receivers, and it is
answerable without knowing which receiver you are on.

The shape-preserving family would replace that with: *`filter` answers the
receiver's own type where a row exists for it, and a `Vec` otherwise.* Which rows
exist becomes something a reader has to memorize per collection, and adding one
later silently changes what an existing expression returns. That is the same
hazard ADR-077 refused when it declined to let `.name` mean either a field or a
call: not that either meaning is wrong, but that nothing at the use site chooses
between them.

Three supporting facts, in the order they bind:

- **Every one of them is expressible.** `m.map_values(f)` is
  `m.map(|(k, v)| (k, f(v))).to_map()`, and it is the same `Map[K, U]` with the
  same entries — keys are untouched, so `to_map`'s last-wins rule never fires.
  `set.filter(p).to_set()`, `counts.map(...).to_counter()`, and so on.
- **Nothing asks for them.** §6.3 lists neither, and no program in
  `tests/aoc-corpus/`, the CLI fixtures or `benchmarks/` is waiting on one. This
  is the standard REP-46 applied to `wrapping_mul` and the `Char` rows applied to
  `is_digit`/`to_upper`: a row goes in when something needs it, not when it would
  be reasonable.
- **The catalog is a duplicate-free table keyed by `(receiver, name, arity)`,
  and both spellings would resolve.** `Map[K, V].map/1` as a concrete row beside
  the generic `Iterable.map/1` is precisely the collision Decision 1's builder
  check is there to reject. Admitting the family means replacing that check with
  a precedence rule, and a precedence rule is the thing that makes "which does
  this call resolve to" a question at all.

**The honest argument on the other side, and how it should be settled.** The
desugared form allocates one tuple per entry that a direct `map_values` would
not — a real cost in a GC'd language with a pacer, and the only claim in this
decision that a measurement could overturn. If it is overturned, the answer is
not one convenience row: it is to state the shape-preserving rule for every
collection at once, with its own ADR and its own answer to precedence. Adding
`map_values` alone would be the first row of a family nobody has decided to
have.

`grid.map` is the one shape-preserving row that stays owed, and it is owed for a
reason none of the above have: §6.4 asks for it by name, and **no pipeline
spelling produces a `Grid`** — a grid needs a width, which a flat item sequence
does not carry. It is not a convenience over an expressible chain; it is the one
case where the chain does not exist.

## Consequences

- **§6.3 is rewritten** around the receiver table in Decision 1, gains
  `to_vec`/`to_set`/`to_map`/`to_counter`/`sorted_by_key`, and gains the
  currency rule from Decision 6. §6.1 and §6.2 gain the conversion routes; §5.7's
  example rows gain the `Iterable` receiver shape. Every fence is executable and
  the design-doc gate already runs them.
- **No diagnostic code is spent.** A non-iterable receiver is `Y110` from the
  same door it is today, because `lookup` matches no row for it. A wrong item
  shape (`[1,2].to_map()`) is an ordinary unification report at the method name.
  A non-orderable `sorted_by_key` key is `Y006`, which is what `sorted` already
  gives.
- **`intrinsics_are_all_recognized_so_there_is_no_second_lowering` re-bases.**
  Its `checked >= 40` floor was written against 47 intrinsics and 23 of those are
  the dead `Seq` half; the floor becomes the new count. The assertion it makes —
  every registered intrinsic is classified by `classify_link` or `classify_sink`
  — is unchanged and is what keeps the nine new rows from lowering to nothing.
- **The LSP gains thirty-five completions on `set.`, `m.`, `counts.`, a
  `Range` and a `Text`** — the twenty-six that move to the `Iterable` receiver
  plus the nine new rows. §19.11's acceptance criterion that a `Map` method is
  absent from `grid.` still holds: `Grid` is out of `PIPELINE_RECEIVERS` and the
  `Map` rows are `Map`-receiver rows.
- **`count()` at arity 0 and `len()` remain two spellings of one question**, now
  on ten receivers instead of one. That duplication is inherited, not introduced
  — `v.count()` and `v.len()` both exist today — and closing it is its own
  decision.
- **`chunks` and `windows` stay deferred.** Their `Vec[Vec[T]]` result is still
  the unanswered descriptor-labelling question M8-WS11 recorded, and nothing here
  answers it.

## Implementation order

Each step is independently testable and leaves the tree green:

1. `TypePattern::Iterable` + `PIPELINE_RECEIVERS` + the shared acceptance rule in
   `praxis-stdlib`; `collect_bounds` arm; the builder's collision check.
2. `bind_receiver` extracted from `infer_catalog_call` / `resolve_deferred_method`,
   with the `iter_item` path. No catalog change yet — behaviour identical.
3. `emit_iter_source` / `emit_iter_item` extracted from `lower_for`;
   `lower_pipeline` calls them. Still `Vec`-only by the catalog, so this is a
   refactor with no surface change and the existing pipeline tests are the gate.
4. Flip the 23 fused rows to the `Iterable` receiver and delete the 23 `Seq`
   twins. This is where `set.map(f)` starts working.
5. The 3 barrier rows plus the snapshot rule from Decision 3.
6. `Sink::CollectInto` and the eight conversion rows. `to_vec`/`to_set`/`to_map`/
   `to_counter` first — they are the ones the ask named — then the four whose
   only cost is a table entry.
7. `sorted_by_key`.
8. §6 amendments and the corpus program that uses them.

Steps 1–3 are the ones with no user-visible effect and the highest chance of
being the ones that go wrong; they should land and be gated before step 4.

## Appendix: the §6.3 replacement, drafted

Held here rather than in `praxis_technical_design.md` because the design-doc
gate executes every `praxis` fence in that file and none of this compiles yet.
Step 8 is a paste.

> ### 6.3 Functional sequences
>
> The language exposes compiler-known sequence pipelines without a user-visible
> iterator type. **A pipeline's receiver is anything a `for` loop can walk**, and
> it yields what the `for` loop's variable would bind:
>
> | Receiver | Item |
> | --- | --- |
> | `Vec[T]`, `Deque[T]`, `Set[T]`, `MinHeap[T]`, `MaxHeap[T]` | `T` |
> | `Range`, `BitSet` | `Int` |
> | `Text` | `Char` |
> | `Map[K, V]` | `(K, V)` |
> | `Counter[T]` | `(T, Int)` |
>
> ```praxis
> var answer = values
>     .filter(|x| x > 0)
>     .map(|x| x * x)
>     .sum()
>
> var loud = counts.filter(|(word, n)| n > 2).map(|(word, n)| word)
> ```
>
> `Grid[T]` is not in the table: `grid.map(fn)` is §6.4's shape-preserving row
> and answers a `Grid`. A grid enters a pipeline through `grid.cells()` or
> `grid.positions()`.
>
> **A pipeline's currency is `Vec`.** Every streaming stage answers a `Vec`,
> whatever the receiver was, and a program that wants a collection back says
> which one — `set.filter(p)` is a `Vec[T]` and `set.filter(p).to_set()` is a
> `Set[T]`. Iteration order is deterministic and seed-independent for every
> receiver, so a pipeline's answer is a function of its input alone.
>
> Streaming stages and sinks:
>
> - `map`, `filter`, `filter_map`, `flat_map`
> - `take`, `skip`, `take_while`
> - `enumerate`, `zip`
> - `fold`, `reduce`, `sum`, `product`, `count`
> - `any`, `all`
> - `find` — the first matching **element**, as `Option[T]`
> - `position` — the first matching element's **index**, as `Option[Int]`
> - `min`, `max`, `min_by`, `max_by`
>
> Barriers — they need the whole sequence before answering, so they are runtime
> calls rather than fused stages, and a chain ends at one and begins again from
> its result:
>
> - `sorted`, `sorted_by_key`, `unique`, `frequencies`
>
> Conversions:
>
> - `to_vec()` — the item sequence as a `Vec`. On a `Vec` receiver this is the
>   receiver itself, not a copy.
> - `to_set()` — a `Set[T]`, duplicates dropped and no order kept. `unique()` is
>   the ordered answer to a different question: a `Vec[T]` in first-occurrence
>   order.
> - `to_map()` — a `Map[K, V]`, on a pipeline whose item is a pair. Last wins.
> - `to_counter()` — a `Counter[T]`, on a pipeline whose item is a `(T, Int)`
>   pair, taking each pair's count. `frequencies()` is the other direction: it
>   *counts* occurrences rather than reading a count.
> - `to_deque()`, `to_min_heap()`, `to_max_heap()`, `to_bitset()` — the same for
>   the remaining constructors. `to_bitset()` needs `Int` members and faults on a
>   negative or oversized one, as `BitSet.insert` does.
>
> There is no `to_grid()`: a grid needs a width, and an item sequence does not
> carry one.
>
> ```praxis
> var counts = words.frequencies()
> var top = counts.to_vec().sorted_by_key(|(word, n)| 0 - n).take(5)
> var back = top.to_counter()
> ```
>
> **There is no `collect`, and this is the one place the list is shorter than a
> Rust programmer expects.** A chain that ends on a streaming stage materializes
> on its own — `v.map(f)` *is* a `Vec[U]` — so the spelling named a step the
> compiler takes anyway (ADR-126). `to_vec` is not its return: for nine of its
> ten receivers it is the only way to reach a `Vec` at all.
>
> The compiler fuses every non-barrier chain into a single loop over the source
> (ADR-029). A receiver that indexes itself — `Vec`, `Deque`, `Range`, `Text` —
> is walked in place with no intermediate allocation; the rest are snapshotted
> once before the loop, which is what a `for` over them already does (ADR-127).
> `v.map(f).filter(p).sum()` is one loop with zero intermediate `Vec`s, and
> `v.map(f).to_set()` is one loop that inserts into the `Set` directly.
>
> `chunks` and `windows` remain deferred — they answer `Vec[Vec[T]]`, which needs
> a rule for what the outer vector's element type is labelled with, and nothing
> in this document forces one.
