# ADR-150: A marker is a place in the program, and a stop is not a fault

**Date:** 2026-08-09
**Status:** accepted
**Milestone:** 12

## Context

§9 builds a debugger that a program can only reach by failing. §9.4's command
list, §9.5's evaluator, §9.6's fallback and §9.7's re-run are all downstream of a
fault, and `commands.md` said so plainly: "There is no `continue`, no `step`, no
`next`, and no breakpoint."

That is the right shape for a *crash* debugger and the wrong shape for the
question a puzzle solve actually asks, which is not "why did this fail?" but
"what does it hold on the third pass?". The answer today is `out(…)`, edited in
and edited back out, which prints one value and loses the frame around it — the
backtrace, the other locals, the types. Everything the crash debugger already
renders is exactly what that question wants, and the only thing missing is a way
to arrive there without breaking the program first.

The machinery is already in place. ADR-104 keeps a live frame's metadata static
and its values in a per-call slot run; ADR-106 makes the collector clear the slots
whose objects it reclaimed, so a slot below the debug stack's `top` names a live
object or nothing. `praxis_snapshot_debug_chain` walks exactly that and deep-copies
it. What makes the walk a *fault* thing is not the walk — it is the `taken` guard,
the fault kind, and the slot the copy is written into.

## Decision

### 1. The marker is `:bp`, decided by position and adjacency

A statement may end with `:bp` — a `COLON` immediately followed by an `Ident`
spelling `bp` — which the parser folds into a `BREAKPOINT` node **inside** the
statement's own node.

This is [ADR-070](./070-an-updating-store-is-a-row-with-a-contextual-operator.md)'s
rule at a second position.
`bp` is an ordinary identifier and a lexer rule claiming `:bp` would take it away
from every program that has a binding by that name, so the decision is contextual:
at the end of a statement a `:` begins nothing else in the grammar, and that is
the one place the marker is admitted. Adjacency is what separates `:bp` from
`: bp`, exactly as it separates `min=` from `min =`.

**Not a function.** A callable `bp()` would be a name in scope — shadowable,
passable, storable in a `Vec` — and none of those is a thing a breakpoint can
mean. It would also need an argument list it has nothing to put in, and a
`Purity` row saying whether the debugger's own evaluator may call it. A marker on
a line has none of those questions.

**Inside the statement, not beside it.** The alternative — a wrapper node around
the marked statement — puts a node between `lower_block` and the `ExprStmt` it
tests for, which is the test that decides a block's tail. A child is a question
the statement node answers about itself.

### 2. A marker stops **after** the statement it marks

`var doubled = seed * 2 :bp` stops with `doubled` bound to `40`.

The two orderings are not equally useful. Stopping *before* shows the state that
the previous statement already showed; stopping *after* shows what the marked
statement did, which is why it was marked. "To see the state before a statement,
mark the one above it" is a rule with no exceptions; the other direction has no
spelling at all.

The rule holds at the one position that is not a statement. A block's trailing
expression **is** the block's value, so a stop cannot follow it as a statement —
`TypedBlock::tail_bp` carries the marker instead, and the MIR builder emits the
stop after the tail has been lowered into its local and before the block yields
it. A file's top-level statements never reach that path: their entry point's tail
is a synthesized `Unit` literal ([ADR-067](./067-a-files-top-level-statements-are-its-program.md)), so a marker on
a file's last line is a statement's like any other.

`TypedStmt::Breakpoint` is a statement of its own rather than a flag on the other
five. A flag would have to say, somewhere, whether the stop is before or after
the binding is stored; an ordering says it by being one.

### 3. The handler is given a snapshot and no context

`praxis_breakpoint(ctx, span_start, span_end)` deep-copies the live chain the way
a fault epilogue does, and hands the copy to a host `fn(BreakpointStop) -> Resume`
that receives **no `*mut RuntimeContext`**.

