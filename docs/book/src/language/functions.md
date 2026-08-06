# Functions and closures

Praxis has two callable forms and one deliberate difference between them. A `fn`
is a top-level declaration and is a function of its parameters and nothing else.
A closure, `|x| …`, is an expression, and it captures the bindings around it.

That is the line to keep in mind, because it is the one the compiler enforces:
naming an outer binding inside a `fn` is an error with its own code, and the
error tells you which of the two forms you wanted.

## `fn`

```praxis
struct Point { x: Int, y: Int }

fn manhattan(a, b) {
    abs(a.x - b.x) + abs(a.y - b.y)
}

fn factorial(n: Int) -> Int {
    if n <= 1 { 1 } else { n * factorial(n - 1) }
}

fn first_even(xs) {
    for x in xs {
        if x % 2 == 0 { return x }
    }
    0 - 1
}

out(manhattan(Point { x: 1, y: 2 }, Point { x: 4, y: 6 }))
out(factorial(5))
out(first_even([1, 3, 6, 7]))
out(first_even([1, 3, 7]))
```

```text
7
120
6
-1
```

The last expression of the body is the result; `return` leaves early. Parameter
and return annotations are optional and inference derives both from use, which
is how `manhattan` knows its arguments have `x` and `y` fields and answers
`Int`. Write them where they document something, or where the error message you
get without them points at the wrong place.

A parameter is a plain name — there is no destructuring in a `fn` parameter
list, unlike a closure's. It is also an ordinary binding, so a function may
assign to its own parameter:

```praxis
fn clamp_low(n) {
    if n < 0 { n = 0 }
    n
}
```

Functions may be declared in any order and may call each other, including
mutually and recursively. They may not be nested: a `fn` inside a `fn` is
`N005`.

**A name has exactly one signature.** There is no overloading on arity or type,
no optional or default parameters, and no named arguments. A call with the wrong
count is `Y024: this function takes 2 argument(s), but 1 were given`. Where
another language would overload, Praxis uses a second name — the method catalog
does, with `min`/`min_by`, `max`/`max_by` and `find`/`position`
([ADR-089](../../../decisions/089-a-name-has-one-signature.md)). The prelude's
own `min` and `max` are the two-argument free functions, and there is no
`min_by` beside them.

## A `fn` does not capture

A `fn` body may name other declarations — functions, structs, enums, variant
constructors, the prelude — because those are reachable from anywhere. It may
not name a `var` outside itself. That is a report, not a silent read:

```praxis
var offset = 10

fn shift(n: Int) -> Int {
    n + offset
}

out(shift(1))
```

```text
error[N007]: `shift` cannot use `offset`: a function does not capture the bindings around it (pass `offset` as a parameter, or use a closure)

  fn-does-not-capture.px:4:9
  4 |     n + offset
    |         ^^^^^^ `shift` cannot use `offset`: a function does not capture the bindings around it (pass `offset` as a parameter, or use a closure)

praxis: 1 error(s)
```

The message names both ways out, and both are ordinary:

```praxis
var offset = 10

fn shift(n: Int, by: Int) -> Int {
    n + by
}

var shift_by_offset = |n| n + offset

out(shift(1, offset))
out(shift_by_offset(1))
```

```text
11
11
```

The boundary is a **`fn` body**, not a closure body. A closure opens no boundary
of its own, so the question is always about the nearest enclosing `fn`, and the
three cases are the three you would expect:

- A closure at the top level using a top-level binding is fine — top-level
  statements are the program's own body, and the binding is in it.
- A closure inside a `fn`, using that `fn`'s own local, is fine. It captures
  something the function has.
- A closure inside a `fn`, using a binding declared outside that `fn`, is
  `N007`. The closure is inside `g`; the binding is not.

### A recursive `fn` is offered only the parameter

The message above names two ways out. A **recursive** function has one, because
a closure cannot name itself: `var f = |n| … f(n - 1) …` resolves `f` in the
environment *before* the declaration, so the call inside is `N001`. Rather than
suggest something it would then refuse, the compiler drops that half and says
which rule took it away:

```praxis
var step = 2

fn countdown(n: Int) -> Int {
    if n <= 0 { 0 } else { 1 + countdown(n - step) }
}

out(countdown(10))
```

```text
error[N007]: `countdown` cannot use `step`: a function does not capture the bindings around it (pass `step` as a parameter)

  fn-recursive-cannot-capture.px:4:46
  4 |     if n <= 0 { 0 } else { 1 + countdown(n - step) }
    |                                              ^^^^ `countdown` cannot use `step`: a function does not capture the bindings around it (pass `step` as a parameter)

help: `countdown` calls itself, so a closure is not the way out: a closure cannot name itself (`N001`)

praxis: 1 error(s)
```

