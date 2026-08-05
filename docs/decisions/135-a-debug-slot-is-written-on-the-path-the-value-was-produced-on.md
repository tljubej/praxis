# ADR-135: A debug slot is written on the path the value was produced on

**Date:** 2026-08-05
**Status:** accepted
**Milestone:** 12

## Context

[ADR-104](./104-the-debugger-view-is-written-once-per-value.md) replaced the
per-safepoint debug spill with one store per definition, and argued — correctly —
that the two produce identical slot contents everywhere a snapshot can be taken.
[ADR-120 part 2](./120-a-box-with-one-reader-in-its-own-block-is-not-a-box.md)
extended it to the scalars whose boxes the forwarding pass deletes, and its own
doc comment records the one thing it had to be careful about: a definition that
can *fault* must not have its store on the raising path, because the value was
never produced. ADR-117's folding puts checked arithmetic's store past its
branch, so that case is right.

Every **other** faulting instruction is a call into a wrapper that sets
`pending_fault` and returns, and `Inst::CheckFault` right behind it is the only
way generated code can learn it happened. The wrapper has to return *something*:
`praxis_vec_get` returns the Unit sentinel. So `def_var` stored the sentinel, and
the debug store immediately behind it recorded it:

```text
  temps:
    <tmp#9: Int>  @ "start + 2"          = 3
    <tmp#10: Int> @ "values[start + 2]"  = Unit
    <tmp#11: Int> @ "values[start] + … " = <uninit>
```

`tmp#10` is the destination of the subscript that faulted. It was never written,
exactly like `tmp#11` — and it printed `Unit` while `tmp#11` printed `<uninit>`.
A reader debugging this program has to already know that `Unit` here means
"nothing happened", which is the one thing the debugger exists to make obvious.

## Decision

**A debug slot is written on the path the value was produced on.**

An unfused faultable instruction and the `Inst::CheckFault` that observes it are
one `Step` in the backend's block loop. Each is still lowered as itself — the
pair is not fused, and the check still emits its two loads and its branch — but
the debugger's store comes after the *step*, which is after the branch, which
puts it in the check's fall-through block.

This is exactly the argument ADR-104 already makes for the fused arithmetic case,
applied at the shape ADR-117 could not fold. It needs no new information:
ADR-088 decision 1 already requires the check to sit in the same block at the
next index, so "the instruction and its check" is locally decidable from two
adjacent instructions, which is what `steps()` was already reading.

On the fault path the slot keeps what it held before, which for a fresh temp is
the `None` the frame's claim starts with — and `None` is what the renderer spells
`<uninit>`.

## Consequences

**What is bought.** The destination of the instruction that faulted reads
`<uninit>`, like every other value that was never produced. The debugger no
longer has one temp per fault that lies in a way only an implementer can decode.

**What it costs.** Nothing in generated code: the same stores are emitted, in the
same order, at the same instruction — one basic block later on exactly one path.
A local defined *earlier* and redefined by a faulting instruction now shows the
earlier value rather than the sentinel, which is the more honest of the two.

**The gate.** `run.rs::the_destination_of_a_faulting_instruction_is_uninit`, over
`fixtures/run/faulting_subscript.px`. It asserts the faulting temp *and* its
seven neighbours, because this is a change to where a store is emitted and a
version that moved it off every path would turn the whole frame `<uninit>` and
still satisfy an assertion about one line. `steps()`'s own unit tests pin the new
grouping.
