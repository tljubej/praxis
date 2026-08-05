# The method catalog

Every method in Praxis is a row in one table. There are 136 of them, they live
in `crates/praxis-stdlib/src/builtins.rs`, and there is no way to add a 137th
from a program: the language has no `impl`, no traits, no extension methods, and
a record carries fields but no methods. This chapter is that table.

The closedness is load-bearing rather than a limitation the compiler tolerates.
Because the catalog is the complete method universe, a name it does not carry at
that arity can never resolve against *any* receiver — so `fn f(x) { x.nope() }`
is refused at `check` time, before anything has said what `x` is
([ADR-093](../../../decisions/093-a-method-that-cannot-resolve-is-reported-at-check.md)).

## How to read these tables

**Method** is the row's name and the type pattern of each parameter; **Result**
is its result pattern. Both are rendered the way the compiler prints a type:
`T`, `U`, `K`, `V` and `Acc` are type variables, two occurrences of one name in
a row are the same type, `(Int, Int)` is a tuple, and `(T) -> Bool` is a closure
parameter. A nullary collection prints bare, so it is `BitSet` and not
`BitSet[]`.

**Mutates** is the row's purity flag. The impure rows are exactly the ones that
change the receiver, and the flag is visible from a program: the crash
debugger's `p` expression evaluator refuses a call to an impure method, because
a debugger that mutates a faulted state cannot resume it.

```praxis
var v = [1, 2, 3]
out(v[9])
```

That faults, and the debugger it drops into will evaluate one of these two calls
and not the other:

```text
error: program faulted: index out of bounds

Backtrace:
#0   <entry>

  locals:
    v: Vec[Int] = [1, 2, 3]
  temps:
    <tmp#1: Vec[Int]> @ "[1, 2, 3]" = [1, 2, 3]
    <tmp#2: Int> @ "1" = 1
    <tmp#3: Unit> = Unit
    <tmp#4: Int> @ "2" = 2
    <tmp#5: Unit> = Unit
    <tmp#6: Int> @ "3" = 3
    <tmp#7: Unit> = Unit
    <tmp#9: Int> @ "9" = 9
    <tmp#10: Int> @ "v[9]" = Unit
    <tmp#11: Unit> @ "out(v[9])" = <uninit>
    <tmp#12: Unit> @ "var v = [1, 2, 3] out(v[9])" = <uninit>
Entered crash debugger. 1 frame(s). Type `help` for commands.
Praxis crash> p v.len()
3
Praxis crash> p v.push(4)
error: method `push` is impure (may mutate state) — `p` rejects mutating expressions
Praxis crash> quit
```

**Faults** is whether the call can raise a runtime fault. For a row backed by a
runtime wrapper this is the wrapper's own declaration in the ABI manifest, which
is what puts a fault check after the call — so "yes" means the check is emitted,
not that you are likely to trigger it. `Vec.push` says yes for a type-mismatch
case a well-typed program cannot reach; `Vec.get` says yes because indexing off
the end is the everyday one.

**Allocates** is whether the call may allocate, and therefore whether its call
site is a garbage-collection safepoint. It is derived from the same manifest
row, which is why `len()` says yes on every collection: the count comes back as
a freshly boxed `Int`.

Neither flag is restated per method — both are read off the wrapper the row
lowers to, so a row cannot disagree with the code it calls. Thirty-one of the
thirty-five pipeline rows have no wrapper to read: the compiler fuses them into
the loop, so their tables below carry no **Allocates** column and their
**Faults** column is what the fused code does rather than a manifest row.

## Sequence collections

### `Vec[T]`

| Method | Result | Mutates | Faults | Allocates | What it does |
|---|---|---|---|---|---|
| `get(Int)` | `T` | no | yes | no | The element at `index`; faults `IndexOutOfBounds` if out of range. |
| `is_empty()` | `Bool` | no | no | no | True iff the vector has no elements. |
| `len()` | `Int` | no | no | yes | Number of elements in the vector. |
| `push(T)` | `Unit` | yes | yes | yes | Append a value to the end; returns Unit. |

```praxis
var v = [10, 20, 30]

out(v.len())
out(v.is_empty())
out(v.get(1))

v.push(40)
out(v)
```

```text
3
false
20
[10, 20, 30, 40]
```

`push` is the only way a vector grows. `v[v.len()] = x` is an
`IndexOutOfBounds` fault and not an append.

### `Deque[T]`

| Method | Result | Mutates | Faults | Allocates | What it does |
|---|---|---|---|---|---|
| `get(Int)` | `T` | no | yes | no | The element at `index` (0-based from the front); faults if out of range. |
| `is_empty()` | `Bool` | no | no | no | True iff the deque has no elements. |
| `len()` | `Int` | no | no | yes | Number of elements in the deque. |
| `pop_back()` | `T` | yes | yes | no | Remove and return the back element; faults if empty. |
| `pop_front()` | `T` | yes | yes | no | Remove and return the front element; faults if empty. |
| `push_back(T)` | `Unit` | yes | yes | yes | Append a value to the back; returns Unit. |
| `push_front(T)` | `Unit` | yes | yes | yes | Prepend a value to the front; returns Unit. |

