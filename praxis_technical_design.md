# Praxis Programming Language

## Technical Design and Implementation Plan

**Status:** Draft implementation contract  
**Version:** 0.3  
**Date:** 2026-07-23  
**Primary implementation language:** Rust  
**Execution model:** Cranelift JIT  
**Editor target:** VS Code through the Language Server Protocol

---

## 1. Executive summary

Praxis is a small, statically typed, garbage-collected programming language designed specifically for Advent of Code-style puzzle solving. It favors rapid iteration, concise data manipulation, practical parsing, strong diagnostics, and interactive crash inspection over systems-programming concerns.

The language is procedural and expression-oriented. It supports functions, closures, records, enums, pattern matching, mutable local state, and functional collection pipelines. Every runtime value, including integers and booleans, is a garbage-collected object referenced through the uniform `GcRef` representation. Type inference should remove nearly all annotations from normal puzzle solutions. There is no ownership system, no lifetime syntax, no user-visible trait system, no user-defined operator overloading, and no manual memory management.

Praxis runs through a low-latency JIT pipeline:

```text
source -> parse -> resolve -> infer -> typed IR -> lower -> Cranelift -> execute
```

It does not produce standalone binaries. The compiler and runtime live in one Rust executable. Standard collections reuse Rust implementations behind a stable runtime ABI. Generated code treats collection objects as opaque garbage-collected references and calls Rust runtime wrappers for operations such as vector growth, hash-map insertion, sorting, and formatting.

Input parsing is a first-class language feature. `read PARSER` is an ordinary expression that lazily reads the process input into an immutable source buffer, applies a compile-time parser expression to the entire buffer, and returns the inferred typed value. Parser structure is written outside backtick templates, where whitespace is insignificant. Literal input syntax is written inside backticks, where punctuation and whitespace describe the input and typed captures extract values.

```praxis
let segments = read lines(`{x1:int},{y1:int} -> {x2:int},{y2:int}`)
```

Because `read` is an expression, its result may be bound with either `let` or `var`, passed directly to a function, or used in another expression.

Runtime failures are not handled manually. Bounds errors, parse mismatches, missing keys, overflow, failed assertions, and explicit panics stop normal execution and enter an interactive crash REPL. The user can inspect the stack, select frames, print local variables, evaluate expressions against captured state, edit the program, and reload it with the same input.

The project also includes a language server and a minimal VS Code extension. The language server reuses the compiler front end and provides diagnostics, completion, method suggestions, hover types, signature help, go-to-definition, references, rename, semantic highlighting, formatting, and input-parser assistance.

---

## 2. Goals and non-goals

### 2.1 Goals

Praxis must:

- Make common Advent of Code input formats concise and obvious to parse.
- Type-check programs before execution.
- Infer local, function, closure, record, collection, and parser-result types aggressively.
- Make procedural loops and functional pipelines equally natural.
- Provide strong built-in collections, grids, counters, queues, heaps, and graph algorithms.
- JIT compile quickly enough that the edit-run cycle feels immediate.
- Reuse Rust standard-library collection implementations where practical.
- Report source-oriented errors with exact input and program locations.
- Enter an interactive debugger after runtime failure in terminal sessions.
- Provide a useful VS Code editing experience from an early project stage.
- Keep language semantics small enough for a single implementation team or coding agent to complete incrementally.

### 2.2 Non-goals

Praxis is not intended to provide:

- Standalone executable generation.
- A stable C or Rust foreign-function interface for user programs.
- Ownership, borrowing, or lifetime annotations.
- Manual memory management.
- User-defined traits, interfaces, protocols, or type classes.
- User-defined operator overloads.
- Exceptions or recoverable parse errors.
- Async programming, threads, or concurrency in the first release.
- Macros or general compile-time metaprogramming.
- A package registry in the first release.
- ABI stability across compiler versions.
- Systems-programming performance or memory predictability.
- Arbitrary user extension of iteration, hashing, formatting, parsing, or ordering.

### 2.3 Design priority order

When two goals conflict, use this order:

1. Correctness and diagnostics.
2. Input ergonomics.
3. Edit-run-debug speed.
4. Language simplicity.
5. Runtime performance.
6. Implementation cleverness.

---

## 3. User experience

### 3.1 Command-line workflow

```text
praxis run day05.px < input.txt
praxis run day05.px --input input.txt
praxis check day05.px
praxis watch day05.px --input input.txt
praxis repl
praxis lsp
```

`run` parses, type-checks, JIT-compiles, and invokes the generated program entry point.

`check` runs the front end without executing code.

`watch` keeps the process and input buffer alive, recompiles on source changes, and reruns automatically. It is optimized for puzzle iteration.

`repl` starts an ordinary interactive session. The crash debugger uses a related but stateful REPL.

`lsp` starts the language server over standard input and standard output.

### 3.2 Source files

Use `.px` as the source extension.

A single file is the normal program unit. Top-level statements are wrapped in a generated entry function.

```praxis
let numbers = read lines(int)

out(numbers.sum())
```

### 3.3 Complete representative program

```praxis
let segments = read lines(`{x1:int},{y1:int} -> {x2:int},{y2:int}`)

fn overlaps(segments, diagonals) {
    let counts = Counter[(Int, Int)]()

    for segment in segments {
        let dx = sign(segment.x2 - segment.x1)
        let dy = sign(segment.y2 - segment.y1)

        if !diagonals && dx != 0 && dy != 0 {
            continue
        }

        let distance = max(
            abs(segment.x2 - segment.x1),
            abs(segment.y2 - segment.y1),
        )

        for step in 0..=distance {
            let point = (
                segment.x1 + dx * step,
                segment.y1 + dy * step,
            )
            counts[point] += 1
        }
    }

    counts.values().count(|n| n >= 2)
}

out(overlaps(segments, false))
out(overlaps(segments, true))
```

---

## 4. Language surface

### 4.1 Lexical conventions

- UTF-8 source files.
- Unicode identifiers are allowed, but ASCII identifiers are recommended.
- `//` starts a line comment.
- `/* ... */` is a nestable block comment.
- Statements are separated by newlines or semicolons.
- Semicolons are optional except when two statements share one line.
- Braces delimit ordinary language blocks.
- Backticks delimit literal parser templates inside parser expressions.

### 4.2 Bindings, reassignment, and shadowing

Every binding stores a `GcRef`, but bindings still have static source-language types.

`let` creates a binding that cannot be reassigned:

```praxis
let width = grid.width()
```

`var` creates a binding that may be reassigned to another value of the same inferred static type:

```praxis
var score = 0
score += 10
score = 25
```

This is invalid because reassignment does not change a binding's type:

```praxis
var score = 0
score = "high" // error: expected Int, found Text
```

A `let` binding may still point to a mutable object:

```praxis
let values = Vec[Int]()
values.push(42)
```

The reference stored in `values` is stable, while the referenced vector is mutated.

#### Rust-style shadowing

A later `let` declaration may shadow an earlier binding with the same name in the same lexical scope. The new binding is a distinct symbol and may have a different type:

```praxis
let a = 4
let a = "Foo"
out(a) // Text
```

This is not reassignment. The compiler allocates a new binding and gives it a new symbol ID. Name lookup after the declaration resolves to the new binding.

The initializer of a shadowing declaration is resolved before the new binding enters scope, matching Rust behavior:

```praxis
let a = 4
let a = a + 1 // the right-hand a is the previous Int binding
```

Closures created before a shadowing declaration retain the binding they originally captured:

```praxis
let a = 4
let show_old = || out(a)
let a = "Foo"
show_old() // prints 4
```

Shadowing and `var` therefore serve different purposes:

- `let name = ...` followed by another `let name = ...` creates a new binding and may change type.
- `var name = ...` followed by `name = ...` updates the existing binding and must preserve type.
- A captured `var` is represented by a GC-managed cell so closures observe later reassignment.
- Passing an argument copies its `GcRef`. Mutating the referenced object is shared; rebinding a local variable never rebinds the caller's variable.

### 4.3 Built-in scalar types and uniform object representation

All runtime language values are GC-managed objects. Variables, fields, tuple elements, enum payloads, function arguments, return values, closure captures, and collection elements are represented uniformly as `GcRef`.

| Type | Runtime payload | Notes |
|---|---|---|
| `Bool` | boolean payload | `true` and `false` may be immortal singleton objects |
| `Int` | signed 64-bit payload | default integer type |
| `UInt` | unsigned 64-bit payload | explicit use only |
| `Float` | IEEE 754 binary64 payload | puzzle convenience |
| `Byte` | unsigned 8-bit payload | byte-oriented input |
| `Char` | Unicode scalar payload | validated scalar value |
| `Text` | immutable UTF-8 payload or source-slice metadata | always referenced through `GcRef` |
| `Unit` | no user payload | may be one immortal singleton object |
| `Never` | no runtime value | diverging control flow |

