//! Type-inference tests (Slice 5).
//!
//! These cover the §19-M2 acceptance criteria that inference owns: inferring
//! function parameter and return types from use (criterion 1), and rejecting
//! cross-type `var` reassignment (criterion 4). They also snapshot inferred
//! schemes/types (§17.1 "inference snapshots").

#![cfg(test)]

use praxis_parser::parse;
use praxis_source::{DiagnosticCategory, SourceMap};

use crate::{analyze_root, SymbolKind};

fn analyze(text: &str) -> crate::Analysis {
    let map = SourceMap::new();
    let id = map.intern("infer_test.px", text);
    let parsed = parse(id, text);
    analyze_root(id, &parsed.tree)
}

/// The rendered scheme of the user binding named `name` (a Let/Var/Fn/Param),
/// or `None` if it has no scheme.
fn scheme_of(text: &str, name: &str) -> Option<String> {
    let analysis = analyze(text);
    analysis
        .names
        .all()
        .iter()
        .filter(|s| s.name == name && s.kind != SymbolKind::Builtin)
        .find_map(|s| s.scheme.as_ref().map(|sc| analysis.db.render_scheme(sc)))
}

/// The type of an expression, observed by binding it: `let _probe = <expr>`
/// and reading `_probe`'s scheme. Returns the rendered type.
fn expr_type(expr: &str) -> String {
    let src = format!("let _probe = {expr}");
    scheme_of(&src, "_probe").unwrap_or_else(|| panic!("no scheme for expr `{expr}`"))
}

fn has_type_error(text: &str) -> bool {
    analyze(text)
        .diagnostics
        .iter()
        .any(|d| d.code().category() == DiagnosticCategory::Type)
}

fn has_name_error(text: &str) -> bool {
    analyze(text)
        .diagnostics
        .iter()
        .any(|d| d.code().category() == DiagnosticCategory::Name)
}

fn has_input_error(text: &str) -> bool {
    analyze(text)
        .diagnostics
        .iter()
        .any(|d| d.code().category() == DiagnosticCategory::Input)
}

/// Like [`has_type_error`] but also runs lowering, so diagnostics emitted during
/// lowering (e.g. exhaustiveness Y120/Y121, which need the lowered patterns) are
/// included. The exhaustiveness checker runs in `lower()`, not `analyze()`.
fn has_type_error_with_lower(text: &str) -> bool {
    use praxis_ast::AstNode;
    use praxis_parser::parse;
    let map = SourceMap::new();
    let id = map.intern("lower_test.px", text);
    let parsed = parse(id, text);
    let mut analysis = analyze_root(id, &parsed.tree);
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
    let module = crate::lower::lower(id, &root, &mut analysis);
    analysis
        .diagnostics
        .iter()
        .any(|d| d.code().category() == DiagnosticCategory::Type)
        || module
            .diagnostics
            .iter()
            .any(|d| d.code().category() == DiagnosticCategory::Type)
}

/// Whether analysis and typed-HIR lowering both accepted `text` without any
/// diagnostic category. Useful for invariants (missing fields, illegal
/// assignment) where the implementation does not yet have a dedicated code.
fn is_clean_with_lower(text: &str) -> bool {
    use praxis_ast::AstNode;
    let map = SourceMap::new();
    let id = map.intern("lower_clean_test.px", text);
    let parsed = parse(id, text);
    let mut analysis = analyze_root(id, &parsed.tree);
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
    let module = crate::lower::lower(id, &root, &mut analysis);
    analysis.diagnostics.is_empty() && module.diagnostics.is_empty()
}

// --- §19-M2 criterion 1: infer non-recursive fn params and returns ---------

#[test]
fn infers_int_function_from_arithmetic_body() {
    // `fn add(a, b) { a + b }` — `+` forces both params and the result to Int.
    // Concrete (no type vars), so the scheme renders without `forall`.
    let scheme = scheme_of("fn add(a, b) { a + b }", "add").expect("add has a scheme");
    insta::assert_snapshot!(scheme, @"(Int, Int) -> Int");
}

#[test]
fn infers_unused_param_is_polymorphic() {
    // `fn greet(name) { "hi" }` — `name` is unused, so it stays a type variable;
    // only the body (Text) constrains the result.
    let scheme = scheme_of("fn greet(name) { \"hi\" }", "greet").expect("scheme");
    insta::assert_snapshot!(scheme, @"forall T. (T) -> Text");
}

#[test]
fn infers_annotated_return_from_body() {
    let src = "fn double(n: Int) -> Int { n + n }";
    let analysis = analyze(src);
    assert!(
        !has_type_error(src),
        "annotated fn should type-check: {:?}",
        analysis.diagnostics
    );
}

// --- §19-M2 criterion 2: shadowed bindings have distinct types -----------

#[test]
fn shadowed_let_changes_type() {
    // `let a = 4` then `let a = "Foo"`: each binding keeps its own type.
    let src = "let a = 4\nlet a = \"Foo\"";
    let analysis = analyze(src);
    let a_schemes: Vec<_> = analysis
        .names
        .all()
        .iter()
        .filter(|s| s.name == "a" && s.kind == SymbolKind::Let)
        .filter_map(|s| s.scheme.as_ref())
        .map(|sc| analysis.db.render_scheme(sc))
        .collect();
    assert_eq!(a_schemes, vec!["Int", "Text"]);
}

// --- §19-M2 criterion 4: reject cross-type var reassignment ---------------

#[test]
fn cross_type_var_reassignment_is_rejected() {
    let analysis = analyze("var x = 0\nx = \"hi\"");
    let type_errs: Vec<_> = analysis
        .diagnostics
        .iter()
        .filter(|d| d.code().category() == DiagnosticCategory::Type)
        .collect();
    assert_eq!(
        type_errs.len(),
        1,
        "expected one Y001, got {:?}",
        analysis.diagnostics
    );
    assert!(analysis.diagnostics[0]
        .message()
        .contains("expected Int, found Text"));
}

#[test]
fn same_type_var_reassignment_is_accepted() {
    assert!(!has_type_error("var x = 0\nx = 1"));
}

