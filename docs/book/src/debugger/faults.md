# The fault model

Praxis has no exceptions, no `try`, and no error-carrying return type. An
operation that cannot produce an answer — an add that overflows, an index past
the end, a key that is not in the map — raises a **fault**. A fault stops the
program where it happened. Nothing catches it, nothing recovers from it, and
there is no syntax that would let you.

That is a deliberate trade. Because a fault is never caught, the runtime is free
to keep the whole call chain and every named value in it, and hand that to a
debugger instead of to an unwinder. What you give up is recovery; what you get
is a crash report that knows your variables by name.

```praxis
var budget = 100
var people = 0
out(budget / people)
```

```text
error: program faulted: division by zero

Backtrace:
#0   <entry>

  locals:
    budget: Int = 100
    people: Int = 0
  temps:
    <tmp#1: Int> @ "100" = 100
    <tmp#3: Int> @ "0" = 0
    <tmp#5: Int> @ "budget / people" = <uninit>
    <tmp#6: Unit> @ "out(budget / people)" = <uninit>
```

`praxis run` exits 1. That report is the *noninteractive* form; run the same
program at a terminal and the same text is followed by a `Praxis crash>` prompt.
[Entering the debugger](entering.md) covers when you get which, and
[the command reference](commands.md) covers what to type at the prompt.

The rest of this chapter is the taxonomy: every fault the runtime can raise,
what raises it, and the exact words it says.

## The kinds

The fault line is always `error: program faulted: ` followed by one of these.
`panic` is the only kind that appends a message.

| message | raised by |
|---|---|
| `integer overflow` | `+`, `-`, `*`, `/`, `%`, unary `-` and the prelude's `abs` on `Int`, when the true result does not fit 64 signed bits |
| `division by zero` | `/` and `%` on `Int` with a zero divisor |
| `index out of bounds` | `xs[i]` past the end or negative, and `m[k]` for a key the map does not hold |
| `input parse mismatch` | a `read` or `parse` whose input does not match the parser |
| `empty collection` | an operation that needs at least one element: `min`, `max`, `pop`, `peek` on an empty one |
| `stack overflow (recursion limit)` | recursion that exhausts the native-stack budget |
| `float-to-int conversion out of range` | `Float.to_int()` on NaN, ±infinity, or a value outside the `Int` range |
| `not a Unicode scalar value` | a code point that is negative, above `0x10FFFF`, or a surrogate |
| `size or extent out of range` | a size the runtime cannot serve: a `BitSet` member, or a `Vec(n, …)` or `Grid(w, h, …)` extent — negative, or too large to address |
| `value does not have the declared type` | a value stored where its destination declared another type |
| `panic: <message>` | `panic(value)` |
| `assertion failed` | `assert(condition)` with a false condition |
| `empty range` | `clamp(v, low, high)` with `low > high` |
| `an argument this algorithm has no answer for` | a negative edge weight or a negative heuristic in `dijkstra` / `a_star` |

`invalid UTF-8 in Text` is the fifteenth and last kind. `praxis run` rejects
input that is not UTF-8 with `error: failed to read input from stdin: stream did
not contain valid UTF-8` and exit 2, so that fault is unreachable from the CLI;
it exists for an embedder that hands the runtime bytes it did not check
([ADR-111](../../../decisions/111-a-text-literals-bytes-are-the-compilers-promise.md)).

The rest of this chapter shows the ones you will actually hit.

## Integer overflow

`Int` is a signed 64-bit integer and its arithmetic is checked, not wrapping.

```praxis
var total = 9223372036854775807
out(total + 1)
```

```text
error: program faulted: integer overflow

Backtrace:
#0   <entry>

  locals:
    total: Int = 9223372036854775807
  temps:
    <tmp#1: Int> @ "9223372036854775807" = 9223372036854775807
    <tmp#3: Int> @ "1" = 1
    <tmp#4: Int> @ "total + 1" = <uninit>
    <tmp#5: Unit> @ "out(total + 1)" = <uninit>
```