The implementation may temporarily load scalar payloads into machine registers while evaluating an operation, but no language-visible storage location contains an unboxed scalar. An arithmetic result becomes a `GcRef` before it can be stored, passed, returned, captured, or inspected by the debugger.

This uniform model is normative even if later optimizations intern small integers, use tagged pointers, or eliminate allocations through escape analysis. Such optimizations must preserve reference and aliasing semantics.

The first implementation may omit `UInt` and `Float` until the integer pipeline is stable, but their reserved type names should not be reused.

### 4.4 Composite types

```text
(Int, Int)
Vec[Int]
Set[Text]
Map[Text, Int]
Option[(Int, Int)]
Grid[Char]
```

There is no user-written generic constraint syntax. Generic-looking collection types are built-in type constructors understood by the compiler.

### 4.5 Records

```praxis
struct Point {
    x: Int
    y: Int
}

let p = Point { x: 3, y: 4 }
```

Field punning is supported:

```praxis
let p = Point { x, y }
```

Parser templates can generate anonymous structural records without requiring a declaration.

### 4.6 Enums

```praxis
enum Tile {
    Empty
    Wall
    Number(Int)
    Portal(Text)
}
```

Pattern matches are exhaustive:

```praxis
let cost = match tile {
    Empty => 1
    Wall => panic("wall has no traversal cost")
    Number(n) => n
    Portal(_) => 0
}
```

### 4.7 Option and absence

`Option[T]` represents normal domain-level absence. It is not an error channel.

```praxis
match map.get(key) {
    Some(value) => use(value)
    None => use_default()
}
```

Indexing a missing map key faults instead of returning an option:

```praxis
let value = map[key]
```

The user chooses between explicit absence with `.get` and assertion-like access with indexing.

### 4.8 No user-visible traits

The language does not contain `trait`, `impl`, `interface`, `where`, or equivalent syntax.

The compiler owns closed tables describing which built-in shapes support:

- Equality.
- Structural hashing.
- Total or partial ordering.
- Formatting.
- Iteration.
- Numeric operations.
- Collection methods.

Records, tuples, and enums automatically support structural operations when their fields support those operations. Users never derive or implement them.

The implementation may internally use capability constraints during inference. They must not appear in source syntax or ordinary diagnostics.

### 4.9 Functions

```praxis
fn manhattan(a, b) {
    abs(a.x - b.x) + abs(a.y - b.y)
}
```

The last expression is returned. Explicit `return` is supported.

The compiler infers parameter and return types from use where possible. Recursive functions require enough annotation to break inference cycles.

```praxis
fn factorial(n: Int) -> Int {
    if n <= 1 { 1 } else { n * factorial(n - 1) }
}
```

### 4.10 Closures

```praxis
let offset = 10
let adjusted = values.map(|x| x + offset)
```

Closures capture values automatically. Mutable captures use GC-managed environment cells. There are no move closures, borrow captures, or lifetime rules.

### 4.11 Control flow

Supported constructs:

- `if` and `else` expressions.
- `match` expressions.
- `for` loops over built-in iterable shapes.
- `while` loops.
- `loop` for explicit infinite loops.
- `break`, optionally with a value in expression loops.
- `continue`.
- `return`.

Blocks are expressions.

### 4.12 Numeric behavior

Integer arithmetic is checked by default. Overflow faults and enters the crash debugger.

Explicit alternatives:

```praxis
a.wrapping_add(b)
a.saturating_add(b)
a.checked_add(b) // returns Option[Int]
```

Division by zero always faults.

#### Float behavior

`Float` is IEEE-754 binary64 (§4.3). Float literals (`3.14`, `1e10`) and Int
literals (`42`) are typed strictly by their syntax: a Float operand makes the
operation Float, an Int operand makes it Int. There is **no implicit widening**;
mixing the two is a type error. Cross-type conversion is explicit:

```praxis
let f: Float = 5.to_float()    // Int -> Float (always exact)
let n: Int   = 3.9.to_int()    // Float -> Int, truncates toward zero
```

Float arithmetic **never faults** — per IEEE-754, `1.0 / 0.0` is `inf`,
`-1.0 / 0.0` is `-inf`, and `0.0 / 0.0` is `NaN`. Comparison uses IEEE-754
ordering, so `NaN` compares unequal to everything (including itself): `NaN ==
NaN` is `false`, `NaN < x` is `false`.

The sole faulting Float operation is the narrowing `Float.to_int()`: it faults
(`FloatToInt`) on `NaN`, `±infinity`, or a finite value outside the signed
64-bit range — these have no exact `Int` representation. (Integer division by
zero faults; float division by zero does not.)

`out()` and `to_text()` format finite values in the shortest round-trippable
form, and the special values as `inf`, `-inf`, `NaN`. The stdlib Float methods
are `abs`, `sqrt`, `floor`, `ceil`, `round`, `sign`, `to_int`, `to_text`,
`is_nan`, `is_infinite`, `min(other)`, `max(other)`; `pi()` and `e()` are
prelude free functions. `%` (remainder) is not defined for floats.

See ADR-037 for the implementation: floats ride the uniform `i64` scalar channel
as their bit pattern, bit-casting to `f64` at arithmetic/comparison points.

---

## 5. Type system

### 5.1 Model

Use an HM-inspired inference engine with extensions for:

- Mutable variables.
- Nominal records and enums.
- Anonymous structural records generated by parser expressions.
- Built-in collection constructors.
- Closure types.
- A small internal capability system.
- Monomorphization of inferred polymorphic functions.

### 5.2 Inference behavior

The following should require no annotations:

```praxis
fn total(values) {
    values.sum()
}

let values = Vec[Int]()
values.push(1)
values.push(2)
out(total(values))
```

Inference determines:

```text
total: Vec[Int] -> Int
values: Vec[Int]
```

### 5.3 Generalization and shadowed bindings

- Immutable `let` bindings may be generalized.
- Mutable `var` bindings are not generalized.
- Every shadowing declaration is inferred independently and receives a new symbol ID.
- A shadowing initializer resolves names in the preceding environment; the new symbol becomes visible only after its initializer is checked.
- Shadowed bindings may have unrelated types.
- Assignment to a `var` never re-runs inference and must preserve the binding's established type.
- Function declarations are generalized after their bodies are checked.
- Recursive declaration groups are inferred together and may require annotations.
- Top-level values may be generalized only when initialization has no mutable capture.

### 5.4 Internal capabilities

The inference engine may create internal constraints such as:

```text
SupportsEq(T)
SupportsHash(T)
SupportsOrd(T)
Iterable(T, Item)
Numeric(T)
```

Constraint resolution is closed and compiler-defined. No user code can add a capability implementation.

Diagnostics translate capability failures into concrete language terms:

```text
error: values of type Grid[Point] cannot be sorted directly
hint: use sort_by(|value| ...)
```

Do not mention an internal trait or capability name to the user.

### 5.5 Structural equality and hashing

Tuples, records, and enums receive compiler-generated equality and hash procedures. Procedures are emitted or referenced through a type descriptor.

A type is hashable when all recursively contained fields are hashable. Function and closure values are not hashable.

### 5.6 Anonymous record identity

Anonymous records are structurally identified within one compilation session by an interned ordered field set:

```text
{ x: Int, y: Int }
```

Field order in source does not affect type identity after canonicalization, but display and construction preserve source order.

### 5.7 Method resolution

Method resolution uses a closed compiler table keyed by receiver type shape and method name.

Example entries:

```text
Vec[T].push(T) -> Unit
Vec[T].len() -> Int
Vec[T].map((T) -> U) -> Seq[U]
Text.ints() -> Vec[Int]
Map[K,V].get(K) -> Option[V]
Grid[T].neighbors4(Point) -> Seq[Point]
```

Every row is a method, including the ones that take no arguments, and every call
site writes the parentheses: `v.len()`, `grid.width()`. There is no property form
(ADR-077). A bare `receiver.name` is a **field** read and only that — the two
constructs have different lowerings (a slot index against the record's definition,
versus a call through the catalog), and a receiver whose type is not yet known
cannot tell them apart, which is what REP-28's deferred field requirement depends
on.

The language server uses the same table for completion and signature help.

---

## 6. Collections and standard data types

### 6.1 Required collection set

