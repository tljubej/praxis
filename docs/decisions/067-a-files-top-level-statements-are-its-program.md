# ADR-067: A file's top-level statements are its program, and `fn main` is the fallback

**Date:** 2026-07-30
**Status:** Accepted — implemented
**Milestone:** Repair (REP-19)

## Context

```praxis
out(1)
let x = 2
out(x)
```

passed `praxis check` and printed **nothing**, then exited 1 with "no `main`
function to run".

`TypedModule.items` held only `fn` declarations. `lower` walked the file's root
looking for `fn`/`struct`/`enum` and dropped everything else, under a comment
saying M4 only JITs `fn` items. `lower_module` emitted only those, and the host
called `main`.

§3.2 has required otherwise since the design doc was written:

> A single file is the normal program unit. Top-level statements are wrapped in a
> generated entry function.

So this is not a decision the design doc left open — it is a sentence nothing
implemented. And it silenced the design doc's own programs: §3.2's example,
§4.2's bindings, and **§3.3's complete representative program**, all of which are
written entirely at top level with no `fn main` anywhere. The corpus copy of §3.3
had to wrap its body in a `fn main()` and say so in a comment.

The one thing §3.2 does not decide is what happens when the file *also* declares
a `fn main`, and that is this ADR's decision.

## Decision 1: the top level is the program, and `fn main` is the fallback

> **Superseded by [ADR-154](./154-a-program-is-its-top-level-statements-and-nothing-else.md).**
> The fallback is gone: the top level is the *only* entry point, and a declared
> `fn main` is an ordinary function. The corpus this decision was protecting has
> since been written in §3.2's own style, which is the condition the last
> paragraph of this section names. Decisions 2, 3 and 4 below still hold.

A file with at least one top-level statement gets a generated entry function
holding them all, in source order, and that is what the host runs. A file with
**no** top-level statements falls back to a declared `fn main`. A file with
neither has no entry point, and the error names both spellings.

The alternative — making `fn main` special forever, or making it an error to have
both — costs more than it buys:

- Every corpus program and every end-to-end test is written as `fn main() { … }`
  with nothing at the top level. Making the top level the *only* entry point
  silently turns twelve corpus programs into no-ops, which is REP-19 in mirror
  image.
- Reporting a file that has both would reject `fn main() { … }` followed by
  `main()`, which is a perfectly ordinary program and is what the two newest
  corpus fixtures are.
- Running the top level *and then* calling `main` would run `main` twice for that
  same program.

So the two spellings are alternatives rather than layers, and the fallback is
written down as a compatibility rule rather than as a second entry-point concept.
The design doc never mentions a `main`; when the corpus is eventually written in
§3.2's own style, the fallback can go.

`praxis_hir::entry_point` is the one place the rule lives. Both hosts that
execute a module ask it — the CLI's `run` and the debugger's `reload` — because
two copies of a two-case rule is how they drift.

## Decision 2: the generated function's name is not an identifier

It is `<entry>`. The parser cannot produce that name, so no program can declare a
second function with it, and no program can call it. That is ADR-064's rule for
the subscript rows (`[]`, `[]=`) applied to the one other name the compiler mints
into the same namespace, and a gate asserts the property rather than trusting it.

The crash debugger renders the name, so a fault in a top-level statement shows a
frame the user can recognize as not theirs:

```
#0   <entry>
  locals:
    v: Vec[?T] = []
```

## Decision 3: the entry point is a `TypedItem::Fn`, with a real symbol

It is a nullary `Unit` function, and it goes through monomorphization, MIR and
the backend as an ordinary item — which is the point: nothing downstream needed a
second concept, and a top-level call to a generic function specializes exactly as
a call from a `fn` body does.

Its `SymbolId` is minted into the `NameTable` rather than faked, because a
`SymbolId` is "an opaque, interned identifier for one declaration" and this *is*
a declaration — one the compiler wrote instead of the file. It carries no `decl`
span for the same reason a builtin does not: there is no source site.

Being a `Unit` function is also what keeps `out(…)` at top level from printing
twice. The host prints an entry point's *answer* only when it is non-`Unit`, and
a file has no value: every top-level statement runs for effect and the tail is
Unit. `out(overlaps(segments, false))` is a statement, not a result.

> **Amended by [ADR-154](./154-a-program-is-its-top-level-statements-and-nothing-else.md).**
> There is no answer line at all now. With the `fn main` fallback gone the entry
> point is always `Unit`, so the host prints nothing of its own and a program
> reports what it printed.

## Decision 4: the statements move, the declarations stay

The entry point is not a source transformation that wraps the file. A `fn` inside
a `fn` is `N005`, so the top-level `fn` items have to stay where they are; the
statements are collected out from between them and the declarations are lowered
as their own items, exactly as before.

## Consequences

- **§3.3's representative program runs verbatim from the design doc.** The corpus
  copy is now the design doc's own text — top-level `let`, top-level `out`s and
  all — which is what S25's acceptance criterion was still short of.
- **Nothing in the tree had to be rewritten.** The suite went from green to green:
  every existing program has its statements inside a `fn main` and takes the
  fallback, and the two that have both call `main()` themselves.
- **A `fn` body that reads a top-level binding is still `Unit`**, silently. It was
  `Unit` before this landed too — the binding had no lowering at all — but the
  design doc's program shape makes top-level bindings the norm, so the defect is
  reachable in a way it was not. Registered as **REP-22**; it is a language
  question (does a `fn` capture? §4.9/§4.10 say closures do and functions do not)
  and the narrow defect that survives either answer is the silence.
- **`praxis check` is unchanged.** Top-level statements were always analyzed; this
  is about what runs, not about what is checked.