```praxis
var d = Deque()
d.push_back(2)
d.push_back(3)
d.push_front(1)

out(d)
out(d.len())
out(d.get(0))
out(d.pop_front())
out(d.pop_back())
out(d.is_empty())
```

```text
[1, 2, 3]
3
1
1
3
false
```

Index 0 is the front, whichever end you have been pushing to.

## Keyed collections

### `Map[K, V]`

| Method | Result | Mutates | Faults | Allocates | What it does |
|---|---|---|---|---|---|
| `contains(K)` | `Bool` | no | no | no | True iff `key` is present in the map. |
| `get(K)` | `Option[V]` | no | no | yes | The value for `key` as `Some(value)`, or `None` if absent. |
| `insert(K, V)` | `Unit` | yes | no | yes | Set `key` to `value`, replacing any prior value; returns Unit. |
| `is_empty()` | `Bool` | no | no | no | True iff the map has no entries. |
| `keys()` | `Vec[K]` | no | no | yes | Every key, as a `Vec[K]`, ordered with `values()`. |
| `len()` | `Int` | no | no | yes | Number of entries in the map. |
| `remove(K)` | `Unit` | yes | no | no | Remove `key` if present; returns Unit. |
| `values()` | `Vec[V]` | no | no | yes | Every value, as a `Vec[V]`, ordered with `keys()`. |

```praxis
var m = Map()
m.insert("a", 1)
m.insert("b", 2)

out(m.len())
out(m.contains("a"))
out(m.get("a"))
out(m.get("z"))
out(m.keys())
out(m.values())

m.remove("a")
out(m)
out(m.is_empty())
```

```text
2
true
Some(1)
None
[a, b]
[1, 2]
{b: 2}
false
```

`get` answers an `Option`; `m[key]` faults on a missing key. Those are the two
halves of one question and the spelling picks which you meant.

`keys()` and `values()` answer `Vec`s in a fixed, deterministic order — by the
key's rendered form — and the two are index-aligned, so `keys()[i]` and
`values()[i]` belong together. To get both at once, walk the map: `for kv in m`
and the pipeline rows below both yield `(K, V)` pairs, and `m.to_vec()` is the
`Vec[(K, V)]`.

### `Set[T]`

| Method | Result | Mutates | Faults | Allocates | What it does |
|---|---|---|---|---|---|
| `contains(T)` | `Bool` | no | no | no | True iff `value` is in the set. |
| `insert(T)` | `Unit` | yes | no | yes | Add `value` to the set; returns Unit. |
| `is_empty()` | `Bool` | no | no | no | True iff the set has no elements. |
| `len()` | `Int` | no | no | yes | Number of elements in the set. |
| `remove(T)` | `Unit` | yes | no | no | Remove `value` if present; returns Unit. |

```praxis
var s = Set()
s.insert(1)
s.insert(1)
s.insert(2)

out(s.len())
out(s.contains(2))

s.remove(2)
out(s)
out(s.is_empty())
```

```text
2
true
{1}
false
```

### `Counter[T]`

| Method | Result | Mutates | Faults | Allocates | What it does |
|---|---|---|---|---|---|
| `get(T)` | `Int` | no | no | yes | The count for `key`, or zero if absent (never faults). |
| `inc(T)` | `Unit` | yes | yes | yes | Increment the count for `key` by one; returns Unit. |
| `is_empty()` | `Bool` | no | no | no | True iff the counter has no keys. |
| `keys()` | `Vec[T]` | no | no | yes | Every key, as a `Vec[T]`, ordered with `values()`. |
| `len()` | `Int` | no | no | yes | Number of distinct keys in the counter. |
| `values()` | `Vec[Int]` | no | no | yes | Every count, as a `Vec[Int]`, ordered with `keys()`. |

```praxis
var c = Counter()
c.inc("x")
c.inc("x")
c.inc("y")

out(c.get("x"))
out(c.get("never seen"))
out(c.len())
out(c.keys())
out(c.values())
out(c.is_empty())
```

```text
2
0
2
[x, y]
[2, 1]
false
```

A `Counter` is the collection whose absent values read as zero, so `get` and
`c[key]` never fault and `len()` counts the keys that were actually touched.

## Priority queues and bit sets

### `MinHeap[T]`

| Method | Result | Mutates | Faults | Allocates | What it does |
|---|---|---|---|---|---|
| `is_empty()` | `Bool` | no | no | no | True iff the min-heap has no elements. |
| `len()` | `Int` | no | no | yes | Number of elements in the min-heap. |
| `peek()` | `T` | no | yes | no | The smallest element without removing it; faults if empty. |
| `pop()` | `T` | yes | yes | no | Remove and return the smallest element; faults if empty. |
| `push(T)` | `Unit` | yes | no | yes | Push a value onto the min-heap; returns Unit. |

