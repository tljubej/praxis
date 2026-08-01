# Every row closed — and three of them were not what the register said

**Date:** 2026-08-01
**Supersedes:** `17-what-is-left-handover.md` for *what is left*; read 17 for how
the work was scoped, and its last section ("the two things this session would
most like the next one to keep") is still the best statement of the culture.

The suite's pass count is not in this file. `just ci` is the statement, and §1 of
`implementation-repair-progress.md` explains why a count lives in one place or
none. What is re-runnable is the gate.

## The one-paragraph answer

**Every §4.1 row is closed and both open decisions are answered.** Handover 17
listed five mechanical rows, six blocked on a language decision, and D16/D18.
All thirteen landed, each with a gate observed red with its own fix removed.
Nine ADRs came out of it (086–094, minus none).

The part worth reading is not that they closed. It is that **three of the rows
described their own defect wrongly**, and the wrongness was load-bearing in each
case — it was the reason the row had been deferred.

## The three corrections

**REP-56 was not a feature.** The row said it "needs a real payload-record type —
a feature, not a repair", and that framing is why a P0 sat open. The payload
record type was already real end to end: `synthesize` built a genuine anonymous
`TypeData::Record`, `unify` already unified anonymous records structurally, and
the field indices already agreed with the runtime slots by construction. The
actual defect was that `infer_pattern` reached the enum through the resolved
constructor **symbol** while lowering reached it through the **scrutinee** — and
a `choice(...)` enum has no symbol, so inference skipped the arm and the field
read lowered as `Unit`. Roughly fifteen lines (ADR-091).

**REP-33 was wrong twice.** It listed five names Appendix D needed — `sorted`,
`frequencies`, `zip`, `map`-on-a-pipeline, `sum` — and three of them were
implemented and working. Six of the eight `Y110`s were **cascade** off the fresh
type variable an unresolved call hands back. And the row was two items, of which
one was a repair and the more valuable half: a program that passes `check` and
dies at lowering is itself the defect shape (ADR-093).

**REP-54's estimate had no fix to build.** The row and ADR-078 both predicted "a
tuple descriptor built from the child descriptors, which the static-descriptor
path has no constructor for". There was nothing to construct: `TUPLE` is a
uniform descriptor like `VEC` and `RECORD`, the per-shape `TupleSchema` lives in
the payload, and `VecPayload::element_descriptor` is a `*const TypeDescriptor`
that could not hold a schema in any case (ADR-092).

The common shape: **each row had been written from the symptom plus a guess at
the cause, and the guess was what made it look expensive.** Every one of them was
re-reproduced before it was planned, and that is what corrected them.

## What landed

| Row | ADR | The answer |
|---|---|---|
| **REP-65** (new) | 086 | A `Text` subscript answers a `Char`. `Char.to_int()`/`Int.to_char()` are the whole new surface |
| **REP-60** stdin | 087 | Empty input is input; "no input" is an embedder state |
| **MIR-10 + 52 + 53** | 088 | A faulting instruction is observed by the next one, **and only a faulting instruction is** |
| **D16** | 089 | A name has one signature. `assert` keeps one argument; `Y024` names the counts |
| **REP-58** | 090 | A `block` item is offered its own lines |
| **REP-56 + 57 + 66** | 091 | A variant pattern's enum is the scrutinee's; a record pattern needs no head |
| **REP-54** | 092 | A template's shape is read from its parts; `PlanNode::Tuple` deleted |
| **REP-33** | 093 | A method that cannot resolve is reported at `check` — one emitter, and it is inference's |
| **D18** | 094 | A template ends at the line it opens on |
| **REP-55** | — | `grid` and `matrix` share one `uniform_row_width`; the fault names the offending line |
| **REP-46** | — | Nine rows: three modes over `add`, `sub`, `mul`. §4.12 states the closure |

## Five things worth not rediscovering

**A deferral's stated reason expires, and nobody re-reads it.** REP-46 was open
because §4.12 said "These three are the family… whether there should be is
undecided (REP-46)" — which reads as the document closing the set. That sentence
was written *by REP-46's own first half*, as a note deferring the question. A
note that defers a question cannot be the authority for the answer. Check who
wrote the sentence you are citing.

**"It reports the right code" is the acceptance-not-value shape, one level up.**
Handover 17's rule is about tests asserting acceptance. The same thing happens to
diagnostics: `m9_matrix_uniformity_faults_on_ragged` asserted *that* a ragged
matrix faulted, and stayed green for as long as the fault named the whole input
instead of the offending row. If a diagnostic carries a span, a count or a name,
assert it.

**A rule with a hand-carved hole is not a rule, and the first arm is where the
hole gets carved.** ADR-088's verifier rule costs a never-taken check at 41
text-literal sites, and the tempting fix was a site claim — there is even a
precedent (`Overflow::Bounded`). It does not carry: the backend *reads* that
claim, so it cannot be silently ignored, whereas this one would be read by
nothing but the verifier's own exception. The cost is registered as REP-67
instead, and it can supersede the decision later without touching the rule.

**Two concurrent `cargo test` processes clobber each other's fixtures.** Five
tests in `praxis-cli/tests/run.rs` wrote fixed filenames into the shared `/tmp` —
one helper even said "named after the calling test so two tests cannot race for
it", which is true within a run and false between two. Four different tests
failed across four runs and every one passed in isolation. `CARGO_TARGET_TMPDIR`
alone does not fix it; the pid does. **A test that fails only when something else
is running teaches you to re-run instead of to look.**

**Slow under load is not broken, and a `ps` line proves less than it looks.**
Twice this session a test binary sitting in state `S` with `0:00.00` CPU was read
as hung. For a process blocked in `Command::output()` waiting on a child, that is
exactly what healthy looks like. Both times the work completed. Snapshot the
binary before timing anything while agents are building.

## What is open

Nothing from the repair. Two rows this round opened:

- **REP-67 (P3)** — split `praxis_alloc_text` so its manifest row becomes
  `Allocates` and ADR-088's rule stops paying for a check that cannot fire. It
  changes what a violated compiler precondition *does*, so it is ADR-017
  territory and must be reconciled with REP-45's faulting-wrapper sweep.
- **D19** — is there a character literal? ADR-086 made `"#"[0]` work, so this is
  ergonomics, not a blocker. **Everything below the parser already exists and is
  dead**: `Lit::Char(u32)` has no constructor, and `AllocKind::Char`'s MIR
  lowering and its Cranelift codegen are complete. It is a lexer, a parser arm
  and `lower_lit`.

Also standing, and neither is new: `for c in text` is `Y005` because
`capability::iter_item` answers `None` for a scalar; and `chunks`/`windows`
remain the two deferred barriers, because they answer `Vec[Vec[T]]` and nothing
in the design document decides what the outer vector's element is labelled with.
