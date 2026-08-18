# Control flow

Praxis has `if`, `while`, `for`, `loop`, `break`, `continue` and `return`, plus
[`match`](pattern-matching.md), which has a chapter of its own. Blocks are
expressions and so are all of these, so the thing that decides a value and the
thing that produces it are usually one piece of syntax.

Two rules carry most of the weight. An `if` produces a value, which is why there
is no ternary operator. A `loop` is the only loop that produces one, and it
produces whatever its `break`s carry.

## `if` is an expression

```praxis
fn grade(score: Int) -> Text {
    if score >= 90 {
        "A"
    } else if score >= 80 {
        "B"
    } else {
        "C"
    }
}

var n = 7
var parity = if n % 2 == 0 { "even" } else { "odd" }

out(parity)
out(grade(95))
out(grade(83))
out(grade(12))
```

```text
odd
A
B
C
```

The condition is an ordinary expression, needs no parentheses, and must be
`Bool` — `if 1 { … }` is `Y001: expected Bool, found Int`, not a truthiness
rule. It is parsed with record literals suppressed, so `if flag { … }` reads the
braces as the branch rather than as `flag`'s field list; a record literal in a
condition has to be parenthesized, as in `if (P { x: 1 }).x == 1 { … }`.

Both branches must agree on a type, and an `if` with no `else` has an implicit
empty one, so its type is `Unit`. That is fine as a statement and an error the
moment you ask it for something:

```praxis
var n = 7
var label = if n > 0 { "positive" }
out(label)
```

```text
error[Y001]: expected Text, found Unit

  if-without-else.px:2:22
  2 | var label = if n > 0 { "positive" }
    |                      ^^^^^^^^^^^^^^ expected Text, found Unit

help: this value is `Unit`; an `if` with no `else` expected `Text` — make the last expression produce a value, or change the declared type to `Unit`

praxis: 1 error(s)
```

## `while`

```praxis
var i = 0
var sum = 0
while i < 10 {
    i = i + 1
    if i % 3 != 0 { continue }
    sum = sum + i
}
out(sum)
```

```text
18
```

`continue` jumps to the next test. A `while` is always `Unit`: it has an exit
path — the condition failing — with no value on it, so there is nothing for it
to produce. `loop`, below, is the one that does.

## `for` over anything iterable

`for binding in iterable { … }` walks the ten collections and `Text`. The
binding is a pattern, so a `Map`'s `(key, value)` pair can be taken apart in
place:

```praxis
for x in [3, 1, 2] { out(x) }
for i in 0..3 { out(i) }
for c in "hi" { out(c) }

var counts = Map()
counts["a"] = 1
counts["b"] = 2
for (word, n) in counts {
    out(word)
    out(n)
}

var heap = MinHeap()
heap.push(3)
heap.push(1)
heap.push(2)
for x in heap { out(x) }
```

```text
3
1
2
0
1
2
h
i
a
1
b
2
1
2
3
```

Each iterable's order is the one its own accessors already promise, and every
one is deterministic — a hash-backed collection is walked in ascending order of
its members, not in hash order, so two runs of the same program agree. A
`MinHeap` is walked in pop order, which is why `3, 1, 2` came back as `1, 2, 3`.

| Iterable | Order |
|---|---|
| `Vec`, `Deque`, `Range`, `Text` | in place, by index |
| `Set` | ascending by member |
| `Map`, `Counter` | ascending by key |
| `BitSet` | ascending bit |
| `MinHeap`, `MaxHeap` | pop order |
| `Grid` | row-major |

"Ascending" is the type's own order, the same one `sorted()` uses: numeric for
`Int`, `Byte` and `Float`, code-point for `Char` and `Text`, and element-wise
left to right for a tuple, a record or an enum. So a `Set[Int]` holding 2 and 10
is walked `2, 10`, and `out(s)` prints that same sequence.

A `for` is `Unit`. It runs its body; it does not collect anything. To build a
value out of a sequence, use a [pipeline](pipelines.md).

The loop variable is an ordinary binding and may be assigned inside the body.
Each step rebinds it, so the assignment does not survive into the next one.

### What happens if you mutate what you are iterating

The seven collections that cannot index themselves — `Set`, `Map`, `Counter`,
`BitSet`, `MinHeap`, `MaxHeap`, `Grid` — are walked through a **snapshot** taken
once, before the loop starts. Mutating one inside its own `for` is well defined
and terminates; the walk does not see the change.

A `Vec`, a `Deque`, a `Range` and a `Text` index themselves, so no snapshot is
taken and the loop re-reads the length on every step. A `push` during the walk
*is* seen:

```praxis
var seen = Set()
seen.insert(1)
seen.insert(2)
for x in seen {
    out(x)
    seen.insert(x + 10)
}
out(seen)

var xs = [1, 2, 3]
for x in xs {
    out(x)
    if x == 1 { xs.push(99) }
}
out(xs)
```

```text
1
2
{1, 2, 11, 12}
1
2
3
99
[1, 2, 3, 99]
```

The `Set` loop ran twice, over the two members the set had when it began, and
both inserts landed anyway. The `Vec` loop ran four times. If that asymmetry
matters to your program, iterate a copy.

A snapshot is the only protocol a collection that cannot index itself can offer.
A hash set and a hash map have no nth member, so answering one is a linear scan
and every loop over a hashed collection would be quadratic; a heap's array is
ordered at its root and nowhere else, so reading it by index answers in
insertion order rather than in heap order. One call that hands back the members
costs one `Vec` per loop and gets every collection right.

## `loop` is the value its `break`s carry