- `Vec[T]`
- `Deque[T]`
- `Map[K, V]`
- `Set[T]`
- `Counter[T]`
- `MinHeap[T]`
- `MaxHeap[T]`
- `BitSet`
- `Grid[T]`
- `Range`

### 6.2 Collection semantics

`Counter[T]` behaves as a map whose absent values read as zero.

```praxis
counts[key] += 1
```

Maps support language-defined update assignments:

```praxis
distance[key] min= candidate
best[key] max= score
```

For `min=` and `max=`, an absent entry accepts the first value.

### 6.3 Functional sequences

The language exposes lazy, compiler-known sequence pipelines without a user-visible iterator type.

```praxis
let answer = values
    .filter(|x| x > 0)
    .map(|x| x * x)
    .sum()
```

Initial operations:

- `map`
- `filter`
- `filter_map`
- `flat_map`
- `fold`
- `reduce`
- `sum`
- `product`
- `count`
- `any`
- `all`
- `find`
- `position`
- `enumerate`
- `zip`
- `chunks`
- `windows`
- `take`
- `skip`
- `take_while`
- `unique`
- `frequencies`
- `sorted`
- `min`
- `max`
- `min_by`
- `max_by`
- `collect`

The compiler lowers pipelines into concrete internal adapters, then fuses common chains into loops. As of M8-WS11, the 22 non-barrier combinators (all of the above except `sorted`/`unique`/`frequencies`/`chunks`/`windows`) fuse into a single loop over the source — `v.map(f).filter(p).sum()` compiles to one loop with zero intermediate Vecs (ADR-029). The five barriers need the whole sequence and require new runtime sort/dedup helpers; they remain deferred.

### 6.4 Grid

Coordinates use `(x, y)` with `x` increasing rightward and `y` increasing downward.

Required API:

```text
grid.width()
grid.height()
grid[x, y]
grid.get(x, y)
grid.contains(x, y)
grid.positions()
grid.cells()
grid.row(y)
grid.column(x)
grid.neighbors4(point)
grid.neighbors8(point)
grid.find(value)
grid.find_all(value)
grid.map(fn)
grid.transpose()
grid.rotate_left()
grid.rotate_right()
```

### 6.5 Graph helpers

Provide closure-based algorithms that do not require materializing a graph object:

```praxis
let distance = bfs_distance(
    start,
    |state| neighbors(state),
    |state| state == goal,
)
```

Initial algorithms:

- BFS traversal.
- BFS shortest distance.
- DFS traversal.
- Dijkstra.
- A-star.
- Flood fill.
- Connected components.
- Topological sort.

---

## 7. Unified input parser DSL

### 7.1 `read` expression and the two parser modes

`read PARSER` is an ordinary prefix expression. It applies `PARSER` to the complete process-input source and returns the parser's statically synthesized result type.

```praxis
let segments = read lines(`{x1:int},{y1:int} -> {x2:int},{y2:int}`)
```

Because it is an expression, the result may be stored in a rebindable variable:

```praxis
var values = read lines(int)
values = values.filter(|value| value > 0)
```

It may also be passed directly:

```praxis
solve(read grid(char))
```

The runtime lazily reads standard input once into an immutable, GC-managed source buffer. Every `read` expression parses that entire buffer from its beginning; it is not a consuming stream operation. This makes repeated reads deterministic, though normal programs should generally parse once.

Parsing an existing `Text` object uses a separate expression:

```praxis
let sample_values = parse(sample, lines(int))
```

A parser expression has two visual modes:

1. **Parser-expression mode:** outside backticks. Whitespace and indentation are ignored.
2. **Template mode:** inside backticks. Characters describe input syntax; `{...}` introduces typed captures.

This boundary is normative.

```praxis
let segments = read lines(
    `{x1:int},{y1:int} -> {x2:int},{y2:int}`
)
```

No whitespace outside the backticks consumes input. Spaces inside the backticks are part of the template's whitespace-matching rules.

### 7.2 Template whitespace

Inside a backtick template:

- Punctuation and non-whitespace text match literally.
- A run of ordinary spaces matches one or more spaces or tabs.
- `\s*` matches zero or more spaces or tabs.
- `\s+` matches one or more spaces or tabs.
- `\n` matches one line ending.
- `\t` matches one tab.
- `\x20` matches exactly one ASCII space.
- Backticks and backslashes use ordinary escapes.

The flexible ordinary-space rule is intentional because AoC inputs frequently align columns with variable spacing.

### 7.3 Captures

Named capture:

```text
{name:int}
```

Anonymous capture:

```text
{int}
```

Named captures produce an anonymous record. Anonymous captures produce a scalar when there is one capture and a tuple when there are multiple captures.

```praxis
lines(`{int}`)          // Vec[Int]
lines(`{int},{int}`)    // Vec[(Int, Int)]
lines(`{x:int},{y:int}`)// Vec[{ x: Int, y: Int }]
```

Named and anonymous captures may not be mixed in one template.

### 7.4 Atomic parsers

Required atomic parsers:

```text
int
uint
float
byte
char
digit
word
identifier
text
rest
```

Semantics:

- `int`: signed decimal integer, surrounding horizontal space handled by caller.
- `word`: non-empty run excluding whitespace and parser-delimiter punctuation.
- `identifier`: ASCII-like identifier syntax by default.
- `text`: minimally consumes text until the following template literal can match.
- `rest`: consumes the remainder of the current region.
- `digit`: one decimal digit, returned as `Int` or `Byte`; choose `Int` for v1 consistency.

### 7.5 Structural parser constructors

Every example below is written as `read constructor(...)`, because a parser
expression is a **sublanguage** and `read` (or `parse(text, ...)`) is where it
begins — §7.1. That is what makes a labelled argument such as `skip:` or
`ranges:` legal: it belongs to the parser-expression grammar and has no meaning
in an ordinary call, where it is a parse error at the `:`.

#### `lines(parser)`

Split the current region into logical lines and apply `parser` to each line. Each application must consume the entire line.

```praxis
read lines(int)
```

Result: `Vec[Int]`.

#### `sections(parser)`

Split the current region on one or more blank lines and apply `parser` to every section.

```praxis
read sections(lines(int))
```

Result: `Vec[Vec[Int]]`.

#### Heterogeneous `sections`

Named arguments parse fixed sections in order:

```praxis
let data = read sections(
    rules: lines(`{before:int}|{after:int}`),
    updates: lines(csv(int)),
)
```

Result:

```text
{
    rules: Vec[{ before: Int, after: Int }],
    updates: Vec[Vec[Int]],
}
```

`repeated(parser)` may appear only as the final named argument and consumes all remaining sections:

```praxis
let bingo = read sections(
    draws: csv(int),
    boards: repeated(matrix(int)),
)
```

#### `csv(parser)`

Split the current region on commas. Ignore horizontal whitespace around each comma. Apply `parser` to each token.

#### `ws(parser)`

Split on one or more spaces or tabs.

#### `sep(separator, parser)`

Split on the exact string separator, with no implicit trimming unless the separator contains spaces.

```praxis
read sep(" -> ", word)
```

#### `chars(parser, skip: policy)`

Apply a parser repeatedly to characters. Optional `skip` policies:

- `none`
- `whitespace`
- `newlines`

```praxis
read chars(one_of("^v<>"), skip: whitespace)
```

#### `grid(cell_parser)`

Parse rectangular lines into `Grid[T]`. Every row must have the same cell count.

```praxis
read grid(char)
read grid(digit)
```

#### `grid(cell_parser, ragged, fill: value)`

Permit uneven rows and pad to the maximum width.

#### `matrix(element_parser)`

Parse lines containing whitespace-separated elements into a rectangular `Matrix[T]` or `Grid[T]`. Prefer one standard type; the implementation should use `Grid[T]` unless matrix-specific algebra is later added.

#### `block(parser...)`

Apply sequential parsers within one current region. A positional parser contributes its captures directly. A named argument contributes one field.

```praxis
read sections(
    block(
        `{source:word}-to-{destination:word} map:`,
        ranges: lines(`{destination:int} {source:int} {length:int}`),
    )
)
```

Result:

```text
Vec[{
    source: Text,
    destination: Text,
    ranges: Vec[{
        destination: Int,
        source: Int,
        length: Int,
    }],
}]
```

A positional template with named captures is flattened into the enclosing block result. A positional parser returning a scalar must be explicitly named to avoid an unclear field name.

#### `one_of(chars)`

Match one character from a literal character set.

```praxis
read chars(one_of("LR"))
```

#### `optional(parser)`

