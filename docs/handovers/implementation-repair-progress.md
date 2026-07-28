# Implementation repair — progress

Living status for `implementation-repair-plan-2026-07-28.md`. **The plan is the
plan; this file is what has actually happened.** Read both before starting a
stage: the plan says what to do and in what order, this says where the tree is
and what the last session changed underneath you.

Update this file at the end of every stage.

## 1. Status

| Stage | State | Commits |
|---|---|---|
| S1 — Runtime identity registry | **done** | `055e894` |
| S2 — Independent hardening | **done** | `5629736`, `70ea4be` |
| S3 — ABI manifest, MIR representation, fault values | not started | |
| S4 — Object layout and heap provenance | not started | |
| S5 — Root-set completeness, native RAII roots | not started | |
| S6 … S21 | not started | |

Also closed out of order: **DBG-01** (`3836b74`), a P0 the plan schedules in
S10. It fell out of S1 — see §3.

Baseline at `136ce4b` was **928 passed, 0 failed, 149 ignored**.
Now: **964 passed, 0 failed, 140 ignored**. `just ci` is green.

Nine of the audit's ignored regressions are un-ignored and passing:

| Test | File |
|---|---|
| `builtin_type_ids_are_globally_unique` | `praxis-runtime/src/descriptor.rs` |
| `regression_unicode_identifier_may_start_with_a_unicode_scalar` | `praxis-parser/src/lex.rs` |
| `regression_in_is_classified_consistently_with_the_keyword_table` | `praxis-syntax/src/kind.rs` |
| `regression_postfix_forms_may_be_interleaved` | `praxis-parser/src/parse.rs` |
| `regression_formatting_does_not_delete_comments` | `praxis-parser/src/fmt.rs` |
| `for_continue_targets_the_increment_block_not_the_header` | `praxis-mir/src/build.rs` |
| `missing_explicit_input_file_is_a_usage_error` | `praxis-cli/tests/run.rs` |
| `float_sign_of_zero_is_zero` | `praxis-runtime/src/abi.rs` |
| `regression_runtime_scalar_descriptors_recover_their_actual_types` | `praxis-debugger/src/evaluate.rs` |

Two `#[cfg(miri)]` regressions (P0-05, P0-14) now run in the **ordinary** suite,
because both fixes are observable without UB detection. §8.4's "no standing
gate" note no longer applies to them; a Miri job is still worth adding.

## 2. Foundations: what actually landed

Partial foundations are the main trap for a fresh context. F1 and F3 are both
partial, and the plan's foundation descriptions do not say so.

**F1 — built-in type identity: landed, minus `compare` semantics.**
`BuiltinTypeId` is the registry; `TypeDescriptor::builtin::<P>` is the only
constructor for a built-in and derives `id` from the variant and `size`/`align`
from the payload type. `BUILTINS` is the lookup table. `compare:
Option<CompareFn>` **exists on every descriptor and is `None` everywhere** —
the field's shape is settled, its semantics are design decision D3 and are not.
See ADR-038.

**F3 — identifier class: predicate half only.** `praxis-syntax/src/ident.rs`
has `is_ident_start` / `is_ident_continue` / `is_ident` (XID + `_`, via the new
`unicode-ident` workspace dependency), and the lexer uses them. **Not done:**
the `Ident` newtype; `_` lexing as `UNDERSCORE` (that is D7 + S12, and it
changes what parses); `praxis-input-parser/src/scan.rs:194` `split_capture`'s
independent ASCII rule (IP-04, S19); `praxis-debugger/src/evaluate.rs`
`sanitize_name`, which still *rewrites* invalid names to `_x` non-injectively
(DBG-03, S12) — its bug-pinning test `sanitize_rejects_digit_leading_and_punct`
is still green and still needs rewriting, per §8.2.

No other foundation has been started. F4 (the ABI manifest) is next on the
critical path and is what S3 is mostly made of.

## 3. Things that changed under you

Mechanical consequences a fresh context will hit immediately.

**Descriptors are `static`, and three fields are private.**
`praxis_runtime::scalars::INT` and its twenty siblings are `static
TypeDescriptor`, not `const &TypeDescriptor` — call sites take `&scalars::INT`.
`id`, `size` and `align` are private: use `.id()`, `.size()`, `.align()`.
`ptr::eq` on two descriptors is now authoritative type identity, which is what
P0-12 / RT-11 / RT-12 / DBG-06 need; ADR-038 supersedes ADR-028's "compare by
TypeId, not by pointer". Existing `TypeId`-equality comparisons in `abi.rs`
were left alone — they are now merely one of two correct spellings.

