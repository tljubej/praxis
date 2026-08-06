# Templates and captures

A backtick template describes one piece of input by looking like it. The
characters between the backticks are the fixed text the input must have, and
`{...}` marks the places where the interesting parts are.

```praxis
read lines(`{x1:int},{y1:int} -> {x2:int},{y2:int}`)
```

That reads `0,9 -> 5,9` and produces `{ x1: 0, y1: 9, x2: 5, y2: 9 }`, one per
line. The template is the whole specification: no split, no trim, no index
arithmetic, and the record's fields are the names you wrote.

Two things are going on in it.

- **Literal text** — `,`, ` -> ` — must be matched by the input. Punctuation and
  words match exactly; a run of spaces has [its own rules](whitespace.md).
- **A capture** — `{x1:int}` — hands a stretch of input to a parser and keeps
  what it produces.

## Named and anonymous captures

A capture is `{name:parser}` or just `{parser}`. Which one you write decides the
shape of the result, and the rule is read off the template's own parts
([ADR-092](../../../decisions/092-a-templates-shape-is-read-from-its-parts.md)).

| template | result |
|---|---|
| no captures | `Unit` |
| one anonymous capture | the captured value itself |
| two or more anonymous captures | a tuple, in order |
| named captures | an anonymous record with those field names |

```praxis
// §7.3's four shapes, read off the template's own parts (ADR-092): no capture
// is Unit, one anonymous capture is that value, two or more are a tuple, and
// named captures are an anonymous record.
fn main() {
    out(parse("hello", `hello`))
    out(parse("42", `{int}`))
    out(parse("1,2", `{int},{int}`))
    out(parse("1,2,x", `{int},{int},{word}`))
    out(parse("x=1", `{name:word}={v:int}`))
}
```

```text
Unit
42
(1, 2)
(1, 2, x)
{ name: x, v: 1 }
```

Anonymous captures are for when the position says everything — coordinate pairs,
two-column tables. Named captures are for everything else, and they are what
makes the rest of the program readable: `s.x1` beats `s.0` the moment there are
more than two of them.

The tuple is an ordinary tuple, all the way down into the collection that holds
it:

```praxis
// A multi-capture template's value is an ordinary tuple, all the way into the
// collection that holds it: it renders and compares like one built by hand.
fn main() {
    var pairs = read lines(`{int},{int}`)
    var same = Vec()
    same.push((1, 2))
    same.push((3, 4))
    out(pairs)
    out(pairs == same)
}
```

Over `1,2\n3,4\n`:

```text
[(1, 2), (3, 4)]
true
```

The record is an [anonymous record](../types/structural-records.md): its type is
its field names and their types, and nothing had to be declared.

**Named and anonymous captures may not be mixed** in one template — the result
would have to be a record and a tuple at once.

```praxis
// Named and anonymous captures may not be mixed in one template: the shape
// would have to be a record and a tuple at once.
fn main() {
    out(read lines(`{x:int},{int}`))
}
```

```text
error[I020]: named and anonymous captures may not be mixed in one template (§7.3)

  template-mixed-captures.px:4:20
  4 |     out(read lines(`{x:int},{int}`))
    |                    ^^^^^^^^^^^^^^^ named and anonymous captures may not be mixed in one template (§7.3)

praxis: 1 error(s)
```

Two captures with the same name is `I021`, for the same reason a record cannot
have two fields called `x`.

## A capture body is a parser expression

The `:` in `{name:parser}` is followed by a **whole parser expression**, not
just an atomic name
([ADR-072](../../../decisions/072-a-template-capture-body-is-a-parser-expression.md)).
Constructor calls, string arguments, and templates of their own all go inside
the braces.

```praxis
// A capture body is a whole parser expression (ADR-072), not just an atomic
// name: a constructor call, a call with a string argument, a `}` inside that
// string, and a template of its own all sit inside `{...}`.
fn main() {
    out(parse("Monkey 0: 79, 98", `Monkey {id:int}: {items:csv(int)}`))
    out(parse("a-b-c", `{parts:sep("-", word)}`))
    out(parse("}", `{c:one_of("}")}`))
    out(parse("at 3,4", `at {p:`{x:int},{y:int}`}`))
}
```