Return `Option[T]`. Failure must consume no input. This is parser-level optionality, not exception recovery.

#### `choice(...)`

Parse one of several full alternatives and generate an anonymous enum.

```praxis
read choice(
    Number: `{name:word}: {value:int}`,
    Operation: `{name:word}: {left:word} {op:char} {right:word}`,
)
```

Result cases are matched as `.Number { ... }` and `.Operation { ... }`.

#### `scan(parser)`

Find repeated parser matches inside otherwise irrelevant text. This supports puzzles that embed instructions in corrupted text.

```praxis
read scan(choice(
    Multiply: `mul({left:int},{right:int})`,
    Enable: `do()`,
    Disable: `don't()`,
))
```

`scan` returns matches in source order and ignores unmatched text. `scan_exact` is a stricter future variant requiring full consumption.

### 7.6 Nested parser example

The previously ambiguous structure is written:

```praxis
let groups = read sections(lines(csv(int)))
```

Its result type is:

```text
Vec[Vec[Vec[Int]]]
```

All whitespace in the parser expression is ignored. No indentation or line break outside a backtick template is part of the input.

### 7.7 Repeated labeled blocks

```praxis
let monkeys = read sections(
    block(
        `Monkey {id:int}:`,
        `  Starting items: {items:csv(int)}`,
        `  Operation: new = old {operator:char} {operand:word}`,
        `  Test: divisible by {divisor:int}`,
        `    If true: throw to monkey {if_true:int}`,
        `    If false: throw to monkey {if_false:int}`,
    )
)
```

Spaces inside backticks describe the input. Because ordinary template spaces match horizontal-space runs, the indentation accepts equivalent aligned spacing. Exact indentation can use `\x20` if a puzzle requires it.

### 7.8 Type derivation

Parser result types are determined at compile time:

| Parser | Result type |
|---|---|
| `int` | `Int` |
| `` `{int}` `` | `Int` |
| `` `{int},{int}` `` | `(Int, Int)` |
| `` `{x:int},{y:int}` `` | anonymous record |
| `lines(P)` | `Vec[result(P)]` |
| `sections(P)` | `Vec[result(P)]` |
| `csv(P)` | `Vec[result(P)]` |
| `ws(P)` | `Vec[result(P)]` |
| `grid(P)` | `Grid[result(P)]` |
| `optional(P)` | `Option[result(P)]` |
| `choice(...)` | anonymous enum |
| named `sections` | anonymous record |
| `block(...)` | flattened anonymous record |

### 7.9 Parser AST

The input parser DSL must have its own typed AST. Do not lower it immediately into string splitting calls.

Suggested nodes:

```text
ParserRoot
ParserExpr
  Atomic(kind)
  Template(parts)
  Lines(child)
  SectionsHomogeneous(child)
  SectionsNamed(fields, repeated_tail?)
  Csv(child)
  WhitespaceSeparated(child)
  Separated(separator, child)
  Characters(child, skip_policy)
  Grid(child, options)
  Block(items)
  Optional(child)
  Choice(cases)
  Scan(child)

TemplatePart
  Literal(text, whitespace_policy)
  Capture(name?, parser)
```

The compiler performs:

1. Parser-expression parsing.
2. Static validation.
3. Result-type synthesis.
4. Parser plan construction.
5. Lowering into typed parser operations.
6. JIT compilation or calls into optimized runtime parser helpers.

### 7.10 Input buffers and source locations

The first `read` lazily reads standard input once into an immutable GC-managed source buffer; later `read` expressions reuse it. Text slices retain:

```text
owner reference
byte offset
byte length
line/column mapping handle
```

Every parser node carries a parser-source span. Every captured value may carry an optional source span in debug mode. Parse faults can therefore show both:

- The failing input location.
- The responsible parser-expression location.

### 7.11 Parse fault behavior

There is no user-visible `ParseError` or `Result`.

A mismatch creates a runtime fault containing:

```text
input span
parser span
expected description
actual preview
parser path
partial root value
```

The program enters the crash debugger when interactive.

---

## 8. Output and diagnostics

### 8.1 Output

```praxis
out(part1)
out("Part 2: {part2}")
```

`out` writes one value followed by a newline.

`dbg(value)` prints to standard error and returns the value.

```praxis
let next = dbg(candidates).first()
```

### 8.2 Compile diagnostics

Diagnostics must include:

- Error code.
- Primary source span.
- Concise explanation.
- Related spans when inference connects distant expressions.
- A concrete suggestion when available.
- Type names in user-facing syntax.

Example:

```text
error[T012]: expected Int, found Text

  day03.px:18:14
  18 | total += line
     |          ^^^^ this value is Text

hint: parse it with the input parser or call line.int()
```

### 8.3 Input diagnostics

```text
input fault: expected integer

  input.txt:18:6
  move x from 2 to 7
       ^

while matching parser:

  day05.px:4:12
  lines(`move {count:int} from {source:int} to {target:int}`)
              ^^^^^^^^^^^

path: moves[13].count
```

---

## 9. Runtime failure and crash REPL

### 9.1 Fault model

The language has no exceptions and no user-facing error propagation type.

Runtime faults include:

- Input parser mismatch.
- Integer overflow.
- Division by zero.
- Out-of-bounds index.
- Missing map key through indexing.
- Failed assertion.
- Explicit `panic`.
- Reached `unreachable`.
- Invalid internal runtime state.

### 9.2 Do not unwind Rust through JIT frames

Rust panics must not cross the runtime ABI. Runtime wrappers must prevent panic escape and translate unexpected panics into internal faults.

Generated code uses explicit fault propagation through a hidden runtime context.

Every potentially faulting operation can set:

```text
RuntimeContext.pending_fault
```

Generated code checks the fault flag at defined safepoints and branches to a fault epilogue.

### 9.3 Preserving debugger state

Each generated function owns a debug frame with:

```text
function id
current source span
caller frame pointer
named local slots
local type descriptors
active input parser path
```

On fault propagation, the function copies or links its debug frame into a persistent crash snapshot before returning. GC references in snapshots become roots. By the time control returns to the host, all language frames have produced stable snapshots even though native stack frames have unwound normally.

This design avoids:

- Rust panic unwinding.
- Platform signal tricks.
- `longjmp` across Rust frames.
- Keeping arbitrary JIT stacks suspended.

### 9.4 Interactive behavior

When attached to a terminal, a fault enters:

```text
runtime fault: map key was not present

  day05.px:47:22
  let next = graph[current]
                   ^^^^^^^^^

Praxis crash>
```

Required commands:

```text
bt                  show stack trace
frame N             select frame
up                  move toward caller
down                move toward callee
locals              show locals in selected frame
                    disambiguate shadowed names with source line or symbol ID
p EXPR              evaluate a read-only expression
type EXPR           show inferred expression type
source              show program source near the fault
input               show input near the active parser cursor
parser              show active input parser near the fault
heap EXPR            recursively inspect a value
restart             rerun compiled program with same input
reload              recompile source and rerun with same input
quit                 exit
help                 show commands
```

### 9.5 Debug expression evaluation

`p EXPR` performs:

1. Parse expression using ordinary Praxis syntax.
2. Resolve names against the selected frame snapshot.
3. Type-check using captured local types.
4. JIT-compile a synthetic read-only function.
5. Execute with references to snapshot slots.
6. Format the result.

Mutating expressions are rejected in the initial debugger. This prevents changes to a state that cannot safely resume.

### 9.6 Noninteractive behavior

If standard input or output is not a terminal, a fault:

1. Prints the diagnostic.
2. Prints the stack trace.
3. Prints top-frame locals up to configured limits.
4. Exits nonzero.

Flags:

```text
--debug=auto
--debug=always
--debug=never
```

### 9.7 Reload behavior

`reload` retains:

- Original input bytes.
- Input filename metadata.
- Command-line arguments.
- Debugger display preferences.

It discards old JIT code and crash snapshots after the new compilation succeeds. A failed recompilation leaves the crash REPL active and prints compile diagnostics.

---

## 10. JIT architecture

### 10.1 Compiler pipeline

```text
Source manager
  -> Lexer
  -> Lossless syntax tree
  -> AST lowering
  -> Name resolution
  -> Type inference
  -> Typed HIR
  -> Monomorphization
  -> MIR
  -> Simplification and fusion
  -> Cranelift IR
  -> JIT finalization
  -> Generated entry invocation
