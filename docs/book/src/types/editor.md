# Inference in the editor

Praxis programs are mostly unannotated, which means the types are real but
invisible. The language server's job here is to put them back on the screen: an
inlay hint beside every binding whose type the source does not state, a hover
that answers what any expression is, and an edit that writes a hint into the file
when you want it permanent.

`praxis lsp` is the server; it speaks LSP over stdio and is not meant to be run
by hand. Wiring it into an editor, and everything the extension does that is not
about types — semantic tokens, rename, code actions, completion, signature help —
is [Editor support](../tooling/editors.md). This chapter is about inference.

## What the hints say

```praxis
fn area(w, h) {
    w * h
}

var side = 4
out(area(side, side + 1))
```

Three bindings, no annotations, and the server answers with three hints:

| where | label | writes an edit |
|---|---|---|
| after `w`, line 1 | `: Int` | yes |
| after `h`, line 1 | `: Int` | yes |
| after `side`, line 5 | `: Int` | yes |

So `fn area(w, h)` reads on screen as `fn area(w: Int, h: Int)`, and `var side`
reads as `var side: Int`. A hint sits at the end of the name, which is where the
annotation would go.

The rule is one rule: **every binding whose type the source does not already
state**. A `fn` parameter, a closure parameter, a `var`, a `for` variable, and a
name a pattern introduces are all the same thing — a name bound to a value — and
they are all read off the same table inference filled. A binding you annotated
yourself gets no hint, because that would be the editor reading the source back
to you.

One thing that is not a binding is hinted as well: a `read` or `parse` whose
result nothing binds. `out(parse("1", int))` gets a `: Int` after the whole
expression, because there is no name to hang it on. When there is one, the
binding's hint already says it and the second is suppressed.

```praxis
var pairs = [(1, "a"), (2, "b")]
for (n, label) in pairs {
    out(label)
}
var f = |q| q + 1
out(f(1))
```

```text
a
b
2
```

Five hints on that file: `pairs: Vec[(Int, Text)]`, then `n: Int` and
`label: Text` from the tuple pattern in the `for` header, then
`f: (Int) -> Int` and `q: Int`. The destructured names are hinted individually
because each is a binding in its own right.

## A variable is shown, never hidden

```praxis
fn pair(item) {
    [item, item]
}

out("pair is declared but never called")
```

```text
pair is declared but never called
```

Nothing calls `pair`, so nothing says what `item` is — and that is not a gap in
the answer. `pair` generalizes to `forall T. (T) -> Vec[T]`, so the hint on
`item` is `: T`: the name `pair`'s own scheme gives the variable. Hover over
`pair` and you see the same `T`, because it is the same variable.

`?T` is the other case, and the question mark is the whole difference:

```praxis
var v = Vec()
out(v.len())
```

```text
0
```

`v` is `Vec[?T]`. The binding is expansive, so the value restriction does not
generalize it ([Generalization](generalization.md)), and nothing in the program
pins the element — so no scheme quantifies that variable and none is going to.
`?` says exactly that, in a hint as in hover as in `praxis check`'s own output,
where the previous chapter's `found (Text) -> ?T` is the same spelling.

Neither is hidden. Hiding one would make "no hint" mean two different things: a
type the source already states, and a type nothing named. Those are precisely the
two cases worth telling apart.

## Accepting a hint

A hint carries a text edit that inserts its own label at its own position, so
accepting it writes the annotation into the file. Accept all three from the first
example and you get exactly this, which behaves identically:

```praxis
fn area(w: Int, h: Int) {
    w * h
}

var side: Int = 4
out(area(side, side + 1))
```

```text
20
```

The three hints are gone from that version: the file states its own types now,
and repeating them back would be noise.

The edit is offered only where the annotation would be both **legal** and
**spellable**.

- Legal: on a `fn` or closure parameter, or a `var`. A `for` variable has no
  annotation syntax, so its hint shows and cannot be accepted — the `n` and
  `label` above are in that state.
- Spellable: the rendered type has to be one the parser reads back. `?T` is not,
  and neither is the `T` of a scheme — the language has no syntax for writing a
  type variable, so `pair`'s `item: T` above shows with no edit. Neither is an
  anonymous record. Neither is a function type, whose spelling this module
  deliberately does not guess — which is why `f: (Int) -> Int` above shows with
  no edit while `q: Int` beside it has one.

```praxis
var points = read lines(`{x:int},{y:int}`)
var first = points[0]
out(points.len() + first.x + first.y)
```

On the two-line input `1,2` / `3,4`:

```text
5
```

Neither hint on that file can be accepted, for the second reason:
`points: Vec[{ x: Int, y: Int }]` and `first: { x: Int, y: Int }` name
[anonymous records](structural-records.md), which the language has no annotation
syntax for. Showing a hint that cannot be applied is better than offering an edit
that would not compile.

There is a test in `crates/praxis-lsp/tests/m12.rs` —
`applying_a_hints_edit_keeps_the_file_clean` — that applies every edit a file's
hints carry and asserts the result still checks with no diagnostics. It is the
only thing that would catch an annotation the grammar refuses.

## Hover

Hover answers with the type, rendered by the same function `praxis check` prints
through. A second renderer here would be a second opinion about what
`Vec[{ x: Int }]` is called.

