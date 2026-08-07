# Evaluating expressions

The three commands that make the crash debugger more than a stack dump are `p`,
`type` and `heap`. Each takes an expression written in ordinary Praxis syntax,
evaluates it against the selected frame's locals, and prints the result — the
value for `p`, the inferred type for `type`, the type and the value for `heap`.

There is no interpreter behind them. `p EXPR` synthesizes a one-function module,
`fn __p_expr(<the locals you named>) { EXPR }`, and runs it through the whole
compiler: parse, resolve, type-check, monomorphize, MIR, Cranelift, call. The
value you get back was computed by machine code generated for the question you
just asked ([ADR-036](../../../decisions/036-synthetic-p-expr-function.md)).
That is why `p` type-checks the way the rest of the language does, and why its
errors are the compiler's errors.

## Asking about a value

Here is a program with one local of each interesting shape. It faults on the
last line.

```praxis
// A frame carrying one local of each interesting shape, so a debugger session
// can ask `p` / `type` / `heap` about all of them. The last line faults.
struct Reading {
    site: Text
    depth: Int
}

enum Signal {
    Ping(Int)
    Quiet
}

fn main() {
    var readings = [Reading { site: "north", depth: 12 }, Reading { site: "south", depth: 30 }]
    var totals = Map()
    totals["north"] = 12
    totals["south"] = 30
    var latest = Ping(30)
    var label = "sonar"
    out(readings[9].depth)
}
```

Every kind of expression you would write in the program works at the prompt:
field access, indexing, pure method calls, arithmetic, tuples, `if`, `match`,
record and enum construction.

```text
Praxis crash> p label
sonar
Praxis crash> p label.len()
5
Praxis crash> p readings.len()
2
Praxis crash> p readings[0]
{ site: north, depth: 12 }
Praxis crash> p readings[0].depth + readings[1].depth
42
Praxis crash> p (readings[0].site, readings[1].depth)
(north, 30)
Praxis crash> p if label.len() > 3 { "long" } else { "short" }
long
Praxis crash> p Reading { site: "east", depth: 1 }
{ site: east, depth: 1 }
Praxis crash> type readings
Vec[Reading]
Praxis crash> heap readings
Vec[Reading]: [{ site: north, depth: 12 }, { site: south, depth: 30 }]
Praxis crash> p totals["north"]
12
Praxis crash> type totals
Map[Text, Int]
Praxis crash> p latest
Ping(30)
Praxis crash> heap latest
Signal: Ping(30)
Praxis crash> p match latest { Ping(n) => n, Quiet => 0 }
30
Praxis crash> quit
```

