# Repair session handover — every stage closed, and the register that is left

**Date:** 2026-07-31
**Tree:** `b557e0a` (everything below was measured there) · **Suite:** 1632 passed, 0 failed, **0 ignored** · `just ci` green

`implementation-repair-progress.md` is still the living status and this file does
not replace it — read §1 and §4 there first. This is the session-scoped note:
what landed, where to start, and the things that would otherwise have to be
rediscovered.

## What landed

| Stage | Findings | Merge |
|---|---|---|
| **S18** — the `Option` contract and enum nominal identity | RT-13, RT-14, RT-15; **D1** answered (ADR-074, ADR-075, ADR-076); ABI 13 → 14 | `8f2de86` |
| **S19** — the input-parser compile pipeline | IP-01 … IP-11; **D10** answered (ADR-072, ADR-073); three repair passes | `81a4b9f` |
| **S20** — the parser runtime's cursor and region ownership | IPR-01 … IPR-14; **D11**, **D12** answered (ADR-078, ADR-079, ADR-080); five repair passes | `0a02c0e` |
| **S21** — pipeline plan representation and per-stage indices | MIR-03 … MIR-08 (ADR-071) | `010ba29` |
| **REP-26 … REP-51** | a design-doc sweep, three fresh-eyes hunts and their reviews; see §4.1 for each row | `9727b34`, `13fe039` |
| **REP-52 … REP-59** | registered by those reviews and by S20's; open by decision, except **REP-59**, which S19 had already fixed before the row was written | — |

## What the repair is now

**It is finished.** Every stage the plan schedules is closed: **S1 … S21**, which
are the audit's, and **S23 … S26**, which the repair added for §4.1's own rows.
S22 is struck as "no action". All **139** of the audit's
findings are addressed and the plan's §4 says so on every row, in one convention.
And the acceptance gate the audit actually shipped is at zero: **`cargo test
--workspace` reports 0 ignored**, against a baseline of 149. That has never been
true before.

Do not run `cargo test --workspace -- --ignored`. There is nothing left for it to
run, and §8.1's rule stands anyway — the ignored suite was never safe to batch.

## What is left

Nothing that blocks anything. Two lists, and they are lists rather than counts on
purpose (see the last section).

**Open rows in the plan's §4.1**, all re-reproduced against `b557e0a` in the
close-out pass rather than carried over on the strength of their rows:

| Row | Sev | What, in one line |
|---|---|---|
| **REP-56** | P1 | A `choice` payload record's fields cannot be read; `praxis check` exits 0 and `praxis run` **aborts the host** (`rc=134`) on REP-37's width guard |
| **REP-57** | P2 | A record pattern nested in a variant pattern has no grammar — REP-56's workaround, worth nothing without it |
| **REP-52** | P2 | A fused `collect` pushes with no fault check |
| **REP-53** | P3 | A method call checks unconditionally, so the manifest's fault column has no reader |
| **REP-54** | P2 | A template with two or more anonymous captures is tagged `Unit`: `read lines(\`{int},{int}\`)` prints `[Unit, Unit]` |
| **REP-58** | P2 | §7.7's own "repeated labeled blocks" example does not run |
| **REP-55** | P3 | `matrix`'s ragged-row fault names the whole input where `grid`'s names the line |
| **REP-33** | P2 | Appendix D checks clean and does not run: eight `Y110`s for `sorted`, `zip`, `map`, `sum`, `frequencies` |
| **REP-46**, half | P2 | `wrapping_sub`/`saturating_mul`/`checked_mul` beside the three `_add` forms that landed |

REP-52 and REP-53 are one job with **MIR-10**, the single §4 row still marked
`PARTIAL — part owed`: its verifier landed, its rule — *a faulting instruction is
followed by a `CheckFault`* — did not, and these two rows are that rule's two
ends. Take all three or none; one `check_fault` added by hand fixes one site and
leaves the class.

REP-33 and REP-46's half are **features the design doc writes and the language
does not have**. They are not repairs and should not be taken as ones.

**Two open decisions**, both belonging to stages that are already closed:

- **D16 (S25)** — does `assert` take a message, and does the language get
  arity-based overloading or optional parameters? A name has one scheme
  (ADR-056), which is also why the six graph helpers each have exactly one
  signature (ADR-060). The plan's warning is the load-bearing half: `assert`'s
  message is the cheapest possible motivating case, so **answering it in
  isolation sets the precedent by accident**. Decide it for the language.