### `MaxHeap[T]`

| Method | Result | Mutates | Faults | Allocates | What it does |
|---|---|---|---|---|---|
| `is_empty()` | `Bool` | no | no | no | True iff the max-heap has no elements. |
| `len()` | `Int` | no | no | yes | Number of elements in the max-heap. |
| `peek()` | `T` | no | yes | no | The largest element without removing it; faults if empty. |
| `pop()` | `T` | yes | yes | no | Remove and return the largest element; faults if empty. |
| `push(T)` | `Unit` | yes | no | yes | Push a value onto the max-heap; returns Unit. |

```praxis
var lo = MinHeap()
lo.push(5)
lo.push(1)
lo.push(3)

out(lo.len())
out(lo.peek())
out(lo.pop())
out(lo.is_empty())

var hi = MaxHeap()
hi.push(5)
hi.push(1)

out(hi.peek())
out(hi.pop())
```

```text
3
1
1
false
5
5
```

`peek` is pure and `pop` is not, which is the only difference between them in
this table and the whole difference at the call site.

### `BitSet`

| Method | Result | Mutates | Faults | Allocates | What it does |
|---|---|---|---|---|---|
| `contains(Int)` | `Bool` | no | no | no | True iff the bit for the integer is set. |
| `insert(Int)` | `Unit` | yes | yes | yes | Set the bit for a non-negative integer; returns Unit. |
| `is_empty()` | `Bool` | no | no | no | True iff no bits are set. |
| `len()` | `Int` | no | no | yes | Number of set bits (popcount). |
| `remove(Int)` | `Unit` | yes | no | no | Clear the bit for an integer; returns Unit. |

```praxis
var b = BitSet()
b.insert(3)
b.insert(70)

out(b.contains(3))
out(b.contains(4))
out(b.len())

b.remove(3)
out(b)
out(b.is_empty())
```

```text
true
false
2
{70}
false
```

`insert` faults on a negative or oversized member; `remove` does not, because
clearing a bit that was never in range is not a question the set has to answer.
`contains` is the one row in the catalog that lowers to a dedicated
scalar-producing instruction rather than a call, which is why it is not a
safepoint
([ADR-118](../../../decisions/118-a-vecs-three-words-are-the-compilers-to-read.md)).

## `Grid[T]`

| Method | Result | Mutates | Faults | Allocates | What it does |
|---|---|---|---|---|---|
| `cells()` | `Vec[T]` | no | no | yes | All cells in row-major order, as a Vec. |
| `column(Int)` | `Vec[T]` | no | yes | yes | Column `x` as a Vec; faults if out of range. |
| `contains(Int, Int)` | `Bool` | no | no | no | True iff (x, y) is within the grid. |
| `find(T)` | `Option[(Int, Int)]` | no | no | yes | The first (x, y) whose cell equals `value` as `Some((x, y))`, or `None`. |
| `find_all(T)` | `Vec[(Int, Int)]` | no | no | yes | All (x, y) positions whose cell equals `value`, as a Vec. |
| `get(Int, Int)` | `T` | no | yes | no | The cell at (x, y); faults if out of range. |
| `height()` | `Int` | no | no | yes | The number of rows. |
| `neighbors4((Int, Int))` | `Vec[(Int, Int)]` | no | no | yes | The 4 orthogonal in-bounds neighbors of a point, as a Vec of (x, y). |
| `neighbors8((Int, Int))` | `Vec[(Int, Int)]` | no | no | yes | The 8 in-bounds neighbors of a point, as a Vec of (x, y). |
| `positions()` | `Vec[(Int, Int)]` | no | no | yes | All (x, y) positions in row-major order, as a Vec. |
| `rotate_left()` | `Grid[T]` | no | no | yes | A copy rotated 90° counter-clockwise. |
| `rotate_right()` | `Grid[T]` | no | no | yes | A copy rotated 90° clockwise. |
| `row(Int)` | `Vec[T]` | no | yes | yes | Row `y` as a Vec; faults if out of range. |
| `set(Int, Int, T)` | `Unit` | yes | yes | no | Set the cell at (x, y); faults if out of range. |
| `transpose()` | `Grid[T]` | no | no | yes | A transposed copy (rows ↔ columns). |
| `width()` | `Int` | no | no | yes | The number of columns. |

A grid is indexed `(x, y)` with `x` the column and `y` the row, and
`positions()`, `cells()` and `find_all()` walk it in row-major order.

```praxis
var g = read grid(one_of(".#"))
var wall = "#"[0]

out(g.width())
out(g.height())
out(g.get(1, 0))
out(g.contains(3, 0))
out(g.row(0))
out(g.column(1))
out(g.cells())
out(g.positions().len())
out(g.neighbors4((1, 1)))
out(g.neighbors8((0, 0)))
out(g.find(wall))
out(g.find_all(wall))
out(g.transpose().row(0))
out(g.rotate_left().row(0))
out(g.rotate_right().row(0))

g.set(0, 0, wall)
out(g.row(0))
```