`loop { … }` repeats until something leaves it. It is the only loop that is an
expression with a value, and that value is the join of every `break` in it:

```praxis
fn collatz_steps(start: Int) -> Int {
    var n = start
    var steps = 0
    loop {
        if n == 1 { break steps }
        if n % 2 == 0 { n = n / 2 } else { n = 3 * n + 1 }
        steps = steps + 1
    }
}

out(collatz_steps(27))
out(collatz_steps(1))
```

```text
111
0
```

The `loop` is the last expression in `collatz_steps`, so its value is the
function's result. Nothing is written twice, and there is no sentinel to
initialize.

The edges all follow from "the join of its breaks":

- `loop { break 42 }` is `Int`.
- `loop { break }` is `Unit` — a bare `break` leaves with nothing, so mixing
  `break` and `break 1` in one loop is a `Y001` rather than a coincidence that
  happens to work.
- `loop { }` is `Never`: it produces no value at all, so it absorbs into
  whatever sits beside it. `if n > 0 { n } else { loop { } }` is an `Int`.

A `break` carrying a value out of a `while` or a `for` is rejected. Those loops
have an exit path — the condition failing, the sequence running out — that no
`break` is on, and there is no value to invent for it:

```praxis
var n = 0
var first = while n < 10 {
    if n * n > 20 { break n }
    n = n + 1
}
out(first)
```

```text
error[Y017]: a `break` carrying a value needs a `loop`; a `while` produces `Unit`

  break-with-value-in-while.px:3:27
  3 |     if n * n > 20 { break n }
    |                           ^ a `break` carrying a value needs a `loop`; a `while` produces `Unit`

praxis: 1 error(s)
```

Rewrite it as a `loop` with the test inside, or read the `var` the `while` left
behind. Those two loops have an exit the compiler cannot fill: nothing in
`while c { break 1 }` says what the loop produces when `c` is false, and there is
no value to invent.

`break` and `continue` apply to the innermost enclosing loop; there are no
labels. Where there is no loop, both are `Y012` — `` `break` outside a loop ``,
`` `continue` outside a loop ``. A closure body is outside every loop around it,
so `loop { var f = || break }` is `Y012` too: a `break` inside a closure has no
loop of its own to leave.

## Ranges

`a..b` is the integers from `a` up to but not including `b`; `a..=b` includes
`b`. Both bounds are required — there is no `a..`, `..b` or `..` — and both are
`Int`.

`..` binds looser than arithmetic, so `0..n - 1` means `0..(n - 1)`, which is
what a range with a computed bound almost always wants.

A range is a value, not just a loop header. It binds to a name, goes in a `Vec`,
is a `Map` key, and is a type a parameter can declare:

```praxis
var window = 2..6
out(window)
out(1..=3)

var windows = [0..2, 3..5]
out(windows)

var names = Map()
names[0..2] = "low"
names[2..4] = "high"
out(names[0..2])

fn width(r: Range) -> Int {
    var n = 0
    for i in r { n = n + 1 }
    n
}
out(width(window))
out(width(3..=7))
```

```text
2..6
1..4
[0..2, 3..5]
low
4
5
```

`1..=3` printed as `1..4`. A range is normalized to its half-open form when it
is built, so `1..=3` and `1..4` are one value — they compare equal and hash to
the same key — and the inclusive spelling is the one thing about a range that is
not recoverable from it afterwards. Being a key is safe because a range has no
mutator at all: its two bounds are as fixed as a tuple's elements.

`Range` is a collection type with no methods of its own, so `r.len()` is `Y110`.
The [pipeline](pipelines.md) methods do work on it: `(1..5).count()` is `4` and
`(1..5).sum()` is `10`.

### A descending range is empty

`5..0` does not count down. It is empty, and the emptiness is established when
the range is built rather than checked by every reader:

```praxis
var down = 5..0
out(down)

var ran = 0
for i in 5..0 { ran = ran + 1 }
out(ran)

for i in 0..0 { ran = ran + 1 }
out(ran)

// Counting down is a reversed range, which is a Vec and not a Range.
for i in (0..3).reversed() { out(i) }
```

```text
5..5
0
0
2
1
0
```

`5..0` printed as `5..5`, because the constructor clamps an `end` below `start`
up to `start`. No range with a negative length exists, so a `for` reading a
range's length can never get a bound that runs the loop backwards. The case that
decides it is `0..n` with `n == 0`: that has to run zero times, not `n` times in
reverse.

**The countdown is `(0..n).reversed()`**, a [pipeline barrier](pipelines.md#barriers)
that answers a `Vec[Int]`. It does not make a descending `Range` — there is no
such value, and the clamp above is why. Writing `5..0` still earns no
diagnostic: it is a legal empty collection, and the language has no warnings to
give it.

The bounds are `Int` and nothing else. A `Float` range would need a step to
yield anything at all — `0.0..1.0` has no elements without one — and a range
that cannot say what it yields is not a collection.

## `return`

`return` leaves the enclosing function, with a value or without one. It is not
needed for the common case — the last expression of a function body is its
result — but it is the way out of the middle of a loop:

```praxis
fn first_even(xs) {
    for x in xs {
        if x % 2 == 0 { return x }
    }
    0 - 1
}
```

A `return` inside a loop leaves the *function*, not the loop, which is why a
`loop` exited only by `return` produces no value and is `Never`.

The fallback there is written `0 - 1` and not `-1` on purpose. A block is an
expression and the expression parser does not stop at a line break, so a `}`
followed by a line beginning with `-` reads as one subtraction spanning both.
Parenthesize, or write the negation so it cannot start a line.