That absence is the load-bearing part, and it is what makes the copy sound
without registering it as a root set. Every reference in it names a live object at
the instant it is taken — ADR-106's weak arm is what guarantees that — and stays
live for exactly as long as no collection runs. A handler with no context has no
heap pointer, no allocator and no way to run generated code, so it *cannot*
collect. The window is the call, and the type is what closes it.

A fault snapshot cannot be built this way, and the difference is instructive: the
host holds one across `restart`, `p EXPR` and every other thing that allocates, so
it has to be an arm of `RuntimeRoots`. A stop's does not outlive its call.

Three properties of the fault path are therefore dropped rather than reused:

- **No `taken` guard.** A fault snapshots once because the unwind calls it once
  per frame. A marker in a loop stops every pass, and the tenth stop must show the
  tenth state.
- **No `SnapshotSlot`.** Writing a stop into the crash slot would make a later
  *real* fault find one already taken and skip its own.
- **No fault kind.** `FaultKind::None` is the honest answer: nothing went wrong.

### 4. `Resume` has two arms, and no third that ends the program

`Continue` returns to the program. `Detach` returns to the program and disarms
every later stop, which is what "quit the debugger" means from inside a live run.

There is nothing sound to put in a third arm. §9.2 forbids unwinding Rust through
JIT frames, and the one path that *does* unwind them is the fault epilogue, reached
by raising a fault — so a `kill` would report the program as having failed when it
did not. A host that wants the run over exits its own process; the language does
not lie about what happened.

### 5. The wrapper is `Effect::Pure`, so a marker costs one call

Nothing in decision 3 allocates and nothing can fault, so the manifest row is
`Pure` and the consequences follow from the row rather than from a special case in
the backend: no root spill before the call, no `CheckFault` after it, and no
`DebugSlots` — ADR-104 already writes a debug slot per *definition*, so the frame
the handler walks is current without anything being written at the stop.

A marked statement is the unmarked statement plus a `call`. A program with no
marker emits nothing at all.

The span rides as two `RawU32` immediates. Boxing it so it could travel as a
`GcRef` argument would put an allocation at the one site whose whole claim is that
it does not have one.

`RUNTIME_ABI_VERSION` does not move, which is
[ADR-149](./149-a-chunking-partitions-and-a-window-slides.md)'s rule for its own
two rows: appending a symbol moves no field, changes no wrapper's meaning, and
adds no *field* generated code reads. A runtime that predates the symbol cannot
serve a call to it, but a missing import is a link error at `Jit::compile` and not
a misread — the failure mode the version guards against is the silent one.

### 6. A stop is the same debugger, minus what it cannot do

`Repl` gains an `Attached` enum in place of its `Option<DebugSession>`: `Nothing`
(unit tests), `Fault` (a fully-unwound run, every command), `Stopped` (live
frames, a lent `TypeDb` and source text).

What a stop loses is what would *execute*. `p EXPR` compiles a function and runs
it, and the program's own frames are still on the shadow and debug stacks —
`Runtime::context`'s contract already rules out a second context executing over
them, because a fresh one starts with the full stack budget while the native stack
is already deep. `restart`/`reload` re-run a program that is in progress. Neither
is a policy choice.

`type EXPR` stays, and stays for a reason worth stating: it type-checks against
the frame's captured locals and executes nothing, so the thing that rules the
other two out does not reach it.

What a stop gains is `continue`, and `Control` grows a third arm to carry it:
`Resume` and `Quit` are different answers for the host, and a `bool` would report
both as neither.

**The help text is separate**, not the fault list with notes appended. Listing a
command the surface will refuse is how a user learns to distrust the help.

### 7. `--debug` decides a stop's surface exactly as it decides a fault's

`never` makes a marker inert. A declining `auto` prints the frame and locals to
stderr and the program **continues** — §9.6's shape minus the exit, which turns a
marker into a trace point for a script, a pipeline or CI. An accepting `--debug`
opens the full-screen debugger on a terminal and the `Praxis stop>` line prompt
off one.

A user who has learned what `--debug` does to a crash should not have to learn a
second thing about a stop. The declining case is also the one that keeps a piped
`praxis run` from having the debugger read the input the program was going to
(§7.10).