with input

```text
.#.
..#
```

```text
3
2
#
false
[., #, .]
[#, .]
[., #, ., ., ., #]
6
[(1, 0), (0, 1), (2, 1)]
[(1, 0), (0, 1), (1, 1)]
Some((1, 0))
[(1, 0), (2, 1)]
[., .]
[., #]
[., .]
[#, #, .]
```

`neighbors4` and `neighbors8` return only the in-bounds neighbours, so they are
already the neighbour function a graph walk wants. `transpose`, `rotate_left`
and `rotate_right` answer copies and leave the receiver alone.

**A `Grid` is deliberately not a pipeline receiver.** `for cell in g` walks it
in row-major order, but `g.map(f)` is a `Y110`: a grid enters a pipeline through
`cells()` or `positions()`, which already answer `Vec`s. The exclusion is what
leaves the name `map` free for a future shape-preserving `Grid[T] -> Grid[U]`
row
([ADR-127](../../../decisions/127-a-pipelines-source-is-the-for-loops-and-a-collection-converts-by-naming-what-it-becomes.md)).

## The pipeline

Thirty-five rows — thirty-four names, because `count` has two arities — sit on
one generic receiver that stands for **ten different receivers**: `Vec`,
`Deque`, `Set`, `MinHeap`, `MaxHeap`, `Range`, `BitSet`, `Map`, `Counter` and
`Text`. That is the `for` loop's list minus `Grid`, and what a receiver yields
here is exactly what the `for` loop's variable would bind — a `Char` from a
`Text`, a `(K, V)` pair from a `Map` or a `Counter`, an element from everything
else.

```praxis
var s = Set()
s.insert(2)
s.insert(1)
out(s.sorted())

var m = Map()
m.insert("a", 1)
m.insert("b", 2)
out(m.map(|pair| pair.0))
out(m.to_vec())

var c = Counter()
c.inc("x")
out(c.to_vec())

var d = Deque()
d.push_back(7)
out(d.sum())

var lo = MinHeap()
lo.push(2)
lo.push(1)
out(lo.to_vec())

var b = BitSet()
b.insert(4)
b.insert(9)
out(b.sum())

out((0..5).sum())
out((0..=5).sum())
out("abc".map(|ch| ch.to_int()))
```

```text
[1, 2]
[a, b]
[(a, 1), (b, 2)]
[(x, 1)]
7
[1, 2]
13
10
15
[97, 98, 99]
```

**A pipeline's currency is `Vec`.** Every stage answers one whatever the
receiver was, which is what makes "what does `filter` return" answerable without
knowing what you started from. A program that wants a different collection back
says which one, with a `to_*` row.