`abs` is in the same family: the negation of the most negative `Int` is not an
`Int`, so `abs(x)` on it overflows rather than answering itself. `abs` is a
prelude function on `Int`, not a method — `x.abs()` is a `Y110` at check time,
because `.abs()` is `Float`'s.

`Float` arithmetic never faults — IEEE-754 answers `inf` and `NaN`, and the
language lets it. Only the narrowing `Float.to_int()` can, and it does so as
`float-to-int conversion out of range`.

## Division by zero

`/` and `%` on `Int` both raise it. There is no "returns zero" convention and no
`checked_div`. The other way those two operators fail is `integer overflow`, for
the one pair that has no answer: the most negative `Int` divided or remaindered
by `-1`.

## Index out of bounds — and the missing key

`xs[i]` past the end raises it:

```praxis
var xs = [10, 20, 30]
out(xs[3])
```

```text
error: program faulted: index out of bounds

Backtrace:
#0   <entry>

  locals:
    xs: Vec[Int] = [10, 20, 30]
  temps:
    <tmp#1: Vec[Int]> @ "[10, 20, 30]" = [10, 20, 30]
    <tmp#2: Int> @ "10" = 10
    <tmp#3: Unit> = Unit
    <tmp#4: Int> @ "20" = 20
    <tmp#5: Unit> = Unit
    <tmp#6: Int> @ "30" = 30
    <tmp#7: Unit> = Unit
    <tmp#9: Int> @ "3" = 3
    <tmp#10: Int> @ "xs[3]" = <uninit>
    <tmp#11: Unit> @ "out(xs[3])" = <uninit>
```

Indexing a map with a key it does not hold raises **the same kind**, in the same
words:

```praxis
var ages = Map[Text, Int]()
ages["ada"] = 36
out(ages["alan"])
```

```text
error: program faulted: index out of bounds

Backtrace:
#0   <entry>

  locals:
    ages: Map[Text, Int] = {"ada": 36}
  temps:
    <tmp#1: Map[Text, Int]> = {"ada": 36}
    <tmp#3: Text> @ ""ada"" = "ada"
    <tmp#4: Int> @ "36" = 36
    <tmp#5> @ "ages["ada"] = 36" = Unit
    <tmp#6: Text> @ ""alan"" = "alan"
    <tmp#7: Int> @ "ages["alan"]" = <uninit>
    <tmp#8: Unit> @ "out(ages["alan"])" = <uninit>
```

The words do not say "key", which is the one place the fault line is less
specific than it could be — earlier design notes promised a `map key was not
present` of its own and the runtime never grew one. The `temps` block is what
tells you which access it was: the faulting expression is named there, as
`@ "ages["alan"]"`.

Indexing a map is the *assertive* read. `Map.get` is the other one — it answers
`Option[V]` and never faults
([ADR-076](../../../decisions/076-absence-is-an-option-and-an-empty-min-is-a-fault.md)).
Choosing between them is choosing whether absence is a bug or a case.

## An empty min, max, pop or peek

A collection with no elements has no minimum, so asking for one is a fault
rather than a zero:

```praxis
var readings = Vec[Int]()
out(readings.min())
```

```text
error: program faulted: empty collection

Backtrace:
#0   <entry>

  locals:
    readings: Vec[Int] = []
  temps:
    <tmp#1: Vec[Int]> = []
    <tmp#3: Int> = 0
    <tmp#4: Int> = 0
    <tmp#8: Unit> @ "out(readings.min())" = <uninit>
```

`sum()` on an empty collection is `0` and `len()` is `0` — those have answers.
`min`, `max`, `pop_front`, `pop_back`, and heap `pop`/`peek` do not.

## A failed assert

`assert(condition)` takes a condition and nothing else. There is no message
argument, and the fault carries none: `assertion failed` beside a `temps` line
naming the condition that was false is already the whole story.

```praxis
var checksum = 41
assert(checksum == 42)
out(checksum)
```

