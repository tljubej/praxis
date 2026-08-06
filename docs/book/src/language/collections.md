# Collections

Praxis ships ten collections and no way to define an eleventh. They are built
into the compiler: each one is a fixed set of rows in the method catalog, with a
runtime representation the collector knows how to trace. There are no traits, no
generic containers you write yourself, and no imports — every name below is
already in scope.

| type | what it is | key or element requirement |
|---|---|---|
| `Vec[T]` | growable ordered sequence; has a literal | none |
| `Deque[T]` | double-ended queue | none |
| `Map[K, V]` | hash map | `K` immutable and hashable |
| `Set[T]` | hash set | `T` immutable and hashable |
| `Counter[T]` | map whose absent values read as zero | `T` immutable and hashable |
| `MinHeap[T]` | priority queue, smallest first | `T` orderable |
| `MaxHeap[T]` | priority queue, largest first | `T` orderable |
| `BitSet` | compact set of non-negative `Int`s | — (members are `Int`) |
| `Grid[T]` | rectangular 2-D array | none |
| `Range` | half-open interval over `Int` | — (members are `Int`) |

`Grid[T]` is covered in [grids and graphs](grid-and-graphs.md), because
everything interesting about it is two-dimensional. `Text` is a scalar rather
than a collection, but it subscripts and iterates like one; see
[Text and Char](text.md).