The **Requires** column below is the row's own constraint on the item type. Its
wording is the compiler's: an unorderable element gets `Y006 values of type ...
cannot be ordered`, and one that cannot be a key gets `Y014 a value of type ...
can change after it is stored`.

### Stages

| Method | Result | Requires | Faults | What it does |
|---|---|---|---|---|
| `enumerate()` | `Vec[(Int, T)]` | — | no | Pair each element with its index. |
| `filter((T) -> Bool)` | `Vec[T]` | — | no | Keep elements satisfying a predicate, collecting into a Vec. |
| `filter_map((T) -> Option[U])` | `Vec[U]` | — | no | Map each element to an Option and keep the Some payloads. |
| `flat_map((T) -> Vec[U])` | `Vec[U]` | — | no | Map each element to a Vec and concatenate the results. |
| `frequencies()` | `Counter[T]` | items usable as keys | no | A Counter holding how many times each element occurs. |
| `map((T) -> U)` | `Vec[U]` | — | no | Apply a function to each element, collecting into a Vec. |
| `skip(Int)` | `Vec[T]` | — | no | Drop the first n elements. |
| `sorted()` | `Vec[T]` | items orderable | yes | A new Vec holding these elements in ascending order. |
| `sorted_by_key((T) -> K)` | `Vec[T]` | the extracted key is orderable | yes | A new Vec ordered by the key the closure extracts. |
| `take(Int)` | `Vec[T]` | — | no | Keep at most the first n elements. |
| `take_while((T) -> Bool)` | `Vec[T]` | — | no | Keep elements until the predicate is false. |
| `unique()` | `Vec[T]` | items usable as keys | no | A new Vec with duplicate elements removed, keeping first occurrences. |
| `zip(Vec[U])` | `Vec[(T, U)]` | — | no | Pair elements with another sequence, stopping at the shorter length. |

`sorted`, `sorted_by_key`, `unique` and `frequencies` are **barriers**: each
needs the whole sequence before it can answer anything, so each is a call into
the runtime rather than a stage the compiler folds into the loop. That is
invisible from a program except in what it costs — the other stages are fused
into a single pass over the source
([ADR-126](../../../decisions/126-a-pipeline-materializes-and-collect-named-a-step-it-takes-anyway.md)),
which is also what the "Faults" column is measuring here: a fused stage has no
wrapper of its own to fault, while `sorted` and `sorted_by_key` do — and
`sorted_by_key`'s also propagates whatever the key closure raised.

### Sinks

| Method | Result | Requires | Faults | What it does |
|---|---|---|---|---|
| `all((T) -> Bool)` | `Bool` | — | no | True if all elements satisfy the predicate (short-circuits). |
| `any((T) -> Bool)` | `Bool` | — | no | True if any element satisfies the predicate (short-circuits). |
| `count()` | `Int` | — | no | Number of elements. |
| `count((T) -> Bool)` | `Int` | — | no | Number of elements satisfying the predicate. |
| `find((T) -> Bool)` | `Option[T]` | — | no | The first matching element, or None. |
| `fold(Acc, (Acc, T) -> Acc)` | `Acc` | — | no | Reduce elements left-to-right with an accumulator and combining closure. |
| `max()` | `Int` | items are `Int` | yes | Largest (Int) element. Faults on an empty sequence. |
| `max_by((T, T) -> Bool)` | `T` | — | yes | Largest element per a `(T, T) -> Bool` "less-than" comparator. |
| `min()` | `Int` | items are `Int` | yes | Smallest (Int) element. Faults on an empty sequence. |
| `min_by((T, T) -> Bool)` | `T` | — | yes | Smallest element per a `(T, T) -> Bool` "less-than" comparator. |
| `position((T) -> Bool)` | `Option[Int]` | — | no | The index of the first matching element, or None. |
| `product()` | `Int` | items are `Int` | yes | Multiply the (Int) elements. |
| `reduce((Acc, T) -> Acc)` | `T` | — | yes | Reduce left-to-right, seeded with the first element. |
| `sum()` | `Int` | items are `Int` | yes | Sum the (Int) elements. |

`count` is the one name in the catalog that carries two arities on a *single*
receiver — `count()` is the element count, `count(pred)` the matching-element
count — which the table's `(receiver, name, arity)` key has always allowed.
(`get`, `contains` and `[]` also appear at two arities, but split across
receivers: one argument on a `Vec`, two on a `Grid`.)

`min`/`max` are `Int` sinks and `min_by`/`max_by` take a "less-than" comparator
and work on anything. `find` answers the element, `position` the index, and both
answer an `Option`.

The seven faulting sinks fault for two reasons and no others. `min`, `max`,
`min_by`, `max_by` and `reduce` raise **empty collection** on an empty sequence:
each has to answer with an element and there is none. `sum` and `product` raise
**integer overflow**, because the running total is checked arithmetic like every
other `+` and `*`. `fold` is the sink that does *not* fault on an empty
sequence — it answers its seed — which is the reason to reach for it over
`reduce`.

```praxis
var v: Vec[Int] = Vec()
out(v.min())
```

```text
error: program faulted: empty collection

Backtrace:
#0   <entry>

  locals:
    v: Vec[Int] = []
  temps:
    <tmp#1: Vec[Int]> = []
    <tmp#3: Int> = 0
    <tmp#4: Int> = 0
    <tmp#6: Unit> = Unit
    <tmp#8: Unit> @ "out(v.min())" = <uninit>
    <tmp#9: Unit> @ "var v: Vec[Int] = Vec() out(v.min())" = <uninit>
```

### Conversions

| Method | Result | Requires | Faults | What it does |
|---|---|---|---|---|
| `to_bitset()` | `BitSet` | items are `Int` | yes | A BitSet holding these (Int) items. Faults on a negative or oversized member. |
| `to_counter()` | `Counter[T]` | items are `(T, Int)` pairs; `T` usable as a key | no | A Counter built from (key, count) pairs. Duplicate keys: last wins. |
| `to_deque()` | `Deque[T]` | — | no | A Deque holding these items, in order. |
| `to_map()` | `Map[K, V]` | items are `(K, V)` pairs; `K` usable as a key | no | A Map built from (key, value) pairs. Duplicate keys: last wins. |
| `to_max_heap()` | `MaxHeap[T]` | items orderable | no | A MaxHeap holding these items. |
| `to_min_heap()` | `MinHeap[T]` | items orderable | no | A MinHeap holding these items. |
| `to_set()` | `Set[T]` | items usable as keys | no | A Set holding these items, duplicates dropped. |
| `to_vec()` | `Vec[T]` | — | no | The items as a Vec. On a Vec receiver this is the receiver itself. |

There is a conversion for every collection that has a constructor, and exactly
one that has none: `to_grid` does not exist, because a grid needs a width and a
flat item sequence does not carry one.

`to_map` and `to_counter` say "my item is a pair" in the receiver pattern rather
than in prose, so `[1, 2].to_map()` fails at the method name with `expected (?T,
?U), found Int` instead of resolving and then faulting.

### The whole pipeline, run

```praxis
var v = [3, 1, 4, 1, 5]

