# Capabilities

Praxis has no `trait`, no `impl`, no `interface` and no `where` clause. It also
has values you can compare, values you can sort, values you can use as a `Map`
key, and values you cannot — and something has to decide which is which. That
something is a closed table inside the compiler: for equality, hashing,
ordering, iteration and arithmetic, the language ships one answer per shape and
no way for a program to add another.

The compiler calls these *capabilities* internally. You will never see the word.
A diagnostic says what the program did and why it cannot work — "values of type
`Point` cannot be ordered" — and never mentions a trait, a bound, or the name of
the requirement. This chapter is what those requirements are and what they look
like when one is not met.

## Equality and hashing are structural

Tuples, records, enums and collections get their `==` and their hash from the
compiler. Nothing is derived and nothing is written down: a composite is
comparable when every component is, recursively, and hashable on exactly the
same terms. Scalars and `Unit` are both; functions and closures are neither.
`[1, 2] == [1, 2]` is `true`, and so is the same comparison between two
separately built `Map`s with the same entries.

```praxis
struct Point { x: Int, y: Int }
enum Move { Step(Int), Stop }

out((1, "a") == (1, "a"))
out((1, "a") == (1, "b"))
out(Point { x: 1, y: 2 } == Point { x: 1, y: 2 })
out(Step(3) == Step(3))
out(Step(3) == Stop)

var seen = Set()
seen.insert(Point { x: 1, y: 2 })
seen.insert(Point { x: 1, y: 2 })
seen.insert(Point { x: 3, y: 4 })
out(seen.len())
```

```text
true
false
true
true
false
2
```

Equality compares contents, not identity: two separately built `Point`s with the
same fields are equal and hash alike, which is what makes the third `insert`
above the only one that grows the set.

Put a function anywhere in the structure and the whole thing stops being
comparable. The report names the **component** that failed, not the type you
wrote:

```praxis
struct Rule { name: Text, apply: (Int) -> Int }

var double = Rule { name: "double", apply: |n| n * 2 }
var triple = Rule { name: "triple", apply: |n| n * 3 }

out(double == triple)
```

```console
$ praxis check compare-functions.px --color never
error[Y004]: values of type `(Int) -> Int` cannot be compared with `==`

  compare-functions.px:6:15
  6 | out(double == triple)
    |               ^^^^^^ values of type `(Int) -> Int` cannot be compared with `==`

praxis: 1 error(s)
```

## Ordering is not structural

Equality recurses; ordering does not. The orderable types are exactly the
scalars `Int`, `UInt`, `Byte`, `Float`, `Char` and `Text`. `Bool` and `Unit` have
no defined order, and no composite has one: not a tuple, not a record, not an
enum, not a collection.

That is a statement about the **source language** — `<`, `sorted()`, a heap
element. A **container** is a different question, and there the answer recurses:
a `Map`, `Set` or `Counter` has to walk and print its keys in one reproducible
sequence, so every type that can be a key has a container order, tuples and
records included, computed element-wise. That order is over the *value* and not
over its printing — a `Set[Int]` walks `2` before `10` — and
[Collections](../language/collections.md) gives it in full. Having one does not
make a type comparable with `<`; the example below stays exactly as it is.

```praxis
struct Point { x: Int, y: Int }

var points = [Point { x: 3, y: 1 }, Point { x: 1, y: 2 }]

out(points.sorted())
```

```console
$ praxis check order-a-record.px --color never
error[Y006]: values of type `Point` cannot be ordered

  order-a-record.px:5:12
  5 | out(points.sorted())
    |            ^^^^^^ values of type `Point` cannot be ordered

praxis: 1 error(s)
```

The answer is to say what to order by. `sorted_by_key` moves the requirement
from the element to whatever the closure extracts, which is where it can be a
scalar:

```praxis
struct Point { x: Int, y: Int }

var points = [Point { x: 3, y: 1 }, Point { x: 1, y: 2 }]

out(points.sorted_by_key(|p| p.x))
out([3, 1, 2].sorted())
out(["pear", "apple"].sorted())
```