Tuples are not in the table — they are a structural type rather than a
collection — but they are what makes `Map[(Int, Int), T]` work, so they are
described [below](#tuples).

## Constructing a collection

Every collection but `Range` is built by calling its name:

```praxis
var v = Vec()
var q = Deque()
var m = Map()
var s = Set()
var counts = Counter()
var lo = MinHeap()
var hi = MaxHeap()
var bits = BitSet()
```

The element types come from what the program later puts in. When that is not
enough — or when you would rather say it than derive it — write the type
arguments in brackets before the parentheses
([ADR-065](../../../decisions/065-a-type-constructors-brackets-are-type-arguments.md)):

```praxis
var counts = Counter[(Int, Int)]()
var ages = Map[Text, Int]()
```

Those brackets are type arguments, not a subscript, because the name in front is
a compiler-owned type constructor. That is the whole rule, and its price is
stated rather than hidden: a `var` binding that shadows one of the ten
constructor names cannot be subscripted. The arguments *unify* with the call's
own variables, so a disagreement is reported at the use that disagrees:

```praxis
var c = Counter[Text]()
c.inc(1)
```

```console
$ praxis check type-arg-mismatch.px --color never
error[Y001]: expected Text, found Int

  type-arg-mismatch.px:2:7
  2 | c.inc(1)
    |       ^ expected Text, found Int

praxis: 1 error(s)
```

Substituting the annotation instead of unifying would have made it win silently;
inferring first and then comparing would have reported at the constructor, which
is not where the mistake is. The wrong *number* of arguments is `Y007`:

```text
error[Y007]: `BitSet` takes 0 type argument(s), but 1 were given
```

### Building one at a size

`Vec` and `Grid` also take a **size and a fill**, which is how you get the
working collection an algorithm allocates for itself — an occupancy board, a
visited mask, a distance table, a DP row:

```praxis
// `Vec(n, fill)` and `Grid(w, h, fill)`: the collection an algorithm allocates
// for itself, rather than one it reads or grows a push at a time.
var row = Vec(5, 0)
var board = Grid(3, 2, '.')
board[1, 1] = '#'

out(row)
out(board)
out(board.width())
out(board.height())
```

```text
[0, 0, 0, 0, 0]
[., ., ., ., #, .]
3
2
```

The element type comes from the fill, so `Vec(3, false)` is a `Vec[Bool]` with
nothing written down, and the bracket form composes with it when you would
rather say it: `Vec[Bool](3, false)`.

**Only those two have a sized form.** The other seven take no arguments at all,
and `Set(3, 0)` is an error saying so. Praxis has no arity overloading anywhere
else — one name, one signature
([ADR-089](../../../decisions/089-a-name-has-one-signature.md)) — and these two
are a deliberate, closed exception, recorded with its reasoning in
[ADR-146](../../../decisions/146-a-collection-constructors-arity-is-its-shape.md).
`Vec` and `Grid` are the collections whose contents are addressed by position,
which is what makes "n of them" mean something: a sized `Set` would be `n`
copies of one element in a set, which is one element.

**The fill is one value stored `n` times, not `n` copies of it.** For a scalar
that is unobservable, but a collection fill gives you `n` names for the *same*
collection:

```praxis
// The fill is one value stored in every slot, not one copy per slot — the same
// reference semantics `var b = a` has. A push into any cell is visible from all
// four, because they are the same `Vec`.
var cells = Grid(2, 2, Vec())
cells[0, 0].push(1)
out(cells[1, 1])
```

```text
[1]
```

That is the same reference semantics a collection already has everywhere else: a
binding names an object rather than owning it, so `var b = a` does not copy and
neither does this. If you want `n` distinct collections, build them:
`(0..n).map(|_| Vec())`.

A negative size — or one so large the runtime cannot allocate it — is not
something `praxis check` can refuse, because the size is an ordinary `Int`
computed at run time. It is a fault instead
([ADR-041](../../../decisions/041-bounded-extents-fault-instead-of-aborting.md)),
and the expression that asked for it is named in the report:

```praxis
// A size is an ordinary `Int` computed at run time, so a negative one is not
// something `praxis check` can refuse. It is a fault.
var n = 0 - 1
var v = Vec(n, 0)
out(v)
```

```text
error: program faulted: size or extent out of range

Backtrace:
#0   <entry>

  locals:
    n: Int = -1
    v: Vec[Int] = <uninit>
  temps:
    <tmp#1: Int> @ "0" = <uninit>
    <tmp#2: Int> @ "1" = 1
    <tmp#3: Int> @ "0 - 1" = -1
    <tmp#5: Int> @ "0" = 0
    <tmp#6: Vec[Int]> @ "Vec(n, 0)" = <uninit>
```

The ceiling is 2²⁸ items, which is two gigabytes of references before a single
element object exists — a judgement about what a program plausibly asks for
rather than what a `usize` happens to hold.

### The list literal

`Vec` is the one collection with a literal
([ADR-099](../../../decisions/099-a-list-literal-is-a-vec-and-a-text-is-iterable.md)).
`[a, b, c]` *is* `Vec()` followed by one `push` per element, in source order —
same type, same methods, same mutability. There is no separate immutable array.

```praxis
// A list literal is a Vec: an allocation followed by one push per element.
var v = [3, 1, 2]
v.push(4)
v[0] = 30

out(v)
out(v.len())
out(v[1])
out(v.get(3))
out(v.is_empty())

// An empty literal has no element to read a type from, so the use decides —
// here, an annotation.
var names: Vec[Text] = []
names.push("ada")
out(names)
```

```text
[30, 1, 2, 4]
4
1
4
false
[ada]
```

Inference mints one fresh element variable and unifies each element with it in
turn, so `[]` is the ordinary case rather than an exception, and a mixed literal
reports at the element that disagrees: `[1, "a"]` is
`error[Y001]: expected Int, found Text` under the `"a"`.

A `[` that *begins* an expression opens a literal; a `[` that continues one
subscripts. Position is the whole tie-break, which means a subscript has to be
written on one line with its receiver — a `v` at the end of one line and a `[i]`
at the start of the next is two statements, a value and a list literal that goes
nowhere, and nothing reports it.

## Subscripting

A subscript is a method-catalog row dispatched on the receiver's shape *and*
its arity, under names no program can spell
([ADR-064](../../../decisions/064-a-subscript-is-a-catalog-row.md)). Six types
read; five of those six also store.

| receiver | `x[i]` reads | `x[i] = v` stores | `min=` / `max=` |
|---|---|---|---|
| `Vec[T]` | `T`, faults out of range | replaces, faults out of range | — |
| `Deque[T]` | `T` at 0-based front offset | replaces, faults out of range | — |
| `Text` | `Char` by Unicode scalar | — (a `Text` is immutable) | — |
| `Map[K, V]` | `V`, **faults if absent** | sets, replacing any prior value | yes, when `V` is `Int` |
| `Counter[T]` | `Int`, **zero if absent** | sets the count outright | — |
| `Grid[T]` | `T` at `[x, y]` | sets the cell at `[x, y]` | — |

`Set`, the heaps, `BitSet` and `Range` have no subscript at all, and there is no
slicing anywhere — an index is a single `Int`, so `v[0..2]` is a type error
(`expected Int, found Range`) rather than a slice. `s[0]` on a `Set[Int]` is:

```text
error[Y020]: values of type `Set[Int]` cannot be indexed with 1 index(es)
```

Because it goes through the same dispatch a method call does, a subscript is
exactly as generic as a method call, and no more: a function that indexes an
unannotated parameter infers, and the first call site decides what the parameter
was. Passing a second receiver *kind* through the same function is a
disagreement about that function's signature rather than a second
instantiation — given `fn first(c, k) { c[k] }`, calling it on both a
`Map[Text, Int]` and a `Vec[Int]` reports:

```text
error[Y001]: expected (Map[Text, Int], Text) -> ?T, found (Vec[Int], Int) -> ?T
```

### A missing `Map` key faults

`m[k]` is the assertion-like read and `m.get(k)` is the one that answers with
absence. They are two different catalog rows pointing at two different runtime
wrappers, so the choice is the program's.

```praxis
// A subscript is dispatched exactly like a method call, so `m` and `k` need no
// annotation: the call site below is what says they are a `Map[Text, Int]` and
// a `Text`.
fn lookup(m, k) {
    m[k]
}

var ages = Map[Text, Int]()
ages["ada"] = 36
out(lookup(ages, "grace"))
```

```text
error: program faulted: index out of bounds

Backtrace:
#0   lookup
#1   <entry>

  locals:
    m: Map[Text, Int] = {ada: 36}
    k: Text = grace
  temps:
    <tmp#3: Int> @ "m[k]" = <uninit>
```

The fault kind is `IndexOutOfBounds` — an index the collection does not hold.
A dedicated `MissingKey` would read better and does not exist. Run without
`--debug never` and that snapshot becomes an interactive session; see
[the fault model](../debugger/faults.md).

A `Counter` read never faults, which is the whole point of the type: an absent
key reads as zero, and that is what makes `counts[k] += 1` work on a key never
seen before.

### Stores replace, and compound stores evaluate the place once

A `Vec` or `Deque` store replaces the element at that index and never appends.
`v[v.len()] = x` is a fault, not a push
([ADR-124](../../../decisions/124-a-field-and-a-sequence-element-are-places.md)):

```praxis
fn store(v, i, x) {
    v[i] = x
}

var v = [1, 2, 3]
store(v, v.len(), 4)
out(v)
```

That program faults with `index out of bounds`. `push` is the spelling that
grows a sequence.

`m[k] += v` is not desugared into `m[k] = m[k] + v`. The receiver and every
index are lowered once into locals that both the read and the write use, so
`m[f()] += 1` calls `f` exactly once.

### `min=` and `max=`

`Map` has two updating stores, and they exist because a read-modify-write cannot
express them: an absent entry accepts the first value, where a plain subscript
read of an absent key would fault
([ADR-070](../../../decisions/070-an-updating-store-is-a-row-with-a-contextual-operator.md)).

```praxis
var distance = Map[Text, Int]()

// An absent entry accepts the first value, so no key has to be seeded.
distance["b"] min= 7
distance["b"] min= 4
distance["b"] min= 9
out(distance["b"])

var best = Map[Text, Int]()
best["b"] max= 7
best["b"] max= 4
best["b"] max= 9
out(best["b"])
```

```text
4
9
```

Three constraints on the form. The value type must be `Int` — the wrappers
compare through the integer payload, so `m["a"] min= "y"` is
`error[Y001]: expected Int, found Text`. `Map` is the only receiver, and a
`Counter` gets its own message rather than the plain-store one:

```text
error[Y020]: values of type `Counter[?T]` cannot be updated with `min=` through 1 index(es)
```

And the operator is contextual and adjacent: `min` and `max` are still ordinary
[prelude](prelude.md) functions, so it is the `=` touching the identifier that
makes the pair an operator. `d[k] min = 3`, with a space, is two statements run
together, and reports as such.

## `Vec`

| method | answer |
|---|---|
| `push(T)` | `Unit` — append to the end |
| `len()` | `Int` |
| `get(Int)` | `T` — faults `IndexOutOfBounds` if out of range |
| `is_empty()` | `Bool` |
| `to_text()` | `Text` — the elements as one line; they must be `Char` |
| `v[i]`, `v[i] = x` | see [Subscripting](#subscripting) |

`get(i)` and `v[i]` are two spellings of one row and behave identically —
neither answers an `Option`, and both fault out of range. There is no
`contains`, `pop`, `insert`, `remove`, `first` or `last`; the
[pipeline](pipelines.md) stages (`any`, `find`, `position`, `sorted`, …) are
where those questions get asked. Reversing is one of them: `v.reversed()`
answers a new `Vec` and leaves the receiver alone, and there is no in-place
`reverse`. The example under [the list literal](#the-list-literal) exercises
every row above.

`to_text()` is the odd one out on this table, because it is the only row here
that is not about a `Vec` of anything — a `Vec[Char]` becomes the line it
spells, which is how a `Grid` row is drawn back
([ADR-144](../../../decisions/144-a-sequence-of-text-joins-and-a-sequence-of-char-becomes-one.md)).

## `Deque`

| method | answer |
|---|---|
| `push_front(T)`, `push_back(T)` | `Unit` |
| `pop_front()`, `pop_back()` | `T` — faults on an empty deque |
| `len()` | `Int` |
| `get(Int)` | `T` — 0-based from the front |
| `is_empty()` | `Bool` |
| `d[i]`, `d[i] = x` | 0-based from the front |

```praxis
var queue = Deque()
queue.push_back("b")
queue.push_back("c")
queue.push_front("a")

out(queue)
out(queue.len())
out(queue[0])

queue[1] = "B"
out(queue.pop_front())
out(queue.pop_back())
out(queue)
```

```text
[a, b, c]
3
a
a
c
[B]
```

## `Map`

| method | answer |
|---|---|
| `insert(K, V)` | `Unit` — replaces any prior value |
| `get(K)` | `Option[V]` |
| `contains(K)` | `Bool` |
| `remove(K)` | `Unit` |
| `len()`, `is_empty()` | `Int`, `Bool` |
| `keys()` | `Vec[K]` |
| `values()` | `Vec[V]` |
| `m[k]`, `m[k] = v`, `m[k] min= v`, `m[k] max= v` | see [Subscripting](#subscripting) |

```praxis
var ages = Map[Text, Int]()
ages["ada"] = 36
ages["alan"] = 41
ages["ada"] += 1

out(ages)
out(ages["ada"])
out(ages.len())
out(ages.contains("alan"))
out(ages.keys())
out(ages.values())

match ages.get("grace") {
    Some(n) => out(n)
    None => out("no entry for grace")
}

ages.remove("alan")
out(ages)
```

```text
{ada: 37, alan: 41}
37
2
true
[ada, alan]
[37, 41]
no entry for grace
{ada: 37}
```

`keys()` and `values()` share one ordering, so they are index-aligned. To get
both halves joined, iterate the map or call `to_vec()`, which answers the
`(K, V)` pairs.

## `Set`

| method | answer |
|---|---|
| `insert(T)` | `Unit` |
| `remove(T)` | `Unit` — a no-op if absent |
| `contains(T)` | `Bool` |
| `len()`, `is_empty()` | `Int`, `Bool` |

```praxis
var seen = Set()
seen.insert(3)
seen.insert(1)
seen.insert(3)

out(seen)
out(seen.len())
out(seen.contains(1))
out(seen.contains(2))

seen.remove(1)
out(seen)
out(seen.is_empty())
```

```text
{1, 3}
2
true
false
{3}
false
```

There are no set operations — no union, intersection or difference. A `filter`
over one and `to_set()` on the result is the spelling.

## `Counter`

A `Counter[T]` is a map from `T` to `Int` whose absent values read as zero.

| method | answer |
|---|---|
| `get(T)` | `Int` — zero if absent, never faults |
| `inc(T)` | `Unit` — add one |
| `len()`, `is_empty()` | `Int`, `Bool` — `len` counts distinct keys |
| `keys()` | `Vec[T]` |
| `values()` | `Vec[Int]` |
| `c[k]`, `c[k] = n` | see [Subscripting](#subscripting) |

```praxis
var counts = Counter[Text]()
for word in ["the", "cat", "the", "sat"] {
    counts[word] += 1
}
counts.inc("cat")

out(counts)
out(counts["the"])
out(counts.len())
out(counts.keys())
out(counts.values())

// An absent key reads as zero, does not fault, and creates nothing.
out(counts["dog"])
out(counts.len())

// A store is not zero-defaulting: it sets the count outright, and a stored
// zero is an entry like any other.
counts["dog"] = 0
out(counts.len())
out(counts)
```

```text
{cat: 2, sat: 1, the: 2}
2
3
[cat, sat, the]
[2, 1, 2]
0
3
4
{cat: 2, dog: 0, sat: 1, the: 2}
```

Reading an absent key does not create it — `len()` is 3 before and after
`counts["dog"]`. Storing one does, even a zero. If you want a counter built
from a sequence in one call, `frequencies()` is it; see
[pipelines](pipelines.md).

## `MinHeap` and `MaxHeap`

| method | answer |
|---|---|
| `push(T)` | `Unit` |
| `pop()` | `T` — smallest (`MinHeap`) or largest (`MaxHeap`); faults if empty |
| `peek()` | `T` — the same element, not removed; faults if empty |
| `len()`, `is_empty()` | `Int`, `Bool` |

```praxis
var lo = MinHeap()
for n in [5, 1, 3] {
    lo.push(n)
}
out(lo.peek())
out(lo.pop())
out(lo.pop())
out(lo.len())

var hi = MaxHeap[Int]()
for n in [5, 1, 3] {
    hi.push(n)
}
out(hi.peek())

// Walking a heap does not drain it: the loop reads a snapshot in pop order.
for n in hi {
    out(n)
}
out(hi.len())
out(hi.is_empty())
```

```text
1
1
3
1
5
5
3
1
3
false
```

**A heap's element type must be orderable, and no composite is.** Only `Int`,
`UInt`, `Byte`, `Float`, `Char` and `Text` are ordered
([ADR-045](../../../decisions/045-ordering-semantics-and-the-compare-callback.md)),
so the Dijkstra habit of pushing a `(cost, node)` pair does not compile:

```praxis
var frontier = MinHeap()
frontier.push((3, "b"))
```

```console
$ praxis check heap-of-pairs.px --color never
error[Y006]: values of type `(Int, Text)` cannot be ordered

  heap-of-pairs.px:2:10
  2 | frontier.push((3, "b"))
    |          ^^^^ values of type `(Int, Text)` cannot be ordered

praxis: 1 error(s)
```

The requirement rides on the *receiver's type*, not on `push`, so it is enforced
wherever the heap's element type gets pinned. For weighted shortest paths, reach
for the `dijkstra` helper in [grids and graphs](grid-and-graphs.md) instead of
building the frontier by hand.

## `BitSet`

A compact set of non-negative `Int`s. Members are bit positions, not objects, so
there is no element type and `BitSet[Int]()` is an arity error.

| method | answer |
|---|---|
| `insert(Int)` | `Unit` — faults on a negative or oversized member |
| `remove(Int)` | `Unit` |
| `contains(Int)` | `Bool` |
| `len()` | `Int` — the popcount |
| `is_empty()` | `Bool` |

```praxis
var bits = BitSet()
bits.insert(64)
bits.insert(1)
bits.insert(300)
bits.insert(1)

out(bits)
out(bits.len())
out(bits.contains(64))
out(bits.contains(65))

// Members come out in ascending numeric order.
for n in bits {
    out(n)
}

bits.remove(64)
out(bits)
out(bits.is_empty())
```

```text
{1, 64, 300}
3
true
false
1
64
300
{1, 300}
false
```

A member outside `0..=4294967295` faults with `size or extent out of range`.

## `Range`

`a..b` is the integers from `a` up to but not including `b`; `a..=b` includes
`b`. Both build the same value — the inclusive form is normalized into its
half-open equivalent, which is why `1..=4` prints as `1..5`
([ADR-059](../../../decisions/059-a-range-is-a-value-and-a-descending-one-is-empty.md)).

**A `Range` has no methods at all**, and no subscript:

```text
error[Y110]: no method `len` on type `Range` taking 0 argument(s)
```

What it has is iteration, and therefore every [pipeline](pipelines.md) stage —
which is where `count()` and `sum()` below come from.

```praxis
out(1..4)
out(1..=4)

for i in 1..4 {
    out(i)
}

out((1..4).count())
out((1..4).sum())

// A descending range is empty, not a countdown. The countdown is a barrier.
out((5..0).count())
out((0..5).reversed())

// A Range has no mutator, so it is usable as a Map key.
var spans = Map()
spans[1..4] = "first three"
out(spans[1..4])
```

```text
1..4
1..5
1
2
3
3
6
0
[4, 3, 2, 1, 0]
first three
```

A descending range is empty rather than reversed, matching Python and Rust.
There is no step or stride form, and `5..0` earns no diagnostic — it is a legal
empty collection, and the constructor clamps rather than the literal being
refused
([ADR-059](../../../decisions/059-a-range-is-a-value-and-a-descending-one-is-empty.md)).

The countdown is written `(0..5).reversed()`, which answers a `Vec[Int]` because
a pipeline's currency is `Vec` — not a descending `Range`, since no such value
exists. See [barriers](pipelines.md#barriers) for why reversal needs the whole
sequence.

## Tuples

A tuple is an anonymous positional product: `(a, b)`, elements read as `.0`,
`.1`, and so on. Its identity is structural — the element type sequence alone —
so two `(Int, Int)`s built anywhere in the program compare and hash as the same
shape ([ADR-026](../../../decisions/026-structural-equality-hashing.md)).

```praxis
var point = (3, 4)
out(point)
out(point.0)
out(point.1)

// A tuple's identity is structural: same elements, same value.
out((3, 4) == point)
out((4, 3) == point)

// Mixed element types are fine, and the arity is part of the shape.
var row = ("ada", 36, true)
out(row.1)

// Tuples key a Map, which is what makes (x, y) coordinates work.
var grid = Map()
grid[(0, 0)] = "start"
grid[(3, 4)] = "goal"
out(grid[point])
out(grid.len())
```

```text
(3, 4)
3
4
true
false
36
goal
2
```

A tuple element is **not** an assignable place: `t.0 = 1` is
`error[Y021]: the left side of an assignment must be a name, a field, or an
index`. And no tuple is orderable, so a `Vec` of pairs has no `sorted()` —
`sorted_by_key(|p| p.1)` is the spelling, and [pattern matching](pattern-matching.md)
is how a pair gets destructured into names.

## Iteration and its order

Every collection is iterable, and so is a `Text`. The order is fixed and
seed-independent — a program's answer never depends on a hash table's
per-process seed.

| receiver | `for` yields | order | how |
|---|---|---|---|
| `Vec[T]` | `T` | index order | walked in place |
| `Deque[T]` | `T` | front to back | walked in place |
| `Range` | `Int` | ascending | walked in place |
| `Text` | `Char` | by Unicode scalar | walked in place |
| `Set[T]` | `T` | ascending by member | one snapshot |
| `BitSet` | `Int` | ascending numerically | one snapshot |
| `MinHeap[T]` | `T` | pop order (ascending) | one snapshot |
| `MaxHeap[T]` | `T` | pop order (descending) | one snapshot |
| `Grid[T]` | `T` | cells, row-major | one snapshot |
| `Map[K, V]` | `(K, V)` | ascending by key | two aligned snapshots |
| `Counter[T]` | `(T, Int)` | ascending by key | two aligned snapshots |

"Ascending" is the **value's** order, not the printed text's: numeric for `Int`,
`Byte` and `Float`, code-point for `Char` and `Text`, `false` before `true`, and
element-wise left to right for a tuple, a record or an enum. It is the same
order `sorted()` uses, so `out(s)` and `out(s.to_vec().sorted())` print the same
sequence, and a `Map[(Int, Int), V]` over a grid comes out in reading order
([ADR-138](../../../decisions/138-a-container-orders-by-the-value-and-not-by-its-printing.md)).

Every key type has such an order, including the ones you cannot write `<` on: a
tuple orders inside a container and `(1, 2) < (1, 3)` is still refused at check
time. Ordering a container is a question about determinism; `<` is a question
about the language.

```praxis
// A hashed collection walks its members in the *value's* order, not in the
// order they print: 2 before 10, and the same sequence on every run.
var seen = Set()
for n in [1, 2, 10, 20, 3] {
    seen.insert(n)
}
out(seen)
for n in seen {
    out(n)
}

// keys() and values() share that one order, so they are index-aligned.
var m = Map()
m[1] = "one"
m[10] = "ten"
m[2] = "two"
out(m.keys())
out(m.values())

// A `for` over a keyed collection yields the (key, value) pair itself.
for kv in m {
    out(kv.1)
}

// A keyed collection prints in the order it iterates: one order, not two.
var names = Map()
names["a"] = 1
names["a1"] = 2
out(names)
out(names.keys())

// A tuple key orders element-wise, left to right — which is what makes a
// Map[(Int, Int), V] over a grid come out in reading order.
var grid = Map()
grid[(1, 10)] = "b"
grid[(1, 9)] = "a"
grid[(0, 100)] = "z"
out(grid)
```

```text
{1, 2, 3, 10, 20}
1
2
3
10
20
[1, 2, 10]
[one, two, ten]
one
two
ten
{a: 1, a1: 2}
[a, a1]
{(0, 100): z, (1, 9): a, (1, 10): b}
```

A keyed collection prints in the order it iterates. That used not to be true:
printing sorted the whole rendered entry, so `a1: 2` came before `a: 1` (because
`1` sorts before `:`), while `keys()`, `values()` and a `for` sorted the key
alone and answered `a` before `a1`. One `Map` had two orders, and a program that
printed it and walked it disagreed with itself. There is now one order, and the
table's column is it.

The last column of the table is not decoration. A collection walked *in place*
re-reads its length each step, so a `push` from inside the loop body is visited;
a snapshotted one is materialized once before the first step and cannot be
affected by the body at all
([ADR-066](../../../decisions/066-a-for-iterates-a-snapshot.md)).

```praxis
// A Vec indexes itself, so a `for` over it re-reads the length each step and
// sees an element pushed by the body.
var v = [1, 2, 3]
for x in v {
    if x == 1 {
        v.push(99)
    }
    out(x)
}

// A Set does not: the loop walks a snapshot taken once, before the first step.
var s = Set()
s.insert(1)
for x in s {
    s.insert(2)
    out(x)
}
out(s)
```

```text
1
2
3
99
1
{1, 2}
```

## What may be a `Map` key or a `Set` element

A key must be **hashable and immutable**
([ADR-057](../../../decisions/057-a-capability-requirement-rides-on-the-scheme-that-quantified-it.md)).
The rule is mutability, not container-ness:

- **In:** every scalar, `Text`, `Range`, and — structurally — tuples, records
  and enums, each a key exactly when all of its components are.
- **Out:** `Vec`, `Deque`, `Map`, `Set`, `Counter`, `MinHeap`, `MaxHeap`,
  `BitSet`, `Grid`, and any function.

```praxis
struct Point { x: Int, y: Int }

enum Dir { North, South }

// A key type is fixed by the first use, so each of these is its own Map.
var by_int = Map()
by_int[7] = "an Int"

var by_text = Map()
by_text["k"] = "a Text"

var by_tuple = Map()
by_tuple[(1, 2)] = "a tuple of Ints"

var by_record = Map()
by_record[Point { x: 1, y: 2 }] = "a record of Ints"

var by_enum = Map()
by_enum[North] = "an enum with no payload"

var by_range = Map()
by_range[1..4] = "a Range"

out(by_int[7])
out(by_text["k"])
out(by_tuple[(1, 2)])
out(by_record[Point { x: 1, y: 2 }])
out(by_enum[North])
out(by_range[1..4])
```

```text
an Int
a Text
a tuple of Ints
a record of Ints
an enum with no payload
a Range
```

A mutable one is refused at check time, at the operation that stores it, in
concrete terms:

```praxis
var seen = Set()
seen.insert([1, 2])
```

```console
$ praxis check mutable-key.px --color never
error[Y014]: a value of type `Vec[Int]` can change after it is stored, so it cannot be used as a key

  mutable-key.px:2:6
  2 | seen.insert([1, 2])
    |      ^^^^^^ a value of type `Vec[Int]` can change after it is stored, so it cannot be used as a key

help: use a value that cannot change — a number, `Text`, or a tuple of those

praxis: 1 error(s)
```

The requirement is on the collection's *key type* rather than on `insert`, so it
reaches through an unannotated parameter too. Given
`fn store(m, k) { m.insert(k, 1) }`, a call `store(m, [1, 2])` reports the same
`Y014` at the `insert` inside `store`, naming the `Vec[Int]` the call site put
in `k`'s place.

### The one hole in the rule

A record is accepted as a key, and a record field *is* assignable. Mutating a
field of a key already stored moves the entry's bucket without moving the entry:

```praxis
struct Point { x: Int, y: Int }

var p = Point { x: 1, y: 2 }
var seen = Set()
seen.insert(p)
out(seen.contains(p))

// A field store changes the value the set hashed. The entry is still there —
// it is just no longer reachable by its own key.
p.x = 99
out(seen.contains(p))
out(seen.len())
out(seen)
```

```text
true
false
1
{{ x: 99, y: 2 }}
```

The entry is still in the set and still prints, and nothing will ever find it
again. Build a fresh record instead of storing into one you have used as a key.
This is the exact hazard the mutable-container rule exists to prevent; the
compiler does not catch it for records.

## Conversions, and what is not here

Every collection converts to eight of the ten, by naming what it becomes:
`to_vec`, `to_set`, `to_map`, `to_counter`, `to_deque`, `to_min_heap`,
`to_max_heap`, `to_bitset`. These are pipeline sinks and are documented in
[pipelines](pipelines.md), along with the stages that get you there. Two notes
belong here:

- The two missing ones are `Grid` and `Range`. There is no `to_grid()` — a grid
  needs a width and an item sequence does not carry one — and no `to_range()`,
  because a `Range` is written `a..b` rather than built from members.
- The conversion has to typecheck: `to_map` needs an item that is a `(K, V)`
  pair and `to_counter` a `(T, Int)` pair, so `[1, 2, 2].to_counter()` reports
  `expected (?T, Int), found Int`. `frequencies()` is the call that *counts*.
- Two conversions leave the collections entirely and answer a `Text`:
  `seq.join(sep)` on a sequence of `Text`, and `chars.to_text()` on a
  `Vec[Char]`. They are two rows rather than one because a generic `join` and a
  `Char`-specific one cannot both exist under one name — the reasoning is in
  [ADR-144](../../../decisions/144-a-sequence-of-text-joins-and-a-sequence-of-char-becomes-one.md).

Also absent, and a reader coming from Python or Rust will look for them:
`Vec.contains` / `pop` / `insert` / `remove`, in-place `reverse` and `sort`
(`reversed()` and `sorted()` answer new `Vec`s instead), set algebra,
`Deque.rotate`, and a `Range` with a step. Each is a `Y110` naming the receiver
and the arity, so the compiler says which method on which type it could not
find rather than guessing:

```text
error[Y110]: no method `contains` on type `Vec[Int]` taking 1 argument(s)
```
