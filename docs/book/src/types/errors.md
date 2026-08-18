# Reading a type error

`praxis check` runs the whole front end — lex, parse, name resolution, inference
— and prints everything it found, sorted by position. A type error tells you
three things: a code, the two types unification was trying to make equal, and the
expression it blames. Most of the work of reading one is knowing which of those
two types came from where, because it is often not the line under the carets.

## The shape of a diagnostic

```praxis
fn checksum(rows: Vec[Int]) -> Int {
    out(rows.len())
}

out(checksum([3, 1, 2]))
```

```text
error[Y001]: expected Int, found Unit

  unit-body.px:2:5
  2 |     out(rows.len())
    |     ^^^^^^^^^^^^^^^ expected Int, found Unit

help: this value is `Unit`; the function body expected `Int` — make the last expression produce a value, or change the declared type to `Unit`

praxis: 1 error(s)
```

Five parts, in order:

- **`error[Y001]`** — the severity and the code. The letter is the category: `T`
  lex, `P` parse, `N` name resolution, `Y` type, `I` input parser. (`R`, runtime,
  is a declared category with no members.) A code is a permanent identifier and
  is never reissued, even when the report behind it is retired;
  [Diagnostic codes](../tooling/diagnostics.md) is the list.
- **The message.** It appears twice — in the header, and again as the label after
  the carets — so a diagnostic reads the same whether you are looking at the top
  of it or at the line.
- **`file:line:col`.** Both numbers count from one.
- **The carets**, under the primary span. A span crossing several lines is
  underlined on each of them; a long one shows its first three lines and its
  last, with an ellipsis between.
- **`help:`**, when there is a concrete suggestion. An advisory one is a
  sentence. A machine-applicable one prints its replacement text on an indented
  line underneath, and that is what the editor offers as a quick fix.