#[test]
fn compound_assignment_type_checked() {
    // `var x = 0; x += "s"` — the RHS must be Int.
    assert!(has_type_error("var x = 0\nx += \"s\""));
}

// --- arithmetic & comparison typing ---------------------------------------

#[test]
fn arithmetic_yields_int() {
    assert_eq!(expr_type("1 + 2 * 3"), "Int");
}

#[test]
fn comparison_yields_bool() {
    assert_eq!(expr_type("1 == 2"), "Bool");
}

#[test]
fn comparison_operand_mismatch_is_rejected() {
    assert!(has_type_error("out(1 == \"a\")"));
}

#[test]
fn unary_minus_is_int() {
    assert_eq!(expr_type("-5"), "Int");
}

#[test]
fn logical_not_is_bool() {
    assert_eq!(expr_type("!true"), "Bool");
}

// --- tuples (M2 deliverable) ----------------------------------------------

#[test]
fn tuple_type_is_inferred() {
    assert_eq!(expr_type("(1, \"a\")"), "(Int, Text)");
}

#[test]
fn nested_tuple_type_is_inferred() {
    assert_eq!(expr_type("(1, (true, 2))"), "(Int, (Bool, Int))");
}

// --- if/while typing -------------------------------------------------------

#[test]
fn if_cond_must_be_bool() {
    let analysis = analyze("if 1 { out(2) }");
    assert!(analysis.diagnostics.iter().any(|d| {
        d.code().category() == DiagnosticCategory::Type && d.message().contains("expected Bool")
    }));
}

#[test]
fn while_cond_must_be_bool() {
    let analysis = analyze("while 1 { out(2) }");
    assert!(analysis.diagnostics.iter().any(|d| {
        d.code().category() == DiagnosticCategory::Type && d.message().contains("expected Bool")
    }));
}

#[test]
fn if_branches_must_match() {
    // then: Int, else: Text — mismatch.
    assert!(has_type_error("if true { 1 } else { \"a\" }"));
}

// --- let-generalization ---------------------------------------------------

#[test]
fn let_int_binding_is_monotype() {
    // A concrete let is monomorphic.
    assert_eq!(scheme_of("let x = 1", "x").unwrap(), "Int");
}

#[test]
fn var_binding_is_monotype() {
    assert_eq!(scheme_of("var x = true", "x").unwrap(), "Bool");
}

// --- recursive functions (§4.9) -------------------------------------------

#[test]
fn recursive_fn_with_annotations_type_checks() {
    let src = "fn fact(n: Int) -> Int { if n <= 1 { 1 } else { n * fact(n - 1) } }";
    assert!(!has_type_error(src));
}

#[test]
fn recursive_fn_call_is_checked_against_its_eventual_signature() {
    // The monomorphic placeholder installed for recursion must be the same slot
    // used by recursive calls; a fresh, disconnected call type would accept
    // Text here and only check the final body result.
    let src = "fn recurse(n: Int) -> Int { recurse(\"wrong\") }";
    assert!(
        has_type_error(src),
        "recursive calls must constrain the declaration being inferred"
    );
}

// --- a representative whole program (clean) -------------------------------

#[test]
fn clean_program_has_no_diagnostics() {
    let src = "fn add(a: Int, b: Int) -> Int { a + b }\nout(add(1, 2))";
    let analysis = analyze(src);
    assert!(analysis.is_clean(), "{:?}", analysis.diagnostics);
}

#[test]
fn out_accepts_any_type() {
    // `out` is polymorphic: out(1), out("a"), out(true) all type-check.
    assert!(!has_type_error("out(1)"));
    assert!(!has_type_error("out(\"a\")"));
    assert!(!has_type_error("out(true)"));
}

// --- M6: input-parser type synthesis (§7.8) --------------------------------

#[test]
fn read_atomic_synthesizes_scalar_type() {
    // `read int` → Int; `read char` → Char (acceptance criterion 4: hover).
    assert_eq!(expr_type("read int"), "Int");
    assert_eq!(expr_type("read char"), "Char");
}

#[test]
fn read_lines_of_int_synthesizes_vec_int() {
    // `read lines(int)` → Vec[Int] (§7.8 derivation table).
    assert_eq!(expr_type("read lines(int)"), "Vec[Int]");
}

#[test]
fn read_nested_sections_lines_csv_int() {
    // `read sections(lines(csv(int)))` → Vec[Vec[Vec[Int]]] (§7.6).
    assert_eq!(
        expr_type("read sections(lines(csv(int)))"),
        "Vec[Vec[Vec[Int]]]"
    );
}

#[test]
fn read_grid_of_char_synthesizes_grid_char() {
    // `read grid(char)` → Grid[Char] (§7.8).
    assert_eq!(expr_type("read grid(char)"), "Grid[Char]");
}

#[test]
fn read_template_synthesizes_tuple() {
    // `read lines(`{int},{int}`)` → Vec[(Int, Int)] (§7.3, §7.8).
    assert_eq!(expr_type("read lines(`{int},{int}`)"), "Vec[(Int, Int)]");
}

#[test]
fn parse_expression_synthesizes_type() {
    // `parse(sample, lines(int))` → Vec[Int], where sample is Text.
    let src = "fn f(sample: Text) { let v = parse(sample, lines(int)); v }";
    assert!(!has_type_error(src));
}

#[test]
fn read_in_fn_then_method_call_typechecks() {
    // Full pipeline: read inside a fn, then call .len() on the result.
    let src = "fn main() -> Int {\n  let v = read lines(int)\n  v.len()\n}\n";
    let analysis = analyze(src);
    assert!(
        !has_type_error(src),
        "type errors: {:?}",
        analysis.diagnostics
    );
}

// --- M7-WS6: structural equality capability (§5.5) --------------------------

#[test]
fn record_equality_typechecks() {
    // `==` on two records of the same type typechecks cleanly (no Y004).
    let src = "struct Point { x: Int, y: Int }\nfn main() -> Int {\n  let a = Point { x: 1, y: 2 }\n  let b = Point { x: 1, y: 2 }\n  if a == b { 1 } else { 0 }\n}\n";
    assert!(!has_type_error(src));
}

