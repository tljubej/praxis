# Milestone 9 Handover — Input Parser v2 (§19.9)

**Date:** 2026-07-27
**Status:** Complete. All §19.9 deliverables landed; all five acceptance criteria met with runnable corpus fixtures. 779 tests pass; `just ci` clean.

> For a fresh context: read this document, then `praxis_technical_design.md` §7
> (the input-parser DSL), §7.5 (structural constructors), §7.8 (type
> derivation), §19.9 (Milestone 9), and §19.10 (Milestone 10). The M8
> handover (`08-milestone-8-handover.md`) covers the collection/pipeline
> foundation M9 builds on.

## 1. What M9 delivered (WS1–WS8)

M9 = **"Input parser v2"** — the harder AoC input shapes, extending the M6/M7
input-parser DSL. Eight workstreams, each committed independently green:

- **WS1 — Foundation.** The `Option[T]` prelude enum (`Some(T)`/`None`), the
  named-args grammar (`PARSER_NAMED_ARG`), and ADR-030 (`matrix(P)` → `Grid[T]`,
  closing §21.1). The headline type-system change: **structural unification for
  same-named enums** (`unify.rs`) — the soundness fix that makes a polymorphic
  `forall T. Option[T]` scheme viable (without it, independently-instantiated
  Option defs would refuse to unify).
- **WS2 — Named heterogeneous `sections` + `repeated` tail.**
  `sections(name: P, ..., tail: repeated(P))` → anonymous record.
- **WS3 — `block`.** Sequential parsers within one region; positional
  named-capture templates *flatten* their fields into the result record.
- **WS4 — `choice` generated enums.** `choice(Name: P, ...)` → anonymous enum;
  first-match-wins with backtracking. Added `TypeDb::anon_enum`.
- **WS5 — `optional`.** `optional(P)` → `Option[result(P)]`; failure consumes
  no input.
- **WS6 — `scan`.** `scan(P)` extracts repeated matches from noisy text (C.9).
- **WS7 — `matrix`, `chars`, `one_of` + ragged-grid runtime.**
  `matrix(P)` → `Grid[T]` (whitespace-tokenized); `chars(P, skip:)` →
  `Vec[Char]`; `one_of("LR")` → `Char`; `grid(P, ragged, fill:)` runtime ready.
- **WS8 — Acceptance fixtures + fixes + ADR-031.** The five §19.9 fixtures; a
  trailing-comma grammar fix; a `block` validation fix; the fixed-width
  deferred ADR.

Plus a Counter-Text-key regression test (WS9 remnant) confirming the M8 "known
bug" was already fixed.

## 2. §19.9 acceptance criteria — status

| Criterion | Fixture | Status |
|---|---|---|
| Parse bingo-style input | `m9_bingo.px` (C.6: `draws`, `repeated(matrix)`) | ✅ |
| Parse almanac-style repeated mapping sections | `m9_almanac.px` (C.8: `repeated(block)`) | ✅ |
| Parse repeated labeled blocks | `m9_repeated_labeled_blocks.px` (§7.7) | ✅ |
| Parse grid plus folded command stream | `m9_grid_and_commands.px` (C.7: `grid`, `chars`/`one_of`) | ✅ |
| Parse noisy embedded instructions | `m9_noisy_scan.px` (C.9: `scan` + `choice`) | ✅ |
| Every complex fixture has a useful failure diagnostic + partial parse state | (partial) | ⚠️ see §6 |

All five parsing criteria are met. The diagnostic-richness criterion is
**partial**: parse failures raise `FaultKind::ParseFailed` (clean, no segfault)
but do not yet carry the §7.11 detail (input span, parser span, expected
description, actual preview, partial root value). This is the one §19.9
acceptance item not fully closed; see §6.

## 3. Where things live

- **Compile-time DSL** (`crates/praxis-input-parser/src/`):
  - `ast.rs` — `ParserAst` (now 18 variants), `BlockItem`, `SkipPolicy`.
  - `validate.rs` — structural checks (I020–I027).
  - `synthesize.rs` — the §7.8 type-derivation table.
  - `plan.rs` — `PlanNode` arena + `lower_to_plan`; the global plan slab.
