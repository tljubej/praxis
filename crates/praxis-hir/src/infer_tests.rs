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

/// Every diagnostic `analyze` + `lower` produce, in source order.
fn analyze_and_lower_diags(text: &str) -> Vec<praxis_source::Diagnostic> {
    use praxis_ast::AstNode;
    let map = SourceMap::new();
    let id = map.intern("lower_diags_test.px", text);
    let parsed = parse(id, text);
    let mut analysis = analyze_root(id, &parsed.tree);
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
    let module = crate::lower::lower(id, &root, &mut analysis);
    let mut all = analysis.diagnostics.clone();
    all.extend(module.diagnostics);
    all
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
fn tuple_return_annotation_is_enforced() {
    let src = "fn bad() -> (Int, Text) { (1, true) }";
    assert!(
        has_type_error(src),
        "direct tuple return annotations must not be silently ignored"
    );
}

#[test]
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
fn function_typed_enum_payload_annotation_is_enforced() {
    let src = "enum Boxed { Box((Int) -> Int) }\n\
               fn main() -> Boxed { Box(1) }";
    assert!(
        has_type_error(src),
        "a function-typed enum payload cannot be constructed from Int"
    );
}

/// The exit tests all ask that a wrong use is *rejected*; this asks that the
/// right one is accepted and carries the shape the annotation wrote. A fresh
/// variable would satisfy every rejection test by never rejecting, so both
/// halves are needed to say the annotation arrived.
#[test]
fn a_tuple_or_function_annotation_is_the_type_it_writes() {
    assert_eq!(
        scheme_of("fn pair(x: (Int, Text)) -> Int { 0 }", "x").as_deref(),
        Some("(Int, Text)"),
        "a tuple-annotated parameter"
    );
    assert_eq!(
        scheme_of("fn apply(g: (Int) -> Text) -> Int { 0 }", "g").as_deref(),
        Some("(Int) -> Text"),
        "a function-annotated parameter"
    );
    assert_eq!(
        scheme_of("let p: (Int, Text) = (1, \"a\")", "p").as_deref(),
        Some("(Int, Text)"),
        "a tuple-annotated `let`"
    );
    assert!(
        !has_type_error("fn apply(g: (Int) -> Text) -> Text { g(1) }"),
        "…and a call that agrees with the annotation is fine"
    );
}

/// A parenthesized single type is that type, not a one-element tuple and not a
/// fresh variable — the grouping `TYPE_REF` holds its name in a nested node, so
/// the "the `Ident` is a direct token" reading is what tells the two apart.
#[test]
fn a_parenthesized_annotation_is_the_type_it_groups() {
    assert_eq!(
        scheme_of("fn only(x: (Int)) -> Int { x }", "x").as_deref(),
        Some("Int")
    );
    assert!(
        has_type_error("fn only(x: (Int)) -> Text { x }"),
        "…and it constrains the body like a bare `Int` would"
    );
}

/// `()` in a parameter group is *no* parameters. It used to resolve to nothing
/// at all, so `() -> Int` described a function of one invented argument and
/// accepted a call with anything in it.
#[test]
fn a_nullary_function_annotation_takes_no_arguments() {
    assert_eq!(
        scheme_of("fn run(g: () -> Int) -> Int { 0 }", "g").as_deref(),
        Some("() -> Int")
    );
    assert!(
        has_type_error("fn run(g: () -> Int) -> Int { g(1) }"),
        "a nullary function cannot be called with an argument"
    );
}

#[test]
fn forward_struct_annotation_is_enforced() {
    let src = "fn bad(point: Point) -> Int { point + 1 }\n\
               struct Point { x: Int }";
    assert!(
        has_type_error(src),
        "a forward-resolved Point annotation cannot degrade to a fresh variable"
    );
}

/// TY-09 stated positively: the exit test only asks that a `Tile` parameter is
/// not an `Int`, which a fresh variable also satisfies once *something* pins
/// it. This asks that the annotation *is* the enum — the thing
/// `lookup_enum_type` was written to do and never did, because nothing called
/// it and `scalar_from_name` asked only for a `struct`.
#[test]
fn a_user_type_annotation_is_the_type_it_names() {
    assert_eq!(
        scheme_of("enum Tile { Empty }\nfn f(t: Tile) -> Int { 0 }", "t").as_deref(),
        Some("Tile"),
        "an enum annotation"
    );
    assert_eq!(
        scheme_of("struct Point { x: Int }\nfn f(p: Point) -> Int { 0 }", "p").as_deref(),
        Some("Point"),
        "a struct annotation"
    );
    assert_eq!(
        scheme_of(
            "enum Tile { Empty }\nfn f(ts: Vec[Tile]) -> Int { 0 }",
            "ts"
        )
        .as_deref(),
        Some("Vec[Tile]"),
        "…and nested inside a collection"
    );
    assert!(
        !has_type_error("enum Tile { Empty, Wall }\nfn f(t: Tile) -> Tile { t }"),
        "an enum annotation that agrees with the body is fine"
    );
}

/// TY-10's ordering property, past the one case the exit test names: it is
/// *dependency* order, not "types first". A struct whose field names a struct
/// declared below it needs the second registered before the first, and neither
/// is a `fn`.
#[test]
fn a_type_declaration_is_registered_after_the_types_it_names() {
    assert_eq!(
        scheme_of(
            "struct Outer { inner: Inner }\n\
             struct Inner { n: Int }\n\
             fn f(o: Outer) -> Int { o.inner.n }",
            "o"
        )
        .as_deref(),
        Some("Outer"),
    );
    assert!(
        has_type_error(
            "struct Outer { inner: Inner }\n\
             struct Inner { n: Int }\n\
             fn f(o: Outer) -> Text { o.inner.n }"
        ),
        "the forward field type is a real Int, not a fresh variable"
    );
    // An enum payload naming a struct declared later, and back again.
    assert!(
        has_type_error(
            "enum Shape { Round(Circle) }\n\
             struct Circle { r: Int }\n\
             fn f() -> Shape { Round(1) }"
        ),
        "a forward enum payload type is enforced"
    );
}

/// A declaration cycle has no fixpoint in a type system without equirecursive
/// types, so the pass must not loop looking for one. It registers what is left
/// in source order, exactly as an unresolvable annotation has always been
/// handled — the point of the gate is that `analyze` returns.
#[test]
fn a_type_declaration_cycle_still_analyzes() {
    let analysis = analyze(
        "struct A { b: B }\n\
         struct B { a: A }\n\
         struct SelfRef { next: SelfRef }\n\
         fn f(x: Int) -> Int { x }",
    );
    // The acyclic declaration below the cycle is still registered.
    assert!(
        analysis
            .names
            .all()
            .iter()
            .any(|s| s.name == "f" && s.scheme.is_some()),
        "the rest of the file is still inferred"
    );
}

#[test]
fn value_binding_name_is_not_accepted_as_a_type() {
    let src = "let Alias = 1\nlet value: Alias = \"text\"";
    assert!(
        has_name_error(src),
        "ordinary value bindings are not type declarations"
    );
}

/// The exit test uses a `let`; every other value kind must be rejected the same
/// way, and the report must be `N003` — the name *is* known, so `N002 unknown
/// type` would be a lie about which mistake was made.
#[test]
fn a_value_in_type_position_is_reported_as_a_value() {
    for src in [
        "let Alias = 1\nlet value: Alias = 1",
        "var Alias = 1\nlet value: Alias = 1",
        "fn Alias() -> Int { 1 }\nlet value: Alias = 1",
        // A prelude value builtin: `out` resolves, and used to be a legal
        // annotation on that basis alone.
        "let value: out = 1",
        // …at any depth inside a structural annotation.
        "let Alias = 1\nfn f(x: (Int, Alias)) -> Int { 0 }",
        "let Alias = 1\nfn f(x: Vec[Alias]) -> Int { 0 }",
    ] {
        let codes: Vec<String> = analyze(src)
            .diagnostics
            .iter()
            .map(|d| d.code().to_string())
            .collect();
        assert!(
            codes.iter().any(|c| c == "N003"),
            "expected N003 for `{src}`, got {codes:?}"
        );
    }
}

/// …and the type names that *are* types still are. A kind check that rejected
/// too much would pass the test above and break every annotated program.
#[test]
fn every_kind_of_type_name_is_still_accepted_in_type_position() {
    for src in [
        "let value: Int = 1",
        "let value: Text = \"a\"",
        "struct Point { x: Int }\nlet value: Point = Point { x: 1 }",
        "enum Tile { Empty }\nlet value: Tile = Empty",
        "let value: Vec[Int] = Vec()",
        "let value: Map[Text, Int] = Map()",
        "let value: Option[Int] = Some(1)",
    ] {
        assert!(
            !has_name_error(src),
            "`{src}` names a type: {:?}",
            analyze(src)
                .diagnostics
                .iter()
                .map(|d| format!("{} {}", d.code(), d.message()))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn malformed_collection_type_arity_is_rejected() {
    let src = "fn identity(value: Map[Int]) -> Map[Int] { value }";
    assert!(
        !is_clean_with_lower(src),
        "Map has exactly two type arguments; malformed collection shapes must not enter TypeDb"
    );
}

// --- mutation and control-flow typing ---------------------------------------

#[test]
fn local_var_reassignment_preserves_its_type() {
    let src = "fn main() -> Int { var x = 0; x = \"bad\"; 0 }";
    assert!(
        has_type_error(src),
        "local assignments must use the resolver's exact lhs symbol"
    );
}

#[test]
fn reassignment_to_let_is_rejected() {
    let src = "fn main() -> Int { let x = 1; x = 2; x }";
    assert!(
        !is_clean_with_lower(src),
        "`let` is immutable; only `var` may be reassigned"
    );
}

#[test]
fn compound_assignment_requires_a_numeric_target() {
    let src = "var flag = true\nflag += false";
    assert!(
        has_type_error(src),
        "matching operand types alone do not make Bool addition valid"
    );
}

/// TY-13's other half: the disconnected lookup did not merely *miss* a local,
/// it could find the wrong symbol. A local named like a top-level binding
/// resolved out to the top-level one, and the assignment constrained *its*
/// type.
#[test]
fn an_assignment_constrains_the_local_it_names_and_no_other() {
    // `count` exists at both levels. Assigning the local Text must not make
    // the top-level `count` a Text.
    let src = "var count = 0\n               fn f() -> Int { var count = \"a\"; count = \"b\"; 0 }";
    assert!(!has_type_error(src), "the local assignment is well-typed");
    let schemes: Vec<String> = {
        let analysis = analyze(src);
        analysis
            .names
            .all()
            .iter()
            .filter(|s| s.name == "count" && s.kind == SymbolKind::Var)
            .filter_map(|s| s.scheme.as_ref())
            .map(|sc| analysis.db.render_scheme(sc))
            .collect()
    };
    assert_eq!(
        schemes,
        vec!["Int", "Text"],
        "each `count` keeps its own type"
    );
    // …and the local really is checked, rather than skipped for lack of a
    // binding to find.
    assert!(
        has_type_error("fn f() -> Int { var local = 0; local = \"bad\"; 0 }"),
        "a local with no top-level namesake is still checked"
    );
    // A captured `var` is assignable through the closure, and checked there.
    assert!(
        !has_type_error(
            "fn f() -> Int { var total = 0; let add = |n| { total += n }; add(1); total }"
        ),
        "a captured var may be assigned"
    );
    assert!(
        has_type_error("fn f() -> Int { var total = 0; let add = |n| { total = \"x\" }; 0 }"),
        "…and the capture is checked"
    );
}

/// TY-14 past the exit test's `let`: every immutable binding kind, and the
/// report is `Y009` rather than a type mismatch about the value.
#[test]
fn only_a_var_may_be_assigned() {
    for src in [
        "fn f() -> Int { let x = 1; x = 2; x }",
        "fn f(p: Int) -> Int { p = 2; p }",
        "fn f(v: Vec[Int]) -> Int { for x in v { x = 1 }; 0 }",
        "enum E { N(Int) }\nfn f(e: E) -> Int { match e { N(n) => { n = 1; n } } }",
    ] {
        let codes: Vec<String> = analyze(src)
            .diagnostics
            .iter()
            .map(|d| d.code().to_string())
            .collect();
        assert!(
            codes.iter().any(|c| c == "Y009"),
            "expected Y009 for `{src}`, got {codes:?}"
        );
    }
    assert!(
        !has_type_error("fn f() -> Int { var x = 1; x = 2; x }"),
        "a `var` is still assignable"
    );
}

/// TY-15 past the exit test's `Bool`: the rule is "numeric", not "not Bool",
/// and an unconstrained target is not yet a mistake.
#[test]
fn a_compound_assignment_needs_a_numeric_target() {
    for src in [
        "var flag = true\nflag += false",
        "var name = \"a\"\nname += \"b\"",
        "fn f() -> Int { var pair = (1, 2); pair += (3, 4); 0 }",
    ] {
        let codes: Vec<String> = analyze(src)
            .diagnostics
            .iter()
            .map(|d| d.code().to_string())
            .collect();
        assert!(
            codes.iter().any(|c| c == "Y010"),
            "expected Y010 for `{src}`, got {codes:?}"
        );
    }
    assert!(
        !has_type_error("var n = 0\nn += 1\nn -= 1\nn *= 2\nn /= 2\nn %= 2"),
        "every compound operator is fine on Int"
    );
    assert!(!has_type_error("var x = 1.5\nx += 0.5"), "…and on Float");
    assert!(
        !has_type_error("var n = 0\nn = 1"),
        "a plain `=` is not arithmetic and needs no numeric target"
    );
}

#[test]
fn if_without_else_cannot_produce_the_then_value_type() {
    // MIR materializes Unit on the false path, so the expression cannot have
    // type Int just because its then branch does.
    let src = "fn maybe(flag: Bool) -> Int { if flag { 1 } }";
    assert!(
        has_type_error(src),
        "the absent else path produces Unit, not Int"
    );
}

/// The half TY-17 must not break, and the reason it had to wait for TY-19: an
/// `if` with no `else` whose then branch *diverges* is still legal, because
/// there is no value to have nowhere to come from.
#[test]
fn an_else_less_if_is_unit_unless_its_branch_diverges() {
    // The ordinary case: no `else`, so the value is Unit.
    assert_eq!(
        scheme_of("fn f(c: Bool) -> Unit { if c { out(1) } }", "f").as_deref(),
        Some("(Bool) -> Unit")
    );
    // A divergent branch is absorbed, so these keep type-checking.
    for src in [
        "fn f(c: Bool) -> Int { if c { panic(\"x\") }\n  1 }",
        "enum E { A }\nfn f(c: Bool, e: E) -> Int { if c { match e { A => panic(\"x\") } }\n  1 }",
    ] {
        assert!(
            !has_type_error(src),
            "`{src}` reported {:?}",
            analyze(src)
                .diagnostics
                .iter()
                .map(|d| format!("{} {}", d.code(), d.message()))
                .collect::<Vec<_>>()
        );
    }
    // …and a real value with no else is still the mismatch the exit test names,
    // whatever the value's type.
    assert!(has_type_error(
        "fn f(c: Bool) -> Unit { if c { \"text\" } }"
    ));
    assert!(has_type_error("fn f(c: Bool) -> Unit { if c { (1, 2) } }"));
}

#[test]
fn early_return_value_must_match_the_function_result() {
    let src = "fn bad() -> Int { return \"wrong\"; 1 }";
    assert!(
        has_type_error(src),
        "an early return must be checked even when the block tail has the declared type"
    );
}

/// TY-18 in the shapes the exit test does not reach: a bare `return` is `Unit`,
/// an unannotated function has its result *pinned* by its returns, a `return`
/// nested in control flow is still checked, and a `return` inside a closure
/// means the closure.
#[test]
fn a_return_is_checked_against_the_function_it_leaves() {
    // A bare `return` produces Unit.
    assert!(has_type_error("fn f() -> Int { return\n  1 }"));
    assert!(!has_type_error("fn f() -> Unit { return\n  out(1) }"));
    // With no annotation, the returns pin the result.
    assert_eq!(
        scheme_of("fn f(c: Bool) { if c { return 1 }\n  2 }", "f").as_deref(),
        Some("(Bool) -> Int")
    );
    assert!(
        has_type_error("fn f(c: Bool) { if c { return 1 }\n  \"two\" }"),
        "a return and the tail must agree even with no annotation"
    );
    // Nested inside control flow, and inside a match arm.
    assert!(has_type_error(
        "fn f(c: Bool) -> Int { if c { return \"x\" }\n  1 }"
    ));
    assert!(has_type_error(
        "enum E { A }\nfn f(e: E) -> Int { match e { A => { return \"x\" } }\n  1 }"
    ));
    // A `return` inside a closure leaves the *closure*: `|n| { return n }` is
    // Int -> Int inside a function returning Text, and that is not an error.
    assert!(
        !has_type_error("fn f() -> Text { let g = |n| { return n }\n  g(\"a\") }"),
        "a closure's return is checked against the closure"
    );
    assert!(
        has_type_error("fn f() -> Text { let g = |n| { if true { return 1 }\n  \"t\" }\n  \"a\" }"),
        "…and it is still checked there"
    );
}

/// TY-19 applied at the function result, which no finding names separately: a
/// body that diverges cannot disagree with the declared type, because it
/// produces no value. This was a `Y001` before the join.
#[test]
fn a_function_whose_body_diverges_matches_any_declared_result() {
    for src in [
        "fn f() -> Int { panic(\"x\") }",
        "fn f() -> Text { panic(\"x\") }",
        "fn f(c: Bool) -> Int { if c { return 1 } else { panic(\"x\") } }",
        "fn f() -> Int { return 1 }",
    ] {
        assert!(
            !has_type_error(src),
            "`{src}` reported {:?}",
            analyze(src)
                .diagnostics
                .iter()
                .map(|d| format!("{} {}", d.code(), d.message()))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn expression_before_trailing_statement_is_not_the_block_value() {
    // Lowering correctly demotes `1` to an effect statement and gives this
    // block a Unit tail. Inference must make the same choice.
    let src = "fn bad() -> Int { 1; let x = 2 }";
    assert!(
        has_type_error(src),
        "inference and lowering must agree on the actual trailing expression"
    );
}

/// TY-16's rule, stated as the shape rather than as one rejection: the block's
/// value is the *last* statement, and only if that statement is an expression.
/// A pending tail is demoted by anything that follows it, whatever kind it is.
#[test]
fn a_blocks_value_is_its_last_statement_and_only_if_it_is_an_expression() {
    // A trailing expression is the value.
    assert_eq!(scheme_of("let b = { 1 }", "b").as_deref(), Some("Int"));
    assert_eq!(
        scheme_of("let b = { let x = 1; 2 }", "b").as_deref(),
        Some("Int")
    );
    // Every non-expression kind demotes a pending tail.
    for src in [
        "let b = { 1; let x = 2 }",
        "let b = { 1; var x = 2 }",
        "fn f() -> Unit { var x = 0; { 1; x = 2 } }",
    ] {
        assert!(
            !has_type_error(src),
            "`{src}` should be clean: {:?}",
            analyze(src)
                .diagnostics
                .iter()
                .map(|d| format!("{} {}", d.code(), d.message()))
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(
        scheme_of("let b = { 1; let x = 2 }", "b").as_deref(),
        Some("Unit"),
        "a `let` after the expression makes the block Unit"
    );
    // Two expression statements: only the second is the value.
    assert_eq!(
        scheme_of("let b = { 1; \"two\" }", "b").as_deref(),
        Some("Text")
    );
    // An empty block is Unit.
    assert_eq!(scheme_of("let b = { }", "b").as_deref(), Some("Unit"));
}

#[test]
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

/// TY-20's two codes, and the boundaries that decide them. A closure is a
/// *function* boundary: a `break` inside one cannot leave a loop outside it,
/// and a `return` inside one leaves the closure rather than nothing at all.
#[test]
fn a_terminator_needs_the_right_enclosing_context() {
    let codes = |src: &str| -> Vec<String> {
        analyze(src)
            .diagnostics
            .iter()
            .map(|d| d.code().to_string())
            .collect()
    };
    // `return` needs a function.
    assert!(codes("return 1").contains(&"Y011".to_string()));
    assert!(codes("let x = 1\nreturn").contains(&"Y011".to_string()));
    // `break`/`continue` need a loop, in every loop-less position.
    for src in [
        "fn f() -> Int { break\n  0 }",
        "fn f() -> Int { continue\n  0 }",
        "break",
        "fn f(c: Bool) -> Int { if c { break }\n  0 }",
    ] {
        assert!(
            codes(src).contains(&"Y012".to_string()),
            "expected Y012 for `{src}`, got {:?}",
            codes(src)
        );
    }
    // A closure is a function boundary in both directions.
    assert!(
        codes("fn f(v: Vec[Int]) -> Int { for x in v { let g = |n| { break }\n  0 }\n  0 }")
            .contains(&"Y012".to_string()),
        "a `break` inside a closure cannot leave a loop outside it"
    );
    assert!(
        !codes("fn f() -> Int { let g = |n| { return n }\n  g(1) }").contains(&"Y011".to_string()),
        "a `return` inside a closure leaves the closure"
    );
    // …and every legal position is still legal.
    for src in [
        "fn f() -> Int { return 1 }",
        "fn f(v: Vec[Int]) -> Int { for x in v { continue }\n  0 }",
        "fn f(c: Bool) -> Int { while c { break }\n  0 }",
        "fn f() -> Int { loop { break }\n  0 }",
        "fn f(v: Vec[Int]) -> Int { for x in v { if x == 1 { continue } }\n  0 }",
    ] {
        let found = codes(src);
        assert!(
            !found.contains(&"Y011".to_string()) && !found.contains(&"Y012".to_string()),
            "`{src}` is legal, got {found:?}"
        );
    }
}

#[test]
fn expression_loop_uses_its_break_value_type() {
    let src = "fn main() -> Int { loop { break 42 } }";
    assert!(
        !has_type_error(src),
        "an expression loop with `break Int` has type Int"
    );
}

/// TY-21's rule, stated rather than demonstrated once: a `loop` **is** the join
/// of the values its `break`s carry. The exit test only asks that one such
/// program is accepted, which a `loop` that stayed a fresh variable would also
/// satisfy — these ask what the type actually *is*, and what belongs to which
/// loop.
#[test]
fn a_loop_is_the_join_of_the_values_its_breaks_carry() {
    // The value is the break's, not the body's: the body here is `Unit`.
    assert_eq!(
        scheme_of("let x = loop { break 42 }", "x").as_deref(),
        Some("Int")
    );
    assert_eq!(
        scheme_of("let x = loop { break \"done\" }", "x").as_deref(),
        Some("Text")
    );
    // Two `break`s that agree; and a `break` nested inside the body still counts.
    assert_eq!(
        scheme_of(
            "fn f(c: Bool) -> Int { let x = loop { if c { break 1 } else { break 2 } }\n  x }",
            "x"
        )
        .as_deref(),
        Some("Int")
    );
    // A bare `break` leaves the loop with nothing, so the loop is `Unit`…
    assert_eq!(
        scheme_of("let x = loop { break }", "x").as_deref(),
        Some("Unit")
    );
    // …and mixing the two spellings is a mismatch, not a coincidence.
    assert!(
        has_type_error("fn f(c: Bool) -> Int { let x = loop { if c { break }\n  break 1 }\n  0 }"),
        "a bare `break` contributes Unit and cannot agree with `break 1`"
    );
    assert!(
        has_type_error(
            "fn f(c: Bool) -> Int { let x = loop { if c { break 1 }\n  break \"two\" }\n  0 }"
        ),
        "two `break`s carrying different types disagree"
    );
    // A `break` belongs to the innermost loop: the inner one is `Int`, and the
    // outer one is `Text` rather than a join across both.
    assert_eq!(
        scheme_of(
            "let x = loop { let inner = loop { break 1 }\n  break \"outer\" }",
            "x"
        )
        .as_deref(),
        Some("Text")
    );
    assert_eq!(
        scheme_of(
            "let x = loop { let inner = loop { break 1 }\n  break \"outer\" }",
            "inner"
        )
        .as_deref(),
        Some("Int")
    );
}

/// D2's first half: a `loop` no `break` leaves produces nothing, so it is
/// `Never` — the bottom type, absorbed wherever branches meet (TY-19). It used
/// to be `Unit`, which made every one of these a `Y001`.
#[test]
fn a_loop_no_break_leaves_is_never() {
    for src in [
        "fn f(c: Bool) -> Int { if c { 1 } else { loop { } } }",
        "fn f() -> Int { loop { } }",
        "fn f() -> Text { loop { } }",
        // Exited only by `return`: still no `break`, still `Never`.
        "fn f(c: Bool) -> Int { loop { if c { return 1 } } }",
    ] {
        assert!(
            !has_type_error(src),
            "`{src}` should be clean: {:?}",
            analyze(src)
                .diagnostics
                .iter()
                .map(|d| format!("{} {}", d.code(), d.message()))
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(
        scheme_of(
            "fn f(c: Bool) -> Int { let x = loop { if c { return 1 } }\n  x }",
            "x"
        )
        .as_deref(),
        Some("Never"),
        "the bottom type, not Unit and not a fresh variable"
    );
}

/// D2's second half, as `Y017`: only a `loop` is an expression loop. A `while`
/// or `for` also leaves by its condition failing, and there is no value the
/// compiler could supply on that path.
#[test]
fn only_a_loop_may_break_with_a_value() {
    let codes = |src: &str| -> Vec<String> {
        analyze(src)
            .diagnostics
            .iter()
            .map(|d| d.code().to_string())
            .collect()
    };
    for src in [
        "fn f(c: Bool) -> Int { while c { break 1 }\n  0 }",
        "fn f(v: Vec[Int]) -> Int { for x in v { break x }\n  0 }",
    ] {
        let found = codes(src);
        assert!(
            found.contains(&"Y017".to_string()),
            "expected Y017 for `{src}`, got {found:?}"
        );
        assert!(
            !found.contains(&"Y012".to_string()),
            "the loop exists — it is the kind of loop that is wrong: {found:?}"
        );
    }
    // A bare `break` is what those two loops are for, and is untouched.
    for src in [
        "fn f(c: Bool) -> Int { while c { break }\n  0 }",
        "fn f(v: Vec[Int]) -> Int { for x in v { break }\n  0 }",
        "fn f() -> Int { loop { break }\n  0 }",
        "fn f() -> Int { loop { break 1 } }",
    ] {
        assert!(
            !codes(src).contains(&"Y017".to_string()),
            "`{src}` is legal, got {:?}",
            codes(src)
        );
    }
    // The nearest loop decides: a value `break` in a `while` nested inside a
    // `loop` is still `Y017`.
    assert!(
        codes("fn f(c: Bool) -> Int { loop { while c { break 1 } }\n  0 }")
            .contains(&"Y017".to_string()),
        "the `break` leaves the `while`, not the `loop` around it"
    );
}

#[test]
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

/// TY-19 past the exit test's one shape: a divergent branch is absorbed
/// wherever branches meet, in either position, and a `match` whose every arm
/// diverges is itself `Never` rather than "whatever the first use wants".
#[test]
fn a_divergent_branch_is_absorbed_wherever_branches_meet() {
    for src in [
        // Either side of an `if`.
        "fn f(c: Bool) -> Int { if c { panic(\"x\") } else { 1 } }",
        "fn f(c: Bool) -> Int { if c { 1 } else { panic(\"x\") } }",
        // Nested, so the join has to happen at both levels.
        "fn f(c: Bool) -> Int { if c { if c { panic(\"x\") } else { 1 } } else { 2 } }",
        // Match arms.
        "enum E { A, B }\nfn f(e: E) -> Int { match e { A => 1, B => panic(\"x\") } }",
        "enum E { A, B }\nfn f(e: E) -> Int { match e { A => panic(\"x\"), B => 2 } }",
    ] {
        assert!(
            !has_type_error(src),
            "a divergent branch constrains nothing: `{src}` reported {:?}",
            analyze(src)
                .diagnostics
                .iter()
                .map(|d| format!("{} {}", d.code(), d.message()))
                .collect::<Vec<_>>()
        );
    }
    // Every arm diverges: the match produces no value, so it is Never — and a
    // fresh variable would have made it silently agree with any use.
    assert_eq!(
        scheme_of(
            "enum E { A, B }\nfn f(e: E) -> Int { let m = match e { A => panic(\"x\"), B => panic(\"y\") }; 0 }",
            "m"
        )
        .as_deref(),
        Some("Never")
    );
}

/// …and the join is not a licence to mix. Two ordinary branches still have to
/// agree, so the absorbing rule cannot be reached by widening everything.
#[test]
fn two_ordinary_branches_still_have_to_agree() {
    assert!(has_type_error(
        "fn f(c: Bool) -> Int { if c { 1 } else { \"two\" } }"
    ));
    assert!(has_type_error(
        "enum E { A, B }\nfn f(e: E) -> Int { match e { A => 1, B => \"two\" } }"
    ));
}

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
fn forward_call_is_checked_against_later_function_signature() {
    let src = "fn first() -> Int { later(\"wrong\") }\n\
               fn later(value: Int) -> Int { value }";
    assert!(
        has_type_error(src),
        "two-pass resolution must be paired with placeholders for all function signatures"
    );
}

#[test]
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

/// …and it is *reported*, not merely survived. The exit test only asks that
/// `analyze` returns; `N005` is what tells the programmer why the function they
/// wrote does not exist.
#[test]
fn a_nested_function_is_reported_as_one() {
    let analysis = analyze("fn main() -> Int { fn local(x: Int) -> Int { x }\n local(1) }");
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.kind() == praxis_source::DiagCode::NestedFunction),
        "{:?}",
        analysis.diagnostics
    );
}

/// The redeclaration is named where the second one is written, and the *first*
/// definition survives — so the rest of the file resolves against something
/// rather than cascading into `N001`s.
#[test]
fn a_duplicate_function_is_reported_once_and_the_first_one_survives() {
    let src = "fn duplicate() -> Int { 1 }\n\
               fn duplicate() -> Int { 2 }\n\
               fn main() -> Int { duplicate() }";
    let analysis = analyze(src);
    let duplicates: Vec<_> = analysis
        .diagnostics
        .iter()
        .filter(|d| d.kind() == praxis_source::DiagCode::DuplicateDeclaration)
        .collect();
    assert_eq!(duplicates.len(), 1, "{:?}", analysis.diagnostics);
    assert!(
        !analysis
            .diagnostics
            .iter()
            .any(|d| d.kind() == praxis_source::DiagCode::UnknownName),
        "the call must still resolve: {:?}",
        analysis.diagnostics
    );
}

/// A `fn` of one name in two *different* scopes is not a redeclaration, and a
/// `let` shadowing a prelude name never was — `is_bound_here` is what keeps the
/// check from firing on either.
#[test]
fn shadowing_an_outer_name_is_not_a_redeclaration() {
    for src in [
        "fn main() -> Int { let out = 1\n out }",
        "fn f(duplicate: Int) -> Int { duplicate }\nfn duplicate() -> Int { 1 }\nfn main() -> Int { f(2) }",
    ] {
        let analysis = analyze(src);
        assert!(
            !analysis
                .diagnostics
                .iter()
                .any(|d| d.kind() == praxis_source::DiagCode::DuplicateDeclaration),
            "{src}: {:?}",
            analysis.diagnostics
        );
    }
}

// --- records and match exhaustiveness ---------------------------------------

#[test]
fn record_literal_requires_every_declared_field() {
    let src = "struct Pair { left: Int, right: Int }\n\
               fn main() -> Int { let pair = Pair { left: 1 }; pair.right }";
    assert!(
        !is_clean_with_lower(src),
        "allocating a record with fewer payloads than its schema is invalid"
    );
}

#[test]
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
fn record_literal_rejects_duplicate_fields() {
    let src = "struct Point { x: Int }\n\
               fn main() -> Int { let point = Point { x: 1, x: 2 }; point.x }";
    assert!(
        !is_clean_with_lower(src),
        "each record field must be initialized exactly once"
    );
}

/// FE-02/D7. A wildcard binds nothing, so `_` is not readable as a value.
///
/// **Rewritten**, not merely un-ignored: the assertion was `has_name_error`,
/// which was the only failure available while `_` lexed as an `Ident` — the arm
/// body was a *reference* to an undeclared name. Now `_` is its own token and
/// has no expression form at all, so the parser rejects it where it stands.
/// The property is the same one; the category it is reported under is not.
#[test]
fn wildcard_pattern_does_not_bind_a_value_named_underscore() {
    let src = "fn main() -> Int { match 1 { _ => _ } }";
    let map = SourceMap::new();
    let id = map.intern("wildcard_test.px", src);
    let parsed = parse(id, src);
    assert!(
        parsed
            .diagnostics
            .iter()
            .any(|d| d.code().category() == DiagnosticCategory::Parse),
        "the wildcard is not a binding visible in the arm body: {:?}",
        parsed.diagnostics
    );
    // …and the pattern position is still fine.
    assert!(is_clean_with_lower(
        "fn main() -> Int { match 1 { _ => 0 } }"
    ));
}

/// D7's other three positions: a binding a program deliberately does not name
/// is legal, introduces nothing, and still *runs* its initializer.
#[test]
fn a_wildcard_binder_is_legal_and_declares_nothing() {
    for src in [
        "fn main() -> Int { let _ = 1; 0 }",
        "fn g(_) -> Int { 0 }\nfn main() -> Int { g(1) }",
        "fn main() -> Int { let f = |_| 0; f(1) }",
    ] {
        assert!(is_clean_with_lower(src), "`{src}` should compile clean");
        let analysis = analyze(src);
        assert!(
            !analysis.names.all().iter().any(|s| s.name == "_"),
            "`{src}` declared a symbol named `_`: {:?}",
            analysis.names.all().len()
        );
    }
}

#[test]
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

/// HIR-09's other half: a capture whose first sighting is an assignment
/// *target* keeps the type of the binding it names. Inference records a type at
/// a name it reads; a write has no such record, so the capture fell back to a
/// fresh variable and the env slot carried `?T` for a `var` every other pass
/// knew was an `Int`.
#[test]
fn a_capture_first_seen_as_an_assignment_target_keeps_its_type() {
    use praxis_ast::AstNode;

    let src =
        "fn main() -> Int { var total = 0\n  let add = |n| { total = n }\n  add(5)\n  total }";
    let map = SourceMap::new();
    let id = map.intern("capture_assign_test.px", src);
    let parsed = parse(id, src);
    let mut analysis = analyze_root(id, &parsed.tree);
    let module = crate::lower::lower(
        id,
        &praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap(),
        &mut analysis,
    );

    fn find_closure(e: &crate::TypedExpr) -> Option<&crate::TypedExpr> {
        if matches!(e, crate::TypedExpr::Closure { .. }) {
            return Some(e);
        }
        e.children()
            .find_map(find_closure)
            .or_else(|| e.blocks().find_map(find_closure_in_block))
    }
    fn find_closure_in_block(b: &crate::TypedBlock) -> Option<&crate::TypedExpr> {
        b.stmts
            .iter()
            .find_map(|s| match s {
                crate::TypedStmt::Let { init, .. } | crate::TypedStmt::Var { init, .. } => {
                    find_closure(init)
                }
                crate::TypedStmt::Assign { value, .. } => find_closure(value),
                crate::TypedStmt::Expr(e) => find_closure(e),
            })
            .or_else(|| find_closure(&b.tail))
    }

    let crate::TypedItem::Fn(main) = &module.items[0];
    let closure = find_closure_in_block(&main.body).expect("the closure");
    let crate::TypedExpr::Closure { captures, .. } = closure else {
        unreachable!("find_closure returns a closure")
    };
    let total = captures
        .iter()
        .find(|c| c.name == "total")
        .expect("`total` is captured");
    assert_eq!(
        analysis.db.render(total.ty),
        "Int",
        "the capture carries the binding's type, not a fresh variable"
    );
    assert!(
        matches!(total.kind, crate::capture::CaptureKind::ByCell),
        "a captured `var` is shared through a cell"
    );
}

/// F20: the one child walker really does cover the enum. A closure is placed in
/// every expression *field* the macro lists, and the walk must find all of them
/// — a field left out of a variant's row loses its subtree silently, which is
/// exactly how HIR-08 hid for three walks and one release.
///
/// The program is deliberately not type-correct in every position (a closure is
/// not an `Int`); lowering builds the nodes regardless, and it is the shape of
/// the tree this asks about, not its types.
#[test]
fn the_child_walker_reaches_every_expression_position() {
    use praxis_ast::AstNode;

    // One closure per position, numbered so a failure names the missing one.
    let src = concat!(
        "struct R { f: Int }\n",
        "enum E { V(Int) }\n",
        "fn main(c: Bool, v: Vec[Int], k: Int) -> Int {\n",
        "  let a = |n| 1\n",                // Let init
        "  var b = |n| 2\n",                // Var init
        "  b = |n| 3\n",                    // Assign value
        "  let d = (|n| 4)(0)\n",           // Call.callee_expr (HIR-08)
        "  out(|n| 5)\n",                   // Call.args
        "  let w = v.map(|n| 6)\n",         // MethodCall.args
        "  let y = v.map(|n| 7).len()\n",   // MethodCall.receiver
        "  let t = (|n| 8, |n| 9)\n",       // Tuple.elements
        "  let p = (|n| 10)\n",             // Paren.inner
        "  let u = !(|n| 11)\n",            // Unary.operand
        "  let z = (|n| 12) == (|n| 13)\n", // Bin.lhs / Bin.rhs
        "  let g = R { f: |n| 14 }.f\n",    // RecordLit.fields, FieldGet.receiver
        "  let e = V(|n| 15)\n",            // EnumVariant.args
        "  if (|n| 16)(0) { let h = |n| 17 } else { let i = |n| 18 }\n", // If cond + branches
        "  while c { let j = |n| 19 }\n",   // While.body
        "  for x in v { let l = |n| 20 }\n", // For.body
        "  let m = loop { let o = |n| 21\n  break |n| 22 }\n", // Loop.body, Break.value
        "  let q = match (|n| 23)(0) { _ => |n| 24 }\n", // Match.scrutinee + arms
        "  return |n| 25\n",                // Return.value
        "}\n"
    );
    let map = SourceMap::new();
    let id = map.intern("child_walk_test.px", src);
    let parsed = parse(id, src);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let mut analysis = analyze_root(id, &parsed.tree);
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
    let module = crate::lower::lower(id, &root, &mut analysis);

    fn walk_expr(e: &crate::TypedExpr, found: &mut Vec<String>) {
        if let crate::TypedExpr::Closure { fn_name, .. } = e {
            found.push(fn_name.clone());
        }
        for child in e.children() {
            walk_expr(child, found);
        }
        for block in e.blocks() {
            walk_block(block, found);
        }
    }
    fn walk_block(b: &crate::TypedBlock, found: &mut Vec<String>) {
        for stmt in &b.stmts {
            match stmt {
                crate::TypedStmt::Let { init, .. } | crate::TypedStmt::Var { init, .. } => {
                    walk_expr(init, found);
                }
                crate::TypedStmt::Assign { value, .. } => walk_expr(value, found),
                crate::TypedStmt::Expr(e) => walk_expr(e, found),
            }
        }
        walk_expr(&b.tail, found);
    }

    let mut found = Vec::new();
    for item in &module.items {
        let crate::TypedItem::Fn(f) = item;
        walk_block(&f.body, &mut found);
    }
    // Every closure the lowerer minted has a distinct `__closure_N` name, so the
    // walk finding all of them is the walk reaching every position.
    let minted: usize = src.matches("|n|").count();
    assert_eq!(
        found.len(),
        minted,
        "the child walk found {} of {minted} closures: {found:?}",
        found.len()
    );
    let unique: std::collections::HashSet<_> = found.iter().collect();
    assert_eq!(unique.len(), found.len(), "a closure was visited twice");
}

// --- TY-07: type constructors validate their own arguments ------------------

/// A wrong number of type arguments in an annotation is named where it was
/// written (`Y007`), not as a downstream `Y001` about a type the user never
/// wrote. Before F5 the annotation interned a `Map[Text]` that could not unify
/// with anything, so the only report came from the first use.
#[test]
fn a_wrong_type_argument_count_is_reported_at_the_annotation() {
    for src in [
        "fn main() -> Int { let m: Map[Text] = Map(); 0 }",
        "fn main() -> Int { let v: Vec[Int, Text] = Vec(); 0 }",
        // A nominal def has a parameter count too, since F12 — `Option` is one
        // definition applied to arguments rather than a name stamped per site.
        "fn main() -> Int { let o: Option[Int, Text] = None; 0 }",
    ] {
        let codes: Vec<u32> = analyze(src)
            .diagnostics
            .iter()
            .filter(|d| d.code().category() == DiagnosticCategory::Type)
            .map(|d| d.code().number())
            .collect();
        assert!(
            codes.contains(&7),
            "`{src}` must report Y007, got {codes:?}"
        );
    }
    // The right count still type-checks clean.
    assert!(!has_type_error(
        "fn main() -> Int { let m: Map[Text, Int] = Map(); 0 }"
    ));
}

/// A declaration that names one member twice is rejected (`Y008`). It used to
/// register a def holding both, and every lookup answered the first — so the
/// second field was silently unreachable rather than diagnosed.
#[test]
fn a_duplicate_field_or_variant_is_rejected() {
    assert!(
        has_type_error("struct Point { x: Int, x: Text }\nfn main() -> Int { 0 }"),
        "a struct may not declare the same field twice"
    );
    assert!(
        has_type_error("enum Tile { Empty, Empty }\nfn main() -> Int { 0 }"),
        "an enum may not declare the same variant twice"
    );
    assert!(
        !has_type_error("struct Point { x: Int, y: Text }\nfn main() -> Int { 0 }"),
        "distinct field names are fine"
    );
}

// ---------------------------------------------------------------------------
// F15 — the per-node inferred-type map, and lowering as its reader.
// ---------------------------------------------------------------------------

/// A program with a closure in each of the twenty-five expression positions
/// F20's walker gate enumerates, plus the shapes inference reaches without
/// going through `infer_expr` (branch, loop and function bodies).
///
/// Shared by the F15 gates below: "every expression node has a type" is only
/// worth asserting over a tree that has every expression node in it.
const EVERY_EXPRESSION_POSITION: &str = concat!(
    "struct R { f: Int }\n",
    "enum E { V(Int) }\n",
    "fn main(c: Bool, v: Vec[Int], k: Int) -> Int {\n",
    "  let a = |n| 1\n",
    "  var b = |n| 2\n",
    "  b = |n| 3\n",
    "  let d = (|n| 4)(0)\n",
    "  out(|n| 5)\n",
    "  let w = v.map(|n| 6)\n",
    "  let y = v.map(|n| 7).len()\n",
    "  let t = (|n| 8, |n| 9)\n",
    "  let p = (|n| 10)\n",
    "  let u = !(|n| 11)\n",
    "  let z = (|n| 12) == (|n| 13)\n",
    "  let g = R { f: |n| 14 }.f\n",
    "  let e = V(|n| 15)\n",
    "  if (|n| 16)(0) { let h = |n| 17 } else { let i = |n| 18 }\n",
    "  while c { let j = |n| 19 }\n",
    "  for x in v { let l = |n| 20 }\n",
    "  let m = loop { let o = |n| 21\n  break |n| 22 }\n",
    "  let q = match (|n| 23)(0) { _ => |n| 24 }\n",
    "  return |n| 25\n",
    "}\n"
);

/// **F15.** Inference records a type for *every* expression node it visits, and
/// it visits every expression node. The map is what lets lowering read instead
/// of re-deriving; a map with holes in it would just move the fresh-variable
/// fallback from lowering into whoever consumes the map.
#[test]
fn every_expression_node_has_a_recorded_type() {
    let map = SourceMap::new();
    let id = map.intern("expr_types_total.px", EVERY_EXPRESSION_POSITION);
    let parsed = parse(id, EVERY_EXPRESSION_POSITION);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let analysis = analyze_root(id, &parsed.tree);

    let mut missing: Vec<String> = Vec::new();
    let mut seen = 0usize;
    for node in parsed.tree.descendants() {
        // A parser expression (`read grid(char)`'s argument) is its own grammar
        // and has no `Type`; `Expr::cast` already refuses it.
        let Some(expr) = praxis_ast::Expr::cast(node.clone()) else {
            continue;
        };
        seen += 1;
        if !analysis
            .expr_types
            .contains_key(&crate::NodeKey::of(expr.syntax()))
        {
            missing.push(format!("{:?} at {:?}", node.kind(), node.text_range()));
        }
    }
    assert!(
        seen > 60,
        "the fixture should have many expressions: {seen}"
    );
    assert!(
        missing.is_empty(),
        "{} expression node(s) inference visited with no recorded type: {missing:?}",
        missing.len()
    );
}

/// **F15.** Lowering never invents a type. `Y099` is what a miss looks like now,
/// and a program covering every expression position produces none — which is
/// the same statement as the test above, made where it matters: the pass that
/// used to fall back to a fresh variable nineteen times over.
#[test]
fn lowering_invents_no_type_for_any_expression_position() {
    use praxis_ast::AstNode;

    let map = SourceMap::new();
    let id = map.intern("no_invented_types.px", EVERY_EXPRESSION_POSITION);
    let parsed = parse(id, EVERY_EXPRESSION_POSITION);
    let mut analysis = analyze_root(id, &parsed.tree);
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
    let module = crate::lower::lower(id, &root, &mut analysis);
    let invented: Vec<_> = module
        .diagnostics
        .iter()
        .filter(|d| d.code() == praxis_source::DiagCode::InternalMissingType.code())
        .collect();
    assert!(
        invented.is_empty(),
        "lowering asked for a type inference never recorded: {invented:?}"
    );
}

/// **F15.** A `NodeKey` is not a `TextRange`. A `PATH_EXPR` and the `Ident`
/// token inside it occupy the *same* range, which is why the per-node map could
/// not have been keyed by range beside `ref_types` — one would have overwritten
/// the other, silently, exactly where a name reference and its expression meet.
#[test]
fn a_node_key_separates_an_expression_from_the_name_inside_it() {
    use praxis_ast::AstNode;

    let src = "fn main() -> Int { let x = 1\n  x }\n";
    let map = SourceMap::new();
    let id = map.intern("node_key.px", src);
    let parsed = parse(id, src);
    let analysis = analyze_root(id, &parsed.tree);

    let path = parsed
        .tree
        .descendants()
        .find(|n| n.kind() == praxis_syntax::SyntaxKind::PATH_EXPR)
        .expect("the `x` reference");
    let name_tok = praxis_ast::PathExpr::from_syntax(path.clone())
        .name()
        .expect("the `x` token");
    assert_eq!(
        path.text_range(),
        name_tok.text_range(),
        "the collision this key exists to make unrepresentable"
    );
    // One range, two maps, two answers — and neither can displace the other.
    assert!(analysis.ref_types.contains_key(&name_tok.text_range()));
    assert!(analysis.expr_types.contains_key(&crate::NodeKey::of(&path)));
    // …and a key carries the kind, so a same-range node of another kind is a
    // different key rather than the same one.
    assert_ne!(
        crate::NodeKey::of(&path),
        crate::NodeKey::of(&path.parent().expect("a parent node")),
    );
}

/// **HIR-01.** The two exit tests pin a call and a method call; this pins the
/// shapes lowering used to answer with something *other* than a second
/// instantiation — the branch points, where it recomputed a join it had no need
/// to, and got a different answer whenever one branch diverged.
#[test]
fn a_lowered_branch_carries_the_join_not_its_first_arm() {
    use praxis_ast::AstNode;

    let src = concat!(
        "fn main(c: Bool, n: Int) -> Int {\n",
        "  let a = if c { panic(\"x\") } else { 1 }\n",
        "  let b = match n { 0 => panic(\"y\"), _ => 2 }\n",
        "  a + b\n",
        "}\n"
    );
    let map = SourceMap::new();
    let id = map.intern("lowered_join.px", src);
    let parsed = parse(id, src);
    let mut analysis = analyze_root(id, &parsed.tree);
    assert!(
        analysis.diagnostics.is_empty(),
        "{:?}",
        analysis.diagnostics
    );
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
    let module = crate::lower::lower(id, &root, &mut analysis);
    let main = module
        .items
        .iter()
        .find_map(|item| match item {
            crate::TypedItem::Fn(f) if f.name == "main" => Some(f),
            _ => None,
        })
        .expect("main");
    let mut rendered = Vec::new();
    for stmt in &main.body.stmts {
        if let crate::TypedStmt::Let { name, init, .. } = stmt {
            rendered.push((name.clone(), analysis.db.render(crate::expr_ty(init))));
        }
    }
    assert_eq!(
        rendered,
        vec![
            ("a".to_string(), "Int".to_string()),
            ("b".to_string(), "Int".to_string()),
        ],
        "a divergent arm is absorbed; reading the first one answers `Never`"
    );
}

/// **HIR-02.** A method name is not a name reference. It has no entry in
/// `refs` — so hover, which asks `refs` first, could never see anything about
/// it — and its result used to be written into `ref_types` at the same range,
/// a map only reference consumers read.
#[test]
fn a_method_name_is_not_a_name_reference() {
    let src = "fn main(v: Vec[Int]) -> Int { v.len() }\n";
    let map = SourceMap::new();
    let id = map.intern("method_ref.px", src);
    let parsed = parse(id, src);
    let analysis = analyze_root(id, &parsed.tree);
    let name_tok = parsed
        .tree
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.text() == "len")
        .expect("the `len` token");
    let range = name_tok.text_range();
    assert!(
        !analysis.refs.contains_key(&range),
        "a method resolves to a catalog entry, not a symbol"
    );
    let m = analysis
        .method_refs
        .get(&range)
        .expect("the method's own map is where it lives");
    assert_eq!(m.entry.name, "len");
    assert_eq!(
        analysis.db.render(analysis.db.follow(m.receiver)),
        "Vec[Int]"
    );
    assert_eq!(analysis.db.render(analysis.db.follow(m.result)), "Int");
}

/// **HIR-03** as the rule, not one shadowing case. A constructor is a *symbol*
/// with `SymbolKind::EnumVariant`, so every question about "is this name a
/// constructor" has one answer — including for the prelude's `Some`/`None`,
/// which no `enum` item declares.
#[test]
fn a_constructor_is_a_symbol_kind_not_a_spelling() {
    let a = analyze("enum E { A, B(Int) }\nfn main() -> Int { 0 }\n");
    let kinds: Vec<(String, SymbolKind)> = a
        .names
        .all()
        .iter()
        .filter(|s| ["A", "B", "Some", "None", "E", "main"].contains(&s.name.as_str()))
        .map(|s| (s.name.clone(), s.kind))
        .collect();
    for (name, kind) in &kinds {
        let expected = match name.as_str() {
            "A" | "B" | "Some" | "None" => SymbolKind::EnumVariant,
            "E" => SymbolKind::Enum,
            _ => SymbolKind::Fn,
        };
        assert_eq!(kind, &expected, "`{name}` is {kind:?}");
    }
    assert_eq!(kinds.len(), 6, "every name accounted for: {kinds:?}");
}

/// …and the half a kind check alone would not give: a local that *holds* a
/// variant has the enum's type too, so the scheme cannot tell a constructor
/// from a binding. Only the kind can.
#[test]
fn a_local_holding_a_variant_is_not_a_constructor() {
    use praxis_ast::AstNode;

    let src = "enum E { A }\nfn main() -> Int {\n  let A = 7\n  A\n}\n";
    let map = SourceMap::new();
    let id = map.intern("variant_value.px", src);
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
        .expect("main");
    assert!(
        matches!(main.body.tail, crate::TypedExpr::Path { .. }),
        "the local shadows the constructor: {:?}",
        main.body.tail
    );
    // The `let A = 7` must survive too — lowering the tail as a constructor
    // also discarded the binding's value.
    assert_eq!(main.body.stmts.len(), 1, "the binding is still there");
}

/// **HIR-07** as the rule. A misspelled constructor is `Y122`; a constructor
/// pattern against a type that has no variants at all is `Y123`; and the
/// arms that *are* right still work.
#[test]
fn a_pattern_that_names_no_variant_is_reported_not_widened() {
    let typo = analyze_and_lower_diags(
        "enum E { A, B }\nfn main() -> Int { match A { Typo(p) => 1, _ => 0 } }",
    );
    assert!(
        typo.iter().any(|d| d.code().number() == 122),
        "a typo is Y122, not a wildcard: {typo:?}"
    );

    let wrong_shape =
        analyze_and_lower_diags("fn main() -> Int { match 1 { Some(n) => n, _ => 0 } }");
    assert!(
        wrong_shape.iter().any(|d| d.code().number() == 123),
        "a constructor pattern on an Int is Y123: {wrong_shape:?}"
    );

    let good =
        analyze_and_lower_diags("enum E { A, B }\nfn main() -> Int { match A { A => 1, B => 2 } }");
    assert!(
        good.is_empty(),
        "the right constructors still work: {good:?}"
    );
}

/// **HIR-07's second half.** A non-exhaustive `match` is reported at the
/// `match`, not at byte 0 of the file — which for a program with two of them
/// named neither.
#[test]
fn a_non_exhaustive_match_is_reported_where_it_is_written() {
    let src = "enum E { A, B }\nfn main() -> Int {\n  let x = A\n  match x { A => 1 }\n}\n";
    let diags = analyze_and_lower_diags(src);
    let y120 = diags
        .iter()
        .find(|d| d.code().number() == 120)
        .expect("a missing `B` arm");
    let span = y120.primary().span;
    let start = span.start().to_usize();
    assert!(
        src.replace("\\n", "\n")[start..].starts_with("match"),
        "the span points at the `match`, not at byte 0 (start {start})"
    );
}

/// **HIR-04** as the rule: a record literal names every declared field exactly
/// once and nothing else. Each half has its own code, so a program with two
/// mistakes reports two things.
#[test]
fn a_record_literal_names_every_field_exactly_once() {
    let missing = analyze_and_lower_diags(
        "struct P { x: Int, y: Int }\nfn main() -> Int { let p = P { x: 1 }\n  p.x }",
    );
    assert!(
        missing.iter().any(|d| d.code().number() == 113),
        "a missing field is Y113: {missing:?}"
    );

    let unknown = analyze_and_lower_diags(
        "struct P { x: Int }\nfn main() -> Int { let p = P { x: 1, typo: 2 }\n  p.x }",
    );
    assert!(
        unknown.iter().any(|d| d.code().number() == 114),
        "an unknown field is Y114: {unknown:?}"
    );

    let duplicate = analyze_and_lower_diags(
        "struct P { x: Int }\nfn main() -> Int { let p = P { x: 1, x: 2 }\n  p.x }",
    );
    assert!(
        duplicate.iter().any(|d| d.code().number() == 115),
        "a duplicate field is Y115: {duplicate:?}"
    );

    let good = analyze_and_lower_diags(
        "struct P { x: Int, y: Int }\nfn main() -> Int { let p = P { y: 2, x: 1 }\n  p.x }",
    );
    assert!(
        good.is_empty(),
        "order is not the rule — every field, once: {good:?}"
    );
}

/// …and an unknown field's initializer is still *type-checked*, because it is
/// an expression the program wrote. It used to be skipped entirely, which is
/// how `Point { x: 1, typo: side_effect() }` deleted the call.
#[test]
fn an_unknown_fields_initializer_is_still_checked() {
    let diags = analyze_and_lower_diags(
        "struct P { x: Int }\n\
         fn main() -> Int { let p = P { x: 1, typo: \"text\" + 1 }\n  p.x }",
    );
    assert!(
        diags.iter().any(|d| d.code().number() == 114),
        "the unknown field itself: {diags:?}"
    );
    assert!(
        diags.len() > 1,
        "its initializer is inferred too, so its own error is reported: {diags:?}"
    );
}

/// **HIR-06's first half as the rule.** Exhaustiveness is a question about
/// *values*, so it is asked at every position a value has — not only at the
/// top. The old check compared top-level variant indices, which made a
/// one-variant enum exhaustive no matter what its payload said.
#[test]
fn a_match_covers_every_payload_position_not_just_the_outer_constructor() {
    let enums = "enum Flag { On, Off }\nenum Wrapped { Wrap(Flag) }\n";

    // `Wrap` is the *only* variant of `Wrapped`, so a top-level check calls
    // this exhaustive. `Wrap(Off)` is a value it does not match.
    let one = analyze_and_lower_diags(&format!(
        "{enums}fn main() -> Int {{ let v = Wrap(On)\n  match v {{ Wrap(On) => 1 }} }}"
    ));
    assert!(
        one.iter().any(|d| d.code().number() == 120),
        "an uncovered payload constructor is Y120: {one:?}"
    );

    // …and covering both closes it, which is the half a blanket rejection of
    // nested patterns would also satisfy.
    let both = analyze_and_lower_diags(&format!(
        "{enums}fn main() -> Int {{ let v = Wrap(On)\n  \
         match v {{ Wrap(On) => 1, Wrap(Off) => 2 }} }}"
    ));
    assert!(both.is_empty(), "both payload cases are covered: {both:?}");

    // A wildcard payload covers every constructor under it, at any depth.
    let wild = analyze_and_lower_diags(&format!(
        "{enums}fn main() -> Int {{ let v = Wrap(On)\n  match v {{ Wrap(_) => 1 }} }}"
    ));
    assert!(wild.is_empty(), "`Wrap(_)` is all of Wrapped: {wild:?}");

    // …and so does a binding, which is the same constructor set by a different
    // pattern form.
    let bound = analyze_and_lower_diags(&format!(
        "{enums}fn main() -> Int {{ let v = Wrap(On)\n  match v {{ Wrap(f) => 1 }} }}"
    ));
    assert!(bound.is_empty(), "`Wrap(f)` is all of Wrapped: {bound:?}");

    // Two levels down, to show the recursion is not one special case.
    let deep = "enum Flag { On, Off }\nenum Inner { In(Flag) }\nenum Outer { Out(Inner) }\n";
    let nested = analyze_and_lower_diags(&format!(
        "{deep}fn main() -> Int {{ let v = Out(In(On))\n  match v {{ Out(In(On)) => 1 }} }}"
    ));
    assert!(
        nested.iter().any(|d| d.code().number() == 120),
        "the gap is two payloads deep: {nested:?}"
    );
}

/// **HIR-06's second half as the rule.** An arm is unreachable when it matches
/// no value the arms above it leave — which is a coverage question, not the
/// syntactic "is there a `_` above me" the old scan asked.
#[test]
fn an_arm_is_unreachable_exactly_when_it_adds_no_coverage() {
    let y121 = |src: &str| {
        analyze_and_lower_diags(src)
            .iter()
            .filter(|d| d.code().number() == 121)
            .count()
    };

    // A repeated constructor: the old scan saw no catch-all and said nothing.
    assert_eq!(
        y121(
            "enum E { A, B }\nfn main() -> Int { let v = A\n  match v { A => 1, A => 2, B => 3 } }"
        ),
        1,
        "the second `A` is dead, and only it"
    );

    // The case the old scan *did* catch still works.
    assert_eq!(
        y121("enum E { A, B }\nfn main() -> Int { let v = A\n  match v { _ => 1, A => 2 } }"),
        1,
        "an arm after a catch-all"
    );

    // A payload an earlier arm already covered — invisible to a top-level scan,
    // because both arms name the same single variant.
    assert_eq!(
        y121(
            "enum Flag { On, Off }\nenum Wrapped { Wrap(Flag) }\n\
             fn main() -> Int { let v = Wrap(On)\n  match v { Wrap(_) => 1, Wrap(On) => 2 } }"
        ),
        1,
        "`Wrap(On)` is inside `Wrap(_)`"
    );

    // …and the half a coverage check gets wrong in the other direction: arms
    // that each add something are all reachable.
    assert_eq!(
        y121(
            "enum Flag { On, Off }\nenum Wrapped { Wrap(Flag) }\n\
             fn main() -> Int { let v = Wrap(On)\n  match v { Wrap(On) => 1, Wrap(Off) => 2 } }"
        ),
        0,
        "two distinct payload constructors are two live arms"
    );

    // An exhaustive pair of `Bool` literals leaves nothing for a third arm,
    // with no `_` anywhere in sight.
    assert_eq!(
        y121("fn main() -> Int { match true { true => 1, false => 2, true => 3 } }"),
        1,
        "`true` and `false` are all of Bool"
    );
}

/// The witness a `Y120` names is the **shape** that is missing, so a nested gap
/// says which one. Naming only the outer constructor — the most a top-level
/// check could say — is the message that sends you looking in the wrong place.
#[test]
fn a_missing_case_is_named_by_the_value_that_is_missing() {
    let diags = analyze_and_lower_diags(
        "enum Flag { On, Off }\nenum Wrapped { Wrap(Flag) }\n\
         fn main() -> Int { let v = Wrap(On)\n  match v { Wrap(On) => 1 } }",
    );
    let y120 = diags
        .iter()
        .find(|d| d.code().number() == 120)
        .expect("a missing `Wrap(Off)`");
    assert!(
        y120.message().contains("Wrap(Off)"),
        "the witness is the whole shape: {}",
        y120.message()
    );

    // A type with no signature to enumerate asks for an arm instead of naming
    // a value, because there is no finite set of values to name.
    let open = analyze_and_lower_diags("fn main() -> Int { match 1 { 1 => 1, 2 => 2 } }");
    let y120 = open
        .iter()
        .find(|d| d.code().number() == 120)
        .expect("an Int match needs a catch-all");
    assert!(
        y120.message().contains("catch-all"),
        "an open signature names no value: {}",
        y120.message()
    );
}

/// A bare constructor name and that constructor at every-wildcard are the same
/// test, because lowering pads a variant pattern to its payload arity (HIR-06).
/// The matrix pairs each column with a type; a row narrower than the payload
/// would pair them off by one.
#[test]
fn a_bare_constructor_name_is_that_constructor_at_any_payload() {
    for arm in ["Some", "Some(_)", "Some(n)"] {
        let src = format!(
            "fn main() -> Int {{ let o = Some(1)\n  match o {{ {arm} => 1, None => 0 }} }}"
        );
        assert!(
            is_clean_with_lower(&src),
            "`{arm}` plus `None` is all of Option[Int]"
        );
    }
    // …and without the `None` arm none of them is.
    for arm in ["Some", "Some(_)", "Some(n)"] {
        let src = format!("fn main() -> Int {{ let o = Some(1)\n  match o {{ {arm} => 1 }} }}");
        let diags = analyze_and_lower_diags(&src);
        assert!(
            diags.iter().any(|d| d.code().number() == 120),
            "`{arm}` alone leaves `None`: {diags:?}"
        );
    }
}
