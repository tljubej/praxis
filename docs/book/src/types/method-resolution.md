# Method resolution

`receiver.name(args)` is a lookup in one table. The compiler owns a closed
**method catalog** — a list of rows, each with a receiver pattern, a name, a
parameter list and a result — and resolving a call means finding the row whose
receiver pattern matches the receiver's inferred type and whose name and arity
match the call
([ADR-020](../../../decisions/020-method-dispatch-and-collections.md)). There is
no `impl`, no trait, no extension method, and no user-defined method: a record
carries fields and nothing else. The rows themselves are
[the method catalog chapter](../language/method-catalog.md); this one is how a
call finds its row and what happens when it cannot.

The same table answers the language server's completion and signature help, so
what the editor offers on a receiver is exactly what will resolve.

## The receiver's type picks the row

One name can be many rows — the catalog has nine `len` rows and six `get` rows.
Which one you get is decided by the receiver's type and the argument *count*,
before any argument's type is looked at:

```praxis
var text = "hello"
var v = [10, 20]
var m = Map()
m.insert("a", 1)

out(text.len())
out(v.len())
out(m.len())

out(text.get(1))
out(v.get(1))
out(m.get("a"))
out(m.get("z"))
```

```text
5
2
1
e
20
Some(1)
None
```

Three of each are reached here, and the three `get`s do not even agree on a
result type. Ask the compiler:

```praxis
var text = "hello"

var v = [10, 20]

var m = Map()
m.insert("a", 1)

panic("stopping here on purpose")
```

Driven with `type text.get(1)`, `type v.get(0)`, `type m.get("a")`,
`type v.map(|n| n * 2)`, `type m.keys()`, `quit`:

```text
error: program faulted: panic: stopping here on purpose

Backtrace:
#0   <entry>

  locals:
    text: Text = hello
    v: Vec[Int] = [10, 20]
    m: Map[Text, Int] = {a: 1}
  temps:
    <tmp#1: Text> @ ""hello"" = hello
    <tmp#3: Vec[Int]> @ "[10, 20]" = [10, 20]
    <tmp#4: Int> @ "10" = 10
    <tmp#5: Unit> = Unit
    <tmp#6: Int> @ "20" = 20
    <tmp#7: Unit> = Unit
    <tmp#9: Map[Text, Int]> = {a: 1}
    <tmp#11: Text> @ ""a"" = a
    <tmp#12: Int> @ "1" = 1
  …(4 more)
Entered crash debugger. 1 frame(s). Type `help` for commands.
Praxis crash> type text.get(1)
Char
Praxis crash> type v.get(0)
Int
Praxis crash> type m.get("a")
Option[Int]
Praxis crash> type v.map(|n| n * 2)
Vec[Int]
Praxis crash> type m.keys()
Vec[Text]
Praxis crash> quit
```

Arity is part of the key, so `[1, 2].get()` is not "wrong number of arguments"
but "no such row" — `Y110`.

Once the row is found, its receiver pattern, its parameters and its result are
instantiated from **one** shared name map and unified with what the call site
holds. That is why two occurrences of `T` in a row are one type, and why an
argument closure's parameter is already pinned before its body is inferred:
`[[1, 2], [3]].map(|inner| inner.len())` knows `inner` is a `Vec[Int]` — so
`len` on it resolves — because the receiver was bound first.

## `Iterable` is a receiver shape, not a type

Most receiver patterns name a constructor: `Vec[T]`, `Map[K, V]`, `Text`. The
sequence rows do not. Their receiver is written `Iterable[T]`, which stands for
ten receivers — the nine collections `Vec`, `Deque`, `Set`, `MinHeap`,
`MaxHeap`, `Range`, `BitSet`, `Map` and `Counter`, plus `Text`, which walks its
characters — bound to what each of them yields (§5.7). No annotation can name
it, because it is not a type.

It is also the one receiver pattern that is **not unified** with the call site's
type. Unifying `Iterable[T]` against a `Vec` would pin every other constructor
out. What is unified instead is the row's *item*, against the same answer
`for x in receiver` would bind.