**Descriptor ids were renumbered.** They follow `BuiltinTypeId`'s declaration
order, which is not the old assignment. Nothing persists them, but any note or
ADR quoting a specific number (ADR-024's `TypeId(8)`, ADR-026's `TypeId 10`,
ADR-027's `TypeId(12)`, ADR-028's "TypeIds 6–19", ADR-030's "TypeId 7") is now
describing a variant, not a literal.

**`SyntaxKind::from_raw_u16`** is the total conversion; `kind_from_raw` calls
it. It relies on the enum having no explicit discriminants —
`every_raw_value_in_range_round_trips` is what keeps that true.

**The lexer emits an `ERROR` token for an unclassifiable character.** It
previously consumed the bytes and emitted nothing, so the tree did not
reproduce the source. This was *not* an audit finding; the widened FE-08
generators found it. Downstream consequence: the parser now sees `ERROR` tokens
in the stream and produces its own recovery diagnostic for them, so a bad
character yields two diagnostics rather than one.

**`SourceMap` stores `Arc<SourceFile>` and `FileView` has no lifetime
parameter.** `map.get(id)` returns an owning view that survives later `intern`
calls. There were zero out-of-crate users, so nothing else changed.

**`praxis_codegen_cranelift::symbols` is `pub`, and `symbols::resolve` is the
one runtime symbol table.** `module.rs`'s independent 57-name registration list
is deleted; `JITBuilder::symbol_lookup_fn` reads the resolver, and
`runtime_funcref` returns an error for a name the resolver does not know. **F4
replaces all of this** with `RuntimeSymbol` — do not invest further here, but
note that adding a runtime symbol today means adding it to `symbols.rs` only,
and that a missing one is now a hard compile error rather than a `dlsym`
success.

**`Jit::check_target(pointer, endianness)`** rejects a non-i64-pointer or
big-endian target at `Jit::new`. It takes the two facts rather than an ISA so
it is directly testable.

**`lower_for` emits an extra block.** The index increment has its own block and
is `continue`'s target. Anything that counts or indexes MIR blocks in a `for`
loop needs re-reading.

**The formatter preserves comments** and decides trailing-vs-standalone by
looking at the whitespace token before the comment. When F8 adds
`Token::preceded_by_newline`, `fmt::starts_new_line` should read that instead of
re-deriving it.

**`praxis_float_sign` no longer uses `f64::signum`.** Both zeros give `0.0`.

## 4. Where to start

The DAG's next candidates, cheapest first:

- **S4 — object layout and heap provenance** (weight 4). Self-contained: F6's
  `GcHeader` repack, one commit, hard barrier before S6. The cheapest real
  stage left and a good next unit.
- **S3 — ABI manifest, MIR representation, fault values** (weight 40). The
  critical path. F4 must land before *any* stage adds a runtime symbol (H16),
  which is P0-12 in S10, MIR-09 in S9 and P0-08 here. H4 and H10 both apply.

Before either, re-read §6 of the plan. The hazards that bind soonest: **H16**
(no new runtime symbol before F4), **H6** (repack `GcHeader` once), **H1/H2**
(P0-06 and P0-07 must be one commit, and P0-08b must not precede them), **H17**
(one ABI-version bump per stage — S4, S5 and S9 all change `#[repr(C)]` types
that generated code reads).

## 5. Design decisions still open

None have been answered. Three block a stage outright (D1, D3, D5); the rest
should be settled while earlier stages are in flight. The ones that bind
soonest, in stage order:

| | Decision | Blocks |
|---|---|---|
| D9 | What the JIT does when `descriptor_for_type` returns `Err` — diagnose, or fall back and reintroduce the bug | S7 |
| D3 | NaN ordering, and whether Text/tuples/records/collections are orderable at all. **This is what `TypeDescriptor::compare` is waiting for**; a new ADR supersedes ADR-026's ordering sentence | S10, blocking P0-12 |
| D7 | After `_` lexes as `UNDERSCORE`, is it still legal in `let _ = f()`, `fn g(_)`, `\|_\| 0`? No `.px` fixture uses them | S12 |
| D8 | Exactly where a newline terminates an expression | S12 |
| D13 | Diagnostic-code allocation for the whole block, before S13 starts | S13/S16 |
| D2 | Loop break-value semantics | S14 |
| D4 | Hashability of mutable collections as Map keys | S17 |
| D5 | The 15 phantom prelude names: implement or delete | S17 |
| D6 | `CollectionCtor::Range`: delete or implement | S17 |
| D1 | `Map.get` / `Grid.find` — `Option[V]` or V-with-Unit. Source-visible | S18 |
| D10 | How much parser-expression grammar a template capture body may contain | S19 |
| D11 | `grid(int)` granularity; greediness of `text`/`word` | S20 |
| D12 | Panic-across-FFI policy — should precede RT-06, RT-07 and the parser findings | cross-cutting |

## 6. Corrections to the plan

Things the plan states that are no longer or were not quite true.

- **DBG-01 is closed** (`3836b74`), not open in S10. S1's exhaustive
  `BuiltinTypeId` match fixed five of its six stale scalar mappings as a side
  effect; the sixth (`Byte`) was fixed with it. **DBG-02** — collection element
  types defaulting to `Int` — is untouched and still needs F11.
- **§8.4 is stale for P0-05 and P0-14.** Both now have standing gates in the
  ordinary suite.
- **F1's `compare` is declared but unpopulated.** S10 does not need to touch 21
  descriptors, but it does need to answer D3 before it can populate one.
- **The plan's exit criterion for S2 names `regression_in_is_classified…` at
  `kind.rs:457` and similar line numbers throughout.** Line numbers across
  `praxis-runtime`, `praxis-parser`, `praxis-syntax`, `praxis-source`,
  `praxis-mir`, `praxis-codegen-cranelift`, `praxis-debugger` and `praxis-cli`
  have all moved. Search by test name, not by line.