`heap EXPR` is `p EXPR` with the type printed in front of the value. It is the
command to reach for when a value's shape is ambiguous on sight — `[]` could be
a `Vec[Int]` or a `Vec[Text]`, `Ping(30)` says nothing about which enum it came
from — and the type answers it on the same line. It does not print more of a
large value than `p` does; see [Rendering, and what it does not
truncate](#rendering-and-what-it-does-not-truncate).

`type EXPR` stops the pipeline before code generation, so it answers for any
expression that type-checks, whether or not that expression could be run.

## What is in scope

The names `p` can see are the **named bindings of the selected frame**, and
nothing else. Not the other frames' locals, not the program's functions — a
synthesized module declares the locals your expression mentioned, and the record
and enum types it needs to spell them, and nothing else. (A type your expression
names is declared too, whether or not a local has it, which is what makes
`p Reading { site: "east", depth: 1 }` above work.)

```praxis
// Three frames, each with its own locals: `p` sees exactly the selected
// frame's, and nothing else in the program.
fn cell(grid: Vec[Int], at: Int) -> Int {
    grid[at]
}

fn edge(grid: Vec[Int], width: Int) -> Int {
    var row = 2
    cell(grid, row * width)
}

fn main() {
    var grid = [1, 2, 3, 4, 5, 6]
    var width = 3
    out(edge(grid, width))
}
```

```text
Praxis crash> bt
#0   cell
#1   edge
#2   main
  (frame 0 selected)
Praxis crash> p at
6
Praxis crash> p grid
[1, 2, 3, 4, 5, 6]
Praxis crash> p row
error: type error: `row` is not defined
Praxis crash> up
frame 1: edge
Praxis crash> p row
2
Praxis crash> p row * width
6
Praxis crash> p at
error: type error: `at` is not defined
Praxis crash> up
frame 2: main
Praxis crash> p grid.len()
6
Praxis crash> p width
3
Praxis crash> p edge(grid, 1)
error: type error: `edge` is not defined
Praxis crash> p widht
error: type error: `widht` is not defined
Praxis crash> frame 0
frame 0: cell
Praxis crash> p grid[at - 1]
6
Praxis crash> p grid[at]
error: expression faulted: index out of bounds
Praxis crash> quit
```

Four things in that session are worth naming.

`row` is not in scope in frame 0 and `at` is not in scope in frame 1: moving the
selection with `up`, `down` or `frame N` changes what `p` can name. The
navigation commands are in the [command reference](commands.md).

`p edge(grid, 1)` fails with the message a misspelling gets. The synthesized
module never declares your program's functions, so there is no `edge` to call —
the read-only gate below never gets a chance to have an opinion about it. A typo
(`widht`) and a real function you cannot call look identical from the prompt.

`p grid[at - 1]` succeeds and `p grid[at]` does not. A `p` expression runs
against the same runtime that faulted, so it can fault itself: an out-of-bounds
index, a division by zero, an overflow. The debugger reports the fault kind,
clears it, and stays at the prompt. Reproducing the fault in one line — with the
index nudged by one — is the fastest way to be sure you have found it.

And `p at` answers `6` for a function that has already returned in the native
sense. There is no suspended stack. The frame chain you are walking is a crash
snapshot, a copy the innermost fault epilogue took before any frame popped
([ADR-033](../../../decisions/033-crash-snapshot-rooting.md)); its values are
rooted for the collector, so they are still there and still valid.

### Shadowed names

A snapshot frame is flat. Every scope of the function contributes its bindings
to one list, so a name that was shadowed appears once per binding, with its own
type.

```praxis
// A snapshot frame is flat: every scope of the function contributes its
// bindings, so a shadowed name is listed once per binding.
fn main() {
    var n = 1
    if n > 0 {
        var n = "two"
        out(n)
    }
    var xs = [1]
    out(xs[9])
}
```

```text
Praxis crash> locals
  locals:
    n: Int = 1
    n: Text = "two"
    xs: Vec[Int] = [1]
```

`locals` shows you both. `p` picks the innermost — the last binding of that name
in the frame — and `type` agrees with it:

```text
Praxis crash> p n
two
Praxis crash> type n
Text
```

There is no syntax for naming the outer one. Read its value off the `locals`
listing.

### What `p` cannot bind

Only user-written bindings are candidates. The compiler temporaries `locals`
lists as `<tmp#4: Bool> @ "n > 0"` are shown but not nameable: they have no
source name to write.

The number of locals **one expression** may name is capped at six. The cap is on
the expression, not on the frame — a function with twenty bindings is fine as
long as each `p` you type mentions at most six of them. It is checked where the
locals are passed as arguments, so it binds `p` and `heap`; `type` never makes
the call and answers a seven-local expression without complaint.

```praxis
// Seven bindings and one long vector: enough to hit the two ceilings `p` has —
// six named locals per expression, and the twelve-local cap the banner (but not
// the `locals` command) applies.
fn main() {
    var a = 1
    var b = 2
    var c = 3
    var d = 4
    var e = 5
    var f = 6
    var g = 7
    var squares = (0..40).map(|n| n * n)
    out(squares[500])
}
```

```text
Praxis crash> p a + b + c + d + e + f
21
Praxis crash> p a + b + c + d + e + f + g
error: the expression names 7 locals; `p` supports up to 6
```

The set of names an expression "mentions" is over-approximated from its tokens,
so `p rec.g` counts a local called `g` against the six even though the `g` there
is a field name. Splitting the question into two `p`s is the fix.

## The read-only gate

A faulted program cannot be resumed, so the debugger must not let you change
what it computed. Every `p` and `heap` expression is walked before it is
compiled, and anything that could mutate, consume input, diverge, or run code
whose effects cannot be proved is rejected
([ADR-034](../../../decisions/034-read-only-purity-gate.md)). The gate sits
between type-checking and code generation, so a rejected expression never
executes.

```praxis
// The purity gate has to have something to refuse, so this frame holds a
// mutable collection, and the session below asks it to mutate.
fn main() {
    var seen = Set()
    seen.insert(3)
    var queue = [1, 2, 3]
    var steps = 0
    out(queue[7])
}
```

```text
Praxis crash> p queue.len()
3
Praxis crash> p queue.sorted()
[1, 2, 3]
Praxis crash> p queue.push(4)
error: method `push` is impure (may mutate state) — `p` rejects mutating expressions
Praxis crash> p seen.insert(9)
error: method `insert` is impure (may mutate state) — `p` rejects mutating expressions
Praxis crash> p { steps = 9; steps }
error: assignment mutates — `p` rejects mutating expressions
Praxis crash> p abs(steps - 5)
error: call to `abs` — `p` cannot prove a user function is read-only
Praxis crash> p |x| x + steps
error: closure literals may capture and mutate — `p` rejects them
Praxis crash> p for q in queue { q }
error: `for` diverges — `p` evaluates a single value
Praxis crash> p read int
error: `read` consumes input and may fault — `p` rejects it
Praxis crash> p while steps < 3 { steps }
error: `while` diverges — `p` evaluates a single value
Praxis crash> p { var t = steps + 1; t * 2 }
2
Praxis crash> p queue.map(|x| x + 1)
error: closure literals may capture and mutate — `p` rejects them
Praxis crash> type queue.push(4)
Unit
Praxis crash> type read int
Int
Praxis crash> quit
```

What the gate refuses, roughly in the order you are likely to meet it:

| rejected | why |
|---|---|
| a method the catalog tags impure (`push`, `insert`, `set`, …) | it mutates its receiver |
| an assignment inside a block | it mutates a binding |
| a call to a named function, prelude functions like `abs` and `min` included | the purity of a function body is not analyzed |
| a closure literal | it may capture and mutate |
| `for`, `while`, `loop`, `break`, `continue`, `return` | they do not yield one value |
| `read`, `parse` | they consume input and can fault on the cursor |

Everything else passes: literals, local reads, arithmetic and comparison, `if`,
`match`, tuples, list literals, ranges, field and tuple-element reads, record and
enum construction, blocks — including `var` declarations inside them, which is
what `p { var t = steps + 1; t * 2 }` is doing — and any method the catalog tags
pure.

The rejection that surprises people is `p abs(steps - 5)`. It looks like the
most innocent expression in the world; it is refused because Praxis has no
purity analysis for function bodies, and the gate's answer for anything it
cannot prove is no. Write the arithmetic out instead. Sorting and summing
through a *method* are catalog entries and are fine; it is the free-function
form that is not.

Mapping is not a way round that. `p queue.map(|x| x + 1)` is refused for the
closure and not for the method, and naming one of your own functions instead
does not help — the synthesized module never declares them, so it is `not
defined`. Every higher-order method is out of reach at the prompt.

Note also that `p queue.sorted()` is allowed even though it builds a new `Vec`.
Allocating is not mutating: a debugger expression allocates on the main GC heap
like any other code
([ADR-032](../../../decisions/032-debugger-expr-main-heap.md)), and what the
gate protects is the state the snapshot holds.

`type` is not gated at all. ADR-034 says `type EXPR` applies the same gate "for
consistency"; the implementation runs the walk only on the paths that execute,
and the last two lines of that session are the proof — `type queue.push(4)`
answers `Unit`, `type read int` answers `Int`. Nothing runs, so there is nothing
to protect, and when `p` refuses an expression you can still ask what it would
have produced.

## Rendering, and what it does not truncate

Values are formatted through the same descriptor machinery `out` uses, so what
`p` prints for a value is what the program would have printed for it. That
formatting is recursive and **complete**: no element cap, no depth cap, no
ellipsis. A forty-element vector prints as forty elements.

```text
Praxis crash> p squares.len()
40
Praxis crash> p squares
[0, 1, 4, 9, 16, 25, 36, 49, 64, 81, 100, 121, 144, 169, 196, 225, 256, 289, 324, 361, 400, 441, 484, 529, 576, 625, 676, 729, 784, 841, 900, 961, 1024, 1089, 1156, 1225, 1296, 1369, 1444, 1521]
```

The design document (§16.3) says debugger output truncates large collections by
default and that `heap` exists for deeper inspection. Neither is true of the
implementation: nothing truncates a value, and `heap` differs from `p` only by
the type prefix. If you want less, ask for less — `p xs.len()`, `p xs[0]`,
`p xs.sorted()[0]`.

The one cap that does exist counts **locals, not elements**, and it applies to
the printed diagnostic rather than to the `locals` command. It is covered in
[Noninteractive mode](noninteractive.md).

## Cost

Each `p`, `type` or `heap` runs the front end, and `p` and `heap` also run a
fresh Cranelift compile of a one-function module. At a human-paced prompt this
is not noticeable, and the module is dropped when the command returns; the
schemas and debug metadata it minted are interned into one shared generation, so
a long session does not grow without bound. Values a `p` allocated stay on the
main heap and outlive the module that built them, which is what lets the
debugger print the result at all.