#[test]
fn tuple_equality_typechecks() {
    // `==` on two tuples of the same shape typechecks cleanly.
    let src =
        "fn main() -> Int {\n  let a = (1, 2)\n  let b = (1, 2)\n  if a == b { 1 } else { 0 }\n}\n";
    assert!(!has_type_error(src));
}

#[test]
fn comparing_functions_is_rejected() {
    // Functions are never equatable (§5.5); comparing two function values must
    // emit Y004 (whose wording must not mention trait/capability).
    let src = "fn f(x: Int) -> Int { x }\nfn g(x: Int) -> Int { x }\nfn main() -> Int {\n  if f == g { 1 } else { 0 }\n}\n";
    assert!(has_type_error(src));
}

#[test]
fn record_with_function_field_not_equatable() {
    // A record containing a function field is not equatable (§5.5). We bind the
    // function to a name first (nested fn literals aren't supported as field
    // values), then construct the record and compare it.
    let src = "struct Box { f: (Int) -> Int }\nfn id(x: Int) -> Int { x }\nfn main() -> Int {\n  let a = Box { f: id }\n  let b = Box { f: id }\n  if a == b { 1 } else { 0 }\n}\n";
    assert!(has_type_error(src));
}

#[test]
fn int_equality_still_typechecks() {
    // Regression: `==` on Int (the pre-existing path) must still typecheck.
    assert!(!has_type_error(
        "fn main() -> Int {\n  if 3 == 3 { 1 } else { 0 }\n}\n"
    ));
}

// --- M7-WSP: exhaustiveness checking (Y120/Y121) ----------------------------

#[test]
fn non_exhaustive_enum_match_is_rejected() {
    // Missing the Wall variant → Y120.
    let src = "enum Tile { Empty, Wall, Number(Int) }\nfn main() -> Int {\n  let t = Empty\n  match t {\n    Empty => 1\n    Number(n) => n\n  }\n}\n";
    assert!(has_type_error_with_lower(src));
}

#[test]
fn exhaustive_enum_match_is_ok() {
    // All three variants covered → no error.
    let src = "enum Tile { Empty, Wall, Number(Int) }\nfn main() -> Int {\n  let t = Empty\n  match t {\n    Empty => 1\n    Wall => 2\n    Number(n) => n\n  }\n}\n";
    assert!(!has_type_error_with_lower(src));
}

#[test]
fn enum_match_with_wildcard_is_ok() {
    // Wildcard catches remaining variants → exhaustive.
    let src = "enum Tile { Empty, Wall, Number(Int) }\nfn main() -> Int {\n  let t = Empty\n  match t {\n    Empty => 1\n    _ => 0\n  }\n}\n";
    assert!(!has_type_error_with_lower(src));
}

#[test]
fn int_match_without_wildcard_is_rejected() {
    // Int has infinitely many values; a literal-only match needs `_` → Y120.
    let src = "fn main() -> Int {\n  let n = 1\n  match n {\n    1 => 10\n    2 => 20\n  }\n}\n";
    assert!(has_type_error_with_lower(src));
}

#[test]
fn int_match_with_wildcard_is_ok() {
    // Int match with `_` → exhaustive.
    let src = "fn main() -> Int {\n  let n = 1\n  match n {\n    1 => 10\n    2 => 20\n    _ => 0\n  }\n}\n";
    assert!(!has_type_error_with_lower(src));
}

#[test]
fn bool_match_without_both_cases_is_rejected() {
    // Bool match missing `false` → Y120.
    let src = "fn main() -> Int {\n  let b = true\n  match b {\n    true => 1\n  }\n}\n";
    assert!(has_type_error_with_lower(src));
}

#[test]
fn bool_match_both_cases_is_ok() {
    // Both true and false covered → exhaustive.
    let src =
        "fn main() -> Int {\n  let b = true\n  match b {\n    true => 1\n    false => 0\n  }\n}\n";
    assert!(!has_type_error_with_lower(src));
}

#[test]
fn arm_after_wildcard_is_unreachable() {
    // An arm after `_` is unreachable → Y121 (a type-category diagnostic).
    let src = "enum Tile { Empty, Wall }\nfn main() -> Int {\n  let t = Empty\n  match t {\n    _ => 0\n    Empty => 1\n  }\n}\n";
    assert!(has_type_error_with_lower(src));
}

// --- M7-WS7: closure type inference (§4.10) ---------------------------------
// The frontend (parse, resolve, infer) is complete; runtime lowering is in
// progress. These test the inferred types.

#[test]
fn closure_infers_identity_type() {
    // `|x| x` should infer `(Int) -> Int` when applied to an Int.
    let ty = expr_type("|x| x");
    // Without a call site, the param is a fresh var; the type is `(a) -> a`.
    // Just verify it type-checks (no error) and produces a function type.
    assert!(ty.contains("->"), "closure type was: {ty}");
}

#[test]
fn closure_typechecks() {
    // A closure that captures an outer variable: `let o = 10; let f = |x| x + o`.
    let src = "fn main() -> Int {\n  let o = 10\n  let f = |x| x + o\n  0\n}\n";
    assert!(!has_type_error(src));
}

#[test]
fn closure_with_typed_param_typechecks() {
    // `|x: Int| x + 1` with an explicit param type.
    let src = "fn main() -> Int {\n  let f = |x: Int| x + 1\n  0\n}\n";
    assert!(!has_type_error(src));
}

#[test]
fn closure_immutable_capture_lowers_clean() {
    // The headline immutable-capture pipeline lowers without diagnostics.
    let src = "fn main() -> Int {\n  let o = 10\n  let f = |x| x + o\n  f(5)\n}\n";
    assert!(!has_type_error_with_lower(src));
}

#[test]
fn mutable_capture_now_supported() {
    // WS7b: a `var` capture is now supported (boxed into a `VarCell`). It lowers
    // without diagnostics.
    let src = "fn main() -> Int {\n  var c = 0\n  let f = |_| c\n  f(0)\n}\n";
    assert!(
        !has_type_error_with_lower(src),
        "mutable capture should be supported (WS7b)"
    );
}

