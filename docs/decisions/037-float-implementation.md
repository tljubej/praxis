# ADR-037: Float scalar implementation

**Date:** 2026-07-28
**Status:** Accepted
**Milestone:** Post-M10 (Float, §4.12)

## Context

§4.3 reserves `Float` as an IEEE-754 binary64 payload, and §4.12 specifies
integer numeric behavior in detail but was silent on Float. `ScalarType::Float`
existed as a reserved variant throughout the compiler, but every layer treated
it as "not yet wired": writing `Float` in source yielded `N002 unknown type`,
there was no float literal syntax, no arithmetic, and no runtime descriptor.

This ADR records the three decisions that had to be made to bring `Float` to
full parity with `Int`.

## Decision 1: f64 rides the uniform i64 codegen channel via bit-cast

The Cranelift backend carries every value — `GcRef`s and all scalar payloads —
as `i64` (`const GC = types::I64`). This is normative for the uniform-object
representation (§4.3) and keeps the calling convention uniform. An `f64` is 64
bits but a *different* Cranelift type (`types::F64`), so it cannot naively ride
the `i64` channel the way `u32` (Char) or `u8` (Bool) do.

**Decision:** transport floats as their IEEE-754 bit pattern (`f64::to_bits()`
as `i64`) through the same channel, and bit-cast to/from `f64` only at the
points that need real float semantics:

- Runtime ABI wrappers (`praxis_alloc_float`/`praxis_float_load`) take/return
  `i64` bit patterns, keeping the `extern "C"` signature uniform with `Int`.
- `Inst::ConstFloat { bits: i64 }` carries the bit pattern (emitted as a plain
  `iconst` — no f64 materialization needed for a constant).
- `Inst::FloatBinOp`/`Inst::FloatCmp` bit-cast operands to `f64`, apply the
  native Cranelift op (`fadd`/`fsub`/`fmul`/`fdiv`/`fcmp`), then bit-cast back.

Cranelift's `bitcast` instruction requires flags that are *exactly*
`MemFlags::new()` or `new().with_endianness(Little)` — any extra bits (notrap,
aligned) are rejected by the verifier. We use the little-endian form, matching
every supported Praxis host.

**Rejected alternative:** a parallel `types::F64` representation for transient
float scalars. This would split the codegen into two type systems and complicate
every scalar-handling site for one type's benefit. The bit-cast approach keeps
the uniform model intact.

## Decision 2: Strict per-literal typing (no implicit widening)

Numeric literals carry their type from their syntax: `1` is `Int`, `1.5` is
`Float`. Arithmetic result type follows the operands:

- If either operand is a float literal *or* has an inferred `Float` type (e.g. a
  method-call result like `16.0.sqrt()`), the result is `Float`.
- Otherwise it is `Int`.
- Mixing a `Float` operand with an `Int`-typed expression fails unification → a
  clean type error. There is **no implicit widening**.

Cross-type conversion is explicit only: `Int.to_float()` and `Float.to_int()`.
This keeps the static type discipline strong (an `Int` never silently becomes a
`Float`) while still allowing `1.5 + 2.5 * 2.0` and `x.sqrt() + y` to infer
naturally.

**Rejected alternatives:**
- *Polymorphic numeric literals* (a bare `3` unifies with Int or Float): more
  ergonomic, but needs new unification machinery and blurs the Int/Float
  distinction.
- *Mixed-mode widening* (Int auto-widens to Float on contact, like Python/JS):
  most convenient, but weakens type safety and spreads coercions through
  inference and codegen.

## Decision 3: `Float.to_int()` faults on NaN / ±inf / out-of-range

§4.12 establishes the convention that bad arithmetic faults into the crash
debugger rather than producing a garbage value (integer overflow faults; integer
division by zero faults). Float arithmetic itself never faults — IEEE-754
defines `1.0/0.0 = +inf`, `0.0/0.0 = NaN`, etc., and these are legitimate
values.

But `Float.to_int()` is a *narrowing* conversion: NaN, ±infinity, and finite
values outside the signed 64-bit range have no exact `Int` representation.
Rust's `as i64` would saturate these silently (`inf → i64::MAX`, `NaN → 0`),
producing plausible-but-wrong values. Per the §4.12 convention, these cases
fault instead with a new `FaultKind::FloatToInt`. Only finite, in-range values
convert (truncating toward zero).

## Consequences

- `Float` is now a first-class scalar: literals, arithmetic, comparison (with
  IEEE-754 NaN semantics via Cranelift `fcmp`), boxing, `out()` formatting,
  and a stdlib method set (`abs`/`sqrt`/`floor`/`ceil`/`round`/`sign`/
  `to_int`/`to_text`/`is_nan`/`is_infinite`/`min`/`max`), plus `pi()`/`e()`.
- The `i64`-channel bit-cast is the pattern any future IEEE-754-width type
  would reuse.
- The `read float` input-parser atom (§7.4) remains deferred — it needs a
  separate runtime parse path and is not part of this workstream.
- Method calls on numeric literals (`16.0.sqrt()`) are now supported (the
  parser's `.`-postfix loop was generalized from identifiers to any primary);
  this also fixed the pre-existing gap that `5.to_float()` didn't parse.