```text
{ id: 0, items: [79, 98] }
{ parts: [a, b, c] }
{ c: } }
{ p: { x: 3, y: 4 } }
```

The scanner finds the end of a capture by tracking depth rather than by looking
for the first `}`, so a `}` inside a string, a `,` inside a call, and a nested
backtick run all stay inside the capture where you wrote them. That is why line
three works: `one_of("}")` is a legal body, and the `}` in its argument does not
close anything.

A name is split off only at a `:` at depth zero, so
`{g:choice(A: word, B: int)}` is a capture *named* `g` whose body is a `choice`
— the colons inside the call are not candidates.

Whitespace around the name is trimmed: `{ n :int}` names `n`. The name itself
must be an identifier, by the same rule that decides what a Praxis binding may be
called, so `{2x:int}` is `I011`. A body naming nothing the compiler recognizes is
`I012` — `{value:intr}` reports *unknown parser `intr`* and suggests `int` — and
a body calling a constructor that does not exist is `I013`. There is no default:
a capture whose kind is unrecognized fails the compile rather than quietly
becoming an `int`.

## A capture is bounded by what follows it

A capture does not take everything it could. It is handed a **region** that ends
where the run of literal parts after it can first match, and it must fill that
region
([ADR-079](../../../decisions/079-a-grid-cell-is-what-its-cell-parser-reads.md)).
"Earliest" is what makes `text` non-greedy, and it applies to every capture, not
only the `text` ones.

The bound is the earliest position at which the **whole run of literal parts up
to the next capture** can match. A run that can match the empty string —
nothing at all, or a `\s*` — constrains nothing, and then the capture takes the
rest of its region.

```praxis
// Every capture is bounded by the run of literal parts that follows it, and
// the bound is the earliest place that whole run can match (ADR-079). A run
// that can match nothing — `\s*`, or nothing at all — is no bound.
fn main() {
    out(parse("x y bar", `{a:text} bar`))
    out(parse("x y bar", `{a:text}\s+bar`))
    out(parse("x  bar", `{a:text}\s*bar`))
    out(parse("aaa", `{a:text}a{b:rest}`))
}
```

```text
{ a: x y }
{ a: x y }
{ a: x }
{ a: , b: aa }
```

Read those four in order:

1. `{a:text} bar` stops `a` at the space before `bar`, not at the first space.
   The bound is where the *run* matches, and the run is a space and then `bar`.
2. `{a:text}\s+bar` is the same policy spelled differently and reads the same
   input the same way. The two spellings used to disagree; they do not now.
3. `{a:text}\s*bar` bounds `a` at `x`, because the earliest place the whole run
   can start is right after it: `\s*` eats the two spaces and `bar` lands. The
   spaces belong to the policy, not to `a`.
4. `{a:text}a{b:rest}` stops at the *first* `a`, which is position zero, so `a`
   is empty. Non-greedy means non-greedy.

The last capture in a template has nothing after it, so nothing bounds it: it
takes the rest of its region and stops where its own parser stops. That is why a
root-level template does not fault on the file's trailing newline.