Mutual recursion counts too, and there the `help:` names the other function in
the cycle. A `fn` that merely *calls* a recursive one is not itself recursive
and keeps both ways out.

A binding declared *after* the function is `N001` rather than `N007`: only `fn`,
`struct` and `enum` are pre-registered for forward reference, so the name is
genuinely not in scope and nothing has crossed a boundary.

This used to compile. `fn f() { x }` read a slot the function did not have and
answered `Unit`; the closure form answered a nine-digit number. Neither was
reported.
[ADR-068](../../../decisions/068-a-function-does-not-capture.md) is the fix and
the argument for reporting rather than for making a `fn` capture.

## Closures

A closure is `|params| body`. The body is one expression, which may be a block.
Parameters are patterns, so a pair can be taken apart in the parameter list, and
`||` — one token — is the empty list:

```praxis
var offset = 10
out([1, 2, 3].map(|x| x + offset))

fn shifted(values, by) {
    values.map(|x| x + by)
}
out(shifted([1, 2, 3], 100))

fn adder(n: Int) -> (Int) -> Int { |x| x + n }
var add5 = adder(5)
out(add5(2))

var seven = || 7
out(seven())

out([(1, 2), (3, 4)].map(|(a, b)| a + b))
```

```text
[11, 12, 13]
[101, 102, 103]
7
7
[3, 7]
```

A function type is written `(Int) -> Int`, and that is what a parameter or a
return annotation says when it holds a closure. A closure parameter may be
annotated too: `|x: Int| x + 1`.

A closure's environment outlives the frame that made it — `adder` returns one
that still has `n` — because the environment is on the garbage-collected heap.
There are no move closures, no borrow captures and no lifetime rules.

Two things a closure cannot do. It cannot name itself — a body that calls `f`
inside `var f = |n| …` gets `N001`, because `f` is not in scope until its own
declaration finishes, so recursion needs a `fn`. And it has no readable form:
`out` on a closure prints `<closure:N>`, where `N` is how many bindings it
captured — `<closure:0>` for one that captures nothing.

### Captured by value, or through a cell

Whether a capture is a copy or a shared cell is not something you write. The
compiler asks one question about the captured binding: does anything, anywhere,
assign to it? A binding nothing assigns to is **copied** into the closure's
environment. A binding something assigns to gets a garbage-collected cell that
the declaring frame and every capturing closure share, so a write on either side
is seen by both.

```praxis
var offset = 10
var add_offset = |x| x + offset
out(add_offset(1))
offset = 100
out(add_offset(1))

var base = 10
var add_base = |x| x + base
out(add_base(1))
var base = 100
out(add_base(1))
```

```text
11
101
11
11
```

`offset` is assigned on line 4, so the closure holds a cell and sees `100`.
`base` is never assigned — the second `var base` is a *new* binding that shadows
the first, with its own symbol and its own type — so `add_base` holds a copy of
`10` and keeps answering `11`.

That is the whole rule, and it is why shadowing and reassignment, which look
alike, behave differently here. See
[bindings and shadowing](bindings.md) for the difference, and
[ADR-125](../../../decisions/125-a-binding-is-a-binding-and-the-compiler-decides-its-storage.md)
for why the compiler derives this rather than taking a keyword for it.

A `for` variable is a fresh binding on every step, so closures made on different
steps hold different bindings rather than sharing one — they do not all end up
at the last step's value. Inside a step the rule above still applies: a loop
variable the body assigns to is a reassigned binding, so that step's closure
holds that step's cell and sees the write.

```praxis
var fs = Vec()
for x in [1, 2, 3] {
    fs.push(|| x)
    x = x + 100
}
for f in fs { out(f()) }
```

```text
101
102
103
```

### A closure that returns a closure captures for it

A closure whose body *is* another closure captures whatever the returned one
names from outside them both — not just what its own body mentions directly.
It has to: the returned closure's environment is filled from the returning
closure's frame at the moment its literal is evaluated, so the returning closure
must be holding the value in order to hand it over.

```praxis
var base = 10
var mk = |a| |b| a * 100 + b * 10 + base
out(mk)
out(mk(1)(2))

var deep = |a| |b| |c| a + b + c + base
out(deep(1)(2)(3))

var n = 0
var bump = |a| |b| { n = n + a + b; n }
out(bump(1)(2))
out(bump(10)(20))
out(n)
```

```text
<closure:1>
130
16
3
33
33
```

`mk` prints `<closure:1>`: it captures one binding, `base`, even though nothing
in `|a| …` names `base` except the closure it returns. Nesting is not a limit —
`deep` threads the same capture down three levels — and a reassigned binding is
still a single shared cell however many environments it passes through, which is
why `bump` accumulates into one `n` that the outer scope reads back.