```

### 10.2 Cranelift integration

Use the Cranelift JIT/module crates. The host registers runtime symbols such as:

```text
praxis_alloc
praxis_int_add
praxis_vec_push
praxis_map_get
praxis_map_insert
praxis_text_format
praxis_write_stdout
praxis_set_fault
```

Generated functions treat every language value as an opaque `GcRef`.

### 10.3 Uniform JIT calling convention

Every generated function receives a hidden first parameter followed only by `GcRef` arguments and returns one `GcRef`:

```text
fn(RuntimeContext*, GcRef...) -> GcRef
```

The context provides:

- GC state.
- Pending fault.
- Debug frame chain.
- Input source table.
- Interned strings.
- Output handles.

No source-language scalar has a separate ABI representation. Generated code may load an `Int`, `Bool`, `Byte`, `Char`, or `Float` payload into a machine register for a local computation, but it must materialize a valid object reference before any safepoint, call, store, return, closure capture, or debugger-visible sequence point.

Function calls copy object references. They do not clone objects. A callee that mutates a referenced collection changes the shared object. Rebinding a callee-local binding changes only that local slot.

### 10.4 Runtime call fault protocol

Runtime wrappers must not panic across the ABI.

A wrapper either:

- Returns normally and leaves `pending_fault` clear.
- Sets `pending_fault` and returns a defined dummy value.

Generated code checks `pending_fault` immediately after calls that can fault. Later optimization may combine checks when safe.

### 10.5 JIT code lifetime

One compiler process may host multiple JIT generations in watch or debugger reload mode.

Use generation arenas:

```text
JitGeneration {
    module
    code memory
    data memory
    type descriptors
    source map
}
```

A generation is released only after no running code or debugger expression references it.

---

## 11. Rust collection reuse and runtime ABI

### 11.1 Principle

Reuse Rust collection implementations internally, but never expose Rust collection layouts or Rust ABI details to JIT-generated code.

Generated code must not call mangled methods such as `Vec::<GcRef>::push` directly.

Instead, expose stable project-owned wrappers whose language-value parameters and results are all `GcRef`:

```rust
#[no_mangle]
pub extern "C" fn praxis_vec_push(
    ctx: *mut RuntimeContext,
    vec: GcRef,
    value: GcRef,
) -> GcRef {
    // Validate descriptors, access Rust Vec<GcRef>, push, and return Unit.
}
```

### 11.2 Uniform Rust collection wrappers

All collection elements are `GcRef`, including integers, booleans, bytes, characters, records, tuples, enums, text, and nested collections.

```text
Vec[T]       -> Rust Vec<GcRef>
Deque[T]     -> Rust VecDeque<GcRef>
MinHeap[T]   -> Rust BinaryHeap<HeapEntry>
MaxHeap[T]   -> Rust BinaryHeap<HeapEntry>
Grid[T]      -> Rust Vec<GcRef> plus width and height
```

The static type `T` is enforced by the compiler and recorded in the collection object's type descriptor. The runtime wrapper verifies descriptor compatibility in debug builds and at unsafe ABI boundaries, but ordinary generated code does not carry per-element dynamic tags beyond each object's descriptor pointer.

This intentionally trades allocation and indirection for a much smaller implementation surface. Later optimizations may specialize storage internally, but specialized collections must remain observationally equivalent to a vector of object references.

### 11.3 Maps, sets, and counters

Reuse Rust hash collections behind opaque GC objects:

```text
Map[K, V]    -> Rust HashMap<DynamicKey, GcRef>
Set[T]       -> Rust HashSet<DynamicKey>
Counter[T]   -> Rust HashMap<DynamicKey, GcRef> // values are Int objects
```

`DynamicKey` stores a rooted `GcRef` and its type descriptor. Its Rust `Hash` and `Eq` implementations delegate to descriptor functions generated or selected by the compiler. The static type checker guarantees that one collection instance receives only the declared key and value types.

Collection methods take and return `GcRef`. For example:

```rust
#[no_mangle]
pub extern "C" fn praxis_vec_push(
    ctx: *mut RuntimeContext,
    vec: GcRef,
    value: GcRef,
) -> GcRef; // Unit on success, fault sentinel protocol on failure
```

### 11.4 Runtime object descriptors

Each GC object type has a descriptor:

```rust
struct TypeDescriptor {
    id: TypeId,
    name: &'static str,
    size: usize,
    align: usize,
    trace: unsafe fn(*mut u8, &mut Tracer),
    drop_value: unsafe fn(*mut u8),
    format: unsafe fn(*const u8, &mut Formatter),
    equals: Option<unsafe fn(*const u8, *const u8) -> bool>,
    hash: Option<unsafe fn(*const u8, &mut DynamicHasher)>,
}
```

Exact Rust types may differ, but all operations must be centralized in descriptors rather than scattered type switches.

### 11.5 Reallocation safety

Generated code must not retain pointers into a Rust vector across an operation that may mutate capacity.

Safe initial strategy:

- Keep all indexed access behind runtime calls.
- Later expose data pointers only within compiler-proven non-mutating regions.
- Reload the data pointer after every possible growth operation.

### 11.6 ABI versioning

The runtime ABI is private to one Praxis executable build. Still, define a runtime ABI version constant and assert compiler/runtime agreement at startup to catch accidental internal mismatch.

---

## 12. Garbage collector

### 12.1 Initial collector

Implement a precise, non-moving, single-threaded mark-and-sweep collector.

Reasons:

- Stable object addresses simplify Rust collection wrappers.
- No write barrier is required.
- Cranelift code can use ordinary opaque pointers.
- Debugger snapshots can retain references safely.
- Advent of Code workloads are short-lived and generally moderate in heap size.

### 12.2 Object header

Conceptual layout:

```text
GcHeader {
    descriptor pointer
    size or size class
    mark bits
    allocation flags
    linked-list or page metadata
}
object payload
```

### 12.3 Root tracking

Use compiler-managed shadow-stack frames or explicit root frames.

Every language value is a GC reference. A function roots references live across allocation safepoints; MIR liveness computes the minimal root set. There is no separate pointer-kind analysis for primitive versus composite locals.

Runtime roots include:

- Current generated frames.
- Global values.
- Interned texts.
- Input buffers.
- Crash snapshots.
- Active debugger-expression arguments.

### 12.4 Safepoints

Initial safepoints:

- Every GC allocation.
- Runtime calls that may allocate.
- Selected loop backedges for long allocation-free loops only if interruption support is added.

### 12.5 Finalization

No user-visible finalizers.

The collector invokes internal drop functions during sweep so Rust collections release their backing allocations.

### 12.6 Text slices

A source slice stores:

```text
owner: GcRef
start: usize  // private runtime metadata
length: usize // private runtime metadata
```

Never expose a raw interior pointer as a long-lived language value. Runtime operations may calculate temporary pointers while the owner is rooted.

---

## 13. Intermediate representations

### 13.1 Lossless syntax tree

The parser should produce a lossless tree retaining trivia. This is required for:

- Formatter.
- Language server incremental edits.
- Accurate diagnostics.
- Code actions.

Use immutable green nodes plus lightweight red wrappers, or an equivalent rowan-style design.

### 13.2 AST

The AST exposes typed node wrappers over syntax nodes. It should avoid copying source strings.

### 13.3 HIR

HIR resolves names and removes surface sugar:

- Top-level statements become generated `main`.
- Method calls become resolved intrinsic or function calls.
- `for` becomes explicit sequence iteration.
- Pattern matching becomes a structured decision representation.
- `read` and `parse` parser expressions become typed parser plans.
- String interpolation becomes formatting nodes.

### 13.4 Typed HIR

Every expression and pattern has an interned type ID. Store source spans and inferred substitutions.

### 13.5 MIR

MIR contains:

- Basic blocks.
- Explicit branches.
- Local slots containing `GcRef` for every language value.
- Typed reference values plus explicitly marked transient scalar payload temporaries.
- Calls.
- Allocation instructions.
- Bounds and overflow checks.
- Fault edges.
- GC safepoints.
- Debug-local metadata.

MIR does not need to be SSA initially. The Cranelift lowering layer creates SSA values and block parameters.

### 13.6 Monomorphization

Inferred polymorphic functions are instantiated for concrete use sites.

Cache instances by:

```text
FunctionId + canonical type arguments + relevant representation choices
```

The user never writes type arguments.

---

## 14. Compiler and repository architecture

Suggested Cargo workspace:

```text
praxis/
  Cargo.toml
  crates/
    praxis-cli/
    praxis-source/
    praxis-syntax/
    praxis-parser/
    praxis-ast/
    praxis-hir/
    praxis-types/
    praxis-input-parser/
    praxis-mir/
    praxis-codegen-cranelift/
    praxis-runtime/
    praxis-stdlib/
    praxis-debugger/
    praxis-lsp/
    praxis-test-support/
  editors/
    vscode/
  tests/
    ui/
    parser/
    typecheck/
    run-pass/
    run-fault/
    input-parsers/
    aoc-corpus/
