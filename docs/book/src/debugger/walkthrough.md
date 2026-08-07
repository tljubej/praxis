# A walkthrough

This chapter is one session, start to finish: a puzzle program that runs,
faults, and gets fixed at the prompt it faulted into. Nothing in it is arranged
for effect. The program is the obvious first draft, the bug is the one that
draft has, and every transcript below is what the debugger printed when the
session was run.

## The program

Sonar sweep, part two. You are given a column of depth measurements and asked
how many three-measurement sliding windows sum to more than the previous
window's sum.

```praxis
// Sonar sweep, part two: count the three-measurement windows whose sum is
// larger than the previous window's.
fn window(depths: Vec[Int], i: Int) -> Int {
    depths[i] + depths[i + 1] + depths[i + 2]
}

fn count_increases(depths: Vec[Int]) -> Int {
    var larger = 0
    var i = 1
    while i < depths.len() {
        if window(depths, i) > window(depths, i - 1) {
            larger = larger + 1
        }
        i = i + 1
    }
    larger
}

var depths = read lines(int)
out(count_increases(depths))
```

The input is the ten-line sample:

```text
199
200
208
210
200
207
240
269
260
263
```

Run it in a terminal — `praxis run walkthrough.px --input walkthrough.in` — and
it faults. Stdin and stdout are a terminal, so the default `--debug auto` puts
you at the prompt without your asking for it. (`--debug always` forces it when
they are not; see [entering the debugger](entering.md).)

## The banner

Before the prompt appears, the debugger prints what the noninteractive mode
would have printed: the fault, the backtrace, and the innermost frame's state.

```text
error: program faulted: index out of bounds

Backtrace:
#0   window
#1   count_increases
#2   <entry>

  locals:
    depths: Vec[Int] = [199, 200, 208, 210, 200, 207, 240, 269, 260, 263]
    i: Int = 8
  temps:
    <tmp#3: Int> @ "depths[i]" = 260
    <tmp#4: Int> @ "1" = 1
    <tmp#5: Int> @ "i + 1" = 9
    <tmp#6: Int> @ "depths[i + 1]" = 263
    <tmp#7: Int> @ "depths[i] + depths[i + 1]" = 523
    <tmp#8: Int> @ "2" = 2
    <tmp#9: Int> @ "i + 2" = 10
    <tmp#10: Int> @ "depths[i + 2]" = <uninit>
    <tmp#11: Int> @ "depths[i] + depths[i + 1] + depths[i + 2]" = <uninit>
Entered crash debugger. 3 frame(s). Type `help` for commands.
```

An honest reading of that, before touching anything: an index went out of
bounds, in `window`, with `i` at 8 and a ten-element vector. The last three
temporaries say where it stopped. `<tmp#9> @ "i + 2" = 10` computed the index
10; `<tmp#10> @ "depths[i + 2]"` is the indexing itself and is `<uninit>`,
because it faulted and never produced a value; and `<tmp#11>`, the three-term
sum that would have consumed it, is `<uninit>` for the same reason.

The temps are why the banner is worth reading rather than skipping. Most are
labelled with the source expression that produced them as well as the value they
hold, so the frame is not just "these variables" but "this arithmetic, and how
far it got". The last temp with a real value is the last thing that worked.

## Confirming it

Start from `bt` anyway. It costs a line and it tells you which frame the
selection is on.

```text
Praxis crash> bt
#0   window
#1   count_increases
#2   <entry>
  (frame 0 selected)
```

Frame 0 is `window`, the innermost. `locals` re-prints what the banner showed —
and, unlike the banner, prints all of it, with no twelve-local cap.

```text
Praxis crash> locals
  locals:
    depths: Vec[Int] = [199, 200, 208, 210, 200, 207, 240, 269, 260, 263]
    i: Int = 8
  temps:
    <tmp#3: Int> @ "depths[i]" = 260
    <tmp#4: Int> @ "1" = 1
    <tmp#5: Int> @ "i + 1" = 9
    <tmp#6: Int> @ "depths[i + 1]" = 263
    <tmp#7: Int> @ "depths[i] + depths[i + 1]" = 523
    <tmp#8: Int> @ "2" = 2
    <tmp#9: Int> @ "i + 2" = 10
    <tmp#10: Int> @ "depths[i + 2]" = <uninit>
    <tmp#11: Int> @ "depths[i] + depths[i + 1] + depths[i + 2]" = <uninit>
```

Now ask the two numbers the fault is about, and then reproduce it:

```text
Praxis crash> p i
8
Praxis crash> p depths.len()
10
Praxis crash> p depths[i + 1]
263
Praxis crash> p depths[i + 2]
error: expression faulted: index out of bounds
```

That is the whole diagnosis of *what* happened. `depths[i + 1]` is the last
element; `depths[i + 2]` is one past the end and faults on demand, in the
debugger, from the same values the program had. A `p` expression runs against
the crash snapshot's own locals, so this is not a re-derivation — it is the
faulting expression, run again.

Two more cheap checks. The type is what you assumed:

```text
Praxis crash> type depths
Vec[Int]
```

and `source` shows the frame's function, so you do not have to go and find it:

```text
Praxis crash> source
window:
  <debug>:3:1
  3 | fn window(depths: Vec[Int], i: Int) -> Int {
    | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^...
  4 |     depths[i] + depths[i + 1] + depths[i + 2]
    | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^...
  5 | }
    | ^
```

The span is the whole function, not the faulting line, and the carets are
clamped to each line — `source` answers "which function is this frame", not
"which expression faulted". The temps answered the second question already.