out(v.map(|n| n * 2))
out(v.filter(|n| n > 2))
out(v.filter_map(|n| if n > 3 { Some(n) } else { None }))
out(v.flat_map(|n| [n, n]))
out(v.take(2))
out(v.skip(3))
out(v.take_while(|n| n < 4))
out(v.enumerate())
out(v.zip(["a", "b"]))

out(v.fold(0, |acc, n| acc + n))
out(v.reduce(|acc, n| acc + n))
out(v.sum())
out(v.product())
out(v.count())
out(v.count(|n| n == 1))
out(v.min())
out(v.max())
out(v.min_by(|a, b| a < b))
out(v.max_by(|a, b| a < b))
out(v.any(|n| n == 4))
out(v.all(|n| n > 0))
out(v.find(|n| n > 3))
out(v.position(|n| n > 3))

out(v.sorted())
out(v.sorted_by_key(|n| 0 - n))
out(v.unique())
out(v.frequencies())

out(v.to_vec())
out(v.to_set())
out(v.to_deque())
out(v.to_min_heap().peek())
out(v.to_max_heap().peek())
out(v.to_bitset())
out([(1, "a"), (2, "b")].to_map())
out([("x", 3)].to_counter())
```

```text
[6, 2, 8, 2, 10]
[3, 4, 5]
[4, 5]
[3, 3, 1, 1, 4, 4, 1, 1, 5, 5]
[3, 1]
[1, 5]
[3, 1]
[(0, 3), (1, 1), (2, 4), (3, 1), (4, 5)]
[(3, a), (1, b)]
14
14
14
60
5
2
1
5
1
5
true
true
Some(4)
Some(2)
[1, 1, 3, 4, 5]
[5, 4, 3, 1, 1]
[3, 1, 4, 5]
{1: 2, 3: 1, 4: 1, 5: 1}
[3, 1, 4, 1, 5]
{1, 3, 4, 5}
[3, 1, 4, 1, 5]
1
5
{1, 3, 4, 5}
{1: a, 2: b}
{x: 3}
```

### What the Requires column refuses

A tuple can be a key but cannot be ordered — no composite in this language can,
because ordering goes through one scalar comparison
([ADR-045](../../../decisions/045-ordering-semantics-and-the-compare-callback.md)).
A record behaves exactly the same way: a fine key, not orderable. A `Vec` is
neither one nor the other: not orderable, for the same composite reason, and not
a key, because it can change after it has been stored. Below, the tuple fails the
first column and the `Vec` fails the second.

```praxis
var pairs = [(2, "b"), (1, "a")]
out(pairs.sorted())

var groups = [[1], [2]]
out(groups.to_set())
```

```console
$ praxis check catalog-bounds.px
error[Y006]: values of type `(Int, Text)` cannot be ordered

  catalog-bounds.px:2:11
  2 | out(pairs.sorted())
    |           ^^^^^^ values of type `(Int, Text)` cannot be ordered

error[Y014]: a value of type `Vec[Int]` can change after it is stored, so it cannot be used as a key

  catalog-bounds.px:5:12
  5 | out(groups.to_set())
    |            ^^^^^^ a value of type `Vec[Int]` can change after it is stored, so it cannot be used as a key

help: use a value that cannot change — a number, `Text`, or a tuple of those

praxis: 2 error(s)
```

`sorted_by_key` is the answer to the first half: the ordering requirement moves
to the key the closure extracts, so the elements themselves need not be
orderable.

```praxis
var pairs = [(2, "b"), (1, "a")]
out(pairs.sorted_by_key(|pair| pair.0))
```

```text
[(1, a), (2, b)]
```

## Scalars

`Text` is the one scalar with members, and `Int`, `Float` and `Char` have the
conversions and the explicit-overflow family.

### `Text`

| Method | Result | Mutates | Faults | Allocates | What it does |
|---|---|---|---|---|---|
| `get(Int)` | `Char` | no | yes | yes | The `Char` at `index`; faults if out of range. `t[index]` is the same row and the same answer. |
| `is_empty()` | `Bool` | no | no | no | True iff the text has no chars. |
| `len()` | `Int` | no | no | yes | Number of Unicode scalar values (chars) in the text. |

```praxis
var line = "héllo"