```praxis
var v = [1, 2, 3]

var s = Set()
s.insert(5)

var m = Map()
m.insert("a", 1)

out(v.map(|n| n * 2))
out(s.map(|n| n * 2))
out((0..3).map(|n| n * 2))
out(m.map(|entry| entry.0))
```

```text
[2, 4, 6]
[10]
[0, 2, 4]
[a]
```

One row, four receivers, and a `Vec` out of every one of them — a pipeline's
currency is `Vec`, whatever it started as.

The ten are not the `for` loop's list. `Grid` is iterable and is deliberately
not an `Iterable` receiver
([ADR-127](../../../decisions/127-a-pipelines-source-is-the-for-loops-and-a-collection-converts-by-naming-what-it-becomes.md)
decision 1): a generic `map` row would claim the name and answer `Vec[U]`, and
§6.4 wants `grid.map` to be the shape-preserving `Grid[T] -> Grid[U]`. That one
is not written yet, so today a grid has no `map` at all.

```praxis
var g = read grid(char)

out(g.map(|c| c))
```

```console
$ praxis check grid-is-not-a-pipeline.px --color never
error[Y110]: no method `map` on type `Grid[Char]` taking 1 argument(s)

  grid-is-not-a-pipeline.px:3:7
  3 | out(g.map(|c| c))
    |       ^^^ no method `map` on type `Grid[Char]` taking 1 argument(s)

praxis: 1 error(s)
```

A grid enters a pipeline through `grid.cells()` or `grid.positions()`, which
already answer `Vec`s.

Because the constraint is on the item, a row can require a *shape* there.
`Iterable[(K, V)].to_map()` means "a `Map` or a `Counter`", because those are
the two whose item is a pair, and asking it of anything else is a type error at
the method name rather than a missing method:

```praxis
out([1, 2].to_map())
```

```console
$ praxis check iterable-item-must-fit.px --color never
error[Y001]: expected (?T, ?U), found Int

  iterable-item-must-fit.px:1:12
  1 | out([1, 2].to_map())
    |            ^^^^^^ expected (?T, ?U), found Int

praxis: 1 error(s)
```

## A receiver that is not known yet

A method call whose receiver is still a type variable cannot be looked up: a
variable is not a shape the table can be keyed by. The call does not fail —
inference records the requirement (this method, this arity, these arguments,
this result) against the variable and **resolves it later**, when the program
says what the receiver is
([ADR-057](../../../decisions/057-a-capability-requirement-rides-on-the-scheme-that-quantified-it.md)
decision 5).

```praxis
fn top_three(rows) {
    rows.sorted().take(3)
}

out(top_three([5, 9, 1, 7]))
```

```text
[1, 5, 7]
```

`rows` was never annotated. The call site pins it to `Vec[Int]`, discharge looks
`sorted` and `take` up against that, and the unification of the row's result is
what gives the whole chain its type.

Discharge **pins** the receiver to the declaration group's level, so
generalization cannot quantify it. That is why the same function cannot be
called on a `Vec[Int]` and a `Vec[Float]` in one program —
[Generalization](generalization.md) has the reasoning and the diagnostic.

The requirements a receiver's own type carries reach through the same channel.
`fn remember(table, key) { table.insert(key, 1) }` learns that `table` is a
`Map` only at the call, and the key rule is applied there anyway:

```praxis
fn remember(table, key) {
    table.insert(key, 1)
}

var seen = Map()
remember(seen, [3, 4])
```

```console
$ praxis check requirement-through-a-parameter.px --color never
error[Y014]: a value of type `Vec[Int]` can change after it is stored, so it cannot be used as a key

  requirement-through-a-parameter.px:2:11
  2 |     table.insert(key, 1)
    |           ^^^^^^ a value of type `Vec[Int]` can change after it is stored, so it cannot be used as a key

help: use a value that cannot change — a number, `Text`, or a tuple of those

praxis: 1 error(s)
```

That rule is [Capabilities](capabilities.md).

## When it cannot resolve: `Y110`

