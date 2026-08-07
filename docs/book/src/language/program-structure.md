# A file is a program

A `.px` file is a whole program. Its top-level statements are what runs, in
source order, and there is no entry-point ceremony to write around them
([ADR-067](../../../decisions/067-a-files-top-level-statements-are-its-program.md)).

```praxis
out("first")

fn double(n) { n * 2 }

var answer = double(21)
out(answer)

struct Point { x: Int, y: Int }

out(Point { x: 1, y: 2 })
```

```text
first
42
{ x: 1, y: 2 }
```

Statements and declarations may be interleaved. The compiler collects the
statements out from between the declarations into one generated function, and
that function is what the host calls; the declarations stay where they are, as
their own items. That is why a `fn` may appear after code that calls it, and why
a `var` may not — a declaration is visible everywhere in the file, a binding
only after the statement that introduces it. (A `struct` and an `enum` are
declarations too, and are visible everywhere on the same terms.)

## `fn main` is the fallback

A file with **no** top-level statements falls back to a declared `fn main`:

```praxis
fn main() {
    out("main ran")
    41 + 1
}
```

```text
main ran
42
```

Note the `42`. When the entry point produces a value, `praxis run` prints it.
A file's top level never does: every top-level statement runs for effect, so
the generated entry function is `Unit` and has no answer to report. That is what
keeps `out(x)` at the top level from printing twice.

The two spellings are alternatives, not layers. If a file has both, the top
level wins and `main` is an ordinary function nobody called:

```praxis
fn main() {
    out("main ran")
}

out("the top level ran")
```

```text
the top level ran
```

Write `main()` yourself if you want it run. A file with neither — only
declarations — has no entry point, and `praxis run` says so:

```praxis
fn helper(n) {
    n + 1
}
```

```text
error: no statements to run and no `main` function
```

`praxis check` accepts that file and exits 0. Having nothing to run is not a
type error; it is discovered by the thing that wanted to run it.

## A function does not see the bindings around it

Top-level bindings are the natural style for a file that is its own program, and
they are the one thing a `fn` cannot reach. A function is not a closure and
captures nothing:

```praxis
var limit = 10

fn over_limit(n) {
    n > limit
}

out(over_limit(11))
```

```text
error[N007]: `over_limit` cannot use `limit`: a function does not capture the bindings around it (pass `limit` as a parameter, or use a closure)

  fn-does-not-capture.px:4:9
  4 |     n > limit
    |         ^^^^^ `over_limit` cannot use `limit`: a function does not capture the bindings around it (pass `limit` as a parameter, or use a closure)

praxis: 1 error(s)
```

The message names both fixes, when both exist — a *recursive* function is told to
pass a parameter and nothing else, because a closure cannot name itself. A
closure written at the top level does capture; see
[functions and closures](functions.md).

A `fn` cannot be declared inside another `fn` either — that is `N005`, and a
`struct` or `enum` in a function body gets the same code. Declarations live at
the top level; everything else is a statement.

## A newline ends a statement

Statements are separated by newlines or by `;`, and a semicolon is only required
when two of them share a line
([ADR-049](../../../decisions/049-the-wildcard-binds-nothing-and-a-newline-ends-a-statement.md)).

```praxis
// A line comment runs to the end of the line.
/* A block comment /* nests */ and may span lines. */

var a = 1; var b = 2; out(a + b)

var total = 1 +
    2 +
    3
out(total)

var scaled = [3, 1, 2]
    .sorted()
    .map(|n| n * 10)
out(scaled)

fn one() { 1; }
out(one())
```

```text
3
6
[10, 20, 30]
1
```

A newline terminates a *statement* and never an operator chain. It is not
consulted anywhere in the operator loop, so `1 +` continues onto the next line
and a `.method()` chain runs down as many lines as you like. A trailing `;` is a
separator and nothing else: `fn one() { 1; }` still answers `1`.

Two statements adjacent on one line with neither separator is `P002`:

```praxis
var a = 1 var b = 2
out(a + b)
```

```text
error[P002]: expected `;` or a line break between statements

  run-on.px:1:11
  1 | var a = 1 var b = 2
    |           ^^^ expected `;` or a line break between statements

praxis: 1 error(s)
```