// ===========================================================================
// Diagnostic span precision.
//
// A type mismatch must point at the offending sub-expression, not the enclosing
// statement/function (the earlier behavior underlined the whole `fn` for a
// return-type error). These tests pin the primary span's byte range to the
// exact expression at fault, so a regression that re-coarsens the span fails
// loudly. (AGENTS.md: "test every language feature extensively".)
// ===========================================================================

/// The primary span `[start, end)` of the first Y001 type-mismatch in `src`, or
/// `None` if there is none. The offsets are absolute byte offsets into `src`.
fn first_mismatch_span(src: &str) -> Option<(u32, u32)> {
    let analysis = analyze(src);
    let d = analysis
        .diagnostics
        .iter()
        .find(|d| d.code().category() == DiagnosticCategory::Type)?;
    let span = d.primary().span;
    Some((span.start().to_u32(), span.end().to_u32()))
}

/// Find the byte span of the first occurrence of `needle` in `src`.
fn span_of(src: &str, needle: &str) -> (u32, u32) {
    let start = src.find(needle).unwrap_or_else(|| {
        panic!("needle `{needle}` not found in src:\n{src}");
    }) as u32;
    (start, start + needle.len() as u32)
}

#[test]
fn return_type_mismatch_points_at_tail_expression() {
    // The user's original example: `out("kurac")` returns `Unit` but `main` is
    // declared `-> Int`. The error must underline `out("kurac")`, not the whole
    // `fn main() -> Int { … }`.
    let src = "fn main() -> Int {\n    let depths = read lines(int)\n    out(\"kurac\")\n}\n";
    let expected = span_of(src, "out(\"kurac\")");
    let actual = first_mismatch_span(src).expect("expected a Y001 mismatch");
    assert_eq!(
        actual, expected,
        "return-type mismatch should point at the tail expression, got {actual:?} (expected {expected:?})",
    );
}

#[test]
fn let_annotation_mismatch_points_at_initializer() {
    // `let x: Int = "hello"` — the error underlines `"hello"`, not the whole let.
    let src = "fn main() -> Int {\n    let x: Int = \"hello\"\n    0\n}\n";
    let expected = span_of(src, "\"hello\"");
    let actual = first_mismatch_span(src).expect("expected a Y001 mismatch");
    assert_eq!(
        actual, expected,
        "let-annotation mismatch should point at the initializer, got {actual:?}",
    );
}

#[test]
fn arithmetic_operand_mismatch_points_at_bad_operand() {
    // `s + 1` where `s` is `Text`: the error underlines the operand `s` in
    // `s + 1`, not the whole binary expression. `s` appears twice (in `let s`
    // and in `s + 1`); the mismatch is at the second use.
    let src = "fn main() -> Int {\n    let s = \"hi\"\n    s + 1\n}\n";
    let use_start = src.find("s + 1").unwrap() as u32;
    let expected = (use_start, use_start + 1);
    let actual = first_mismatch_span(src).expect("expected a Y001 mismatch");
    assert_eq!(
        actual, expected,
        "arithmetic mismatch should point at the bad operand `s`, got {actual:?}",
    );
}

#[test]
fn if_condition_mismatch_points_at_condition() {
    // `if 5 { … }` — the error underlines `5`, not the whole `if`.
    let src = "fn main() -> Int {\n    if 5 { 1 } else { 0 }\n}\n";
    let expected = span_of(src, "5");
    let actual = first_mismatch_span(src).expect("expected a Y001 mismatch");
    assert_eq!(
        actual, expected,
        "if-condition mismatch should point at the condition, got {actual:?}",
    );
}

#[test]
fn mismatch_carries_a_help_hint_when_found_is_unit() {
    // The Unit→Int return mismatch should attach a `help:` suggestion (§8.2).
    let src = "fn main() -> Int {\n    out(\"x\")\n}\n";
    let analysis = analyze(src);
    let d = analysis
        .diagnostics
        .iter()
        .find(|d| d.code().category() == DiagnosticCategory::Type)
        .expect("expected a type error");
    assert!(
        !d.suggestions().is_empty(),
        "Unit mismatch should carry a help hint: {:?}",
        d.suggestions()
    );
    assert!(
        d.suggestions()[0].label.contains("Unit"),
        "the hint should explain the Unit value: {:?}",
        d.suggestions()[0].label
    );
}

// ===========================================================================
// Adversarial front-end regressions.
//
// These tests intentionally pin semantic contracts that cross AST accessors,
// resolution, inference, and typed-HIR lowering. Several currently expose
// known bugs and are expected to fail until the implementation is corrected.
// ===========================================================================

// --- annotation preservation ------------------------------------------------

#[test]
#[ignore = "known bug: direct tuple annotations are dropped by AST accessors"]
fn tuple_parameter_annotation_is_enforced() {
    // A direct TUPLE_TYPE child must not disappear merely because Param::ty()
    // only recognizes TYPE_REF nodes.
    let src = "fn bad(x: (Int, Text)) -> Int { x + 1 }";
    assert!(
        has_type_error(src),
        "using a tuple-annotated parameter as Int must be rejected"
    );
}

#[test]
#[ignore = "known bug: direct function annotations are dropped by AST accessors"]
fn function_parameter_annotation_is_enforced() {
    // The annotation says the argument to `f` is Int, so calling it with Text
    // is invalid even though bottom-up inference could otherwise choose Text.
    let src = "fn bad(f: (Int) -> Int) -> Int { f(\"wrong\") }";
    assert!(
        has_type_error(src),
        "function-typed parameter annotation must constrain calls"
    );
}

#[test]
#[ignore = "known bug: direct tuple return annotations are dropped by AST accessors"]
fn tuple_return_annotation_is_enforced() {
    let src = "fn bad() -> (Int, Text) { (1, true) }";
    assert!(
        has_type_error(src),
        "direct tuple return annotations must not be silently ignored"
    );
}

