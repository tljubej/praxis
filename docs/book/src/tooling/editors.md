# Editor support

Praxis ships a language server in the same binary as the compiler. `praxis lsp`
speaks JSON-RPC over stdin and stdout, and everything an editor knows about a
`.px` file comes from it — the diagnostics are the ones `praxis check` prints,
the types are the ones inference derived, the method list is the catalog
dispatch searches. There is a thin VS Code extension in `editors/vscode/` that
launches it; any editor with an LSP client can do the same.

There is no formatter. That is not an oversight, and it is covered at the end of
this chapter.

## Starting it

```console
$ praxis lsp
```

It reads framed LSP messages on stdin and writes them on stdout, so running it
by hand gets you a process waiting for a `Content-Length` header. Point your
editor's LSP client at that command with no arguments. `--stdio` is accepted and
ignored — several clients append it to the server's argv to select a transport,
and stdio is the only transport this server has, so refusing the flag would look
like a crash before a byte of protocol was spoken.

In VS Code, install the extension (see [The extension](#the-extension) below)
and set `praxis.binaryPath` to your `praxis` binary, or put it on `PATH`.

The process is a single synchronous loop on one thread. There is no async
runtime, no worker pool, and no lock, because the whole working set is one file
and a `rowan` syntax tree cannot cross a thread anyway
([ADR-095](../../../decisions/095-the-language-server-is-a-synchronous-stdio-loop.md)).
A `$/cancelRequest` that arrives while an earlier request is being served drops
the queued request; a request already running finishes.

## What it serves

This is exactly the capability set the server reports at `initialize`. Nothing
is advertised that is not implemented: an editor that is told the server handles
something stops offering its own behaviour for it, so a capability claimed and
not delivered is worse than one that is visibly missing.

| Request | What you get |
|---|---|
| `textDocument/publishDiagnostics` | Every error `praxis check` would print, after a 150 ms debounce |
| `textDocument/hover` | The inferred type; a method's signature and its one-line documentation; a parser constructor's signature and result type |
| `textDocument/completion` | Receiver methods and record fields after `.`, enum variants in a pattern, parser atomics and constructors inside a `read`, lexical names elsewhere |
| `textDocument/signatureHelp` | The callee's signature and which parameter the cursor is in |
| `textDocument/definition` | The declaration site of the name under the cursor |
| `textDocument/documentSymbol` | Top-level `fn`, `struct`, `enum` and `var`, with fields and variants nested under them |
| `textDocument/references` | Every use of *that binding* — not every occurrence of the word |
| `textDocument/rename` and `prepareRename` | A whole-file rename, or a refusal that says what it would have broken |
| `workspace/symbol` | The same symbols across every `.px` file under the workspace roots |
| `textDocument/inlayHint` | The type of every binding the source does not annotate |
| `textDocument/codeAction` | The quick fixes carried by the diagnostics in the requested range |
| `textDocument/semanticTokens/full` | Fourteen token classes, four of them for the input-parser sublanguage |

Text is synchronized incrementally, and `positionEncoding` is negotiated: the
server picks UTF-8 when the client offers it and falls back to UTF-16, the
protocol's default, when it does not. The conversion happens in one module at
the protocol boundary, so nothing below the language server has an opinion about
what a UTF-16 code unit is
([ADR-096](../../../decisions/096-positions-convert-at-the-protocol-boundary.md)).

Everything is scoped to **one file**. `workspace/symbol` is the only query that
reads the disk, and it parses rather than analyzes.

### Diagnostics

The editor's underlines and `praxis check`'s output come from the same query
layer. `praxis check` does not have a pipeline of its own: it builds a snapshot,
asks it for diagnostics, and renders them
([ADR-097](../../../decisions/097-the-shared-query-layer-lives-in-praxis-lsp.md)).
So the set, the order and the decision to analyze a tree that already has parse
errors are stated once and read by both. A diagnostic you can see in the editor
and not on the command line is not a thing that can happen.

Reports are published after a 150 ms pause in typing. The debounce is there so a
half-typed `.` does not flash an error the next keystroke retracts.

Every code the server can publish is listed in
[Diagnostic codes](diagnostics.md).

### Hover

Hover prefers the innermost thing it can name. Inside a `read` body that is the
parser expression, because every other map is silent in there; then a method
name; then a name reference or its declaration; then a name in type position;
then the innermost expression with a recorded type.

A method hover is the catalog row dispatch selected, so the signature shown is
the one the compiler will use, and the sentence under it is the catalog's own:

```text
Vec[Int].sum() -> Int

Sum the (Int) elements.
```

A parser constructor hovers as its signature and its documentation, followed by
the type the whole expression synthesizes and what that type is the type *of*:

```text
lines(parser) -> Vec[T]

Split the region into lines and apply the parser to each. Every line must be consumed whole.

Vec[Int]

input parser result
```

A [prelude](../language/prelude.md) name keeps its scheme and gains §16.1's own
sentence, which for the graph helpers is most of what there is to know — the
scheme names two type variables and does not say that the closure *is* the
graph:

```text
bfs: forall T. (T, (T) -> Vec[T]) -> Vec[T]

Breadth-first walk: `bfs(start, |s| neighbors(s))` answers every state reached, in the order it was reached.
```

A name in **type position** hovers too, and answers with what the type is:

```text
Int

Signed 64-bit integer. Written `42` or `1_000_000`.

built-in type
```

Both sentences come from `crates/praxis-stdlib/src/prelude.rs`, the table name
resolution seeds the root scope from — so a prelude name the compiler declares
and a prelude name the editor can describe are the same list.

**A binding that shadows one of these is described as itself.** `var out = 1`
hovers as `out: Int` with no sentence under it: the lookup is by symbol, and a
prelude symbol is the one with no declaration site. A lookup by spelling would
put "Write one value to stdout" under a local that does nothing of the kind.

### Completion

The context is decided before the list is built, and the order of the tests is
the order of specificity: a `.` beats everything, then the parser sublanguage,
then a record literal, then a match pattern, then the lexical fallback.

After a `.`, the receiver's type is read from what inference already recorded for
the expression to the left of the dot — `rows.` does not parse as an expression,
but `rows` does, and that is enough. Fields come first, then every catalog method
whose receiver pattern matches, each carrying its signature as the item's detail
and the catalog's sentence as its documentation — so `rows.` on a `Vec[Int]`
offers `push` as `(T) -> Unit`, `len` as `() -> Int`, `get` as `(Int) -> T`,
`map` as `((T) -> U) -> Vec[U]`, and so on down the catalog.

The filter is `pattern_matches` — the same function method dispatch calls, not a
restatement of it — so a method the list offers is a method the call will
resolve. The index operators (`[]`, `[]=`, `[]min=`, `[]max=`) are catalog rows
too and are excluded, because `grid.[]` is not syntax.

Inside a parser expression you get §7.4's atomics and §7.5's constructors, each
with its own description as documentation, plus the enclosing constructor's own
keyword argument (`skip:` for `chars`, `fill:` for `grid`) and `grid`'s `ragged`
flag. Those come from `Constructor::keyword_arg`, so a constructor added to the
language is offered without anybody updating a list.

The lexical fallback offers what is in scope, and that is mostly the stdlib:
thirty-one prelude names and seven built-in type names against however many the
file declares. Each carries its description, and a type name — which has no
scheme, because nothing instantiates `Int` — says `type` as its detail. A
`match` over an `Option` offers `Some` and `None` with the prelude's sentences;
a user enum that happens to spell a variant `Some` gets nothing, because the
description belongs to `Option` and not to the word.

Trigger characters are `.`, `` ` ``, `{` and `:` — the last three because
completion inside a template fires on text that is not yet an expression.

### Signature help

Two kinds of callee. An ordinary call or method call answers with the scheme
inference gave it or the catalog entry dispatch selected; a parser constructor
answers from §7.5's argument-shape table — so a constructor added to the
language has a signature without anybody writing one, and the cursor inside
`read lines(…)` gets back `lines(parser) -> Vec[T]`.

Each of the three carries its documentation, and this is where it is worth the
most: `clamp`'s parameters render as `Int, Int, Int` and `a_star`'s as four bare
closure types, so which one is the low bound and which is the heuristic is
precisely what the labels cannot say. A constructor with two forms carries it on
both, rather than on whichever the editor preselects.

The active parameter is counted from the top-level commas before the cursor, so
it is the parameter you are actually typing. A comma nested inside another
call's arguments belongs to that call and is not counted here.

### Navigation

Definition, references and rename all start from the same lookup: the **symbol**
the word denotes, not the word. Two shadowed bindings share a spelling and have
distinct symbols, so asking about one never returns the other's uses — which is
the property a text search cannot have. `references` honours the client's
`includeDeclaration` flag rather than ignoring it.

`workspace/symbol` walks the workspace folders for `.px` files, skipping
`target/`, `node_modules/` and dotted directories, capped at 2000 files and 16
levels deep. There is no persistent index: the walk runs per query, because an
AoC workspace is tens of small files and a cache would need file-system events
the server would then have to be right about. An **open buffer beats the file on
disk**, so a name you just deleted is not offered. With no workspace folders at
all the picker answers from the open documents, which is what VS Code's
single-file mode needs.

### Semantic tokens

Full-document only; deltas are not implemented. The legend has fourteen entries:
the ten ordinary ones (`keyword`, `type`, `function`, `method`, `variable`,
`parameter`, `property`, `enumMember`, `number`, `string`) and four for the
input parser — `parserConstructor`, `parserTemplateText`, `parserCaptureName`
and `parserCaptureType`.

The four parser classes are read from the compiler's own spanned index of the
parser expression. Where a capture's name stops and its type begins is something
only the compiler knows, and a second scanner in the language server would be
free to disagree with it. Parser tokens are collected first and win every
overlap, because a backtick template is one token to the lexer and four to the
editor.

### Inlay hints

Hints are **on**, and the rule is one line: every binding whose type the source
does not already state. A `fn` parameter, a closure parameter, a `var`, a `for`
variable and a name a pattern introduces are all the same thing — a name bound
to a value — and they are all in `Analysis::decls`, which is where the rule is
read from. One hint belongs to no binding: a `read` or `parse` expression that
nothing binds carries its result type at the end of the expression, because
there is no name to hang it on.

```praxis
fn add(a, b) { a + b }

var total = 0
for n in [1, 2, 3] {
  total = add(total, n)
}
out(total)
```

```text
6
```

In the editor that file reads as `fn add(a: Int, b: Int)`, `var total: Int = 0`
and `for n: Int in [1, 2, 3]`.

Two details worth knowing. A type that inference has not pinned shows as `?T` —
the same spelling hover and `praxis check` use — rather than being hidden, because
hiding it would make "no hint" mean both *the source already says this* and *the
compiler does not know*. And a hint carries an edit that writes the annotation
into the file only where the annotation is legal and spellable: a `for` variable
has no annotation syntax, and neither `?T` nor an anonymous record is something
the parser would read back. Those hints show and cannot be accepted, which beats
an edit that does not compile.

The server has no setting to turn them off. `editor.inlayHints.enabled` is the
editor's, and a second switch would be a second place for the answer to live.

### Code actions

A quick fix is a diagnostic's machine-applicable suggestion, and it is written
by the pass that found the mistake — not by a table of common errors kept in the
language server
([ADR-132](../../../decisions/132-a-code-action-is-a-diagnostics-machine-applicable-suggestion.md)).
The whole of `code_action.rs` is the twenty lines that turn a suggestion with a
`replacement` into a `WorkspaceEdit`. It knows nothing about any particular
diagnostic, and a suggestion with no replacement stays advice: it is already in
the message as a `help:` line, and an action that changes nothing is a menu
entry that does nothing.

The consequence is that every fix the editor offers is one `praxis check` also
prints. Four families carry one today.

**An unknown parser constructor, atomic or capture kind** — `I013`, `I010`,
`I012` — against §7.4's and §7.5's two tables searched as one list:

```praxis
var rows = read line(int)
out(rows.len())
```

```text
error[I013]: unknown parser constructor `line` (§7.5)

  quick-fix-constructor.px:1:17
  1 | var rows = read line(int)
    |                 ^^^^ unknown parser constructor `line` (§7.5)

help: did you mean `lines`?
      lines

praxis: 1 error(s)
```

The action is titled *Did you mean `lines`?* and replaces the four characters the
caret sits under. The report points at the constructor's name rather than the
whole call, because a fix replaces what the report underlines.

**An unknown name** — `N001` — against the scope chain the resolver was holding
when the lookup failed:

```praxis
var counts = [1, 2, 3]
var total = 0
for n in counts {
  total += n
}
out(totl)
```

```text
error[N001]: `totl` is not defined

  quick-fix-name.px:6:5
  6 | out(totl)
    |     ^^^^ `totl` is not defined

help: did you mean `total`?
      total

praxis: 1 error(s)
```

**An unknown method** — `Y110` — against the catalog rows dispatch would have
searched, so the offered call is one that would resolve:

```praxis
var xs = [3, 1, 2]
out(xs.sortd())
```

```text
error[Y110]: no method `sortd` on type `Vec[Int]` taking 0 argument(s)

  quick-fix-method.px:2:8
  2 | out(xs.sortd())
    |        ^^^^^ no method `sortd` on type `Vec[Int]` taking 0 argument(s)

help: did you mean `sorted`?
      sorted

praxis: 1 error(s)
```

**A non-exhaustive match** — `Y120` — from the same witnesses the message names:

```praxis
enum Dir { North, South, East, West }

fn step(d: Dir) -> (Int, Int) {
  match d {
    North => (0, -1)
    South => (0, 1)
  }
}

out(step(North))
```

```text
error[Y120]: non-exhaustive match: missing `East`, `West`

  quick-fix-match.px:4:3
  4 |   match d {
    |   ^^^^^^^^^ non-exhaustive match: missing `East`, `West`...
  5 |     North => (0, -1)
    | ^^^^^^^^^^^^^^^^^^^^...
  6 |     South => (0, 1)
    | ^^^^^^^^^^^^^^^^^^^...
  7 |   }
    | ^^^

help: add the missing match arms
          East => panic("todo")
          West => panic("todo")

praxis: 1 error(s)
```

One threshold decides when a near miss is near enough, for all four: an edit
distance within `max(1, len / 3)`, counted in characters so a non-ASCII
identifier is not silently excluded. It refuses more than a reader expects —
`v.lenght()` gets no fix, because `lenght` is three edits from `len` — and that
is the rule working. At a budget wide enough to catch it, `abc` starts
suggesting `xyz`.

Actions are computed from the server's **own** diagnostics at the current
revision, not from the `context.diagnostics` a client echoes back: those are
from whatever version the client last received, and an edit computed against
text that has since changed lands on the wrong bytes.

### Rename

A rename is accepted when applying it changes nothing but the spelling. The
server writes the edit into a copy of the file, analyzes the copy, and requires
name resolution to come out the same: every reference resolving to the symbol it
resolved to before, and no diagnostic code's count going up
([ADR-131](../../../decisions/131-a-rename-is-safe-when-re-resolution-is-unchanged.md)).

The alternative would have been a list of collision kinds, and the argument
against it is that nobody can be sure they finished the list. Asking the
resolver directly covers the cases somebody would have written down and the ones
they would not — including a reference to *another* binding of the new name that
starts resolving to the renamed one.

A refusal comes back as a request error carrying a sentence, because a client
shows an error and silently ignores an empty edit. Take the seven-line program
from [Inlay hints](#inlay-hints) above, and put the cursor on its `total`.
Renaming it to `out`:

```text
renaming to `out` would change what `out` on line 7 refers to
```

Renaming it to `add`:

```text
renaming to `add` would change what `add` on line 5 refers to
```

Renaming it to a keyword:

```text
`match` is a keyword
```

Renaming it to `sum` is accepted, and rewrites all four occurrences.

Spelling is checked first, against the lexer's own keyword table and its own
identifier rule, so a keyword added to the language later is refused here
without anybody remembering to update the server.

Two consequences. A rename costs one extra full analysis — a few milliseconds on
a puzzle-sized file, for an operation you perform by hand and wait for. And it
is conservative in one direction on purpose: renaming a binding *to* a typo
somebody wrote elsewhere is refused, because the other name would start
resolving to this binding, and that is a capture whether or not you meant it.

`prepareRename` refuses a position whose symbol has no declaration site. A
prelude name like `out` or `Vec` is declared in the compiler, and renaming it in
one file would rename nothing.

## The extension

`editors/vscode/` holds a VS Code extension that is intentionally thin. It
registers `.px`, provides comment/bracket/indent configuration, launches
`praxis lsp`, ships a TextMate grammar, and exposes four commands. There is no
parsing and no type logic in TypeScript — everything the editor knows comes from
the compiler over the protocol, so the two cannot disagree.

| Command | What it runs |
|---|---|
| `Praxis: Run File` | `praxis run <file>`, with `--input input.txt` appended when that file sits beside the source |
| `Praxis: Check File` | `praxis check <file>` |
| `Praxis: Watch File` | `praxis watch <file>` — **not implemented**; the binary says so and exits 2 |
| `Praxis: Restart Language Server` | Stops and relaunches the server process |

The first three run in an integrated terminal rather than an output channel,
because the [crash debugger](../debugger/entering.md) is interactive and an
output channel cannot answer a prompt. The document is saved first: `praxis`
reads the file from disk, so running an unsaved buffer would report on code you
are no longer looking at.

Two settings. `praxis.binaryPath` (default `praxis`) is the one path every
command and the server use; changing it restarts the server, because pointing at
a different build is a relaunch and not a reconfiguration. `praxis.trace.server`
turns on JSON-RPC tracing.

To install it, package the directory and load the result:

```console
$ cd editors/vscode
$ npm install && npm run compile && npx @vscode/vsce package
$ code --install-extension praxis-0.1.0.vsix
```

Then point `praxis.binaryPath` at your build, or `cargo install --path
crates/praxis-cli` to put `praxis` on `PATH`.

### The TextMate grammar

Highlighting arrives twice. `syntaxes/praxis.tmLanguage.json` paints a file
instantly — before the server attaches, while it is restarting, and anywhere
that does not speak LSP — and semantic tokens refine it once analysis has run.

The two layers must not fight, so they emit the **same** TextMate scopes: the
grammar directly, and the semantic tokens through the extension's
`semanticTokenScopes` map.

| Construct | Scope |
|---|---|
| Parser constructor (`lines`, `grid`, …) | `entity.name.function.parser.praxis` |
| Template literal text | `string.quoted.other.template.praxis` |
| Capture name (`n` in `{n:int}`) | `variable.other.capture.praxis` |
| Capture type (`int` in `{n:int}`) | `support.type.capture.praxis` |

Where they are allowed to disagree is which identifiers are parser constructors:
`lines` is a constructor inside a parser expression and an ordinary name outside
one, and a regular expression can only approximate the region. The disagreement
resolves toward the compiler, because semantic tokens win.

A grammar's keyword list is a copy of the lexer's that no compiler checks, and
the failure mode is invisible — a word quietly stops being coloured and nobody
files it. So `crates/praxis-cli/tests/grammar.rs` reads these JSON files at test
time and asserts that every keyword in the lexer's table, every
`AtomicKind::keyword()` and every `Constructor::keyword()` appears in the
grammar, and that every custom semantic token type maps to a scope the grammar
emits. That runs in the ordinary Rust test suite, so checking the extension for
drift needs no Node toolchain.

## There is no formatter

`praxis fmt` does not exist, `documentFormattingProvider` is not advertised, and
the range and on-type variants are not advertised either. This was a scope
decision, not an omission: an editor told this server formats would stop
offering its own behaviour and then do nothing on `Format Document`, which is
worse than the feature being visibly missing. As things stand, VS Code keeps
whatever it would have done by itself.

Three other things named in the design document are also not implemented:
unused-binding warnings (the compiler emits no warnings at all — every
diagnostic it can produce is an error), semantic token deltas and incremental
reparse, and multi-file analysis. Rename, references and diagnostics are one
file; `workspace/symbol` is the only query that looks past it.