- **Runtime interpreter** (`crates/praxis-runtime/src/parser.rs`):
  - The `walk_*` family — one per constructor.
  - `walk_sections`/`walk_sections_named`/`walk_block` parse each section against
    a **bounded byte view** (so a child consumes only its section).
  - `walk_template` returns the **real cursor** (not `bytes.len()-offset`),
    which is what makes `block`'s sequential advancement work.
  - `Rt::alloc_enum` builds Option/choice values matching the `EnumPayload`
    layout codegen expects (reuses the M7 `ENUM` descriptor; no new TypeId).
- **Grammar** (`crates/praxis-parser/src/parse.rs`): `parse_parser_call` handles
  named args, the `repeated(...)` tail, keyword args (`skip:`/`fill:`), the
  `ragged` flag, and trailing commas.
- **HIR bridge** (`crates/praxis-hir/src/parser_lower.rs`): M9 constructors
  dispatch by name before the M6 `Constructor` table; `CallArg::{Named,
  RepeatedTail}`; `build_sections_named`/`build_block`/`build_choice`.
- **Type system** (`crates/praxis-types/src/`):
  - `unify.rs` — the M9 same-named-enum structural-unify arm.
  - `db.rs` — `anon_enum`.
  - `generalize.rs` — already handled `Enum` (instantiate re-registers fresh
    defs; the unify arm makes them merge).
- **Inference** (`crates/praxis-hir/src/infer.rs`): `Option`/`Some`/`None`
  scheme seeding; `Option[T]` type-annotation resolution.
- **Prelude** (`crates/praxis-stdlib/src/prelude.rs`): `Option`, `Some`, `None`.
- **Acceptance fixtures**: `tests/aoc-corpus/m9_*.px`.
- **ADRs**: `docs/decisions/030-matrix-is-grid.md`, `031-fixed-width-deferred.md`.

## 4. Key engineering insights

1. **The polymorphic-enum soundness fix was the load-bearing change.** Praxis
   enums were nominal/identity-only (`unify.rs` compared by `EnumDefId`). A
   polymorphic `forall T. Option[T]` is unusable under that rule: each
   instantiation mints a fresh def, so `Some(5)` and an `Option[Int]`
   annotation carry different def-ids and refuse to unify. The fix relaxes the
   arm: two enums with the **same name and variant-name signature** unify their
   payloads pairwise and link to one canonical def. Safe because two
   user-declared enums can't share a name in one scope, so this only ever fires
   for compiler-stamped copies. This one change fixed `Option[T]` *and*
   `choice`'s anonymous enums simultaneously, and mirrors how anonymous records
   already unified.

2. **Sections must parse bounded regions.** The original `walk_sections`
   passed the full `bytes` from the section offset, so a child like `lines(int)`
   consumed to end-of-input. Both `walk_sections` and `walk_sections_named` now
   pass each section as a bounded byte view. (Caveat: source-slice `Text`
   offsets inside a section are relative to the sub-slice; value-producing
   parsers — int/csv/grid/matrix — are unaffected. The absolute-offset
   refinement for `word`/`text` inside sections is a follow-up.)

3. **`walk_template` must return the real cursor.** It previously returned
   `bytes.len() - offset`; nothing in the M6/M7 tree consumed that value, so
   changing it to the true match-end position was safe — and is what lets
   `block` advance item-by-item.

4. **`None` is a monotype, not a function.** The first cut gave `None` the
   scheme `forall T. () -> Option[T]`, which made `let v = None` infer as a
   *function* type, breaking match exhaustiveness. Zero-payload variants get
   the enum type directly (matching how `infer_enum` handles user
   zero-payload variants).

5. **Keyword-arg values aren't parser expressions.** `skip: whitespace` and
   `fill: 0` have values that aren't valid parsers; converting them would emit
   spurious "unknown atomic parser" diagnostics. The HIR bridge captures their
   raw source text instead, and the `ragged` flag is recognized and skipped.

## 5. Definition of Done

Per §20.1: every deliverable has unit + integration tests, `just ci` is green,
and deliberate deviations are recorded in `docs/decisions/`.

- ✅ Each constructor has unit tests in its crate + JIT end-to-end tests.
- ✅ `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace`
  all pass (779 tests).
- ✅ ADR-030 (matrix-is-Grid) and ADR-031 (fixed-width deferred) record the
  two §21/§19.9 design decisions.

## 6. Known limitations / follow-ups for M10+