## Finding the caller that is actually wrong

`window` is not the bug. `window(depths, 8)` on a ten-element vector is a
perfectly reasonable thing to refuse; the question is who asked. Go up a frame.

```text
Praxis crash> up
frame 1: count_increases
Praxis crash> locals
  locals:
    depths: Vec[Int] = [199, 200, 208, 210, 200, 207, 240, 269, 260, 263]
    larger: Int = 5
    i: Int = 8
  temps:
    <tmp#2: Int> @ "0" = 0
    <tmp#4: Int> @ "1" = 1
    <tmp#6: Int> @ "depths.len()" = 10
    <tmp#7: Bool> @ "i < depths.len()" = true
    <tmp#8: Int> @ "window(depths, i)" = 792
    <tmp#9: Int> @ "1" = 1
    <tmp#10: Int> @ "i - 1" = 6
    <tmp#11: Int> @ "window(depths, i - 1)" = 769
    <tmp#12: Bool> @ "window(depths, i) > window(depths, i - 1)" = true
    <tmp#13: Unit> = Unit
    <tmp#14: Int> @ "1" = 1
    <tmp#15: Int> @ "larger + 1" = 5
    <tmp#16: Unit> = Unit
    <tmp#17: Unit> = Unit
    <tmp#18: Int> @ "1" = 1
    <tmp#19: Int> @ "i + 1" = 8
    <tmp#20: Unit> = Unit
    <tmp#21: Unit> @ "while i < depths.len() { if window(depths, i) > window(depths, i - 1) { larger = larger + 1 } i = i + 1 }" = <uninit>
```

The line that answers it is in there:

```text
    <tmp#7: Bool> @ "i < depths.len()" = true
```

The loop guard evaluated to `true` on the iteration that faulted. That is the
bug stated in one line — the guard let `i = 8` through, and `window` needs
`i + 2` to be a valid index. The right-hand side of the guard is one temp above:
`depths.len()` is 10, so the guard admits `i` up to 9, and the largest `i` the
body can survive is 7.

Ask for the bound the loop should have had:

```text
Praxis crash> p depths.len() - 2
8
```

Eight — which is exactly the `i` that faulted. `i < depths.len() - 2` would have
stopped one iteration earlier.

Note also `larger: Int = 5`. The counting was finished before the crash: the
program had the right answer and then walked off the end anyway. That is a
common shape for an off-by-one, and a reason not to trust "it printed the right
number for the sample" as evidence of anything.

One more command, to close off the other explanation. The program reads its
input, so it is worth ruling the input out:

```text
Praxis crash> input
(no input context — not a parse failure)
```

The input parsed. Whatever is wrong is in the program. (When it is not, [the
parser commands](parser.md) are where the session goes instead.)

And because a bug that only happens sometimes is a different bug, confirm it is
deterministic:

```text
Praxis crash> restart
program faulted: index out of bounds
3 frame(s); frame 0 selected.
Praxis crash> bt
#0   window
#1   count_increases
#2   <entry>
  (frame 0 selected)
```

`restart` re-ran the same compiled code against the same input and got the same
fault, with a fresh snapshot and the frame cursor back at 0.

## Fixing it without leaving the prompt

The fix is four characters: `while i < depths.len() - 2`.

```praxis
    while i < depths.len() - 2 {
```

Edit the file in your editor — the debugger holds no lock on it — and type
`reload`. It re-reads the source from disk, recompiles it, and, if the compile
succeeds, discards the old machine code and snapshot and re-runs against the
same input the first run was given:

```text
Praxis crash> reload
5
program completed: Unit
Praxis crash> quit
```

The `5` is the program's own output, on stdout, and it is the right answer.
`program completed: Unit` is the debugger saying the run finished without a
fault and that the entry point returned `Unit`. You are still at the prompt — a
clean run does not exit the debugger — so `quit` is how the session ends.

If the edit does not compile, nothing is lost:

```text
Praxis crash> reload
error: parse error: expected `)`, found unexpected token
(session unchanged — the old snapshot is still active)
Praxis crash> p i
8
```

The old snapshot, the old locals and the old `p` are all still there; fix the
edit and type `reload` again. The same holds for a type error:

```text
Praxis crash> reload
error: type error: expected Text, found Int
(session unchanged — the old snapshot is still active)
```

## The fixed program

For completeness, the program that comes out the other end:

```praxis
// Sonar sweep, part two: count the three-measurement windows whose sum is
// larger than the previous window's.
fn window(depths: Vec[Int], i: Int) -> Int {
    depths[i] + depths[i + 1] + depths[i + 2]
}

fn count_increases(depths: Vec[Int]) -> Int {
    var larger = 0
    var i = 1
    while i < depths.len() - 2 {
        if window(depths, i) > window(depths, i - 1) {
            larger = larger + 1
        }
        i = i + 1
    }
    larger
}

var depths = read lines(int)
out(count_increases(depths))
```

```text
5
```

## What that session cost

Fifteen commands and one edit, and at no point did the program have to be re-run
with a print statement in it. That is the trade the crash debugger is making: a
fault is not a report about a program that has gone, it is a program that has
stopped, with its values still in the heap and its frames still readable.

The three habits worth taking from it:

- **Read the temps.** They carry the source expression that produced each value.
  The last temp with a value is where the computation got to, and the first one
  without is the operation that failed.
- **Reproduce the fault with `p`.** One line, against the real values. If it
  does not fault, your theory is wrong.
- **Go up.** The innermost frame is where the fault fired, and it is very rarely
  where the mistake is.
