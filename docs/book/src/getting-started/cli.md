# The command line

`praxis` has three commands — `run`, `check` and `lsp` — and one global flag,
`--color`. The only other flags are `run`'s `--input` and `--debug`, and `lsp`'s
`--stdio`.

```console
$ praxis --help
Praxis is a small, statically typed, garbage-collected language for Advent of Code-style puzzle solving.

Homepage: https://github.com/tljubej/praxis

Usage: praxis [OPTIONS] <COMMAND>

Commands:
  run    Parse, type-check, JIT-compile, and run the program
  check  Run the front end (lex + parse + type-check) without executing
  lsp    Start the language server over stdio. Speaks JSON-RPC LSP on stdin/stdout; not meant to be run by hand
  help   Print this message or the help of the given subcommand(s)

Options:
      --color <COLOR>
          When to color diagnostic output: `auto` (default) colors iff stderr is a terminal; `always` forces color; `never` emits plain text
          
          [default: auto]

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

## `praxis run`

```text
praxis run <FILE> [--input <FILE>] [--debug auto|always|never] [--color auto|always|never]
```

`run` is the whole pipeline: lex, parse, resolve, infer, lower to typed HIR,
monomorphize, lower to MIR, verify, JIT-compile with Cranelift, execute. Nothing
is written to disk and there is no build artifact to clean up.

```console
$ praxis run hello.px
Hello, Praxis!
```

If the front end finds an error, nothing is compiled and nothing runs:

```console
$ praxis run sonar-draft.px --input sonar.in
error[Y110]: no method `length` on type `Vec[Int]` taking 0 argument(s)

  sonar-draft.px:4:20
  4 | for i in 1..depths.length() {
    |                    ^^^^^^ no method `length` on type `Vec[Int]` taking 0 argument(s)

error[N001]: `increase` is not defined

  sonar-draft.px:10:5
  10 | out(increase)
     |     ^^^^^^^^ `increase` is not defined

help: did you mean `increases`?
      increases

praxis: 2 error(s)
```

### What gets printed, and where

**A program's own output goes to stdout. Everything the compiler says goes to
stderr.** That split holds for diagnostics, for the runtime-fault report and for
the crash debugger's prompt. So `praxis run day05.px > answer.txt` captures the
answer and still shows you the errors, and `2>/dev/null` gets you the answer and
nothing else.

There is one more thing that can land on stdout. When the program's entry point
is an `fn main` with a return type other than `Unit`, `run` prints the value it
returned:

```praxis
fn main() -> Int {
    var values = read lines(int)
    values.len()
}
```

```console
$ praxis run count-lines.px --input count-lines.in
6
```

A file whose top level has statements uses those statements as the program
instead, and that generated entry returns `Unit` — which is why `out(...)` at
the top level does not also echo the last `out`'s argument as a result line.
[A file is a program](../language/program-structure.md) has the rule in full.

### `--input FILE`

Read the process input from a file rather than from standard input.

```console
$ praxis run sonar.px --input sonar.in
7
```

```console
$ praxis run sonar.px < sonar.in
7
```

The two are equivalent to the program and differ in when the bytes are read.
`--input` is read **eagerly**, before the program starts, so an unreadable path
is reported before any output is produced:

```console
$ praxis run sonar.px --input nope.txt
error: failed to read input file `nope.txt`: No such file or directory (os error 2)
$ echo $?
2
```

Standard input is read **lazily**, by the program's first `read` and never
before. A program with no `read` in it never touches stdin, which is what stops
`praxis run hello.px` from blocking forever on a terminal or on a CI harness
that is holding the pipe open. A terminal stdin reads as empty rather than
waiting for a human who was not asked for anything.

An I/O error on either path is reported and exits `2`. It is never laundered
into empty input: a truncated read would otherwise produce a confidently wrong
answer. An empty *file*, on the other hand, is input — a zero-byte `--input` is
passed through as the empty text, and what a parser makes of that is the
parser's business. `read lines(int)` over nothing is `[]`; a parser that needs
content faults at offset `0..0` and says what it expected to find there.

### `--debug auto|always|never`

What to do when the program faults at run time — division by zero, an index out
of bounds, integer overflow, a failed `read`, an explicit `panic`.

| value | behaviour |
|---|---|
| `auto` (default) | enter the interactive crash debugger **iff stdin and stdout are both terminals** |
| `always` | enter the debugger regardless, reading commands from stdin |
| `never` | print the noninteractive fault report and exit |

The default is the useful one at a keyboard and the safe one everywhere else:
piped, redirected or run from a script, `auto` behaves as `never`, so nothing
ever hangs waiting on a prompt nobody can see.

```console
$ praxis run divide-by-zero.px --debug never
error: program faulted: division by zero

Backtrace:
#0   <entry>

  locals:
    total: Int = 10
    n: Int = 0
  temps:
    <tmp#1: Int> @ "10" = 10
    <tmp#3: Int> @ "0" = 0
    <tmp#5: Int> @ "total / n" = <uninit>
    <tmp#6: Unit> @ "out(total / n)" = <uninit>
```

That report is the same state the interactive debugger would show you, printed
once instead of offered as a prompt. `--debug always` is how you drive the
debugger from a script: it reads its commands from stdin, which is what this
book's own debugger examples do. See [Noninteractive
mode](../debugger/noninteractive.md) and [Entering the
debugger](../debugger/entering.md).

## `praxis check`

```text
praxis check <FILE> [--color auto|always|never]
```

The front end only: lex, parse, resolve names, infer types, check `match`
coverage. No code is generated and the program is not run, so it is the fast
answer to "does this file make sense", and it is the command an editor's
save-hook wants.

On success it prints nothing:

```console
$ praxis check sonar.px
$ echo $?
0
```

`check` and the language server are not two implementations of the front end.
`praxis check` routes through the same query layer the LSP server answers from,
so a divergence between what the command line prints and what your editor
underlines is unrepresentable rather than merely unlikely. The sort order, the
decision to analyze a tree that already has parse errors, and the set of
diagnostics that reaches you are settled once, inside the query, and both
consumers read them from there.

`run` performs the same analysis before it compiles anything, so a file `check`
rejects is a file `run` refuses, with the same text. The reverse does not hold,
and in two ways. `check` cannot tell you about a fault that only happens when
the program runs. It also stops one pass earlier than `run` does: lowering to
typed HIR reports a handful of errors of its own — `Y013` for an integer literal
outside the range of `Int`, `Y125` for a `for` or closure binding that can fail
to match — and those reach you from `praxis run` only. Not from `check`, and not
from the editor either, which publishes the same query layer's diagnostics. A
clean `check` means the front end is satisfied, not that the file compiles.

## `praxis lsp`

```text
praxis lsp [--stdio]
```

The language server. It speaks JSON-RPC over stdin and stdout and is not meant
to be run by hand — the VS Code extension launches it, and so will any other LSP
client you point at the binary. Run it in a terminal and it will sit waiting for
a protocol header, then exit `1` when its stdin closes.

`--stdio` is accepted and ignored. Several clients append it to the server's
argv to select a transport, and stdio is the only transport this server has, so
the flag names something already true. It exists because the alternative was
exiting `2` on an argument the convention says is harmless — before a byte of
protocol was spoken — which every client reports as "the server crashed" rather
than as a bad flag.

The server is one synchronous loop with no async runtime. Its working set is a
single file and the front end answers in single-digit milliseconds, so there is
nothing for a second thread to do: the loop owns the document store and the
query cache outright, and holds no lock, because there is no other thread that
could want one. A `$/cancelRequest` drops a request still sitting in the queue;
one already being served runs to completion. [Editor
support](../tooling/editors.md) lists what it serves.

## `--color auto|always|never`

Global: it may be given before or after the subcommand. It styles diagnostics
throughout, and the `error:` label of the fault report. The rest of that report
— the backtrace, the locals, the temps — and every line the crash debugger
prints once it has a prompt are plain text whatever you pass.

| value | behaviour |
|---|---|
| `auto` (default) | style output **iff stderr is a terminal** |
| `always` | style even when piped |
| `never` | plain text |

`--color never` is what you want when capturing output for a test or a
transcript. `auto` already does the right thing when you redirect — the ANSI
codes are omitted because stderr is not a terminal — so `never` is for the case
where stderr *is* a terminal and you want plain text anyway.

## Exit codes

| code | meaning |
|---|---|
| `0` | success — the program ran to completion, or `check` found no errors |
| `1` | the file has errors and was not run, **or** it ran and faulted |
| `2` | usage or I/O — bad flag, unknown subcommand, missing argument, unreadable source or `--input` file |

`1` covers both "did not compile" and "compiled and then died", which are
distinguishable from the output but not from the status. If you need to tell
them apart in a script, run `praxis check` first: it returns `1` for exactly the
first case.

An internal compiler error — a MIR verifier failure, a JIT failure — also exits
`1`, with a message that begins `internal error:` or `error: JIT compilation
failed`. Those are compiler bugs, not program errors.