- **D18 (S19 round 3)** — may a backtick template span a raw newline? §7.2 says
  `\n` *matches* a line ending and never says whether a raw one may appear in the
  source form. Nothing needs the answer until a template is left unclosed, and
  then it decides everything: `` read `{int` `` is a `T002` spanning the rest of
  the file, which is **true** — under D10 a backtick inside a capture opens a
  nested template — and useless. The recovery alternative was tried and measured
  and is worse: a well-formed two-level template reports `T002` twice, because
  recovery cannot tell an unclosed run from a nested one without already knowing
  where the outer one ends. Answering "a template may not span a raw newline"
  makes recovery unnecessary; it is a language rule and must be taken as one.

## Five things worth not rediscovering

1. **S20's whitespace rule took five rounds, and three of the answers are written
   down as wrong on purpose.** The rule is one sentence — *whitespace the parser
   offered it does not read is nobody's* — and ADR-078's "Whitespace is data when
   the parser offered it reads it" keeps the failed attempts because the shape of
   the mistake outlives the answer. Round one applied exhaustion at a root region
   running to end-of-file, and faulted every real input. Round two trimmed exactly
   one line terminator and reproduced round one on a file ending in a blank line,
   one byte later, with character-for-character the same three messages. **Both
   were a count of bytes, and the rule is not a count.** Round three split it into
   a parser-*independent* extent half and a parser-*dependent* bound half, which
   then gave opposite answers to the one question the rule exists to settle — is a
   trailing space a cell `char` could read? — so `grid(char)` over `"ab\ncd \n"`
   was ragged while `grid(char)` over `"ab\ncd\n  \n"` silently lost four cells.
   A half that cannot ask the question cannot answer it. Round four made it one
   question with one answer (`walk_exact` asks, `ByteRegion::is_all_whitespace` is
   the predicate, `cursor::trailing_blank_run` is the same question at line
   scale); round five brought the last two constructs — a template capture and a
   CSV field — inside it. **If you are about to write a byte count into this area,
   you are on round one again.**

2. **A branch that predates a stage silently un-does that stage's renames.** S18
   renamed three defect-pinning tests, and each rename *is* the fix's assertion:
   `map_get_absent_returns_unit` →
   `an_absent_map_get_answers_none_and_a_present_one_answers_some`,
   `adv_map_get_absent_returns_unit` →
   `an_absent_map_get_is_a_none_the_program_can_match_on`, and
   `adv_pipeline_empty_source_min_is_zero` →
   `an_empty_min_or_max_faults_rather_than_answering_zero`. A branch cut
   before S18 still carries all three under their old names, and a union-flavoured
   merge **adds them back** rather than colliding: the suite goes green with two
   tests asserting `Map.get` returns Unit and one asserting an empty `min` is `0`,
   which is the contract D1 deleted. The same session has the receipts on both
   sides — `b2184c8` is literally *"restore the brace the S19 merge dropped from
   jit.rs"*, and **REP-59** was filed as open against a branch whose base merge
   (`50c6914`) had taken an earlier S19 than `main` did, so the row described a
   defect that was already fixed on the tree anyone would run. After any merge in
   this repo, grep for the **old** names, not just for failures.

3. **The recurring gate failure is a test that asserts an operation was
   *accepted*.** It has cost this repair a finding at a time, in every subsystem
   that has one. Three tests covered
   `Grid.rotate_left`/`rotate_right` and all three asserted `width() * 10 +
   height()` — and both rotations of a 3×2 grid have identical dimensions, so the
   two functions could perform each other's rotation undisturbed (REP-36).
   `filter_map` was pinned by a test asserting `map`'s answer (REP-38). `find` was
   pinned by a test whose *name* was the finding, `pipeline_position_is_alias_of_find`
   (REP-39). The `Float` rendering was covered by a descriptor test whose one
   value is `2.5`, which carries a `.` and passes under either rule (REP-44). And
   S21's own exit-criterion test for `flat_map` asserts that the compiler
   survives, plus a count a wrong nesting also produces (MIR-06). The rule
   this session ended on is in the progress doc and is worth repeating: **a test is
   a gate only if it was observed red with its fix removed, and the observation is
   written down.** A test that passes on `main` before the fix is a *companion* —
   worth having, because it rules out a fix that breaks the ordinary path — and
   counting one as a gate is a bookkeeping error a reviewer caught twice here.

