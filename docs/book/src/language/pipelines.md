# Pipelines

A pipeline is a chain of method calls that walks a sequence of values: a
**source**, some **stages** that transform the elements one at a time, and a
**sink** that answers with a single value. The compiler knows every one of these
methods by name and compiles a chain of them into a single loop over the source.

Two things distinguish it from the iterator chains you may be used to. It is
**eager** — it runs where it is written, not where its result is consumed. And
it **materializes on its own** — a chain that stops without a sink is already a
`Vec`, so there is no `collect`.

## A chain, from source to sink

```praxis
// The shape of every pipeline: a source, some stages, and a sink.
fn main() {
    var readings = [3, -1, 4, 1, -5, 9, 2, 6]

    // A sink ends the chain and answers a scalar.
    out(readings.filter(|x| x > 0).map(|x| x * x).sum())

    // The same chain, one stage per line. A leading `.` continues the
    // expression across the newline that would otherwise end the statement.
    var answer = readings
        .filter(|x| x > 0)
        .map(|x| x * x)
        .sum()
    out(answer)

    // No sink: the chain still ends, and what it ends as is a Vec.
    out(readings.filter(|x| x > 0).map(|x| x * x))

    // `count` has two arities: every element, or the matching ones.
    out(readings.count())
    out(readings.count(|x| x < 0))
}
```

```text
147
147
[9, 16, 1, 81, 4, 36]
8
2
```

The arguments are ordinary [closures](functions.md). Nothing about a closure
changes because it is written inside a chain, and a closure bound to a variable
works as well as one written in place.

## A pipeline's source is the `for` loop's

The receiver of a pipeline is what a `for` loop walks, and it yields exactly
what the loop's variable would bind ([ADR-127](../../../decisions/127-a-pipelines-source-is-the-for-loops-and-a-collection-converts-by-naming-what-it-becomes.md)).
There are ten of them, one short of the `for` loop's own list:

| Receiver | Item |
| --- | --- |
| `Vec[T]`, `Deque[T]`, `Set[T]`, `MinHeap[T]`, `MaxHeap[T]` | `T` |
| `Range`, `BitSet` | `Int` |
| `Text` | `Char` |
| `Map[K, V]` | `(K, V)` |
| `Counter[T]` | `(T, Int)` |

```praxis
// A pipeline's item is the `for` loop's variable. The same source, twice.
fn main() {
    var m = Map()
    m["a"] = 1
    m["b"] = 2

    var loop_total = 0
    for kv in m {
        loop_total = loop_total + kv.1
    }
    out(loop_total)
    out(m.map(|kv| kv.1).sum())

    // Same order, too. Both walks read the same deterministic snapshot.
    for kv in m {
        out(kv.0)
    }
    out(m.map(|kv| kv.0))
}
```

```text
3
3
a
b
[a, b]
```

Iteration order is deterministic and seed-independent for every receiver — a
`Set` walked twice in one run, or in two runs, yields the same order — so a
pipeline's answer is a function of its input alone.

Here is one chain over each of the ten:

```praxis
// One chain over each of the ten things a pipeline can start from.
fn main() {
    // Vec[T] and Deque[T] yield T.
    out([3, 1, 2].map(|x| x * 10).sum())

    var d = Deque()
    d.push_back(10)
    d.push_front(20)
    out(d.to_vec())

    // Set[T], MinHeap[T] and MaxHeap[T] yield T, from a snapshot in a
    // deterministic order.
    var s = Set()
    s.insert(3)
    s.insert(1)
    s.insert(2)
    out(s.filter(|x| x > 1).sorted())

    var lo = MinHeap()
    lo.push(5)
    lo.push(2)
    out(lo.sum())

    var hi = MaxHeap()
    hi.push(5)
    hi.push(2)
    out(hi.max())

    // Range and BitSet yield Int.
    out((1..6).map(|n| n * n).sum())

    var bits = BitSet()
    bits.insert(2)
    bits.insert(5)
    out(bits.to_vec())

    // Text yields Char — the same value `t[i]` answers.
    out("mississippi".count(|c| c == "s"[0]))
    out("hello".filter(|c| c != "l"[0]).to_vec())

    // Map[K, V] yields (K, V), and Counter[T] yields (T, Int).
    var m = Map()
    m["a"] = 1
    m["b"] = 2
    out(m.map(|kv| kv.1).sum())
    out(m.to_vec())

    var tally = ["the", "cat", "the", "dog", "the"].frequencies()
    out(tally.filter(|(word, n)| n > 1).map(|(word, n)| word))
}
```

