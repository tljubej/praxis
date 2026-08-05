# ADR-061: A `fn` name in value position is a closure over an adapter

**Date:** 2026-07-30
**Status:** Accepted — implemented
**Milestone:** Repair (stage S24 — REP-01)

## Context

```praxis
fn double(n: Int) -> Int { n * 2 }
fn main() -> Int {
    var f = double
    f(3)
}
```

<!-- The example was written `let f = double`. `let` was retired by ADR-125;
     the spelling is updated here because current documentation links to this
     entry, so a reader arrives at a keyword the compiler refuses. The prose
     below still quotes the original line where it is describing history. -->

`praxis check` accepted this and `praxis run` **aborted the host with a SIGBUS**.
Three things were each individually reasonable:

* inference accepted it, because a `fn`'s type *is* a `Func` — the symbol's
  scheme is `(Int) -> Int` and a `let` may hold one;
* `lower_path` produced a `TypedExpr::Path` naming the `fn`'s symbol, as it does
  for every name;
* the MIR builder looked that symbol up in the current frame's locals, found
  nothing — a top-level `fn` is not a binding — and fell through to its
  "unresolved: allocate a Unit placeholder so downstream lowering is sound"
  arm.

Then `f(3)` took the *existing and correct* indirect-call path — the callee
resolves to a local, so read its closure `fn_ptr` and `call_indirect` through it
— and read a `Unit` object's payload as a function pointer.

This is REP-01, found while landing the graph helpers (ADR-060) and **the last
P0 in the repair**. It is pre-existing and has nothing to do with the helpers;
they are only a new way to reach it, and their descriptor check turns it into a
`TypeMismatch` fault rather than a crash — containment for one caller, not a fix.

D15 asks what a bare `fn` name in value position should *mean*, and offers two
answers.

## Decision: a closure value

`let f = double` allocates a closure over `double` with an empty environment.
The alternative — reject a bare `fn` name outside a callee position and require
`|n| double(n)` — is cheaper (an inference check) but makes the type system
claim something the language will not do, and taxes every closure-taking API
with a wrapper lambda. A `fn`'s type already is a `Func`; this makes the value
match the type.

The invariant the stage had to establish is the same under either answer and is
now established: **no program that passes `praxis check` may abort the host.**

## The adapter, which the plan's sketch did not have

The plan expected this to reuse `praxis_alloc_closure` with an empty environment
and no other change, on the grounds that "a top-level `fn` captures nothing, so
the existing `fn(ctx, closure_self, args…)` convention already fits". The
environment is indeed empty. **The convention does not fit**: a closure's
synthetic function takes the closure itself as a hidden first explicit argument
and a top-level `fn` does not, so handing `double`'s own address to
`praxis_alloc_closure` would have every argument land one slot to the left — the
closure ref in `n`, and the last argument dropped. Silently wrong, which is worse
than the crash it replaced.

So each `fn` used as a value gets one **adapter**: a synthetic MIR function
`__fnvalue_double` whose params are `[closure_self, p0 … pn]` and whose body is
one direct call to `double` with `[p0 … pn]`, dropping the self slot. It is
emitted once per function however many times the function is used as a value,
alongside the lifted closures in `lower_module`, and named like the other
synthetic functions (`__closure_0`, `__p_expr`) so it cannot be mistaken for a
user function in a backtrace. Nothing else about the closure path changes: the
allocation, the `fn_ptr` read, the `call_indirect`, the rooting and the fault
check are the existing ones.

The arity comes from the `Func` type at the use site, which inference has already
checked against the declaration, so the adapter cannot disagree with either side
about how many arguments there are.

The adapter checks for a fault after its one call. It is on the fault path's way
out, and a walk that carried on over a Unit sentinel is the failure mode ADR-060
records at the other closure boundary.

## `TypedExpr::FnValue`, not a flag on `Path`

A `fn` in value position is its own typed-tree node. The distinguishing fact is
the symbol's **kind**, not its type: a `let` holding a closure is a `Path` and
has a `Func` type too, so a scheme cannot tell a declaration from a binding that
holds one. That is the same reason `SymbolKind::EnumVariant` exists (HIR-03), and
`let A = Empty` is the same shape of counterexample.

The node reaches lowering only in value position: `lower_call` resolves a named
callee itself and never comes through the path lowering, which is why a direct
call still lowers to a direct call and does not allocate a closure first.
`a_direct_call_does_not_go_through_a_function_value` is the gate on that, because
the alternative would have been a silent cost on every call in every program.

## A generic `fn` has no function value: `Y018`

Monomorphization is driven by call sites (MONO-01/MONO-02), and a *value* has
none. So `let f = id` for a generic `id` has nothing to specialize: the adapter
would call a clone-source the mono pass drops, and the JIT would fail with
"unresolved user function `id`" — a Cranelift error out of a program `praxis
check` accepted, which is TY-33's shape exactly.

It is reported in **inference** instead, as `Y018`, where the name is written and
where `praxis check` can see it (a lowering-time report would be clean under
`check` and only fail under `run` — the asymmetry REP-12 was about). The message
names the remedy rather than the machinery, because there is an exact one:

```text
error[Y018]: `id` is generic, so it has no single function value;
             write `|x| id(x)` to fix its type arguments at the call
```

and it works, because a closure body *is* a call site. Giving a generic function
a real function value needs monomorphization from a use-site substitution witness
rather than from a call's argument types — which the progress doc already records
as unlanded (S15's "mono still keys its cache on the call site's types") — so
this is a diagnostic now and a possible feature later, not a wrong answer either
way.

`Y018` is the next free code in ADR-051's `Y0xx` block, which was contiguous
through `Y017`.

## Consequences

* `let f = double`, `apply(double, 20)`, `bfs_distance(0, step, at_goal)` and
  `fs.push(double)` all work — through a `let`, a parameter of declared function
  type, a graph helper's closure argument (where `praxis-runtime` calls *back*
  into generated code), and a collection element called postfix.
* One synthetic function per adapted `fn`, in the same list as the lifted
  closures. A program that uses no `fn` as a value emits none.
* A `fn` value costs an allocation per evaluation of the name, exactly as a
  closure literal does. `let f = double` inside a loop allocates per iteration;
  hoisting is the program's business, as it is for `|n| double(n)`.
* The crash debugger's `p EXPR` rejects a function value, next to closure
  literals and for the same reason: there is nothing readable to show, and the
  evaluator's generation has no adapter compiled.
* `praxis-debugger`'s purity walk, `mono`'s type walk and MIR's two type walks
  each gained an arm. All four are exhaustive on purpose, so the variant could
  not be added without visiting them.