A newline is consulted at `break` and `return` too: a line break after the
keyword means "no value", whatever the next token is. `return` on its own line
returns nothing and the line below it is a separate statement. It also stands in
for the comma between `struct` fields, between `enum` variants and between
`match` arms — but not inside a record literal, where the comma is required.

### The two line-leading brackets

A `(` and a `[` each *begin* something and *continue* something, and the newline
is what breaks the tie: one asked to continue the expression before it does not
do so across a line break. So a call whose callee ends a line and whose argument
list begins the next is two expressions rather than a call:

```praxis
fn double(n) { n * 2 }

var doubled = double
(21)

out(doubled)
```

```text
<closure:0>
```

`doubled` is the function itself; `(21)` is a parenthesized `21`, evaluated and
thrown away. Nothing is reported, because nothing is ill-formed — that is the
cost of the rule, and it is paid so that a match arm beginning `(a, b) =>` on
its own line is an arm rather than an argument list for the arm above it. The
same tie-break settles `p.x` followed by a line-leading `(`: that is a field
read and a parenthesized expression, not a method call.

`[` behaves the same way, and has to, because a line-leading `[` is a list
literal:

```praxis
var rows = [[1, 2], [3, 4]]

var first = rows
[0]

out(first)
out(rows[0])
```

```text
[[1, 2], [3, 4]]
[1, 2]
```

`first` is the whole of `rows`, and `[0]` is a one-element list nobody kept. The
fix for both traps is the same: move the bracket up onto the previous line.

## Source text

Source is UTF-8. Identifiers may be Unicode — ASCII is the recommendation, not
the rule — and text is measured in characters, not bytes.

```praxis
var π = 3.14159
var größe = "Fjörð"

out(π)
out(größe.len())
```

```text
3.14159
5
```

`//` starts a line comment. `/* … */` is a block comment and nests, so
commenting out a region that already contains a comment works. An unterminated
one is `T001` rather than a silently swallowed file.

The keywords are `var`, `fn`, `if`, `else`, `while`, `for`, `in`, `loop`,
`match`, `return`, `break`, `continue`, `read`, `struct`, `enum`, `true` and
`false`. That is the whole list. `out`, `panic`, `Vec`, `max` and the rest of
the prelude are ordinary identifiers that happen to be defined, and so are the
builtin type names `Int`, `Text`, `Bool`, `Char`, `Float`, `Unit` and
`Never` — `var max = 5` is legal and shadows the builtin — and since
[ADR-125](../../../decisions/125-a-binding-is-a-binding-and-the-compiler-decides-its-storage.md)
so is `let`. See [bindings and shadowing](bindings.md).

## Reading input

A program that reads its input does so with a `read` expression, usually as the
first top-level statement:

```praxis
var numbers = read lines(int)

out(numbers.sum())
```

Given `1`, `2` and `3` on three lines:

```text
6
```

The input file is named with `--input`, or arrives on stdin. The `read`
expression is a small language of its own and has [its own
chapter](../input/read.md); `lines` and `int` are that language's vocabulary and
not the program's, so outside a `read` expression neither name is defined.

## The generated function has a name you cannot write

It is `<entry>`, which is not an identifier, so no program can declare a second
one and no program can call it. You meet it when a top-level statement faults:

```praxis
var n = 0
out(10 / n)
```

```text
error: program faulted: division by zero

Backtrace:
#0   <entry>

  locals:
    n: Int = 0
  temps:
    <tmp#1: Int> @ "0" = 0
    <tmp#3: Int> @ "10" = 10
    <tmp#4: Int> @ "10 / n" = <uninit>
    <tmp#5: Unit> @ "out(10 / n)" = <uninit>
```

The angle brackets are the point: frame `#0` is the file, not something anybody
wrote. Everything else about that frame is ordinary — the entry point goes
through inference, monomorphization and the backend as a nullary `Unit` function
like any other, so its locals are inspectable and a top-level call to a generic
function specializes exactly as a call from a `fn` body does. What you see above
is the non-interactive rendering, from `praxis run --debug never`. At a terminal
the default `--debug auto` drops you into the [crash
debugger](../debugger/faults.md), stopped at that frame with those locals live.