```text
error: program faulted: assertion failed

Backtrace:
#0   <entry>

  locals:
    checksum: Int = 41
  temps:
    <tmp#1: Int> @ "41" = 41
    <tmp#3: Int> @ "42" = 42
    <tmp#4: Bool> @ "checksum == 42" = false
    <tmp#5: Unit> @ "assert(checksum == 42)" = <uninit>
    <tmp#6: Unit> @ "out(checksum)" = <uninit>
```

`<tmp#4: Bool> @ "checksum == 42" = false` is the assertion's own condition,
evaluated, kept, and shown.

## An explicit panic

`panic(value)` is the one that carries words. The value is rendered through its
descriptor — exactly as `out` would render it — and appended to the fault line.

```praxis
var mode = "diagonal"
panic("unsupported mode: " + mode)
```

```text
error: program faulted: panic: unsupported mode: diagonal

Backtrace:
#0   <entry>

  locals:
    mode: Text = "diagonal"
  temps:
    <tmp#1: Text> @ ""diagonal"" = "diagonal"
    <tmp#3: Text> @ ""unsupported mode: "" = "unsupported mode: "
    <tmp#4: Text> @ ""unsupported mode: " + mode" = "unsupported mode: diagonal"
    <tmp#5> @ "panic("unsupported mode: " + mode)" = <uninit>
```

The argument does not have to be `Text`: `panic(xs)` on a `Vec[Int]` produces
`error: program faulted: panic: [1, 2, 3]`.

`unreachable` does **not** exist. Older design notes list "reached
`unreachable`" as a fault kind; there is no such function in the prelude, and
`unreachable()` is an ordinary undefined-name error at check time. Write
`panic("unreachable: ...")` instead.

## A parse fault

An input parser that does not match its input raises `input parse mismatch`, and
the fault line grows two more: where in the input it stopped, what it wanted
there, and a preview of the bytes.

```praxis
var rows = read lines(`{name:word} {score:int}`)
out(rows.len())
```

with the input

```text
ada 36
alan oops
```

```text
error: program faulted: input parse mismatch
       at input offset 12..12: expected int
       actual: ada 36⏎alan oops⏎

Backtrace:
#0   <entry>

  locals:
    rows: Vec[{ name: Text, score: Int }] = <uninit>
  temps:
    <tmp#1> = "ada 36\nalan oops\n"
    <tmp#2: Int> = 1
    <tmp#5: Int> @ "rows.len()" = <uninit>
    <tmp#6: Unit> @ "out(rows.len())" = <uninit>
```

The offset is a byte offset into the whole input, and `⏎` is how the preview
draws a newline. Offset 12 is the `o` of `oops`. Note `rows` itself: the binding
the `read` was assigned to is `<uninit>`, because the parse never produced a
value to assign. At the prompt, the `input` and `parser` commands render the same
detail without the arithmetic; see
[inspecting the input parser](parser.md) and
[when a parse fails](../input/faults.md).

## Recursion depth

Recursion is bounded by a **byte budget**, not a call count. Every generated
function's prologue charges its own frame against what is left and faults before
the native stack can overflow and take the host process down with it
([ADR-105](../../../decisions/105-the-recursion-guard-spends-a-byte-budget.md)).

```praxis
fn down(n) { if n == 0 { 0 } else { down(n - 1) + 1 } }
out(down(1000000))
```

That faults with `error: program faulted: stack overflow (recursion limit)`. The
budget buys 8000 frames of an ordinary narrow function and fewer of a wide one —
a frame's cost grows with the number of heap values it holds live — so the depth
you reach is a property of the function, not a constant worth quoting.

The backtrace that follows it has **one line per frame**, which for this program
is exactly eight thousand lines of `#N   down`. That is worth knowing before you
run it at a terminal, and it is why this chapter quotes the fault line and not
the report.

## Allocation size

