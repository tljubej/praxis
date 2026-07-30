# Repair session handover — S23, S24, and S26 in part

**Date:** 2026-07-30
**Tree:** `28ffae6` · **Suite:** 1399 passed, 0 failed, 38 ignored · `just ci` green

`implementation-repair-progress.md` is still the living status and this file does
not replace it — read §1 and §4 there first. This is the session-scoped note: what
landed, what the next session should pick up, and the three things that would
otherwise have to be rediscovered.

## What landed

| Stage | Rows | Commits |
|---|---|---|
| **S23** — independent hardening, round two | REP-13, REP-11, REP-12, REP-02 | `9ea5495`, `809d138`, `c64f0d6`, `2a1fa57`, docs `1ef7ba4` |
| **S24** — function values | REP-01 (the last P0) | `ce5f323`, docs `c7e005b` |
| **S26** — declaration/pattern/inference gaps | REP-06, REP-05 (of five) | `3306a04`, docs `28ffae6` |

**The repair has no P0 left.** One decision was answered on the way: **D15**, as
recommended — a bare `fn` name in value position is a closure value (**ADR-061**).

Seven of §4.1's fourteen rows remain: **REP-07…REP-10** (all of S25) and
**REP-03, REP-04, REP-14** (the rest of S26).

## Where to start

**REP-03 + REP-04, together.** They are the same defect from two ends, the plan
puts them first within S26, and they are the last *inference* rows in the repair.
Everything needed to start is below.

The mechanism, confirmed against this tree:

```rust
// crates/praxis-hir/src/capability.rs:392, inside `iter_item`
TypeData::Var(_) => return Some(t),   // ← an unresolved receiver answers with *itself*
```

So for `fn total(r) { var t = 0\n for i in r { t = t + i }\n t }` the loop
variable and the iterator become **one variable**, and `t + i` pins that variable
to `Int`. Reproduction (verified just now):

```text
error[Y005]: values of type `Int` cannot be iterated
  2 |   for i in r { t = t + i }
```

A legal program, rejected — identically for `Vec`, `BitSet` and `Range`, which is
why TY-34's gates all annotate their parameters.

The fix the plan asks for: `iter_item` answers an unresolved receiver with a
**fresh** item variable, and the deferred `Iterable { item }` constraint relates
the two. That is also what makes **REP-04** checkable at all — `capability::check`
answers the yes/no today and never unifies the item, so a constraint that
discharges at a *differently*-itemed iterable is not caught. Doing either alone
leaves the other unfixable in the same shape.

Read **ADR-057** first; it is the map for the constraint channel, and
`Inferer::require_cap` is the only door a capability check may go through.

Exit criteria are in plan §5, S26. After that: **REP-14** (D17 blocks only its
wording; the detect half can land first), then **S25**, whose acceptance criterion
— §3.3's representative program compiling — is the most visible thing left in the
repair.

## Three things worth not rediscovering

1. **The corpus is executed by a Rust test now** —
   `crates/praxis-cli/tests/corpus.rs` (REP-12). It walks `tests/` for `.px`
   rather than listing it, and each program is a triple: the source, a
   **required** `name.out` with its expected stdout, and a `name.in` for the ones
   that `read`. A program with no `.out` fails the test rather than being skipped.
   **So the per-stage corpus triage is now partly automatic**; what is still
   manual each stage is the `praxis check` sweep over
   `crates/praxis-cli/tests/fixtures`, which is not under `tests/`. That sweep was
   run after each of this session's three stages and found only the three fixtures
   that are *meant* to report.

2. **A `Copy` runtime allocation goes through a `Payload<T>` handle** (REP-02),
   not a bare `&TypeDescriptor`: `gc_alloc(ctx, scalars::INT_PAYLOAD, value)`. Add
   a handle beside any new descriptor. The pairing is checked during const
   evaluation of the `static`, and the value's type at the call — the `alloc_with`
   path keeps its runtime assertions and its doc comment says why.

3. **Two diagnostic codes were spent, and both blocks moved.** `Y018` (a generic
   `fn` used as a value, ADR-061) and `Y124` (too many sub-patterns, REP-05).
   **`Y019` is the next free code in the `Y0xx` user block and `Y125` in the
   `Y12x` match block.** ADR-051 is the allocation; amend it before spending
   another.

## Two places the plan was wrong, and what replaced it

Both are recorded in the ADRs and the progress doc; they are here because a
reader of the plan alone would be misled.

- **S24's sketch says the existing closure calling convention "already fits" a
  top-level `fn`.** It does not: a closure's synthetic function takes the closure
  as a hidden first explicit argument and a top-level `fn` does not, so
  `praxis_alloc_closure` over the `fn`'s own address would land every argument one
  slot to the left — silently wrong, worse than the crash it replaced. Each
  adapted `fn` therefore gets one **adapter** (`__fnvalue_double`) whose params
  are `[closure_self, p0…pn]` and whose body is one direct call forwarding
  `[p0…pn]`. ADR-061.
- **REP-06's row says F19's `resolve_top_stmt`/`resolve_block_stmt` split is what
  makes the report possible.** No split was needed: a block's statements already
  go through `resolve_top_stmt`, so the check is one guard in
  `resolve_struct`/`resolve_enum`.

## One thing this session added that no row asked for

**`Y018`**: a *generic* `fn` used as a value. Monomorphization is driven by call
sites and a value has none, so `let f = id` would have reached the JIT as
"unresolved user function `id`" — a Cranelift error out of a program `praxis
check` accepted, which is TY-33's shape again. It is reported in inference
instead, and the message names the remedy (`|x| id(x)`, which works because a
closure body *is* a call site). Giving a generic function a real function value
needs monomorphization keyed on a use-site substitution witness, which S15 already
records as unlanded — so this is a diagnostic now and a possible feature later,
not a wrong answer either way. ADR-061 has the reasoning.