```

### 14.1 Crate responsibilities

| Crate | Responsibility |
|---|---|
| `praxis-source` | files, spans, line maps, source snapshots |
| `praxis-syntax` | token and syntax node definitions |
| `praxis-parser` | ordinary language parser and recovery |
| `praxis-ast` | typed syntax wrappers |
| `praxis-hir` | name resolution and HIR |
| `praxis-types` | type interning, inference, capability resolution |
| `praxis-input-parser` | parser-expression lexer, template parser, type synthesis, parser plans |
| `praxis-mir` | lowering, CFG, liveness, fault and GC analysis |
| `praxis-codegen-cranelift` | JIT module and ABI lowering |
| `praxis-runtime` | GC, collections, text, I/O, runtime ABI |
| `praxis-stdlib` | compiler-known functions and method catalog |
| `praxis-debugger` | crash snapshots and REPL |
| `praxis-lsp` | LSP transport and compiler queries |
| `praxis-cli` | commands, watch mode, integration |

### 14.2 Shared compiler database

The CLI and LSP must share the same front-end query API.

Conceptual queries:

```text
source_text(file)
lex(file)
parse(file)
lower_ast(file)
module_scope(file)
resolve_name(position)
infer_function(function)
type_of(expression)
input_parser_at(position)
completion_context(position)
references(symbol)
```

Use a revisioned query cache. A library such as Salsa may be evaluated, but the architecture must not depend on undocumented behavior. A custom dependency-tracked cache is acceptable.

---

## 15. Language server

### 15.1 Process model

`praxis lsp` runs a JSON-RPC Language Server Protocol process over stdio.

The VS Code extension launches it and sends workspace configuration.

The server maintains:

- Open-document overlays.
- Source revisions.
- Parsed syntax trees.
- Per-function type-inference caches.
- Symbol indexes.
- Standard-library method metadata.

### 15.2 Required first-release features

#### Diagnostics

- Syntax errors with recovery.
- Name-resolution errors.
- Type errors.
- Exhaustiveness errors.
- Input-parser syntax and type errors.
- Unused binding warnings, configurable.

Diagnostics should update after a short debounce and must not require a full JIT compile.

#### Hover

Show:

- Inferred variable type.
- Function signature.
- Closure type when useful.
- Record fields.
- Method documentation.
- Input parser result type.

Example hover over `segments`:

```text
Vec[{
    x1: Int,
    y1: Int,
    x2: Int,
    y2: Int,
}]
```

#### Completion

Contexts:

- Lexical identifiers.
- Fields after `.`.
- Built-in methods based on receiver type.
- Enum cases in patterns.
- Named fields during record construction.
- Input parser constructors outside backticks.
- Capture atom types inside `{...}` in templates.
- Parser named arguments such as `skip`, `fill`, and `ragged`.

#### Signature help

Show signatures for functions and parser constructors:

```text
sections(parser) -> Vec[T]
sections(name: parser, ..., tail: repeated(parser)) -> record
lines(parser) -> Vec[T]
```

#### Navigation

- Go to definition.
- Find references.
- Document symbols.
- Workspace symbols.
- Rename local and top-level symbols.

#### Semantic tokens

Token classes should distinguish:

- Language keywords.
- Types.
- Functions and methods.
- Local variables.
- Record fields.
- Input parser constructors.
- Input template literal text.
- Input capture names.
- Input capture types.

#### Inlay hints

Optional inferred-type hints for:

- Function parameters.
- `let` bindings with non-obvious anonymous record types.
- Closure parameters.
- Input parser roots.

Keep hints off by default or conservative to avoid visual noise.

#### Formatting

The formatter must:

- Format ordinary Praxis syntax deterministically.
- Preserve backtick template contents exactly except normalized escape spelling.
- Format parser-expression mode with conventional function-call indentation.
- Never introduce semantic whitespace inside templates.

### 15.3 Input-parser editor support

The language server must understand whether a cursor is:

- Outside a `read` or `parse` parser expression.
- In parser-expression mode.
- In a backtick template.
- Inside a capture.
- Inside an atomic parser name.

Provide specialized diagnostics such as:

```text
unknown parser constructor `line`
did you mean `lines`?
```

and:

```text
mixed named and anonymous captures are not allowed
```

Hovering a parser expression should show its synthesized result type.

### 15.4 VS Code extension

The extension should be intentionally thin.

Responsibilities:

- Register `.px` files.
- Provide language configuration for comments, brackets, and indentation.
- Launch `praxis lsp`.
- Restart the server after binary updates.
- Expose commands:
  - `Praxis: Run File`
  - `Praxis: Check File`
  - `Praxis: Watch File`
  - `Praxis: Restart Language Server`
- Provide a TextMate grammar as fallback highlighting before semantic tokens arrive.
- Display debugger output in an integrated terminal.

Do not implement parsing or type logic in TypeScript.

### 15.5 LSP performance targets

These are engineering targets, not hard language semantics:

- Syntax diagnostics for a typical AoC file should feel immediate.
- Local type diagnostics should update without checking unrelated functions.
- Completion should use cached receiver types.
- The LSP must remain responsive when the JIT runtime is not available.

---

## 16. Standard library organization

### 16.1 Prelude

Automatically available:

```text
out dbg panic assert
abs sign min max clamp gcd lcm
Vec Deque Map Set Counter MinHeap MaxHeap Grid BitSet
bfs bfs_distance dfs dijkstra a_star flood_fill
```

### 16.2 Method catalog

Maintain one machine-readable method catalog consumed by:

- Type checker.
- HIR lowering.
- Code generation.
- Documentation generator.
- Language server completion and hover.

Suggested fields:

```text
receiver pattern
method name
parameter type pattern
result type pattern
purity/fault flags
allocation flag
runtime symbol or intrinsic lowering
documentation
stability status
```

Avoid duplicating method signatures across crates.

### 16.3 Formatting

All built-in values support debugger formatting. User-visible `out` formatting is structural and deterministic.

For large collections, debugger output truncates by default and supports `heap` for deeper inspection.

---

## 17. Testing strategy

### 17.1 Test categories

#### Lexer and parser tests

- Golden syntax trees.
- Error recovery.
- Unicode and escape handling.
- Backtick template boundaries.

#### Type-checker tests

- Inference snapshots.
- Capability failures.
- Anonymous record unification.
- Closure captures.
- Recursive functions.
- Exhaustiveness.

#### Input-parser tests

- Every parser constructor.
- Nested result types.
- Exact source spans.
- Whitespace behavior.
- Heterogeneous sections.
- Partial-value capture on faults.
- Representative AoC formats.

#### MIR and codegen tests

- MIR snapshots.
- Primitive arithmetic.
- Branches and loops.
- Calls and recursion.
- Fault edges.
- GC root liveness.

#### Runtime tests

- Vector reallocation.
- Map/set hashing and equality.
- GC tracing and sweep.
- Text slicing.
- Fault translation without panic escape.

#### Debugger tests

- Frame snapshots.
- Local value formatting.
- Expression evaluation.
- Restart and reload.
- Noninteractive fault output.

#### LSP tests

- Diagnostics after incremental edits.
- Completion snapshots.
- Hover types.
- Rename safety.
- Input-template semantic tokens.

### 17.2 UI test format

Use source files with expected annotations or separate `.stderr` snapshots:

```text
tests/ui/type-mismatch.px
tests/ui/type-mismatch.stderr
```

Provide a bless mode for intentional diagnostic updates.

### 17.3 AoC corpus

Create a legal, local corpus of handcrafted format-equivalent fixtures rather than copying proprietary puzzle text wholesale.

Cover at least:

- One integer per line.
- Two whitespace-separated columns.
- CSV integers.
- Fixed textual templates.
- Blank-line groups.
- Header plus repeated sections.
- Grid.
- Ragged grid plus command stream.
- Fixed-width diagram plus commands.
- Repeated labeled blocks.
- Nested list values.
- Token scan inside noisy text.
- Boolean gate lines.

Every fixture must include:

- Parser expression.
- Expected synthesized type.
- Expected parsed value.
- At least one failing input and expected diagnostic.

### 17.4 Fuzzing

Fuzz targets:

- Ordinary lexer/parser.
- Input-parser expression parser.
- Template parser.
- Type inference unification.
- Runtime collection wrapper boundaries.
- Formatter idempotence.

The compiler must never crash or hang on malformed source. It should produce diagnostics or a controlled internal-error report.

---

## 18. Performance strategy

### 18.1 First priorities

Optimize compilation latency before peak execution speed.

Initial optimization passes:

- Constant folding.
- Dead-block removal.
- Simple copy propagation.
- Inlining of tiny intrinsic wrappers.
- Sequence pipeline fusion.
- Obvious bounds-check elimination.
- Escape analysis for small closure environments.

### 18.2 Allocation strategy

Start with the normative all-object representation for every scalar and composite value. Optimize only after profiling, and preserve the uniform language-level reference semantics.

Potential later optimizations:

- Tagged or interned small scalar objects.
- Allocation elimination for non-escaping temporary scalar results.
- Specialized backing storage hidden behind the same collection ABI.
- Stack promotion of non-escaping objects with debugger-safe materialization.
- Arena allocation for parser-generated short-lived objects.
- Generational GC.
- Inline small text slices.

### 18.3 Benchmark suites

Maintain separate benchmarks for:

- Front-end latency.
- Input parsing throughput.
- JIT code generation.
- Integer loops.
- Grid traversal.
- Hash-map-heavy workloads.
- Sequence pipelines.
- GC stress.
- LSP incremental updates.

---

## 19. Milestone plan

Each milestone is a gate. Do not begin large work from a later milestone until the current gate passes its acceptance suite. Small preparatory interfaces are allowed when they reduce rework.

### Milestone 0: Workspace and contracts

**Deliverables**

- Cargo workspace and crate skeletons.
- CI for formatting, linting, tests, and documentation.
- Source span and diagnostic data structures.
- Runtime ABI design document encoded as Rust types.
- Method catalog schema.
- Test harness with snapshot support.

**Acceptance criteria**

- `cargo test --workspace` passes.
- A dummy `.px` file can be loaded and diagnosed through the CLI.
- CI rejects formatting and clippy regressions.
- No crate dependency cycles.

### Milestone 1: Lossless syntax and basic parser

**Deliverables**

- Lexer.
- Lossless syntax tree.
- Parser for literals, bindings, blocks, calls, functions, arithmetic, `if`, `while`, and `out` calls.
- Error recovery sufficient for LSP use.
- Basic formatter skeleton.

**Acceptance criteria**

- Golden syntax tests cover valid and invalid files.
- Parser produces multiple diagnostics from one malformed file.
- Formatter is idempotent on milestone syntax.
- No panic on fuzzed token streams.

### Milestone 2: Name resolution and core type inference

**Deliverables**

- Scopes and symbol IDs.
- Built-in static types.
- Function inference.
- Tuples.
- `let`, `var`, assignment, and Rust-style shadowing.
- Basic method catalog lookup.
- User-facing type diagnostics.

**Acceptance criteria**

- Infer non-recursive function parameters and return values from use.
- Accept `let a = 4; let a = "Foo"` and resolve each occurrence to the correct symbol.
- Resolve a shadowing initializer against the previous binding.
- Reject cross-type `var` reassignment.
- Hover query returns the inferred type and symbol identity for each shadowed occurrence.

### Milestone 3: Uniform object heap and collector

**Deliverables**

- `GcRef` and object header representation.
- Type descriptors for `Unit`, `Bool`, `Int`, `Byte`, `Char`, and `Text`.
- Precise non-moving mark-and-sweep collector.
- Root-frame API and runtime context.
- Immortal singleton support for `Unit` and booleans.
- Allocation and payload access helpers.

**Acceptance criteria**

- Every runtime value in interpreter/runtime tests is a `GcRef`.
- Scalar allocation, tracing, formatting, equality, and collection work correctly.
- GC stress tests preserve nested references.
- No source-language storage slot or public runtime wrapper uses an unboxed scalar ABI.

### Milestone 4: Cranelift JIT core and fault protocol

**Deliverables**

- MIR for object-based control flow.
- Cranelift lowering.
- Uniform `fn(RuntimeContext*, GcRef...) -> GcRef` calling convention.
- JIT symbol registration and generated top-level entry point.
- Boxed arithmetic, comparison, branching, loops, and function calls.
- Pending-fault state, generated checks, and source maps.

**Acceptance criteria**

- Execute boxed integer arithmetic, branches, loops, and recursive function calls.
- No object files or linker invocation.
- Overflow and division-by-zero return to the host without Rust unwinding.
- Named locals are available as `GcRef` values in fault snapshots.

### Milestone 5: Text, Vec, bindings, and debug metadata

**Deliverables**

- Immutable `Text` and source slices.
- Rust-backed `Vec<GcRef>`.
- Basic collection methods and structural formatting.
- GC-managed cells for captured `var` bindings.
- Debug frame registration with shadowed-symbol metadata.

**Acceptance criteria**

- Vector growth and nested vectors survive collection.
- `let` object mutation, `var` reassignment, and closure-shared `var` cells follow the specified semantics.
- Shadowed locals are distinguishable in debugger frames by source name and symbol ID.
- Missing/index faults preserve collection locals in snapshots.

### Milestone 6: Input parser v1 and `read`

**Deliverables**

- Prefix `read parser_expression` syntax.
- `parse(text, parser_expression)` syntax.
- Lazy process-input source buffering.
- Parser-expression lexer and parser.
- Backtick template parser.
- Atomic parsers: `int`, `char`, `word`, `text`, `rest`, `digit`.
- Constructors: `lines`, `sections`, `csv`, `ws`, `sep`, `grid`.
- Compile-time result-type synthesis.
- Source-aware input faults.

**Acceptance criteria**

- Parse all simple corpus fixtures without user string manipulation.
- Bind `read` results with both `let` and `var`.
- Multiple `read` expressions deterministically parse the same complete source buffer.
- Hover over a `read` result binding displays the synthesized nested type.
- Whitespace outside backticks never affects parsing.
- Parse mismatch enters the fault pipeline with input and parser spans.

### Milestone 7: Records, enums, pattern matching, and closures

**Deliverables**

- Nominal records and enums.
- Anonymous structural records from parser expressions.
- Pattern matching and exhaustiveness.
- Closures and GC environments.
- Structural equality and hashing descriptors.
- Monomorphized inferred polymorphism.

**Acceptance criteria**

- Store parser-generated records in vectors and maps.
- Use tuples and records as set/map keys.
- Compile closure pipelines with captured values.
- Reject non-exhaustive matches.

### Milestone 8: Full collection set and sequence pipelines

**Deliverables**

- `Map`, `Set`, `Counter`, `Deque`, heaps, `BitSet`, complete `Grid`.
- Closed method catalog.
- Lazy internal sequence representation.
- Pipeline lowering and basic fusion.
- Graph algorithms.

**Acceptance criteria**

- Solve representative grid, BFS, Dijkstra, counting, and frequency fixtures.
- Counter missing values behave as zero.
- `min=` and `max=` map updates work.
- Method completion data is generated from the same catalog used by the compiler.

### Milestone 9: Input parser v2

**Deliverables**

- Named heterogeneous `sections`.
- `repeated` tail sections.
- `block`.
- `matrix` or finalized grid-matrix design.
- `optional`.
- `choice` generated enums.
- `scan`.
- Ragged grids.
- Fixed-width diagram helper if corpus evidence still justifies it.

**Acceptance criteria**

- Parse bingo-style input.
- Parse almanac-style repeated mapping sections.
- Parse repeated labeled blocks.
- Parse grid plus folded command stream.
- Parse noisy embedded instructions.
- Every complex fixture has a useful failure diagnostic and partial parse state.

### Milestone 10: Crash debugger REPL

**Deliverables**

- Terminal crash REPL.
- Stack/frame navigation.
- Local display.
- Read-only expression evaluator through JIT.
- Input/parser context commands.
- Restart and reload.
- Noninteractive fallback behavior.

**Acceptance criteria**

- Inspect scalar-object, text, record, vector, map, and grid locals.
- Evaluate expressions using selected-frame locals.
- Reload after editing and rerun with identical input.
- GC retains all objects reachable from snapshots.
- No command can mutate or resume a faulted state in v1.

### Milestone 11: Language server MVP and VS Code extension

**Deliverables**

- `praxis lsp` process.
- Document synchronization and incremental source revisions.
- Diagnostics.
- Hover.
- Completion, including receiver methods.
- Signature help.
- Go-to-definition.
- Document symbols.
- Semantic tokens.
- Thin VS Code extension.

**Acceptance criteria**

- Editing a typical puzzle file updates diagnostics without running JIT code.
- Typing `grid.` suggests valid grid methods with signatures.
- Hovering a parser expression or `read` result displays its inferred result type.
- Input parser constructors and capture types receive distinct semantic highlighting.
- VS Code run/check commands invoke the local Praxis binary.

### Milestone 12: LSP completeness and formatter

**Deliverables**

- Find references.
- Rename.
- Workspace symbols.
- Inlay hints.
- Stable formatter.
- Code actions for common mistakes.
- Method and parser documentation in hover.

**Acceptance criteria**

- Rename updates all valid references and rejects unsafe collisions.
- Formatter preserves template semantics byte-for-byte except documented escape normalization.
- Code actions can fix misspelled parser constructors and add missing match arms.

### Milestone 13: Corpus validation and performance hardening

**Deliverables**

- Broad format-equivalent AoC corpus.
- Benchmark suite.
- Compilation and runtime profiling.
- Iterator fusion improvements.
- GC and collection tuning.
- Diagnostic polish.

**Acceptance criteria**

- Representative puzzles from many years require no manual line splitting for normal parsing.
- Typical programs use few or no type annotations.
- No known compiler crashes in fuzz and corpus tests.
- Watch mode supports repeated compile-run cycles without unbounded generation or heap growth.

### Milestone 14: Release packaging

**Deliverables**

- Versioned CLI release.
- VS Code extension package.
- Installation documentation.
- Language reference.
- Input parser reference.
- Runtime/debugger guide.
- Examples repository.

**Acceptance criteria**

- Clean install on supported platforms.
- VS Code discovers and starts the language server.
- Example suite passes from installed artifacts.
- Release checklist and compatibility notes are complete.

---

## 20. Coding-agent execution rules

A coding agent implementing this design should follow these rules:

1. Treat this document as the current contract. Record deliberate deviations in `docs/decisions/` before implementing them.
2. Keep the compiler front end independent from Cranelift and the runtime.
3. Never duplicate type or method knowledge between compiler and LSP.
4. Never expose Rust `Vec`, `HashMap`, or other layout assumptions to generated code.
5. Never allow a Rust panic to unwind through JIT-generated frames.
6. Keep source spans on all syntax, HIR, parser-expression, and diagnostic nodes.
7. Add snapshot tests with each diagnostic or syntax feature.
8. Add a run-pass and run-fault test with each code-generation feature.
9. Add a language-server test with each new completion-visible method.
10. Prefer simple closed implementations over extensible user-facing mechanisms.
11. Do not optimize representation before a benchmark identifies a bottleneck.
12. Do not advance a milestone while its acceptance criteria are failing.

### 20.1 Definition of done for a feature

A feature is done only when it has:

- Syntax or API documentation.
- Front-end tests.
- Type-checker tests where applicable.
- Runtime or codegen tests where applicable.
- Diagnostics for invalid use.
- LSP metadata when user-facing.
- No new unhandled panic path.
- Formatter coverage if syntax changes.

### 20.2 Pull request sizing

Prefer vertical slices that demonstrate one behavior end to end. Example:

```text
parse Vec.push call
-> resolve method
-> type-check argument
-> lower to MIR
-> emit runtime call
-> execute test
-> expose completion and hover
```

Avoid large horizontal changes that add many AST nodes without executable or diagnosable behavior.

---

## 21. Open design decisions

These items are intentionally deferred and should be decided through small prototypes:

1. Whether `Matrix[T]` exists separately from `Grid[T]`.
2. Whether `GcRef` remains a raw non-null pointer or later adopts an internal tagged-pointer optimization without changing semantics.
3. Whether anonymous records canonicalize field order or preserve order in identity.
4. Whether general recursive functions require complete signatures or only one anchor annotation.
5. Whether parser templates permit multiline content in v1.
6. Whether exact ordinary spaces inside templates need a shorter escape than `\x20`.
7. Whether the LSP query engine uses Salsa or a custom revision cache.
8. Whether debugger expression functions can allocate freely or use a separate temporary heap generation.
9. Whether any specialized collection backing stores are justified after profiling while retaining the uniform object ABI.
10. Which operating systems are required for the first packaged release.

None of these decisions should block milestones 0 through 5.

---

## Appendix A: Input parser grammar sketch

This is illustrative EBNF, not the final parser source.

```text
read_expr       := "read" parser_expr
parse_expr      := "parse" "(" expression "," parser_expr ")"