What is *not* captured is anything either closure declares. `|a| |b| b + a`
captures nothing: `a` is the outer closure's own parameter, and `b` is the
inner's. Only a name declared outside both becomes an environment slot.

## A `fn` name in value position is a closure

Writing a function's name without calling it produces a closure over it, with an
empty environment. It can be bound, passed, stored and called like any other:

```praxis
fn double(n: Int) -> Int { n * 2 }

fn apply(f, x) { f(x) }

var f = double
out(f(3))
out(apply(double, 20))
out([1, 2, 3].map(double))
```

```text
6
40
[2, 4, 6]
```

A direct call — `double(3)` — is still a direct call and allocates nothing. It
is only the name *in value position* that builds a closure, and it builds one
per evaluation: `var f = double` inside a loop allocates on every iteration, the
same as `|n| double(n)` would. Hoist it if that matters.

A **generic** function has no function value, because there is nothing at a
value to specialize it against:

```praxis
fn identity(x) { x }

var f = identity
out(f(3))
```

```text
error[Y018]: `identity` is generic, so it has no single function value; write `|x| identity(x)` to fix its type arguments at the call

  generic-fn-as-a-value.px:3:9
  3 | var f = identity
    |         ^^^^^^^^ `identity` is generic, so it has no single function value; write `|x| identity(x)` to fix its type arguments at the call

praxis: 1 error(s)
```

The remedy in the message works because a closure body *is* a call site, and a
call site is what fixes the type arguments.
[ADR-061](../../../decisions/061-a-fn-name-in-value-position-is-a-closure.md)
has the reasoning, including why this used to abort the host process.

## How a parameter generalizes

A parameter whose type the body never pins is quantified, and the function may
then be called at several types in one program:

```praxis
fn identity(x) { x }

out(identity(1))
out(identity("s"))
out(identity(true))
```

```text
1
s
true
```

Calling a **method** on a parameter pins it. There is one lowered body per
source function, and a method call site carries exactly one catalog entry and
one receiver type, so a quantified receiver would be several receiver types at
one call site with no way to compile any of them. Two call sites that disagree
are therefore a disagreement about the function's signature, reported as one:

```praxis
fn head(xs) { xs[0] }

out(head([1, 2, 3]))
out(head(["a", "b"]))
```

```text
error[Y001]: expected (Vec[Int]) -> ?T, found (Vec[Text]) -> ?T

  parameter-pinned-by-a-method.px:4:5
  4 | out(head(["a", "b"]))
    |     ^^^^^^^^^^^^^^^^ expected (Vec[Int]) -> ?T, found (Vec[Text]) -> ?T

praxis: 1 error(s)
```

Annotate the parameter, or write a second function.

### `for` over a parameter is the exception

Iterating a parameter splits the two facts. The **iterable** stays quantified,
so one source body serves a `Vec`, a `Range` and a `Set`; the **element** is
pinned, so it is one type for the whole program, even in a body that never
touches it:

```praxis
fn total(items) {
    var t = 0
    for i in items { t = t + i }
    t
}

var seen = Set()
seen.insert(4)
seen.insert(9)

out(total([1, 2, 3]))
out(total(0..5))
out(total(seen))
```

```text
6
10
13
```

`total` is "any iterable, of `Int`". The asymmetry is not a preference: a `Vec`
and a `Range` are walked through different runtime accessors, so a single
compiled body could not serve both — one of them would read a length out of the
wrong word. One clone per iterable kind is the only way the accessors can be
right. The element, in contrast, has to be one type for the loop variable's slot
to have one.

Disagreeing about the element is reported at the call site, with the operation
that requires it as a note:

```praxis
fn total(items) {
    var t = 0
    for i in items { t = t + i }
    t
}

out(total([1, 2, 3]))
out(total(["a", "b"]))
```

```text
error[Y001]: expected Int, found Text

  iterated-parameter-mismatch.px:8:5
  8 | out(total(["a", "b"]))
    |     ^^^^^^^^^^^^^^^^^ expected Int, found Text

note: this is the operation that requires it

  iterated-parameter-mismatch.px:3:14
  3 |     for i in items { t = t + i }
    |              ^^^^^

praxis: 1 error(s)
```

`Y001` and not "cannot be iterated": `Vec[Text]` iterates perfectly well, and
the body is correct for every other instantiation of `total`. What is wrong is
`t + i`, at this one call.
[ADR-062](../../../decisions/062-an-iterated-parameter-is-generic-in-the-iterable-and-not-its-element.md)
is the decision, and [generalization](../types/generalization.md) is the wider
picture.
