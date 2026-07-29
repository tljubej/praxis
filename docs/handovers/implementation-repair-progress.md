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
| S3 — ABI manifest, MIR representation, fault values | **done** | `8a30db1`, `7075017`, `91b64c1`, `1f7eca7` |
| S4 — Object layout and heap provenance | **done** | `dfe42b6` |
| S5 — Root-set completeness, native RAII roots | **done** | `7911337` |
| S6 — Allocation pacing, effect metadata, heap lifecycle | **done** | `7182d56`, `ce08ae3`, `b384df9`, `968af35`, `3b5bfb7` |
| S7 — Descriptor totality, typed collections, fault representation | **done** | `d014067`, `c00aab7`, `6da6037`, `eda8c69` |
| S8 — Generation arena for JIT and plan metadata | **done** | `1311132`, `b60da0a` |
| S9 — MIR root exactness, debug/root split, verifier | **done** | `ad9bbdf`, `d9521f9`, `e15a444` |
| S10 — Semantic comparison, nominal schema identity | **done** | `29ff4f6`, `83de924`, `510ffc3`, `ff35f68` |
| S11 — TypeDb core: levels, schemes, nominal identity | **done** | `8aa9069`, `aa9deea`, `d69881e`, `5efd0e2` |
| S12 — Parser grammar: wildcard, separators, struct-literal suppression | **done** | `4504a1d`, `0c4b2ce`, `e49a803`, `13db789` |
| S13 — Annotations honored, declaration passes, mutability and scope | **done** | `6037662`, `9129f76`, `4f45455`, `9c6b48a`, `6fd5e46`, `de6b0e2` |
| S14 — Control flow: bottom type, contexts, joins, loop values | **done** | `fd909a1`, `92a1b84`, `bf91879`, `9cffbe5`, `f93b25f`, `ea506a6` |
| S15 — Per-use types into HIR and MIR, then monomorphization | **done** (one exit criterion deferred; see §4) | `93c05ec`, `f3c76a8`, `77b2ad6`, `b560e67` |
| S16 — Records, patterns, exhaustiveness, enum constructors | **done** | `57e2e5b`, `06f3c44`, `b8e2c7b`, `e918741` |
| S17 … S21 | not started | |

Also closed out of order: **DBG-01** (`3836b74`), a P0 the plan schedules in
S10, and **MONO-03** (S15) — F12's `TypeKey` *is* its fix, so it closed with
TY-06 rather than waiting for the stage that owns it. **DBG-02** is closed in
part (see §6).

Baseline at `136ce4b` was **928 passed, 0 failed, 149 ignored**.
Now: **1264 passed, 0 failed, 53 ignored**. `just ci` is green.

**S11 is closed.** All five exit-criterion tests pass and all eight findings the
stage owns are fixed and gated — TY-01…TY-07 and TY-22.

**S12 is closed.** All four findings are fixed — FE-02, FE-04, FE-06, DBG-03 —
and all six exit-criterion tests pass. D7 and D8 are answered *and implemented*
(ADR-049); FE-06's rule is ADR-050.

**S13 is closed.** All ten findings are fixed — F2, TY-08…TY-15, TY-23, TY-24 —
and all fourteen exit-criterion tests pass. D13 was answered before the stage
opened (ADR-051); the stage's own decisions are ADR-052. TY-12 needed no code:
it is S11's `Y007`. The corpus triage pass the plan mandates found **nothing**
newly rejected — see §4.

**S14 is closed.** All six findings are fixed and gated — TY-16…TY-21 — and all
six exit-criterion tests pass, plus the fourth criterion that is a MIR change
rather than a test. **D2 is answered *and implemented*** (ADR-053); the stage
also amended ADR-051 for `Y017`.

**S16 is closed.** All five findings are done — **HIR-03**, **HIR-04**,
**HIR-05**, **HIR-06** and **HIR-07** — and all eight exit-criterion tests pass.
HIR-05 needed no code; the plan predicted that, and it was right. The stage's
ADR is **055**.

Eighty-four of the audit's ignored regressions are un-ignored and passing.
The seven added by S16:

| Test | File | Finding |
|---|---|---|
| `lowering_respects_a_local_that_shadows_an_enum_variant` | `infer_tests.rs` | HIR-03 |
| `record_literal_requires_every_declared_field` | `infer_tests.rs` | HIR-04 |
| `record_literal_rejects_unknown_fields` | `infer_tests.rs` | HIR-04 |
| `record_literal_rejects_duplicate_fields` | `infer_tests.rs` | HIR-04 |
| `nested_enum_pattern_must_cover_payload_constructors` | `infer_tests.rs` | HIR-06 |
| `duplicate_enum_arm_is_unreachable` | `infer_tests.rs` | HIR-06 |
| `unknown_enum_variant_pattern_is_rejected` | `infer_tests.rs` | HIR-07 |

HIR-06's ten new gates:

| Test | File | Pins |
|---|---|---|
| `a_match_covers_every_payload_position_not_just_the_outer_constructor` | `infer_tests.rs` | HIR-06's first half as the *rule* — a one-variant enum, a wildcard payload, a binding payload, and a gap two payloads deep |
| `an_arm_is_unreachable_exactly_when_it_adds_no_coverage` | `infer_tests.rs` | HIR-06's second half as the rule — a repeated constructor, the old scan's own case, a subsumed payload, an exhausted `Bool`, and that two distinct payload constructors are two *live* arms |
| `a_missing_case_is_named_by_the_value_that_is_missing` | `infer_tests.rs` | the witness is the shape (`Wrap(Off)`), and an open signature still asks for an arm |
| `a_bare_constructor_name_is_that_constructor_at_any_payload` | `infer_tests.rs` | the padding rule — `Some`, `Some(_)` and `Some(n)` are one test (see §4: this one does **not** fail against the unfixed tree) |
| `a_padded_payload_wildcard_selects_its_arm_at_runtime` | `jit.rs` | the plan's explicit warning — MIR's decision tree on a source-reachable wildcard *in a payload slot*, both spellings and both directions |
| `a_covered_constructor_with_an_uncovered_payload_is_not_exhaustive` | `exhaustive.rs` | the same rule at the unit level, plus the message |
| `a_repeated_constructor_adds_no_coverage` | `exhaustive.rs` | unreachability without a catch-all in sight |
| `a_binding_payload_covers_every_constructor_under_it` | `exhaustive.rs` | why `Wrap(_)` then `Wrap(On)` is one dead arm and not one missing case |
| `bool_is_covered_by_its_two_literals` | `exhaustive.rs` | `Bool`'s signature is closed — no `_` needed, and a third arm is dead |
| `an_unreachable_arm_still_counts_as_covered` | `exhaustive.rs` | an arm the checker rejects still enters the matrix |

S16's six earlier gates:

| Test | File | Pins |
|---|---|---|
| `a_constructor_is_a_symbol_kind_not_a_spelling` | `infer_tests.rs` | HIR-03's rule — every variant, including the prelude's `Some`/`None`, is `SymbolKind::EnumVariant` |
| `a_local_holding_a_variant_is_not_a_constructor` | `infer_tests.rs` | why the *kind* and not the scheme: `let A = Empty` has the enum's type too — and that the binding survives |
| `a_record_literal_names_every_field_exactly_once` | `infer_tests.rs` | HIR-04's three halves as three codes, and that field *order* is not the rule |
| `an_unknown_fields_initializer_is_still_checked` | `infer_tests.rs` | why the initializer is inferred anyway — skipping it is what deleted the side effect |
| `a_pattern_that_names_no_variant_is_reported_not_widened` | `infer_tests.rs` | HIR-07 — `Y122` for a typo, `Y123` for a wrong shape, and the right constructors still work |
| `a_non_exhaustive_match_is_reported_where_it_is_written` | `infer_tests.rs` | HIR-07's second half — `Y120` at the `match`, not at byte 0 |

**S15 is closed**, with one exit criterion deferred for a reason that is new
information — §4 has it. All seven findings the stage owns are fixed and gated:
**F20**, **HIR-08** and **HIR-09** landed first, **MONO-03** closed in S11, and
**HIR-01, HIR-02, MONO-01, MONO-02** are **F15**, which *is* the stage. The
ADR is **054**.

Seventy-seven of the audit's ignored regressions are un-ignored and passing.
The six added by S15:

| Test | File | Finding |
|---|---|---|
| `immediately_invoked_closure_boxes_its_mutable_capture` | `infer_tests.rs` | HIR-08 |
| `lowered_polymorphic_call_result_uses_the_callsite_instantiation` | `infer_tests.rs` | HIR-01/MONO-01 |
| `lowered_generic_method_result_uses_the_receiver_instantiation` | `infer_tests.rs` | HIR-01 |
| `specialized_clone_carries_concrete_types_throughout` | `mono.rs` | MONO-01 |
| `zero_argument_generic_result_is_specialized_from_use_context` | `mono.rs` | MONO-02 |
| `empty_vec_float_has_the_float_element_descriptor_before_any_push` | `adversarial_audit.rs` | **S7's** last exit criterion, blocked on F15 |

S15's fourteen new gates:

| Test | File | Pins |
|---|---|---|
| `the_child_walker_reaches_every_expression_position` | `infer_tests.rs` | F20 — a closure in all twenty-five expression field positions, all found |
| `an_immediately_invoked_closure_mutates_the_var_it_captured` | `jit.rs` | HIR-08 end to end; the program returned `0` |
| `a_capture_first_seen_as_an_assignment_target_keeps_its_type` | `infer_tests.rs` | HIR-09's second half — `?T` before, `Int` after |
| `every_expression_node_has_a_recorded_type` | `infer_tests.rs` | F15's totality, over a tree with all twenty-five positions in it |
| `lowering_invents_no_type_for_any_expression_position` | `infer_tests.rs` | the same statement where it matters — no `Y099` out of the pass that had nineteen fresh-variable fallbacks |
| `a_node_key_separates_an_expression_from_the_name_inside_it` | `infer_tests.rs` | why the key is not a `TextRange`: a `PATH_EXPR` and its `Ident` share one |
| `a_lowered_branch_carries_the_join_not_its_first_arm` | `infer_tests.rs` | HIR-01 at the branch points, where lowering answered `Never` for a divergent first arm |
| `a_method_name_is_not_a_name_reference` | `infer_tests.rs` | HIR-02 — absent from `refs`, present in `method_refs`, and the entry is the *result* |
| `hover_over_a_method_name_reports_its_result_type` | `hover_tests.rs` | HIR-02 as the query it broke |
| `hover_over_a_receiver_still_reports_the_binding` | `hover_tests.rs` | …and a same-ranged name is still a name |
| `two_instantiations_of_one_generic_are_two_concrete_functions` | `mono.rs` | MONO-01 as the rule — each clone's *own* type, not just its name |
| `a_zero_argument_generic_is_specialized_per_result_type` | `mono.rs` | MONO-02 — two clones, and the generic original dropped |
| `a_for_bindings_slot_carries_the_iterators_element_type` | `build.rs` | P0-02's half that F15 unblocked |
| `a_closure_and_its_indirect_call_carry_their_types` | `build.rs` | …and the closure value's own `Func` |

`mutable_capture_records_error` (`capture.rs`) is plan §8.2's fourth entry and
was **inverted** rather than deleted: it asserted the WS7a refusal of a mutable
capture, and is now `a_mutable_capture_is_an_ordinary_capture`, which states
what replaced it — the capture is recorded and classified `Var`, which is what
selects `ByCell`.

Seventy-one of the audit's ignored regressions were un-ignored by S14 and
earlier. The six added by S14:

| Test | File | Finding |
|---|---|---|
| `never_branch_coerces_to_the_other_branch_type` | `infer_tests.rs` | TY-19 |
| `expression_before_trailing_statement_is_not_the_block_value` | `infer_tests.rs` | TY-16 |
| `if_without_else_cannot_produce_the_then_value_type` | `infer_tests.rs` | TY-17 |
| `early_return_value_must_match_the_function_result` | `infer_tests.rs` | TY-18 |
| `control_flow_terminators_require_a_legal_enclosing_context` | `infer_tests.rs` | TY-20 |
| `expression_loop_uses_its_break_value_type` | `infer_tests.rs` | TY-21 |

S14's sixteen new gates:

| Test | File | Pins |
|---|---|---|
| `never_is_its_own_type_and_not_a_scalar` | `types_tests.rs` | `Never` has no representation, so it is not a `ScalarType`; and two `Never` slots are one type |
| `join_absorbs_never_from_either_side` | `types_tests.rs` | the absorbing law, both directions, three shapes |
| `join_of_two_ordinary_types_is_still_unification` | `types_tests.rs` | a join is **not** general subtyping — and a variable is *linked*, not joined to a third type |
| `join_all_seeds_with_never_and_reports_which_element_failed` | `types_tests.rs` | the empty join, the all-divergent join, and the failure index |
| `a_divergent_branch_is_absorbed_wherever_branches_meet` | `infer_tests.rs` | either `if` position, nested, and across `match` arms; an all-divergent `match` is `Never` |
| `two_ordinary_branches_still_have_to_agree` | `infer_tests.rs` | the half a widening bug would break |
| `a_blocks_value_is_its_last_statement_and_only_if_it_is_an_expression` | `infer_tests.rs` | TY-16 as the rule, not one rejection |
| `an_else_less_if_is_unit_unless_its_branch_diverges` | `infer_tests.rs` | TY-17's ordinary case, its divergent carve-out, and that a real value with no `else` still mismatches |
| `a_return_is_checked_against_the_function_it_leaves` | `infer_tests.rs` | TY-18 in the shapes the exit test misses — a bare `return`, an unannotated result the returns pin, nesting, and a closure's `return` |
| `a_function_whose_body_diverges_matches_any_declared_result` | `infer_tests.rs` | TY-19 at the function result, which no finding names separately |
| `a_terminator_needs_the_right_enclosing_context` | `infer_tests.rs` | TY-20's two codes, both closure directions, and that five legal positions stay legal |
| `a_loop_is_the_join_of_the_values_its_breaks_carry` | `infer_tests.rs` | TY-21 as the rule — what the type *is*, the bare-`break`-is-`Unit` half, and which loop a `break` belongs to |
| `a_loop_no_break_leaves_is_never` | `infer_tests.rs` | D2's first half, including a loop exited only by `return` |
| `only_a_loop_may_break_with_a_value` | `infer_tests.rs` | D2's second half as `Y017`, that it is *not* `Y012`, and that a bare `break` in a `while`/`for` is untouched |
| `expression_loop_returns_the_value_its_break_carried` | `jit.rs` | TY-21 end to end — the half inference cannot see |
| `a_loop_that_never_breaks_compiles_as_a_diverging_branch` | `jit.rs` | why a `Never`-valued loop gets no result slot (D9) |
| `every_break_writes_the_loop_result` | `jit.rs` | two exits, one slot |

The twelve added by S13's second half:

| Test | File | Finding |
|---|---|---|
| `tuple_parameter_annotation_is_enforced` | `infer_tests.rs` | TY-08 |
| `function_parameter_annotation_is_enforced` | `infer_tests.rs` | TY-08 |
| `tuple_return_annotation_is_enforced` | `infer_tests.rs` | TY-08 |
| `function_typed_record_field_annotation_is_enforced` | `infer_tests.rs` | TY-08 |
| `function_typed_enum_payload_annotation_is_enforced` | `infer_tests.rs` | TY-08 |
| `user_enum_annotation_is_enforced` | `infer_tests.rs` | TY-09 |
| `forward_struct_annotation_is_enforced` | `infer_tests.rs` | TY-10 |
| `value_binding_name_is_not_accepted_as_a_type` | `infer_tests.rs` | TY-11 |
| `malformed_collection_type_arity_is_rejected` | `infer_tests.rs` | TY-12 (no code; `Y007`) |
| `local_var_reassignment_preserves_its_type` | `infer_tests.rs` | TY-13 |
| `reassignment_to_let_is_rejected` | `infer_tests.rs` | TY-14 |
| `compound_assignment_requires_a_numeric_target` | `infer_tests.rs` | TY-15 |

S13's eleven new gates:

| Test | File | Pins |
|---|---|---|
| `every_annotation_position_sees_a_tuple_or_function_type` | `ast_tests.rs` | TY-08 at the level it lived at — six positions, both invisible kinds |
| `only_the_three_type_node_kinds_are_annotations` | `ast_tests.rs` | the kind set is one predicate, and a `TUPLE_EXPR` is not in it |
| `a_tuple_or_function_annotation_is_the_type_it_writes` | `infer_tests.rs` | the half the exit tests cannot see — a fresh variable rejects nothing, so "is rejected" is not "arrived" |
| `a_parenthesized_annotation_is_the_type_it_groups` | `infer_tests.rs` | `(Int)`, whose `Ident` is in a nested node |
| `a_nullary_function_annotation_takes_no_arguments` | `infer_tests.rs` | `()` is `Unit`, so `() -> Int` is nullary |
| `a_user_type_annotation_is_the_type_it_names` | `infer_tests.rs` | TY-09 stated positively, and nested inside a collection |
| `a_type_declaration_is_registered_after_the_types_it_names` | `infer_tests.rs` | TY-10 is *dependency* order, not "types first" |
| `a_type_declaration_cycle_still_analyzes` | `infer_tests.rs` | the rounds terminate |
| `a_value_in_type_position_is_reported_as_a_value` | `infer_tests.rs` | TY-11 — every value kind, at any depth, as `N003` |
| `every_kind_of_type_name_is_still_accepted_in_type_position` | `infer_tests.rs` | …and the half a kind check breaks silently |
| `an_assignment_constrains_the_local_it_names_and_no_other` | `infer_tests.rs` | TY-13's other half: the wrong symbol, not just a missing one |
| `only_a_var_may_be_assigned` | `infer_tests.rs` | TY-14 across four immutable binding kinds, as `Y009` |
| `a_compound_assignment_needs_a_numeric_target` | `infer_tests.rs` | TY-15's rule is "numeric", not "not `Bool`" |

The five added by S12's second half:

| Test | File | Finding |
|---|---|---|
| `regression_same_line_statements_require_a_semicolon` | `parse.rs` | FE-04 |
| `regression_semicolons_separate_top_level_statements` | `parse.rs` | FE-04 |
| `regression_newline_terminates_a_bare_return` | `parse.rs` | FE-04 |
| `regression_parenthesized_record_literal_is_valid_in_a_condition` | `parse.rs` | FE-06 |
| `regression_match_arm_may_return_a_record_literal` | `parse.rs` | FE-06 |

F8/FE-04's twelve new gates:

| Test | File | Pins |
|---|---|---|
| `a_token_records_whether_a_line_break_precedes_it` | `praxis-syntax/src/lib.rs` | the fact rides on the token, not the trivia |
| `only_the_first_token_on_a_line_is_preceded_by_a_newline` | `lex.rs` | the flag is consumed, not sticky |
| `same_line_tokens_report_no_newline` | `lex.rs` | what the separator check has to be able to see |
| `a_line_break_anywhere_in_the_trivia_run_counts` | `lex.rs` | a comment after the break must not hide it |
| `a_line_comment_ends_the_line_it_is_on` | `lex.rs` | …including when EOF eats the `\n` |
| `an_operator_continues_across_a_line_break` | `parse.rs` | D8's "never inside the Pratt loop", stated |
| `a_method_chain_continues_across_a_line_break` | `parse.rs` | …and for postfix |
| `a_newline_terminates_a_bare_break` | `parse.rs` | `break`'s half; the exit test covers `return` |
| `a_semicolon_separates_two_statements_on_one_line_in_a_block` | `parse.rs` | the `;` still works where it already did |
| `each_missing_separator_is_reported_once_and_parsing_continues` | `parse.rs` | one `P002` per run-on, no cascade |
| `a_block_demands_a_separator_but_its_closing_brace_is_one` | `parse.rs` | the block loop, both ways |
| `match_arms_on_one_line_need_a_comma` | `parse.rs` | the arm loop's comment made true |

FE-06's three new gates:

| Test | File | Pins |
|---|---|---|
| `every_bracket_restores_record_literals_inside_a_condition` | `parse.rs` | six bracket shapes, not just the exit test's parens |
| `a_match_arm_allows_a_record_literal_at_any_depth` | `parse.rs` | block, closure and nested `if` inside an arm |
| `a_keyword_head_still_claims_its_brace_as_a_block` | `parse.rs` | the property the flag existed to provide, in all four heads |

DBG-03's one new gate:

| Test | File | Pins |
|---|---|---|
| `two_unusable_names_do_not_collide_into_one_parameter` | `evaluate.rs` | the `_x` collision, through `collect_bindings` and `synthesize` |

F2's two new gates:

| Test | File | Pins |
|---|---|---|
| `every_code_is_distinct` | `praxis-source/src/diagnostic.rs` | the question the old scheme could not ask |
| `all_lists_every_variant` | `praxis-source/src/diagnostic.rs` | a variant missing from `ALL` is one the first test never checks |

The two un-ignored by S13's first half:

| Test | File | Finding |
|---|---|---|
| `analyzing_nested_function_never_panics` | `infer_tests.rs` | TY-23 |
| `duplicate_function_declarations_are_rejected` | `infer_tests.rs` | TY-24 |

TY-23/TY-24's three new gates:

| Test | File | Pins |
|---|---|---|
| `a_nested_function_is_reported_as_one` | `infer_tests.rs` | the exit test only asks that `analyze` returns; `N005` is the report |
| `a_duplicate_function_is_reported_once_and_the_first_one_survives` | `infer_tests.rs` | one `N004`, no cascade of `N001`s |
| `shadowing_an_outer_name_is_not_a_redeclaration` | `infer_tests.rs` | why the check is `is_bound_here` and not `lookup` |

`sanitize_rejects_digit_leading_and_punct` is plan §8.2's second entry and had to
be **rewritten**: it asserted the `_x` rewrite itself. It is now
`an_unusable_local_name_is_rejected_rather_than_rewritten`, which states the
property that replaced it — a name is usable as written or its local is dropped,
and there is no third name. `token_carries_kind_and_span` (praxis-syntax) was
**amended** for the new field.

The three added by S12's first half:

| Test | File | Finding |
|---|---|---|
| `enum_payload_types_participate_in_monomorphization_cache_key` | `mono.rs` | MONO-03 (F12) |
| `regression_lone_underscore_has_its_dedicated_token_kind` | `lex.rs` | FE-02 |
| `wildcard_pattern_does_not_bind_a_value_named_underscore` | `infer_tests.rs` | FE-02; **rewritten**, see below |

FE-02's two new gates:

| Test | File | Pins |
|---|---|---|
| `an_underscore_inside_a_name_is_still_an_identifier` | `lex.rs` | the split is on the whole run, not the first byte |
| `a_wildcard_binder_is_legal_and_declares_nothing` | `infer_tests.rs` | D7's three positions — legal, silent, and the initializer still runs |

`wildcard_pattern_does_not_bind_a_value_named_underscore` had to be
**rewritten**: it asserted `has_name_error`, which was the only failure
available while `_` lexed as an `Ident` (the arm body was a reference to an
undeclared name). `_` now has no expression form at all, so the parser rejects
it where it stands — same property, different category. This is the fourth time
in the repair a fix has changed *which* check catches a mistake; expect it.

F12's seven new gates:

| Test | File | Pins |
|---|---|---|
| `instantiating_a_scheme_does_not_mint_a_nominal_definition` | `types_tests.rs` | **TY-06**, stated at the level it lived at |
| `an_instantiated_option_is_the_canonical_def_at_a_fresh_argument` | `types_tests.rs` | …and that the instance is still usable |
| `every_option_names_the_one_option_def` | `types_tests.rs` | TY-06 — **rewritten** from `same_named_enums_unify_structurally`, which asserted the workaround |
| `option_instances_unify_through_their_arguments` | `types_tests.rs` | **rewritten** from `same_named_enums_unify_payloads_pairwise` |
| `option_at_two_element_types_is_two_types` | `types_tests.rs` | the distinction `render` could not make |
| `a_variant_payload_is_read_through_the_instances_arguments` | `types_tests.rs` | the def holds `T`; the use holds the argument |
| `a_canonical_key_groups_by_structure_and_by_definition` | `types_tests.rs` | `TypeKey` is identity where `render` was display |
| `a_wrong_type_argument_count_is_unconstructible` | `types_tests.rs` | TY-07's rule, extended to nominal defs |
| `monomorphization_distinguishes_option_element_types` | `jit.rs` | MONO-03 end to end — two clones, one program |