A size the runtime cannot serve is a fault rather than an abort. It used to be a
`usize` cast on the way into a Rust allocation, where a negative width became
`usize::MAX` and the process died with no diagnostic at all
([ADR-041](../../../decisions/041-bounded-extents-fault-instead-of-aborting.md)).

```praxis
var seen = BitSet()
seen.insert(1000000000000000000)
out(seen.len())
```

```text
error: program faulted: size or extent out of range

Backtrace:
#0   <entry>

  locals:
    seen: BitSet = {}
  temps:
    <tmp#1: BitSet> = {}
    <tmp#3: Int> @ "1000000000000000000" = 1000000000000000000
    <tmp#4: Unit> @ "seen.insert(1000000000000000000)" = <uninit>
    <tmp#5: Int> @ "seen.len()" = <uninit>
    <tmp#6: Unit> @ "out(seen.len())" = <uninit>
```

A negative member raises it too. The same guard covers the sized collection
constructors, and there it is the ordinary way to reach this fault: `Vec(n, fill)`
and `Grid(w, h, fill)` take extents the program computes, so a negative one — or a
`width * height` past the cap of 2^28 cells — stops the program the same way
([ADR-146](../../../decisions/146-a-collection-constructors-arity-is-its-shape.md)).
It cannot be a `check`-time refusal: a size is an `Int` like any other, and its
value is not known until it runs. `Grid()` and a `read grid(…)` never raise it —
the first asks for 0×0, and the second builds its payload from input it has
already read.

## `<uninit>`: the value that was never produced

Look again at the temp for the faulting expression. `budget / people`, `total +
1`, `xs[3]` and `ages["alan"]` are all `<uninit>`, and so is every temp above
them that was waiting on one. **`<uninit>` means the value was never produced**,
and it is the same answer however the operation failed.

The two ways it can fail are worth knowing anyway, because they are why the
debugger can say this at all. Checked `Int` arithmetic is inline machine code
with an inline overflow test and a branch to a cold block, and that cold block
goes straight to the fault epilogue — the operation is not a call whose result
gets stored, so on the faulting path nothing is stored into the destination slot
([ADR-102](../../../decisions/102-a-check-is-a-branch-not-a-call.md),
[ADR-117](../../../decisions/117-a-raise-that-branches-is-its-own-observation.md)).
Everything else — indexing, `min`, `insert`, `to_int` — is a call into a runtime
wrapper, and a wrapper that raises still has to return *something* across the ABI
boundary. The debugger's store for the destination comes after the fault check
rather than before it, so the sentinel the wrapper returned is never written down
([ADR-135](../../../decisions/135-a-debug-slot-is-written-on-the-path-the-value-was-produced-on.md)).

It used to be written down, and the two shapes read differently: `xs[3]` showed
`= Unit` in a slot the same line typed `Int`. That was the sentinel, not a value
the program computed, and telling the two apart meant knowing which operations
are calls. Now the frame says it directly.

A slot whose type genuinely *is* `Unit` and whose value is `Unit` — the temp for
a `seen.insert(…)` statement that ran, say — is an ordinary value and tells you
nothing either way.

## What is not a fault

- **A type error.** Everything the checker can prove wrong is a compile-time
  diagnostic and the program never starts. See
  [reading a type error](../types/errors.md).
- **Absence.** `Map.get` and `Grid.find` answer `Option[T]`; a `None` is a case
  to match, not a failure. See [enums and Option](../language/enums.md).
- **An unreadable input file.** `praxis run` reports it and exits 2 before the
  program runs. A missing `--input` file is not a parse fault.
- **A Rust panic inside the runtime.** Every runtime entry point is wrapped so a
  panic cannot unwind into generated frames
  ([ADR-080](../../../decisions/080-totality-is-the-contract-and-catch-unwind-is-the-proof.md)).
  If one ever escaped it would arrive as a `panic` fault whose message begins
  `internal error: a panic escaped the runtime wrapper` — or, where generated
  code would never look at the fault slot after that call, be printed and the
  process aborted. Seeing either is a bug in Praxis, not in your program.