4. **`praxis check` is not the triage, and `< /dev/null` is not an input.** `Y110`
   and `Y112` are reported at *lowering*, which `check` does not reach, so a
   `check`-clean corpus can hide a `run`-broken one — that is REP-12's shape,
   REP-33's, and REP-56's. And every `read`-driven program faults `ParseFailed`
   against an empty stdin, so a sweep piping `/dev/null` into all of them proves
   only that `read` needs input. `< /dev/null` is also what `Command::output`
   binds, which is why 1500 tests never noticed that the host read stdin before
   the program started (REP-51) — for a milestone and a half. **Both commands,
   each program's own `.in`, or the sweep is theatre.**

5. **The runtime's guards convert this class of defect into an abort, and that is
   the design.** REP-56 no longer reports `Y112`; it kills the process with
   `int_payload reads eight bytes; Unit is 0 wide` and `internal error: a panic
   escaped the runtime wrapper praxis_int_load`. That is REP-37's width assertion
   plus ADR-080's `catch_unwind` doing exactly their jobs — the eight-byte read of
   a zero-width payload is *caught*, not performed. So an abort with one of those
   two messages in it is a **front-end** bug wearing runtime clothing: read it as
   "a value reached lowering as `Unit`", and look at inference, not at the ABI.

## The bookkeeping rule this session cost the most

**A rule stated in four places goes stale in three.** Almost every documentation
commit in this session's log is that: `cc37f81` ("one statement of the whitespace
rule, and it names live code"), `1bf3e86` ("the corpus count in the doc and in the
gate are one fact"), `c02b5f4`, `335345f` ("four counted claims that were off by
one, and one that overclaimed"), `049bb85`, `88c65ce`, `b557e0a` ("the sentences
S20's final review found untrue"). None of them found a defect in the code. Every
one found a sentence that had been true when it was written.

The specific form to avoid:

> **Where a claim is a count, drop the number.**

A count is a claim that nothing makes fail. §4.1's opening said "Twenty-five
defects … and every one of them is done", then "Nine rows are open", and both
were wrong within days of being written, in a register that has since run to
REP-59. A list goes stale *visibly* — a reader who checks REP-59 finds it and
argues — where a count goes stale *silently*. §4.1 and §1 both name their open
rows now and neither counts them.

Two smaller forms of the same rule, both paid for here:

- **State a rule once, in the place that can be checked.** ADR-078's whitespace
  rule names the live predicates (`ByteRegion::is_all_whitespace`,
  `cursor::trailing_blank_run`) rather than restating the rule in prose beside
  them, so a rename breaks the reference instead of leaving a second, older
  statement standing.
- **A measurement names the tree it was taken on.** REP-59 was a real defect on
  the branch that measured it and fixed on the tree it was filed against. A row
  that says "measured at `<commit>`" can be re-run; one that says "is" cannot.

## The corpus triage

Both commands over every `.px` under `tests/` and every fixture under
`crates/praxis-cli/tests/fixtures`, each program driven with **its own `.in`**
where one exists. The full table is in the progress doc's §1; the parts worth
having here:

- `tests/aoc-corpus` (18 programs) and `tests/input-parsers` (8) are **clean on
  both commands and every one matches its `.out`**.
- The 30 CLI fixtures have no `.out` and should not: nothing walks them. They are
  driven by `crates/praxis-cli/tests/{check,run}.rs` with the expectation written
  in Rust beside the call. **Ten of them exist to fail** — four report at `check`
  (`bad_byte`, `parse_error`, `type_error`, `unterminated_template`), five fault
  at run with a clean `check` (`overflow`, `debug_temps`, `div_by_zero`,
  `float_to_int_nan`, `debug_backtrace`), and one is refused by the host rather
  than faulting (`no_statements_and_no_main`). `float_div_by_zero` is **not** one
  of them: it prints `inf`, because IEEE division is not the integer rule.
- **Two corpus programs have no `.in` and that is correct.**
  `day07_closure_pipeline.px` inlines its value and
  `day10_bfs_shortest_distance.px` hand-encodes its adjacency; neither contains a
  `read`. A `.in` is required only of a program that reads.
- **One fixture reads and has no `.in`:** `reads_lines_of_int.px`, which
  `run.rs:225` drives with the stdin `"1\n2\n3\n"` and expects `3\n3`. Given that
  input it answers `3` and `3`. Given `/dev/null` it faults `ParseFailed` — see
  item 4 above.