#[test]
#[ignore = "known bug: user-enum annotations are not converted to inference types"]
fn user_enum_annotation_is_enforced() {
    // User enum names resolve, but inference must also turn the annotation into
    // that enum type (not fall back to a fresh variable).
    let src = "enum Tile { Empty }\nfn bad(tile: Tile) -> Int { tile + 1 }";
    assert!(
        has_type_error(src),
        "a Tile parameter cannot be inferred as Int"
    );
}

#[test]
#[ignore = "known bug: direct function field annotations are dropped by AST accessors"]
fn function_typed_record_field_annotation_is_enforced() {
    // The older equality test initializes this field with a function, which
    // accidentally pins the dropped annotation and therefore cannot detect the
    // accessor bug. An Int initializer distinguishes the two paths.
    let src = "struct Box { f: (Int) -> Int }\n\
               fn main() -> Int { let value = Box { f: 1 }; 0 }";
    assert!(
        has_type_error(src),
        "a function-typed record field cannot be initialized with Int"
    );
}

#[test]
#[ignore = "known bug: direct function enum payload annotations are dropped by AST accessors"]
fn function_typed_enum_payload_annotation_is_enforced() {
    let src = "enum Boxed { Box((Int) -> Int) }\n\
               fn main() -> Boxed { Box(1) }";
    assert!(
        has_type_error(src),
        "a function-typed enum payload cannot be constructed from Int"
    );
}

#[test]
#[ignore = "known bug: type definitions are inferred sequentially despite two-pass resolution"]
fn forward_struct_annotation_is_enforced() {
    let src = "fn bad(point: Point) -> Int { point + 1 }\n\
               struct Point { x: Int }";
    assert!(
        has_type_error(src),
        "a forward-resolved Point annotation cannot degrade to a fresh variable"
    );
}

#[test]
#[ignore = "known bug: resolver accepts value symbols in type position"]
fn value_binding_name_is_not_accepted_as_a_type() {
    let src = "let Alias = 1\nlet value: Alias = \"text\"";
    assert!(
        has_name_error(src),
        "ordinary value bindings are not type declarations"
    );
}

#[test]
#[ignore = "known bug: collection annotation arity is never validated"]
fn malformed_collection_type_arity_is_rejected() {
    let src = "fn identity(value: Map[Int]) -> Map[Int] { value }";
    assert!(
        !is_clean_with_lower(src),
        "Map has exactly two type arguments; malformed collection shapes must not enter TypeDb"
    );
}

// --- mutation and control-flow typing ---------------------------------------

#[test]
#[ignore = "known bug: assignment lookup uses disconnected inference scopes"]
fn local_var_reassignment_preserves_its_type() {
    let src = "fn main() -> Int { var x = 0; x = \"bad\"; 0 }";
    assert!(
        has_type_error(src),
        "local assignments must use the resolver's exact lhs symbol"
    );
}

#[test]
#[ignore = "known bug: assignment does not require a mutable var binding"]
fn reassignment_to_let_is_rejected() {
    let src = "fn main() -> Int { let x = 1; x = 2; x }";
    assert!(
        !is_clean_with_lower(src),
        "`let` is immutable; only `var` may be reassigned"
    );
}

#[test]
#[ignore = "known bug: compound assignment checks equality but not numeric support"]
fn compound_assignment_requires_a_numeric_target() {
    let src = "var flag = true\nflag += false";
    assert!(
        has_type_error(src),
        "matching operand types alone do not make Bool addition valid"
    );
}

#[test]
#[ignore = "known bug: if-without-else is inferred as its then-branch type"]
fn if_without_else_cannot_produce_the_then_value_type() {
    // MIR materializes Unit on the false path, so the expression cannot have
    // type Int just because its then branch does.
    let src = "fn maybe(flag: Bool) -> Int { if flag { 1 } }";
    assert!(
        has_type_error(src),
        "the absent else path produces Unit, not Int"
    );
}

#[test]
#[ignore = "known bug: explicit return values are not unified with function results"]
fn early_return_value_must_match_the_function_result() {
    let src = "fn bad() -> Int { return \"wrong\"; 1 }";
    assert!(
        has_type_error(src),
        "an early return must be checked even when the block tail has the declared type"
    );
}

#[test]
#[ignore = "known bug: inference retains a non-trailing block expression as the value"]
fn expression_before_trailing_statement_is_not_the_block_value() {
    // Lowering correctly demotes `1` to an effect statement and gives this
    // block a Unit tail. Inference must make the same choice.
    let src = "fn bad() -> Int { 1; let x = 2 }";
    assert!(
        has_type_error(src),
        "inference and lowering must agree on the actual trailing expression"
    );
}

#[test]
#[ignore = "known bug: resolver/inference do not track function and loop control-flow context"]
fn control_flow_terminators_require_a_legal_enclosing_context() {
    for src in [
        "return 1",
        "fn main() -> Int { break 1; 0 }",
        "fn main() -> Int { continue; 0 }",
    ] {
        assert!(
            !is_clean_with_lower(src),
            "out-of-context control flow must be rejected: {src}"
        );
    }
}

#[test]
#[ignore = "known bug: break values do not determine expression-loop type"]
fn expression_loop_uses_its_break_value_type() {
    let src = "fn main() -> Int { loop { break 42 } }";
    assert!(
        !has_type_error(src),
        "an expression loop with `break Int` has type Int"
    );
}

#[test]
#[ignore = "known bug: Never is not treated as the bottom type during unification"]
fn never_branch_coerces_to_the_other_branch_type() {
    let src = "fn choose(flag: Bool) -> Int { if flag { panic(\"stop\") } else { 1 } }";
    use praxis_ast::AstNode;

    let map = SourceMap::new();
    let id = map.intern("never_lub_test.px", src);
    let parsed = parse(id, src);
    let mut analysis = analyze_root(id, &parsed.tree);
    assert!(
        !analysis
            .diagnostics
            .iter()
            .any(|d| d.code().category() == DiagnosticCategory::Type),
        "Never is the bottom type and must not conflict with an Int branch"
    );
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
    let module = crate::lower::lower(id, &root, &mut analysis);
    let choose = module
        .items
        .iter()
        .find_map(|item| match item {
            crate::TypedItem::Fn(f) if f.name == "choose" => Some(f),
            _ => None,
        })
        .expect("choose function");
    let if_ty = match &choose.body.tail {
        crate::TypedExpr::If { ty, .. } => *ty,
        other => panic!("expected if tail, got {other:?}"),
    };
    assert_eq!(
        analysis.db.render(if_ty),
        "Int",
        "the branch join itself must choose Int, not merely suppress a mismatch"
    );
}