```text
60
[20, 10]
[2, 3]
7
5
55
[2, 5]
4
[h, e, o]
3
[(a, 1), (b, 2)]
[the]
```

`Grid[T]` is the one iterable that is *not* a pipeline receiver: `grid.map(fn)`
is reserved for the shape-preserving row that answers a `Grid`, and that row is
not implemented yet. A grid enters a pipeline through `grid.cells()` or
`grid.positions()`, which already answer `Vec`s — see
[grids and graphs](grid-and-graphs.md).

## The catalog

Everything below is a row in the [method catalog](method-catalog.md), keyed by
receiver, name and arity — one row per combinator, shared by all ten receivers.
`T` is the item type; `U`, `K`, `V` and `Acc` are fresh.

**Streaming stages.** Each transforms one element at a time and answers a `Vec`.

| Stage | Argument | Result |
| --- | --- | --- |
| `map(f)` | `(T) -> U` | `Vec[U]` |
| `filter(p)` | `(T) -> Bool` | `Vec[T]` |
| `filter_map(f)` | `(T) -> Option[U]` | `Vec[U]` |
| `flat_map(f)` | `(T) -> Vec[U]` | `Vec[U]` |
| `take(n)` | `Int` | `Vec[T]` |
| `skip(n)` | `Int` | `Vec[T]` |
| `take_while(p)` | `(T) -> Bool` | `Vec[T]` |
| `enumerate()` | — | `Vec[(Int, T)]` |
| `zip(other)` | `Vec[U]` | `Vec[(T, U)]` |

**Sinks.** Each ends the chain with one value.

| Sink | Argument | Result |
| --- | --- | --- |
| `sum()` | — | `Int` — the item must be `Int` |
| `product()` | — | `Int` — the item must be `Int` |
| `count()` | — | `Int` |
| `count(p)` | `(T) -> Bool` | `Int` |
| `min()`, `max()` | — | `Int` — the item must be `Int`; faults on an empty sequence |
| `min_by(lt)`, `max_by(lt)` | `(T, T) -> Bool` | `T` — faults on an empty sequence |
| `any(p)`, `all(p)` | `(T) -> Bool` | `Bool` |
| `find(p)` | `(T) -> Bool` | `Option[T]` |
| `position(p)` | `(T) -> Bool` | `Option[Int]` |
| `fold(init, f)` | `Acc`, `(Acc, T) -> Acc` | `Acc` |
| `reduce(f)` | `(T, T) -> T` | `T` — faults on an empty sequence |

**Conversions.** Also sinks, so they fuse into the same loop.

| Conversion | Result |
| --- | --- |
| `to_vec()` | `Vec[T]` |
| `to_set()` | `Set[T]` |
| `to_map()` | `Map[K, V]` — the item must be a `(K, V)` pair |
| `to_counter()` | `Counter[T]` — the item must be a `(T, Int)` pair |
| `to_deque()` | `Deque[T]` |
| `to_min_heap()`, `to_max_heap()` | `MinHeap[T]`, `MaxHeap[T]` |
| `to_bitset()` | `BitSet` — the item must be `Int` |

**Barriers.** These need the whole sequence before they can answer their first
element, so they are not fused: a chain ends at one and begins again from its
result.

| Barrier | Argument | Result |
| --- | --- | --- |
| `sorted()` | — | `Vec[T]` — `T` must be orderable |
| `sorted_by_key(f)` | `(T) -> K` | `Vec[T]` — `K` must be orderable |
| `unique()` | — | `Vec[T]`, in first-occurrence order |
| `reversed()` | — | `Vec[T]`, back to front — no requirement on `T` |
| `frequencies()` | — | `Counter[T]` |
| `join(sep)` | `Text` | `Text` — the items must be `Text` |

