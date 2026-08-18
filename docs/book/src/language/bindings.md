# Bindings and shadowing

`var` is the language's one binding form. It introduces a name, that name may be
reassigned, and the type it was inferred at is the type it keeps. There is no
second keyword and no immutable binding class.

```praxis
var score = 0
score += 10
score = 25
out(score)

var name: Text = "praxis"
out(name.len())

var seen: Vec[Int] = Vec()
seen.push(3)
out(seen)
```

```text
25
6
[3]
```

The `: Text` and `: Vec[Int]` are optional. Inference reads the type off the
initializer, and off the later uses when the initializer leaves it open — bare
`Vec()` is fine, and the first `push` decides the element type. Writing the
annotation pins the type at the declaration instead, which moves the error to
the line that disagrees with you rather than the line after it.

## `let` does not exist

`let` is not a keyword. The distinction it would draw is inferred rather than
declared, and the word is not reserved either, so it is an ordinary identifier
the compiler has never heard of.

```praxis
let x = 5
out(x)
```

```text
error[N009]: `let` is not a keyword; a binding is written with `var`

  let-is-gone.px:1:1
  1 | let x = 5
    | ^^^ `let` is not a keyword; a binding is written with `var`

help: replace it with `var`
      var
```

`N009` is its own code, and the reason is the fix. `let` is not a misspelling of
anything, so the near-miss search that answers `totl` with `total` has nothing
useful to say about it: the budget is one edit for a three-letter name, and the
name one edit away is `Set`. The rule is right in general and wrong for this
word, so this word is answered before the search runs.

That is the first of four errors from those two lines: `let` and `x` run
together with no separator (`P002`), and `x` is then never declared, so both
mentions of it are `N001`. `var let = 5` compiles, if you want the word — which
is why this is reported where a statement *starts* rather than in the lexer.

## Assignment keeps the type

Reassignment writes a new value into an existing binding. It never re-runs
inference, so the value has to have the type the binding already has:

```praxis
var score = 0
score = "high"
```

```text
error[Y001]: expected Int, found Text

  retype.px:2:1
  2 | score = "high"
    | ^^^^^ expected Int, found Text

praxis: 1 error(s)
```

The span is on the *target*, not the value: the binding is the thing with the
expectation.

## The compound operators

There are five — `+=`, `-=`, `*=`, `/=` and `%=` — and each is its binary
operator's rule applied to a place. `n += 1` is `n = n + 1`, so what the
compound accepts is what the operator accepts, and the right-hand side types
against the binding rather than being inferred on its own.

```praxis
// Each compound is its binary operator applied to a place.
var n = 10
n += 3
n -= 2
n *= 4
n /= 3
n %= 5
out(n)

var f = 10.0
f += 3.0
f -= 2.0
f *= 4.0
f /= 4.0
out(f)

var s = "a"
s += "b"
out(s)
```

```text
4
11.0
ab
```

Two consequences fall straight out of "it is the binary operator":

