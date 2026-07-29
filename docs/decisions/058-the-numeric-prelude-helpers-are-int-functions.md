# ADR-058: The numeric prelude helpers are monomorphic `Int` functions, and their arity is their wrapper's

**Date:** 2026-07-30
**Status:** Accepted — implemented
**Milestone:** Repair (stage S17 — TY-33, unit 2 of 4)

## Context

§16.1's second prelude line is `abs sign min max clamp gcd lcm`. All seven were
strings and nothing else: the name resolved (it is in `PRELUDE`), inference gave
it a **fresh type variable**, and the call then lowered as
`CallTarget::User("abs")` — a direct call to a function nobody defined. That is
TY-33, and it had two symptoms at once.

A fresh variable unifies with anything, so nothing was checked:

```praxis
fn main() -> Text { abs("x") }   // accepted
fn main() -> Int  { min(3) }     // accepted
```

And then the program that typechecked could not run:

```text
$ praxis run distance.px
error: JIT compilation failed: Cranelift error: unresolved user function `abs`
```

ADR-056 closed the first of D5's four units (`panic`, `assert`, `dbg`) and left
this one waiting on TY-31's numeric constraint. TY-31 landed — and delivered
something else, which is why this ADR exists rather than a one-line follow-up:
`Bound::Is(ScalarType)` on catalog rows, **not** a capability bound. The reason
is in ADR-057 D6 and it applies here verbatim.

`pi` and `e` are on the same §16.1 line in `PRELUDE` but are not part of this
unit: they are nullary `Float` constants and already had schemes and dispatch.

## Decision 1: all seven are monomorphic on `Int`

| Name | Scheme |
|---|---|
| `abs` | `(Int) -> Int` |
| `sign` | `(Int) -> Int` |
| `min` | `(Int, Int) -> Int` |
| `max` | `(Int, Int) -> Int` |
| `clamp` | `(Int, Int, Int) -> Int` |
| `gcd` | `(Int, Int) -> Int` |
| `lcm` | `(Int, Int) -> Int` |

The plan says these "want TY-31's numeric constraint". They do not, and TY-31 is
why. A genuinely polymorphic `abs` would carry
`Capability::Kind(CapKind::Numeric)` on its own binder through F10's channel —
which works, and which nothing emits yet — and would then have to **pick a
lowering per instantiation**, because there is no numeric wrapper that serves
both scalar widths: `praxis_int_abs` reads an `i64` payload and
`praxis_float_abs` bit-casts the same word to an `f64`. A capability that
admitted `Float` and then lowered as `Int` is precisely the mistake TY-31 found
in the sinks, where `Vec[Float].sum()` returned `9222246136947933184` — a
float's bits added as an integer.

The `Float` cases already have an answer, and it is the one §4.12 wrote down:
they are **methods**. `(0.0 - 1.5).abs()`, `1.5.min(2.5)`, `x.sign()`,
`x.sqrt()`, `x.floor()` — all twelve of them, on a `Float` receiver, with their
own wrappers. So the free functions are the `Int` ones, and saying so costs a
program nothing it could otherwise write.

This is a floor, not a ceiling. The day a `Vec[Float]` sink or a numeric-generic
user function needs one polymorphic name, the channel is already built: emit the
`Numeric` capability on the binder, and give the lowering a per-instantiation
symbol choice. Until then a monomorphic signature is what the lowerings actually
support, and it is honest about it.

## Decision 2: a helper's arity is its wrapper's arity, stated once

`praxis_stdlib::NUMERIC_HELPERS` is seven rows of `(name, RuntimeSymbol)`. The
row does **not** carry a parameter count: `NumericHelper::arity` is
`self.symbol.arity()`, read off the F4 manifest.

That makes "a prelude name whose signature disagrees with the wrapper it calls"
unrepresentable rather than merely unlikely. `min(3)` is rejected because
`praxis_int_min` takes two operands, not because a second table said `2`. The
manifest is already the authority on the wrapper's shape (F4), and the whole
lesson of that foundation was that a second copy of the same number drifts.

Both consumers read the one table. Inference builds the scheme from
`helper.arity()`; MIR's `build.rs` takes `helper.symbol` as the call target. So
the lowering is **one path for all seven** rather than seven branches, and the
fault check after the call is driven by the manifest's own `Effect`
(`sym.faults()`) — the same shape ADR-056 gave the control names.