```text
[{ x: 1, y: 2 }, { x: 3, y: 1 }]
[1, 2, 3]
[apple, pear]
```

`min_by` and `max_by` are the same move for the same reason. A `MinHeap` or
`MaxHeap` element carries the requirement from the heap's own type, because a
heap orders it whether or not you ever call a comparison.

## A key must be hashable *and* immutable

Hashing and equality are one question about a value's representation — a
descriptor's `hash` and `equals` callbacks are written together — so anything
comparable is hashable. That is exactly why "hashable" is the **wrong**
requirement for a `Map` key.

A `Vec` hashes fine. What it cannot do is stay findable: `key.push(2)` after
`table.insert(key, v)` moves the entry's bucket without moving the entry, and
nothing will ever look there again. So the rule is **mutability, not
container-ness**.

- Out, as a `Map` key, a `Set` element or a `Counter` key: `Vec`, `Map`, `Set`,
  `Deque`, `Grid`, `Counter`, `MinHeap`, `MaxHeap`, `BitSet`.
- In, structurally: scalars, `Text`, tuples, records and enums — a tuple or a
  record is a key exactly when every component is.
- In, and the one collection that is: `Range`. It has no mutator at all, so its
  two bounds are as fixed as a tuple's elements.

That is Python's rule (`list`, `dict` and `set` set `__hash__ = None` for this
reason). Rust's `HashMap<Vec<i32>, V>` is the counterexample that does not
transfer: it is legal only because the borrow checker makes mutating a held key
impossible, and Praxis has assignment and no borrow checker.

```praxis
var seen = Set()
var path = [1, 2]

seen.insert(path)
```

```console
$ praxis check mutable-key.px --color never
error[Y014]: a value of type `Vec[Int]` can change after it is stored, so it cannot be used as a key

  mutable-key.px:4:6
  4 | seen.insert(path)
    |      ^^^^^^ a value of type `Vec[Int]` can change after it is stored, so it cannot be used as a key

help: use a value that cannot change — a number, `Text`, or a tuple of those

praxis: 1 error(s)
```

The message is the reason rather than the rule, and deliberately so: "not
hashable" would be both jargon and a lie.

A tuple or a record of scalars is the everyday fix, and it is what a coordinate
key wants anyway:

```praxis
struct Point { x: Int, y: Int }

var seen = Set()
seen.insert((1, 2))
seen.insert((1, 2))
seen.insert((3, 4))

var visits = Map()
visits.insert(Point { x: 0, y: 0 }, 1)
visits.insert(Point { x: 0, y: 0 }, 2)

out(seen.len())
out(visits.get(Point { x: 0, y: 0 }))
```

```text
2
Some(2)
```

The requirement is asked at the method call — the place a program actually puts
a value into a collection — and after the arguments have unified, so
`m.insert(key, 1)` has already decided what `K` is by then. `var m = Map()`
mints two variables and the first `insert` is what says what they are.

The rule is about the *type*, not about what a particular value does next. A
record's fields are assignable, so writing one after it has been stored as a key
loses the entry exactly the way pushing to a `Vec` would — the type check cannot
see that, and [Collections](../language/collections.md) shows what it looks
like. Prefer a tuple where the key is only a key.

## A requirement rides the scheme that quantified it

The interesting case is a requirement discovered inside a *generic* function,
about a variable that is then quantified. `fn same(a, b) { a == b }` needs `a`
and `b` to be comparable — but at the point the body is checked, nothing has
said what they are, and an unresolved variable is optimistically anything.

The requirement is not decided there and discarded. It is attached to the scheme
that quantified the variable, and **re-emitted at every instantiation** against
whatever that use site put in the variable's place.

```praxis
fn same(a, b) {
    a == b
}

fn double(n) {
    n * 2
}

out(same(1, 1))
out(same(double, double))
```

