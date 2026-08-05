# What inference does

No binding, parameter, return type or expression in Praxis needs a type
annotation. Every one of them has a static type all the same; the compiler works
them out from what the program does with the values. A complete solution can be
written without the word `Int` appearing in it — across the corpus in
`tests/aoc-corpus/` the commonest annotation is `fn main() -> Int`, and several
programs have none. Only a `struct` field and an `enum` payload must say what
they hold, because a declaration is where a type is *stated* rather than
deduced.

The engine is Hindley–Milner inference extended with what the language actually
has: mutable bindings, nominal records and enums, the structural records the
input parser derives, collection constructors, closures, and a small closed set
of internal requirements (§5.1). Types live in an interned arena — a type is a
32-bit handle into a table, so every expression can carry one for free, and a
type *variable* is a slot in that same table rather than a separate kind of
thing ([ADR-007](../../../decisions/007-type-representation-interning.md)).

Here is the design document's own promise, §5.2, as a program:

```praxis
fn total(values) {
    values.sum()
}

var values = [1, 2, 3]
out(total(values))
```

```text
6
```

Nothing is annotated. `[1, 2, 3]` makes `values` a `Vec[Int]`; passing it to
`total` makes `total`'s parameter a `Vec[Int]`; `sum` on a `Vec[Int]` makes the
result an `Int`. Information flows in the other direction too — had `values`
been built empty and pushed into afterwards, the `push` would have decided the
element type just the same.

## Asking the compiler what it inferred

Inference is only pleasant if you can see its answers. There are three ways to
get them, and all three print the same rendering.

**The editor is the everyday one.** The language server writes an inlay hint at
every binding the source does not annotate — parameters, `var`s, `for`
variables, names a pattern introduces — and hover gives the full scheme. That is
[its own chapter](editor.md).

**A deliberate mismatch works anywhere.** Feed a function an argument it cannot
take, and the diagnostic prints the signature inference derived:

```praxis
fn add(a, b) {
    a + b
}

out(add(1, 2))
out(add("one", 2))
```

```console
$ praxis check inferred-signature.px --color never
error[Y001]: expected (Int, Int) -> Int, found (Text, Int) -> ?T

  inferred-signature.px:6:5
  6 | out(add("one", 2))
    |     ^^^^^^^^^^^^^ expected (Int, Int) -> Int, found (Text, Int) -> ?T

praxis: 1 error(s)
```

`expected` is `add`'s inferred type — `(Int, Int) -> Int`, derived from `+`
alone. `found` is the function type this call site would need. Reading a
signature out of a mismatch is a habit worth having; the
[error chapter](errors.md) covers the rest of what these reports say.

**The crash debugger answers directly.** `type EXPR` type-checks an expression
against the faulted frame's locals and prints the result without running it:

```praxis
fn inspect(values, total, name) {
    panic("stopping here on purpose")
}

var values = Vec[Int]()
values.push(3)
values.push(4)
inspect(values, values.sum(), "run")
```

Driven with `type values`, `type total`, `type name`, `type values.sorted()`,
`quit`:

```text
error: program faulted: panic: stopping here on purpose

Backtrace:
#0   inspect__Vec_Int__Int_Text
#1   <entry>

  locals:
    values: Vec[Int] = [3, 4]
    total: Int = 7
    name: Text = run
  temps:
    <tmp#4: Text> @ ""stopping here on purpose"" = stopping here on purpose
    <tmp#5> @ "panic("stopping here on purpose")" = Unit
Entered crash debugger. 2 frame(s). Type `help` for commands.
Praxis crash> type values
Vec[Int]
Praxis crash> type total
Int
Praxis crash> type name
Text
Praxis crash> type values.sorted()
Vec[Int]
Praxis crash> quit
```

The frame's `locals` block already names each binding's type; `type` extends the
same question to expressions the program never wrote. The frame name in the
backtrace — `inspect__Vec_Int__Int_Text` — is the monomorphized clone the call
site selected, which is the inferred signature written a third way.

## What a type variable is, and how it prints

An unsolved type is a **type variable**: a slot in the arena that nothing has
said anything about yet. Inference mints one whenever it needs a type it does
not know — a fresh parameter, an empty collection's element, the result of a
call it has not resolved — and links it when the program constrains it.

A variable renders with a leading question mark: `?T`, `?U`, `?V`. That is the
honest spelling for "still open", and it is the same one in a diagnostic, in
hover, and in an inlay hint. In `found (Text, Int) -> ?T` above, `?T` is the
result `add` would have to produce; the call site never constrained it, because
the argument disagreed first.

A variable a *scheme* quantifies prints without the question mark. `fn greet(name)
{ "hi" }` is `forall T. (T) -> Text`: nothing constrains `name`, so the type is
generic in it. The two spellings are one distinction — a bound variable versus a
free one — and which one you get is the subject of the
[generalization chapter](generalization.md). Only a scheme knows which of its
variables it binds, which is why a type printed on its own shows every variable
as `?`
([ADR-047](../../../decisions/047-scheme-owned-binders-and-the-level-newtype.md)).

## Unification