out(line.len())
out(line.is_empty())
out(line.get(1))
out(line.get(1).to_int())
out((233).to_char())
```

```text
5
false
é
233
é
```

`len()` counts Unicode scalar values and `get`/`t[i]` index by them, not by
bytes — which is why `"héllo".len()` is 5 and `line.get(1)` is `é`. `Char.to_int`
and `Int.to_char` are the round trip out of and back into a character, and they
are `Int` and `Char` rows rather than `Text` ones.

There is no `split`, no `chars` and no `to_upper`: all three are `Y110`. `for ch
in text` is how a `Text` is walked, and the pipeline rows above apply to it
directly
([ADR-099](../../../decisions/099-a-list-literal-is-a-vec-and-a-text-is-iterable.md)).

### `Int`

| Method | Result | Mutates | Faults | Allocates | What it does |
|---|---|---|---|---|---|
| `checked_add(Int)` | `Option[Int]` | no | no | yes | Add, answering None where the checked `+` would fault. |
| `checked_mul(Int)` | `Option[Int]` | no | no | yes | Multiply, answering None where the checked `*` would fault. |
| `checked_sub(Int)` | `Option[Int]` | no | no | yes | Subtract, answering None where the checked `-` would fault. |
| `saturating_add(Int)` | `Int` | no | no | yes | Add, clamping to Int's ends instead of faulting. |
| `saturating_mul(Int)` | `Int` | no | no | yes | Multiply, clamping to Int's ends instead of faulting. |
| `saturating_sub(Int)` | `Int` | no | no | yes | Subtract, clamping to Int's ends instead of faulting. |
| `to_char()` | `Char` | no | yes | yes | The `Char` with this Unicode scalar value; **faults** (`InvalidChar`) if it is negative, above `0x10FFFF`, or a surrogate. The narrowing half of the pair, as `Float.to_int` is. |
| `to_float()` | `Float` | no | no | yes | Widen to `Float`; the explicit Int→Float conversion. |
| `wrapping_add(Int)` | `Int` | no | no | yes | Add with two's-complement wraparound instead of a fault. |
| `wrapping_mul(Int)` | `Int` | no | no | yes | Multiply with two's-complement wraparound instead of a fault. The one row here a program could not write for itself: every arithmetic operator is checked and the language has no bitwise operators. |
| `wrapping_sub(Int)` | `Int` | no | no | yes | Subtract with two's-complement wraparound instead of a fault. |

### `Float`

| Method | Result | Mutates | Faults | Allocates | What it does |
|---|---|---|---|---|---|
| `abs()` | `Float` | no | no | yes | Absolute value. |
| `ceil()` | `Float` | no | no | yes | Round toward positive infinity. |
| `floor()` | `Float` | no | no | yes | Round toward negative infinity. |
| `is_infinite()` | `Bool` | no | no | no | True iff ±infinity. |
| `is_nan()` | `Bool` | no | no | no | True iff NaN. |
| `max(Float)` | `Float` | no | no | yes | The larger of two floats. If either is NaN, returns the other. |
| `min(Float)` | `Float` | no | no | yes | The smaller of two floats. If either is NaN, returns the other. |
| `round()` | `Float` | no | no | yes | Round half away from zero. |
| `sign()` | `Float` | no | no | yes | Sign as -1.0 / 0.0 / 1.0. NaN yields NaN. |
| `sqrt()` | `Float` | no | no | yes | Square root. Negative inputs yield NaN (IEEE-754). |
| `to_int()` | `Int` | no | yes | yes | Truncate toward zero to an Int. Faults on NaN, ±inf, or out of i64 range. |
| `to_text()` | `Text` | no | no | yes | Format as Text (shortest round-trip form; inf/-inf/NaN as literals). |

### `Char`

| Method | Result | Mutates | Faults | Allocates | What it does |
|---|---|---|---|---|---|
| `to_int()` | `Int` | no | no | yes | The Unicode scalar value, as an `Int`. Never faults. |

All three receivers at once:

```praxis
var x = -2.5

out(x.abs())
out(x.sign())
out(x.floor())
out(x.ceil())
out(x.round())
out(x.to_int())
out(x.to_text())
out(x.min(1.0))
out(x.max(1.0))
out((2.0).sqrt())
out((0.0 / 0.0).is_nan())
out((1.0 / 0.0).is_infinite())

