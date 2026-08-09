# ADR-151: A variable a scheme quantified is not free, and a frame that is not the innermost is in a call

**Date:** 2026-08-09
**Status:** accepted
**Milestone:** 12

## Context

One program found three separate things saying something untrue about it:

```
var c = || {out("foo"):bp}

fn foo(c) { c() }
fn zoo(c) { c() }

fn run(c) {
  var i = 0
  loop {
    if i%2 == 0 { foo(c) } else { zoo(c) }
    i += 1
  }
}

run(c)
```

- The editor rendered `foo`'s parameter `c` as `() -> ?T`. `?` is the spelling
  [pretty.rs](../../crates/praxis-types/src/pretty.rs) reserves for a variable
  *nothing binds* — "which only appear during a failed inference" — so a
  correctly inferred generic function read as one the checker had given up on,
  and a program that then ran anyway read as one that should not have.
- Stopping at the marker and continuing, the backtrace put `run` on line 13
  (`loop {`) or line 19 (`i += 1`), never on the `foo(c)` or `zoo(c)` it was
  actually inside.
- The stopped closure's source pane said "no source span recorded for this
  frame" about a line that is right there in the file.

The three are unrelated in mechanism and identical in shape: a surface reporting
something the compiler *knows* as something it does not.

## Decision

### 1. A binding inside a generic body is rendered inside the binders that quantified it