`reversed` is the barrier with an empty requirement column, and that is its own
claim rather than an omission: `sorted` reads the element's `compare` callback
and `unique` reads its `hash` and `equals`, while reversal reads nothing at all
— so a `Vec` of closures reverses where it cannot be sorted
([ADR-145](../../../decisions/145-a-reversal-needs-the-whole-sequence-so-it-is-a-barrier.md)).
It is also what a countdown is written with: `for i in (0..n).reversed()`, since
`n..0` is an empty range rather than a descending one.

`join` is the one barrier that answers a scalar rather than a sequence. Its
separator is a required argument, because the catalog has no optional ones —
`join("")` is the no-separator spelling and says so where it is written — and it
renders nothing: `[1, 2].join(",")` is `expected Text, found Int`, and the
spelling is `[1, 2].map(|n| n.to_text()).join(",")`. A sequence of `Char` uses
`to_text()` instead, which is a `Vec[Char]` row rather than a pipeline one
([ADR-144](../../../decisions/144-a-sequence-of-text-joins-and-a-sequence-of-char-becomes-one.md)).

`chunks` and `windows` are not implemented. Both answer `Vec[Vec[T]]`, and what
descriptor the outer vector carries is an open question.

`sum`, `product`, `min` and `max` are `Int` operations, not generically numeric.
`[1.5, 2.5].sum()` is `error[Y001]: expected Int, found Float`. For anything
else, `min_by` and `max_by` take a "less-than" comparator and work at any
element type.

## Each stage counts its own input

A stage cannot see the source. It sees what the stage before it handed it, and
that is what `take`, `skip`, `enumerate`, `zip` and `position` count
([ADR-071](../../../decisions/071-a-pipeline-chain-is-nested-and-each-stage-counts-its-own-input.md)).

```praxis
// Every stage that asks "which element is this?" counts its own input, not
// the source's.
fn main() {
    var v = [1, 2, 3, 4, 5, 6, 7, 8]
    var evens = |x| x % 2 == 0

    // The first two survivors, not the survivors among the first two.
    out(v.filter(evens).take(2))
    out(v.filter(evens).skip(1))

    // 0, 1, 2, 3 — the numbering the filter handed on.
    out(v.filter(evens).enumerate())

    // Paired with the argument's 0th, 1st, 2nd element.
    out(v.filter(evens).zip(["a", "b", "c"]))

    // The index among the evens: 6 is the third one.
    out(v.filter(evens).position(|x| x == 6))

    // A splice flattens, and the count keeps running across it.
    out(v.take(3).flat_map(|x| [x, x * 10]).enumerate())
}
```

```text
[2, 4]
[4, 6, 8]
[(0, 2), (1, 4), (2, 6), (3, 8)]
[(2, a), (4, b), (6, c)]
Some(2)
[(0, 1), (1, 10), (2, 2), (3, 20), (4, 3), (5, 30)]
```

The bound `take` and `skip` take is an ordinary `Int` expression, evaluated once
before the loop. Degenerate bounds mean what they read as: `take(0)` and
`take(-1)` are empty, `skip(-1)` drops nothing.

`zip`'s argument and `flat_map`'s closure result are `Vec`s specifically, not
the ten receivers. The fused loop indexes both directly, so `v.zip(s)` on a
`Set` is a type error at the argument and the spelling is `v.zip(s.to_vec())`.

## `fold` carries an accumulator

`fold` is the sink for anything the fixed ones do not cover. The accumulator can
be any type, including a [record](records.md) or a collection.

```praxis
// `fold` threads an accumulator of any type through the chain.
struct Run {
    best: Int
    total: Int
    count: Int
}

fn main() {
    var xs = [3, 9, 4, 1, 12, 7]

    // A record accumulator: three answers from one pass.
    var r = xs.fold(Run { best: 0, total: 0, count: 0 }, |acc, x| Run {
        best: max(acc.best, x),
        total: acc.total + x,
        count: acc.count + 1,
    })
    out(r.best)
    out(r.total)
    out(r.count)

    // A Vec accumulator: a running total, one entry per element. A collection
    // is a reference, so the closure hands the same one back each step.
    var running = xs.fold([0], |acc, x| {
        acc.push(acc[acc.len() - 1] + x)
        acc
    })
    out(running)

    // `reduce` is `fold` seeded with the first element, so it has no answer
    // for an empty sequence and faults instead of inventing one.
    out(xs.reduce(|a, b| max(a, b)))
}
```