A call that cannot find a row is `Y110`, reported by inference at
`praxis check` time — not at `run`, and not by lowering
([ADR-093](../../../decisions/093-a-method-that-cannot-resolve-is-reported-at-check.md)).
There is one emitter and it has two wordings.

**The receiver is known.** The message names the type and the arity, and offers
the nearest row this receiver actually has:

```praxis
var v = [1, 2, 3]

out(v.lenn())
```

```console
$ praxis check no-such-method.px --color never
error[Y110]: no method `lenn` on type `Vec[Int]` taking 0 argument(s)

  no-such-method.px:3:7
  3 | out(v.lenn())
    |       ^^^^ no method `lenn` on type `Vec[Int]` taking 0 argument(s)

help: did you mean `len`?
      len

praxis: 1 error(s)
```

The suggestion is drawn from the rows dispatch would have searched — this
receiver's, not the whole catalog's — so `v.lenght()` is never offered a `Map`
method.

**No receiver has that name at all.** Because the catalog is the complete method
universe, a name it does not hold at that arity can never resolve against
anything, so it is refused before the receiver is known:

```praxis
fn describe(thing) {
    thing.frobnicate()
}

out(1)
```

```console
$ praxis check no-method-anywhere.px --color never
error[Y110]: no type has a method `frobnicate` taking 0 argument(s)

  no-method-anywhere.px:2:11
  2 |     thing.frobnicate()
    |           ^^^^^^^^^^ no type has a method `frobnicate` taking 0 argument(s)

praxis: 1 error(s)
```

`describe` is never called and `thing` is never pinned. There is nothing to name
in the sentence, so the wording drops the receiver half rather than printing
`?T` into a message that is supposed to be concrete.

The two wordings divide by whether the receiver is known, not by whether the
call is reached. A name the catalog *does* hold stays deferred — `fn total(values)
{ values.sum() }` with no call site is clean, because `sum` exists on `Vec[T]`
at arity 0 and a later call site may still answer it.

A subscript is a catalog row too, dispatched under the name `[]`, but it has its
own code and wording: `s[0]` on a `Set` is `Y020`, "values of type `Set[Int]`
cannot be indexed with 1 index(es)", rather than a missing method nobody wrote.

## A call has parentheses; a bare dot is a field

`v.len()` is a method call. `v.len` is a **field read**, and it is only that
([ADR-077](../../../decisions/077-a-zero-argument-accessor-is-a-call-and-a-bare-dot-name-is-a-field.md)).
There is no property form, and none of the zero-argument accessors have one:
`grid.width()`, `grid.height()`, `v.len()`, `text.is_empty()`.

```praxis
var v = [1, 2, 3]

out(v.len)
```

```console
$ praxis check accessor-is-a-call.px --color never
error[Y112]: no field `len` on type `Vec[Int]`

  accessor-is-a-call.px:3:7
  3 | out(v.len)
    |       ^^^ no field `len` on type `Vec[Int]`

praxis: 1 error(s)
```

Three reasons the rule is worth having, in the order they bind.

**The two lower differently.** A field read carries a slot *index* taken from
the record's definition and becomes a load; a method call is a catalog row and a
runtime call. Letting one syntax mean either would need a tie-break, and the
only available one is "whichever the receiver happens to have" — so adding a
field to a `struct` whose name matched a catalog row would silently change what
an existing expression does.

**A receiver whose type is not yet known could not tell them apart.** A field
read on an unresolved receiver rides the same deferral channel a method call
does. Under a property form, `fn f(v) { v.len }` would emit a requirement with
two possible discharges — a field of that name, or a nullary row of that name —
and nothing at the read site could choose.

**The catalog is the dispatch table.** A property read would be a second
dispatch surface over the same names, keyed differently, and the language server
would have to render one row two ways.

The consequence is that a record field may be called `len`, and the two spellings
stay apart:

```praxis
struct Reading { len: Int, label: Text }

var r = Reading { len: 7, label: "north" }

out(r.len)
out(r.label)
out(r.label.len())
```

```text
7
north
5
```

`r.len` reads the field. `r.len()` looks for a row on `Reading`, finds none —
records carry no rows — and is `Y110`.