`empty_enum_payload_and_no_payload_are_equivalent` and `enum_def_variant_lookup`
were **amended**: the first built two same-named nominal enums (F12 makes them
two types — it now uses anonymous ones, which is the arm that survives), the
second read `db.enum_defs[0]` positionally (index 0 is now the prelude
`Option`). `a_wrong_type_argument_count_is_reported_at_the_annotation` gained
an `Option[Int, Text]` case.

The four added by S11's first half:

| Test | File | Finding |
|---|---|---|
| `instantiation_preserves_non_quantified_variable_identity` | `types_tests.rs` | TY-02 (F9) |
| `deep_resolve_rewrites_record_field_links` | `types_tests.rs` | the `_ => t` half of F9 — and TY-04 with it |
| `empty_enum_payload_and_no_payload_are_equivalent` | `types_tests.rs` | TY-05; **rewritten**, see below |
| `linking_an_outer_var_to_an_inner_type_prevents_inner_generalization` | `types_tests.rs` | TY-01 |
| `forward_call_is_checked_against_later_function_signature` | `infer_tests.rs` | TY-22 |

`empty_enum_payload_and_no_payload_are_equivalent` had to be **rewritten**, not
merely un-ignored: it compared `None` against `Some(vec![])`, and TY-05's fix is
that only one of those spellings exists. It now asserts the property at both
levels the bug lived at — the representation admits one payload-less spelling,
and the two constructors that used to disagree produce defs that unify.

S11's six new gates for the findings that had none:

| Test | File | Pins |
|---|---|---|
| `a_wrong_arity_collection_is_unconstructible` | `types_tests.rs` | TY-07 — **rewritten** from `unify_vec_mismatched_arity_fails`, which built the type the fix makes unrepresentable |
| `degenerate_tuples_and_duplicate_names_are_unconstructible` | `types_tests.rs` | TY-07's other two shapes |
| `a_wrong_type_argument_count_is_reported_at_the_annotation` | `infer_tests.rs` | `Y007` — named where it was written, not as a downstream `Y001` |
| `a_duplicate_field_or_variant_is_rejected` | `infer_tests.rs` | `Y008` — silently accepted before |
| `a_scheme_owns_its_binders_and_generalization_mutates_nothing` | `types_tests.rs` | TY-03 — **rewritten** from `generalized_var_state_is_marked` (plan §8.2) |
| `generalizing_one_scheme_does_not_change_another` | `types_tests.rs` | TY-03 stated directly: the invalid state a `Scheme` could encode |

F9's six new gates:

| Test | File | Pins |
|---|---|---|
| `an_identity_fold_returns_the_same_handles_and_interns_nothing` | `fold.rs` | TY-02's identity preservation |
| `a_changed_leaf_rebuilds_the_types_that_contain_it` | `fold.rs` | …and that it still rebuilds when it must |
| `records_and_enums_are_folded_through_their_defs` | `fold.rs` | the two arms every walk skipped |
| `an_unchanged_record_keeps_its_def` | `fold.rs` | no specialized def when nothing specialized |
| `a_shared_child_is_folded_once` | `fold.rs` | the memo, on the sharing case |
| `a_record_that_contains_itself_terminates` | `fold.rs` | the memo, on the cycle case |

The five added by S10's second half:

| Test | File | Finding |
|---|---|---|
| `float_heap_entries_use_numeric_order` | `heaps.rs` | P0-12 |
| `text_ordering_is_lexicographic_without_payload_reinterpretation` | `adversarial_audit.rs` | P0-12 |
| `ordering_rejects_bool_operands` | `infer_tests.rs` | P0-12 |
| `ordering_rejects_function_operands` | `infer_tests.rs` | P0-12 |
| `ordering_rejects_composites_without_a_matching_runtime_lowering` | `infer_tests.rs` | P0-12 |

S10's sixteen new gates:

| Test | File | Pins |
|---|---|---|
| `float_compare_is_numeric_with_nan_last` | `scalars.rs` | ADR-045 D2 — NaN last, ±0.0 agreeing with `equals` |
| `scalar_compare_reads_its_own_payload_width` | `scalars.rs` | the four-byte `Char`, the unsigned `Byte` |
| `bool_and_unit_declare_no_ordering` | `scalars.rs` | ADR-045 D1's absence, stated |
| `text_compares_lexicographically_whatever_its_representation` | `text.rs` | owned and slice `Text` order the same |
| `char_heap_entries_order_by_unicode_scalar_value` | `heaps.rs` | the non-ASCII case the input parser cannot yet deliver |
| `entries_of_different_types_do_not_dispatch_a_callback` | `heaps.rs` | ADR-045 D3 in the heap |
| `an_unorderable_element_type_compares_equal_rather_than_reading_bytes` | `heaps.rs` | the degenerate-but-consistent order |
| `a_text_heap_pops_in_lexicographic_order` | `heaps.rs` | the heap really uses the descriptor |
| `text_equality_compares_bytes_not_the_payload_discriminant` | `adversarial_audit.rs` | P0-12's equality half (see §6) |
| `text_comparison_never_extracts_a_scalar_from_the_payload` | `adversarial_audit.rs` | the lowering choice, not just the answer |
| `small_scalars_are_extracted_at_their_own_width` | `adversarial_audit.rs` | no eight-byte read of a `Char`/`Bool` |
| `records_from_two_generations_are_equal_when_their_type_is` | `adversarial_audit.rs` | RT-12 end to end, two generations one heap |
| `anonymous_records_of_one_shape_are_equal_across_schema_allocations` | `records.rs` | RT-12 — the plan's first named gate |
| `nominal_records_of_different_types_are_never_equal` | `records.rs` | RT-12 — the plan's second, plus nominal ≠ anonymous |
| `one_nominal_name_over_two_shapes_is_two_types` | `records.rs` | why `same_type` checks the shape as well as the name |
| `heap_element_orderability_agrees_with_the_runtime` | `infer_tests.rs` | rewritten; the capability and the descriptor agree |

The three added by S9:

| Test | File | Finding |
|---|---|---|
| `local_dead_after_its_last_use_is_not_rooted_at_a_later_safepoint` | `liveness.rs` | MIR-02 |
| `exact_roots_shrink_between_two_safepoints_in_one_block` | `liveness.rs` | MIR-02 |
| `empty_element_returning_sinks_fault_instead_of_returning_uninitialized_gc_refs` | `adversarial_audit.rs` | MIR-09 |

S9's fifteen new gates:

| Test | File | Pins |
|---|---|---|
| `a_slot_spilled_at_one_safepoint_is_nulled_once_its_local_dies` | `liveness.rs` | MIR-01 — the `dead` set exists and is computed |
| `the_live_and_dead_sets_of_a_safepoint_are_disjoint` | `liveness.rs` | a slot is spilled or nulled, never both |
| `the_debug_set_still_shows_what_the_root_set_dropped` | `liveness.rs` | MIR-16 — H3's property, stated where it lives |
| `check_fault_carries_an_annotated_debug_set` | `liveness.rs` | the debugger safepoint that is not a GC one |
| `a_dead_local_stops_being_reachable_from_its_frame` | `adversarial_audit.rs` | MIR-01 end to end — 3000 elements that used to survive |
| `an_unannotated_set_is_empty_and_says_so` | `annot.rs` | the seal |
| `an_annotated_empty_set_is_not_an_unannotated_one` | `annot.rs` | "nothing is live" ≠ "the pass never ran" |
| `a_scalar_local_in_the_root_set_is_rejected` | `verify.rs` | the plan's first named verifier gate |
| `an_out_of_range_jump_target_is_rejected` | `verify.rs` | the plan's second (MIR-11's class) |
| `a_move_gc_out_of_a_scalar_is_rejected` | `verify.rs` | P0-03, now enforced rather than documented |
| `an_unannotated_safepoint_is_rejected` | `verify.rs` | what the seal buys the verifier |
| `returning_a_scalar_is_rejected` | `verify.rs` | the ABI returns a `GcRef` |
| `a_bounded_division_is_rejected` | `verify.rs` | `sdiv` traps; no bound rules out a zero divisor |
| `an_out_of_range_operand_is_rejected` | `verify.rs` | the cheap builder-bug shape |
| `an_annotated_function_verifies` | `verify.rs` | the rules are satisfiable |

`adv_pipeline_empty_source_reduce` (`jit.rs`) was **rewritten**, not
un-ignored: it asserted only that the host survived, because there was no
contract to assert. There is one now.

RT-16's five gates (S10's only landed finding; it had none, per plan §8.3):

| Test | File | Pins |
|---|---|---|
| `map_formatting_does_not_follow_hash_table_order` | `maps.rs` | RT-16 — the same map renders one way |
| `set_formatting_does_not_follow_hash_table_order` | `maps.rs` | RT-16 |
| `counter_formatting_does_not_follow_hash_table_order` | `maps.rs` | RT-16 |
| `heap_formatting_does_not_depend_on_insertion_order` | `heaps.rs` | RT-16 — and asserts the two backing arrays really differ |
| `a_min_heap_renders_smallest_first` | `heaps.rs` | pop order through `Reverse` |

The one added by S8:

| Test | File | Finding |
|---|---|---|
| `record_schema_cache_is_scoped_by_type_database_not_bare_def_id` | `adversarial_audit.rs` | MIR-12, DBG-06 |

S8's fifteen new gates, for the four findings that had none:

| Test | File | Pins |
|---|---|---|
| `the_same_def_id_in_two_generations_gets_two_schemas` | `generation.rs` | MIR-12/DBG-06 at the unit level |
| `one_def_id_in_one_generation_is_one_schema` | `generation.rs` | the sharing the fix must *not* break |
| `a_failed_schema_build_caches_nothing` | `generation.rs` | a D9 refusal leaves no half-schema |
| `tuple_schemas_are_shared_by_shape` | `generation.rs` | structural keying survives the move |
| `repeated_identical_metadata_stops_growing_the_arena` | `generation.rs` | DBG-05/MIR-13 — interning, not just reclaiming |
| `a_retired_generation_releases_its_arena` | `generation.rs` | H15 — `retire` needs the proof to compile |
| `a_shared_generation_survives_a_partial_retire` | `generation.rs` | a second handle keeps its pointers |
| `generation_ids_are_distinct_and_nonzero` | `generation.rs` | the key half that is not the def id |
| `repeated_evaluation_stops_growing_the_generation` | `evaluate.rs` | DBG-05 through the *real* `p` path |
| `zero_is_not_a_plan_id` | `plan.rs` | IP-12 — the sentinel has no encoding |
| `registered_plans_round_trip_through_their_raw_id` | `plan.rs` | IP-12 — the MIR immediate round-trips |
| `an_unregistered_id_resolves_to_nothing` | `plan.rs` | IP-12 — out of range is `None`, not an index |
| `a_compiled_plan_owns_its_interned_strings` | `plan.rs` | IP-12 — the arena really owns them |
| `registration_past_the_bound_is_refused` | `plan.rs` | IP-12 — bounded, and refuses before pushing |
| `retiring_parser_plans_empties_the_arena` | `teardown.rs` | IP-12 — plans and schemas go together |

The thirteen added by S7's second half:

| Test | File | Finding |
|---|---|---|
| `dynamic_keys_with_different_descriptors_are_never_equal` | `praxis-runtime/src/dynamic_key.rs` | RT-09 |
| `vec_push_rejects_a_value_with_the_wrong_descriptor` | `praxis-runtime/src/abi.rs` | P0-11 |
| `grid_cell_vectors_preserve_the_grid_element_descriptor` | `praxis-runtime/src/abi.rs` | P0-11 |
| `grid_position_vectors_use_the_point_tuple_descriptor` | `praxis-runtime/src/abi.rs` | P0-11 |
| `constructed_grid_cells_satisfy_the_declared_element_descriptor` | `praxis-runtime/src/abi.rs` | P0-11 |
| `grid_positions_vec_uses_the_point_tuple_descriptor` | `adversarial_audit.rs` | P0-11 |
| `grid_text_row_preserves_the_grid_cell_descriptor` | `adversarial_audit.rs` | P0-11 |
| `tuple_schema_uses_the_enum_descriptor_for_enum_elements` | `adversarial_audit.rs` | P0-11 |
| `tuple_schema_uses_the_unit_descriptor_for_unit_elements` | `adversarial_audit.rs` | P0-11 |
| `nested_record_inequality_dispatches_to_the_record_descriptor` | `adversarial_audit.rs` | P0-11 |
| `regression_runtime_vec_descriptor_recovers_its_real_element_type` | `praxis-debugger/src/evaluate.rs` | DBG-02 |
| `empty_vectors_with_different_element_types_are_not_equal` | `praxis-runtime/src/collections.rs` | RT-10 |
| `tuple_equality_uses_shape_not_schema_allocation_identity` | `praxis-runtime/src/tuples.rs` | RT-11 |

`tuple_schema_uses_the_unit_descriptor_for_unit_elements` needed its **program**
rewritten, not its assertion: `let unit = { let ignored = 1 }\n  (unit, 7)`
parses as a *call* of `(unit, 7)`, which is FE-04 (S12). Any test whose source
puts a parenthesized expression at the start of the line after a `let` hits
this — bind the tuple to a name and return the name.

**One exit-criterion test could not be un-ignored.**
`empty_vec_float_has_the_float_element_descriptor_before_any_push` is not
blocked on P0-11: `let values: Vec[Float] = Vec()` reaches codegen with the
element type still a variable, so the descriptor is legitimately null. It no
longer *aborts the test process* on a null deref — it reads the descriptor as
an `Option`. S7 read the cause as TY-08 and S13 disproved that: inference does
apply the annotation (pushing an `Int` is a `Y001`); `lower_call`
re-instantiates the callee's scheme instead of using the call site's inferred
type. It is **F15/MONO-01 in S15**, and the `#[ignore]` reason now says so.

The eight new gates for the findings that had none:

| Test | File | Pins |
|---|---|---|
| `a_mismatched_key_never_dispatches_the_equality_callback` | `dynamic_key.rs` | RT-09 — the callback must not run at all |
| `keys_of_different_types_are_unequal_in_a_real_hash_set` | `dynamic_key.rs` | RT-09 through a real `HashSet` |
| `a_bit_outside_the_representable_range_has_no_index` | `bitset.rs` | RT-07 — the resize is unwritable |
| `a_negative_or_absurd_grid_extent_faults_instead_of_allocating` | `abi.rs` | RT-07 extents |
| `an_in_range_grid_extent_still_builds_its_cells` | `abi.rs` | RT-07's other side |
| `a_bitset_member_outside_the_representable_range_faults` | `abi.rs` | RT-07 members |
| `bitset_queries_outside_the_range_are_absent_rather_than_faults` | `abi.rs` | RT-07 — queries stay total |
| `neighbors_of_an_extreme_point_are_empty_rather_than_a_panic` | `abi.rs` | RT-07 neighbour overflow |
| `a_known_element_type_with_no_descriptor_fails_the_compile` | `lower.rs` | P0-11's D9 answer |

Plus the seven crate tests in `praxis-repr/src/tests.rs`, of which
`every_builtin_value_round_trips` is F11's stated contract: a live sample of all
twenty-one built-ins, round-tripped by descriptor **pointer**.

Two tests were **rewritten**, not merely un-ignored, because S6 inverted their
premise:

- `reset_restores_collection_pacing` grew the threshold by calling
  `collect_with`, and an explicit collection no longer grows it (RT-04). It now
  drives a real paced collection through the new test-only
  `Heap::maybe_collect_with`.
- `setting_none_cannot_create_a_pending_fault` was built on
  `fault.set(FaultKind::None)`, which no longer compiles (RT-17 made `set` take
  a `RaisedFault`). It now pins the property where it still lives —
  `RaisedFault::new` rejecting `None`, every other kind round-tripping.

Earlier, thirteen findings-without-gates got one, all checked against the
unfixed code:

| Test | Pins |
|---|---|
| `predicate_wrappers_return_bool_singletons_and_allocate_nothing` | RT-03's other 24 sites |
| `every_scalar_boxing_wrapper_paces_the_collector` | P0-08b, verified to fail against the unfixed `praxis_text_len` |
| `a_reclaimed_block_is_reused_for_the_next_object_of_its_layout` | RT-01's re-heading |
| `reset_discards_the_free_list` | RT-01 vs. arena teardown |
| `dropping_heap_finalizes_reachable_payloads_too` | RT-02 — rooting must not exempt |
| `resetting_then_dropping_finalizes_each_payload_once` | RT-02 double-finalize |
| `a_snapshot_may_be_dropped_after_the_runtime_it_names` | H8 (in `crash_snapshot.rs`) |
| `an_explicit_collection_does_not_grow_the_pacing_threshold` | RT-04 half two |
| `pacing_charges_the_bytes_a_payload_owns` | RT-04 half one |
| `a_source_slice_text_is_charged_nothing_beyond_its_block` | RT-04, the double-count it must not do |
| `alloc_char_rejects_a_negative_code_point` | RT-18, the other end of the range |
| `alloc_text_reports_invalid_utf8_as_its_own_fault_kind` | RT-17's second `None` caller |
| `an_out_of_range_or_non_boundary_slice_is_unconstructible` | RT-06 (in `text.rs`) |

(Earlier nine, from S3/S5: `closure_capture_indices_never_flow_through_gc_locals`,
`call_result_locals_retain_their_inferred_static_types`,
`pipeline_runtime_call_destinations_retain_vec_and_unit_types`,
`automatic_gc_roots_the_ambient_input_buffer`,
`automatic_gc_roots_parse_failure_partial_values`,
`automatic_gc_roots_runtime_owned_crash_snapshots`,
`nested_allocating_helpers_root_intermediate_results`,
`bool_and_unit_abi_allocations_reuse_runtime_singletons`,
`reset_restores_collection_pacing`; their no-gate findings were P0-02, P0-03
and DBG-04. Earlier three: `overaligned_payload_accessor_matches_initialized_address`,
`foreign_heap_root_cannot_delay_reclamation`,
`fault_epilogue_returns_the_valid_unit_sentinel`. Earlier nine:
`builtin_type_ids_are_globally_unique`,
`regression_unicode_identifier_may_start_with_a_unicode_scalar`,
`regression_in_is_classified_consistently_with_the_keyword_table`,
`regression_postfix_forms_may_be_interleaved`,
`regression_formatting_does_not_delete_comments`,
`for_continue_targets_the_increment_block_not_the_header`,
`missing_explicit_input_file_is_a_usage_error`, `float_sign_of_zero_is_zero`,
`regression_runtime_scalar_descriptors_recover_their_actual_types`. Search by
test name — line numbers throughout the plan have all moved.)

## 2. Foundations: what actually landed

Partial foundations are the main trap for a fresh context.

**F15 — the per-node inferred-type map + `MethodRef`: landed whole.**
`praxis_hir::NodeKey` is `(TextRange, SyntaxKind)` and a distinct *type* from a
token range, so the `PATH_EXPR`-node-vs-`Ident`-token collision with `ref_types`
is unrepresentable. `Analysis::expr_types` holds **every** inferred expression's
type; `Analysis::method_refs` holds each method call's catalog entry, receiver
and result, keyed by the method-name token. `CallSite` gained a `result` field.
Lowering is a pure reader: `symbol_type`, `call_result_type` and `param_type`
are gone with the five `db.instantiate` re-instantiations and **all nineteen**
`db.fresh_var()` fallbacks — a miss is `Y099` and never a variable. Every type
lowering reads is **deep-resolved** (memoized), so the typed tree is concrete to
its leaves. `mono::specialize` substitutes the scheme's own binders. See
ADR-054. **Not done:** nothing F15 itself sketches — but note the two *extra*
insertion points §3 records, and that a `TypedStmt` walker is still not needed.

**F20 — the one derived `TypedExpr` child walker: landed whole.**
`TypedExpr::{children, children_mut, blocks, blocks_mut}` are generated by one
`macro_rules!` whose rows list each variant's child fields exactly once; the
match is exhaustive, so a new variant is a compile error rather than a silent
omission. Both surviving walkers — `mono::rewrite_expr` and `build.rs`'s
`collect_closures_expr` — are folds over it, each keeping only the one thing it
does. The *third* walk (escape analysis) is **deleted**, not converted: HIR-08's
stronger fix is to record `escaping_vars` in `lower_closure`, where the capture
list is already in hand. **Not done:** the plan's `Folded`-style change flag has
no analogue here, and there is deliberately no `children` for `TypedStmt` — the
three statement walks are four lines each and the block/stmt shape is stable.