```text
12
36
6
[0, 3, 12, 16, 17, 29, 36]
12
```

`reduce` is `fold` without the seed: its accumulator *is* the element type, and
the first element starts it.

## Searching answers an `Option`

`find` answers the matching element, `position` answers its index, and a miss is
`None` in both cases
([ADR-082](../../../decisions/082-find-answers-the-element-and-a-miss-is-none.md)).
The result is the ordinary [`Option`](enums.md) enum, so a
[`match`](pattern-matching.md) is how you read it.

```praxis
// `find` answers the element, `position` answers the index, and a miss is None.
fn main() {
    var words = ["alpha", "beta", "gamma"]

    match words.find(|w| w.len() == 4) {
        Some(w) => out("found " + w)
        None => out("nothing that long")
    }

    match words.position(|w| w.len() == 4) {
        Some(i) => out(i)
        None => out(-1)
    }

    // The sentinel problem an Option removes: -1 is a perfectly ordinary
    // element and a perfectly ordinary index.
    var v = [10, -1, 30]
    out(v.find(|x| x < 0))
    out(v.find(|x| x > 100))

    // `any` and `all` stop as soon as the answer is decided.
    out(v.any(|x| x < 0))
    out(v.all(|x| x < 0))
}
```

```text
found beta
1
Some(-1)
None
true
false
```

The `Option` is what retires a sentinel that used to be in band. `-1` is a legal
element of a `Vec[Int]` and a legal index of nothing, so a `find` that answered
`-1` on a miss could not tell `[10, -1, 30]`'s first negative from no match at
all.

## An empty sequence

Most sinks have a right answer for an empty source, and give it:

```praxis
// The sinks that have a right answer for an empty sequence.
fn main() {
    var scores: Vec[Int] = []
    out(scores.sum())
    out(scores.product())
    out(scores.count())
    out(scores.any(|x| x > 0))
    out(scores.all(|x| x > 0))
    out(scores.find(|x| x > 0))
    out(scores.position(|x| x > 0))
    out(scores.fold(100, |acc, x| acc + x))
    out(scores.map(|x| x * 2))
}
```

```text
0
1
0
false
true
None
None
100
[]
```

Five do not: `min`, `max`, `min_by`, `max_by` and `reduce` all derive their
answer from an element, and an empty sequence has none. They **fault**, and they
deliberately do not answer `None`
([ADR-076](../../../decisions/076-absence-is-an-option-and-an-empty-min-is-a-fault.md)):
an empty `min` is a mistake in the program, where a `find` that matches nothing
is ordinary domain absence. A `0` would be worse than either — it is below every
element of `[3, 4]` and above every element of `[-3, -4]`, and nothing at the
call site could tell it from a real minimum.

```praxis
// An empty `min` has no answer, and the fault says so.
fn main() {
    var scores: Vec[Int] = []
    out(scores.min())
}
```

```text
error: program faulted: empty collection

Backtrace:
#0   main

  locals:
    scores: Vec[Int] = []
  temps:
    <tmp#1: Vec[Int]> @ "[]" = []
    <tmp#3: Int> = 0
    <tmp#4: Int> = 0
    <tmp#8: Unit> @ "out(scores.min())" = <uninit>
```

That is what `praxis run --debug never` prints. With a terminal and the default
`--debug auto`, the same fault opens the [crash
debugger](../debugger/entering.md) at the failing frame instead.

## A pipeline's currency is `Vec`

Every streaming stage answers a `Vec`, whatever the source was. `set.filter(p)`
is a `Vec[T]`, not a `Set[T]`. A program that wants a collection back says which
one:

