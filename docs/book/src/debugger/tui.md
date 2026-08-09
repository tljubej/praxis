# The full-screen debugger

On a terminal, a fault opens a full-screen debugger: the frame chain, the
selected frame's source, and its locals, all on screen at once, with the arrow
keys moving between frames.

```text
 ✗ division by zero  ·  3 frame(s)
╭ backtrace ───────────────────────╮╭ ratio · ratio.px ────────────────────────────────────────╮
│▶ #0  ratio   :4                  ││  3 │ fn ratio(a, b) {                                    │
│  #1  step    :8                  ││▶ 4 │     a / (b - a)                                     │
│  #2  <entry> :15                 ││  5 │ }                                                   │
│                                  ││                                                          │
╰──────────────────────────────────╯│                                                          │
╭ locals ──────────────────────────╮│                                                          │
│ bindings                         ││                                                          │
│  a     Int = 20                  ││                                                          │
│  b     Int = 20                  ││                                                          │
│ temps                            │╰──────────────────────────────────────────────────────────╯
│  tmp#3 Int = 0                   │╭ output ──────────────────────────────────────────────────╮
│  tmp#4 Int = <uninit>            ││Type `:` to run a command, `?` for keys.                   │
│                                  ││↑↓ or j/k select a frame; u/d walk the call stack.        │
╰──────────────────────────────────╯╰──────────────────────────────────────────────────────────╯
 backtrace   ↑↓ frame  tab pane  : cmd  r restart  ? keys  q quit
```

That screen is the whole diagnosis of this crash. `b - a` is `tmp#3`, and it is
`0`; `tmp#4` is the divide that never produced a value; and the `▶` is on the
line both of them came from.

This is a view over the same engine the [command reference](commands.md)
documents — every command still runs through it, so the two surfaces cannot
answer the same question differently. What the screen adds is that you do not
have to ask: moving to a frame shows you its source and its locals together,
which on the line-oriented prompt took `up`, `locals`, `source`, and holding the
results in your head.

## Which surface you get

A terminal gets the full-screen debugger. Anything else gets the
[line-oriented prompt](commands.md), and that is the correct surface for it
rather than a lesser one — a pipe has no keystrokes to read and no screen to
draw on.

| Standard input and output | `--debug` | What you get |
| --- | --- | --- |
| A terminal | `auto` or `always` | The full-screen debugger |
| A pipe, a redirect, a CI runner | `always` | The `Praxis crash>` prompt |
| A pipe, a redirect, a CI runner | `auto` | [The report, then exit 1](noninteractive.md) |

A [`:bp` stop](breakpoints.md) reads the same table, with the last row's exit
replaced by "and the program carries on".

So the scripted sessions throughout this chapter still behave exactly as
written: `printf 'bt\nquit\n' | praxis run … --debug always` is a pipe, and takes
the prompt.

The crash report is printed before the screen opens, and the screen is an
*alternate* one — so quitting the debugger reveals the report still sitting in
your scrollback. You keep both.

## The panes

**backtrace** — every frame, innermost first, with the line each one faulted on.
`▶` marks the selection. That line number is the *faulting* line, not the line
the function is declared on: for frame 0 it is where the fault happened, and for
a caller it is the call that led there.

**source** — the selected frame's function, with `▶` on the faulting line and the
faulting subexpression underlined inside it. The frame's recorded span covers the
whole function, so the marked line is recovered from the temps instead: a temp
that carries a source span but never received a value is an expression that
started evaluating and did not finish, and the narrowest one is the innermost
such expression.

The pane opens on the marked line, not on the function's first line — in anything
longer than the pane those are not the same place, and the fault is the part you
came to see. If the marked line already fits on the first screenful the pane stays
at the top, so the signature stays visible; past that it centres the fault. `↑`
and `↓` scroll from wherever that lands, and changing frame returns to it.

**locals** — the selected frame's slots, in the same two sections `locals`
prints: `bindings` for what you wrote, `temps` for the compiler's intermediates
with the source expression each materialized. Values are cut to the width of the
column at an element boundary, so a long collection reads `[0, 1, 2, ...]` rather
than running off the pane mid-element.

**output** — a transcript of the commands you have run and what they answered.

## Keys

Press `?` for this list without leaving the debugger.

| Key | Does |
| --- | --- |
| `↑` / `k` | Select the frame above — toward `#0` |
| `↓` / `j` | Select the frame below |
| `home` / `end` | The first / last frame in the list |
| `u` / `d` | Up / down the *call stack*, from whichever pane has focus |
| `tab` / `shift-tab` | Move focus between panes |
| `pgup` / `pgdn` | A page of whatever the focused pane counts in — frames in the backtrace, lines elsewhere |
| `c` | `continue` — let a program stopped at a [`:bp` marker](breakpoints.md) run on |
| `p` | Open the command line already primed with `p ` |
| `r` / `R` | `restart` / `reload` |
| `l` / `b` | Run `locals` / `bt` into the output pane |
| `i` / `P` | `input` / `parser` context |
| `:` | Type any command |
| `?` | The key list; any key dismisses it |
| `q`, `ctrl-c` | Quit |

The arrows are *spatial*: they move the highlight the way they point. Since the
backtrace is drawn innermost-first, `↓` goes to a **higher** frame number and `↑`
back toward `#0`. `home` and `end` are the two ends of that list.

`u` and `d` are the other thing you might mean — the *call stack*, in the sense
the [`up` and `down` commands](commands.md#up-down) use, so a keypress and a typed
command never disagree. `u` selects the caller, which on an innermost-first list
is downward on screen; that contradiction is why the call-stack motion has its own
pair of keys rather than being hung on the arrows.

They are also the way to change frame without first moving focus: arrows scroll
whichever pane holds focus, while `u` and `d` move frames from anywhere.

## The command line

`:` opens a command line that accepts everything in the
[command reference](commands.md), including `p EXPR`, `type EXPR`, `heap EXPR`,
`restart` and `reload`. Results land in the output pane.

```text
╭ output ──────────────────────────────────────────────────╮
│Type `:` to run a command, `?` for keys.                  │
│↑↓ or j/k select a frame; u/d walk the call stack.        │
│❯ p b - a                                                 │
│0                                                         │
╰──────────────────────────────────────────────────────────╯
```

`↑` and `↓` walk the command history while you are typing, `esc` abandons the
line without running it, and `ctrl-c` does the same. Since `p EXPR` is the
command you reach for most, `p` on its own opens the line with that prefix
already typed.

`quit` typed as a command does what `q` does.

## What is not here

No `continue`, `step`, `next`, or breakpoints, and no way to change a value —
for the same reasons the [command reference](commands.md#what-is-not-here) gives.
A full screen does not change what a faulted program can be asked to do.