**F5 — sealed `Type` + validated constructors: landed whole.** `Type` and
`VarId` have private fields; `TypeDb::intern` is `pub(crate)`; the four shaped
constructors take `TupleElems` / `CollectionArgs` / `FieldSet` / `VariantSet`,
and `TypeCtorError` is what refusing looks like. `TypeDb::type_from_raw` is the
one checked route back from a stored `u32`. `EnumVariantDef.payload` is a
`Vec<Type>` (TY-05, which is in F5's own sketch) and `EnumDef.name` is an
`Option<String>` matching `RecordDef.name`, so `anon_record`/`anon_enum` are
gone — `register_record(None, …)` is what they were. See ADR-046. **Not done:**
nothing; F5's `Type(0)` half was already three sites after F16, and all three
are converted.

**F12 — nominal identity (`DefId + args`) + `TypeKey`: the static half landed
whole; the runtime half is S18's.** `TypeData::Record`/`Enum` carry `args`,
`RecordDef`/`EnumDef` carry `params`, `record_type`/`enum_type` take arguments
and are fallible, `TypeDb::option_def()`/`option_of()` are the one canonical
`Option`, and `praxis_types::key::TypeKey` + `canonical_key` are the structural
identity the monomorphizer now keys on. `fold_record`/`fold_enum` branch on
whether the def is generic — arguments for a generic one, field types for a
non-generic one — which is the whole of TY-06. See ADR-048. **Not done,
deliberately:** the runtime half — `EnumSchema`, `EnumPayload`'s schema pointer,
`praxis_alloc_enum`'s new parameter — which is **RT-13 in S18** (plan §5 says so
explicitly, and says RT-13 needs TY-06's canonical `Option` first). `SchemaIdentity`
is untouched; nothing in `praxis-types` is `#[repr(C)]`, so **S11 spent no ABI
bump and S12 starts fresh**.

**F10 — scheme-owned binders + `Level`: landed in part, deliberately.**
`VarState::Generalized` is deleted, `Scheme`'s fields are private
(`binders()` / `body()`), `generalize` mutates nothing, `instantiate`
substitutes by binder membership, and `instantiate_with_mapping` exists for
MONO-01. `Level` is a newtype whose only mutator is `clamp_to`, so TY-01's
reversed comparison is unwritable. **Not done, and it is the larger half:** the
**constraint channel** — `praxis_stdlib::capability::CapKind`,
`praxis_types::constraint::{Capability, Constraint}`,
`TypeDb::take_dischargeable`, `TypeDb::substitute`, and the single exhaustive
`praxis_hir::capability::check`. Its only consumers are S17's TY-25…TY-34 and
RT-08; adding it now is unused surface. See ADR-047.

**F9 — the one `TypeFolder`: landed whole.** `praxis_types::fold` is the only
walk over `TypeData`, its match has no catch-all, and the five hand-written
walks are five folders. It adds a cycle memo and identity preservation, neither
of which any walk had. **Not done:** the plan's `Folded { Unchanged, Rebuilt }`
enum — the change flag is internal to the default arms and needs no public
type; and `fold_record`/`fold_enum` take no `args`, because
`TypeData::Record { def }` has none until F12.

**F1 — built-in type identity: landed whole.** `BuiltinTypeId` is the registry;
`TypeDescriptor::builtin::<P>` is the only constructor for a built-in. `compare`
is populated on the five orderable descriptors and deliberately `None` on the
other sixteen (S10, ADR-045); D3 is answered. See ADR-038.

**F3 — identifier class: predicates, the wildcard split, and the debugger's
consumer.** `praxis-syntax/src/ident.rs` has `is_ident_start` /
`is_ident_continue` / `is_ident`, and the lexer uses them; a **lone** `_` now
lexes as `SyntaxKind::UNDERSCORE` rather than `Ident` (FE-02, D7 — ADR-049), and
`Parser::expect_binder` is the binding position that accepts either.
`praxis-debugger`'s `collect_bindings` asks `is_ident` and **drops** a local it
cannot spell (DBG-03) — `sanitize_name` is deleted, and `praxis-syntax` is a new
dependency of `praxis-debugger`. **Not done:** the `Ident` newtype, and
`praxis-input-parser/src/scan.rs` `split_capture`'s independent ASCII rule
(IP-04, S19) — that is the last of F3's three rules still standing alone.

**F8 — `Token::preceded_by_newline` + `StmtSeparator`: landed whole.** `Token`
carries the flag, set for the whole trivia run in front of it; `StmtSeparator
{ Semicolon, Newline, EndOfBlock }` is what both statement loops must produce;
`Parser::newline_before` is the one reader and it is consulted in three places
only (both statement loops and `starts_expr`). See ADR-049. **Not done:**
nothing — but the plan's `Semicolon(Token)` carries no token (see §6).

**F4 — ABI manifest: landed whole.** `praxis_stdlib::abi` is 137 rows, one per
`praxis_*` wrapper: linker name, `AbiKind` params, `AbiRet`, and an `Effect`
(`Pure` / `Faults` / `Allocates` / `AllocatesAndFaults`).
`praxis_runtime::abi::address` is the one symbol→address table.
`RuntimeSymbol::from_name` + `address` is all `symbols::resolve` is now. See §3
for what this means for a stage that adds a symbol (it is now two lines).

**F6 — GcHeader repack: landed whole.** See ADR-039 and §3.

**F7 — composite root set, native RAII roots and the `Safepoint` token: landed
whole.** `RuntimeRoots` is the only thing `Heap::collect`/`maybe_collect` takes,
`NativeScope`/`Rooted` are how native code roots what it holds, and
`Heap::alloc`/`alloc_with` take a `Safepoint` minted only by `Heap::pace`. See
ADR-040 and §3. **Not done:** `ScopedVec` — no site needed it once the payload
accessors took `Rooted`, and the parser interpreter (the one place the plan
wants it) is S20.

**F17 — `RootSlots`/`DebugSlots` and the `verify` pass: landed, minus two
rules.** `praxis_mir::annot` holds both sets; neither has a public constructor
taking ids, so a builder can only write `unannotated()` and
`liveness::annotate` is the only filler. `Inst::CheckFault` carries **only** a
`DebugSlots` — it allocates nothing, so it roots nothing. `praxis_mir::verify`
runs after `annotate` at all four pipeline sites plus both codegen test
harnesses. **Not done, deliberately:** `ScalarLiveAcrossSafepoint` (it fires on
every `lower_seq_*` accumulator and is harmless there — a scalar is a copy and
cannot dangle) and `OpaqueAtDescriptorSite` (H10 — S15 tried; see §4 for what it
now waits on). See ADR-044 §5.
**Effect-driven safepoints** — the other third of F17 — did not land: a
safepoint is still decided by instruction shape, not by
`CallTarget::Runtime(sym).allocates()`. Nothing needed it, and the shapes agree
today.

**F18 — `Option<GcRef>` debug values and the Unit epilogue: landed whole.**
`DebugLocal.value` is an `Option<GcRef>`, niche-optimized to the same word;
`is_real_ref` and `null_sentinel_ref` are deleted from all three copies. The
fault-epilogue half (`emit_unit_return`) landed in S3.

**F16 — MirType and raw-word elimination: landed, minus `TupleShape`.**
`MirType::{Known, Opaque}` is `Local.ty`, `AllocKind::Tuple.ty` and
`AllocKind::Collection.args`; `Inst::LoadCapture` carries the capture index as
an immediate; `alloc_empty_vec` is an ordinary `AllocKind::Collection`. **Not
done:** `TupleShape` (the validated `Box<[Type]>` that makes arity < 2
unrepresentable) — the fused `enumerate`/`zip` tuples have no real type until
MIR-05 supplies one in S21, so they are `MirType::Opaque` and the backend keeps
its degenerate empty-schema path. `MirType::expect_known`/`MirTypeError` are
also not written: their only consumer is F17's verifier (S9).

**F13 — the generation arena: landed whole, and the `Generation` is the only
owner.** `praxis_codegen_cranelift::generation::Generation` is a `bumpalo::Bump`
plus the record-schema, tuple-schema, string and debug-metadata caches; a `Jit`
holds one behind an `Rc`, declared *after* `module`. Reclamation is
`Generation::retire(rc, HeapDrained)`, and `Runtime::teardown(self)` is the only
minter of a `HeapDrained`. Every allocator interns. `PlanId` is a `NonZeroU32`,
`register_plan` is bounded and fallible, and `retire_parser_plans(&proof)` drops
the plans and the interpreter's schema cache together. See ADR-043. **Not done:**
the `Generation` does not yet own the *enum* schemas (F12/RT-13 has not created
them), and `tuples::POINT` is still a process-static leak with no generation to
hang it on.

**F11 — the `praxis-repr` bridge: landed whole.** New crate, deps
`praxis-types` + `praxis-runtime` + `praxis-stdlib`, no cycle.
`descriptor_for_type` is exhaustive and fallible; `type_for_value` is its
inverse and reads payloads; `element_descriptors_for` is what a collection
constructor asks; `type_for_descriptor` is the value-less case. The payload
reads live in `praxis_runtime::repr::instance_repr`, a total match over
`BuiltinTypeId`, so the bridge never does a raw payload read itself. See
ADR-042. **Not done:** `descriptor_supports(d, cap)` — the capability-agreement
half, whose consumer is F10 (S17).

No other foundation has been started.

## 3. Things that changed under you

Mechanical consequences a fresh context will hit immediately. Items from earlier
sessions are kept — they are still true.

### From this session (S16's HIR-06)

**`exhaustive.rs` is a usefulness matrix, and it takes `&mut TypeDb`.** The two
ad-hoc walks (`uncovered_constructors`, `pattern_catches_all`) are gone. `Y120`
and `Y121` are two calls to one `useful(matrix, q, types)`: an arm is
unreachable when it is not useful against the arms above it, and a match is
non-exhaustive when a bare `_` still is. The `&mut` is because a payload's type
under the scrutinee's arguments comes from `variant_payload_of`, which interns.
See ADR-055.

**A variant pattern now carries exactly one sub-pattern per payload slot.**
`lower_pattern` pads with `TypedPattern::Wildcard` and truncates extras (after
lowering them, so anything wrong *inside* an extra still reports). **If you read
`TypedPattern::EnumVariant.subpatterns`, its length is the variant's arity** —
it used to be however many the source wrote, which is what let
`emit_subpattern_tests` read a payload index the object does not have.

**A `TypedPattern::Wildcard` now reaches MIR in a payload slot from source.**
Before, the decision tree only ever saw one from a synthesized top-level
fallback. It emits an `EnumPayloadGet` it then discards, which is correct and is
gated end to end by `a_padded_payload_wildcard_selects_its_arm_at_runtime`.
**A bare `Some` therefore reads a payload it never used to.**

**`Y120`'s message changed shape.** It names the missing *value*
(``missing `Wrap(Off)` ``), capped at three witnesses, and falls back to
``missing a `_` catch-all arm`` only when the type has no signature to
enumerate. Nothing asserted the old text; if you add a snapshot over it, note
that the witness order is signature order.

**Only enums and `Bool` have a closed signature.** Everything else — including
`Unit`, records and tuples — requires a `_`, which is what the old `_ =>` arm
did for all of them. **Do not add a `Closed` arm for a type without pattern
syntax to enumerate it**: a `_`-less match on it would come out exhaustive with
no arm that can run.

### From an earlier session (S16's HIR-03, HIR-04 and HIR-07)

**A record literal is exact, and `infer_record_lit` is what says so.** Every
declared field exactly once and nothing else — `Y113` missing, `Y114` unknown,
`Y115` duplicate. **An unknown field's initializer is still inferred**, because
it is an expression the program wrote; skipping it is what deleted
`Point { x: 1, typo: side_effect() }`'s call.

**`SymbolKind::EnumVariant` is what a constructor is.** Not `Fn`, which is what
variants used to be bound as, and not a spelling. `Lowerer::enum_variant_of`
takes a `SymbolId` and checks the kind *first* — the scheme cannot answer the
question, because `let A = Empty` has the enum's type too.

**The prelude declares `Option`'s variants**, so `PreludeEntry` carries
`is_variant_ctor` and `seed_prelude` binds `Some`/`None` with the variant kind.
`Inferer::seed_builtin_schemes` filters on `Builtin | EnumVariant`; **if you
narrow that filter, every `Option` program fails with "unresolved user function
`Some`"** — the constructors lose their schemes and lower as ordinary calls.

**`Y122` and `Y123` are spent** — a pattern naming a variant the scrutinee has
not, and a constructor pattern against a type with no variants. Both come from
`lower_pattern`, at the constructor token's range.

**`exhaustive::check` takes the `match`'s span.** `Y120` used to be reported at
byte 0 of the file, for every match in it.

### From an earlier session (S15's F15 — read this if you touch HIR)

**`TypedExpr.ty` is what inference recorded, deep-resolved.** Not a
re-derivation, not an instantiation, not a fresh variable. If you need a
lowered node's type, read it; if you are *writing* a lowering, take it from
`Lowerer::node_ty(node.syntax())` and nothing else.

**A lowering that asks for a type inference never recorded emits `Y099`.**
`DiagCode::InternalMissingType` was allocated in ADR-051 and unused; it is used
now. If you add an expression form, inference must visit it or lowering will
report — which is the point, and is why the two totality gates exist.

**Inference records at three places, not one.** `infer_expr` is the entry the
plan names; the other two are `infer_block_inner` (a branch body, a loop body
and a function body are `infer_block` calls, never `infer_expr` calls) and the
two `PATH_EXPR` nodes nothing evaluates — a call's callee name and a record
literal's head. The callee's entry is deliberately **not** also written to
`ref_types`: hover over a callee shows the binding's generalized scheme, which
`hover_over_out_shows_polymorphic_scheme` pins.

**A `TypedFn`'s `fn_type` is its scheme's *body*, binders and all.** It used to
be a fresh instantiation, whose variables nothing else in the tree mentioned.
Its parameters, its body's expressions and its own type are now one variable
set — which is the whole of MONO-01, because `mono::specialize` rewrites that
set by `substitute_params(t, scheme.binders(), chosen)`. **If you mint a
`TypedFn` anywhere else, use the scheme body.**

**A generic function's `TypedFn` therefore renders with type variables in it.**
That is correct and was true before; they were merely a *different* set from
the ones its own parameters used.

**`infer_fn` unifies the signature placeholder with `(params) -> result`
*before* the body is inferred.** A recursive call used to unify a bare variable,
so `let v = build(n-1); v.push(n)` could not resolve `push` — and lowering hid
it by re-resolving the method later. This is strictly more checking: a recursive
call's arguments are checked where they are written.

**A method call's entry and result come from `Analysis::method_refs`.**
Lowering does not call `catalog::lookup`. If inference could not resolve the
method there is no entry, and lowering reports `Y110` — keeping the receiver and
argument subtrees, which it used to discard along with every closure in them.

**`Lowerer::loop_results` is deleted**, and so is `lower_bin`'s Int-or-Float
heuristic, `lower_if`'s join, `lower_match`'s first-arm read and
`lower_tuple`'s reconstruction. All were recomputations of an answer inference
had. **If you add a branch point, inference joins it; lowering reads it.**

**The method catalog's `enumerate` and `zip` rows are wrong**, and F15 makes it
matter. Both declare `result: Vec[T]` — the receiver's own element type — so
`v.enumerate()` on a `Vec[Int]` types as `Vec[Int]` rather than
`Vec[(Int, Int)]`. Lowering used to re-derive a harmless fresh variable from
that row and now believes it. Nothing reads the wrong type today (the fused
pipeline keeps its result Vec's element `Opaque` on purpose), but **do not
derive a descriptor from a pipeline's result type** until the rows are fixed.
See §4.

### From an earlier session (S15's F20, HIR-08 and HIR-09)

**`TypedExpr::children()` / `blocks()` (and their `_mut` twins) are the walk.**
If you need to recurse over the typed tree, fold over them; do not write a
match. The macro that generates them lists each variant's child fields once, at
the enum's definition site in `lower.rs`. **Adding a variant is a compile
error** there; adding a *field* still needs the row updated, which is why the
gate puts a closure in every position.

**`TypedModule::escaping_vars` is accumulated by the lowerer**, in
`lower_closure`, and `collect_escaping_vars` is gone. `CaptureKind::ByCell` and
membership in `escaping_vars` are one fact with one computation now. **If you
add a capture kind that escapes, record it there.**

**`CaptureAnalysis::errors` and `CaptureError` are deleted.** They carried the
WS7a "mutable capture unsupported" refusal that WS7b lifted; nothing read them
and `Y130` was never emitted. `Y130` remains unallocated and is not in ADR-051.

**A capture's type comes from the reference site, then from the binding.**
`ref_types` first (it is the use-site instantiation), and when the discovering
reference is an assignment *target* — which inference records no type for — the
binding's own scheme, skipping a polymorphic one. Only then a fresh variable,
and F15 is what removes that last fallback.

### From an earlier session (S14's TY-21)

**`Inferer::loop_depth` is now `Inferer::loops`, a stack of `LoopCtx`.** Each
frame carries a `LoopFlavour` — `Expression` for a `loop`, `Statement(keyword)`
for a `while`/`for` — and the **join of every `break` value seen so far**, seeded
with `Never`. `infer_break` joins its value into the top frame; `infer_loop` pops
the frame and answers with what it accumulated. `Inferer::in_loop(flavour, body)`
is the push/infer/pop helper, so a new loop form cannot forget the pop.

**A `loop` is `Never` when nothing `break`s out of it, and `Unit` when a bare
`break` does.** `loop { }` and a loop exited only by `return` produce no value;
`break` with no expression leaves with nothing, which is `Unit`. Mixing `break`
and `break 1` in one loop is therefore a `Y001`, not a coincidence. See ADR-053.

**A value `break` in a `while`/`for` is `Y017`**, a new code (ADR-051 amended).
It is *not* `Y012`: the loop exists, and it is the kind of loop that is wrong.
Only a `loop` is an expression loop, because only it has no non-`break` exit.

**The HIR lowerer keeps the same stack** (`Lowerer::loop_results`), for the same
reason `lower_if` had to join: reading the loop body's type answers what the loop
*repeats*, not what it produces. `lower_closure` clears and restores it, as
inference does. **If you add a loop form, push a frame in both passes.**

**MIR: `LoopCtx` carries `result: Option<LocalId>`.** A `loop` whose type is
neither `Unit` nor `Never` gets a result slot each `break` writes before jumping,
exactly as `lower_if` gives an `if` one. `Unit` keeps the literal it always had;
`Never` deliberately gets **no** slot — it has no runtime representation, so the
local would fail the compile at its descriptor site (D9), and the exit block is
unreachable anyway. `while`/`for` and the two fused-pipeline loops pass `None`.

**The MIR builder no longer tolerates a missing loop context.** `lower_break`,
`lower_continue`, `break_loop` and `continue_loop` read the stack with an
`expect`. The first two are justified by TY-20 (a `break` with no loop is `Y012`,
and no MIR is built for a program that reports); the last two by the fusion
pushing its own frame before it emits a stage. This was the plan's fourth S14
exit criterion.

**`Y017` is the block's ninth spent code**, so the next free number is `Y018`.
ADR-051's "findings that need no code" list no longer contains TY-21.

### From an earlier session (S14's TY-19, TY-16, TY-17, TY-18 and TY-20)

**`Never` is `TypeData::Never`, not `ScalarType::Never`.** The variant is gone
from `praxis_stdlib::type_pattern::ScalarType` — so `PatternScalar::Never` is
gone too — and `db.never()` interns the new variant. Every exhaustive match over
`TypeData` needs a `Never` arm: `fold` (`fold_never`), `canonical_key`
(`TypeKey::Never`), `render`, `praxis_repr::descriptor_for_type` (no runtime
representation), `praxis_hir::catalog::type_to_pattern` (`None` — nothing can be
a receiver of this type) and `praxis_hir::capability` (every capability holds
**vacuously**, including `supports_ord`, so a divergent branch is not what makes
a sort illegal).

**`TypeDb::join(a, b)` is what a branch point uses, and `unify` is unchanged.**
`join` absorbs `Never` from either side and unifies everything else. It is
deliberately *not* general subtyping — `Never` is the only absorbed type,
because it is the only type with no values — so inference stays principal.
`join_all(iter)` is the arm/break-list form: it seeds with `Never` and its error
is `(index, UnifyError)`, naming the element that disagreed.

**Three sites join instead of unifying: `infer_if`, `infer_match`, `lower_if`.**
`lower_if` matters independently — it read the *then*-block's type, so even with
inference happy the typed tree carried `Never` for an `if` whose then branch
diverges. **If you add a branch point, join it.**

**A `match` seeds its result with `Never`, not a fresh variable.** A match whose
every arm diverges is `Never`; it used to be a fresh variable that silently
agreed with whatever the first later use wanted.

**An `if` with no `else` is `join(then, Unit)`.** `if c { 1 }` is now a `Y001`;
`if c { panic("x") }` and `if c { return 1 }` are not, because the divergent
branch is absorbed. This is the ordering the plan insists on — TY-19 *before*
TY-17 — and it is load-bearing, not stylistic.

**A block's value is its last statement, and only if that statement is an
expression.** `infer_block_inner` keeps a *pending* tail that any following
statement demotes, which is `lower_block`'s shape. `{ 1; let x = 2 }` used to
infer `Int` while lowering gave it a `Unit` tail.

**`Inferer::fn_results` is the function-context stack**, innermost last: the
result type of each function whose body is being inferred. `infer_fn` pushes the
declared return type, or a fresh variable that the `return`s and the tail both
constrain when there is no annotation — the result has to exist *before* the
body is inferred, which is why it is a stack rather than a check afterwards. A
closure pushes its own (`infer_closure_body`), because `return` inside one
leaves the closure. **Empty means "not inside a function"**, which is what makes
a top-level `return` a `Y011`.

**`Inferer::loop_depth` is the loop-context counter**, and `Y012` is a
`break`/`continue` with it at zero. **Both boundaries are function boundaries,
not lexical ones:** `infer_closure_body` clears the depth and restores it, so a
`break` inside a closure cannot leave a loop outside it. (TY-21 replaced the
counter with a stack of frames — `Inferer::loops` — and `Y012` is now
`loops.is_empty()`. The rule is unchanged; see this session's entry above.)

**A function body *joins* with its declared result.**
`fn f() -> Int { panic("x") }` used to be a `Y001`; every path diverges, so
there is no value to disagree. If you add a site where a body meets a declared
type, join it.

**`|| expr` does not parse.** `||` lexes as the logical-or token, so a
zero-parameter closure is a `P001`. Pre-existing, unrelated to S14 — but it
bites when writing a test that wants a closure and does not care about its
parameter. Use `|n| …`.

### From an earlier session (S13's TY-08…TY-15)

**`praxis_hir::decl` is new, and it runs before any expression is inferred.**
`declare(...)` registers every top-level `struct`/`enum` in *dependency order*
and mints every top-level `fn`'s signature placeholder, then seals both into a
`TypeEnv` that has no mutator outside the module. `infer_top_stmt` **no longer
dispatches `struct`/`enum`** — if you add a type-declaration form, declare it in
the pass or it will silently have no type. See ADR-052.

**A `TypeRef` is any of three node kinds.** `SyntaxKind::is_type_node` is
`TYPE_REF | TUPLE_TYPE | FN_TYPE`, and `TypeRef::cast` accepts all three
(TY-08). Every accessor — `Param::ty`, `LetStmt::ty`, `VarStmt::ty`,
`FnItem::return_type`, `Field::ty`, `EnumVariant::payload_types` — therefore
*sees* a written tuple or function type where it saw nothing before. **Any test
whose source has one of those annotations now checks it.**

**A parenthesized type is the type it groups, and `()` is `Unit`.** `(Int)` used
to be a fresh variable (its `Ident` lives in a nested `TYPE_REF`, and the reader
only looked at direct tokens), and `() -> Int` used to be a function of one
invented argument. `flatten_param_group` maps `Unit` to no parameters.

**Resolving an annotation lives in `decl::Annotations`, not in `Inferer`.**
`Inferer::resolve_type` delegates. `scalar_from_name`, `collection_from_name`,
`collection_ctor_for`, `tuple_or_degenerate` and `flatten_param_group` all moved
there; `resolve::is_collection_ctor_name` is gone in favour of
`decl::is_type_ctor_name`, which is the same list plus `Option`, written once.

**`NameResolution::type_refs` is new: a name in type position → its symbol.**
Deliberately *not* in `refs` — an annotation name is not a value reference, and
everything that walks `refs` would have had to learn to skip it. Inference reads
`type_refs`; it does not look a type name up itself.

**`lookup_struct_type` and `lookup_enum_type` are gone**, and with them the
`SymbolKind::Struct`-only fallback that made a user `enum` annotation resolve to
nothing (TY-09). A user type in an annotation is `type_env.ty(type_refs[range])`;
a record literal's head is `type_env.ty(refs[range].symbol)`.

**`SymbolKind::BuiltinType` is new**, and `SymbolKind::is_type()` —
`BuiltinType | Struct | Enum` — is the one place the set is written. The prelude
holds `out` *and* `Int`, and only one of them may appear in type position. A
value name there is **`N003`**, not `N002`: the name is known, so calling it
unknown would be a lie (TY-11).

**A variant pattern's constructor is a recorded reference.**
`resolve_pattern_bindings` records it (non-diagnosing — an unknown variant is
`Y122` in S16, not `N001`), and `infer_pattern` reads the symbol out of `refs`.

**Closure parameter annotations are checked.** They never were; `|x: Nope| x`
was silent.

**The `Inferer` has no `ScopeTree` and no `scope: ScopeId` parameter** (TY-13).
It pushed empty child scopes that mirrored nothing, and `infer_assign` was the
only reader — so a local assignment either went unchecked or constrained a
same-named top-level binding instead. Every binding question is answered by
`refs` / `decls` / `type_refs`. `Inference.scopes` still carries the resolver's
tree through to `Analysis`, untouched.

**Only a `var` may be assigned (`Y009`), and a compound assignment needs a
numeric target (`Y010`).** A `let`, a parameter, a `for` binding and a pattern
binding are all immutable. `Y010` fires only when the target's type *resolves*
— an unbound variable is one a later use may still pin, so `fn f(a) { a += 1 }`
is not an error.

**`empty_vec_float_has_the_float_element_descriptor_before_any_push` was not
TY-08's.** Its `#[ignore]` reason said the annotation never reaches the
initializer; it does — pushing an `Int` into `let values: Vec[Float] = Vec()` is
a `Y001`. `lower_call` re-instantiates the callee's scheme instead of using the
call site's inferred type, so the typed tree carries `Vec[?T]`. That is
F15/MONO-01 in **S15**, and the reason now says so.

### From an earlier session (S13's TY-23 and TY-24)

**A nested `fn` is `N005` and a redeclared one is `N004`.** Both used to be
accepted in their own way — the first by panicking in `infer_fn`, the second by
emitting two functions under one JIT symbol. `resolve_fn` reports the nesting
(it asks `Resolver::is_top_level`, i.e. the *tree*, so TY-24's undeclared second
`fn` is not mistaken for a nested one) and `register_top_level` reports the
duplicate.

**`ScopeTree::is_bound_here(scope, name)` is new.** `lookup` walks the parent
chain, so it cannot tell a redeclaration from a shadow — a `let out = 1` finds
the prelude's `out`. Use `is_bound_here` for any "is this already declared
*here*?" question.

**`infer_fn` no longer assumes a `fn` was declared.** A name with no `decls`
entry yields `fn_symbol = None` and the body is still inferred, so a file with a
nested or duplicated function keeps reporting everything else. **If you add a
declaration form, resolution must declare it or inference will silently skip
its symbol** — the `expect` that used to catch that is gone on purpose.

**The first of two duplicate `fn`s survives.** `register_top_level` keeps the
existing binding and skips the second, so calls still resolve. A stage that
changes this to "keep the last" will turn one `N004` into a cascade.

### From an earlier session (S13's F2 — the diagnostic-code registry)

**`DiagnosticCode::new` is `pub(crate)` to `praxis-source`.** You cannot write a
code at a call site any more. `praxis_source::DiagCode` is the closed set;
`DiagCode::X.code()` is the only route to a rendered pair, and adding a
diagnostic means adding a variant **and amending ADR-051**.

**`Diagnostic::new`/`build` take a `DiagCode`.** `Diagnostic` stores it and
renders on demand, so **`d.code()` is unchanged** for every reader (it still
answers a `DiagnosticCode`, with `.category()` and `.number()`). `d.kind()` is
the new accessor and is what to compare against — `d.kind() ==
DiagCode::ExpectedStatementSeparator` beats `d.code().number() == 2`.

**`ValidationError.code` is a `DiagCode`, not a `&'static str`.**
`praxis-input-parser` tagged its errors `"I024"` and `praxis-hir` parsed the
number back out with `unwrap_or(0)`, so a typo in the string was a silent
`I000`. `code_number` is deleted.

**`Inferer::diag(at, number, msg)` is `diag(at, code, msg)`** in
`praxis-hir/src/lower.rs`. Its two callers were the only places `Y110` and
`Y112` existed.

**`praxis-hir/src/diagnostics.rs` is still where the wording lives**, and its
constructors are unchanged apart from the code argument. A new diagnostic goes
there, not at the site.

### From S12's F8/FE-04, FE-06 and DBG-03

**A `Token` has three fields, and `Token::new` takes three arguments.**
`preceded_by_newline` is true iff the trivia run immediately before it contained
a `\n`/`\r` or was a line comment. Three construction sites workspace-wide
(`lex.rs`'s `push`, which is now also what emits EOF, and one crate test).
Trivia *accumulates* the fact; a meaningful token *consumes* it, so only the
first token on a line reports one.

**Two statements need a separator, and `;` is no longer the only one.** Both
statement loops end by calling `Parser::expect_stmt_separator`, which returns
`Some(StmtSeparator)` or emits **`P002`** and returns `None`. The top-level loop
consumes a `;` for the first time. A run-on is one diagnostic and parsing
continues — it does not cascade.

**A newline is consulted in exactly three places**, all of them
`Parser::newline_before`: the two statement loops, and `starts_expr` (which is
`break`/`return`'s optional-value decision). **Never inside the Pratt loop** —
`1 +\n2` is one addition and a `.method()` chain crosses lines. If you add a
grammar rule that wants a newline, ask whether it is a statement boundary; D8
says nothing else is.

**Match arms are separated by a comma or a line break.** The arm loop's
`is_pattern_start` check is still there as the recovery guard, but two arms on
one line with no comma is now a `P002`. `match x { A => 1 B => 2 }` used to
parse.

**`P002` is the stage's one new diagnostic code**, and it is in the *parse*
block (`P0xx`), which D13 does not allocate. `P001` was the only one spent
before; `Parser::error` is still `P001` and `Parser::error_with` takes a code.

**`Parser` has no `no_struct_literal` field — suppression is a parameter.**
`StructLit { Allowed, Suppressed }` threads through `parse_expr_bp` →
`parse_prefix` → `parse_atom` → `parse_name_or_call`, which is the one reader.
`parse_expr()` is the *bracketed* entry (Allowed) and
`parse_expr_no_struct_lit()` is the four keyword heads'. Every bracketed context
re-enters at Allowed; a closure body inherits. See ADR-050. **If you add an
expression form that can contain a `{`, decide which it is** — the compiler will
make you, because `parse_atom` takes the parameter.

**A record literal is legal in a match arm body and inside any bracket.**
`match x { A => Point { x: 1 } }` and `if (Point { x: 1 } == p) { 0 }` used to be
`P001`. Any test that expected a parse error there is now wrong.

**`sanitize_name` is gone.** `collect_bindings` filters on
`is_bindable_name` (F3's `is_ident`) and drops a local whose name the language
cannot spell, rather than binding it under a shared `_x`. A `LocalBinding.name`
is now always the name as written, Unicode included.

### From S12's FE-02 — the wildcard token

**A lone `_` lexes as `SyntaxKind::UNDERSCORE`, not `Ident`.** Only the
one-character run: `_x`, `__`, `x_`, `_1` and `snake_case` are all still
identifiers. Every AST name accessor looks for an `Ident`, so a wildcard binder
is an **absent name** all the way down — `LetStmt::name()` answers `None` and
the resolver declares nothing. See ADR-049.

**`Parser::expect_binder(what)` is the binding position**, and it accepts
`Ident` or `UNDERSCORE`. Three call sites: `let`/`var`, a `fn` parameter, a
closure parameter. If you add a fourth binding position, use it — `expect(Ident,
…)` there would make `_` a parse error.

**A nameless binding lowers to a statement expression.** `lower_let`/`lower_var`
returned `None` when there was no name, which dropped the statement *and its
initializer's effects*; `let _ = d.pop_front()` stopped popping. They now fall
through to `lower_discarding_binding`, which evaluates and discards. Two JIT
tests (`deque_drained_is_empty`, `grid_get_out_of_bounds_faults`) are what
caught it.

**`_` has no expression form.** Reading one is `P001: expected an expression` at
the token. Any test that expected an *unresolved name* for `_` in value position
is now looking at the wrong category.

### From S11's F12 — TY-06, and MONO-03

**`TypeData::Record` and `TypeData::Enum` carry `args`.** A pattern that matches
one needs `{ def, .. }` at minimum; a site that reads field or payload *types*
needs the arguments. `RecordDef` and `EnumDef` carry `params: Vec<VarId>` — the
def's own type parameters, empty for everything except the prelude `Option`.

**`record_type`/`enum_type` take arguments and return a `Result`.**
`db.record_type(def, args)` / `db.enum_type(def, args)`; a count that disagrees
with the def's `params` is `TypeCtorError::TypeArgCount`. `db.record(name,
fields)` and `db.enum_(name, variants)` are unchanged and still infallible —
they register a **non-generic** def and instantiate it at no arguments, which is
what every caller wants. `register_record`/`register_enum` gained a `params`
argument between the name and the member set.

**There is one `Option`, and `TypeDb::new` seeds it.** `db.option_def()` is the
def, `db.option_of(elem)` is `Option[elem]`. **Do not spell the variant list
out** — the three sites that did (the `Some`/`None` prelude schemes, the
`Option[T]` annotation arm, the input parser's `Optional`) are all `option_of`
now, and `option_type` in `infer.rs` is deleted. A consequence worth knowing:
**slot 0 of every `TypeDb` is `Option`'s parameter `T`**, and `enum_defs[0]` is
`Option` — a test that indexed a def table positionally is now off by one.

**A def's field/payload types are in terms of its `params`; a *use* reads them
through its `args`.** `db.record_fields_of(def, args)`,
`db.record_field_of(def, args, name)` and `db.variant_payload_of(def, args, idx)`
substitute; they are identity and free when `params` is empty.
`db.enum_def(def).variants[i].payload` answers the **definition's** `T`, not the
element type — which is why `lower.rs`'s `lookup_enum_variant_by_name` no longer
returns a payload at all.

**`unify` merges two enum defs only when both are anonymous.** The
same-*name*-and-signature arm existed to reassemble the copies TY-06 made; with
one `Option` def there are no copies. Two named enums with different def-ids now
mismatch, as two named records always have. Same def-id unifies **pairwise on
arguments** — that is where `Option[?T] ~ Option[Int]` pins `?T`.

**`render` prints `Option[Int]`.** A nominal type with arguments prints them.
No snapshot moved (none held a rendered `Option`), which the plan half-expected
— but a *new* snapshot will show them.

**`TypeKey` is the identity; `db.render` is display.**
`db.canonical_key(t) -> TypeKey` is read-only and interns nothing. **Do not key
a cache on a rendered type.** The monomorphizer was, which is MONO-03; its cache
key is a `Vec<TypeKey>` now, and the mangled clone *name* — still built from the
rendered types, because a symbol has to be readable — is disambiguated with a
counter through `MonoPass::fresh_mangled_name`.

**`ScalarType` and `CollectionCtor` derive `Hash`**, so `TypeKey` can.

**A generic record fails the compile.** `record_schema_for` (codegen) builds a
schema from the def's field types, so a def with parameters would resolve
descriptors for its parameters. The language cannot declare one, and
`TypedExpr::RecordLit` carries no arguments to substitute — so it bails with a
named diagnostic rather than emitting a wrong layout.

**`RUNTIME_ABI_VERSION` is still 12.** Nothing in `praxis-types` is `#[repr(C)]`,
and the runtime half of F12 (RT-13) is S18's. **S11 spent no bump — S12 starts
fresh.**

### From S11's F5, F10, TY-01/03/05/07/22

**A `Type` cannot be written down; only the arena mints one.** `Type(0)` and
`Type(id)` no longer compile. If you have a raw `u32` — the debugger's
`DebugLocalMeta.type_id` is the only one — go through
`TypeDb::type_from_raw(u32) -> Option<Type>`, against **the same `TypeDb`** that
minted it. `VarId` is sealed the same way; `VarId::as_type()` is the total
conversion out of it, and `Type::to_u32()` is still how you store one.

**`TypeDb::intern` is `pub(crate)`.** Reach a composite through `tuple` /
`collection` / `func` / `record` / `enum_`. Three crates were using `intern`
directly to build collections and tuples the shaped constructors would have
refused — `praxis-repr` was building a `Range[Int]`, and `Range` is nullary.

**The shaped constructors take validated payloads.**
`db.tuple(TupleElems)`, `db.collection(ctor, CollectionArgs) -> Result`,
`db.record(Option<String>, FieldSet) -> Type`,
`db.enum_(Option<String>, VariantSet) -> Type` (and
`register_record`/`register_enum` for the def id alone). Convenience shortcuts
that cannot fail: `db.pair(a, b)`, `db.vec(t)`, `db.map(k, v)`,
`db.unary_collection(ctor, t)`. **`anon_record` and `anon_enum` are gone** —
pass `None` for the name.

**A tuple of fewer than two elements is not a tuple.** Both HIR sites that could
produce one now have a `tuple_or_degenerate` helper: zero elements is `Unit`,
one element is that element. If you add a site that builds a tuple from a
possibly-short list, use it rather than `TupleElems::new(...).expect(...)`.

**`EnumVariantDef.payload` is a `Vec<Type>`, and `EnumDef.name` is an
`Option<String>`.** An empty payload *is* the payload-less variant (TY-05).
`EnumVariantDef::new(name, payload)` and `EnumVariantDef::bare(name)` are the
constructors. Every `.unwrap_or_default()` / `map_or(Vec::new(), …)` mirror is
gone; do not add one back.

**`Y007` and `Y008` are spent.** `Y007` is a wrong type-argument count named at
the annotation; `Y008` is a duplicate field or variant in a declaration. **D13's
block allocation for S13/S16 must treat both as taken.**

**`praxis_input_parser::synthesize` returns `Result<Type, TypeCtorError>`**, and
`praxis-hir`'s `parser_lower` turns a failure into an `I001` diagnostic. Its
record and enum shapes take their names from user source, so a duplicate is user
input.

**A malformed method-catalog row panics.** `pattern_to_type`'s collection arm
`expect`s, because the catalog is compiler-authored data. S18's RT-14/RT-15
sweep is where that becomes a standing test.

**There is no `VarState::Generalized`.** `VarState` is `Unbound { level } |
Linked { target }`. A `match` over it is one arm shorter, and "is this variable
quantified?" is a question only a `Scheme` can answer — ask
`scheme.binders().contains(&v)`.

**`Scheme`'s fields are private.** `scheme.body()` and `scheme.binders()`; there
is no way to build a scheme with a binder list that disagrees with its body.
`instantiate_with_mapping` returns the fresh variable per binder, in binder
order, which is what MONO-01 wants.

**`generalize` mutates nothing.** It reads levels and collects. If you were
relying on generalization to make a variable un-unifiable, it does not any more
— the level discipline is the guarantee.

**A binding level is a `Level`, and it only goes down.** `Level::clamp_to` is
the only mutator; `is_deeper_than` is the generalization test. `enter_level` /
`exit_level` / `level()` all speak `Level` now. Writing the level-lowering rule
backwards is a type error, which is the point.

**`render` prints `?T` for every unbound variable.** Only `render_scheme` (and
the new `render_in_scheme`) know which variables are bound, so only they print a
bare `T`. A snapshot that showed `T` for a type rendered outside its scheme was
reading the arena's global flag.

**Top-level inference runs inside a declaration-group level.** `infer_with_tree`
calls `infer_declaration_group`, which enters one level, mints a signature
placeholder for **every** top-level `fn` before inferring any body, then infers
the statements and exits. A `fn` generalizes at `self.decl_site` — the level the
group was entered *from* — via `TypeDb::generalize_at`. Do not switch that back
to `generalize`: the group's level is still open, and `self.level()` there
quantifies nothing.

**A forward call is checked.** `fn first() { later("wrong") }` above
`fn later(value: Int)` is now a `Y001`. Any test that relied on a forward call
being unchecked will start reporting.

### From S11's F9

**There is one walk over `TypeData`, and it is `praxis_types::fold`.** A new
`TypeData` variant is now one compile error instead of five silent skips.
`lower_levels`, `occurs`, `generalize_walk`, `instantiate_walk` and
`deep_resolve` are five folders; none of them matches on `TypeData` any more.
**Do not write a sixth hand-rolled walk.**

**A folder owns its memo.** `TypeFolder` requires `db()` and `memo()`; the memo
is per traversal and is what terminates recursion through a record def and what
makes a shared subterm cost one visit. Creating a folder means holding
`&mut TypeDb`, a `FoldMemo`, and the walk's own state.

**Pruning counts as a change.** The default composite arms rebuild when a child
folds to something different — *including* when the difference is only that a
link was followed. That is what `deep_resolve` is; it is **not** what a
visitor wants, so an inspection-only folder writes `visit_only_composites!()`
and rebuilds nothing. A visitor that uses the rebuilding defaults will intern
fresh composites in the middle of unification.

**`instantiate` returns the same handle when nothing was substituted** (TY-02).
A monomorphic-in-practice scheme no longer copies its whole body, and a record
or enum no longer gets a specialized def per use site unless a field type
actually changed. No snapshot moved, which the plan half-expected to.

**`instantiate_walk`'s `.expect` is gone.** A `Generalized` var the scheme does
not list is left alone rather than panicked on; TY-03's scheme-owned binders
are what make it unrepresentable.

### From S10's P0-12 and RT-12

**`TypeDescriptor::compare` is populated, and D3 is answered — ADR-045.** Only
`Int`, `Byte`, `Char`, `Float` and `Text` declare one; the other sixteen
descriptors are `None` and `is_orderable()` now means something. The callback is
the **container** order: total, NaN last and equal to itself, `-0.0 == +0.0`.
The source-level `<` on a `Float` is unchanged and still IEEE (`Inst::FloatCmp`,
NaN unordered) — the two are different operations and the ADR keeps them apart.

**`supports_ord` is narrower, and `infer` finally calls it.** Composites —
tuples, collections, records, enums — are **not orderable**, so `(1, 2) < (1, 3)`
is now `Y006` where it used to compile. The capability had never been called
from anywhere, which is why `true < false` type-checked. If a stage needs
composite ordering back, it needs a language decision *and* a recursive
`praxis_value_cmp`, not a one-line `all`.

**A comparison's lowering follows its operand type, and `Text` is a runtime
call.** `compare_kind` in `build.rs` picks: `Float` → `FloatCmp`, `Int`/`UInt` →
`IntCmp` on an `Int` extract, `Bool`/`Char` → `IntCmp` at *their own width*,
everything else (`Text`, `Byte`, composites, unresolved vars, functions) →
through the descriptor — `praxis_struct_eq` for `==`/`!=`, the new
`Inst::ValueCmp` → `praxis_value_cmp` for an ordering. **There is no longer a
"read eight bytes and compare" fallback**; do not add one back for a new type.

**`praxis_value_cmp` is the 138th manifest row**, `(Ctx, Gc, Gc) -> RawI64,
Faults`. It returns `-1`/`0`/`1` and raises `TypeMismatch` when the operands'
descriptors differ or the type has no `compare`. `Inst::ValueCmp` carries **no
root or debug set at all** (the wrapper allocates nothing, so it is not a
safepoint) and is followed by a `CheckFault`. Adding an instruction still
touches five exhaustive matches.

**`praxis_struct_eq` and `praxis_value_cmp` both require descriptor pointer
identity before dispatching a callback**, as `DynamicKey` has since S7. A
miscompile is a fault or a `false`, not a callback run on a foreign layout.

**Text equality changed answer.** It used to extract eight bytes, which for a
`#[repr(C)] enum TextPayload` is the *discriminant* — so every pair of owned
strings compared **equal** and an owned/slice pair never did. Any test that
passed because two different strings were "equal" was passing for that reason.

**`HeapEntry::cmp` dispatches through the element descriptor** and answers
`Equal` — without calling anything — when the two entries' descriptors differ or
the type has no order. `HeapEntry::int_key` is gone; its two test callers now
render through `Debug`, which formats through the descriptor.

**`RecordSchema` has an `identity` field, and `record_equals` no longer compares
schema addresses.** `SchemaIdentity::{Anonymous, Nominal(&'static str)}`;
`RecordSchema::same_type` compares the identity *and* the field shape (names +
descriptor pointers). Three producers construct schemas — `generation.rs`,
`parser.rs`, test fixtures — and all three must now say which they are. The
codegen reads `RecordDef.name`: `Some` → `Nominal`, `None` → `Anonymous`.
`Generation::record_schema` takes the identity as its second argument.

**`record_hash` mixes the identity, the field names and the field descriptor
ids**, because everything `same_type` compares has to be in the hash.

**`RUNTIME_ABI_VERSION` is still 12.** `RecordSchema` gained a field, but
generated code never reads through the pointer it embeds — only the runtime
does, and both are compiled together. **S10's bump was unspent; S11's is still
unspent — nothing in `praxis-types` is `#[repr(C)]`, so F5 and F10 needed none.
F12 does, and it is the last thing S11 owns.**

### From S9, plus S10's RT-16

**`Map`/`Set`/`Counter` formatting is sorted, and heaps render in pop order.**
`maps::write_sorted` is the single place the hash collections' order is
decided; it sorts *rendered entries*, which is lexicographic, not numeric
(`10` before `9`), because `TypeDescriptor::compare` is still `None`
everywhere. Any test or snapshot that depended on hash-table order was already
nondeterministic; any that now expects numeric key order will need D3 first.


**A safepoint carries two sets, and they are different types.**
`Inst::{Alloc, Materialize, Call, CallIndirect, StructEq}` have
`roots: RootSlots` **and** `debug: DebugSlots` where they had one
`live_roots: Vec<LocalId>`. `Inst::CheckFault` has `debug` and **no roots
field at all**. Adding a safepoint instruction means writing
`RootSlots::unannotated()` / `DebugSlots::unannotated()` — there is no way to
hand-write a set, which is the point: 61 literals in `build.rs` did, and
`annotate` overwrote every one.

**`emit_spill` is gone; there are `spill_roots`, `spill_debug` and
`spill_safepoint`.** The first writes the shadow frame (and *nulls* the dead
slots); the second writes the debug frame; the third is both, and is what a GC
safepoint calls. `CheckFault` calls `spill_debug` only.

**`loop_roots` is gone from the pipeline lowerings.** `invoke_closure`,
`call_predicate`, `idx_ge_len`, `emit_bounds_check`, `emit_increment`,
`idx_cmp_const`, `sink_alloc`, `emit_sink_body`, `emit_flat_map_inner`,
`run_stage` and `alloc_empty_vec` all lost the parameter. If you add a helper,
do not thread one back in.

**Root sets shrink, and shadow slots get nulled.** Any test that assumed a
value stays rooted after its last use is now wrong. Any reasoning that "the
frame holds everything ever written" is now wrong: at each safepoint the frame
holds exactly `RootSlots::live`, because `RootSlots::dead` was just zeroed.

**`DebugLocal.value` is an `Option<GcRef>`.** A bare `.as_vec()` on one no
longer compiles. `is_real_ref` is deleted from `crash_snapshot.rs`,
`render.rs` and `evaluate.rs` — match on the `Option`.

**`Inst::IntBinOp` has an `overflow: Overflow` field.** `Checked` is
source-level arithmetic; `Bounded` is a site whose operands are bounded by a
collection's length (loop index bumps, `count`, the `+ 0` scalar copy) and
emits bare arithmetic with no overflow test. `Bounded` on `Div`/`Rem` is a
verifier error.

**`MethodEntry.can_fault` is a method, not a field.** Derived from the ABI
manifest like `allocates()`. It corrected one row: `bitset.insert` declared it
could not fault while `praxis_bitset_insert` raises `InvalidSize`.

**Every host verifies.** `praxis_mir::verify(f)` runs after `annotate` in
`praxis-cli/src/run.rs`, `praxis-debugger/src/session.rs`,
`praxis-debugger/src/evaluate.rs`, and both codegen test harnesses. **If you
add MIR that breaks an invariant, the whole 382-test JIT suite fails with a
named block and instruction**, not with a mysterious segfault. Adding an
instruction now touches five exhaustive matches, not four — `verify.rs`'s
`operands` is the new one.

**`praxis_raise_empty_collection` exists** and returns the **Unit sentinel**,
not `Void`. A `Void` row would have put the context pointer in a rootable slot
(`call_symbol` hands back `ctx` for a void wrapper).

**`RUNTIME_ABI_VERSION` is 12.** Bumped for F18: the `DebugLocal` layout is
unchanged, but "no value yet" moved from `NonNull::dangling()` to the all-zero
`None`, and generated code now writes zero into a dead slot. **S9's bump is
spent — S10 starts fresh.**

### From S8

**A `Jit` owns a `Generation`, and `lower_function` takes one.** Every piece of
metadata the backend mints — record schemas, tuple schemas, field names,
function names, debug-local arrays, embedded text literals — goes into
`generation`, not into a `Box::leak`. `leak_static_str` is deleted. **Do not add
a `Box::leak` to `lower.rs`**; add a `Generation` method, where the interning
test sees it.

**`build_debug_local_metas` returns `(*const DebugLocalMeta, usize)`, not a
slice.** The array is interned by content, so two calls with identical metadata
return the same pointer.

**The record-schema cache is keyed `(GenerationId, RecordDefId)` and lives in
the generation.** A bare `RecordDefId` is a per-`TypeDb` positional index; the
process-global map it replaces is the MIR-12/DBG-06 bug. `tuple_schema_for` is
keyed by the descriptor sequence, same as before, but per generation.

**Records built in two generations no longer compare equal.** `record_equals`
compares schema *pointers*, and each generation has its own. That is RT-12 (S10)
— schema identity should be nominal or structural, not allocational. The
debugger works around it by sharing one evaluation generation.

**`Runtime::teardown(self) -> HeapDrained` exists, and it is the only proof
minter.** `Generation::retire`, `Jit::retire`,
`praxis_runtime::retire_parser_plans` and `DebugSession::teardown` all require
one. **A generation that is merely dropped leaks its arena on purpose** —
`Drop` does nothing and the `Bump` is `ManuallyDrop`. Forgetting to retire costs
memory, never soundness (hazard H15, ADR-043).

**The debugger shares one generation across every `p EXPR`.**
`DebugSession.eval_generation` is a new public field (the CLI constructs it), and
`Jit::in_generation(Rc<Generation>)` is how a throwaway module joins an existing
arena. `evaluate` and `heap` take a `&Rc<Generation>`; `type_of` does not (it
never JITs).

**`Repl::into_session` exists** and drops the snapshot before handing the
session back, which is H8's ordering made explicit rather than inherited from
field declaration order.

**A parser plan is a `PlanId`, not a `u32`, and there is no zero.**
`TypedExpr::Read`/`Parse` carry `plan: PlanId` (re-exported from
`praxis_hir::PlanId` so MIR need not depend on the input-parser crate).
`lower_to_plan` returns an owning `CompiledPlan`; `register_plan` takes it and
returns `Result<PlanId, TooManyPlans>`. The old `plan_index: 0` failure sentinel
is gone — a failed analysis lowers to `error_expr()`.

**`praxis_run_parser` validates the id it reads back.**
`crate::parser::run_plan_by_index(ctx, idx as u32, …)` is now
`run_plan_by_id(ctx, idx: i64, …)`, which does a checked `try_from` plus
`PlanId::from_raw`. Anything naming no plan is a `ParseFailed` fault.

**The runtime's parser schema caches own their storage.**
`leak_record_schema`/`leak_tuple_schema` are `record_schema_for`/
`tuple_schema_for` in `parser.rs`, backed by one `SCHEMAS` registry of boxed
entries. They must be cleared with the plans (their field names point into plan
storage), which is what `retire_parser_plans` does.

**`RUNTIME_ABI_VERSION` is still 11.** S8 changed no `#[repr(C)]` type generated
code reads: `RecordSchema`/`TupleSchema`/`DebugLocalMeta` keep their layouts, and
only the *storage* moved. **S8's ABI bump budget is unspent — S9 starts fresh.**

### From S7's second half

**There is a new crate, `praxis-repr`, and it is where descriptor questions go.**
`praxis_repr::descriptor_for_type(db, ty) -> Result<&'static TypeDescriptor,
NoRuntimeRepr>`. `lower.rs`'s own `descriptor_for_type` is now a three-line
wrapper that turns the error into an `anyhow` diagnostic; the twenty-line match
with three `_ => INT` arms is gone. **Do not add a local descriptor match
anywhere** — add an arm to the bridge, where the round-trip test sees it.

**A descriptor request can fail, and there are two kinds of failure.**
`NoReprCause::NoSuchObject` (`Never`, `UInt`, `Range`, `Seq`, a non-built-in
descriptor) is always a compile error. `NoReprCause::Unresolved` (a type
variable) is tolerated *only* where null is representable — a collection's
element descriptor — because `let xs = Vec()` generalizes at the `let` and S15
is what fixes it. Ask `e.is_unresolved()`, do not match on the reason string.

**A null element descriptor now means "unknown", and survives.**
`praxis_vec_new`/`praxis_deque_new` keep a null argument null instead of
rewriting it to `INT`. Read it through `VecPayload::element()` /
`DequePayload::element()` / `GridPayload::element()`, which return
`Option<&'static TypeDescriptor>` — **a bare deref of `element_descriptor` is
now a null deref**, and in a test that aborts the whole process rather than
failing one case. Three payloads are affected; Map/Set/Counter/heaps still hold
a non-null `&'static` and still default null to `INT` (a remaining
inconsistency, noted in ADR-042).

**`push` adopts or rejects; it never retags.** `praxis_vec_push` and both
`praxis_deque_push_*` call `adopt_or_reject`, which sets the descriptor if there
is none, accepts if it matches by pointer, and raises `FaultKind::TypeMismatch`
otherwise **without storing the value**. `praxis_grid_set` does the same.

**`FaultKind` gained `InvalidSize` and `TypeMismatch`** — a `match` over it is
two arms longer again (four since S6). Neither needed an ABI bump: generated
code never switches on a fault kind and the `#[repr(C)]` enum's width is
unchanged. **`RUNTIME_ABI_VERSION` is still 11.**

**Grid results carry real descriptors.** `cells`/`row`/`column` are tagged with
the grid's cell descriptor; `positions`/`neighbors4`/`neighbors8`/`find_all`
with `tuples::TUPLE`. `praxis_grid_new` fills cells with the *zero value of the
cell type* (`default_cell`, an exhaustive match over `BuiltinTypeId`) and raises
`TypeMismatch` for a composite cell type it cannot invent a value for.

**Collection equality compares element descriptors, and tuple equality compares
shape.** `same_element` (pointer identity, two nulls agree) leads
`vec_equals`/`deque_equals`/`grid_equals`. `TupleSchema::same_shape` replaces
the schema-*address* comparison, and `tuple_hash` now mixes each slot's
descriptor id so `Eq` and `Hash` still agree.

**`DynamicKey`'s fields are private.** Use `k.value()` and `k.descriptor()`.
Equality short-circuits on descriptor pointer identity before touching a
callback.

**`GridExtent` and `BitIndex` are the only routes from an `Int` to a size.**
`BitSetPayload::{insert, contains, remove}` take a `BitIndex`, not a `usize`.
Both cap (`MAX_CELLS = 2^28`, `BitIndex::MAX = 2^32 - 1`) rather than merely
checking overflow. See ADR-041.

**The debugger reads types out of values, not out of descriptors.**
`descriptor_to_type`/`descriptor_id_to_type` are deleted;
`collect_bindings` calls `praxis_repr::type_for_value`. `p xs.get(0)` on a
`Vec[Text]` now types as `Text`.

### From S6 and the first half of S7

**`Heap::alloc` and `alloc_with` take a `Safepoint` as their first argument.**
The only producer is `Heap::pace(&RuntimeRoots)`, which performs the
`maybe_collect` — so obtaining the token *is* the pacing, and "allocate without
pacing" is unwritable on this path. The token is neither `Copy` nor `Clone`: one
token, one allocation. A wrapper that allocates twice paces twice.

**A `praxis_*` wrapper allocates through `gc_alloc` / `gc_alloc_with`** (abi.rs),
which pace and allocate in one step. If you add an allocating wrapper, use them.

**`Heap::alloc_unpaced` / `alloc_with_unpaced` are the named back door**,
`pub(crate)`, for the two callers that must not pace: the host's
`Runtime::alloc_*` helpers and `parser.rs`. **Do not reach for them from a new
runtime wrapper.** S20's IPR-14 removes the parser's use. In-crate tests use
them too — a test has no `RuntimeContext` to pace against.

**A `Bool` answer comes from `bool_ref(ctx, b)`**, which reads
`ctx.true_ref`/`ctx.false_ref`. `Heap::alloc_immortal` now takes an
`ImmortalWitness` only `immortal.rs` can construct, so `Immortals::new` is the
one place an immortal is minted.

**Swept storage is reused.** `Heap` keeps a free list keyed on `BlockLayout`
(the whole `[header|payload]` block's size and align, not the payload's own).
`BlockLayout::of` is the one calculation `alloc_raw` lays a block out with and
`sweep` files it under. Consequence worth knowing: **a stale `GcRef` can now
name a *live* object of a different type**, where before it named poisoned
storage. That is inherent to a reclaiming non-moving allocator and is why
poisoning (S4) had to land first (H7); it also means a rooting bug now
manifests as type confusion rather than as a clean panic.

**`Heap` has a `Drop`, and a `GcRef` does not outlive the heap.** It finalizes
every still-live payload, so reading a `GcRef` after the `Runtime` drops is now
a visible use-after-free. Both `take_crash_snapshot` consumers were audited
(H8): `Repl` declares `snapshot` before `session`, so the snapshot dies first,
and the debugger replaces its snapshot while the runtime is alive.
`CrashSnapshot`/`ParseDetail` hold `GcRef`s but have no `Drop` that
dereferences one.

**Pacing charges what an object costs, not its block.** `TypeDescriptor` gained
`owned_bytes: Option<OwnedBytesFn>`, set with the `const` builder
`.with_owned_bytes(f)` on the thirteen owning descriptors; scalars and
`VarCell` leave it `None`. A 64 KiB `Text` now reaches the 64 KiB threshold on
its own. A source-slice `Text` charges nothing (it borrows its owner).
**Post-allocation growth is still uncharged** — a `push` that reallocates the
spine — but the elements pushed are themselves paced allocations.

**Only a paced collection grows the threshold.** `Heap::collect` /
`Runtime::collect_now` no longer double it. Any test that grew the threshold by
calling `collect_with` is now wrong; `reset_restores_collection_pacing` was
rewritten for exactly this, and `#[cfg(test)] Heap::maybe_collect_with` is how
an in-crate test drives the paced path.

**`RUNTIME_ABI_VERSION` is 11**, bumped by RT-17's `Fault` repack. S6's half
needed no bump at all: `Heap` gained a field and `TypeDescriptor` gained a
field, but generated code holds the `Heap` pointer without dereferencing it and
passes descriptors by pointer without reading their fields — the only thing it
reads out of the runtime's own types is `GcHeader::payload_offset_for`, which is
unchanged. **S7's one bump (H17) is spent, and S7 is closed — S8 starts with a
fresh one.**

**`Fault` is one field wide, and `Fault::set` takes a `RaisedFault`.** The
`pending: bool` is gone — `is_pending()` is `kind != None`, so the two cannot
disagree. `RaisedFault` is a `FaultKind` proven not to be `None`, with an
associated constant per raisable kind (`RaisedFault::INT_OVERFLOW`, …);
`RaisedFault::new` is the only fallible route in. `Fault.kind` is private; use
`kind()`. **`FaultKind` gained `InvalidChar` and `InvalidText`** — a `match` over
it is two arms longer.

**`praxis_alloc_char` range-checks before narrowing.** `value as u32` truncated,
so `0x1_0000_0041` silently became `'A'` and a negative wrapped into range. Both
now raise `FaultKind::InvalidChar`.

**A source-slice `Text` is a validated `SourceSlice`.** `TextPayload::Slice`
holds one, its fields are private, and `SourceSlice::new` is the only
constructor — it rejects a range past the owner, an overflowing length, and ends
that split a multi-byte scalar. `Runtime::alloc_text_slice` and the parser's now
return `Option<GcRef>` and are `unsafe` (they read the owner's payload to
validate). `text_bytes` has no clamp and `text_str` no `unwrap_or("")`: both
were how a bad range became a *different, plausible* `Text` rather than a
failure.

### From S5 and the first half of S6

**`Heap::collect` and `maybe_collect` take a `&RuntimeRoots`, not a `&dyn
RootSet`.** In-crate tests use `Heap::collect_with` / `Runtime::collect_with`
(both `#[cfg(test)]`); a host that wants to force a collection calls
`Runtime::collect_now()`, which takes no root set on purpose. `RootSet` and
`RootScope` still exist and are still what `ShadowFrame` / `CrashSnapshot` /
`NativeRootFrame` implement — only the *entry point* is sealed.

**`RuntimeRoots::from_context` is exhaustive over five arms**, and its
`push_roots` destructures the struct rather than reading fields, so adding an
owner to `RuntimeContext` without rooting it is a compile error. If you add a
field that can hold a `GcRef`, that is where it goes.

**Native code roots through `NativeScope`.** `unsafe { NativeScope::new(ctx) }`
pushes a frame onto `ctx.native_roots` and pops it on `Drop`; `scope.root(r)`
takes `&self` (the roots are behind a `RefCell`) so several `Rooted` may be live
at once. Each scope costs one `Box`; if that shows up in a profile, S6 is the
stage that revisits the allocation path.

**`*_payload_mut` takes a `Rooted<'s>` and returns `&'s mut T`.** All nine
accessors and their 26 call sites. If you add a wrapper that needs one, open a
scope — that is the point, not an inconvenience: the old `&'static mut` said
the payload outlives the program.

**`RuntimeContext` gained three fields** — `native_roots` (S5), then `true_ref`
and `false_ref` (S6) — all appended, so generated-code offsets are unchanged.
**`RUNTIME_ABI_VERSION` / `COMPILER_EXPECTED_ABI_VERSION` are 10.**

**`praxis_alloc_bool` / `praxis_alloc_unit` return cached singletons** off the
context and allocate nothing. **`Heap::reset` resets the pacing counter and
threshold.** **`Runtime::heap_mut` is gone** — there is no safe route to
`&mut Heap` from a `Runtime`, which is what made `Heap::reset` dangerous.

**Automatic collection now happens with no generated frame on the stack.** That
is the whole point of P0-06, and it is the thing most likely to surprise: any
test that allocated through the `praxis_*` wrappers and assumed the pacing
counter kept climbing is now wrong. `maybe_collect_runs_under_pressure` was
rewritten for exactly this.

**A MIR local's `ty` is a `MirType`, not a `Type`.** `MirType::known() ->
Option<Type>` is the only reader, so every consumer decides what to do with the
absence. `Function::new_local`, `Builder::alloc_gc`/`alloc_temp`,
`AllocKind::Tuple.ty` and `AllocKind::Collection.args` all take it. A site that
has no type says `MirType::Opaque`; there is no longer any way to write
`Type(0)` and mean "unknown".

**Most call and allocation destinations now carry their real type.** The
`TypedExpr` being lowered already had one at far more sites than the plan
assumed — call/method-call/indirect-call results, tuple/record/enum
allocations, match results, field reads, `for` items and bindings, the parser
input and result, the closure-self param, and the pipeline result Vec and push
Units. What is still `Opaque`: pipeline accumulators, fused-loop items, the
fused `enumerate`/`zip` tuples, the `VarCell` slot, and every `Scalar` slot.

**`Opaque` resolves to nothing in the backend, not to something wrong.**
`collection_element_descriptor_for(db, args, i)` returns null (which every
`praxis_*_new` wrapper already reads as "unknown element type"), `tuple_schema_for`
returns the empty schema, and `build_debug_local_metas` emits a null descriptor
plus `praxis_runtime::debug::NO_STATIC_TYPE` (`u32::MAX`) — the debugger's
`type_str` already omits both. `descriptor_for_type` still takes a
`praxis_types::Type` and keeps its `_ => INT` fallback on the `Known` path (H9);
P0-11 in S7 makes that exhaustive.

**Closure captures load by immediate.** `Inst::LoadCapture { dst, closure,
index: u32 }` replaces `ConstInt` + `MoveGc` + `Call(ClosureCapture)`, and there
is no `check_fault` after it (the wrapper is `Effect::Pure`). Adding an
instruction touches four exhaustive matches: `ir.rs`, `liveness.rs` defs and
uses, and `lower_inst`. It is deliberately **not** in `safepoint_roots_slot`.

**`MoveGc` is `Gc` → `Gc`.** Documented, and true at every site — but not yet
*enforced*: the check is F17's verifier (S9). The gate today is
`closure_capture_indices_never_flow_through_gc_locals`, which now lowers a
closure program and a pipeline program.

**`GcHeader` is 24 bytes and its fields are private.** `descriptor`, `size`,
`payload_offset`, `mark`, `_pad`, `heap_id`. Use the accessors; construct with
`GcHeader::new` (allocator only) or `GcHeader::detached()` (tests). **The
payload no longer starts at `size_of::<GcHeader>()`** — it starts at
`GcHeader::payload_offset_for(payload_align)`, which is the single layout
authority and the only thing generated code may use to reach a payload.

**Every allocation carries a `HeapId`.** `Heap::mark` skips any reference whose
`heap_id` is not this heap's — including a swept one, since `Heap::sweep`
poisons (`descriptor = null; heap_id = 0`) before unregistering.
`GcHeader::descriptor()` now **panics** on a poisoned header rather than
dereferencing null. `Heap::reset` mints a fresh `HeapId`. `Heap::owns(GcRef)` is
public.

**A runtime symbol is a `RuntimeSymbol`, not a string.**
`CallTarget::Runtime(RuntimeSymbol)`, `TypedExpr::MethodCall.lowering_symbol:
Option<RuntimeSymbol>` (`None` = intrinsic; the empty-string sentinel is gone),
`MethodLowering::RuntimeSymbol(RuntimeSymbol)`. **Adding a runtime symbol is now
exactly two edits**: a row in `crates/praxis-stdlib/src/abi.rs` and an arm in
`praxis_runtime::abi::address`. Both are exhaustive; miss either and it does not
compile. H16 is discharged.

**`MethodEntry.allocates` is deleted.** Use `MethodEntry::allocates()`, which
reads the manifest. Note the correction it forced: `Vec.len` declared
`allocates: false`, but `praxis_vec_len` boxes its result and can collect. Any
safepoint reasoning that trusted the old field was wrong for the `*_len` family.

**Codegen derives every runtime signature.** `signature_for(sym, module)` from
the manifest row; `call_symbol` checks arity and narrows each argument to the
declared width. The twelve `*_sig()` builders, `runtime_funcref` and
`call_runtime_by_name` are gone. Call convention comes from
`module.isa().default_call_conv()`, pointer width from
`module.target_config().pointer_type()`. `const GC: types::Type = I64` still
exists for the value channel and is untouched.

**`RunnableFunction` and `MainEntry` are `fn(*mut RuntimeContext) -> GcRef`.**
The trailing phantom `GcRef` is gone (a zero-parameter `main` never had it), so
callers no longer allocate a Unit to fill it. `evaluate.rs`'s `null_sentinel()`
is deleted with it.

**`Int` arithmetic is lowered natively; two new raise wrappers exist.**
`Inst::IntBinOp` emits `iadd`/`isub`/`imul`/`sdiv`/`srem` on the scalar channel
with the overflow predicate computed inline, then calls
`praxis_raise_int_overflow_if` / `praxis_raise_div_by_zero_if` **with the
predicate** rather than branching around the call. Consequences worth knowing:

- Arithmetic no longer allocates, so an arithmetic site is not a safepoint.
  Anything that assumed `IntBinOp` needed a root spill is now over-spilling.
- `sdiv`/`srem` *trap* (process abort) on a zero divisor and on `i64::MIN / -1`,
  so the lowering substitutes a divisor of `1` in both cases and reports the
  fault. Do not remove that substitution.
- The raise wrappers are unconditional calls. If a later stage wants the
  branch-around form instead, `Inst::CheckFault`'s arm already shows how to
  split a Cranelift block mid-MIR-block.

**Fault epilogues return `ctx.unit_ref`, not `iconst 0`.** `UNIT_REF_OFFSET` in
`lower.rs` is the load. Both fault exits (the `Terminator::Fault` block and the
stack-overflow guard) use it.

**Effect classification is a judgement, and it is recorded per row.**
`Allocates` means "may trigger a collection", so a wrapper that only hands back
an immortal singleton is `Pure` however "alloc" its name reads —
`praxis_alloc_bool`, `praxis_alloc_unit` and the six `praxis_int_*` comparisons
are all `Pure`. The rows were derived by call-graph analysis over `abi.rs` and
spot-corrected (`praxis_run_parser` reaches the parser interpreter and is
`AllocatesAndFaults`). P0-08b took the manifest as the intent and made the
wrappers match it, in both directions: the fourteen that allocated without
pacing now pace, and the `Pure` predicate wrappers now really do allocate
nothing. A new row's `Effect` is a claim about the wrapper you are about to
write — write the wrapper to it.

### From earlier sessions

**Descriptors are `static`, and three fields are private.**
`praxis_runtime::scalars::INT` and its twenty siblings are `static
TypeDescriptor` — call sites take `&scalars::INT`. `id`, `size` and `align` are
private: use `.id()`, `.size()`, `.align()`. `ptr::eq` on two descriptors is
authoritative type identity; ADR-038 supersedes ADR-028's "compare by TypeId,
not by pointer".

**Descriptor ids were renumbered.** They follow `BuiltinTypeId`'s declaration
order. Any ADR quoting a specific number is describing a variant, not a literal.

**`SyntaxKind::from_raw_u16`** is the total conversion; `kind_from_raw` calls it.

**The lexer emits an `ERROR` token for an unclassifiable character**, so a bad
character yields two diagnostics rather than one.

**`SourceMap` stores `Arc<SourceFile>` and `FileView` has no lifetime
parameter.**

**`Jit::check_target(pointer, endianness)`** rejects a non-i64-pointer or
big-endian target at `Jit::new`.

**`lower_for` emits an extra block.** The index increment has its own block and
is `continue`'s target.

**The formatter preserves comments.** `fmt::starts_new_line` re-derives the
newline fact by walking `prev_sibling_or_token`. An earlier note here said F8
would let it read `Token::preceded_by_newline` instead; it cannot — F8's flag
lives on `praxis_syntax::Token`, the *pre-tree* token, and the formatter holds a
rowan `SyntaxToken`. The two rules are also not the same: `starts_new_line`
stops at the first sibling *node*, where F8's looks through the whole trivia
run. Leave it alone unless the flag is threaded into the green tree.

**`praxis_float_sign` no longer uses `f64::signum`.** Both zeros give `0.0`.

## 4. Where to start

**S16 is closed.** All five findings are fixed and gated — HIR-03, HIR-04,
HIR-05, HIR-06 and HIR-07 — and all eight exit-criterion tests pass. HIR-05
needed no code, exactly as the plan predicted ("discharged entirely by FE-02 in
S12 plus a wildcard-param representation for `|_|`") — `|_| 0` compiles clean
and declares nothing, and `a_wildcard_binder_is_legal_and_declares_nothing`
already pins all three of D7's positions including the closure param. The
stage's ADR is **055**.

**Pick up at S17 — the constraint channel and capabilities.** Eleven findings,
weight 66, the largest stage in the repair, and the one the plan gates on three
design decisions. **§5 has the answers: D5 and D6 are answered, both against the
plan's recommendation and both in the direction of more work — `..`/`..=` lands
in full, and all fifteen prelude names get implemented including the six graph
helpers. D4 is still open and blocks TY-32 and RT-08.** Read §5's S17 paragraph
before anything else; the stage as answered is several sessions of work, and
`panic` is the piece that must land first.

What S17 will want from what is already here:

- **F10's constraint channel is the stage's foundation and it did not land in
  S11**, deliberately — §2 says exactly what is missing:
  `praxis_stdlib::capability::CapKind`,
  `praxis_types::constraint::{Capability, Constraint}`,
  `TypeDb::take_dischargeable`, `TypeDb::substitute`, and the single exhaustive
  `praxis_hir::capability::check`. Land it as its own commit with the suite
  green, per plan §9 step 2. What *did* land is the half TY-01/TY-03/TY-22
  needed: `Scheme`'s private binders, `generalize` mutating nothing,
  `instantiate_with_mapping`, and the `Level` newtype.
- **TY-29 reshapes the same `Scheme` struct TY-03 did.** The plan says do it in
  one reshape, not two, and to do TY-29 first because everything else in the
  stage consumes its worklist.
- **`descriptor_supports(d, cap)` is F11's unlanded half**, and S17 is its
  consumer — §2 records it as the one thing `praxis-repr` still owes.
- **`TypeDescriptor::compare` is populated on five descriptors and `None` on the
  other sixteen** (ADR-045). TY-32's heap-ordering half and RT-08 read it;
  `heap_element_orderability_agrees_with_the_runtime` (`infer_tests.rs`) is the
  gate that the capability and the descriptor agree, and it is green.
- **`numeric_scalars_are_orderable` (`capability.rs`) is H18's last entry** — a
  currently-*passing* test that asserts `Text`/`Char` orderability and changes
  meaning in this stage. Rewrite it in the same commit, per plan §8.2.
- **A compound assignment against an unconstrained target is still not
  reported**, and S13 left it there on purpose: `fn f(a) { a += 1 }` leaves `a`
  a variable, and pinning it to `Int` would silently change inference for every
  unannotated numeric parameter. That is TY-31's `Y015`.
- **`Y015` (`NotNumeric`), `Y014` (`NotHashable`), `Y016`
  (`OperatorNotDefined`) and `Y013` (`IntLiteralOutOfRange`) are already
  allocated and unused** — ADR-051 reserved them for exactly these findings.
  Check ADR-051 before allocating anything new; the `Y12x` block is full through
  `Y123`, so `Y124` is the next free match code.
- **`praxis-stdlib`'s catalog rows for `enumerate` and `zip` are wrong**, and
  S15 recorded it as a finding the register does not have. If S17 touches the
  catalog's type variables (TY-31's bound migration crosses 74 sites), fix those
  two rows in the same pass and tell H10 about it — see the S15 entry below.

What S16 leaves behind, all deliberate:

- **A wrong sub-pattern count is not diagnosed.** `Wrap(a, b)` against a
  one-slot variant lowers `b` (so anything wrong inside it still reports) and
  then drops it. The register has no finding for it, `Y122`/`Y123` cover the two
  neighbouring mistakes, and truncating is strictly safer than the payload
  read past the end that it replaces. It is the natural companion to whatever
  stage next allocates a `Y12x`.
- **Records and tuples have no pattern syntax**, so their signature is `Open`
  and a match on one needs a `_`. When the grammar grows one, they become
  `Closed` with a single constructor and the matrix already handles that shape.
- **`a_bare_constructor_name_is_that_constructor_at_any_payload` passes against
  the unfixed tree.** It pins the padding *rule*, not the bug — the matrix reads
  sub-patterns through `get(i).unwrap_or(&WILDCARD)`, so it is defensive against
  an unpadded row on its own. Its runtime half (`jit.rs`) is where the padding's
  new behaviour is observed. The other four new gates and both exit tests were
  verified failing against `22f9132`.
- **`lower_record_lit` still silently skips a field it cannot place.** It is
  unreachable for a clean program (inference reports first), so it is a
  defensive `continue` rather than a hole.
- **An unconstrained scrutinee stays silent** in `lower_pattern`, and the matrix
  meets the same case: a type variable has an `Open` signature, so a `_`-less
  match on one is `Y120` exactly as every other unenumerable type is. Inference
  has already reported the variable itself.
- **`SymbolKind::EnumVariant` is how "is this name a constructor" is answered**,
  and the kind is load-bearing on its own: a scheme cannot tell a constructor
  from a binding that *holds* one, because `let A = Empty` has the enum's type
  too. `PreludeEntry::is_variant_ctor` marks `Some`/`None`, which no `enum` item
  declares; `seed_builtin_schemes` filters on the kind *pair*, and forgetting
  that is what makes every `Option` test fail with "unresolved user function
  `Some`".

**S11 is closed.** All eight findings the stage owns are fixed and gated —
TY-01…TY-07 and TY-22 — and all five exit-criterion tests pass. TY-04 needed no
work of its own: F9's fold walks record fields and enum payloads, which is the
whole finding, and `deep_resolve_rewrites_record_field_links` is green. TY-06
was F12's static half; see ADR-048 and §3.

**S12 is closed.** FE-02, FE-04, FE-06 and DBG-03 are all fixed, all six
exit-criterion tests pass, and the stage's two ADRs (049, 050) are written. It
spent no ABI bump: nothing it touched is `#[repr(C)]`.

**S13 is closed.** All ten findings are fixed and gated — F2, TY-08…TY-15,
TY-23, TY-24 — and all fourteen exit-criterion tests pass. The stage's ADR is
**052**. Notes worth carrying:

- **TY-12 needed no code at all.** S11's `Y007` already covered a wrong
  type-argument count on collection ctors *and* on `Option`, so
  `malformed_collection_type_arity_is_rejected` was green before the stage
  touched it. The progress note that predicted this was right; check before
  writing, as it said.
- **TY-08 is not what blocks the empty-`Vec[Float]` descriptor.** Its
  `#[ignore]` reason claimed the annotation never reaches the initializer. It
  does — `let values: Vec[Float] = Vec(); values.push(1)` is a `Y001`. What
  loses the element type is `lower_call`, which re-instantiates the callee's
  scheme rather than using the type inferred at the call site, so the typed tree
  carries `Vec[?T]`. That is **F15/MONO-01 in S15**, the same finding
  `lowered_polymorphic_call_result_uses_the_callsite_instantiation` gates, and
  the reason now says so.
- **The corpus triage pass found nothing.** Every `.px` under `tests/` (all
  thirteen, including the twelve `aoc-corpus` programs) and every fixture under
  `crates/praxis-cli/tests/fixtures` was run through `praxis check`; the only
  files that report are the three that are *meant* to
  (`bad_byte.px`, `parse_error.px`, `type_error.px`). `Y009` was the risk — every
  compound assignment and reassignment in the corpus and in `jit.rs` targets a
  `var` holding an `Int`. This is the **fifth** prediction of wide churn in the
  repair that delivered none.

**S14 is closed.** All six findings are fixed and gated — TY-16…TY-21 — all six
exit-criterion tests pass, and the fourth exit criterion (a MIR change, not a
test) is done: `lower_break`, `lower_continue`, `break_loop` and `continue_loop`
read the loop stack with an `expect`. The stage's ADR is **053**, and it amended
**051** for `Y017`. Notes worth carrying:

- **The corpus triage found nothing, for the sixth time.** Every `.px` under
  `tests/` and every CLI fixture was re-run through `praxis check` after TY-21;
  only the three files that are *meant* to report do. `Y017` was the risk —
  nothing in the corpus breaks with a value out of a `while`/`for`.
- **F20 was not needed and did not land.** The plan wants the one derived
  `TypedExpr` child walker before TY-21 "reshapes `Loop`". TY-21 reshaped
  nothing: `TypedExpr::Loop` already had a `ty`, and the fix was to compute it.
  F20's consumers are now entirely S15's (HIR-08, MONO-01, MONO-02, HIR-09).
- **TY-21 is three passes computing one join**, and each has to. See §3.

**S15 is closed.** All seven findings are fixed and gated — F20, HIR-01,
HIR-02, HIR-08, HIR-09, MONO-01 and MONO-02, with MONO-03 already closed in S11
— and five of the six exit-criterion tests pass and are un-ignored, plus S7's
`empty_vec_float_has_the_float_element_descriptor_before_any_push`, which was
blocked on this stage. The ADR is **054**. Notes worth carrying:

- **F15 is the stage, and §2/§3 say what landed.** Read §3's first block before
  touching HIR — the typed tree's types come from one map now, and several
  things a lowering used to compute for itself are gone.
- **The sixth exit criterion is deferred, and the reason is new information.**
  The plan asks S15 to turn on the MIR verifier's no-`Opaque`-at-a-
  descriptor-site rule (**H10**) on the grounds that F15 supplies the missing
  types. F15 does supply them: the `for`-loop item and its binding slot, the
  parser result, the closure value, the indirect call result, `out(x)`'s result
  and a pipeline's *source* item are all `MirType::Known` now, and every
  `AllocKind::Collection` a program writes carries real type arguments. Two
  descriptor sites are left, both `AllocKind::Tuple { ty: Opaque }`: the tuple a
  fused `enumerate` or `zip` builds. They need **two** things:
  1. **MIR-05's per-stage item types (S21).** A fused chain knows what its
     source yields; what stage *n* yields is a fact no stage carries.
  2. **A method catalog that describes `enumerate` and `zip`.** Both rows
     declare `result: Vec[T]`. Verified, not read off the rows:
     `fn main(v: Vec[Int])` with `let e = v.enumerate()` gives `e :: Vec[Int]`.
     This is **a finding the register does not have**, and F15 sharpens it —
     lowering believes the catalog now. It belongs with S21's MIR-05, or with
     whatever stage next touches `praxis-stdlib`'s sequence rows. Until then,
     `alloc_empty_vec` deliberately does **not** read its element type from the
     chain's result type, though that type is `Known` at every call site: it
     would swap an honest null descriptor — which the runtime repairs on the
     first push — for a wrong one.
- **`MirType::expect_known`/`MirTypeError` are still unwritten**, for the same
  reason they were in S9: their only consumer is the rule above.
- **The corpus triage found nothing, for the seventh time.** Every `.px` under
  `tests/` and every CLI fixture went through `praxis check`, and the corpus
  went through `praxis run` as well (which is what exercises lowering and mono;
  `check` stops at `analyze`). Only the three files that are *meant* to report
  do. **One pre-existing breakage surfaced and is not S15's:**
  `tests/aoc-corpus/day02_grid_of_char.px` calls `map.len()` on a `Grid[Char]`,
  and the catalog has no `Grid.len` — it has `width`/`height`. Confirmed
  against the tree before this stage's first commit. No Rust test covers the
  corpus directory, which is why nothing had caught it.

**What S15 deliberately left:**

- **`TypedStmt` still has no derived walker**, and F15 added no statement form.
- **`lower_block` derives its type from its tail rather than reading the block
  node.** An `else if` body has no `BLOCK_EXPR` node at all — lowering
  synthesizes the block — so the tail is the only answer available for every
  block, and TY-16 already made the two rules identical.
- **A `VarCell` local and the parser's input buffer stay `Opaque`.** Neither
  holds a value of a language type: one is a cell object wrapping the value, the
  other is a runtime-owned buffer.
- **`mono` still keys its cache on the call site's types, not on a use-site
  substitution witness.** `instantiate_with_mapping` returns the mapping and
  `specialize` uses it, but the *cache key* is `(callee, arg keys, result key)`.
  Two call sites that agree on arguments and result share a clone, which is
  correct; a callee with a phantom binder that neither mentions would collapse
  them, and no such callee exists.
- **Nothing checks that a `TypedModule` has no unbound `TypedExpr.ty`.** F15's
  sketch proposes a debug walk asserting it. The two totality gates state the
  property one level up — inference records every node, lowering reports rather
  than inventing — and an unbound *variable* is legitimate in a generic
  function's clone-source, so the walk would need to know which functions are
  about to be specialized.

**What S14 deliberately left:****What S14 deliberately left:**

- **`join` is used at six sites.** `infer_if`, `infer_match`, `lower_if`, the
  function result in `infer_fn`, a closure's result in `infer_closure_body`, and
  now a loop's breaks in `infer_break`/`lower_break`. **If you add a branch
  point, join it.**
- **A divergent statement does not make the rest of a block unreachable.**
  `{ panic("x"); 1 }` still infers `Int` for the block, because the tail is the
  last statement whatever precedes it. Nothing in the audit asks for
  reachability analysis, and `Never`'s absorption is about *branches meeting*,
  not about dead code. The same goes for a `loop`: `loop { }` is `Never`, but the
  statements after it are still inferred.
- **`while` and `for` are `Unit` unconditionally**, and a value `break` in one is
  now `Y017` rather than a silently discarded value. Nothing gives them a value
  form; D2 says nothing should.
- **A `loop` whose `break` value type is still a variable gets a `Known` MIR
  local holding it**, which is the ordinary unresolved-annotation shape and the
  D9 carve-out already tolerates it (a null descriptor). F15 is what makes such a
  type resolve at all.
- **No reachability analysis decides whether a `break` is *reachable*.**
  `loop { if false { break 1 } }` is `Int`, not `Never`: the join is over the
  `break`s that are *written*, not the ones that can run. Making it otherwise is
  a dataflow pass no finding asks for.

**What S13 deliberately left:**

- **F19's SCC-ordered binding groups did not land**, and no S13 finding needs
  them. What landed is the *type* environment and the fn signature placeholders.
  Dependency-ordered binding groups over the **call graph** are what mutual
  recursion needs to generalize correctly — see "What S11 deliberately left"
  below, which is still exactly true. `infer_declaration_group` is where they go.
- **A type-declaration cycle registers rather than reports.** `struct A { b: B }`
  / `struct B { a: A }` and `struct Node { next: Node }` come out of the pass
  with a fresh variable where the recursive member should be. Making that work is
  equirecursive (or iso-recursive) types — a language feature; making it an
  *error* is a language decision. ADR-052 records both as out of scope.
- **A compound assignment against an unconstrained target is not reported.**
  `fn f(a) { a += 1 }` leaves `a` a variable. Pinning it to `Int` would make the
  check total and would silently change inference for every unannotated numeric
  parameter; S17's TY-31 (`Y015`) is where numeric constraints get a channel.
- **A nested `struct`/`enum` is still ignored, silently.** `register_top_level`
  never declared one and the declaration pass only walks top-level statements, so
  a `struct` inside a block has no symbol and no type. It was equally ignored
  before; F19's dispatcher split (`resolve_top_stmt` vs `resolve_block_stmt`) is
  what would make it an `N005`-shaped report, and no finding asks for it.
- **`Analysis.scopes` is still public and still populated.** Inference no longer
  reads it, but it is resolution's output and the LSP is its notional consumer.

**What S12 deliberately left:**

- **The FE-04 trap is open, and D8's chosen rule does not close it.** `let x = 1`
  followed by a line starting with `(` still parses as a *call* of the
  parenthesized expression — the newline is a statement separator, but it does
  not stop the postfix loop. ADR-049 records the two rejected alternatives and
  the workaround (bind the tuple to a name). It is the reason
  `tuple_schema_uses_the_unit_descriptor_for_unit_elements` was rewritten in S7,
  and it will bite again the same way.
- **`starts_expr` still asks two questions.** A newline says "no value", and so
  does a token that cannot begin an expression (`;`, `}`, `)`, `,`, `else`,
  `in`). The second list is unchanged and is still a hand-written set.
- **FE-05 is not S12's.** F8's unblocks list names it, but the interleaved
  postfix loop landed in S2 and `regression_postfix_forms_may_be_interleaved` is
  green.
- **A closure body inherits the ambient struct-literal suppression** rather than
  resetting it, because `|` is not a bracket the grammar closes over. See
  ADR-050; nothing in the corpus depends on either choice.

**F12's runtime half is still owed, and it is S18's.** What did *not* land, on
purpose:

1. **`EnumSchema`, `EnumPayload`'s schema pointer, and
   `praxis_alloc_enum(ctx, tag, arity)`'s new schema parameter** — that is
   **RT-13**, which the plan's finding register and §5's S18 paragraph both
   place in S18, and which S18 says must land "in ONE commit with codegen"
   because the `#[repr(C)]` layout crosses the JIT boundary. It also needs its
   own `RUNTIME_ABI_VERSION` / `COMPILER_EXPECTED_ABI_VERSION` bump (12 → 13),
   exactly once (H17). §4's earlier reading of F12 as "one commit across codegen
   and runtime" merged the two halves; the plan does not, and RT-13's own entry
   says it depends on TY-06's canonical `Option` — which now exists.
2. **`SchemaIdentity` is untouched.** RT-12 landed
   `Nominal(&'static str)` in S10 and nothing about `RecordSchema::same_type`
   changed here. When S18 gives a runtime enum an identity, the *record*
   identity can carry arguments alongside the name at the same time — see
   ADR-048's last alternative for why the name, not the plan's `Nominal(u64)`.
3. **DBG-02's value-less half is still open.** A record object records its
   nominal name; turning that name back into a `Type` needs a
   name→`RecordDefId` lookup *and* field types the schema does not carry (it has
   descriptors). That is F15's per-node type map, in S15.

**What S11 deliberately left:**

- **F10's constraint channel did not land**, and §2 says why: its only consumers
  are S17. What landed is the half TY-01/TY-03/TY-22 need.
- **Mutual recursion is checked but not properly generalized.** Two functions
  that call each other now unify against real placeholders — strictly better
  than the previous silence — but the earlier one generalizes before the later
  one's body has constrained the shared variables. Doing it right needs
  dependency-ordered binding groups (SCCs over the call graph), which is
  **F19's `DeclGroup` driver in S13**. `infer_declaration_group` is where that
  lands; it already has the two-phase shape and the group level.
- **`praxis_struct_eq`'s duplicate-name check is at the `TypeDb`, not at the
  record literal.** `record_literal_rejects_duplicate_fields`
  (`infer_tests.rs`) is still `#[ignore]`d: `Point { x: 1, x: 2 }` is a
  *literal*, and F5 validates *definitions*. That is HIR-05's, in S16.
- **A wrong-arity annotation resolves to no type**, so a `let` with one gets a
  fresh variable after the `Y007`. That is the same shape as every other
  unresolved annotation and is TY-08/TY-09's territory (S13).
- **A record cannot be generic and nothing enforces that in `praxis-types`.**
  `register_record` takes a `params` list because the shape is one with
  `register_enum`, but no caller passes a non-empty one and the language has no
  `struct P[T]` syntax. The two places that would silently do the wrong thing
  refuse instead: codegen's `record_schema_for` bails, and `unify` will not
  merge two generic defs structurally. If S16 gives records parameters, those
  are the two sites to fix first, plus `TypedExpr::RecordLit`, which carries a
  `RecordDefId` and no arguments.
- **`TypedExpr::EnumVariant.ty` is the constructor scheme's own type**, not a
  per-use instantiation, so it is `Option[?T]` with an unresolved argument. That
  was equally true before F12 (the def held the scheme's `T`); it is the same
  looseness F15 fixes with a per-node type map in S15. Inference *does*
  instantiate — `infer.rs`'s `lookup_enum_variant` mints a fresh use and unifies
  it against the scrutinee — so the type a *program* sees is right; it is the
  typed tree that carries the loose one.

Two things S10 leaves for a later stage, both deliberate:

- **`maps::write_sorted` still sorts *rendered* entries**, so `{10: a, 9: b}`
  prints `10` first. `TypeDescriptor::compare` now exists for `Int`, so the
  sort *could* be numeric — ADR-045 leaves it alone because changing what a
  program prints is a user-visible change that belongs with the sort/`Ordered`
  work, not with a bug-fix stage. One function, `maps.rs`.
- **Composite ordering is a compile error**, not a lowering. `(1, 2) < (1, 3)`
  is `Y006`. Bringing it back needs a language decision *and* a recursive
  `praxis_value_cmp` — see ADR-045 decision 1.

**What S10 deliberately left:**

- **`Text.get` returns an `Int`**, so the language has no way to *write* a
  non-ASCII `Char` and the input parser cannot *read* one: `grid(char)` scans
  bytes, so `aβ` fails to parse. That is why
  `char_ordering_uses_unicode_scalar_values_without_out_of_bounds_reads` was
  rewritten to an ASCII grid, with the non-ASCII half covered at the runtime
  level. The parser half belongs to S19/S20, beside IPR-06.
- **`praxis_struct_eq` still answers `0` for a non-equatable type** rather than
  faulting, which is the pre-existing defensive default. Only the
  descriptor-*mismatch* case was tightened.
- **The debugger still shares one evaluation generation across `p EXPR`.** RT-12
  removed the correctness reason for it (records from two generations now
  compare equal); it stays as the thing that bounds a long session (DBG-05).
- **DBG-02's value-less half is still open**, and RT-12 moved it forward without
  closing it: a record object now *does* record its nominal name, in
  `SchemaIdentity::Nominal`. What the debugger cannot yet do is turn that name
  back into a `Type` — that needs a name→`RecordDefId` lookup and field types
  the schema does not carry (it has descriptors, not types). F12, with enums and
  closures. `praxis_runtime::repr`'s `Unrecorded` reason for `Record` is now
  half-true and should be reworded when that lands.

**What S9 deliberately left:**

- **`ScalarLiveAcrossSafepoint` is not a verifier rule**, and ADR-044 §5
  records why: it fires on every `lower_seq_*` accumulator by construction and
  is harmless there, since a scalar is a copy of a payload and cannot dangle.
  Do not add it without moving those accumulators into `Gc` slots first.
- **`OpaqueAtDescriptorSite` stays off** (H10), and
  **`MirType::expect_known`/`MirTypeError` are still unwritten** — the plan
  puts them in S9 because F17's verifier is their only consumer, but that
  consumer is the rule H10 defers. (S9 expected them in S15; S15 could not turn
  the rule on, and §4's S15 entry says what it now waits for.)
- **Effect-driven safepoints did not land.** F17's third part wanted
  `CallTarget::Runtime(sym).allocates()` to decide what is a safepoint;
  instruction shape still decides. The two agree today, and nothing needed the
  change. A `Pure` runtime call still gets a root set it does not need.
- **`v.sum()` overflow is still observed late.** The accumulator is
  `Overflow::Checked`, but no `CheckFault` follows the loop, so the fault is
  sticky and the host sees it after `main` returns instead of unwinding at the
  sink. This is the residue of P0-08's open question; the verifier has no
  "every faulting instruction is observed" rule because the codebase does not
  satisfy one yet.
- **`min`/`max` on an empty sequence still return `0`.** They share MIR-09's
  empty case but not its defect — the accumulator is a scalar initialized to
  `0`, so the answer is defined, if debatable. `adv_pipeline_empty_source_min_is_zero`
  pins it. Whether `0` is right is **D1**'s question, and S18 is where it gets
  settled alongside `Map.get` / `Grid.find`.

**What S8 deliberately left** (still true):

- **`tuples::POINT` is still a process-static leak.** One `TupleSchema` for every
  grid position, minted by the runtime rather than by a compile, so there is no
  generation to hang it on. Bounded at one.
- **Enum schemas have no generation home** because they do not exist yet
  (RT-13/F12, S18). When they do, they belong in `Generation` beside the record
  and tuple caches — the plan sketch already lists an `enum_schemas` field.
- **A plan is registered per compile and never deduplicated.** A debugger session
  that reloads a thousand times registers a thousand plans; `MAX_PLANS` (2^20)
  catches a runaway, and `retire_parser_plans` reclaims at teardown, but there is
  no interning as there is for JIT metadata. Keying on the `ParserAst` would give
  it; nothing needs it yet.

Re-read §6 of the plan first. The hazards that still bind: **H17** (RT-13 spends
S18's one bump — S11's went unspent, because the runtime half did not land), and
**H10** in its long form — the MIR verifier's "no `Opaque` in a
descriptor-producing position" rule, which S15 could **not** turn on. It now
waits on **S21** (MIR-05's per-stage item types) *and* on a catalog fix for
`enumerate`/`zip`; §4's S15 entry says why, and it is a finding the register does
not have. **H3 is discharged** — the debug/root
split landed first, the two m11 tests are green, and
`the_debug_set_still_shows_what_the_root_set_dropped` now states the property
at the level it lives at rather than leaving it to a CLI snapshot three layers
away. **H15 is discharged** — ADR-043 encodes the ordering in `HeapDrained`
rather than documenting it. **H13 is discharged** — FE-04 landed before FE-06
(see §6 for what the hazard's own example actually does). **H18 has one entry
left**: `numeric_scalars_are_orderable` (S17) —
`mutable_capture_records_error` was inverted with HIR-09.
`sanitize_rejects_digit_leading_and_punct` was rewritten with DBG-03,
`generalized_var_state_is_marked` with S11, and the vec-adopts-first-descriptor
assertions turned out not to need rewriting at all (§6). **H1, H2, H4, H6, H7,
H8, H9 and H16 remain discharged.**

## 5. Design decisions still open

**D14 is answered** — see ADR-040. The `Safepoint` token shipped with a named,
`pub(crate)` `Heap::alloc_unpaced` back door for the host helpers and the
parser interpreter (the plan's option 2), rather than landing IPR-14 out of
stage order (option 1) or deferring the token (option 3).

**D9 is answered** — see ADR-042. The JIT **fails the compile with a
diagnostic** naming the type and the site, which is the option the plan's own S7
text annotates "(correct)". Two carve-outs where absence is already
representable and already rendered: debug metadata emits a null descriptor plus
`NO_STATIC_TYPE` (what `MirType::Opaque` locals already do), and a collection's
element descriptor may be null. A third distinction fell out of the work and is
part of the answer: an *unresolved type variable* is not the same failure as a
type that can have no object, and only the first is tolerated — see
`NoReprCause` and hazard H10.

**D12 (panic-across-FFI) remains open** and RT-06/RT-07 landed without it. What
shipped is narrower than a policy: the two wrappers the audit named now fault
instead of aborting. Other wrappers still reach Rust panics on malformed input.

**D3 is answered** — see ADR-045. Only the scalars with a `compare` callback are
orderable (`Int`, `Byte`, `Char`, `Float`, `Text`); composites are rejected at
compile time rather than admitted with an unchosen semantics. NaN sorts last and
equals itself **inside a container**, where a total order is mandatory; the
source-level `<` on a `Float` stays IEEE, and the ADR's point is that these are
two operations, not one. `f64::total_cmp` was rejected for splitting `-0.0` from
`+0.0`, which would have disagreed with `equals` for ordinary values. The
"compare by TypeId, not by pointer" contradiction the plan flags under D3 was
already resolved by ADR-038 (descriptors are `static`); ADR-045 decision 3 is
what makes both dispatch sites *use* pointer identity.

**D7 and D8 are answered and implemented — see ADR-049.** `_` is legal in every
binding position and introduces nothing (D7, FE-02); a newline terminates a
statement and never a subexpression, consulted only between statements and at
`break`/`return`'s optional-value decision (D8, F8/FE-04).

**D13 is answered — see ADR-051.** The whole block is allocated: `N003`–`N005`,
`Y009`–`Y016`, `Y099`, `Y113`–`Y115`, `Y122`–`Y123`, and `I011`–`I014`/`I023`/
`I028`. Six findings the plan routes through D13 need no code at all, and TY-12
turns out to be `Y007`. The plan's inventory of *taken* codes was incomplete —
it missed `N000`, `Y110`, `Y112`, the whole `I0xx` validation block and both the
`T0xx` and `P0xx` categories; ADR-051 opens with the corrected one. All five
codes it allocated to S13 — `N003`–`N005`, `Y009`, `Y010` — are now spent, and
the stage needed no number the ADR did not anticipate.

**S13 raised no new decision, and answered none.** Its choices are recorded as
ADR-052 rather than as answers to plan decisions, because the plan does not list
any: what to do with a type-declaration cycle, whether an annotation name goes
in `refs`, and when a compound assignment is reportable are all questions the
work raised and settled locally. Two of them *touch* language design — recursive
data types, and pinning an unconstrained numeric — and ADR-052 explicitly leaves
both open.

**D2 is answered *and implemented* — see ADR-053.** Both halves landed with
TY-21; the answer as the repo owner gave it (2026-07-29) was:

- **A `loop` with no reachable `break` is `Never`**, not `Unit`. It is the
  bottom type's whole job — `loop {}` produces no value — and it makes
  `if c { 1 } else { loop { } }` an `Int` rather than a `Y001`. It also agrees
  with `panic()`, which is already `Never`.
- **A value `break` inside `while`/`for` is a type error.** Those two have a
  `Unit`-valued exit path the compiler cannot fill, so only `loop` is an
  expression loop. This is the plan's own recommendation, and it needs the
  loop-context stack to carry a *flavour*.

What implementing it added that the answer does not say: a **bare** `break`
contributes `Unit`, so `loop { break }` is `Unit` and mixing the two spellings is
a `Y001` — the alternative (a bare `break` contributing nothing) would have made
`loop { if c { break }\n break 1 }` quietly an `Int`.

**S14 spent one new code, `Y017`**, and ADR-051 is amended for it — the amendment
path that ADR's own last consequence describes. The `Y0xx` user block is now
spent through `Y017`; the next free number is `Y018`.

**D1 and D5 still block their stages**; neither has been answered.

**S17 is the next stage, and three decisions gate it — D4, D5 and D6.** Two are
now answered by the repo owner, **and both were answered against the plan's
recommendation, in the direction of more language rather than less**. A fresh
context must size the stage accordingly: S17 was already the largest in the
repair at 11 findings and weight 66, and these two answers add a syntax-to-
runtime vertical slice and a graph library on top of it.

- **D6 (TY-34, `Range`) — answered: implement `..`/`..=` in full.** *Not* the
  8-site deletion. That is a lexer, parser, AST, type-rule, MIR-lowering and
  runtime-iteration slice; the plan rates it XL on its own. `CollectionCtor::Range`
  and `capability::iter_item`'s `Range => Int` arm are the two places the type
  system already assumes it exists. Land it as its own sequence of commits, not
  inside another finding.
- **D5 (TY-33, the fifteen phantom prelude names) — answered: implement all
  fifteen.** *Not* the plan's ten-and-delete-six split. That includes the six
  graph helpers — `bfs`, `bfs_distance`, `dfs`, `dijkstra`, `a_star`,
  `flood_fill` — which are a graph library and are, by the plan's own sizing,
  more work than the rest of S17 combined. **`panic` must still land first**: it
  typechecks today and then fails to compile, so every other prelude name is
  built on a hole until it is filled. Suggested order: `panic`, `assert`, `dbg`,
  then the eight numeric helpers, then the six graph helpers as their own stage-
  sized unit.
- **D4 (hashability) — still open.** The question asked was what other languages
  do. **Python rejects mutable containers as keys**: `list`/`dict`/`set` set
  `__hash__ = None`, so `{[1,2]: "x"}` is a `TypeError`, and a `tuple` is
  hashable only if every element is. Ruby and most dynamic languages agree;
  Java allows it and documents the result as unspecified once a key mutates.
  **Rust is the counterexample that does not apply here**: `HashMap<Vec<i32>, V>`
  is legal because the borrow checker makes mutating a live key impossible.
  Praxis has `var` mutation and no borrow checker, so it has Python's problem
  without Rust's guardrail — which is why the plan, and this note, recommend
  confirming the rejection (`Y014`, worded per §5.4 so it never names the
  capability). **Do not start TY-32 or RT-08 until this is confirmed.**

**F10's constraint channel is S17's foundation and it is deliberately not
started.** Landing it half-way would be the partial foundation §2 opens by
naming as the main trap: its `Capability::HasMethod` arm is decided by the
method catalog (TY-30's territory), its `Iterable` arm is the one place
`iter_item` needs `&mut TypeDb`, and constraint *discharge ordering* is
judgement the plan flags rather than specifies. It wants a session that starts
with D4/D5/D6 answered, not a vocabulary commit with an arm that returns `Ok`
because its decider is not wired yet.

**D13's block has two fewer numbers than it did.** S11 spent `Y007` (a wrong
type-argument count) and `Y008` (a duplicate field or variant): both are cases
TY-07's fix made *detectable*, and leaving them undiagnosed would have meant
either silently dropping a field or reporting a downstream `Y001` about a type
the user never wrote. The allocation must start from `Y009`. The taken codes
today are `T001`–`T005`, `P001`–`P002`, `N001`–`N002`, `Y001`–`Y008`,
`Y120`–`Y121`, and `I001`/`I010` in the input-parser range. F12 spent no further
code: a wrong type-argument count on a *nominal* def (`Option[Int, Text]`) is
the same mistake as on a collection ctor and reuses `Y007`.

**S12 spent `P002`, and it is not out of D13's block.** The plan lists FE-04's
"statement separator" among the diagnostics D13 must allocate, but categories
are numbered independently and the parse category had only `P001` in it. The
`Y0xx` allocation still starts at `Y009` and is otherwise exactly as the plan
describes it.

**D1 gained a second case.** It was scoped to `Map.get` / `Grid.find`; the same
question is now also open for `min`/`max` on an empty sequence, which return
`0` because their accumulator is a scalar seeded to `0`. MIR-09 gave the three
*seeded* sinks (`reduce`, `min_by`, `max_by`) a fault, because their answer was
genuinely undefined; `min`/`max` have a defined answer that may be the wrong
one. Settle both together.

| | Decision | Blocks |
|---|---|---|
| D4 | Hashability of mutable collections as Map keys | S17 |
| D5 | The 15 phantom prelude names: implement or delete | S17 |
| D6 | `CollectionCtor::Range`: delete or implement | S17 |
| D1 | `Map.get` / `Grid.find` — `Option[V]` or V-with-Unit; and `min`/`max` on an empty sequence. Source-visible | S18 |
| D10 | How much parser-expression grammar a template capture body may contain | S19 |
| D11 | `grid(int)` granularity; greediness of `text`/`word` | S20 |
| D12 | Panic-across-FFI policy — should precede RT-06, RT-07 and the parser findings | cross-cutting |

## 6. Corrections to the plan

Things the plan states that are no longer or were not quite true.

- **HIR-08's minimal fix and its real fix are in different files.** The plan
  warns against shipping the one-line version (add `callee_expr` to the
  destructure) because it fixes the instance and leaves the class — and then
  offers a *stronger* option in a footnote: derive `escaping_vars` from the
  captures the lowerer already holds. That option is the fix that landed, and it
  is better than the footnote suggests: it does not merely make the walk
  unnecessary, it makes the walk **impossible to get wrong**, because there is no
  longer a second computation to disagree with `CaptureKind::ByCell`.
- **HIR-09 is two findings in one line, and only the first is about the error.**
  "Capture analysis still carries obsolete errors" is a deletion. "A capture
  first seen on an assignment LHS can also fall back to a fresh type" is a real
  type bug with a `?T` in the env slot, and it needs the binding's scheme — a
  different fix in a different crate position.
- **Lowering drops three subtrees the child walker can reach.** Found while
  writing F20's coverage gate: `lower_method_call` discards its *receiver* when
  it reports `Y110`, so a closure inside one vanishes from the typed tree
  entirely. Nothing in the audit names it, and it is a symptom of the thing F15
  fixes — lowering re-inferring rather than reading. See §4's item 2.
- **TY-21 needed a diagnostic code, and ADR-051 said it needed none.** That was
  right for the finding as the audit wrote it — "a `loop`'s value is not its
  `break` value" is a missing computation — and wrong the moment **D2** answered
  that a `while`/`for` may not carry a value out, which is a mistake that has to
  be named. The ADR is amended; the pattern to expect is that a *decision* can
  turn a structural fix into a report, and the "needs no code" list is only as
  good as the decisions behind it.
- **TY-21 reshaped no `TypedExpr` variant, so F20 was not a prerequisite.** The
  plan wants F20 landed "BEFORE TY-17/TY-21 add `TypedExpr` variants". Neither
  did: TY-17 changed one `ty` computation and TY-21 changed another. F20's
  consumers are all S15's now.
- **TY-21 is three passes, and only one of them is inference.** The plan's
  sketch is the loop stack; the effort is that the answer has to survive into the
  typed tree (`lower_loop`) and into MIR (a result slot every `break` writes).
  This is the *second* time in S14 that a fix described as an inference bug was
  half a lowering bug — `lower_if` was the first — and the shape is the same: the
  backend reads the typed tree, not the inference state.
- **TY-18 and TY-20 are one stack, and the plan says so.** "TY-18 and TY-20
  share the same fn_stack — build each once" is exactly right: TY-20's `Y011`
  is `fn_results.is_empty()`, one line on top of TY-18's stack. The loop half is
  separate and was a plain counter until TY-21 turned it into the stack of
  flavoured frames the plan describes — which is also what the plan says
  ("TY-20 and TY-21 share the same loop_stack — build each once"; building it
  twice cost one small refactor and no rework).
- **The context boundaries are *function* boundaries, and no finding says so.**
  A closure clears the loop depth (a `break` inside one cannot leave a loop
  outside it) and pushes its own result (a `return` inside one leaves the
  closure). Both fall out of treating a closure as a function, and both are
  wrong if the stacks are treated as lexical nesting.
- **`fn f() -> Int { panic("x") }` was a `Y001` and no finding names it.** It is
  TY-19's rule at a site TY-19's own description does not mention — the function
  result. TY-18 is where it got fixed, because that is where `infer_fn`'s result
  computation was already being rewritten.
- **TY-19's fix is a `join`, and `unify` is untouched.** The audit's wording —
  "`Never` is not a bottom type in unification" — reads as an instruction to
  make `unify` absorb it. Doing that would make `fn f() -> Never { 1 }` type-
  check, and would let a divergent type flow into any position. The plan is
  right and the audit's phrasing is not: absorption belongs at the *branch
  point*, which is `TypeDb::join`.
- **TY-19 is two changes, and only one of them is in `praxis-types`.** Moving
  `Never` out of `ScalarType` touches six exhaustive matches across four crates
  (`fold`, `canonical_key`, `render`, `praxis-repr`, `praxis-hir`'s catalog and
  capability bridges). The plan sizes TY-19 **L** and that is about right, but
  the effort is in the sweep, not in the join, which is fifteen lines.
- **`lower_if` needed the join too, and no finding says so.** TY-17 is written
  as an *inference* bug. Inference joining was not enough: `lower_if` computed
  the `if`'s type from the then-block alone, so the typed tree carried `Never`
  for `if c { panic("x") } else { 1 }` even once inference was happy. The exit
  test's second assertion is what catches it; a fix that only touched
  `infer_if` would pass the first and fail the second.
- **TY-16's fix is `lower_block`'s shape, not a new rule.** The audit describes
  inference "retaining the last expression it saw". Lowering already had the
  right rule — a pending tail that any following statement demotes — so the fix
  is to make the two passes share one rule rather than to invent a second.
- **TY-08 is an AST-accessor bug, not an inference one, and its scope is the
  `cast`.** The audit's wording — "AST type accessors cast only `TYPE_REF`" — is
  exactly right, and the fix is eleven lines. The plan sizes it **L** and §4 of
  this file called it "the big one"; it was the smallest of the eight. What made
  it look large is that it *gates* five exit tests. Two adjacent gaps surfaced
  only once the nodes arrived (a parenthesized group resolved to nothing, and
  `()` in a parameter group resolved to nothing), and both are two lines.
- **TY-08 does not unblock `empty_vec_float_…`.** §4 said its `#[ignore]` reason
  "already names TY-08". The reason was wrong: inference applies the annotation
  (pushing an `Int` into `let values: Vec[Float] = Vec()` is a `Y001`), and what
  loses the element type is `lower_call` re-instantiating the callee's scheme.
  F15/MONO-01, S15.
- **F19 is two independent halves, and S13 needs one.** The plan's F19 block
  describes a sealed `TypeEnv` *and* a `DeclGroup` with SCC-ordered binding
  groups, and lists seven findings under "SEVEN FINDINGS, ONE PASS". Only the
  environment is load-bearing for S13: TY-09 needs it total, TY-10 needs the
  type declarations ordered, TY-13 needs `ScopeTree` out of the `Inferer`. The
  SCCs are over the **call graph** and exist to generalize mutual recursion
  correctly — a residue S11 recorded and no S13 finding names. TY-01 and TY-22
  were already closed in S11 without them.
- **F19's `ScopeTree::bind` returning the displaced symbol was not needed.**
  The sketch has `bind` answer `Option<SymbolId>` so `register_top_level` can
  diagnose a displaced `Fn`. S13's earlier half used `is_bound_here` instead,
  which asks the question directly rather than inferring it from a return value
  nobody else wants; `bind` still returns `()`.
- **The plan's `check_type_annotation` note assumes the scope is the answer.**
  §4 said "it resolves through the scope tree, so the symbol's `SymbolKind` is
  right there". True — but the *inferer* also needs that symbol, and it has no
  scope tree after TY-13. So resolution records the answer
  (`NameResolution::type_refs`) rather than each pass computing it.
- **S13's "biggest source of newly-rejected valid-looking programs" did not
  materialize.** The plan mandates one corpus triage pass at the end of the
  stage for exactly this; it found nothing. Every `.px` in `tests/` and every CLI
  fixture still checks clean, and the only new rejection class — `Y009`,
  assignment to a non-`var` — has no instance anywhere in the corpus or in
  `jit.rs`. That is the **fifth** F-block or stage to predict wide churn and
  deliver none (TY-02, F12's `Option[Int]` rendering, FE-02, F8, S13). Budget
  accordingly.
- **The plan's list of taken diagnostic codes was incomplete.** D13's text and
  §5 of this file both said "`N001`-`N002`, `Y001`-`Y008`, `Y120`-`Y121`, and
  `I001`/`I010`". The tree also had `N000`, `Y110`, `Y112`, `I000`, `I020`-`I027`
  and `I030`, plus the whole `T0xx` and `P0xx` categories. ADR-051 opens with the
  corrected inventory; F2's `every_code_is_distinct` is what keeps it honest from
  here.
- **F2's `DiagCode` variant *order* cannot drive the numbers.** The plan's sketch
  lists the variants in a natural reading order and implies the numbers follow;
  `Y110`/`Y112`/`Y120`/`Y121` shipped years earlier and are fixed points, so the
  numbers come from ADR-051 and only the shape comes from the sketch.
- **F2 is bigger than "S · praxis-source"** by one crate the sketch does not
  name: `praxis-input-parser`'s `ValidationError.code` was a `&'static str` that
  `praxis-hir` parsed back into a number with `unwrap_or(0)`. A typo in it was a
  silent `I000`, and the plan describes F2 as touching `praxis-hir`,
  `praxis-input-parser`, `praxis-parser` — but as *consumers*, not as a crate
  with its own parallel code representation to delete.
- **TY-23's fix is in resolution, not inference.** The audit says "a nested
  function item reaches `expect` and panics", which reads as an inference bug.
  Inference panicked because *resolution* never declared the function and never
  said why. Removing the `expect` alone would have made a nested `fn` silently
  vanish; the report has to come from where the nesting is visible.
- **TY-24 cannot be checked with `ScopeTree::lookup`.** A redeclaration and a
  shadow both find a symbol — `let out = 1` finds the prelude's `out`. The check
  needs "bound in *this* scope", which did not exist; `is_bound_here` is new.
- **F8's "HIGHEST TEST CHURN of any foundation" did not happen — at all.** The
  plan budgets "~40 insta snapshots in `parse.rs`, plus every
  `crates/praxis-cli/tests/fixtures/run/*.px` with same-line statements", and
  S12's own paragraph repeats it. The suite passed unchanged: every fixture,
  every snapshot and every inline test source already separated its statements
  with newlines, which is precisely what the new rule demands. This is the
  **fourth** F-block to predict wide churn and deliver none (TY-02, F12's
  `Option[Int]` rendering, FE-02, F8). Check before budgeting for it.
- **`StmtSeparator::Semicolon` carries no `Token`.** The plan's sketch is
  `Semicolon(Token)`; nothing reads it, and an unread enum field is a
  `dead_code` error under `-D warnings`. The three variants are unit variants.
  The property the plan wants from the type — that the loop cannot advance
  without producing a separator or emitting a diagnostic — is unaffected.
- **F8's `newline_before` reads the *token*, not the trivia.** The flag is set
  on the token after the run, so the answer survives the trivia having already
  been emitted into the green tree — which matters, because `current_span` and
  `bump` both call `eat_trivia` and the separator check runs after them.
- **The match-arm loop is F8's fifth replacement and the plan does not list it
  as a finding.** F8's "Replaces" mentions `parse.rs:955-977`, but FE-04's own
  description is only about statements. Arms now demand a comma or a line break;
  `match x { A => 1 B => 2 }` used to parse and is now a `P002`.
- **H13's example does not mis-parse the way it says.** The hazard claims that
  allowing struct literals in arm bodies before arm separation is newline-aware
  mis-parses `match x { A => Point { x: 1 } B => … }`. Traced against the real
  grammar, the record literal parses correctly and `is_pattern_start(B)` finds
  the next arm either way. What FE-04 actually buys is that the *missing comma*
  is now reported instead of silently accepted. The ordering is still right —
  FE-04 first — it is just cheaper than advertised.
- **FE-06 is not "make the flag scoped"; the flag had to go.** A parser-wide
  `bool` cannot express "until the ambiguity ends", only "until this call
  returns" — which is why it leaked through parentheses *and* into arm bodies
  from two different call sites. `StructLit` as a parameter is what makes "does
  this leak?" answerable by reading one call chain. See ADR-050.
- **DBG-03's fix is to reject, not to rename injectively.** The audit describes
  the finding as "maps every invalid name to `_x`, creating collisions", which
  reads as an argument for unique renaming. There is nothing to rename *to*: a
  `p EXPR` cannot mention a name the language cannot spell, so a local with one
  has no use as a parameter. Dropping it is what `collect_bindings` already does
  for a local whose `type_id` this `TypeDb` never minted.
- **F12 is two halves in two stages, and only the static one is S11's.** The
  plan's F12 block describes the `praxis-types` reshape and the runtime
  `EnumSchema` together, and its ORDER paragraph says codegen and runtime "MUST
  land in one commit". Both are true of the *runtime* half — but that half is
  **RT-13**, which the finding register puts in **S18**, and which S18's own
  ordering paragraph says depends on TY-06's canonical `Option` def from S11.
  Landing it in S11 would have been landing S18 out of stage order. What S11
  owes is TY-06, and TY-06 is entirely in `praxis-types` and its consumers.
- **F12's `params` belong on `RecordDef` as well as `EnumDef`, and nothing uses
  them.** The plan's sketch gives both defs a `params: Vec<VarId>`, which is
  right for symmetry — but the language has no generic record syntax, so every
  record def has an empty list and every record instance has no arguments. The
  cost of the symmetry is two sites that must refuse a shape they cannot handle
  (see §4); the benefit is that `fold`, `unify` and the substitution helpers are
  written once for both.
- **F12's `TypeKey` needs no cycle memo, and `canonical_key` is not a fold.**
  The plan annotates it "a `fold`", but `fold` returns a `Type` and needs
  `&mut TypeDb`; a key describes a type without creating one, so it is a plain
  recursion over `&self`. It terminates because a nominal type's key holds its
  *def id* rather than its fields — the side tables are never walked — which is
  also what makes it cheap.
- **F12's canonical `Option` has to be seeded by `TypeDb::new`, not threaded
  from `seed_builtin_schemes`.** The plan's judgement note proposes threading a
  def-id "from `seed_builtin_schemes` through the Inferer and the parser
  lowerer". The parser lowerer's `synthesize` has no Inferer to thread from —
  it takes a bare `&mut TypeDb` — so the def has to belong to the arena. Two
  consequences a fresh context will hit: `TypeDb` needs a hand-written
  `Default`, and **`enum_defs[0]` and slot 0 are now the prelude's**, so a test
  that indexed either positionally moves.
- **MONO-03 closed with TY-06, three stages before the one that owns it.** The
  plan schedules it in S15 with the rest of monomorphization. Its cause is
  entirely F12's — a display string standing in for identity — so once `TypeKey`
  exists the fix is one type substitution in `mono.rs`. Its `#[ignore]`d gate
  `enum_payload_types_participate_in_monomorphization_cache_key` went green
  unmodified. What is left for S15 is the other two ignored mono tests
  (`specialized_clone_carries_concrete_types_throughout`,
  `zero_argument_generic_result_is_specialized_from_use_context`), which are
  MONO-01/MONO-02 and are about *substituting* the clone, not about keying it.
- **The plan expects snapshot churn from `pretty.rs` printing `Option[Int]`.
  None appeared** — no snapshot in the tree held a rendered `Option`, and none
  of `infer_tests.rs`/`hover_tests.rs`'s insta assertions did either. This is
  the second time an F-block predicted wide churn and delivered none (TY-02 was
  the first); check before budgeting for it.
- **F12's fix for TY-06 is not "stop calling `register_enum` from
  `instantiate_walk`".** The plan's Replaces list reads that way. The call is
  correct *as a specialization* for a def whose field types are its own
  children, and the anonymous structural records `deep_resolve` walks need it.
  What makes it wrong for `Option` is that `Option`'s payload is a *parameter*.
  So the fold branches on `params`, and both behaviours survive — which is also
  why `an_unchanged_record_keeps_its_def` and
  `records_and_enums_are_folded_through_their_defs` still pass unchanged.
- **The M9 same-named-enum unification arm was load-bearing for anonymous
  enums too.** The plan treats it as pure workaround. `choice(...)` templates
  (§7.5) still mint one anonymous def per synthesis, and they merge by variant
  signature exactly as anonymous records merge by field-name set — so the arm
  narrows to `name.is_none()` rather than disappearing.

- **F5's `register_record`/`register_enum` could not keep taking a bare name.**
  The plan's signature is `register_record(&mut self, name: Option<String>,
  fields: FieldSet) -> RecordDefId`, which is what landed — and it makes
  `anon_record`/`anon_enum` redundant, so the four constructors are two. The
  knock-on the sketch does not mention: `EnumDef.name` had to become an
  `Option<String>` too, because the synthetic `""` an anonymous enum carried was
  the same absence spelled as a value, and `unify`'s same-name arm compares it.
- **F5 needs two diagnostic codes, and the plan allocates none.** Validating in
  the constructor makes two previously-invisible mistakes visible — a wrong
  type-argument count and a duplicate member — and both are user input. They are
  `Y007` and `Y008`; see §5 for what that costs D13.
- **F5's `CollectionArgs` does not make `collection` infallible.** The plan
  keeps the `Result` and it is right to: `CollectionArgs::Unary(t)` is legal to
  write and `Map` does not take it, so the shape and the ctor can still
  disagree. `CollectionArgs::new(ctor, args)` is the checked builder; the
  constructor re-checks.
- **`synthesize` genuinely needs the `Result` the plan asks for, and not only
  for tidiness.** Its record and enum shapes take their *names from user source*
  — `{x:int}` captures, `choice(...)` cases — so a duplicate is user input.
  `validate` catches those cases today, which is why the plan hedges; threading
  the `Result` is what keeps the two from drifting apart silently.
- **F10 has two halves and only one belongs to S11.** The plan lists the
  constraint channel, the `Level` newtype and scheme-owned binders as one
  foundation. TY-01/TY-03/TY-22 need the second and third; the first
  (`CapKind`, `Capability`, `Constraint`, `take_dischargeable`, the exhaustive
  `capability::check`) has no consumer before S17, and the plan's own
  finding-to-stage mapping puts every one of them there.
- **F10's `Scheme` sketch has `binders` and `constraints` as struct fields with
  accessors.** Only `binders`/`body` landed; `constraints()` arrives with the
  channel. `instantiate_with_mapping` and `generalize_at` landed early because
  the reshape made them one line each.
- **TY-01 and TY-22 are one edit *because of the `Level` newtype*, not only
  because of the placeholder.** The plan says correcting `lower_levels` requires
  moving the recursive placeholder; the mechanism is that the placeholder was
  minted at the *outer* level, so the corrected clamp pulls every parameter and
  result out to level zero and no signature generalizes. `fn id(x) { x }` used
  at two types is the shortest program that shows it.
- **TY-22's fix does not need F19.** The plan routes forward declarations
  through F19's `DeclGroup` driver (S13). Predeclaring a signature placeholder
  per top-level `fn`, at a group level, closes the finding on its own — what
  F19 adds is *dependency-ordered* groups, which is what mutual recursion needs
  and forward checking does not.
- **TY-04 needed no work of its own.** F9's fold walks record fields and enum
  payloads, which is the entire finding; `deep_resolve_rewrites_record_field_links`
  went green with F9. The plan orders it after TY-06 (F12), which is only
  necessary if the fix has to know about nominal args — it does not.
- **TY-05's gating test could not survive its own fix**, for the same reason
  RT-17's could not: it compared the two spellings the fix collapses into one.
  Rewritten to assert the collapse. This is the third time in the repair
  (`setting_none_cannot_create_a_pending_fault`,
  `heap_element_requires_a_runtime_compatible_ordering`) — when a fix makes an
  invalid state unrepresentable, expect its gate to need rewriting rather than
  un-ignoring, and expect plan §8.2's list of five to be an undercount.
- **`unify_vec_mismatched_arity_fails` belongs on plan §8.2's list too.** It is
  green today and its *setup* builds the type TY-07 makes unrepresentable, so
  un-ignoring is not the issue — it stops compiling. Rewritten as
  `a_wrong_arity_collection_is_unconstructible`.
- **F9 landed before F5, not after it.** The plan orders F9 after F5 "because
  the folders must re-intern through the checked constructors". Nothing about
  the fold depends on that: the default arms rebuild through
  `intern`/`register_record`/`anon_record`/`register_enum`, and F5 changes those
  signatures in one place each when it lands. Landing F9 first bought TY-02 and
  `deep_resolve`'s missing arms immediately, and gave F10/F12 a fold to be
  expressed as.
- **F9's sketch has one folder shape; two are needed.** The rebuilding defaults
  are right for `instantiate` and `deep_resolve` and *wrong* for the three
  inspection-only walks, because pruning a linked child counts as a change — so
  a level-lowering walk over `Vec[?a→Int]` would intern a fresh `Vec[Int]`
  during unification. `visit_only_composites!()` is the second shape: descend
  for effect, rebuild nothing.
- **F9's `Folded { Unchanged, Rebuilt(Type) }` is not public API.** The change
  flag lives inside the default arms; nothing outside needs to name it.
- **F9's `fold_record`/`fold_enum` take no `args`.** The plan's signatures are
  post-F12; `TypeData::Record { def }` has no arguments today.
- **The plan expects snapshot churn from TY-02's identity preservation
  ("programs that currently depend on spurious var freshness will infer
  differently"). None appeared** — the whole suite passed unchanged. Worth
  knowing before assuming a later identity change is safe for the same reason:
  this one was, and it was checked rather than assumed.
- **F5's sealing half is much smaller than the plan's L estimate.** F16 already
  converted MIR's forty `Type(0)` sites to `MirType::Opaque`, so three forged
  handles remain workspace-wide (`render.rs`, `evaluate.rs`, `exhaustive.rs`).
  The validated-constructor half is the work that is left.
- **P0-12 is an *equality* bug too, and the audit describes only the ordering
  half.** "MIR sends every non-Float ordering operation through integer
  extraction" is true of `==` as well, and for `Text` the consequence is worse
  than a wrong order: `TextPayload` is a `#[repr(C)]` enum, so the eight-byte
  load reads the *discriminant*, and every pair of owned strings compared equal.
  `m9_option_text_payload` passes today and has always passed for that reason —
  `s == "hi"` was true for any `s`. A stage that fixes only the ordering
  operators leaves that in place.
- **The plan's `praxis_struct_cmp` is `praxis_value_cmp`, and it is not
  composite-only.** F4's sketch names it beside `StructEq` and P0-12's exit
  criteria are all scalars; what the codebase needed is a *general* descriptor
  dispatch, because the type that cannot be compared in the scalar channel is
  `Text`. `Inst::ValueCmp` carries no root set — the wrapper is
  `Effect::Faults`, so unlike `StructEq` it is not a safepoint.
- **P0-12's fix is "restrict ordering" *and* "add compare callbacks", not
  either/or.** The audit offers them as alternatives. The callbacks are what
  `Text`/`Char`/`Float` need; the restriction is what tuples and records need,
  because a callback for them is a language decision (ADR-045 decision 1). Doing
  only the first leaves `(1, 2) < (1, 3)` comparing schema pointers; doing only
  the second leaves `"a" < "b"` comparing addresses.
- **The plan's D3 text says the pointer-vs-id contradiction must be resolved
  "if duplicated consts are real".** It was resolved in S1: descriptors are
  `static`, so pointer identity is authoritative (ADR-038). What D3 still had to
  answer was NaN and composites.
- **S10's exit criteria list one test the stage cannot pass as written.**
  `char_ordering_uses_unicode_scalar_values_without_out_of_bounds_reads` feeds
  `aβ\n` to `read grid(char)`, and the cell parser scans bytes — the input is a
  parse error whatever the ordering does. `read grid(char)` is also the *only*
  source of `Char` values (`Text.get` returns the scalar value as an `Int`, and
  there is no char literal), so the non-ASCII half cannot be written in the
  language at all. The test now uses `ab\n`, and the non-ASCII property is
  pinned at the runtime level.
- **Two of S10's exit-criteria tests were already un-ignored before the stage
  started**: `regression_runtime_scalar_descriptors_recover_their_actual_types`
  (S1) and `regression_runtime_vec_descriptor_recovers_its_real_element_type`
  (S7's DBG-02 half).
- **RT-12 needs no `TypeDb` work, and F12 is not its prerequisite.** The plan
  routes RT-12 through F12's `DefId + args` key, which is XL and belongs to S11.
  The runtime half stands on its own: `SchemaIdentity::Nominal(&'static str)`
  holds the *declared name*, which is what distinguishes `Point` from `Vector`,
  and comparing the field shape alongside it is what keeps one name over two
  shapes (a generic instantiation, a reloaded definition) apart. F12 replaces the
  name with the real key when nominal identity gains type arguments; nothing else
  about `same_type` changes.
- **F12's sketch has `SchemaIdentity::Nominal(u64)`, a generation-scoped def
  key.** A `&'static str` is what landed: the schema already holds interned
  `&'static str` field names, comparing by content cannot collide, and there is
  no key registry to build. `#[repr(C)]` either way.
- **`heap_element_requires_a_runtime_compatible_ordering` (`infer_tests.rs`)
  belongs on plan §8.2's list of tests that assert the bug.** It is `#[ignore]`d
  rather than green, so it does not fail as a regression — but its assertion
  ("a `MinHeap[Text]` must be a type error") is *inverted* by the fix, and
  un-ignoring it would be asserting the defect. Rewritten to assert the
  agreement instead. Its sibling `heap_element_must_be_orderable` (a *function*
  in a heap) stays ignored: nothing enforces element orderability at a method
  call yet, which is S17's constraint channel.
- **F17's `RootSlots` sketch is one field short.** `unannotated()` / `iter()` /
  `is_annotated()` / `set()` describe the *live* set only, but MIR-01 needs a
  second one: which slots to **null**. Nulling every non-root slot at every
  safepoint would cost `gc_count` stores per safepoint, so `RootSlots` carries
  `live` and `dead`, and `dead` comes from a forward may-dataflow over "which
  slots might still hold a value". The plan describes MIR-01 as "dead-slot
  clearing" without saying where the list comes from; it is a third analysis,
  not a filter on the second.
- **F17's `verify` cannot have all the rules it lists, and two of the absences
  are decisions.** `ScalarLiveAcrossSafepoint` fires on every `lower_seq_*`
  accumulator — F17 predicts this and asks for an explicit decision, which is
  ADR-044 §5: the rule is not implemented, because a scalar is a *copy* of a
  payload and cannot dangle, so the invariant that matters is already stated by
  `RootIsNotGc` + `MoveGcFromScalar`. `MissingTerminator` is unrepresentable —
  `Block.term` is not an `Option` — so there is no rule rather than a heuristic
  about placeholder self-jumps.
- **F17's "effect-driven safepoints" did not land, and nothing needed them.**
  The `Inst` shape still decides what is a safepoint;
  `CallTarget::Runtime(sym).allocates()` is not consulted. The two agree at
  every site today. `AllocatingCallNotASafepoint` therefore has no rule.
- **`MirType::expect_known`/`MirTypeError` do not land in S9.** The plan puts
  them here because F17's verifier is their only consumer — but that consumer
  is `OpaqueAtDescriptorSite`, which H10 defers. Writing the API without its rule
  would be adding unused surface. (S9 said "they land in S15"; S15 could not turn
  the rule on — §4's S15 entry says why.)
- **The plan says "two loop-increment `IntBinOp` sites"; there are eight
  compiler-written sites**, and they split three ways. Three loop index bumps,
  two `count` accumulators and one `+ 0` scalar copy are bounded by a
  collection's length and are now `Overflow::Bounded`. Two are `sum`/`product`
  accumulators, which *can* genuinely overflow and are `Checked` — and are still
  not followed by a `CheckFault`, so the fault is observed after `main` returns
  rather than at the sink. That residue is why the verifier has no "every
  faulting instruction is observed" rule.
- **MIR-01 needed the accumulator initialization MIR-09 brought, not just the
  clears.** `reduce`/`min_by`/`max_by` left a `Gc` slot unwritten on the empty
  path, and liveness roots it at the loop header — so the backend spilled an
  *undefined Cranelift value* into the shadow frame for the collector to
  dereference, on every empty `reduce`, whether or not anyone read the result.
  The audit describes MIR-09 as a bad return value; it was also a live rooting
  bug.
- **MIR-09's raise wrapper cannot be `Void`.** `call_symbol` hands back the
  *context pointer* for a void wrapper (a deliberate simplification), so an
  `Inst::Call` to one writes `ctx` into its `Gc` destination — the exact class
  of bug this stage removes. `praxis_raise_empty_collection` is
  `(Ctx) -> Gc, Faults` and returns the Unit sentinel. H16's "MIR-09 adds
  `praxis_raise_empty_collection`" is otherwise right, and it really was two
  edits.
- **S9's ABI bump is for a layout that did not change.** `DebugLocal.value`
  went from `GcRef` to `Option<GcRef>`, which is the same word at the same
  offset. What changed is the *meaning*: "no value yet" moved from
  `NonNull::dangling()` to the all-zero `None`, and generated code now writes
  zero into a dead shadow slot. A previous-version runtime reads either zero as
  a reference. The rule "check what generated code actually reads" cuts both
  ways — a stable layout is not automatically a free stage.
- **F18's `DebugLocalMeta` sketch is already what shipped**, in S6: the
  `NO_STATIC_TYPE` half landed with `MirType::Opaque`, not here. Only the
  `DebugLocal.value` half was S9's, and the fault-epilogue half was S3's.
- **F13's `Generation::retire(self, HeapDrained)` signature could not be
  written as sketched.** A `Jit` holds the generation behind an `Rc` (the
  debugger shares one across every `p EXPR`), so it is
  `Generation::retire(Rc<Generation>, HeapDrained)` and a still-shared
  generation is left alone rather than freed. The plan's `#[must_use]` is also
  not what landed: Rust has no linear types, so the enforcement is that
  `Generation::drop` *leaks*. Reclaiming needs the proof; forgetting to reclaim
  costs memory, not soundness.
- **F13 says "every `Box::leak` becomes `gen.alloc*`". Reclaiming is not enough
  on its own.** A debugger session never ends, so DBG-05's "must not grow
  without bound" needs *interning*, and needs the `p` path to share one
  generation rather than mint one per command. Both landed; the plan sketch
  mentions neither.
- **F13's `PlanArena` cannot live in the codegen generation.** Plans are
  registered during HIR lowering, before a `Jit` exists. It is a process-wide
  bounded arena in `praxis-input-parser` instead, retired through
  `praxis_runtime::retire_parser_plans` — the proof has to be applied from
  `praxis-runtime` because `praxis-input-parser` cannot depend on it (the
  interpreter points the other way).
- **F13's "39 workspace-wide" leak count included test helpers.** The production
  sites S8 owed are all converted; what remains under `Box::leak` in non-test
  code is `tuples::POINT` (one process-static schema, bounded at one) and the
  `Box::leak(Box::new(rt.context()))` idiom in in-crate tests.
- **S8's exit criterion "leak_static_str must no longer exist" is structural.**
  It is deleted, and there is no test for it because there is nothing left to
  test — its callers take a `&Generation`.
- **S8 needed no ABI bump.** `RecordSchema`, `TupleSchema` and `DebugLocalMeta`
  keep their `#[repr(C)]` layouts; only their storage moved. Check what
  generated code actually reads before spending a stage's bump.
- **DBG-01 is closed** (`3836b74`), not open in S10. **DBG-02 is closed for
  values**: the debugger reads a value's real type out of its payload through
  F11's `type_for_value`, and its gate
  `regression_runtime_vec_descriptor_recovers_its_real_element_type` is
  un-ignored. What remains of DBG-02 in S10 is the *value-less* half — a record
  or enum object does not record which nominal type it is (F12), and a closure
  records no signature. Both report why rather than guessing.
- **S7's exit criteria list one test that S7 cannot pass.**
  `empty_vec_float_has_the_float_element_descriptor_before_any_push` needs the
  `let values: Vec[Float] = Vec()` annotation to reach the *construction site*.
  P0-11 makes the descriptor honestly *null* instead of wrongly `Int`; it cannot
  make it `Float`. S7 attributed the gap to TY-08 and S13 disproved that —
  inference applies the annotation fine. `lower_call` re-instantiates the
  callee's scheme instead of using the type inferred at the call site, so it is
  **F15/MONO-01 in S15**.
- **P0-11's "expect passing tests to flip" was half right.**
  `vec_float_push_adopts_float_descriptor_and_preserves_signed_zero_semantics`
  (adversarial_audit.rs, the test H18 names) **still passes unchanged**, because
  its `let a = Vec()` genuinely has no static element type and adopt-on-first-push
  is the honest answer there. What P0-11 removed was *retagging* a vector that
  had been told its type. The test that actually needed rewriting was
  `tuple_schema_uses_the_unit_descriptor_for_unit_elements`, and for an unrelated
  reason (FE-04, see §1).
- **A test that derefs a payload pointer can abort the whole test binary.**
  `empty_vec_float_…` did, once the descriptor became legitimately null: a null
  deref in Rust is a non-unwinding panic, so one bad assumption took out
  thirty-six other tests in the same process with SIGABRT and no failure list.
  Plan §8.1's "do not batch the ignored suite" is about this class; the cheap
  defence is to read through the `Option` accessor.
- **§8.4 is stale for P0-05 and P0-14.** Both now have standing gates in the
  ordinary suite. A Miri job is still worth adding.
- **F1's `compare` is declared but unpopulated.** S10 does not need to touch 21
  descriptors, but it does need to answer D3 before it can populate one.
- **Line numbers throughout the plan have moved.** Search by test name.
- **F6's "batch the ABI bump with F7 and F18" is not what happened.** H17's
  one-bump-per-stage rule and S5's own exit criterion ("bump exactly once here
  for the native_roots field") both say per stage, so S4 bumped to 8 on its own.
  S5 bumped to 9, S6 to 10. **S6's second half needed no bump**: `Heap` gained a
  field and `TypeDescriptor` gained a field, but generated code holds the `Heap`
  pointer without dereferencing it and passes descriptors by pointer without
  reading their fields. Check what generated code actually reads before spending
  a stage's bump.
- **F6 says `Heap::mark` should debug-panic on a foreign root.** It skips
  instead. A panic would fail `foreign_heap_root_cannot_delay_reclamation`
  (which expects the collection to complete), and it would crash the debugger's
  second-heap configuration, which is the one place this actually arises.
  Reasoning in ADR-039.
- **F4's `AbiRet` sketch has three variants.** It needs a fourth, `Ptr`:
  `praxis_closure_fn_ptr`, `praxis_push_shadow_frame` and
  `praxis_push_debug_frame` return pointers, not `GcRef`s.
- **F4 lists `MethodLowering::Intrinsic(Intrinsic)`.** `Intrinsic` is still
  `&'static str`; only the `RuntimeSymbol` arm was typed. A real `Intrinsic`
  enum is worth doing when S21 touches the pipeline lowerings.
- **P0-13's exit criterion "a symbol absent from the manifest fails to compile"
  is now structural**, not a test: `CallTarget::Runtime` takes a `RuntimeSymbol`,
  so an absent symbol is unspellable. The tests that exist assert the property
  that replaced the bug — signatures derived from rows, narrow params declared
  narrow, void wrappers with no result.
- **P0-08's raise symbols take a predicate.** The plan says "two non-allocating
  raise symbols"; they are `praxis_raise_int_overflow_if` and
  `praxis_raise_div_by_zero_if`, each `(ctx, condition: i64) -> void`. Taking
  the condition instead of branching around the call keeps an arithmetic site
  in one basic block, which is what let native lowering land without touching
  the block bookkeeping. If a future stage prefers the branch form, nothing
  about the fault protocol changes.
- **The parser interpreter still cannot collect, and that is now enforced by a
  name.** H1 lists `parser.rs`'s unrooted `Vec<GcRef>` intermediates alongside
  the grid helpers, but the two are not in the same state: the grid helpers call
  `praxis_*` wrappers (which pace), while `parser.rs` allocates through
  `heap_ref(ctx).alloc_unpaced` / `alloc_with_unpaced`, which is the only route
  that skips pacing. Its intermediates are latent, not live. **IPR-14 (S20) must
  give them `NativeScope`s in the same commit that moves those nine call sites
  to the paced `alloc`/`alloc_with`** — that is when H1's mechanism actually
  arrives for the parser, and when the back door loses its second caller.
- **S6's exit criteria were sharper than the plan's list.** The plan names five
  ignored tests; three were already un-ignored before this session, and P0-08b,
  P0-08c and RT-04's second half had no gating test at all (plan §8.3 says so
  for RT-05 and P0-08c only). Ten new gates were written; §1 lists them.
- **P0-08b is "fourteen wrappers", but RT-03 was twenty-four more.** The audit
  scoped RT-03 as "Bool/Unit ABI *helpers*"; in practice `praxis_alloc_bool` and
  `praxis_alloc_unit` were two of twenty-six sites calling `alloc_immortal`, and
  the other twenty-four — every comparison, `contains`, `is_empty` — are the
  ones a program calls in a loop. A stage that reads a finding's one-line
  summary as its extent will under-fix it.
- **RT-17's gating test could not survive its own fix.**
  `setting_none_cannot_create_a_pending_fault` asserts a *runtime* property —
  `set(None)` leaves the fault not-pending — and the fix made the call
  unspellable, so the assertion has no code to run against. §8.2's list of
  bug-pinning tests does not include it, but it belongs there in spirit: when a
  fix promotes a runtime check to a compile-time one, expect the gate to need
  rewriting rather than un-ignoring.
- **RT-06 needed a newtype, not a validating constructor.** Enum *variant*
  fields cannot be private in Rust, so a `TextPayload::slice(...) -> Option<Self>`
  associated function leaves `TextPayload::Slice { .. }` just as spellable as
  before. The validated `SourceSlice` struct is what actually closes it — worth
  remembering wherever the plan says "make X unconstructible" about a variant
  (F14's `TextSlice`, IP-10's `NonEmptySeparator`).
- **P0-08c's exit criterion is partly obsolete.** "an assert that the effect
  table covers every symbol registered in symbols.rs" — `symbols.rs` no longer
  has a list (F4 deleted it; `resolve` is `from_name` + `address`), and
  `MethodEntry.allocates` was already deleted. What remains, and what landed, is
  a `const` block over `RuntimeSymbol::ALL` in `praxis-stdlib/src/abi.rs`.
  `every_manifest_symbol_resolves_to_a_distinct_address`
  (`praxis-codegen-cranelift/src/symbols.rs`) already covered the other half.
- **P0-08 raised a question the plan does not answer.** The two loop-increment
  `Inst::IntBinOp` sites in `build.rs` (the `for`-loop index bump) are *not*
  followed by a `check_fault`. An overflow there now sets a pending fault that
  is only observed at the next check. That is harmless (faults are sticky and
  the index would have to reach `i64::MAX`), but a MIR verifier rule "every
  faulting instruction is followed by a CheckFault" — MIR-10, S9 — would flag
  it, and the right answer is probably to mark the increment non-faulting
  rather than to add a check.
- **`MethodEntry.can_fault` is dead metadata.** Nothing reads it — every method
  call emits an unconditional `check_fault` in `build.rs`. That is why RT-07's
  and P0-11's new faults are observed by generated code without touching a
  lowering, and it is also why the field's `false` values (`bitset_insert`,
  `vec_push`) were never wrong in a way anyone noticed. F17's verifier (S9) is
  where "faulting instruction ⇒ CheckFault" becomes a real rule; either wire
  `can_fault` to the manifest's `Effect` there or delete it.
- **Adding a `FaultKind` variant needs no ABI bump.** Generated code never
  switches on the kind — it calls `praxis_check_fault`, which answers a bool —
  and the `#[repr(C)]` enum's width does not change. Four variants have been
  added across S7 (`InvalidChar`, `InvalidText`, `InvalidSize`, `TypeMismatch`)
  under one bump, which was spent on the `Fault` *repack*, not on the kinds.
