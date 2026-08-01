# ADR-088: A faulting instruction is observed by the next one, and only a faulting instruction is

**Date:** 2026-08-01
**Status:** accepted — implemented
**Milestone:** Repair (MIR-10, REP-52, REP-53)
**Amends:** ADR-044, whose final Consequences bullet explained why this rule was absent.

## Context

§10.4 says generated code checks `pending_fault` immediately after a call that
can fault. **Nothing enforced it**, and MIR-10 is the one §4 audit row still
reading `PARTIAL — part owed`: its verifier landed and its rule did not.

Both directions had rotted, and the two registered rows are the same rule seen
from its two ends:

- **REP-52** — the fused `collect` sink pushed into its result `Vec` with no
  check at all. `praxis_vec_push` is `AllocatesAndFaults`.
- **REP-53** — every method call emitted a check whether or not its wrapper
  could fault, so neither `MethodEntry::can_fault` nor the ABI manifest's fault
  column had a reader. A column nobody reads is a column that drifts.

Handover 17 states the trap plainly: *"Adding a `check_fault` at the one
`Sink::Collect` site fixes one site and leaves the class, which is exactly what
the verifier rule exists to prevent."* So the deliverable is the rule, and the
two rows are what the rule then forces.

A read-only prototype of the rule, run over every tracked `.px`, found more than
the two registered ends — which is the argument for the rule rather than the
patches:

| | |
|---|---|
| 41 | `Alloc { AllocKind::Text }` with no check (`praxis_alloc_text` raises `InvalidText`) — one per text-literal evaluation |
| 1 | the fused `Sink::Sum`/`Product` accumulator's `IntBinOp { Checked }` |
| ~170 | checks after instructions that cannot fault — `praxis_vec_len` (`Allocates`), `praxis_map_insert`, closure and collection allocation |

The knowledge of "can this instruction fault" was split three ways and nothing
related them: the ABI manifest (`RuntimeSymbol::faults()`), the Cranelift
backend's inline `Inst`→`RuntimeSymbol` mapping, and ~34 hand-written
`check_fault()` judgements in lowering. Only two of the 34 consulted the
manifest.

## Decision

### 1. The rule, in both directions

An instruction that can fault **is immediately followed, in the same block, by
`Inst::CheckFault`** (`VerifyError::UnobservedFault`), and a `CheckFault`
**immediately follows an instruction that can fault**
(`VerifyError::RedundantFaultCheck`).

The pairing is positional and within one block. The weaker property — "some
check dominates this point" — is the one the defect already satisfied:
`v.sum()`'s overflow *was* eventually observed, by the next loop-header check,
one iteration late and with a snapshot showing the following element's operands.

**The converse is the half that does the work.** Without it the forward rule is
satisfied by checking after everything, which is exactly what lowering did — so
REP-53's fix would have had no invariant behind it and would have regressed to
unconditional the first time a site was copied. With it,
`panic_fault_is_observable`'s premise — that a `Pure`/`Allocates` wrapper is
never followed by a `CheckFault`, so its panic path may abort rather than fault
— is enforced rather than asserted.

§10.4's "later optimization may combine checks when safe" relaxes **both**
directions together: a combined check observes several faulting instructions, so
neither "immediately followed" nor "follows a faulting instruction" survives it.
The pass that introduces one relaxes this rule with it. Until then, a rule that
admitted a shape nothing builds would not catch the site that forgot.

### 2. One authority for the instruction→symbol mapping

`Inst::fault_reason()` lives in `praxis-mir::ir` and derives its answer from the
ABI manifest via `AllocKind::symbols()` / `ScalarKind::alloc_symbol()` /
`load_symbol()` — **the same functions the Cranelift backend now calls** to
choose which symbol to emit. This is not optional decoration: with the mapping
restated inline in `lower_inst`, the verifier's answer and the call the backend
actually emits are two statements of one fact, and the next person to change a
symbol in the backend makes the verifier lie.

It answers *why* rather than a bare `bool` because the verifier names the reason
in its error, and a second answer to "which symbol is this" is the drift the
mapping exists to prevent.

### 3. No carve-out for `Alloc { Text }` — take the cost, register the cure

`praxis_alloc_text` genuinely faults (`InvalidText`), but the compiler hands it
bytes that came from a Rust `String`, so the fault cannot fire at a generated
call site. 41 corpus sites now emit a check that can never be taken.

**A site claim was rejected.** It was tempting — `Overflow::Bounded` is exactly
such a claim (ADR-044 decision 6) — but the precedent does not carry: that claim
removes two instructions *and a call per loop iteration* and **the backend reads
it**, so it cannot be silently ignored. This one would remove one call per
literal and be read by nothing but the verifier's own exception. MIR-10 exists
because a rule with a hand-carved hole is not a rule, and putting the hole in the
very first arm is how that happens.

The real cure is to move the fault out of the symbol: split `praxis_alloc_text`
into a validating helper for the raw-stdin path and an ABI entry whose UTF-8
precondition is the compiler's, making the row `Allocates`. That changes what a
violated compiler precondition *does*, which is ADR-017 territory, and it must be
reconciled with REP-45's `a_wrapper_that_can_raise_a_fault_declares_that_it_faults`
sweep. **Registered as REP-67**, not ridden in here — and it can supersede this
decision later without touching the rule, which is the property to want.

### 4. The `sum`/`product` accumulator gets a per-element check

§10.4 is unambiguous, and it is what makes the overflow divert *at the addition*,
so the crash snapshot shows the operands that overflowed rather than the next
element's. The alternative is a new MIR instruction for a hoisted check, i.e.
inventing a shape for a hypothetical optimization. The cost is roughly the
per-iteration `praxis_vec_len` check REP-53 just deleted from the same loop
header.

This is the case ADR-044's final Consequences bullet named as the reason the rule
could not be adopted. It is now adopted, and that bullet is amended.

## Consequences

- **Lowering has one reader of the fault column.** `Builder::call_runtime` and
  `Builder::alloc` push the instruction and then check iff the symbol's manifest
  row says it faults. ~34 local judgements become 3 deliberate ones (checked
  `IntBinOp`, `ValueCmp`, and the fused accumulator), the same way F17 replaced
  61 hand-written root lists with one pass.
- **A dead call is deleted.** The Cranelift `IntBinOp { Checked }` path ended
  with `let _ = call_symbol(…, CheckFault, …)` whose result was discarded with no
  branch after it — a leftover from before MIR carried `Inst::CheckFault`. It
  cost a call per checked arithmetic op and observed nothing.
- **ADR-044's Consequences bullet is stale in a second way and is amended.** It
  says the `sum` accumulator's overflow is one the host sees "after `main`
  returns". Measured: it is not. `v.sum()` over `[i64::MAX, 1]` followed by
  `out(111)` faults without printing `111` — the fault is observed inside the
  loop, one iteration late. The bullet's conclusion was right and its mechanism
  was wrong.
- **Two documents stop restating the rule.** `panic_fault_is_observable`'s doc
  and its test's doc both asserted "MIR emits one only after a call it classifies
  as faultable" as a premise. That premise is now enforced, so both point at the
  verifier instead of repeating it.
- Both new `VerifyError` variants name function, block and instruction index —
  MIR has no source span, and inventing one would be worse than naming the
  instruction.