These do **not** block the §19.9 parsing gate (all five criteria pass) but are
real gaps surfaced by M9:

**Parser / interpreter:**
- **Rich parse diagnostics (§7.11).** Failures raise `ParseFailed` cleanly but
  without the §7.11 detail (input span, parser span, expected, actual preview,
  partial root value). This is the one partial §19.9 acceptance item. The
  `WalkResult::Err(())` needs to become `Err(ParseFail { … })` threaded up,
  with partial results as the "partial root value".
- **Template literals with leading-space text after a capture** double-handle
  the space (the scanner includes the space in the literal text *and* the
  SpaceRun ws policy). Forms with literal-before-capture (`Monkey {id:int}:`)
  work; `{x:word} map:` does not. The scanner should strip leading whitespace
  from literal text into the ws policy.
- **`word` doesn't stop at `-`/`:`** etc., so `{source:word}-to-{dest:word}`
  is greedy. Either narrow `take_word_run`'s delimiter set or make template
  captures non-greedy up to the next literal.
- **Ragged-grid `fill:` value grammar.** The runtime `walk_grid_ragged` is
  complete, but `fill: <bare-value>` isn't parser-parseable (a bare scalar
  token isn't recognized by `parse_parser_expr`). Accept a wider token set for
  `fill:` values.
- **Source-slice `Text` offsets inside sections** are relative to the bounded
  sub-slice, not the original buffer. Value parsers are unaffected; thread the
  absolute base offset for correct `word`/`text` slices inside sections.

**Type system / inference:**
- **Anonymous-record field access** through (a) a single `repeated`-only
  `sections` field, (b) a match-bound `choice`/enum payload record, and (c)
  deeply-nested records, hits an inference gap ("no field `x` on this type").
  Binding through `let` sometimes works. This is pre-existing (surfaced by M9's
  record-producing constructors), not parser-specific.
- **`choice` with record-payload cases** + field access on the matched payload
  hits the same gap; scalar-payload choices work fully.

**Carry-forwards deferred from M8 (elected scope, not closed):**
- `find`/`position` → `Option[Int]` (now that `Option[T]` exists). Catalog +
  ABI + test updates.
- Tuple field access `.0`/`.1` (ADR-026). Unblocks `enumerate`/`zip`.
- `min=`/`max=` parser assignment operators (runtime helpers exist).
- `for` over Map/Set/Grid/Counter; `Grid.map`; recursive named closures.
- Pipeline barriers (Y110): `sorted`/`unique`/`frequencies`/`chunks`/`windows`.

**Confirmed-not-bugs (regression tests added):**
- Counter with vec-sourced `Text` keys accumulates correctly
  (`counter_vec_sourced_text_keys_accumulate`). The M8 handover's "known bug"
  is not reproducible; the M8 audit §6.4 was right.

## 7. Test count

779 tests workspace-wide (was 728 at M8 close). M9 added:
- 8 `Option[T]` JIT tests; 6 enum-unify unit tests; 4 named-arg parser tests.
- Per-constructor JIT tests: 5 sections, 3 block, 4 choice, 4 optional, 3 scan,
  5 matrix/chars/one_of.
- 1 Counter-Text-key regression test.
- 5 acceptance fixtures (`tests/aoc-corpus/m9_*.px`).

## 8. Milestone 10 — what to build

**Milestone 10: Crash debugger REPL** (`praxis_technical_design.md` §19.10,
quoted verbatim):

> **Deliverables**
> - Terminal crash REPL.
> - Stack/frame navigation.
> - Local display.
> - Read-only expression evaluator through JIT.
> - Input/parser context commands.
> - Restart and reload.
> - Noninteractive fallback behavior.
>
> **Acceptance criteria**
> - Inspect scalar-object, text, record, vector, map, and grid locals.
> - Evaluate expressions using selected-frame locals.
> - Reload after editing and rerun with identical input.
> - GC retains all objects reachable from snapshots.
> - No command can mutate or resume a faulted state in v1.

The skeleton crate `praxis-debugger` already self-documents
`FILLED_AT_MILESTONE = 10`. M10's natural first step is wiring that crate to
the runtime's fault path (which M9's richer parse diagnostics, when landed,
will feed directly: a `ParseFailed` fault should drop into the REPL with the
input/parser context commands). The §7.11 parse-diagnostic work deferred from
M9 (§6 above) is the on-ramp.