parser_root     := parser_expr EOF

parser_expr     := atom
                 | template
                 | call

atom            := IDENT

template        := BACKTICK template_part* BACKTICK

template_part   := template_literal
                 | capture

capture         := "{" IDENT ":" parser_expr "}"
                 | "{" parser_expr "}"

call            := IDENT "(" arguments? ")"

arguments       := argument ("," argument)* ","?

argument        := parser_expr
                 | IDENT ":" parser_expr

// Parser-expression whitespace and comments are ignored.
// Template bytes are handled by the template lexer.
```

Static validation distinguishes parser names, capture types, named arguments, and generated-result field collisions. The ordinary language parser recognizes `read` as a prefix expression and delegates the following token range to the parser-expression grammar.

---

## Appendix B: Runtime ABI sketch

```rust
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct GcRef(*mut GcHeader);

#[repr(C)]
pub struct RuntimeContext {
    pub heap: *mut Heap,
    pub pending_fault: *mut Fault,
    pub debug_top: *mut DebugFrame,
    pub input_source: GcRef,
    pub current_generation: u64,
    // Additional private fields.
}

// Generated-language ABI:
// fn(ctx: *mut RuntimeContext, args: GcRef...) -> GcRef

#[no_mangle]
pub extern "C" fn praxis_int_add(
    ctx: *mut RuntimeContext,
    left: GcRef,
    right: GcRef,
) -> GcRef;