**The host disarms stops before handing a fault off.** `restart` re-runs the
program from inside the debugger's own screen, and a stop handler firing there
would take the terminal from the debugger already holding it.

### 8. The stopped frame's line is known, not inferred

`fault_span` finds a frame's line by looking for the narrowest temp that carries a
span and never received a value — an expression that started and did not finish.
That is the right inference for a fault and the wrong answer for a stop: the
marked statement has *completed*, so its temps are all written, and the inference
would point at the next line.

A stop knows its line, because the marker's span rode along with it. `marked_span`
uses it for frame 0 and keeps the inference for the callers, which are mid-call
exactly as a fault's are.

## Consequences

**`praxis check` says nothing about a marker.** It is well-formed syntax with no
type of its own, so there is no diagnostic to raise from the front end. Whether a
committed marker deserves a warning is a real question and this decision does not
answer it: a warning that fires on a line the author is actively debugging is the
kind that gets silenced, and the book says the thing instead.

**There is no `step` or `next`.** Both need the debugger to place a stop where the
program has no marker, which means either patching code or a per-statement check —
one is a second compilation path and the other is the cost decision 5 exists to
avoid. `:bp` on the next line is what a `next` would have done, and the source is
right there.

**A stop cannot nest.** The handler is *taken* rather than borrowed for the
duration, so a marker reached from inside a stop finds none installed and returns.
Nothing in the language can reach one today — the handler runs no Praxis code —
and the taking is what keeps that true if something later can.

**The gates.**
`parse::a_breakpoint_marker_is_a_marker_only_where_a_type_cannot_be` is decision
1: every statement form, the marker as a *child*, the space that is not a marker,
and `bp` still an ordinary name.
`grammar::the_breakpoint_marker_has_a_rule_of_its_own` is the editor half, and
exists because Gate 1's sweep over `SyntaxKind`'s keywords will never reach a
word that is not one.
`hir_tests::a_breakpoint_marker_lowers_to_a_stop_after_its_statement` and
`a_demoted_tails_marker_is_demoted_with_it` are decision 2 — the second is the
case a tail that turns out not to be one creates, where a marker left on the block
would fire a statement late.
`build::a_breakpoint_is_one_bare_call_where_the_statement_ended` is decision 5 as
three absences (no `RootSlots`, no `DebugSlots`, no `CheckFault`), and
`a_marker_on_a_branching_statement_stops_once_on_the_join` is decision 2 for a
statement with more than one path through it, and
`jit::a_marker_on_a_tail_stops_after_it_and_keeps_its_value` is its other half —
the position that is not a statement, where the value has to survive the stop.
`jit::a_breakpoint_stops_with_live_frames_and_the_program_finishes` is decision 3
end to end: the caller is still in the chain, so the frames had not unwound, and
the program answers its own value, so the stop returned.
`a_marker_in_a_loop_stops_every_pass_until_the_host_detaches` is the dropped
`taken` guard plus decision 4, and
`a_marker_with_no_handler_installed_is_inert` is the embedder that wants none.
`repl::a_stop_refuses_what_would_execute_and_serves_what_reads` and
`a_stops_help_lists_continue_and_not_restart` are decision 6;
`the_stop_loop_ends_the_three_ways_it_can` pins that EOF resumes rather than
detaching, because a pipe that ran out is not a user who asked to be left alone.
`tui::c_resumes_a_stop_and_q_detaches_it` is `Control`'s third arm at the key that
produces it, and
`a_stop_marks_the_marker_and_a_caller_keeps_the_inference` is decision 8.
`run::a_breakpoint_off_a_terminal_prints_and_keeps_running`,
`debug_never_makes_a_breakpoint_inert`,
`the_stop_prompt_continues_from_one_pass_to_the_next` and
`quitting_a_stop_detaches_instead_of_killing_the_program` are decision 7's four
rows through the real binary.
`docs/book/examples/debugger-c/` is the whole of it as two running programs, one
per surface.