```praxis
// A pipeline's currency is Vec. To get a collection back, name it.
fn main() {
    var s = Set()
    s.insert(3)
    s.insert(1)
    s.insert(2)

    out(s.filter(|x| x > 1))
    out(s.filter(|x| x > 1).to_set().len())

    // to_vec is the route out of a keyed collection: keys() and values()
    // answer two aligned halves and nothing joins them.
    var m = Map()
    m["a"] = 1
    m["b"] = 2
    out(m.to_vec())

    // ...and to_map is the route back in.
    var scaled = m.map(|kv| (kv.0, kv.1 * 10)).to_map()
    out(scaled["b"])

    // The rest of the set, one per collection that has a constructor.
    out([1, 2, 3].map(|x| x % 2).to_set().len())
    out([3, 1, 2].to_deque().pop_front())
    out([3, 1, 2].to_min_heap().pop())
    out([3, 1, 2].to_max_heap().pop())
    out([1, 4].to_bitset().contains(4))
    out(["a", "b", "a"].frequencies().to_vec().to_counter()["a"])

    // On a Vec receiver, to_vec is the identity — the same reference, not a
    // copy.
    var v = [1]
    var same = v.to_vec()
    same.push(2)
    out(v.len())
}
```

```text
[2, 3]
2
[(a, 1), (b, 2)]
20
2
3
1
3
true
2
2
```

One rule instead of a rule per collection, and it is answerable without knowing
which receiver you are on. The alternative was `filter` returning the receiver's
own type where a row happened to exist and a `Vec` otherwise, which nobody can
hold in their head — the reasoning is in
[ADR-127](../../../decisions/127-a-pipelines-source-is-the-for-loops-and-a-collection-converts-by-naming-what-it-becomes.md)
decision 6.

`to_map` and `to_counter` say "a pair" in their receiver, so a mismatch is a
type error at the method name rather than a fault later: `[1, 2].to_map()` is
`error[Y001]: expected (?T, ?U), found Int`. `to_bitset` says `Int` the same
way, and still faults on a negative or oversized member, which is a value
question no type can answer.

There is no `to_grid()`: a grid needs a width, and a flat item sequence does not
carry one.

## Barriers

```praxis
// The six combinators that need the whole sequence before they answer.
fn main() {
    var v = [3, 1, 4, 1, 5, 9, 2, 6, 5]

    out(v.sorted())
    out(v.unique())
    out(v.reversed())
    out(v.frequencies())
    out(["a", "b", "c"].join(", "))

    // A countdown is a reversed range: `5..0` is empty, not descending.
    out((0..5).reversed())

    // A chain ends at a barrier and begins again from its result.
    out(v.filter(|x| x > 2).sorted().take(3))

    // A pair is not orderable, so a Counter orders by an extracted key.
    var tally = ["the", "cat", "the", "dog", "the", "cat"].frequencies()
    out(tally.to_vec().sorted_by_key(|p| 0 - p.1))

    // A barrier takes any source, not only a Vec.
    var names = Set()
    names.insert("bb")
    names.insert("a")
    names.insert("ccc")
    out(names.sorted())
    out(names.sorted_by_key(|t| t.len()))
}
```

```text
[1, 1, 2, 3, 4, 5, 5, 6, 9]
[3, 1, 4, 5, 9, 2, 6]
[5, 6, 2, 9, 5, 1, 4, 1, 3]
{1: 2, 2: 1, 3: 1, 4: 1, 5: 2, 6: 1, 9: 1}
a, b, c
[4, 3, 2, 1, 0]
[3, 4, 5]
[(the, 3), (cat, 2), (dog, 1)]
[a, bb, ccc]
[a, bb, ccc]
```

`sorted_by_key` exists because a keyed collection cannot order its own items. No
composite type is orderable
([ADR-045](../../../decisions/045-ordering-semantics-and-the-compare-callback.md)),
so the moment a pipeline's item is a pair — the moment its source is a `Map` or
a `Counter` — `sorted` is unavailable:

```praxis
// A pipeline whose item is a pair has no `sorted`.
fn main() {
    var m = Map()
    m["a"] = 1
    out(m.to_vec().sorted())
}
```