#[no_mangle]
pub extern "C" fn praxis_vec_new(
    ctx: *mut RuntimeContext,
    element_type: *const TypeDescriptor,
) -> GcRef;

#[no_mangle]
pub extern "C" fn praxis_vec_push(
    ctx: *mut RuntimeContext,
    vec: GcRef,
    value: GcRef,
) -> GcRef;

#[no_mangle]
pub extern "C" fn praxis_vec_get(
    ctx: *mut RuntimeContext,
    vec: GcRef,
    index: GcRef,
) -> GcRef;
```

All arguments and return values are object references. Wrappers validate descriptors where required, set `pending_fault` on failure, and return a valid sentinel object such as `Unit` when control must unwind through generated fault checks.

## Appendix C: Input examples

### C.1 One number per line

```praxis
let values = read lines(int)
```

### C.2 Two columns

```praxis
let rows = read lines(`{left:int} {right:int}`)
```

### C.3 CSV program

```praxis
let program = read csv(int)
```

### C.4 Blank-line groups

```praxis
let groups = read sections(lines(int))
```

### C.5 Rules and updates

```praxis
let data = read sections(
    rules: lines(`{before:int}|{after:int}`),
    updates: lines(csv(int)),
)
```

### C.6 Bingo

```praxis
let bingo = read sections(
    draws: csv(int),
    boards: repeated(matrix(int)),
)
```

### C.7 Grid and commands

```praxis
let data = read sections(
    map: grid(char),
    moves: chars(one_of("^v<>"), skip: whitespace),
)
```

### C.8 Repeated map sections

```praxis
let almanac = read sections(
    seeds: block(`seeds: {values:ws(int)}`),
    maps: repeated(block(
        `{source:word}-to-{destination:word} map:`,
        ranges: lines(`{destination:int} {source:int} {length:int}`),
    )),
)
```

### C.9 Noisy instruction scanning

```praxis
let instructions = read scan(choice(
    Multiply: `mul({left:int},{right:int})`,
    Enable: `do()`,
    Disable: `don't()`,
))
```

---

## Appendix D: First end-to-end demo target

The first public demo should support this program:

```praxis
let pairs = read lines(`{left:int} {right:int}`)

let left = pairs.map(|p| p.left).sorted()
let right = pairs.map(|p| p.right).sorted()

let distance = left
    .zip(right)
    .map(|(a, b)| abs(a - b))
    .sum()

let counts = right.frequencies()
let similarity = left
    .map(|value| value * counts[value])
    .sum()

out(distance)
out(similarity)
```

This demo forces the implementation to integrate:

- Input parsers.
- Anonymous records.
- Vectors.
- Closures.
- Sorting.
- Zip and map pipelines.
- Counter/map lookup.
- Integer arithmetic.
- Output.
- Type hints in the LSP.
- Runtime faults and debug state if input or indexing fails.

It is a better integration target than isolated language syntax because it represents the intended daily workflow.
