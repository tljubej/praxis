# Generalization

Generalization is the step that turns a type with open variables into a
**scheme** — `forall T. (T) -> T` — so that each use of the name gets its own
fresh copy. It is what lets one function serve two element types, and it is the
one place where whether you *write* to a binding changes what its type means.
[What inference does](model.md) is the background; this chapter is the rule.

```praxis
fn swap(a, b) {
    (b, a)
}

fn twice(x) {
    [x, x]
}

var id = |x| x

out(id(1))
out(id("two"))
out(swap(1, "one"))
out(swap(true, 2.5))
out(twice(7))
out(twice("s"))
```

```text
1
two
(one, 1)
(2.5, true)
[7, 7]
[s, s]
```

`id` is `forall T. (T) -> T`, `swap` is `forall T U. (T, U) -> (U, T)`, and
`twice` is `forall T. (T) -> Vec[T]`. Each call instantiates a fresh copy, so
the `Int` use and the `Text` use never meet.

## The rule: a binding something writes is not generalized

Praxis has one binding form. `let` was removed, and with it the keyword that
used to carry this distinction
([ADR-125](../../../decisions/125-a-binding-is-a-binding-and-the-compiler-decides-its-storage.md)).
What replaced it is a fact name resolution already knows: **is this binding ever
the target of an assignment?**

- A binding nothing writes is generalized, under the value restriction.
- A binding something writes is not.

Which of the two a binding is, is inferred and never declared. Add one
assignment and the same initializer stops being polymorphic:

```praxis
var id = |x| x
id = |n| n + 1

out(id(1))
out(id("two"))
```

```console
$ praxis check reassigned-binding.px --color never
error[Y001]: expected (Int) -> Int, found (Text) -> ?T

  reassigned-binding.px:5:5
  5 | out(id("two"))
    |     ^^^^^^^^^ expected (Int) -> Int, found (Text) -> ?T

praxis: 1 error(s)
```

The gate is a soundness requirement, not tidiness. Assignment *instantiates* the
target's scheme and unifies the copy, so writing to a generalized binding does
not constrain it. Without the gate, `var id = |x| x` would generalize to
`forall T. (T) -> T`, `id = |n| n + 1` would leave it there, and `id("two")`
would then type-check and hand a `Text` to a closure that adds one to it — a
wrong-typed call reaching the backend, not a missing diagnostic.

`fn` declarations generalize too, after their bodies are checked, and the gate
does not reach them: it is a fact read off a `var` statement, and a `fn` is not
one. Every binding in the language is assignable — `Y009`, the old "not a
`var`", is retired — so writing to a `fn` name is accepted. It does nothing:

```praxis
fn ident(x) {
    x
}

ident = |n| n + 100

out(ident(1))
out(ident("two"))
```

```text
1
two
```

The call still runs the declaration, and the scheme is still generic. That the
write is discarded rather than refused is a rough edge, not a rule to lean on.

## Levels decide which variables are quantified

The textbook rule — "quantify every variable not free in the environment" — is
wrong here, because inference is partial: a variable minted inside a function
body may still be reachable from an outer binding that has not been inferred
yet. Praxis uses Pottier and Rémy's **binding levels**
([ADR-008](../../../decisions/008-let-generalization-levels.md)).

Every type variable records the level at which it was created. Entering a
binding's body raises a counter; leaving it restores it. Generalizing at a
binding site quantifies exactly the unbound variables whose level is *strictly
deeper* than that site. The correctness rule lives in unification: when a
younger variable is linked to a type containing older ones, the older ones are
**lowered** to the younger's level, so an inner generalization cannot quantify
something the enclosing scope still reaches.

You do not write levels and they never appear in a diagnostic. The observable
consequence is the one above — polymorphism where the environment does not
constrain a variable, and a monotype where it does.

## A scheme owns its binders

A scheme carries its own binder list. Nothing in the arena records "this
variable is quantified", because that is a fact about *a* scheme, and only the
scheme that quantified it knows
([ADR-047](../../../decisions/047-scheme-owned-binders-and-the-level-newtype.md)).

That is what decides how a variable prints. Inside a scheme that binds it, a
variable is `T`; where no scheme binds it — a bare type, a half-solved call, an
element nothing pinned — it is `?T`. The question mark means "free here", not
"broken".

A parameter of a generic `fn` is on the first side of that line even though its
own type is a monotype: `c` in `fn foo(c) { c() }` is `() -> T`, because `foo` is
`forall T. (() -> T) -> T` and that is the variable
([ADR-151](../../../decisions/151-a-bound-variable-is-not-free-and-a-frame-is-in-a-call.md)).
Every surface that shows a binding's type — hover, an inlay hint, completion,
signature help — asks the same question, so a `?` in any of them means the same
thing in all of them.

The same fact has a user-visible edge: a generic `fn` has no single function
value, so it cannot be passed as one.

```praxis
fn ident(x) {
    x
}

var f = ident
out(f(1))
```

```console
$ praxis check generic-function-value.px --color never
error[Y018]: `ident` is generic, so it has no single function value; write `|x| ident(x)` to fix its type arguments at the call

  generic-function-value.px:5:9
  5 | var f = ident
    |         ^^^^^ `ident` is generic, so it has no single function value; write `|x| ident(x)` to fix its type arguments at the call

praxis: 1 error(s)
```