A capture is offered the bytes at the cursor including its own leading
whitespace — whether to skip that is [the child's decision](atoms.md#leading-spaces-and-tabs),
not the template's. What the leading run does *not* do is bound the capture; see
[whitespace](whitespace.md#a-capture-is-not-bounded-by-its-own-leading-whitespace).

## A template ends at the line it opens on

A raw newline may not appear inside a template
([ADR-094](../../../decisions/094-a-template-ends-at-the-line-it-opens-on.md)).
`\n` is how a template matches a line ending, and it is the only way — which is
also how a template reaches a second line.

```praxis
// A template ends at the line it opens on (ADR-094), so `\n` is the only way
// it matches a line ending — and the only way one reaches a second line. The
// escape matches CRLF as well; a raw newline never did.
fn main() {
    out(parse("1\n2\n", `{a:int}\n{b:int}`))
    out(parse("1\r\n2\n", `{a:int}\n{b:int}`))
}
```

```text
{ a: 1, b: 2 }
{ a: 1, b: 2 }
```

The escape matches CRLF as well as LF. A raw newline in a template never did —
it was whitespace but not a space, so it fell through to literal text and matched
LF only. The rule removes that trap; the replacement is one character longer.

It also bounds the report when a template is left open. The run cannot outlive
its line, so an unterminated backtick names one line instead of the rest of the
file:

```praxis
// The same rule bounds the report when a template is left open: the run ends
// at the line's end, so `T002` names one line and there is no cascade.
fn main() {
    var v = read `{int`
    out(v)
}
```

```text
error[T002]: unterminated backtick template

  template-unterminated.px:4:18
  4 |     var v = read `{int`
    |                  ^^^^^^ unterminated backtick template

praxis: 1 error(s)
```

One error, not a cascade. The `}` closing the enclosing block is no longer
swallowed by the token, so the parser and the type checker never see the damage.

## A template is a parser expression everywhere or nowhere

Backticks mean "parser expression" and nothing else. A template outside `read`
and `parse` has nothing to read from, so it is a diagnostic rather than a `Text`
that happens to contain braces
([ADR-084](../../../decisions/084-a-template-is-a-parser-expression-everywhere-or-nowhere.md)).

```praxis
// A backtick template is a parser expression everywhere or nowhere (ADR-084).
// Outside `read` and `parse` it has nothing to read from, so it is an error
// rather than a `Text` that happens to contain braces.
fn main() {
    var t = `n = {int}`
    out(t)
}
```

```text
error[Y023]: a backtick template is a parser expression; write `read` before it, or pass it to `parse(text, ...)`

  template-value-position.px:5:13
  5 |     var t = `n = {int}`
    |             ^^^^^^^^^^^ a backtick template is a parser expression; write `read` before it, or pass it to `parse(text, ...)`

praxis: 1 error(s)
```

This used to type-check and print `n = {int}` — a program that asked to parse an
integer printing the word `{int}`. The message names the fix, because the fix is
always the same word.

A backtick is never a way to build text. `"..."` is the text literal, and the
braces in it are §8.1's [interpolation](../language/text.md#interpolation--renders-a-value)
— `"n = {n}"` renders `n`. The two mechanisms share nothing but the character:
a template's `{name:parser}` names a capture in the input-parser DSL, while a
literal's `{expr}` is an ordinary Praxis expression rendered into text.

## Escapes

Inside a template, `` \` `` is a backtick and `\\` is a backslash. `\n`, `\t`,
`\x20`, `\s*` and `\s+` are whitespace policies and are covered in
[Whitespace, lines and positions](whitespace.md). Anything else after a backslash
is an error naming exactly the sequence you wrote.

A double quote in literal text is a double quote: a string literal is only a
thing *inside* a capture body, so `` `He said "hi" {x:int}` `` is an ordinary
template.

```praxis
// Backticks and backslashes take ordinary escapes; a quote inside literal text
// is just a quote, because a string literal is only a thing inside a capture.
fn main() {
    out(parse("a`b 3", `a\`b {x:int}`))
    out(parse("a\\b 4", `a\\b {x:int}`))
    out(parse("He said \"hi\" 5", `He said "hi" {x:int}`))
}
```

```text
{ x: 3 }
{ x: 4 }
{ x: 5 }
```

## Where templates go

A template is a parser expression, so it goes anywhere one does: as the whole
operand of `read`, as the child of [`lines`, `sections`, `ws`, `sep` or
`scan`](structural.md), as a `block` item, as a `choice` case, and — as above —
inside another template's capture.

The one thing to know about a template inside a `block` is that it is offered
its own line plus one more for each `\n` it writes, where every other kind of
item is offered the rest of the region. That rule is
[`block`'s](structural.md), and it is why a template with a trailing capture
does not swallow the item after it.

For what each of these produces as a type, see
[How a parser gets its type](type-derivation.md).