// --- scalar and builtin operation constraints -------------------------------

#[test]
#[ignore = "known bug: parse does not constrain its input expression to Text"]
fn parse_requires_text_input() {
    let src = "fn main() -> Int { parse(1, int) }";
    assert!(
        has_type_error(src),
        "the first argument of parse(text, parser) must be Text"
    );
}

#[test]
#[ignore = "known bug: unary minus chooses Float only for literal syntax"]
fn unary_minus_accepts_float_typed_variables() {
    let src = "fn negate(x: Float) -> Float { -x }";
    assert!(
        !has_type_error(src),
        "Float negation must depend on the operand type, not only literal syntax"
    );
}

#[test]
#[ignore = "known bug: numeric inference admits the undefined Float remainder operation"]
fn float_remainder_is_rejected() {
    let src = "fn bad() -> Float { 5.0 % 2.0 }";
    assert!(
        has_type_error(src),
        "the language defines no `%` operation for Float"
    );
}

#[test]
#[ignore = "known bug: lowering silently saturates out-of-range Int literals"]
fn integer_literal_overflow_is_diagnosed() {
    let src = "fn main() -> Int { 9223372036854775808 }";
    assert!(
        !is_clean_with_lower(src),
        "an out-of-range literal must not silently become i64::MAX"
    );
}

#[test]
fn ordering_rejects_bool_operands() {
    let src = "fn bad() -> Bool { true < false }";
    assert!(
        has_type_error(src),
        "supports_ord says Bool has no defined total order"
    );
}

#[test]
fn ordering_rejects_function_operands() {
    let src = "fn id(x: Int) -> Int { x }\nfn bad() -> Bool { id < id }";
    assert!(
        has_type_error(src),
        "function values have no structural ordering"
    );
}

#[test]
fn ordering_rejects_composites_without_a_matching_runtime_lowering() {
    let src = "fn bad() -> Bool { (1, 2) < (1, 3) }";
    assert!(
        has_type_error(src),
        "composite ordering cannot be admitted while MIR reinterprets the payload as one i64"
    );
}

#[test]
#[ignore = "known bug: assert has no prelude type scheme"]
fn prelude_assert_requires_bool() {
    let src = "fn main() -> Unit { assert(1) }";
    assert!(
        has_type_error(src),
        "prelude calls need real schemes instead of unconstrained fresh types"
    );
}

// --- capability constraints must survive polymorphism -----------------------

#[test]
#[ignore = "known bug: equality constraints are discarded at generalization"]
fn polymorphic_equality_rejects_function_instantiation() {
    let src = "fn equal(a, b) { a == b }\n\
               fn f(x: Int) -> Int { x }\n\
               fn g(x: Int) -> Int { x }\n\
               fn main() -> Bool { equal(f, g) }";
    assert!(
        has_type_error(src),
        "SupportsEq must be retained and checked when T becomes a function"
    );
}

#[test]
#[ignore = "known bug: iterable constraints are discarded at generalization"]
fn iterable_constraint_rejects_int_instantiation() {
    let src = "fn drain(values) -> Unit { for value in values { out(value) } }\n\
               fn main() -> Unit { drain(1) }";
    assert!(
        has_type_error_with_lower(src),
        "a generic Iterable constraint cannot disappear after generalization"
    );
}

#[test]
#[ignore = "known bug: method lookup cannot constrain an unresolved receiver"]
fn collection_method_constrains_unannotated_receiver_parameter() {
    // This is the §5.2 inference example: use of `.sum()` should constrain the
    // unannotated parameter to a numeric iterable shape, then the call site
    // pins its element type to Int.
    let src = "fn total(values) { values.sum() }\n\
               fn main() -> Int { let values = Vec(); values.push(1); total(values) }";
    assert!(
        !has_type_error_with_lower(src),
        "method use plus the call site should infer a concrete collection receiver"
    );
}

#[test]
#[ignore = "known bug: numeric collection sinks do not constrain their element type"]
fn sum_requires_int_elements() {
    let src = "fn main() -> Int {\n\
                 let values = Vec()\n\
                 values.push(true)\n\
                 values.sum()\n\
               }";
    assert!(
        has_type_error_with_lower(src),
        "sum/product/min/max lower as Int operations and must reject Bool elements"
    );
}

#[test]
#[ignore = "known bug: map operations do not enforce key hashability"]
fn map_key_must_be_hashable() {
    let src = "fn id(x: Int) -> Int { x }\n\
               fn main() -> Unit { let map = Map(); map.insert(id, 1) }";
    assert!(
        has_type_error_with_lower(src),
        "function values cannot be structural map keys"
    );
}

#[test]
#[ignore = "known bug: mutable structural values are admitted as hash keys"]
fn mutable_collection_cannot_be_used_as_a_map_key() {
    // Hashing a Vec by its current contents and then mutating it changes the
    // bucket it belongs to. Map lookup/removal can no longer find that entry,
    // so mutable collection identities must be rejected as keys even when
    // their elements are otherwise hashable.
    let src = "fn main() -> Unit {\n\
                 let key = Vec()\n\
                 key.push(1)\n\
                 let table = Map()\n\
                 table.insert(key, \"stored\")\n\
                 key.push(2)\n\
               }";
    assert!(
        has_type_error_with_lower(src),
        "a mutable collection cannot be a structural map key"
    );
}