A wrapping closure is one instantiation, which is a value. A monomorphic `fn`
name needs no wrapper — it already denotes one function.

## Shadowing

Every declaration that reuses a name is a **new binding** with a new symbol id,
inferred independently. Its initializer resolves names in the environment that
existed *before* it, so `var x = x + 1` reads the old `x` and defines a new one,
and the two may have completely unrelated types.

```praxis
var value = read lines(int)
var value = value.sum()
var value = value > 10

out(value)

var label = "hello"
var label = label.len()
out(label)
```

Given

```text
3
4
5
```

it prints

```text
true
5
```

`value` is a `Vec[Int]`, then an `Int`, then a `Bool`. Nothing is reassigned
here, so each of the three is inferred and generalized on its own terms, and
hovering each occurrence in the editor gives a different symbol and a different
type.

Shadowing is also the only way to rebind a name at a new type. `value = "text"`
after `var value = 1` is a `Y001`; `var value = "text"` is a new binding.

An assignment's target is decided by scope, not by spelling. In

```praxis
var a = 1
a = 2
var a = "s"
```

the assignment writes the **first** `a` — which is therefore the one that is not
generalized — and the third line introduces a second, unrelated binding.

## Two things that deliberately do not generalize

### A receiver a method was called on is pinned

```praxis
fn total(values) {
    values.sum()
}

var counts = [1, 2]
var weights = [1.5, 2.5]

out(total(counts))
out(total(weights))
```

```console
$ praxis check pinned-receiver.px --color never
error[Y001]: expected (Vec[Int]) -> ?T, found (Vec[Float]) -> ?T

  pinned-receiver.px:9:5
  9 | out(total(weights))
    |     ^^^^^^^^^^^^^^ expected (Vec[Int]) -> ?T, found (Vec[Float]) -> ?T

praxis: 1 error(s)
```

`total`'s parameter is `Vec[Int]` — a monotype — because the first call said so.
This is not an oversight; it is
[ADR-057](../../../decisions/057-a-capability-requirement-rides-on-the-scheme-that-quantified-it.md)
decision 5, and the reason is lowering rather than inference. There is **one
lowered body per source function**, and monomorphization clones a body whose
method calls have already been resolved. One call site therefore carries one
catalog row and one receiver type; a quantified receiver would be N receiver
types at one call site with nothing to lower. §5.2 states the same answer from
the other end: `total` is `Vec[Int] -> Int`.

If you need both, write two functions, or give the second one a closure to do
the arithmetic.

### An iterated parameter is generic in the *iterable* and not in its *element*

A `for` loop is the exception, and it splits the other way
([ADR-062](../../../decisions/062-an-iterated-parameter-is-generic-in-the-iterable-and-not-its-element.md)).
The collection stays quantified — MIR picks the runtime accessors from the
iterator's constructor, so one clone per iterable kind is the only way the
symbols can be right — while the item is pinned, for the same reason a method
receiver is.

```praxis
fn total(items) {
    var t = 0
    for i in items {
        t = t + i
    }
    t
}

var set = Set()
set.insert(4)

out(total([1, 2]))
out(total(0..5))
out(total(set))
```

```text
3
10
4
```

One function, three iterable kinds, three clones. Disagree about the *element*
instead and it is a mismatch, reported at the call that broke it with the `for`
as a note:

```praxis
fn show_all(items) {
    for i in items {
        out(i)
    }
}

show_all([1, 2])
show_all(["a", "b"])
```

```console
$ praxis check iterated-element-is-pinned.px --color never
error[Y001]: expected Int, found Text

  iterated-element-is-pinned.px:8:1
  8 | show_all(["a", "b"])
    | ^^^^^^^^^^^^^^^^^^^^ expected Int, found Text

note: this is the operation that requires it

  iterated-element-is-pinned.px:2:14
  2 |     for i in items {
    |              ^^^^^

praxis: 1 error(s)
```

The report is at the call, because `for i in items` is correct for every other
instantiation of `show_all`; the note says which operation imposed the
requirement. That two-span shape is the general form for anything a scheme
carried — see [Capabilities](capabilities.md).

A method *on the item* resolves exactly the same way, which is worth writing
down because it is the combination the example above does not cover — it does
arithmetic on the item rather than calling anything:

```praxis
fn widths(rows) {
    for row in rows {
        out(row.len())
    }
}

widths([[1, 2, 3], [4, 5]])
```

```text
3
2
```

### …and a value derived from a pinned receiver is pinned too

The pin reaches further than the parameter. A subscript's result, a method's
result and a `for`'s item are all pinned by the same rule, so a helper can
subscript twice with no annotation anywhere:

```praxis
fn pick(t, i, j) {
    t[i][j]
}

out(pick([[7, 8], [9, 10]], 0, 0))
```

```text
7
```

`pick` is `(Vec[Vec[Int]], Int, Int) -> Int`, reconstructed from two subscripts
and one call. And because the derived receiver is pinned, `pick` refuses a
second element type for exactly the reason `total` does above — calling it on a
`Vec[Vec[Text]]` in the same program is a `Y001` at the second call, not a
second clone.

This is [ADR-137](../../../decisions/137-a-deferred-receiver-resolves-in-rounds-and-the-channel-runs-to-a-fixpoint.md).
Resolving a deferred method *produces* the receiver's result type, and that
result is what the next link waits on, so the constraint channel discharges in
rounds until nothing is left to answer.