A `note:` block, when a diagnostic has one, sits between the snippet and the
`help:` and carries a second span with its own snippet. It is how a report says
"the mistake is here, and the requirement it broke was written over there" — see
[capabilities](#a-capability-the-type-does-not-have), below.

The trailer counts errors, and the exit code follows it: 1 if there were any,
0 if not.

## `expected` and `found` are positions, not judgements

Unification is symmetric — it makes two types equal and neither is privileged.
The *message* is not symmetric, and the rule is mechanical: the type the context
already required is printed first, and the type the expression just brought is
printed second. An annotation comes before its initializer, a parameter before
its argument, a comparison's left operand before its right.

Arithmetic does not go by position at all. `+ - * / %` settle on one target type
for the whole operation — `Text` if either operand is a `Text`, `Float` if either
is a float, `Int` otherwise — and check both operands against that. The type
printed first is the operator's, and the blame falls on whichever operand
disagrees with it, on whichever side that operand stands:

```praxis
var value = "12"
out(1 + value)
```

```text
error[Y001]: expected Text, found Int

  operand-order.px:2:5
  2 | out(1 + value)
    |     ^ expected Text, found Int

praxis: 1 error(s)
```

The `Text` came from `value`, which is what made this a `Text` addition. The
`Int` is the literal, and the carets are under it even though it is on the left.

So `expected` does not mean "what you wanted". It means "what inference had
already decided by the time it got here", and when that decision is the wrong
one, the error lands downstream of it.

```praxis
var value = "12"

if value.len() == 2 {
    var value = 12
    out(value + 1)
}

out(value + 1)
```

```text
error[Y001]: expected Text, found Int

  shadowed.px:8:13
  8 | out(value + 1)
    |             ^ expected Text, found Int

praxis: 1 error(s)
```

The blame is on `1`, an integer literal that is not wrong about anything. The
`Text` in the message comes from line 1. Line 4 declares a *second* binding
called `value`, and it goes out of scope with the `if` — so the last line is
about the first `value`, which never stopped being a `Text`.

Shadowing is legal and deliberate: a `var` may redeclare a name in the same scope
or in an inner one, and the two are different bindings with different types. That
is what you want when you are narrowing a value step by step, and a trap when you
did not mean it. The editor tells them apart — hover over each `value` above and
you get `value: Text` and `value: Int`, because a name's identity is its symbol
and not its spelling. See [Inference in the editor](editor.md).

## A wrong argument blames the whole call

```praxis
fn double(n: Int) -> Int {
    n * 2
}

var raw = "21"
out(double(raw))
```

```text
error[Y001]: expected (Int) -> Int, found (Text) -> ?T

  wrong-argument.px:6:5
  6 | out(double(raw))
    |     ^^^^^^^^^^^ expected (Int) -> Int, found (Text) -> ?T

praxis: 1 error(s)
```

This shape surprises people, so it is worth knowing why it happens. A call to a
named function does not check its arguments one at a time. It builds the function
type the call site implies — `(the argument types) -> ?result` — and unifies the
callee against it in one step. The span is therefore the whole call, and the two
types in the message are two whole function types.

Read it by diffing them left to right. `(Int) -> Int` against `(Text) -> ?T`: the
first parameter is where they part, so the first argument is the one to look at.
`?T` is not a mistake in your program — it is the fresh variable standing for the
call's result, which nothing pinned because unification stopped before reaching
it. Every `?` in a rendered type means the same thing: a variable inference has
not resolved.

A *method* call is dispatched through the catalog instead, which unifies the
parameters one by one, so a method's wrong argument is blamed on the argument
itself. `element-pinned.px`, below, is that shape.

## The wrong number of arguments: `Y024`

```praxis
var total = 41 + 1
assert(total == 42, "the total is wrong")
out(total)
```

```text
error[Y024]: this function takes 1 argument(s), but 2 were given

  wrong-arity.px:2:1
  2 | assert(total == 42, "the total is wrong")
    | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ this function takes 1 argument(s), but 2 were given

praxis: 1 error(s)
```

A name in Praxis has exactly one signature. There is no arity-based overloading,
no optional parameters and no default arguments, so a count mismatch is never a
near miss for another version of the name — it is arithmetic, and `Y024` says so
rather than showing two function types to compare by eye.

The same rule settles the case above. `assert` takes a condition and nothing
else; the name that carries a sentence is `panic`. A failed `assert` already
prints the condition's own source text, its evaluated value and every local in
the frame, which is more than a hand-written message would have said.

`Y024` is raised inside unification rather than at the call site, so calling a
closure value with the wrong number of arguments reports it too, not just a call
to a named `fn`. It fires when the two function types being matched are the ones
that differ in length; a wrong-arity closure *passed as an argument* is a
mismatch one level down, and comes back as the whole-type `Y001` above.

## A method that does not exist: `Y110`

```praxis
var counts = Map[Text, Int]()
counts.insert("ada", 1)
out(counts.contain("ada"))
```

```text
error[Y110]: no method `contain` on type `Map[Text, Int]` taking 1 argument(s)

  no-such-method.px:3:12
  3 | out(counts.contain("ada"))
    |            ^^^^^^^ no method `contain` on type `Map[Text, Int]` taking 1 argument(s)

help: did you mean `contains`?
      contains

praxis: 1 error(s)
```

The message names the receiver's type *and* the arity, because both are part of
what selects a method: `v.get(0)` and `v.get()` are different questions about one
name. The `help:` is a machine-applicable fix — the label is the sentence, the
indented line under it is the replacement — and it is offered only when what you
wrote is within an edit distance of `max(1, len / 3)` of a real name. Two short
names that differ everywhere are not neighbours, so no suggestion appears.

When nothing has pinned the receiver there is no type to name, and the message
says the other true thing instead:

```praxis
fn shout(word) {
    word.upper()
}

out("nothing calls shout")
```

```text
error[Y110]: no type has a method `upper` taking 0 argument(s)

  method-nowhere.px:2:10
  2 |     word.upper()
    |          ^^^^^ no type has a method `upper` taking 0 argument(s)

praxis: 1 error(s)
```

Nothing calls `shout`, so nothing says what `word` is — and the call is refused
anyway, because the method catalog is the *complete* method universe of this
language. There is no user-defined `impl`, a record carries no methods, and so a
name the catalog holds at no arity can never resolve against any receiver.
Waiting for a call site would be waiting forever.

The rule is narrow on purpose. It asks "does any row hold this name at this
arity", not "does any row match this receiver", so
`fn total(values) { values.sum() }` still checks with no call site: `sum` exists,
the requirement is deferred, and a caller answers it later. Only a name that
exists nowhere is refused early, and inference is what refuses it. `Y110` has
one emitter, and it is not lowering's, so `praxis check` sees every missing
method without compiling anything.

[Method resolution](method-resolution.md) covers how a receiver picks a row.

## A capability the type does not have

Some requirements are not "this type equals that type" but "this type can do
this". They are recorded where the operation is written and discharged when
something finally says what the type is — so the two ends can be a long way
apart, and the diagnostic shows both.

```praxis
fn largest(values) {
    var best = 0
    for v in values {
        if v > best {
            best = v
        }
    }
    best
}

out(largest(42))
```

```text
error[Y005]: values of type `Int` cannot be iterated

  not-iterable.px:11:5
  11 | out(largest(42))
     |     ^^^^^^^^^^^ values of type `Int` cannot be iterated

note: this is the operation that requires it

  not-iterable.px:3:14
  3 |     for v in values {
    |              ^^^^^^

praxis: 1 error(s)
```

The primary span is the call, because the call is what supplied the offending
type. The `note:` is the `for` that wanted an iterable in the first place. Read
the two as one sentence: *this argument cannot be iterated, and here is the loop
that needs to iterate it.*

The wording never names the mechanism. There is no "`Int` does not implement
`Iterable`" in this compiler; a message says what the program did and what the
type cannot do. The family:

| code | when |
|---|---|
| `Y004` | compared with `==`, and its values cannot be compared |
| `Y005` | iterated, and it is not iterable |
| `Y006` | ordered — sorted, heaped, `<` — and it has no ordering |
| `Y014` | used as a `Map` key or `Set` element, and it can change after it is stored |
| `Y015` | used in arithmetic, and it is not numeric |
| `Y016` | given an operator the language does not define for it |

`Y016` is not a mismatch: both operands agree and the operation still has no
meaning. [Capabilities](capabilities.md) is the chapter on what each one
requires.

## A value used at two types

```praxis
var widths = []
widths.push(3)
widths.push("4")
out(widths.len())
```

```text
error[Y001]: expected Int, found Text

  element-pinned.px:3:13
  3 | widths.push("4")
    |             ^^^ expected Int, found Text

praxis: 1 error(s)
```

`[]` mints a `Vec` whose element type is a variable. Line 2 pins that variable to
`Int` — a variable resolves once — and line 3 is where the consequence is
noticed. The blame is on the second use and the decision was made at the first,
which is the general shape of this error: when the type in `expected` is not one
you wrote down anywhere, look for the earlier line that implied it.

A `fn` is different. It can be used at several types, because its scheme is
generalized at the declaration and instantiated fresh at every call.
[Generalization](generalization.md) is the chapter on where that line falls.

## Text and numbers

The most common mismatch in a puzzle program is a `Text` where an `Int` was
wanted, and it carries a `help:`:

```praxis
var raw = "12"
var count: Int = raw
out(count)
```

```text
error[Y001]: expected Int, found Text

  text-to-number.px:2:18
  2 | var count: Int = raw
    |                  ^^^ expected Int, found Text

help: this is `Text`; `.int()` answers `Option[Int]`, so take it apart with `match` (or use `read lines(int)`)

praxis: 1 error(s)
```

Both halves of that help are real, and they answer different questions.

`Text.int()` reads the number a text spells, and `Text.float()` is its twin.
Both answer an [`Option`](../language/enums.md#option) rather than the scalar,
because a text that is not a number is *absence* and not a fault — input is
routinely not what a program hoped, and a conversion that crashed would give you
no way to ask first.

```praxis
var raw = "12"
var count = match raw.int() { Some(n) => n, None => 0 }
out(count + 1)

// Whitespace is trimmed; anything that is not a number is `None`.
out(" 42 ".int())
out("abc".int())
out("1.5".float())
```

```text
13
Some(42)
None
Some(1.5)
```

What counts as a number is **the input parser's answer**, not a second one: the
two methods run the same scanner the `int` and `float`
[atoms](../input/atoms.md) do, over the whole trimmed text. So `"1 2"`,
`"12abc"`, `"0x10"` and a value past `Int`'s range are `None` — and so are
`"+5".int()` and `"inf".float()`, which surprise people until you know where the
rule comes from.

The other half is the [input parser](../input/read.md), and it is the one to
reach for when the text came from input in the first place: `read lines(int)`
never produces the value at all if the line is not a number, and reports where
it broke.

```praxis
var raw = "12"
var count = parse(raw, int)
out(count + 1)
```

```text
13
```

## A name that is not defined

Not a type error, but the one you will hit beside them, and the clearest example
of a machine-applicable fix:

```praxis
var total = 0

for n in [1, 2, 3] {
    totl += n
}

out(total)
```

```text
error[N001]: `totl` is not defined

  misspelled.px:4:5
  4 |     totl += n
    |     ^^^^ `totl` is not defined

help: did you mean `total`?
      total

praxis: 1 error(s)
```

`N0xx` is the name-resolution category: a name that is not in scope, a name in
type position that names a value, a second `fn` of a name already declared (a
`var` may redeclare, a `fn` may not), a `fn` body reaching for a binding declared
outside it. They are mistakes about what was
*declared*, which is why they are not `Y0xx` — there is no pair of types that
failed to unify.

## `check`, `run` and the editor cannot disagree

Every diagnostic in this chapter is produced by `praxis check`, and `praxis run`
prints exactly the same text before declining to run the program. Both exit 1.

That is not discipline, it is construction. The set of diagnostics, their order,
and the decision to analyze a file even when parsing has already complained are
stated once, in the query layer that both commands call. The editor calls the
same query, so the squiggle under your cursor carries the same code, the same
message and the same span the terminal would print. That is the subject of the
[next chapter](editor.md).