out((7).to_float())
out((9223372036854775807).wrapping_add(1))
out((9223372036854775807).saturating_add(1))
out((9223372036854775807).checked_add(1))
out((5).checked_sub(1))
out((3).wrapping_mul(4))
```

```text
2.5
-1.0
-3.0
-2.0
-3.0
-2
-2.5
-2.5
1.0
1.4142135623730951
true
true
7.0
-9223372036854775808
9223372036854775807
None
Some(4)
12
```

Integer arithmetic is checked by default, and the nine `wrapping_`/`saturating_`
/`checked_` rows are how a program opts out of the fault for one operation.
`Int` has no `to_text()` and neither does `Char`; the `to_text` family is
`Float`'s alone today.

## Subscripts

`m[key]`, `v[i] = x` and `grid[x, y]` are catalog rows too, dispatched on the
receiver's shape and the index count exactly as a method call is. Their names —
`[]`, `[]=`, `[]min=`, `[]max=` — are not identifiers, so no program can call
them by name; the subscript grammar is their only caller
([ADR-064](../../../decisions/064-a-subscript-is-a-catalog-row.md)).

Six receivers read. Five of the six also store: every one but `Text`, which is
immutable.

| Receiver | Spelling | Result | Mutates | Faults | Allocates | What it does |
|---|---|---|---|---|---|---|
| `Counter[T]` | `c[key]` | `Int` | no | no | yes | `c[key]` — the count for `key`, or zero if absent; never faults. |
| `Deque[T]` | `d[i]` | `T` | no | yes | no | `d[i]` — the element at `i` (0-based from the front); faults if out of range. |
| `Grid[T]` | `g[x, y]` | `T` | no | yes | no | `g[x, y]` — the cell at (x, y); faults if out of range. |
| `Map[K, V]` | `m[key]` | `V` | no | yes | no | `m[key]` — the value for `key`; **faults** if absent. |
| `Text` | `t[i]` | `Char` | no | yes | yes | `t[i]` — the `Char` at `i`, indexing by Unicode scalar value and not by byte; faults if out of range. |
| `Vec[T]` | `v[i]` | `T` | no | yes | no | `v[i]` — the element at `i`; faults if out of range. |
| `Counter[T]` | `c[key] = n` | `Unit` | yes | no | yes | `c[key] = n` — set the count for `key`. |
| `Deque[T]` | `d[i] = value` | `Unit` | yes | yes | no | `d[i] = value` — replace the element at `i` (0-based from the front); faults if out of range (it never inserts). |
| `Grid[T]` | `g[x, y] = value` | `Unit` | yes | yes | no | `g[x, y] = value` — set the cell at (x, y); faults if out of range. |
| `Map[K, V]` | `m[key] = value` | `Unit` | yes | no | yes | `m[key] = value` — set `key`, replacing any prior value. |
| `Vec[T]` | `v[i] = value` | `Unit` | yes | yes | no | `v[i] = value` — replace the element at `i`; faults if out of range (it never appends — `push` is the spelling that grows a vector). |
| `Map[K, Int]` | `m[key] max= n` | `Unit` | yes | no | yes | `m[key] max= n` — keep the larger value; an absent entry accepts the first value. |
| `Map[K, Int]` | `m[key] min= n` | `Unit` | yes | no | yes | `m[key] min= n` — keep the smaller value; an absent entry accepts the first value. |

```praxis
var v = [10, 20, 30]
v[0] = 11
out(v[0])

var d = Deque()
d.push_back("a")
d[0] = "b"
out(d[0])

out("praxis"[2])

var m = Map()
m["k"] = 1
out(m["k"])

var c = Counter()
c["x"] = 4
out(c["x"])
out(c["never seen"])

var best = Map()
best["r"] min= 5
best["r"] min= 3
best["r"] max= 4
out(best)
```

```text
11
b
a
1
4
0
{r: 4}
```

`min=` and `max=` exist as their own rows rather than as read-modify-write over
the other two, because they give an absent entry a meaning no read can express:
the first value is accepted as-is. A subscript *read* of an absent `Map` key
faults, so there would be nothing to compare against.

```praxis
var m = Map()
m.insert("a", 1)
out(m["b"])
```

```text
error: program faulted: index out of bounds

Backtrace:
#0   <entry>

  locals:
    m: Map[Text, Int] = {a: 1}
  temps:
    <tmp#1: Map[Text, Int]> = {a: 1}
    <tmp#3: Text> @ ""a"" = a
    <tmp#4: Int> @ "1" = 1
    <tmp#5: Unit> @ "m.insert("a", 1)" = Unit
    <tmp#6: Text> @ ""b"" = b
    <tmp#7: Int> @ "m["b"]" = Unit
    <tmp#8: Unit> @ "out(m["b"])" = <uninit>
    <tmp#9: Unit> @ "var m = Map() m.insert("a", 1) out(m["b"])" = <uninit>
```

## When a method does not resolve

Two diagnostics cover almost everything. `Y110` is "this table has no such row"
— including the wrong argument count, since arity is part of the key, so
`[1, 2].get()` is `no method 'get' on type 'Vec[Int]' taking 0 argument(s)`.
`Y001` is "the row exists and your types do not fit it", which is what the item
shapes produce; the two requirement columns above produce `Y006` and `Y014`
instead.

```praxis
out("a,b".split(","))
out([1.5, 2.5].sum())
out([1, 2].to_map())
```

```console
$ praxis check catalog-refusals.px
error[Y110]: no method `split` on type `Text` taking 1 argument(s)

  catalog-refusals.px:1:11
  1 | out("a,b".split(","))
    |           ^^^^^ no method `split` on type `Text` taking 1 argument(s)

error[Y001]: expected Int, found Float

  catalog-refusals.px:2:16
  2 | out([1.5, 2.5].sum())
    |                ^^^ expected Int, found Float

error[Y001]: expected (?T, ?U), found Int

  catalog-refusals.px:3:12
  3 | out([1, 2].to_map())
    |            ^^^^^^ expected (?T, ?U), found Int

praxis: 3 error(s)
```

The second is why `sum` is spelled as a bound rather than a literal `Vec[Int]`
receiver: the row still *matches* a `Vec[Float]`, so the report is about the
element type you have rather than "no method `sum` on this type".

See [method resolution](../types/method-resolution.md) for how a call finds its
row, and [diagnostic codes](../tooling/diagnostics.md) for the full list.