```text
error[Y006]: values of type `(Text, Int)` cannot be ordered

  pair-not-orderable.px:5:20
  5 |     out(m.to_vec().sorted())
    |                    ^^^^^^ values of type `(Text, Int)` cannot be ordered

praxis: 1 error(s)
```

The closure extracts an orderable key from an item that is not one, so the
ordering requirement moves off the element and onto the key. There is still no
reverse flag on `sorted`, but there are now two ways to write a descending sort:
`0 - p.1` as the key, or `sorted().reversed()`. The first is one pass and the
second is two, which is the whole difference.

`unique()` and `to_set()` answer different questions: `unique` keeps
first-occurrence order in a `Vec`, and a `Set` has no order to preserve.
`frequencies()` and `to_counter()` are the two directions of the same type
change — `frequencies` *counts* occurrences of each element, `to_counter`
*assigns* the count each pair already carries.

## What to unlearn, coming from Rust

**There is no laziness.** A pipeline runs at the point it is written. There is no
adaptor object, no `impl Iterator`, nothing to hold and consume later.

```praxis
// A pipeline runs where it is written. Nothing waits for a consumer.
fn seen(x) {
    out(x)
    x * 2
}

fn main() {
    var v = [1, 2, 3]
    var mapped = v.map(|x| seen(x))
    out("--- the map is already finished ---")
    out(mapped)
}
```

```text
1
2
3
--- the map is already finished ---
[2, 4, 6]
```

**There is no `collect`.** A chain that ends on a streaming stage materializes
anyway, so the word named a step the compiler was taking whether or not you
wrote it ([ADR-126](../../../decisions/126-a-pipeline-materializes-and-collect-named-a-step-it-takes-anyway.md)).
The method does not exist:

```praxis
// `collect` is not a method. A chain materializes without being told to.
fn main() {
    var v = [1, 2, 3]
    out(v.map(|x| x * 2).collect())
}
```

```text
error[Y110]: no method `collect` on type `Vec[Int]` taking 0 argument(s)

  no-collect.px:4:26
  4 |     out(v.map(|x| x * 2).collect())
    |                          ^^^^^^^ no method `collect` on type `Vec[Int]` taking 0 argument(s)

praxis: 1 error(s)
```

`to_vec()` is not `collect` under another name. On a `Vec` it is the identity,
and it answers the same reference rather than a copy; on the other nine
receivers it is a real conversion, because nothing a `Set` or a `Map` holds is a
`Vec` until something asks for one.

**The chain is still one loop.** Eager does not mean a `Vec` per stage:
`v.map(f).filter(p).sum()` compiles to a single loop with no intermediate
allocation ([ADR-029](../../../decisions/029-pipeline-fusion.md)). A stage or
sink that stops the stream therefore stops the whole loop, which is observable
when a stage has a side effect:

```praxis
// The whole chain is one loop over the source, so a stage or sink that stops
// the stream stops the loop.
fn seen(x) {
    out(x)
    x
}

fn main() {
    var v = [1, 2, 3, 4, 5]

    // `any` answers as soon as it can, and the map behind it stops with it.
    out(v.map(|x| seen(x)).any(|x| x > 1))
    out("---")

    // `take` stops when it meets the element after the last one it keeps, so
    // the stage in front of it runs once more than it keeps.
    out(v.map(|x| seen(x)).take(2))
}
```

```text
1
2
true
---
1
2
3
[1, 2]
```

A source that indexes itself — `Vec`, `Deque`, `Range`, `Text` — is walked in
place. The rest are snapshotted once before the loop, which is what a `for` over
them already does.

**A pipeline is not an expression type.** There is no `Seq[T]` you can annotate,
pass to a function or store in a record: `var xs: Seq[Int] = []` is reported as
`N002`, an unknown type. The value between two stages does not exist at run
time, and the value at the end of a chain is an ordinary one — a `Vec`, the
[collection](collections.md) a conversion named, or a scalar.

**`Grid.map` is not the pipeline's `map`.** `Grid` is deliberately outside the
ten receivers so the name stays free for the shape-preserving version that
answers a `Grid`, and that version is not implemented: `g.map(|c| c)` is `Y110`
today.
