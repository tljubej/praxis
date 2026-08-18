# ADR-154: A program is its top-level statements, and `fn main` is an ordinary function

**Date:** 2026-08-19
**Status:** Accepted — implemented
**Milestone:** —

## Context

[ADR-067](./067-a-files-top-level-statements-are-its-program.md) made a file's
top-level statements its program and kept a declared `fn main` as a fallback for
a file with none. It said what the fallback was for, and when it could go:

> Every corpus program and every end-to-end test is written as `fn main() { … }`
> with nothing at the top level. […] The design doc never mentions a `main`; when
> the corpus is eventually written in §3.2's own style, the fallback can go.

The cost of keeping it is the thing a reader hits first. Two spellings that are
alternatives rather than layers means a file containing both runs one of them,
and *which* one is a rule you have to know:

```praxis
fn main() {
    out("main ran")
}

out("the top level ran")
```

prints one line, and a reader who expects the other has no way to tell from the
program which it will be. The rule was documented, in a chapter section and in
this ADR, and documenting it is not the same as it being legible. A language
with one entry point does not need the section.

The fallback also kept the host's *answer line* alive. A `fn main() -> Int`
returned a value and `praxis run` printed it, so a program had two ways to
report a result — `out(…)` and the return — and which one a program used was
again a property of which spelling it happened to be written in.

## Decision 1: the top level is the only entry point

`praxis_hir::entry_point` answers `<entry>` or nothing. A declared `fn main` is
an ordinary function: nothing calls it that the file did not call itself, and a
file whose whole program sits inside one has nothing to run.

ADR-067's Decisions 2, 3 and 4 are untouched — the generated function is still
named `<entry>`, still a `TypedItem::Fn` with a real symbol, and still collects
the statements out from between the declarations rather than wrapping the file.
Only the second half of its Decision 1 is superseded.

The alternative — reporting a file that has both as an error — was rejected for
the reason ADR-067 gave and that reason still holds: `fn main() { … }` followed
by `main()` is an ordinary program, and two corpus fixtures are exactly that. A
function named `main` is not special enough to be worth a diagnostic.

## Decision 2: a file with nothing to run says so, and names the fix

```
error: no statements to run
note: this file declares `fn main`, but a Praxis program is its top-level statements — call it with `main()`, or move its body to the top level
```

The `note:` is only printed when the file declares a `fn main`, because that is
the one way to get here that used to work. A file of other declarations gets the
first line alone, and being told about a `main` it does not have would be noise.

The check moved ahead of the JIT. "Nothing to run" is knowable from the module,
and compiling declarations nobody is going to call buys the report nothing.
`praxis check` is unchanged and still exits 0 on both files: having nothing to
run is not a type error, and it is discovered by the thing that wanted to run it.

## Decision 3: `praxis run` prints no answer line

The entry point is `Unit`, and now it is the only entry point, so the host's
"print the result when it is not `Unit`" path had exactly one caller left and no
way to reach it. It is gone, along with the entry point's return type, which was
read before monomorphization purely to ask that question.

A program reports what it printed. That is one rule where there were two, and it
is what the design doc's own programs (§3.2, §3.3, §4.2) already assumed.

## Consequences

- **115 `.px` programs were rewritten**, across the AoC corpus, the input-parser
  corpus, the CLI fixtures and the book's examples. The body of each `fn main`
  moved to the top level; the 48 that returned `Int` or `Float` gained an
  explicit `out(…)` around the tail expression, because the host no longer
  prints it. Every `.out` in the tree is byte-identical afterwards, which is
  what says the rewrite was a rewrite and not a change of behaviour.
- **The corpus now reads the way the design doc writes.** 28 of the 36 programs
  under `tests/aoc-corpus/` carry no type annotation at all; the commonest one
  used to be `fn main() -> Int`, which was an annotation the entry-point
  convention demanded rather than one inference needed.
- **`<entry>` is in every backtrace now**, where a program written in the `fn
  main` convention used to show `main`. Twelve `.fault` and five `.session`
  expectations in the book changed for that reason and no other.
- **The design doc's §13.3 is not updated.** It says "Top-level statements become
  generated `main`", which ADR-067 already deviated from by naming the generated
  function `<entry>`; §20 rule 1 puts deviations here rather than in the
  document, and this is the second half of the same one.
- **`fn main` still compiles, and is still tested.** Two fixtures keep it:
  `top_level_beside_fn_main.px`, where the top level calls it and it runs once,
  and `only_fn_main.px`, where nothing does and the run reports it.
