# ADR-139: A pattern's name is a name in the frame, and a slot no pattern names is a temp

**Date:** 2026-08-06
**Status:** accepted
**Milestone:** 12

## Context

[ADR-125](./125-a-binding-is-a-binding-and-the-compiler-decides-its-storage.md)
says a binding is a binding: "a parameter, a `for` variable and a name a pattern
introduces" are bindings in exactly the sense a `var` is, and the compiler — not
the syntax that introduced them — decides their storage. The crash debugger
disagreed. Handover 31 §9 caught it on an Advent of Code solve:

```text
  locals:
    xs: Vec[Int] = [1, 2, 3]
    total: Int = 6
    ? = 3
```

`? = 3` is `item`, from `for item in xs`. The rule the snapshot actually
implemented was "a binding with a declaration statement keeps its name", which is
the one distinction ADR-125 says does not exist. A destructuring `for` was worse
— three anonymous rows for one loop, the scrutinee and both components — and a
`match` arm's payload did not appear among the bindings at all.

The payload's absence turned out to be the same defect wearing a different
surface. It was not missing; it was in the `temps:` section as `<tmp#7> = 77`,
below the elision in the handover's own repro. A pattern binding either **owns**
a freshly allocated slot, which lowering allocated with no name, or **aliases**
a slot lowering had already classified `Temp`. Which one it gets is decided by
whether the binding is reassigned or captured, not by `for` versus `match`. So:
one mechanism, two surfaces.

The mechanism is upstream of MIR. `TypedPattern::Bind` carried `(symbol, ty)` and
nothing else, so the name the parser had in its hand stopped at the AST and every
consumer downstream was working from a symbol id it could not print. There was no
slot→symbol map that lowering had forgotten to populate; there was no name to
populate it with.

The display was not the whole cost. `praxis-debugger`'s `collect_bindings` admits
a frame local as a `p EXPR` parameter when it is user-classified and has an
identifier name, so an unnamed or temp-classified local can never be one:
`p item`, `p a` and `p payload` all answered "`item` is not defined" for names in
plain sight, and the book documented both halves as known rough edges.

## Decision

**A name a pattern introduces is carried on the pattern and reaches the frame as
a binding; a slot no pattern names is a temp.**

Four parts.

**1. The name and its span live on the pattern.** `TypedPattern::Bind` carries
`name: String` and `span: (u32, u32)` — the same pair `TypedStmt::Var` carries,
for the same consumers. Both construction sites are in `praxis-hir`'s
`pattern.rs`, which is the whole of the HIR change, because
[ADR-130](./130-a-matchs-coverage-is-analysis-answer-and-the-pattern-is-built-once.md)
had already made the pattern get built exactly once. The span is the *name*
token's, not the enclosing pattern's, so `Some(p)` points at `p`.

**2. A binding that owns a slot names it.** The four MIR sites that allocate
storage for a pattern binding — the `for` variable, the reassigned `match`
binding, a destructuring component, and a captured one — pass the name, the
`LocalDebugKind::User` classification, the pattern's type and its span, which is
what the `TypedStmt::Var` site has always passed.

**3. A binding that aliases another binding's slot does not rename it.** A
pattern name that is only read gets no slot: it aliases the one the value is
already in. `Function::adopt_binding_name` retitles that slot when it is an
unnamed temp — the payload a `match` arm just extracted, the argument a
destructuring closure parameter received — and is a **no-op** when the slot
already belongs to a binding. That second half is load-bearing: in
`match v { n => … }` the scrutinee's local *is* `v`'s, and relabelling it would
erase a binding the programmer did write in order to show one they wrote too.

**4. A container the programmer did not name is a compiler temp.** A
destructuring `for`'s item slot and a destructuring closure parameter's
whole-argument slot hold values with no names: `for (a, b) in pairs` names `a`
and `b`, and nothing names the pair. Both are now `Temp`, carrying the iterated
expression's or the function's span, so they read `<tmp#4: (Int, Int)> @ "pairs"`
— which explains itself — instead of appearing in `locals:` as a binding the
source never wrote. The closure case previously took the pattern's *source text*
as a name, so a frame listed `(a, b): (Int, Int) = (1, 2)` beside its own `a` and
`b`; `TypedParam::name` is now an `Option` and there is no pattern text to
mistake for a name.

The same reasoning settles a `VarCell`. A captured-and-written binding's cell is
not the binding — it holds a `VarCell` object, not the bound value — so it is a
temp carrying the binding's span, where it used to be a `User` local named
`__cell_n` whose value read `<var-cell>`: an internal name, no type, and the
wrong value. Naming it plainly `n` would have been worse, because `p n` would
then bind the cell instead of what is in it. Showing a captured-and-written
binding *by value* needs the renderer to dereference the cell, which is a
separate change and is recorded as such.

## Consequences

**What is bought.** Every binding form ADR-125 lists renders `name: Type = value`
in a crash snapshot. `p item`, `p a` and `p payload` work, for free and with no
change in `praxis-debugger`: `collect_bindings` was already written against the
right contract and was starved of locals that satisfied it. A destructuring
`for`'s three anonymous rows become two named bindings and one self-describing
temp.

**What it costs.** More lines in a dump, and more honest ones: a destructuring
`for` shows five rows where it showed three. The noninteractive banner's cap of
twelve may elide more; the `locals` REPL command is uncapped, so nothing is lost.
No local is added or removed anywhere, only its metadata rewritten, which is why
no `<tmp#N>` tag in any existing program renumbers —
[ADR-128](./128-a-shadow-slot-is-a-live-range-not-a-name.md)'s `nameless` census
shrinks, and its measurement table becomes historical rather than wrong.

**The gate is a rule, not a test.** `VerifyError::UserLocalHasNoName` refuses any
function with a `Gc` slot classified as a binding and carrying no name — which is
exactly the state that renders `? = value`. A nameless slot is fine; a nameless
slot *claiming to be a binding* is the illegal state, and the next binding form
cannot regress to `?` in silence. On top of it,
`run.rs::every_pattern_binding_prints_its_name_and_type` asserts all six forms in
one frame **and** that the frame contains no `? = ` row at all, because a fix that
named five of the six would satisfy every positive assertion; and
`docs/book/examples/debugger-a/pattern-bindings.px` is the book's own copy, run by
`verify.sh`, which is what keeps the chapter's claim and the compiler's output
from drifting apart.

**Related.** [ADR-021](./021-debug-frame-metadata.md) §4.2's `(source_name,
symbol_id)` pair is what this finally supplies a source name for.
[ADR-104](./104-the-debugger-view-is-written-once-per-value.md) writes the slot;
this decides what the slot is called.
[ADR-135](./135-a-debug-slot-is-written-on-the-path-the-value-was-produced-on.md)
decides when it is written.
