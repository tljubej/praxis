# ADR-068: A function does not capture, and saying so is `N007`

**Date:** 2026-07-30
**Status:** Accepted — implemented
**Milestone:** Repair (REP-22)

## Context

```praxis
var x = 1
fn f() { x }
out(f())          // Unit

var y = 5
fn g() { |n| n + y }
out(g()(1))       // 4388746929
```

<!-- The two bindings were written `let`. `let` was retired by ADR-125; the
     spelling is updated here because current documentation links to this
     entry, so a reader arrives at a keyword the compiler refuses. -->

Both passed `praxis check`. Resolution resolved the name to the top-level symbol
and inference typed it; MIR had no slot for it inside the function, so the read
produced whatever was there. The bare form answered `Unit`. The closure form is
worse: capture analysis captured a symbol with no slot, so the environment cell
held whatever the read found — a nine-digit number out of a program that reported
nothing.

Both forms were measured at `a1c0b76`, before ADR-067 lowered top-level
statements, and answer the same way there. ADR-067 did not cause this. What it
changed is how easily a program reaches it: the design doc's own program shape
(§3.2, §3.3, §4.2) puts bindings at the top level, so a program written the way
the doc writes them walks straight into it.

## Decision: report it — a `fn` does not capture

§4.9 describes functions. §4.10 describes closures, and only §4.10 says
"capture":

> Closures capture values automatically.

So the language already draws this line; nothing implemented it. `N007` is that
line, reported at the **use site**, at name resolution.

The two alternatives were weighed and rejected:

- **Make top-level bindings globals.** That is a language feature §3.2 does not
  ask for. It needs storage, an initialization order, a GC root, and an answer for
  what a `fn` called *before* the binding's statement runs sees. A file's
  top-level statements are its program (ADR-067), which makes them a function
  body; promoting some of its locals to globals because a `fn` happens to name
  them is a different language.
- **Make a `fn` capture.** Then `fn` and closure differ only in syntax, and every
  function acquires a hidden environment. §4.9's example is a pure function of its
  parameters and the doc's own framing is that a closure is the thing that
  captures.

The message names both ways out, because both are ordinary:

```
error[N007]: `f` cannot use `x`: a function does not capture the bindings around
it (pass `x` as a parameter, or use a closure)
```

## Decision 2: the boundary is a `fn` body, not a closure body

A closure body opens no boundary of its own, so the question is always about the
nearest enclosing `fn`. Three cases fall out, and all three are what the language
already promised:

| Program | Answer |
|---|---|
| `let offset = 10` … `v.map(\|x\| x + offset)` at top level | fine — §4.10's own example, and after ADR-067 both live in `<entry>` |
| `fn f(v) { let k = 10\n v.map(\|x\| x + k) }` | fine — the closure captures its own function's local |
| `fn g() { \|n\| n + y }` with `y` outside | `N007` — the closure is inside `g`, and `y` is not |

`Resolver::fn_boundary` is saved and restored around a `fn` body rather than set
and cleared: a nested `fn` is already `N005`, but it is still *resolved*, and
clearing the boundary on the way out would silence every reference in the rest of
the enclosing body.

## Decision 3: it is the symbol's **kind** that decides

Only a `let`, a `var` or a parameter crosses badly — those are locals of the
function that declared them. Everything else a `fn` body can name is reachable
from anywhere by construction: another `fn`, a `struct`, an `enum`, a variant
constructor, and the prelude's builtins. So the check asks the kind, not only the
scope, and `fn f() -> Int { helper() }` is untouched.

A binding declared *after* the function is `N001` and not this: only `fn`,
`struct` and `enum` are pre-registered for forward reference, so the name is
genuinely not in scope and there is no binding to have crossed anything.

## Decision 4: report, and record the reference anyway

The resolved reference is still recorded, so inference types the body as written
and adds nothing. That is `N004`'s no-cascade rule and REP-14's: one report per
use site is the whole answer, and a second report from inference about a type it
could not derive would name a consequence rather than the mistake.

## Consequences

- **`N007` is spent. `N008` is the next free code in ADR-051's `N0xx` block**,
  and `Y019` is still free — declaration and name mistakes go in `N0xx` (ADR-051),
  and this is one: the name resolves, and what is wrong is *where* it was declared
  relative to what reads it, exactly as `N005`'s is.
- **`DiagCode::ALL` is 64 long.** Its count assertion is updated with the variant,
  which is what that test is for.
- **One existing assertion moved.** `a_nested_type_declaration_is_reported_at_the_declaration`
  asserted that `let base = 1` / `struct Point {…}` / `fn main() -> Int { base }`
  is clean — it is about "top level is the file, not the first statement", and the
  `fn main` reading `base` was incidental. A top-level statement reads the binding
  now, so the test still says what it meant to.
- **The two forms REP-22 registered are now compile errors**, so neither the
  `Unit` nor the nine-digit answer is reachable from a program that passes
  `praxis check`. This closes the register's last P0.