A hover answer is Markdown, and the type is inside a fenced `praxis` block so the
editor colours it. On `points` in the file above the server sends:

````text
```praxis
points: Vec[{ x: Int, y: Int }]
```
````

On the `len` of `points.len()` it sends the catalog row itself — receiver, name,
parameters, result — and the row's own documentation:

````text
```praxis
Vec[{ x: Int, y: Int }].len() -> Int
```

Number of elements in the vector.
````

That sentence is not written in the language server. It is the catalog entry's
`doc` field, taken from the entry method resolution actually selected, so the row
that runs and the sentence you read are the same row.

A [prelude](../language/prelude.md) name keeps its scheme and gains §16.1's own
sentence under it, and a name in **type position** — the `Int` in `var n: Int`,
the `Vec` in `Vec[Text]` — answers with what the type is. Neither sentence is
written in the language server either; both come from the same
`crates/praxis-stdlib/src/prelude.rs` table name resolution seeds the root scope
from.

````text
```praxis
abs: (Int) -> Int
```

Absolute value of an `Int`. Faults on `Int`'s minimum, which has no positive counterpart. `Float` has its own `x.abs()`.
````

The preference order is innermost-wins: a parser expression, then a method name,
then a name reference, then a declaration site, then a name in type position,
then the innermost expression node with a recorded type. The last of those is why
hover works on things that are not names at all — a list literal, a
subexpression, a call.

## Hover inside a `read`

```praxis
var groups = read sections(lines(`{a:int},{b:int}`))
out(groups.len())
```

On an input of two blank-line-separated groups:

```text
2
```

An input parser is a tree of constructors, and each node has a type of its own.
Hovering `sections` gives the constructor's signature, its documentation, and the
whole expression's result:

````text
```praxis
sections(parser) -> Vec[T]
```

Split the region on blank lines and apply the parser to each section. With named arguments, parses fixed sections in order into a record.

---

```praxis
Vec[Vec[{ a: Int, b: Int }]]
```

*input parser result*
````

Hovering the `lines` inside it gives **that node's** type, not the root's, and
the label under it says which of the two you are looking at:

````text
```praxis
lines(parser) -> Vec[T]
```

Split the region into lines and apply the parser to each. Every line must be consumed whole.

---

```praxis
Vec[{ a: Int, b: Int }]
```

*parser expression*
````

This works because inference keeps the parser AST it built, along with the
synthesized type of every node in it, keyed by span
([ADR-098](../../../decisions/098-the-parser-ast-is-retained-by-inference.md)).
The alternative was a second scanner over template interiors living in the
language server, free to disagree with the compiler about where a capture ends.
The index means "which parser node is the cursor in" is a lookup against spans
the compiler computed, so it cannot disagree.

## Two bindings with one name

```praxis
var value = "12"

if value.len() == 2 {
    var value = 12
    out(value + 1)
}

out(value + 1)
```

Hover over the `value` on line 1 and you get `value: Text`. Hover over the one on
line 4, or its use on line 5, and you get `value: Int`. Line 8 is `value: Text`
again — it is the outer binding, which the inner one shadowed only for the length
of the `if`.

Hints agree: two of them on this file, `: Text` on line 1 and `: Int` on line 4,
because those are the two declarations. This is worth knowing because
[the shadowing error in the previous chapter](errors.md#expected-and-found-are-positions-not-judgements)
is exactly the case where hovering the name is faster than reading upwards for
it. A name's identity in this compiler is its symbol, never its spelling, and
every editor feature keyed on identity — hover, rename, find-references, inlay
hints — reads that symbol.

## The editor and `praxis check` cannot disagree

That file reports one error at the terminal:

```text
error[Y001]: expected Text, found Int

  shadowed.px:8:13
  8 | out(value + 1)
    |             ^ expected Text, found Int

praxis: 1 error(s)
```

Open it in an editor and the server publishes one diagnostic: code `Y001`,
message `expected Text, found Int`, severity error, source `praxis`, over the
range that starts at line 8 column 13 and ends one character later. The same
code, the same message, the same span.

This is structural, not a coincidence that holds today.
[ADR-097](../../../decisions/097-the-shared-query-layer-lives-in-praxis-lsp.md)
put the front-end query layer in the `praxis-lsp` crate and made `praxis check`
call it: the CLI builds a snapshot of the file and asks it for diagnostics, and
the server's publish path does the same thing to the same snapshot type. Which
diagnostics exist, what order they come in, and whether a file whose parse
already failed still gets analyzed are decided in one place. A divergence is not
unlikely; it is unrepresentable.

Two consequences worth knowing:

- **A file with a syntax error still gets its type errors.** Parse recovery keeps
  the tree usable, and the editor going blank on one stray character is worse
  than a slightly confused analysis.
- **Nothing is executed to produce them.** The language server's manifest does
  not depend on the MIR, code generation or runtime crates at all, and a test
  reads the manifest and asserts it — so "diagnostics without running your
  program" holds by construction rather than by observation.

## What is memoized

A snapshot is one file at one revision, and it runs the parse once and inference
once no matter how many questions you ask it. Hover, hints, diagnostics and
go-to-definition on an unedited file all read the same analysis. An edit builds a
new snapshot and drops the old one — with its tree, its types and its source map
together — which is what keeps an editor session that has been open for an hour
from holding an hour of keystrokes.