#[test]
#[ignore = "known bug: mutable structural values are admitted as set keys"]
fn mutable_collection_cannot_be_used_as_a_set_element() {
    // Set elements are hash keys too and have the same hash-stability
    // requirement as Map keys.
    let src = "fn main() -> Unit {\n\
                 let key = Vec()\n\
                 key.push(1)\n\
                 let values = Set()\n\
                 values.insert(key)\n\
                 key.push(2)\n\
               }";
    assert!(
        has_type_error_with_lower(src),
        "a mutable collection cannot be a structural set key"
    );
}

#[test]
#[ignore = "known bug: heap operations do not enforce element orderability"]
fn heap_element_must_be_orderable() {
    let src = "fn id(x: Int) -> Int { x }\n\
               fn main() -> Unit { let heap = MinHeap(); heap.push(id) }";
    assert!(
        has_type_error_with_lower(src),
        "function values cannot be ordered by a heap"
    );
}

/// **Inverted** by ADR-045, and rewritten rather than un-ignored. This asserted
/// that a `MinHeap[Text]` must be a *type error*, because `SupportsOrd` admitted
/// `Text` while `HeapEntry::cmp` read every payload as an `i64` — so accepting
/// the program produced pointer ordering. The runtime half now exists
/// (`TEXT.compare`, dispatched by `HeapEntry::cmp`), so the two agree and the
/// program is legitimately accepted.
///
/// What it pins now is that agreement: a type the capability admits into a heap
/// is one the runtime can actually order. `a_text_heap_pops_in_lexicographic_order`
/// (praxis-runtime `heaps.rs`) is the other half — that the order is the right
/// one.
#[test]
fn heap_element_orderability_agrees_with_the_runtime() {
    let src = "fn main() -> Unit {\n\
                 let heap = MinHeap()\n\
                 heap.push(\"z\")\n\
                 heap.push(\"a\")\n\
               }";
    assert!(
        !has_type_error_with_lower(src),
        "Text is orderable in both halves now: the capability and the descriptor"
    );
}

#[test]
#[ignore = "known bug: Map.get is still typed as V instead of Option[V]"]
fn map_get_returns_option() {
    let src = "fn lookup(map: Map[Text, Int]) -> Option[Int] { map.get(\"key\") }";
    assert!(
        !has_type_error(src),
        "normal map absence is represented by Option[V], not a dynamically typed Unit/V result"
    );
}

#[test]
#[ignore = "known bug: HIR lowering re-instantiates polymorphic call results without arguments"]
fn lowered_polymorphic_call_result_uses_the_callsite_instantiation() {
    use praxis_ast::AstNode;

    let src = "fn id(value) { value }\nfn main() -> Float { id(1.5) }";
    let map = SourceMap::new();
    let id = map.intern("lower_call_type_test.px", src);
    let parsed = parse(id, src);
    let mut analysis = analyze_root(id, &parsed.tree);
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
    let module = crate::lower::lower(id, &root, &mut analysis);
    let main = module
        .items
        .iter()
        .find_map(|item| match item {
            crate::TypedItem::Fn(f) if f.name == "main" => Some(f),
            _ => None,
        })
        .expect("main function");
    let call_ty = match &main.body.tail {
        crate::TypedExpr::Call { ty, .. } => *ty,
        other => panic!("expected call tail, got {other:?}"),
    };
    assert_eq!(
        analysis.db.render(call_ty),
        "Float",
        "typed HIR must preserve the concrete result inferred at this call site"
    );
}

#[test]
#[ignore = "known bug: HIR lowering instantiates method results without receiver substitutions"]
fn lowered_generic_method_result_uses_the_receiver_instantiation() {
    use praxis_ast::AstNode;

    let src = "fn main() -> Float { let values = Vec(); values.push(1.5); values.get(0) }";
    let map = SourceMap::new();
    let id = map.intern("lower_method_type_test.px", src);
    let parsed = parse(id, src);
    let mut analysis = analyze_root(id, &parsed.tree);
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
    let module = crate::lower::lower(id, &root, &mut analysis);
    let main = module
        .items
        .iter()
        .find_map(|item| match item {
            crate::TypedItem::Fn(f) if f.name == "main" => Some(f),
            _ => None,
        })
        .expect("main function");
    let get_ty = match &main.body.tail {
        crate::TypedExpr::MethodCall { name, ty, .. } if name == "get" => *ty,
        other => panic!("expected get method tail, got {other:?}"),
    };
    assert_eq!(
        analysis.db.render(get_ty),
        "Float",
        "typed HIR must carry Vec[Float].get as Float"
    );
}

#[test]
#[ignore = "known bug: HIR enum lowering looks up constructor text instead of the resolved symbol"]
fn lowering_respects_a_local_that_shadows_an_enum_variant() {
    use praxis_ast::AstNode;

    let src = "enum E { A }\nfn main() -> Int { let A = 7; A }";
    let map = SourceMap::new();
    let id = map.intern("variant_shadow_test.px", src);
    let parsed = parse(id, src);
    let mut analysis = analyze_root(id, &parsed.tree);
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
    let module = crate::lower::lower(id, &root, &mut analysis);
    let main = module
        .items
        .iter()
        .find_map(|item| match item {
            crate::TypedItem::Fn(f) if f.name == "main" => Some(f),
            _ => None,
        })
        .expect("main function");
    let path_ty = match &main.body.tail {
        crate::TypedExpr::Path { ty, .. } => *ty,
        other => panic!("the shadowing local must remain a Path, got {other:?}"),
    };
    assert_eq!(analysis.db.render(path_ty), "Int");
}

// --- declaration ordering and analyzer robustness ---------------------------

#[test]
#[ignore = "known bug: inference does not predeclare later function signatures"]
fn forward_call_is_checked_against_later_function_signature() {
    let src = "fn first() -> Int { later(\"wrong\") }\n\
               fn later(value: Int) -> Int { value }";
    assert!(
        has_type_error(src),
        "two-pass resolution must be paired with placeholders for all function signatures"
    );
}