Every rule above is one mechanism: **unification**. Two types are made equal, or
the attempt is a diagnostic. Unifying a variable with a type links the slot;
unifying two concrete types recurses into their parts; unifying two things that
cannot be the same is `Y001`, whose `expected` half is always the requirement
and whose `found` half is always what the program wrote.

Three of unification's failures carry their own codes, because a generic
expected/found would bury the mistake.

A call with the wrong number of arguments is `Y024`, not a whole-signature
mismatch to diff by eye:

```praxis
fn add(a, b) {
    a + b
}

out(add(1, 2, 3))
```

```console
$ praxis check wrong-arity.px --color never
error[Y024]: this function takes 2 argument(s), but 3 were given

  wrong-arity.px:5:5
  5 | out(add(1, 2, 3))
    |     ^^^^^^^^^^^^ this function takes 2 argument(s), but 3 were given

praxis: 1 error(s)
```

A unification that would make a type contain itself is `Y002`. The occurs check
is what stops inference from looping:

```praxis
fn apply_to_self(f) {
    f(f)
}

out(1)
```

```console
$ praxis check infinite-type.px --color never
error[Y002]: an infinite type would be required here

  infinite-type.px:2:5
  2 |     f(f)
    |     ^^^^ an infinite type would be required here

praxis: 1 error(s)
```

A type constructor written with the wrong number of arguments is `Y007`:
`Vec[Int, Text]` reports at the annotation rather than interning quietly.

## Annotations are optional, and legal

Nothing above forbids writing the type down. An annotation is checked by
unification like everything else — it is a *requirement*, not a substitution, so
writing one can only reject programs, never change what an accepted one means.

```praxis
struct Point { x: Int, y: Int }

fn shift(p: Point, dx: Int, dy: Int) -> Point {
    Point { x: p.x + dx, y: p.y + dy }
}

var origin: Point = Point { x: 0, y: 0 }
var counts: Map[Text, Int] = Map()
var double: (Int) -> Int = |n: Int| n * 2
var pair: (Int, Text) = (1, "one")

counts.insert("moves", 2)
out(shift(origin, 3, 4))
out(counts.get("moves"))
out(double(21))
out(pair)
```

```text
{ x: 3, y: 4 }
Some(2)
42
(1, one)
```

The positions that take one:

| Position | Spelling | Optional? |
|---|---|---|
| Binding | `var name: T = …` | yes |
| Function parameter | `fn f(a: T, b: U)` | yes |
| Function result | `fn f(…) -> T` | yes |
| Closure parameter | `\|n: T\| …` | yes |
| `struct` field | `struct S { f: T }` | **required** |
| `enum` variant payload | `enum E { V(T) }` | **required** |

A type is a scalar name (`Int`, `UInt`, `Byte`, `Float`, `Bool`, `Char`,
`Text`), a declared `struct` or `enum` name, a type constructor with its
arguments (`Vec`, `Deque`, `Map`, `Set`, `Counter`, `MinHeap`, `MaxHeap`,
`Grid`, `Option`, and the nullary `Range` and `BitSet`), a tuple `(A, B)`, a
function `(A) -> B`, or `Unit`. A parenthesized type is the type it groups, and
`()` is `Unit`, so `() -> Int` takes no arguments.

There is no annotation syntax on a `for` variable or on a name a pattern binds —
`for x: Int in v` is a parse error. Those are the two places the editor shows a
hint it cannot offer to write into the file.

There is also no annotation form for a `read`: a parser expression's shape *is*
its type, which is [type derivation](../input/type-derivation.md).

## What inference will not do for you

**A name has one signature.** Two functions cannot share a name, a call cannot
select between arities, and no parameter has a default
([ADR-089](../../../decisions/089-a-name-has-one-signature.md)). Where another
language would overload, Praxis spells the second shape as a second name: `min`
and `min_by`, `find` and `position`, `sorted` and `sorted_by_key`.

**Recursion is checked; mutual recursion generalizes early.** A directly
recursive function needs no annotation — `fn fact(n) { if n <= 1 { 1 } else { n *
fact(n - 1) } }` comes out `(Int) -> Int` — because the name is bound to a
placeholder before its body is checked and the placeholder is unified with the
derived type after. A call to a function declared *below* unifies against the
very placeholder that later declaration will resolve, so a disagreement is
reported rather than skipped — reported wherever the conflict surfaces, which
for a forward call is usually inside the callee's body rather than at the call
([ADR-047](../../../decisions/047-scheme-owned-binders-and-the-level-newtype.md)).
A mutually recursive pair is checked in both directions, but the first of the
two generalizes before the second has finished constraining it; annotate a
mutually recursive pair if it misbehaves.

**A receiver a method was called on is pinned.** `fn total(values) {
values.sum() }` is `(Vec[Int]) -> Int` once one call site says `Vec[Int]`, not
"any sequence of numbers". That is deliberate, and it is explained in
[Generalization](generalization.md).

**Nothing is inferred across files.** A program is one file (§3.2).

---

The rest of this part takes the four pieces in turn:
[Generalization](generalization.md) is which variables get quantified and which
do not; [Method resolution](method-resolution.md) is how `.name()` finds its
row; [Capabilities](capabilities.md) is the closed set of things the compiler
decides about a type; and [Records without names](structural-records.md) is what
the input parser's derived record types are and how they relate to a `struct`.