```console
$ praxis check requirement-rides.px --color never
error[Y004]: values of type `(Int) -> Int` cannot be compared with `==`

  requirement-rides.px:10:5
  10 | out(same(double, double))
     |     ^^^^^^^^^^^^^^^^^^^^ values of type `(Int) -> Int` cannot be compared with `==`

note: this is the operation that requires it

  requirement-rides.px:2:10
  2 |     a == b
    |          ^

praxis: 1 error(s)
```

`same(1, 1)` is fine; `same(double, double)` is not; and the report is at the
call, with the `==` as a note. Reporting at `a == b` alone would name code that
is correct for every other instantiation of `same`. Reporting at the call alone
would leave you asking why. Both spans, one diagnostic.

## Iteration and arithmetic

The same machinery covers the other two closed questions.

Iterating something that is not one of the ten iterable collections — or `Text`,
the one scalar with members — is `Y005`:

```praxis
for x in 5 { out(x) }
```

```console
$ praxis check not-iterable.px --color never
error[Y005]: values of type `Int` cannot be iterated

  not-iterable.px:1:1
  1 | for x in 5 { out(x) }
    | ^^^^^^^^^^^^^^^^^^^^^ values of type `Int` cannot be iterated

praxis: 1 error(s)
```

Arithmetic on something that has none is `Y010` when the target's type is
already known:

```praxis
var flag = true
flag += false
```

```console
$ praxis check not-numeric.px --color never
error[Y010]: values of type `Bool` do not support this operation

  not-numeric.px:2:1
  2 | flag += false
    | ^^^^ values of type `Bool` do not support this operation

praxis: 1 error(s)
```

The numeric set is `Int`, `UInt`, `Byte` and `Float` — and `%` is narrower
still, undefined for `Float`, which is a rule at one operator rather than a
capability at all. Orderable and numeric are **different sets**: `Text` and
`Char` are ordered and are not numbers, `Bool` is neither.

When the target's type is *not* known at the operation, the requirement rides
the scheme like any other and reports at the call, as `Y015`:

```praxis
fn combine(a, b) {
    a += b
    a
}

out(combine(1, 2))
out(combine(true, false))
```

```console
$ praxis check deferred-numeric.px --color never
error[Y015]: values of type `Bool` cannot be used in arithmetic

  deferred-numeric.px:7:5
  7 | out(combine(true, false))
    |     ^^^^^^^^^^^^^^^^^^^^ values of type `Bool` cannot be used in arithmetic

note: this is the operation that requires it

  deferred-numeric.px:2:5
  2 |     a += b
    |     ^

praxis: 1 error(s)
```

The two codes are the same rule at two moments: `Y010` when the operation can
name the type, `Y015` when a later use pinned it. Pinning the target to `Int` at
the operation instead would narrow every unannotated numeric parameter in the
language, which is why the requirement waits.

## The whole list

| Code | When | Message shape |
|---|---|---|
| `Y004` | `==` / `!=` on a type with no equality | ``values of type `T` cannot be compared with `==` `` |
| `Y005` | `for` over something not iterable | ``values of type `T` cannot be iterated`` |
| `Y006` | sorting, a heap, or `<` on an unordered type | ``values of type `T` cannot be ordered`` |
| `Y010` | arithmetic on a known non-numeric type | ``values of type `T` do not support this operation`` |
| `Y014` | a `Map`/`Set`/`Counter` key that can change | ``a value of type `T` can change after it is stored, so it cannot be used as a key`` |
| `Y015` | arithmetic discovered after a later use pinned the type | ``values of type `T` cannot be used in arithmetic`` |

Each of them names a concrete type and a concrete operation. None of them names
the requirement, because the requirement has no name a program could write.

The remaining requirements the same channel carries are not yes/no questions at
all — they *produce* something when they hold. "This receiver has this method"
resolves to a catalog row, "this receiver is iterable" resolves to an item type,
and "this receiver has this field" resolves to a field type. Those are
[method resolution](method-resolution.md) and the `for` half of
[generalization](generalization.md).