#[test]
#[ignore = "known bug: duplicate top-level functions survive with the same runtime symbol"]
fn duplicate_function_declarations_are_rejected() {
    let src = "fn duplicate() -> Int { 1 }\n\
               fn duplicate() -> Int { 2 }\n\
               fn main() -> Int { duplicate() }";
    assert!(
        !is_clean_with_lower(src),
        "two functions cannot be emitted under one JIT symbol name"
    );
}

#[test]
#[ignore = "known bug: a parsed nested function trips an inference expect"]
fn analyzing_nested_function_never_panics() {
    // Even if nested named functions are ultimately rejected, `analyze`'s
    // public contract says unsupported/malformed input becomes diagnostics.
    let result = std::panic::catch_unwind(|| {
        analyze("fn main() -> Int { fn local(x: Int) -> Int { x }; local(1) }")
    });
    assert!(
        result.is_ok(),
        "a parsed nested FnItem must not trip an internal `expect`"
    );
}

// --- records and match exhaustiveness ---------------------------------------

#[test]
#[ignore = "known bug: record literals do not validate missing fields"]
fn record_literal_requires_every_declared_field() {
    let src = "struct Pair { left: Int, right: Int }\n\
               fn main() -> Int { let pair = Pair { left: 1 }; pair.right }";
    assert!(
        !is_clean_with_lower(src),
        "allocating a record with fewer payloads than its schema is invalid"
    );
}

#[test]
#[ignore = "known bug: record literals silently drop unknown fields and their initializers"]
fn record_literal_rejects_unknown_fields() {
    let src = "struct Point { x: Int }\n\
               fn side_effect() -> Int { out(\"must not disappear\"); 2 }\n\
               fn main() -> Int { let point = Point { x: 1, typo: side_effect() }; point.x }";
    assert!(
        !is_clean_with_lower(src),
        "an unknown field must be diagnosed instead of deleting its initializer"
    );
}

#[test]
#[ignore = "known bug: record literals accept duplicate field payloads"]
fn record_literal_rejects_duplicate_fields() {
    let src = "struct Point { x: Int }\n\
               fn main() -> Int { let point = Point { x: 1, x: 2 }; point.x }";
    assert!(
        !is_clean_with_lower(src),
        "each record field must be initialized exactly once"
    );
}

#[test]
#[ignore = "known bug: underscore lexes and resolves as an ordinary binding"]
fn wildcard_pattern_does_not_bind_a_value_named_underscore() {
    let src = "fn main() -> Int { match 1 { _ => _ } }";
    assert!(
        has_name_error(src),
        "the wildcard is not a binding visible in the arm body"
    );
}

#[test]
#[ignore = "known bug: exhaustiveness does not recursively inspect variant payloads"]
fn nested_enum_pattern_must_cover_payload_constructors() {
    let src = "enum Flag { On, Off }\n\
               enum Wrapped { Wrap(Flag) }\n\
               fn main() -> Int {\n\
                 let value = Wrap(On)\n\
                 match value { Wrap(On) => 1 }\n\
               }";
    assert!(
        has_type_error_with_lower(src),
        "covering one nested payload constructor is not exhaustive"
    );
}

#[test]
#[ignore = "known bug: exhaustiveness does not flag duplicate constructor arms"]
fn duplicate_enum_arm_is_unreachable() {
    let src = "enum E { A, B }\n\
               fn main() -> Int {\n\
                 let value = A\n\
                 match value { A => 1, A => 2, B => 3 }\n\
               }";
    assert!(
        has_type_error_with_lower(src),
        "a duplicate variant arm adds no coverage and must be Y121"
    );
}

#[test]
#[ignore = "known bug: unknown constructor patterns lower to catch-all wildcards"]
fn unknown_enum_variant_pattern_is_rejected() {
    let src = "enum E { A }\n\
               fn main() -> Int { let value = A; match value { Typo(payload) => 1 } }";
    assert!(
        !is_clean_with_lower(src),
        "a misspelled variant cannot silently become an exhaustive wildcard"
    );
}

// --- input-parser conversion preserves source structure ---------------------

#[test]
#[ignore = "known bug: every template capture reuses the first capture kind"]
fn mixed_template_capture_kinds_are_preserved() {
    let src = "fn main() -> Int {\n\
                 let row = read `{name:word},{port:int}`\n\
                 row.port + 1\n\
               }";
    assert!(
        !has_type_error(src),
        "the `port` capture is Int even when an earlier capture is Word"
    );
}

#[test]
#[ignore = "known bug: unknown template capture kinds silently default to Int"]
fn unknown_template_capture_parser_is_diagnosed() {
    let src = "let value = read `{value:intr}`";
    assert!(
        has_input_error(src),
        "a misspelled capture parser must not silently default to Int"
    );
}

#[test]
#[ignore = "known bug: unknown parser constructors are dropped without a diagnostic"]
fn unknown_parser_constructor_is_diagnosed() {
    let src = "let value = read frobnicate(int)";
    assert!(
        has_input_error(src),
        "unknown constructor conversion must emit I010-style feedback"
    );
}

#[test]
#[ignore = "known bug: parser conversion discards extra constructor arguments"]
fn optional_rejects_extra_arguments() {
    let src = "let value = read optional(int, word)";
    assert!(
        has_input_error(src),
        "special constructors must validate source arity before discarding arguments"
    );
}

// --- closure escape analysis ------------------------------------------------

#[test]
#[ignore = "known bug: escape analysis skips the callee expression of calls"]
fn immediately_invoked_closure_boxes_its_mutable_capture() {
    use praxis_ast::AstNode;

    let src = "fn main() -> Int { var count = 0; (|n| { count += n; count })(1) }";
    let map = SourceMap::new();
    let id = map.intern("immediate_closure_test.px", src);
    let parsed = parse(id, src);
    let mut analysis = analyze_root(id, &parsed.tree);
    let count = analysis
        .names
        .all()
        .iter()
        .find(|s| s.name == "count" && s.kind == SymbolKind::Var)
        .expect("count var")
        .id;
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
    let module = crate::lower::lower(id, &root, &mut analysis);
    assert!(
        module.escaping_vars.contains(&count),
        "a closure in Call.callee_expr still requires its captured var to be boxed"
    );
}