- **`%=` is `Int`-only**, because [`%` is](numbers.md#float). `f %= 2.0` is
  `Y016`, the same refusal `f % 2.0` gets. The other four are defined for
  `Float`.
- **`+=` on a `Text` is concatenation**, because [`+` is](text.md#-is-the-only-arithmetic-operator).
  It is the one compound that does not require a number.

Everything else needs a numeric target, and `Y010` is the error when it does not
get one. The operators are statements and not expressions, so `var x = (n += 1)`
does not parse — see the [grammar](../appendix/grammar.md).

`n = n + 1` is the rule and not the lowering: a target that is a field or an
element is [evaluated once](#places-fields-and-elements), not once to read and
again to write.

## Every binding is assignable

A function parameter, a `for` loop's variable and a name introduced by a pattern
are bindings in exactly the sense a `var` is, and all of them may be written:

```praxis
fn clamp_low(n) {
    if n < 0 { n = 0 }
    n
}

out(clamp_low(-3))
out(clamp_low(7))

for i in 0..3 {
    i = i * 10
    out(i)
}

var total = 0
for (a, b) in [(1, 2), (3, 4)] {
    a = a * 100
    total += a + b
}
out(total)
```

```text
0
7
0
10
20
406
```

Writing a `for` variable changes this step and nothing else — the next step
rebinds it from the sequence. Writing a parameter changes the callee's binding
and nothing at the call site; see [the binding and the object](#the-binding-and-the-object)
below for what *is* shared.

## Shadowing

A later `var` may shadow an earlier binding of the same name in the same scope.
This is not reassignment: it allocates a new binding, with a new symbol, and the
new one may have an unrelated type.

```praxis
var a = 4
var a = "Foo"
out(a)

var b = 4
var b = b + 1
out(b)

var c = 4
var show_old = || out(c)
var c = "Foo"
show_old()
out(c)
```

```text
Foo
5
4
Foo
```

Three rules are in that program. The name resolves to the newest binding
declared *above* the use, so `out(a)` is the `Text`. A shadowing initializer is
resolved in the environment before the new binding enters scope, so the `b` on
the right of `var b = b + 1` is the old `Int` — this is Rust's rule and the same
trap when you meant to assign. And a closure made before a shadowing declaration
keeps the binding it captured, so `show_old` still prints `4` after `c` has
become a `Text`.

Shadowing is the only way to rebind a name at a new type. If you want the
`Text`, shadow; if you want the same `Int` with a new value, assign.

## The compiler decides the storage

Removing `let` removed two decisions the programmer used to make by choosing a
keyword. The compiler makes them now, from one fact name resolution can see:
whether anything ever writes the binding.

### Generalization

A binding nothing writes is generalized, under the usual value restriction. A
closure bound to such a name is generic and each use instantiates it fresh:

```praxis
var id = |x| x

out(id(1))
out(id("text"))
```

```text
1
text
```

Add one assignment to `id` and the same program stops compiling:

```praxis
var id = |x| x
id = |x| x

out(id(1))
out(id("text"))
```

```text
error[Y001]: expected (Int) -> Int, found (Text) -> ?T

  reassigned-not-generic.px:5:5
  5 | out(id("text"))
    |     ^^^^^^^^^^ expected (Int) -> Int, found (Text) -> ?T

praxis: 1 error(s)
```

`id` is monomorphic, `out(id(1))` pinned it to `(Int) -> Int`, and the `Text`
call is the error. This gate is a soundness requirement rather than a
convenience: assignment *instantiates* a scheme and unifies the copy, so a
generalized binding would not be constrained by being written, and
`id = |n| n + 1` followed by `id("s")` would type-check and reach the backend as
a wrong-type call. [Generalization](../types/generalization.md) covers the value
restriction itself.

### Capture

A captured binding that something writes is boxed into a GC-managed cell, so the
closure observes the write. One that nothing writes is copied into the closure's
environment, which is cheaper. The choice is the compiler's, made from the same
fact:

```praxis
var n = 1
var show_n = || out(n)
n = 2
show_n()

var fns = Vec[() -> Int]()
for i in 0..3 {
    i = i * 10
    fns.push(|| i)
}
for f in fns {
    out(f())
}
```

```text
2
0
10
20
```

`show_n` prints `2`, not `1`: it shares `n` with the code that wrote it. The
loop shows the other half of the rule — boxing is per *binding event*, not per
name. A `for` variable is a fresh binding each step, so the closure made on step
*i* keeps step *i*'s value even though the variable is assigned inside the loop.

## The binding and the object

Rebinding a name and mutating an object are separate operations, and only the
second is visible to anyone else. Passing an argument copies a reference: the
callee's parameter is its own binding pointing at the caller's object.

```praxis
fn rebind(xs) {
    xs = [9, 9]
    xs
}

fn mutate(xs) {
    xs.push(9)
}

var values = [1]
out(rebind(values))
out(values)

mutate(values)
out(values)
```

```text
[9, 9]
[1]
[1, 9]
```

`rebind` writes its own binding and the caller's `values` is untouched.
`mutate` writes the object both names refer to, and the caller sees it.

## Places: fields and elements

An assignment target may be a name, a field, or an index. A field or element
store writes into an object; it is the second kind of write above, not a
rebinding.

```praxis
struct Point { x: Int, y: Int }

var p = Point { x: 1, y: 1 }
p.x = 5
p.y += 2
out(p)

var xs = [1, 2, 3]
xs[0] = 100
xs[2] += 1
out(xs)

var counts = Counter[Text]()
counts["a"] += 1
counts["a"] += 1
out(counts["a"])
```

```text
{ x: 5, y: 3 }
[100, 2, 4]
2
```

A compound operator evaluates its place once. `p.x += 1` loads and stores
through the same receiver, so `pick(log).x += 1` calls `pick` a single time.

**A sequence store replaces and never appends.** `xs[xs.len()] = v` is a fault,
not a push:

```praxis
var xs = [1, 2, 3]
xs[3] = 4
```

```text
error: program faulted: index out of bounds
```

(followed by the backtrace described in [a file is a
program](program-structure.md#the-generated-function-has-a-name-you-cannot-write)).
Use `push` when you meant to grow the vector. `Vec`, `Deque`, `Map`, `Counter`
and `Grid` accept an indexed store; `Text` is the one subscript you can read and
not write, because a `Text` is an immutable payload with nothing to write
through. A tuple element is not a place either:

```praxis
var t = "abc"
t[0] = "z"

var pair = (1, 2)
pair.0 = 5
```

```text
error[Y020]: values of type `Text` cannot be assigned through 1 index(es)

  not-a-place.px:2:1
  2 | t[0] = "z"
    | ^^^^ values of type `Text` cannot be assigned through 1 index(es)

error[Y021]: the left side of an assignment must be a name, a field, or an index

  not-a-place.px:5:1
  5 | pair.0 = 5
    | ^^^^^^ the left side of an assignment must be a name, a field, or an index

praxis: 2 error(s)
```

`Y021` is also what you get for `f() = 1` — a target that names no storage at
all. Rebuild the tuple instead, or use a [record](records.md), which is the
thing in this language with named, writable fields.
