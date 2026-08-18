# Diagnostic codes

Every problem the Praxis compiler reports carries a code: a category letter and
a three-digit number, such as `Y001` or `I013`. The code is a permanent,
user-facing identifier. It is allocated once and written in exactly one place in
the compiler — a closed enum whose `code()` method is the only expression in the
tree that pairs a category with a number. A number nobody registered has no
route into a diagnostic, and a number spent once is never reissued: re-spending
one is how an old message and a new one come to answer to the same name.

This chapter is the complete index. Every code below can be produced by the
compiler; nothing is reserved for later.

## What a diagnostic looks like

```praxis
var lines = read lines(word)
var total = 0
for line in lines {
  total += line
}
out(total)
```

```text
error[Y001]: expected Int, found Text

  diagnostic-anatomy.px:4:3
  4 |   total += line
    |   ^^^^^ expected Int, found Text

praxis: 1 error(s)
```

Five parts, and only the last two are optional:

- **The severity and the code.** Always `error`. The compiler emits no warnings
  and no notes — every diagnostic in this chapter is fatal to the run.
- **The location**, as `file:line:column`, one-based.
- **The snippet**, with a caret run under the primary span.
- **Related spans**, when inference connects two distant expressions. They render
  as extra snippets and arrive in the editor as LSP related information.