## Decision 3: a helper that leaves the `Int` range faults

| Wrapper | Effect | Why |
|---|---|---|
| `praxis_int_abs` | `AllocatesAndFaults` | `abs(Int::MIN)` has no positive counterpart — `praxis_int_neg`'s edge, exactly |
| `praxis_int_sign` | `Allocates` | total: every `Int` has a sign in range |
| `praxis_int_min` / `max` | `Pure` | returns **an operand**, so it allocates nothing |
| `praxis_int_clamp` | `Faults` | returns an operand; faults on an inverted range |
| `praxis_int_gcd` | `AllocatesAndFaults` | one input pair has no `Int` answer: `gcd(Int::MIN, Int::MIN)` is 2^63 |
| `praxis_int_lcm` | `AllocatesAndFaults` | overflows long before its operands do |

Faulting rather than wrapping is what §4.12 already does for `+`/`-`/`*` and
what TY-28 chose for an out-of-range literal: a number nobody wrote is worse
than a stop. `gcd` and `lcm` are computed in `i128` and range-checked on the way
out, so the only refusal is a result that genuinely has no `Int` — a naive `i64`
`gcd` would have wrapped on `Int::MIN` instead.

`min`, `max` and `clamp` hand back **the reference they were given**, not an
equal copy. An `Int` object is immutable, so sharing it is what "the smaller of
the two" means — and it is what makes those three `Pure`, so their call sites are
not safepoints. A version that allocated would pass every value test while
quietly making three of the seven helpers collect.

## Decision 4: an inverted `clamp` range is a fault, and it borrows `InvalidSize`

`clamp(v, low, high)` with `low > high` names an **empty** range. There is no
operand to return and no answer that is not invented, so it faults. Every
neighbouring language agrees that this is a caller error: Rust's `Ord::clamp`
panics, `std::clamp` in C++ is undefined behaviour, and a panic across
`extern "C"` is what §10.4 forbids — which leaves a fault.

The kind is **`FaultKind::InvalidSize`**, whose doc is widened to say so. This is
a borrowed kind, and ADR-056 D2 argued against borrowing one: "a fault kind that
had to borrow another's name would be the RT-17 mistake again." The reason it is
borrowed anyway is H17 — **S17's one ABI bump is spent**, by ADR-056 itself, and
a new `FaultKind` variant is a bump by this repo's own convention (v11 and v13
both declare one). `InvalidSize` is the closest honest fit: it already means "an
argument the runtime cannot honour", and its three existing cases are a negative
`Grid` extent, an overflowing cell count and an out-of-range `BitSet` member.

**A dedicated empty-range kind is owed to the next stage that spends a bump.**
TY-34 is its natural owner: D6 answers a descending `a..b` as *empty*, which
raises the same question about the same shape, and the two should get one kind
between them.

## Consequences

- **`RUNTIME_ABI_VERSION` stays 13.** Seven new symbols are additive — a row in
  the manifest and an arm in `praxis_runtime::abi::address`, both exhaustive
  matches — and no `#[repr(C)]` layout changed.
- **`clean.px` typechecks for a new reason.** The CLI's happy-path fixture calls
  `abs(a - b)` on two annotated `Int` parameters. It was accepted before because
  `abs` had no type; it is accepted now because it has the right one.
- **`seed_builtin_schemes` is no longer "the builtins that need a scheme".** It
  is every prelude name that has one, and a name absent from it gets a fresh
  variable — which is the bug, not the default. The six graph helpers are the
  last names in that state (D5's unit 3).
- **§3.3's representative program is closer to compiling.** `sign(...)`,
  `abs(...)` and `max(abs(dx), abs(dy))` all typecheck and run; what is left in
  that program is `0..=distance`, which is TY-34.
- **`gc_alloc` is generic over its payload and checks the width at runtime.**
  Writing `gc_alloc(ctx, &scalars::INT, 0)` — an `i32` literal — aborts the
  process with "payload size mismatch for descriptor Int" from inside
  `extern "C"`, which is a non-unwinding panic across the ABI. It cost one
  debugging cycle in this unit. That is D12's policy question, not this ADR's,
  but the shape is worth recording: the descriptor knows the payload type and the
  signature does not have to be generic.