`foo` is `forall T. (() -> T) -> T` and its parameter is `() -> ?a` — a monotype
over the very variable `foo`'s scheme quantifies. Generalization owns its binders
and rewrites no arena slot ([ADR-008](./008-let-generalization-levels.md)'s F10),
so `?a` stays exactly what it was, and nothing on the parameter's own scheme can
say what became of it.

`Symbol::enclosing_binders` says. `Inferer::name_enclosing_binders` runs once
after the file is inferred and records, on every binding whose monotype mentions
a variable some scheme quantified, that scheme's binder list;
`TypeDb::render_with_binders` then names it the way its owner does. Hover, hover
over a declaration, inlay hints, completion and signature help all render through
`Symbol::rendered_type`, so `?` means one thing on all five surfaces: **no scheme
in this program binds this variable.**

**Display only, and the field is not folded into the scheme.** A parameter of a
generic function is a *monotype* in that function's body — `c` cannot be used at
two types inside `foo` — so quantifying it on the parameter's own scheme would
make `infer_assign`'s `instantiate` hand `c = |…| …` a fresh variable instead of
constraining the parameter, and `var f = |x| x` followed by `f = |n| n + 1` is
the unsoundness §5.3 already spends the value restriction on.

**The search is over schemes, not over syntax.** Instantiation mints fresh
variables per use site, so a variable is normally exactly one scheme's; a
mutually-recursive declaration group is the exception, since its members
generalize at one site and can each list a shared variable. The first match wins
there and the tie costs nothing — what is read off the list is the *name* a
binder gets, and two schemes that both quantify one variable both name it.
Walking the syntax instead would need the enclosing item of every declaration,
which nothing else in the pass wants.

**The inlay hint's *edit* is gated on the binder list and not on the spelling.**
`is_spellable` refuses a rendering containing `?`, and naming the variable
properly is precisely what removes the `?` — so `T`, which is not a type name the
parser knows, would otherwise arrive with an edit that writes an annotation the
program cannot compile.

### 2. A caller's line is the call whose callee is the frame above it

`fault_span` picks the narrowest temp that carries a span and never received a
value, which is the right inference for the frame a *fault* is in: the expression
that started and did not finish.

It cannot answer for a caller in a loop, and the reason is structural rather than
a matter of tuning. Debug slots are written once per definition and never cleared
([ADR-104](./104-the-debugger-view-is-written-once-per-value.md)) —
"a value that has been produced stays renderable" — so on the second pass `run`'s
`foo(c)` temp holds the *first* pass's `Unit` while the temps for the statements
this pass has not reached hold nothing. "Started and did not finish" and "has not
started yet" become the same observation. On pass one the narrowest such temp is
the `1` of `i += 1`; from pass two on it is the `loop` itself. Neither is a call.

So the answer is not inferred either. `Function::debug_callees` records, per
local, the function a **direct** call defines it from, and `call_span` joins that
against the name of the frame above. `marked_span`'s three answers are now
ordered by how well each is *known*: the marker's own span for frame 0, the
recorded call for a caller, the inference for what is left.

**It is static metadata, so a call costs nothing.** The alternative — a caller
storing its current call site into its own `DebugFrameEntry` before each call —
would put a store on every call in every program to serve the debugger, which is
the cost [ADR-150](./150-a-marker-is-a-place-in-the-program-and-a-stop-is-not-a-fault.md)
decision 5 exists to avoid. `DebugLocalMeta` is built by the backend and read by
the runtime, never by generated code, so a field on it moves no offset generated
code knows and `RUNTIME_ABI_VERSION` does not move either.

**The callee joins `DebugMetaKey`**, beside `slot_kind` and for that field's
reason rather than the span's: the generation interns metadata by content, and a
`__fnvalue_*` adapter's return slot has an empty name, a positional symbol id and
no span, so two adapters over functions of one type are identical in every field
but this. Leaving it out of the key would intern them together and put one
adapter's callee in the other's frame.

**An indirect call records nothing**, and the fallback is what covers it: a
closure's target is a value, so there is no name at compile time to match. In the
program above that is `foo`'s own frame, calling `c()` — and the inference is
right there, because `c()` is the only expression in `foo` that started.

**Two calls to one callee in one frame prefer the unfinished one.** `f(); f()`
stopped in the second has the first one's value in hand. Where that does not
separate them either — a loop, again — the narrowest span wins, so the answer is
at least the same from stop to stop.

**First writer wins on a local two calls define.** An `if`'s arms write their
value into the join's slot, and there is no callee that is right on both paths.
The debugger's fallback covers such a frame; a slot naming a call the program is
not in would be worse than one naming none.

### 3. A closure's frame span is its literal's

`lower_closure_fn` passed `(0, 0)` and called a closure "synthetic". Only its
*name* is. Its body is source the programmer wrote, and `TypedExpr::Closure`
already carries the literal's span, so the frame gets it and the debugger renders
the lines.

`(0, 0)` keeps its meaning at the two functions that really have no source: the
`__fnvalue_*` adapter, whose body is a forwarding call nobody wrote
([ADR-061](./061-a-fn-name-in-value-position-is-a-closure.md)),
and the debugger's own `__p_expr`.

## Consequences

**A generic function now looks generic in the editor instead of broken.** That is
the whole of decision 1's user-visible effect: the types were always right, and
monomorphization always pinned them. What changes is that `?T` is once again
evidence of something worth looking at.

**A `praxis check` diagnostic is unaffected.** Diagnostics render a `Type`
through `db.render`, not a binding through `Symbol`, and a mismatch reported
against a variable an enclosing scheme quantified is still reported the same way.

**A caller's line can still be wrong, and now only where the program is
ambiguous**: two calls to one callee from one frame, both already evaluated once.
The rule states which one it picks rather than leaving it to slot order.

**The gates.**
`hover_tests::a_generic_fns_parameter_is_named_by_the_scheme_that_bound_it` is
decision 1 at both hover doors, with the owning `fn` still showing its own
`forall`; `a_variable_no_scheme_quantifies_still_renders_unbound` is the half
that did not move, and `m12::a_hint_offers_an_edit_only_where_the_annotation_would_compile`
is the edit gate that `T` must not pass.
`build::a_direct_calls_local_records_the_function_it_targets` is decision 2's
metadata and its absence on an indirect call;
`generation::two_locals_that_differ_only_in_their_callee_are_not_one_local` is
the interning key, at the shape that would otherwise share one;
`tui::a_callers_span_is_the_call_to_the_frame_above_it` is the join, with the two
decoys the inference would have taken;
`a_calls_span_survives_the_value_an_earlier_pass_left_in_it` is the loop, which
is the case that made this a decision;
`the_unfinished_call_wins_when_two_name_the_same_callee` is the tie-break; and
`a_caller_is_marked_at_its_call_and_frame_zero_at_its_marker` is the ordering
end to end.
`jit::a_callers_frame_records_the_call_that_led_to_the_frame_above` is decisions
2 and 3 through real compiled code, on the program at the top of this document:
four passes, alternating callees, and the closure frame's extent.
`build::a_lifted_closure_carries_its_literals_span` is decision 3 with the
adapter beside it, which must keep the empty span.