- **A `help:` line**, when the compiler has a concrete suggestion. When the
  suggestion also carries a replacement, the replacement is printed under it —
  and that is the same value the editor offers as a
  [quick fix](editors.md#code-actions).

Diagnostics come out in source order, and one mistake can produce more than one:

```praxis
struct Point { x: Int, y: Int }

fn shift(p: Point, dx: Int) -> Point {
  Point { x: p.x + dx, z: p.y }
}

var p = shift(Point { x: 1, y: 2 })
out(p.x)
```

```text
error[Y113]: `Point` literal is missing a field: y

  two-mistakes.px:4:3
  4 |   Point { x: p.x + dx, z: p.y }
    |   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ `Point` literal is missing a field: y

error[Y114]: `Point` has no field `z`

  two-mistakes.px:4:24
  4 |   Point { x: p.x + dx, z: p.y }
    |                        ^ `Point` has no field `z`

error[Y024]: this function takes 2 argument(s), but 1 were given

  two-mistakes.px:7:9
  7 | var p = shift(Point { x: 1, y: 2 })
    |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^ this function takes 2 argument(s), but 1 were given

praxis: 3 error(s)
```

`praxis check` exits 1 when it reported anything and 0 when it did not.

## The categories

| Prefix | Category | Raised by |
|---|---|---|
| `T0xx` | Lex | The lexer — a token that cannot be formed |
| `P0xx` | Parse | The parser — a token that cannot appear here |
| `N0xx` | Name | Name resolution and declaration checking |
| `Y0xx` | Type | Inference, member lookup and match coverage |
| `I0xx` | Input | The `read`/`parse` parser sublanguage |
| `R0xx` | Runtime | Declared, and has no members. Run-time failures are [faults](../debugger/faults.md), not diagnostics: they carry a fault kind, not a code |

The numbers inside a category are not contiguous and are not meant to be.
`Y09x` is internal errors, `Y11x` member errors and `Y12x` match errors, and the
gaps between those blocks are what keeps them blocks. `Y009` is **retired**: it
reported an assignment to a binding that cannot be written, and Praxis has no
such binding. Its number stays spent.

## Lex — `T0xx`

| Code | Message | What it means |
|---|---|---|
| `T001` | ``unterminated block comment`` | A `/*` with no matching `*/` before end of file. |
| `T002` | ``unterminated backtick template`` | A backtick template with no closing backtick. Nested templates count, so a `` ` `` inside a capture body does not close the outer one. |
| `T003` | ``unexpected character in source`` | A byte the lexer cannot classify. It becomes an `ERROR` token and lexing continues. |
| `T004` | ``unterminated text literal`` | A `"` string with no closing quote on its line. Also what an interpolated literal earns when a hole never closes. Interpolation adds **no** lex code: the lexer pre-scans a literal and splits it into fragments only once it has proved the literal closes on its line with every hole balanced, so one that does not close is a single text token and this code, holes or no holes. |
| `T005` | ``invalid escape in text literal`` / ``invalid escape in character literal`` | A `\` followed by a character that is not a recognized escape. One code, two messages: `"…"` and `'…'` read one escape table, which holds `\{` and `\}` as well — a language with two escape tables has two answers to what `\n` is, and the second answer is always found somewhere worse. Every fragment of an interpolated literal is validated on the same terms as a whole one; a `\` inside a *hole* is not an escape at all, because a hole holds expression tokens. |
| `T006` | ``unterminated character literal`` | A `'` with no closing quote before the end of its line. |
| `T007` | ``a character literal holds exactly one character`` / ``empty character literal: `''` names no character`` | A `'…'` whose body is not exactly one Unicode scalar. Two messages under one code; the too-long form offers a machine-applicable rewrite to a text literal. |

## Parse — `P0xx`

| Code | Message | What it means |
|---|---|---|
| `P001` | ``expected an expression``, ``expected a pattern``, ``expected a type``, ``expected a parser expression``, `` expected `{` to begin function body ``, ``an interpolated text literal is not a pattern; a pattern tests a constant``, ``unexpected token, skipping to recover``, … | The general "this token cannot appear here". The message names what the grammar wanted; the parser then recovers and keeps going, so one `P001` does not stop the rest of the file being checked. |
| `P002` | `` expected `;` or a line break between statements ``, `` expected `,` or a line break between match arms ``, `` expected `,` or a line break between … `` | Two things run together with no separator. Statements are separated by a newline or a `;`; list elements and match arms by a newline or a `,`. |

## Name — `N0xx`

| Code | Message | What it means |
|---|---|---|
| `N000` | ``internal: parse tree root is not a SOURCE_FILE`` | An internal error. It should be unreachable; seeing it is a compiler bug. |
| `N001` | `` `{name}` is not defined `` | A name that is not in scope. Carries a *did you mean* fix when a name in scope is close enough. |
| `N002` | `` unknown type `{name}` `` | A type annotation naming a type that does not exist — a typo, or a scalar name from another language (`UInt`, `Double`, `Str`). |
| `N003` | `` `{name}` is a value, not a type `` | A name used in type position that resolves to a value. The name is known; it is the wrong sort of thing. |
| `N004` | `` `{name}` is already declared in this scope `` | One name declared twice in one scope. |
| `N005` | `` `{name}` cannot be declared inside another function ``, `` `{name}` cannot be declared inside a function `` | A `fn`, `struct` or `enum` declared inside a function body. Only a source file's own statements are a declaration position. The wording differs by whether the nested thing is a function or a type. |
| `N006` | `` `{name}` refers to itself, and a self-referring type is not supported ``, `` `{name}` refers to itself through `{other}`, and … `` | A `struct` or `enum` declaration in a reference cycle, directly or through other declarations. `Vec[Node]` inside `Node` is the same cycle: what is missing is the language feature, not the values. |
| `N007` | `` `{fn}` cannot use `{name}`: a function does not capture the bindings around it (pass `{name}` as a parameter, or use a closure) ``, `` … (pass `{name}` as a parameter) `` | A `fn` body naming a binding declared outside it. A closure captures; a function does not. When the `fn` is recursive — directly or mutually — the closure half is dropped and a `help:` line says why: a closure cannot name itself, which is `N001`. |
| `N008` | `` `{name}` is {kind}, so `{name} { … }` does not build a record `` | A record literal whose head names something that is not a `struct` — an `enum`, a value, a builtin. |
| `N009` | `` `{name}` is not a keyword; a binding is written with `{replacement}` `` | A keyword the language retired, written where a statement starts. `let` is the whole table. It is not a misspelling of anything, so it gets an exact fix rather than the near-miss search that answers `N001`. `let` is still a legal identifier, which is why the position matters. |

`N010` is the next free Name code.

## Type — `Y0xx`, the user block

| Code | Message | What it means |
|---|---|---|
| `Y001` | ``expected {expected}, found {found}`` | Two types that would not unify. The most common diagnostic in the language, and the one that carries a `help:` line when the fix is explanatory rather than mechanical. |
| `Y002` | ``an infinite type would be required here`` | An occurs-check failure: a type would have to contain itself, as in unifying `a` with `(a) -> a`. |
| `Y003` | ``annotation says {annotated}, but use implies {derived}`` | An explicit annotation conflicts with what inference derived from the uses. |
| `Y004` | `` values of type `{ty}` cannot be compared with `==` `` | `==` or `!=` applied to a type with no structural equality — a function value, for instance. |
| `Y005` | `` values of type `{ty}` cannot be iterated `` | A `for` over something that is not a collection or a range. |
| `Y006` | `` values of type `{ty}` cannot be ordered `` | A value used where an ordering is required — a sort, a heap, a `<` — whose type has none. |
| `Y007` | `` `{ctor}` takes {want} type argument(s), but {got} were given `` | A type constructor in an annotation given the wrong arity: `Map[Int]`, `Vec[Int, Text]`, `Option[Int, Text]`. |
| `Y008` | ``duplicate {field\|variant} `{name}` `` | A `struct` or `enum` declaring one member twice. |
| `Y010` | `` values of type `{ty}` do not support this operation `` | A compound assignment (`+=`, `-=`, …) whose target is not numeric. |
| `Y011` | `` `return` outside a function `` | A `return` at the top level of a file. |
| `Y012` | `` `{break\|continue}` outside a loop `` | A `break` or `continue` with no loop to leave. A closure is a function boundary, so a loop outside a closure is not one a `break` inside it can leave. |
| `Y013` | `` `{literal}` is outside the range of `Int` `` | An integer literal too large for a 64-bit signed integer, in an expression or in a pattern. |
| `Y014` | `` a value of type `{ty}` can change after it is stored, so it cannot be used as a key `` | A mutable value used as a `Map` key or `Set` element. It would hash to a different bucket than the one holding it once it changed, and the entry would become unreachable. Carries a `help:` naming what to use instead. |
| `Y015` | `` values of type `{ty}` cannot be used in arithmetic `` | Arithmetic on a type that has none. |
| `Y016` | `` `{op}` is not defined for `{ty}` `` | An operator the language does not define for this operand type. Not a mismatch: both operands agree, and the operation still has no meaning. |
| `Y017` | `` a `break` carrying a value needs a `loop`; a `{while\|for}` produces `Unit` `` | A `break` with a value out of a `while` or `for`. Only `loop` is an expression loop; the other two also leave by their condition failing, and there is no value to supply on that path. |
| `Y018` | `` `{name}` is generic, so it has no single function value; write `\|x\| {name}(x)` to fix its type arguments at the call `` | A generic `fn` used as a value. Monomorphization is driven by call sites and a bare value has none; a closure body *is* a call site. |
| `Y019` | `` values of type `{ty}` have no element `{n}` — only a tuple does ``, `` a tuple of {arity} elements has no element `{n}` — its elements are `0` to `{arity-1}` `` | A `.n` element access on a non-tuple, or past the end of a tuple. One code, two messages: the arity is the useful thing to say when there is one. |
| `Y020` | `` values of type `{ty}` cannot be indexed with {n} index(es) ``, `` … cannot be assigned through {n} index(es) ``, `` … cannot be updated with `{min=\|max=}` through {n} index(es) `` | A subscript a type does not have, in any of three directions. The wrong *arity* on a receiver that does index is here too: `grid[x]` where a grid is written `grid[x, y]`. Three messages because the sets differ — a `Text` reads through `t[0]` and has no element store, and a `Counter` has a store but no updating store. |
| `Y021` | ``the left side of an assignment must be a name, a field, or an index`` | An assignment whose left side is not a place at all: `f() = 1`, `a + b[0] = 1`. A field is a place, so `p.x = 1` is fine. |
| `Y022` | `` `{name}` is {a builtin\|an enum constructor}, so it has no function value; {call it: `{name}()`\|write `\|x\| {name}(x)` to call it} `` | A prelude builtin or an enum constructor named without being called. `Y018`'s neighbour, one symbol kind over: a monomorphic `fn` at least *has* a value, and these have none, so `out(pi)` and `var h = abs` name something there is nothing to hold. Which remedy the message names depends on the arity. |
| `Y023` | `` a backtick template is a parser expression; write `read` before it, or pass it to `parse(text, ...)` `` | A backtick template written where a value is expected. The parser sublanguage is entered at `read` or `parse(text, …)` and nowhere else. |
| `Y024` | ``this function takes {expected} argument(s), but {found} were given`` | A call whose argument count does not match. A name in Praxis has exactly one signature — no overloading, no default parameters — so a count mismatch is never a candidate for some other signature. |

`Y025` is the next free code in this block. `Y009` is retired and is not
reissued.

## Type — `Y09x`, internal

| Code | Message | What it means |
|---|---|---|
| `Y099` | ``internal: inference recorded no type for this {kind} expression`` | An internal error: lowering asked for a type inference never recorded. It has its own number so that "did we emit an internal error?" stays a greppable question and never appears in the block a user is told to look up. |

## Type — `Y11x`, member errors

| Code | Message | What it means |
|---|---|---|
| `Y110` | `` no method `{name}` on type `{ty}` taking {n} argument(s) ``, `` no type has a method `{name}` taking {n} argument(s) `` | A method call that does not resolve. The second message is for the shape where nothing has pinned the receiver and the catalog holds that name at that arity on no receiver at all — printing `?T` there would be the least useful half of the sentence. Carries a *did you mean* fix, drawn from the catalog rows dispatch would have searched. |
| `Y112` | `` no field `{name}` on type `{ty}` `` | A field read on a type that does not have it. |
| `Y113` | `` `{Type}` literal is missing a field: {name} ``, `` `{Type}` literal is missing fields: {names} `` | A record literal that does not initialize every declared field. |
| `Y114` | `` `{Type}` has no field `{name}` `` | A record literal **or pattern** naming a field the type does not have. |
| `Y115` | `` field `{name}` is initialized more than once ``, `` field `{name}` is matched more than once `` | A record literal or pattern naming one field twice. In a pattern the second sub-pattern would silently replace the first, so one of the two bindings would never happen. |

`Y111` stays unallocated: `Y110` and `Y112` were assigned as "method" and
"field" with a gap between them, and closing it now would make the two look like
a range they never were. `Y116` is the next free member code.

## Type — `Y12x`, match errors

| Code | Message | What it means |
|---|---|---|
| `Y120` | ``non-exhaustive match: missing {witnesses}`` | A `match` that does not cover every value. Carries a fix that writes the missing arms, built from the same witnesses the message names. |
| `Y121` | ``unreachable match arm`` | An arm an earlier arm already covers entirely. |
| `Y122` | `` `{Type}` has no variant `{name}` `` | A pattern naming a variant the scrutinee's enum does not have. |
| `Y123` | `` `{ … }` is not a pattern for `{ty}` ``, `` `{name}` is `{ty}`, which has no fields to match ``, ``a tuple pattern names two elements or more``, `` `{ … }` cannot tell which record it matches here; name the record (`P { … }`) or annotate the value `` | A pattern whose shape cannot match: a record pattern against a non-record, a one-element tuple pattern, or an anonymous record pattern in a position where nothing says which record it is. |
| `Y124` | `` `{Variant}` in `{Enum}` holds {want} value(s), but this pattern names {got} `` | A variant pattern whose sub-patterns do not fit the payload: more than the variant holds, or a **bare** name for a variant that holds some. Naming fewer *inside parentheses* is legal and padded with wildcards, so `Some(_)` and `Some(n)` are one test; bare `Some` is not the third spelling of it. Carries a fix that writes one `_` per slot. |
| `Y125` | `` a `for` binding must match every item, and {reason} does not ``, `` a closure parameter must match every argument, and {reason} does not `` | A pattern that can fail, in a position that has no second arm to fall through to. |

`Y126` is the next free match code.

## Input — `I0xx`

These come from the `read`/`parse` sublanguage: the template scanner, the
constructor tables, and the validator that checks a call's shape before anything
is built. See [The `read` expression](../input/read.md).

| Code | Message | What it means |
|---|---|---|
| `I000` | ``malformed parser expression``, ``malformed parser constructor call``, `` malformed `repeated(...)` tail `` | The lowerer could not read the parser expression at all — the tree it was handed is not one a parser can be built from. |
| `I001` | ``a tuple type needs at least 2 elements, got {n}``, `` `{ctor}` takes {want} type argument(s), got {got} ``, `` duplicate record field `{name}` ``, `` duplicate enum variant `{name}` ``, ``too many parser plans registered in one process (limit {n})`` | A parser AST that could not be turned into a type or into a runnable plan. |
| `I010` | `` unknown atomic parser `{name}` `` | A word in atomic position that names no [atomic parser](../input/atoms.md). Carries a *did you mean* fix. |
| `I011` | `` `{name}` at byte {n} is not a capture name: a capture name is an identifier `` | The text before the `:` in a `{…}` capture is not an identifier. |
| `I012` | `` unknown parser `{name}` at byte {n}: no atomic or constructor is spelled that way `` | The capture kind after the `:` in a `{…}` names neither an atomic nor a constructor. Carries a *did you mean* fix. |
| `I013` | `` unknown parser constructor `{name}` ``, `` unknown parser constructor `{name}` at byte {n} `` | A call in parser position whose head names no [constructor](../input/structural.md). The span is the constructor's name, not the whole call, because a fix replaces what the report underlines. Carries a *did you mean* fix. |
| `I014` | `` `{ctor}` argument {n} is {what}, but {wanted} ``, `` `{ctor}` has an argument that is not a parser expression ``, `` `{name}:` takes a parser, not a literal value ``, `` `{name}:` needs a value ``, ``a parser constructor's literal argument must be a text literal``, `` `{ctor}` does not take {what} ``, `` `grid`'s ragged form is written `grid(P, ragged, fill: value)` — `ragged` and `fill:` come together or not at all `` | A constructor argument that is the wrong kind of thing, or one the constructor does not take. Every call is checked against the constructor's own argument shape before a parser is built. |
| `I020` | ``named and anonymous captures may not be mixed in one template`` | One template with both `{n:int}` and `{int}`. |
| `I021` | `` duplicate capture name `{name}` in template `` | One capture name used twice in a template. |
| `I022` | `` `{ctor}` expects {expected}, got {n} ``, `` `repeated` expects 1 argument, got {n} `` | A constructor called with the wrong number of arguments. |
| `I023` | `` `sep` needs a non-empty separator: an empty one never advances `` | An empty separator, which cannot move a cursor. |
| `I024` | `` duplicate section field `{name}` ``, `` duplicate block field `{name}` `` | A `sections` or `block` declaring one field name twice. The `repeated(…)` tail is a field of the generated record too, and is counted. |
| `I025` | `` named `sections` requires at least one field ``, `` `choice` requires at least one case `` | A `sections` or `choice` with nothing in it. |
| `I026` | `` a positional `block` item returning a scalar must be named `` | A positional `block` item whose parser returns a scalar. There is no field name to give it. A template is exempt: a no-capture template contributes no field and still consumes input. |
| `I027` | `` duplicate choice case `{name}` `` | A `choice` declaring one case name twice. |
| `I028` | `` `repeated(...)` is only the final named argument of a `sections` call ``, `` `sections` takes at most one `repeated(...)` tail ``, `` a `repeated(...)` tail may appear only as the final named argument: it consumes every remaining section, so nothing can follow it `` | A `repeated(…)` somewhere it cannot go. It consumes every remaining section, so nothing can follow it, and there can be only one. |
| `I030` | ``invalid escape `{seq}` at byte {n}``, ``unterminated capture starting at byte {n}``, ``empty capture `{}` at byte {n}``, ``malformed capture body at byte {n}: {detail}``, ``{what} nesting is deeper than {limit} at byte {n}`` | A backtick template the scanner could not read. The byte offset is into the template's own text. |

An input mistake is reported against the template or the call that contains it:

```praxis
var moves = read lines(`move {count:int} from {int} to {target:int}`)
out(moves.len())
```

```text
error[I020]: named and anonymous captures may not be mixed in one template

  input-mistake.px:1:24
  1 | var moves = read lines(`move {count:int} from {int} to {target:int}`)
    |                        ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ named and anonymous captures may not be mixed in one template

praxis: 1 error(s)
```

Failures while a parser is *running* are not diagnostics. They are input faults,
they carry a fault kind rather than a code, and they open the
[crash debugger](../debugger/parser.md).

## Which command sees what

`praxis check` and the editor run the same front end and report the same set —
they share one query layer, so a diagnostic in one and not the other is not
representable.

`praxis run` adds nothing to that set. **Every diagnostic a well-formed program
can earn is decided during analysis**, so `praxis check` exiting 0 is a claim
about your program rather than about which passes happened to run. The sharpest
case is a pattern in a binding position, because deciding it needs the
scrutinee's type and the variant's payload — the sort of question a compiler is
tempted to leave until it is building code:

```praxis
enum Shape { Circle(Int), Square(Int) }

var shapes = [Circle(1), Square(2)]
for Circle(r) in shapes {
  out(r)
}
```

`praxis check` reports it and exits 1, and `praxis run` refuses it with the same
text at the same span before executing a line of it:

```text
error[Y125]: a `for` binding must match every item, and a variant pattern does not

  once-lowering-only.px:4:5
  4 | for Circle(r) in shapes {
    |     ^^^^^^^^^ a `for` binding must match every item, and a variant pattern does not

praxis: 1 error(s)
```

The one code raised past analysis is `Y099`, and it is not an exception to the
rule: it says inference recorded no type for a node lowering reached, which is a
compiler bug rather than a mistake in your program. No program you can write
earns it.

## Adding a code

A code is allocated by adding a variant to `DiagCode` in `praxis-source`.
`DiagnosticCode::new` is crate-private, so there is no way to construct an
unregistered number, and `DiagCode::code()`'s exhaustive match is the single
place a `(category, number)` pair is written. Attaching a `Suggestion` with a
`replacement` where the mistake is detected is all it takes for the new code to
have a quick fix in the editor — there is no table in the language server to
update.
