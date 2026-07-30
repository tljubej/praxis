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

/// Whether `text` reports the given input-parser diagnostic. Stronger than
/// [`has_input_error`]: it pins *which* rule fired, so a test cannot pass on
/// some unrelated `I0xx` the change happened to provoke.
fn reports_input_code(text: &str, code: praxis_source::DiagCode) -> bool {
    let wanted = code.code();
    analyze(text).diagnostics.iter().any(|d| d.code() == wanted)
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

/// A declaration cycle has no fixpoint in a type system without recursive types,
/// so the pass must not loop looking for one. The point of the gate is that
/// `analyze` returns and the rest of the file is still inferred.
///
/// **Amended by REP-14 (ADR-063).** The comment used to end "it registers what is
/// left in source order, exactly as an unresolvable annotation has always been
/// handled", which stated the defect: registering a cycle member with a fresh
/// variable and saying nothing left one unchecked member per recursive
/// declaration. It is reported now, and the assertion below says so — without
/// that line the test passed equally well against the silence.
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
    // …and each of the three cycle members is reported, exactly once.
    assert_eq!(
        analysis
            .diagnostics
            .iter()
            .filter(|d| d.kind() == praxis_source::DiagCode::RecursiveTypeDeclaration)
            .count(),
        3,
        "one N006 per cycle member: {:?}",
        analysis.diagnostics
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
fn parse_requires_text_input() {
    let src = "fn main() -> Int { parse(1, int) }";
    assert!(
        has_type_error(src),
        "the first argument of parse(text, parser) must be Text"
    );
}

#[test]
fn unary_minus_accepts_float_typed_variables() {
    let src = "fn negate(x: Float) -> Float { -x }";
    assert!(
        !has_type_error(src),
        "Float negation must depend on the operand type, not only literal syntax"
    );
}

#[test]
fn float_remainder_is_rejected() {
    let src = "fn bad() -> Float { 5.0 % 2.0 }";
    assert!(
        has_type_error(src),
        "the language defines no `%` operation for Float"
    );
}

#[test]
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
fn prelude_assert_requires_bool() {
    let src = "fn main() -> Unit { assert(1) }";
    assert!(
        has_type_error(src),
        "prelude calls need real schemes instead of unconstrained fresh types"
    );
}

/// TY-33's first unit as a rule, not one rejection: each of the three
/// output/control names has the type §8.1/§9.1 gives it, and the type is what
/// makes each usable. `assert` refuses a non-`Bool`; `dbg` is the identity, so
/// it can wrap any subexpression without changing what the program computes;
/// `panic` is `Never`, so it satisfies any declared result.
#[test]
fn each_control_builtin_has_the_type_its_contract_needs() {
    // `assert` takes a Bool and gives back Unit.
    assert!(!has_type_error("fn main() -> Unit { assert(true) }"));
    assert!(has_type_error("fn main() -> Unit { assert(1) }"));
    assert!(has_type_error("fn main() -> Unit { assert(\"yes\") }"));
    // …and it is Unit, not the condition: an Int result does not match.
    assert!(has_type_error("fn main() -> Int { assert(true) }"));

    // `dbg` returns exactly what it was given, at each of two element types.
    assert!(!has_type_error("fn main() -> Int { dbg(1) }"));
    assert!(!has_type_error("fn main() -> Text { dbg(\"x\") }"));
    assert!(has_type_error("fn main() -> Text { dbg(1) }"));
    // The identity holds inside an expression, which is the point of `dbg`.
    assert!(!has_type_error("fn main() -> Int { dbg(1) + 2 }"));

    // `panic` diverges, so it matches any declared result — and it accepts any
    // value, as `out` does.
    assert!(!has_type_error("fn main() -> Int { panic(\"stop\") }"));
    assert!(!has_type_error("fn main() -> Unit { panic(1) }"));
    assert!(!has_type_error(
        "fn f(c: Bool) -> Int { if c { 1 } else { panic(\"stop\") } }"
    ));
}

/// The half a type test cannot see: each of the three lowers to a runtime call
/// rather than to a user function that does not exist. `panic` **typechecked**
/// before this stage and then failed the compile with "unresolved user function
/// `panic`" — a clean program that could not run (TY-33).
#[test]
fn each_control_builtin_reaches_the_backend() {
    assert!(is_clean_with_lower("fn main() -> Unit { panic(\"stop\") }"));
    assert!(is_clean_with_lower("fn main() -> Unit { assert(true) }"));
    assert!(is_clean_with_lower("fn main() -> Int { dbg(7) }"));
}

/// TY-33's second unit as the rule: each of §16.1's seven numeric helpers is
/// `(Int, …) -> Int` (ADR-058), which means each of the three things a phantom
/// name could not do — reject a wrong operand type, reject a wrong operand
/// count, and be used where an `Int` is required.
///
/// Before this every one of them got a fresh type variable, so `abs("x") + 1`
/// was accepted and `min(1)` was accepted, and then the program failed the
/// compile.
#[test]
fn each_numeric_helper_has_the_int_type_its_contract_needs() {
    // The result is `Int`, in each arity.
    assert!(!has_type_error("fn main() -> Int { abs(-5) }"));
    assert!(!has_type_error("fn main() -> Int { sign(-9) }"));
    assert!(!has_type_error("fn main() -> Int { min(3, 7) }"));
    assert!(!has_type_error("fn main() -> Int { max(3, 7) }"));
    assert!(!has_type_error("fn main() -> Int { clamp(11, 0, 10) }"));
    assert!(!has_type_error("fn main() -> Int { gcd(12, 18) }"));
    assert!(!has_type_error("fn main() -> Int { lcm(4, 6) }"));
    // …and it is `Int`, not "whatever the caller wanted".
    assert!(has_type_error("fn main() -> Text { abs(-5) }"));
    assert!(has_type_error("fn main() -> Bool { min(3, 7) }"));
    assert!(has_type_error("fn main() -> Float { gcd(12, 18) }"));

    // The operands are `Int`. A `Text` or a `Float` in any position is refused —
    // the whole difference between a scheme and a fresh variable.
    assert!(has_type_error("fn main() -> Int { abs(\"x\") }"));
    assert!(has_type_error("fn main() -> Int { sign(1.5) }"));
    assert!(has_type_error("fn main() -> Int { min(3, \"7\") }"));
    assert!(has_type_error("fn main() -> Int { max(true, 7) }"));
    assert!(has_type_error("fn main() -> Int { clamp(11, 0, 1.0) }"));
    assert!(has_type_error("fn main() -> Int { lcm(4.0, 6.0) }"));

    // The arity is the wrapper's, and inference is what enforces it: `min` takes
    // two operands and `clamp` takes three, so neither a short nor a long call
    // typechecks.
    assert!(has_type_error("fn main() -> Int { min(3) }"));
    assert!(has_type_error("fn main() -> Int { min(3, 4, 5) }"));
    assert!(has_type_error("fn main() -> Int { abs(1, 2) }"));
    assert!(has_type_error("fn main() -> Int { clamp(11, 0) }"));

    // The `Float` counterparts are methods (§4.12), not these names — which is
    // why the free functions can be monomorphic on `Int` at all.
    assert!(!has_type_error("fn main() -> Float { (0.0 - 1.5).abs() }"));
    assert!(!has_type_error("fn main() -> Float { 1.5.min(2.5) }"));
}

/// A helper composes: it is an ordinary `Int`-valued expression, so it nests
/// inside arithmetic, inside another helper, and inside a call whose parameter
/// is annotated. This is the shape §3.3's representative program writes
/// (`max(abs(dx), abs(dy))`), and a fresh variable would have accepted it
/// without checking anything.
#[test]
fn a_numeric_helper_composes_like_any_int_expression() {
    assert!(!has_type_error(
        "fn manhattan(ax: Int, ay: Int, bx: Int, by: Int) -> Int {\n\
         \x20   abs(ax - bx) + abs(ay - by)\n\
         }"
    ));
    assert!(!has_type_error(
        "fn spread(dx: Int, dy: Int) -> Int { max(abs(dx), abs(dy)) }"
    ));
    assert!(!has_type_error(
        "fn take(n: Int) -> Int { n }\n\
         fn main() -> Int { take(clamp(gcd(12, 18), 0, 10)) }"
    ));
    // …and the nesting is checked, not waved through: `sign` yields an `Int`, so
    // handing it to a `Text` parameter is still a mismatch.
    assert!(has_type_error(
        "fn take(t: Text) -> Text { t }\n\
         fn main() -> Text { take(sign(-1)) }"
    ));
}

/// TY-34's type rule (ADR-059): a range's bounds are `Int` and the range itself
/// is the nullary `Range` collection, whose element type is `Int`.
///
/// `Int` bounds **only**: `iter_item` already says a range yields `Int`, and
/// admitting `Float` bounds would make that a lie with no step to fix it (D6).
#[test]
fn a_ranges_bounds_are_ints_and_a_range_is_a_collection_of_them() {
    // Both forms are `Range`, and a `Range` annotation accepts one.
    assert!(!has_type_error("fn f() -> Range { 0..5 }"));
    assert!(!has_type_error("fn f() -> Range { 0..=5 }"));
    assert!(!has_type_error("fn f(n: Int) -> Range { 0..n - 1 }"));
    // …and it is a `Range`, not an `Int` and not a `Vec[Int]`.
    assert!(has_type_error("fn f() -> Int { 0..5 }"));
    assert!(has_type_error("fn f() -> Vec[Int] { 0..5 }"));

    // Each bound is an `Int`. Every non-`Int` bound is refused, in either
    // position — the half that distinguishes a real rule from a fresh variable.
    assert!(has_type_error("fn f() -> Range { \"a\"..5 }"));
    assert!(has_type_error("fn f() -> Range { 0..\"z\" }"));
    assert!(has_type_error("fn f() -> Range { 0.0..1.0 }"));
    assert!(has_type_error("fn f() -> Range { true..false }"));
    assert!(has_type_error("fn f(v: Vec[Int]) -> Range { 0..v }"));

    // A range yields `Int`, so the loop variable is one.
    assert!(!has_type_error(
        "fn f() -> Unit { for i in 0..3 { out(i + 1) } }"
    ));
    assert!(has_type_error(
        "fn f() -> Unit { for i in 0..3 { out(i + \"x\") } }"
    ));
    // …and a range is iterable at all, which is what `Range`'s `iter_item` arm
    // has claimed since before the syntax existed. Annotated, because an
    // *unannotated* iterated parameter unifies with its own element type — see
    // `iter_item`'s optimism at `infer_for`, which affects every iterable
    // equally and is not TY-34's.
    assert!(!has_type_error(
        "fn total(r: Range) -> Int { var t = 0\n for i in r { t = t + i }\n t }\n\
         fn main() -> Int { total(0..4) }"
    ));
}

/// A range is a first-class **value** (D6), not only a `for`-header form: it
/// binds to a name, passes as an argument, comes back as a result, and — because
/// its bounds cannot change once it is built — is a legal `Map` key (ADR-057 D4).
#[test]
fn a_range_is_an_ordinary_value() {
    assert!(!has_type_error(
        "fn f() -> Unit { let r = 0..5\n for i in r { out(i) } }"
    ));
    assert!(!has_type_error(
        "fn widen(r: Range) -> Range { r }\n\
         fn main() -> Range { widen(1..2) }"
    ));
    // A `Range` is hashable *and* immutable, so it is a key — the distinction
    // TY-32/D4 turned on.
    assert!(!has_type_error(
        "fn main() -> Unit { let m = Map()\n m.insert(0..3, 1) }"
    ));
    assert!(!has_type_error(
        "fn main() -> Unit { let s = Set()\n s.insert(0..3) }"
    ));
    // …and it is equatable, so two ranges compare.
    assert!(!has_type_error("fn f() -> Bool { (0..3) == (0..3) }"));
    // It is not orderable: only the five scalars with a `compare` are (ADR-045).
    assert!(has_type_error("fn f() -> Bool { (0..3) < (1..4) }"));
}

/// A **bare** nullary collection name is the type it names. `Range` and `BitSet`
/// are the only two ctors with no type arguments, so they are the only names that
/// appear in type position without brackets — and that path never reached
/// `collection_from_name`, so the annotation resolved to nothing and the binding
/// silently became a fresh variable.
///
/// The symptom was a function whose annotated parameter took any type at all and
/// then unified with whatever its body did: `fn total(r: Range) { for i in r { … } }`
/// reported "values of type `Int` cannot be iterated". `BitSet` had it too.
#[test]
fn a_bare_nullary_collection_name_is_the_type_it_names() {
    // The annotation is enforced, in both nullary ctors, in every position.
    assert!(has_type_error("fn f(r: Range) -> Int { r }"));
    assert!(has_type_error("fn f(b: BitSet) -> Int { b }"));
    assert!(has_type_error("fn f() -> Range { 1 }"));
    assert!(!has_type_error("fn f(r: Range) -> Range { r }"));
    assert!(!has_type_error("fn f() -> Range { 0..1 }"));
    assert!(!has_type_error("fn f() -> BitSet { BitSet() }"));
    assert!(has_type_error("fn f(r: Range) -> Unit { out(r + 1) }"));
    // Nested inside another collection, and as a local's annotation.
    assert!(!has_type_error("fn f(v: Vec[Range]) -> Vec[Range] { v }"));
    assert!(has_type_error(
        "fn f() -> Unit { let r: Range = 5\n out(r) }"
    ));

    // …and a ctor that *does* take arguments, written bare, is the `Y007` it has
    // always been for a wrong count — not a silent variable.
    let codes: Vec<u32> = analyze("fn f(v: Vec) -> Int { 1 }")
        .diagnostics
        .iter()
        .filter(|d| d.code().category() == DiagnosticCategory::Type)
        .map(|d| d.code().number())
        .collect();
    assert!(
        codes.contains(&7),
        "a bracket-less `Vec` must report its missing argument as Y007, got {codes:?}"
    );
}

/// The half a type test cannot see: each of the seven lowers to its own runtime
/// call. Before this they reached the backend as `CallTarget::User("abs")` and
/// the compile failed with "unresolved user function `abs`" — `panic`'s symptom,
/// on seven more names (TY-33).
#[test]
fn each_numeric_helper_reaches_the_backend() {
    for src in [
        "fn main() -> Int { abs(-5) }",
        "fn main() -> Int { sign(-9) }",
        "fn main() -> Int { min(3, 7) }",
        "fn main() -> Int { max(3, 7) }",
        "fn main() -> Int { clamp(11, 0, 10) }",
        "fn main() -> Int { gcd(12, 18) }",
        "fn main() -> Int { lcm(4, 6) }",
    ] {
        assert!(is_clean_with_lower(src), "did not lower: {src}");
    }
}

// --- §6.5's graph helpers (TY-33 unit 3, ADR-060) ---------------------------

/// A neighbour function over `Int` states, for the tests below. Written once
/// because every graph helper takes one and the interesting part is never the
/// graph.
const STEPS: &str = "fn steps(n: Int) -> Vec[Int] { Vec() }\n";

/// Each of the six has the signature its contract needs: one state type, a
/// neighbour function of it, the result the helper's name promises — and the
/// arity, which a fresh type variable could not enforce at all.
///
/// Before this every one of them got a fresh variable, so `bfs(1)` was accepted,
/// `bfs("a", |n| steps(n))` was accepted, and the program then failed the
/// compile with "unresolved user function `bfs`" (TY-33).
#[test]
fn each_graph_helper_has_the_signature_its_contract_needs() {
    // The results are what §6.5's names promise: an order, a set, a cost table,
    // an optional distance.
    assert!(!has_type_error(&format!(
        "{STEPS}fn main() -> Vec[Int] {{ bfs(1, |n| steps(n)) }}"
    )));
    assert!(!has_type_error(&format!(
        "{STEPS}fn main() -> Vec[Int] {{ dfs(1, |n| steps(n)) }}"
    )));
    assert!(!has_type_error(&format!(
        "{STEPS}fn main() -> Set[Int] {{ flood_fill(1, |n| steps(n)) }}"
    )));
    assert!(!has_type_error(&format!(
        "{STEPS}fn main() -> Map[Int, Int] {{ dijkstra(1, |n| steps(n), |a, b| 1) }}"
    )));
    assert!(!has_type_error(&format!(
        "{STEPS}fn main() -> Option[Int] {{ bfs_distance(1, |n| steps(n), |n| n == 9) }}"
    )));
    assert!(!has_type_error(&format!(
        "{STEPS}fn main() -> Option[Int] \
         {{ a_star(1, |n| steps(n), |a, b| 1, |n| 0, |n| n == 9) }}"
    )));

    // …and each is *that* result, not "whatever the caller wanted".
    assert!(has_type_error(&format!(
        "{STEPS}fn main() -> Set[Int] {{ bfs(1, |n| steps(n)) }}"
    )));
    assert!(has_type_error(&format!(
        "{STEPS}fn main() -> Vec[Int] {{ flood_fill(1, |n| steps(n)) }}"
    )));
    assert!(has_type_error(&format!(
        "{STEPS}fn main() -> Int {{ bfs_distance(1, |n| steps(n), |n| n == 9) }}"
    )));
    assert!(has_type_error(&format!(
        "{STEPS}fn main() -> Set[Int] {{ dijkstra(1, |n| steps(n), |a, b| 1) }}"
    )));

    // The arity is the wrapper's, and inference enforces it: neither a short
    // call nor a long one typechecks.
    assert!(has_type_error(&format!("{STEPS}fn main() {{ bfs(1) }}")));
    assert!(has_type_error(&format!(
        "{STEPS}fn main() {{ bfs(1, |n| steps(n), |n| true) }}"
    )));
    assert!(has_type_error(&format!(
        "{STEPS}fn main() {{ bfs_distance(1, |n| steps(n)) }}"
    )));
    assert!(has_type_error(&format!(
        "{STEPS}fn main() {{ a_star(1, |n| steps(n), |a, b| 1, |n| 0) }}"
    )));
}

/// Each closure parameter has the shape its position declares, and the state
/// type is **one** variable shared by all of them. That is the whole difference
/// between a signature and a fresh variable: the neighbour function's argument,
/// its element type, the weight's two operands, the goal's argument and the
/// start state are the same `T`, so disagreeing about it anywhere is an error.
#[test]
fn a_graph_helpers_closures_agree_with_each_other_about_the_state() {
    // The neighbour function returns a `Vec` of the *state* type, not of
    // anything else.
    assert!(has_type_error(
        "fn steps(n: Int) -> Vec[Text] { Vec() }\n\
         fn main() { bfs(1, |n| steps(n)) }"
    ));
    // …and it takes the state type, so a start of another type is refused.
    assert!(has_type_error(&format!(
        "{STEPS}fn main() {{ bfs(\"a\", |n| steps(n)) }}"
    )));
    // The goal predicate answers `Bool`.
    assert!(has_type_error(&format!(
        "{STEPS}fn main() {{ bfs_distance(1, |n| steps(n), |n| n + 1) }}"
    )));
    // The weight and the heuristic answer `Int`.
    assert!(has_type_error(&format!(
        "{STEPS}fn main() {{ dijkstra(1, |n| steps(n), |a, b| true) }}"
    )));
    assert!(has_type_error(&format!(
        "{STEPS}fn main() {{ a_star(1, |n| steps(n), |a, b| 1, |n| \"far\", |n| n == 9) }}"
    )));
    // A weight function takes *two* states — the edge, not the endpoint.
    assert!(has_type_error(&format!(
        "{STEPS}fn main() {{ dijkstra(1, |n| steps(n), |a| 1) }}"
    )));
    // And a neighbour function must be a function at all.
    assert!(has_type_error(&format!("{STEPS}fn main() {{ bfs(1, 2) }}")));
}

/// A graph helper's state has to be one the walk can *remember* — a `Set`
/// element and a `Map` key — so D4's rule reaches it: a mutable collection is
/// refused, and it is refused **at the call**, which is the only place that can
/// name the type.
#[test]
fn a_graph_helpers_state_must_be_one_the_walk_can_remember() {
    let codes: Vec<u32> = analyze(
        "fn steps(v: Vec[Int]) -> Vec[Vec[Int]] { Vec() }\n\
         fn main() { var start = Vec()\n  start.push(1)\n  bfs(start, |v| steps(v)) }",
    )
    .diagnostics
    .iter()
    .filter(|d| d.code().category() == DiagnosticCategory::Type)
    .map(|d| d.code().number())
    .collect();
    assert!(
        codes.contains(&14),
        "a mutable state type must be the same Y014 a Map key is, got {codes:?}"
    );

    // Every mutable collection is refused, in every helper.
    for state in ["Vec[Int]", "Set[Int]", "Map[Int, Int]", "Deque[Int]"] {
        let src = format!(
            "fn steps(v: {state}) -> Vec[{state}] {{ Vec() }}\n\
             fn walk(start: {state}) {{ bfs(start, |v| steps(v)) }}"
        );
        assert!(has_type_error(&src), "a {state} state was accepted");
    }
    // …and an immutable one is not. A record of scalars is what a grid position
    // is written as, and it is a legal state.
    assert!(!has_type_error(
        "struct P { x: Int, y: Int }\n\
         fn steps(p: P) -> Vec[P] { Vec() }\n\
         fn main() { dfs(P { x: 0, y: 0 }, |p| steps(p)) }"
    ));
    assert!(!has_type_error(
        "fn steps(t: Text) -> Vec[Text] { Vec() }\n\
         fn main() { flood_fill(\"a\", |t| steps(t)) }"
    ));
}

/// The state requirement rides F10's channel rather than being decided at the
/// call: a helper called on an *unannotated* parameter defers the requirement,
/// the enclosing function's scheme claims it, and the caller that pins the type
/// is where it is answered.
///
/// This is the hardest thing the channel does, and it is what D5 meant by "a
/// graph helper's signature is where the channel gets its hardest test".
#[test]
fn a_graph_state_requirement_reaches_through_a_generic_function() {
    // `walk`'s parameter is a variable when `bfs` is checked, so the
    // requirement cannot be decided there. It is decided at `main`'s call.
    assert!(has_type_error(
        "fn walk(start, step) { bfs(start, step) }\n\
         fn steps(v: Vec[Int]) -> Vec[Vec[Int]] { Vec() }\n\
         fn main() { var s = Vec()\n  s.push(1)\n  walk(s, |v| steps(v)) }"
    ));
    // …and the same generic function is fine at a state that can be remembered,
    // which is what makes the rejection a requirement and not a ban on
    // deferring.
    assert!(!has_type_error(&format!(
        "fn walk(start, step) {{ bfs(start, step) }}\n\
         {STEPS}fn main() {{ walk(1, |n| steps(n)) }}"
    )));
}

/// The half a type test cannot see: each of the six lowers to its own runtime
/// call. Before this they reached the backend as `CallTarget::User("bfs")` and
/// the compile failed with "unresolved user function `bfs`" — `panic`'s
/// symptom, on the last six names that had it (TY-33).
#[test]
fn each_graph_helper_reaches_the_backend() {
    for src in [
        "fn main() -> Vec[Int] { bfs(1, |n| steps(n)) }",
        "fn main() -> Vec[Int] { dfs(1, |n| steps(n)) }",
        "fn main() -> Set[Int] { flood_fill(1, |n| steps(n)) }",
        "fn main() -> Map[Int, Int] { dijkstra(1, |n| steps(n), |a, b| 1) }",
        "fn main() -> Option[Int] { bfs_distance(1, |n| steps(n), |n| n == 9) }",
        "fn main() -> Option[Int] { a_star(1, |n| steps(n), |a, b| 1, |n| 0, |n| n == 9) }",
    ] {
        let program = format!("{STEPS}{src}");
        assert!(is_clean_with_lower(&program), "did not lower: {src}");
    }
}

// --- capability constraints must survive polymorphism -----------------------

#[test]
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
fn iterable_constraint_rejects_int_instantiation() {
    let src = "fn drain(values) -> Unit { for value in values { out(value) } }\n\
               fn main() -> Unit { drain(1) }";
    assert!(
        has_type_error_with_lower(src),
        "a generic Iterable constraint cannot disappear after generalization"
    );
}

#[test]
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

/// TY-30 as the *rule*, not one accepted program: a method called on a receiver
/// nothing has typed yet is a **requirement**, and the use site answers it.
///
/// The exit test only asks that §5.2's program is accepted. What it cannot see is
/// the answer — §5.2 states it exactly, `total: Vec[Int] -> Int` — and that the
/// resolution runs in both directions: the entry's *result* pins the call, and
/// the entry's *parameters* pin the arguments the deferred call passed.
#[test]
fn a_method_on_an_unannotated_receiver_is_resolved_by_the_use_site() {
    // §5.2's own answer, written down.
    let sum = "fn total(values) { values.sum() }\n\
               fn main() -> Int { let values = Vec(); values.push(1); total(values) }";
    assert_eq!(
        scheme_of(sum, "total").as_deref(),
        Some("(Vec[Int]) -> Int")
    );

    // Not a special case of `sum`: any catalog entry, on any receiver shape the
    // catalog models — including a scalar one.
    let len = "fn size(v) { v.len() }\n\
               fn main() -> Int { let v = Vec(); v.push(1); size(v) }";
    assert_eq!(scheme_of(len, "size").as_deref(), Some("(Vec[Int]) -> Int"));
    let text = "fn size(t) { t.len() }\nfn main() -> Int { size(\"abc\") }";
    assert_eq!(scheme_of(text, "size").as_deref(), Some("(Text) -> Int"));

    // The deferred entry pins the *arguments* too. `x` has no annotation and no
    // use of its own; `push`'s parameter is what says it is an `Int`.
    let arg = "fn add(v, x) { v.push(x) }\n\
               fn main() -> Unit { let v = Vec(); v.push(1); add(v, 2) }";
    assert_eq!(
        scheme_of(arg, "add").as_deref(),
        Some("(Vec[Int], Int) -> Unit")
    );
    assert!(!has_type_error_with_lower(arg));

    // …so an argument that disagrees with it is reported, which is the half a
    // "the program is accepted" test cannot reach.
    let bad = "fn add(v, x) { v.push(x) }\n\
               fn main() -> Unit { let v = Vec(); v.push(1); add(v, \"s\") }";
    assert!(has_type_error_with_lower(bad));

    // And the result is checked against the annotation the deferred function
    // wrote, rather than being whatever the annotation says.
    let wrong = "fn total(values) -> Text { values.sum() }\n\
                 fn main() -> Text { let v = Vec(); v.push(1); total(v) }";
    assert!(has_type_error_with_lower(wrong));
}

/// The receiver a method was called on is **pinned**, not quantified (TY-30).
///
/// This is the contract, and it is why `pin_to_level` exists. There is one
/// lowered body per source function — monomorphization clones a tree lowering
/// already resolved — so one method call site carries one catalog entry and one
/// receiver type. Two receivers at one call site is not a shape the compiler can
/// lower, so it is a disagreement about `total`'s signature instead.
///
/// The second half is what keeps the rule from being "nothing generalizes": a
/// parameter no method was called on is quantified exactly as before.
#[test]
fn a_receiver_a_method_was_called_on_is_not_quantified() {
    let two = "fn total(values) { values.sum() }\n\
               fn main() -> Int {\n\
                 let a = Vec()\n\
                 a.push(1)\n\
                 let b = Vec()\n\
                 b.push(1.0)\n\
                 total(b)\n\
                 total(a)\n\
               }";
    assert!(
        has_type_error_with_lower(two),
        "one method call site cannot carry two receiver types"
    );

    // A parameter with no method call on it still generalizes — `id` is used at
    // two types in one program and neither pins the other.
    let generic = "fn id(x) { x }\n\
                   fn main() -> Int { let t = id(\"s\"); id(1) }";
    assert_eq!(
        scheme_of(generic, "id").as_deref(),
        Some("forall T. (T) -> T")
    );
    assert!(!has_type_error_with_lower(generic));
}

/// A requirement the receiver's *type* carries is checked once the receiver
/// resolves, and a deferred method call is no exception (TY-30 × TY-32/D4).
///
/// `store` never says what `m` is. The `insert` inside it is what makes it a
/// `Map`, and the key rule then applies to the key the call site chose — so the
/// mutable-collection-as-key refusal reaches through a function whose signature
/// was inferred entirely from a deferred method.
#[test]
fn a_deferred_method_still_carries_its_receivers_own_requirements() {
    let src = "fn store(m, k) -> Unit { m.insert(k, 1) }\n\
               fn main() -> Unit {\n\
                 let m = Map()\n\
                 let key = Vec()\n\
                 key.push(1)\n\
                 store(m, key)\n\
               }";
    let diags = analyze_and_lower_diags(src);
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y014"),
        "a Vec key must be refused through a deferred insert, got {:?}",
        diags
            .iter()
            .map(|d| d.code().to_string())
            .collect::<Vec<_>>()
    );

    // The same shape with an immutable key is accepted, so the refusal is the
    // key rule and not "a deferred insert cannot resolve".
    let ok = "fn store(m, k) -> Unit { m.insert(k, 1) }\n\
              fn main() -> Unit { let m = Map(); store(m, \"k\") }";
    assert!(!has_type_error_with_lower(ok));
}

/// A method the receiver does not have is reported **once**, by lowering.
///
/// TY-30 adds a second place that knows about the call, and a capability channel
/// that reports as well as resolves would say the same thing twice. It resolves
/// only: `Y110` has one emitter, it has the method-name span, and both shapes
/// that reach it — a receiver the program pinned, and a receiver nothing ever
/// pinned — produce exactly one diagnostic.
#[test]
fn a_method_the_receiver_does_not_have_is_reported_once() {
    for src in [
        // Pinned by the call site, and `Int` has no `nope`.
        "fn f(x) { x.nope() }\nfn main() -> Unit { f(1) }",
        // Never pinned at all: nothing resolves, and lowering still reports.
        "fn f(x) { x.nope() }\nfn main() -> Unit { }",
    ] {
        let codes: Vec<String> = analyze_and_lower_diags(src)
            .iter()
            .map(|d| d.code().to_string())
            .collect();
        assert_eq!(
            codes,
            vec!["Y110".to_string()],
            "one report per missing method, got {codes:?} for {src:?}"
        );
    }
}

#[test]
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

/// TY-31 as the *rule*: the four aggregating sinks are **Int** operations, and
/// the catalog says so now.
///
/// The exit test only asks about `Bool` on `sum`. `Float` is the case that
/// mattered more and that no test asked for: `Vec[Float].sum()` typechecked and
/// returned `9222246136947933184` — the float's bits, added as an integer. That
/// is why the bound is `Int` and not `Numeric`, which is what the finding's
/// wording ("numeric element types") would have given: `CapKind::Numeric`
/// includes `Float`, and the lowering does not.
#[test]
fn the_int_sinks_require_int_elements() {
    for sink in ["sum", "product", "min", "max"] {
        for (elem, push) in [("Bool", "true"), ("Float", "1.5"), ("Text", "\"a\"")] {
            let src = format!("fn main() -> Int {{ let v = Vec(); v.push({push}); v.{sink}() }}");
            assert!(
                has_type_error_with_lower(&src),
                "`{sink}` on a Vec[{elem}] must be rejected"
            );
        }
        // …and Int is accepted, so the bound is the element type and not the
        // sink.
        let ok = format!("fn main() -> Int {{ let v = Vec(); v.push(1); v.{sink}() }}");
        assert!(!has_type_error_with_lower(&ok), "`{sink}` on Vec[Int]");
    }
}

/// A bound **pins** an element nothing has named yet — it does not merely permit
/// one (TY-31).
///
/// That is why [`Bound::Is`] is discharged by unification rather than by the
/// constraint channel. A pipeline's intermediate element type is a fresh variable
/// at the moment the sink is looked up, and a deferred yes/no would answer
/// "optimistically capable" and let the closure return whatever it liked.
#[test]
fn a_sinks_element_bound_pins_an_unresolved_pipeline_stage() {
    // `map`'s result element is a variable when `.sum()` is resolved; the bound
    // pins it, so the closure's own body is what fails.
    assert!(has_type_error_with_lower(
        "fn main() -> Int { let v = Vec(); v.push(1); v.map(|x| \"s\").sum() }"
    ));
    // The same chain with an Int-returning closure is clean, so the rejection is
    // the element type and not the fusion.
    assert!(!has_type_error_with_lower(
        "fn main() -> Int { let v = Vec(); v.push(1); v.map(|x| x * 2).sum() }"
    ));
    // And through TY-30's deferred resolution, where the receiver itself was a
    // variable when the method was written.
    assert!(has_type_error_with_lower(
        "fn total(values) { values.sum() }\n\
         fn main() -> Int { let v = Vec(); v.push(true); total(v) }"
    ));
}

/// `enumerate` and `zip` say what they build (TY-31).
///
/// Both rows declared `result: Vec[T]` — the *receiver's* element type — so
/// `v.enumerate()` on a `Vec[Int]` came out `Vec[Int]` and the tuple the fused
/// loop really builds was invisible. `zip` was wrong twice over: the same row
/// also required the other sequence to have the receiver's element type.
///
/// Found by S15 while it was deciding whether `alloc_empty_vec` could read its
/// element type from the chain's result (it could not, because of this), and
/// recorded there as a finding the register does not have.
#[test]
fn enumerate_and_zip_report_the_pairs_they_build() {
    let e = "fn main() -> Unit { let v = Vec(); v.push(1); let pairs = v.enumerate(); out(pairs) }";
    assert_eq!(scheme_of(e, "pairs").as_deref(), Some("Vec[(Int, Int)]"));

    // A non-Int element, so "the index is an Int" is visible as a *separate*
    // fact from the element type.
    let text =
        "fn main() -> Unit { let v = Vec(); v.push(\"a\"); let pairs = v.enumerate(); out(pairs) }";
    assert_eq!(
        scheme_of(text, "pairs").as_deref(),
        Some("Vec[(Int, Text)]")
    );

    // `zip` pairs two *different* element types — which the old row made
    // impossible to write.
    let z = "fn main() -> Unit {\n\
               let a = Vec()\n\
               a.push(1)\n\
               let b = Vec()\n\
               b.push(\"s\")\n\
               let pairs = a.zip(b)\n\
               out(pairs)\n\
             }";
    assert_eq!(scheme_of(z, "pairs").as_deref(), Some("Vec[(Int, Text)]"));
    assert!(!has_type_error_with_lower(z));
}

/// A compound assignment's numeric requirement survives generalization (TY-31,
/// `Y015`).
///
/// S13 left this reported only for a target whose type was already known, and
/// said why: `a += 1` inside a generic function says nothing about `a` yet, and
/// pinning it to `Int` would silently narrow every unannotated numeric binding.
/// Deferring is the third option — the requirement rides on the scheme, and the
/// call site is where it is answered.
///
/// The two codes are the two situations. `Y010` is reported *at the operation*
/// and can name it; `Y015` is reported at the use that pinned the target, which
/// is somewhere else entirely.
#[test]
fn a_compound_assignments_numeric_requirement_survives_generalization() {
    // A `var` whose type is still a variable when the `+=` is checked: both
    // sides are the function's own parameter, so nothing has pinned it.
    let bad = "fn twice(a) -> Unit { var acc = a\n  acc += a }\n\
               fn main() -> Unit { twice(true) }";
    let codes: Vec<String> = analyze_and_lower_diags(bad)
        .iter()
        .map(|d| d.code().to_string())
        .collect();
    assert!(
        codes.iter().any(|c| c == "Y015"),
        "expected Y015 at the call that chose Bool, got {codes:?}"
    );

    // The same function called at a number is clean — so the requirement is
    // about the instantiation and not about the `+=`.
    let ok = "fn twice(a) -> Unit { var acc = a\n  acc += a }\n\
              fn main() -> Unit { twice(2) }";
    assert!(!has_type_error_with_lower(ok));

    // A target that is *already* known still reports at the operation, with the
    // operation's own code.
    let concrete: Vec<String> = analyze_and_lower_diags("var flag = true\nflag += false")
        .iter()
        .map(|d| d.code().to_string())
        .collect();
    assert!(
        concrete.iter().any(|c| c == "Y010"),
        "a known target keeps Y010, got {concrete:?}"
    );
}

#[test]
fn map_key_must_be_hashable() {
    let src = "fn id(x: Int) -> Int { x }\n\
               fn main() -> Unit { let map = Map(); map.insert(id, 1) }";
    assert!(
        has_type_error_with_lower(src),
        "function values cannot be structural map keys"
    );
}

#[test]
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
fn unknown_template_capture_parser_is_diagnosed() {
    let src = "let value = read `{value:intr}`";
    assert!(
        has_input_error(src),
        "a misspelled capture parser must not silently default to Int"
    );
    // Any `I0xx` satisfies the line above; only I012 satisfies ADR-051, which
    // allocated `UnknownCaptureKind` for exactly this and had no constructor
    // anywhere in the tree. Every `ScanError` used to be flattened into I030.
    assert!(
        reports_input_code(src, praxis_source::DiagCode::UnknownCaptureKind),
        "the code ADR-051 allocated for this is I012, not the generic I030"
    );
}

/// **IP-04 and IP-06 through the bridge**, which is where they are observable
/// as *diagnostics*: `ScanError` used to be flattened into `TemplateScan`
/// (I030) by one `err_diag` call, so I011, I012 and I013 were allocated in
/// ADR-051 and constructed nowhere.
#[test]
fn a_template_scan_error_reports_the_code_its_own_rule_was_given() {
    use praxis_source::DiagCode;

    for (src, code) in [
        ("let v = read `{9x:int}`", DiagCode::InvalidCaptureName),
        ("let v = read `{value:intr}`", DiagCode::UnknownCaptureKind),
        (
            "let v = read `{x:frobnicate(int)}`",
            DiagCode::UnknownConstructor,
        ),
        (
            "let v = read `{x:csv(int, int)}`",
            DiagCode::ConstructorArity,
        ),
        ("let v = read `prefix\\`", DiagCode::TemplateScan),
    ] {
        assert!(
            reports_input_code(src, code),
            "{src} must report {code:?}, not the generic template-scan code"
        );
    }
}

#[test]
fn unknown_parser_constructor_is_diagnosed() {
    let src = "let value = read frobnicate(int)";
    assert!(
        has_input_error(src),
        "unknown constructor conversion must emit I010-style feedback"
    );
}

#[test]
fn optional_rejects_extra_arguments() {
    let src = "let value = read optional(int, word)";
    assert!(
        has_input_error(src),
        "special constructors must validate source arity before discarding arguments"
    );
}

/// **IP-07's sweep.** Eight of §7.5's fourteen constructors were dispatched by
/// an `if ctor_name == "…"` chain that ran *before* the arity table, took
/// `args.into_iter().next()`, and dropped everything else. So a wrong argument
/// count was not an error, a wrong argument *kind* was not an error, and a name
/// with no row at all was not an error either — it was `None` with no
/// diagnostic.
///
/// Every name at its correct shape is clean; every mistake reports; and the
/// accepted calls are checked for the AST they built, not merely for the
/// absence of a complaint — a silent drop leaves no diagnostic behind.
#[test]
fn every_constructor_checks_its_arguments_before_it_builds_anything() {
    use praxis_source::DiagCode;

    // §7.5's table, each at the shape the design doc writes.
    for (call, expected) in [
        ("lines(int)", "Vec[Int]"),
        ("sections(lines(int))", "Vec[Vec[Int]]"),
        ("csv(int)", "Vec[Int]"),
        ("ws(int)", "Vec[Int]"),
        ("sep(\" -> \", word)", "Vec[Text]"),
        ("grid(char)", "Grid[Char]"),
        ("grid(char, ragged, fill: 0)", "Grid[Char]"),
        ("matrix(int)", "Grid[Int]"),
        ("chars(one_of(\"^v<>\"), skip: whitespace)", "Vec[Char]"),
        ("one_of(\"LR\")", "Char"),
        ("optional(int)", "Option[Int]"),
        ("scan(int)", "Vec[Int]"),
        (
            "block(`{id:int}`, items: lines(int))",
            "{ id: Int, items: Vec[Int] }",
        ),
    ] {
        let src = format!("let value = read {call}");
        assert!(
            !has_input_error(&src),
            "`{call}` is §7.5's own shape and must be accepted"
        );
        assert_eq!(
            scheme_of(&src, "value").as_deref(),
            Some(expected),
            "`{call}` must build the parser it names, not a truncated one"
        );
    }

    // A name with no row: `Constructor::from_keyword(&name)?` used to swallow
    // this whole.
    assert!(
        reports_input_code("let v = read frobnicate(int)", DiagCode::UnknownConstructor),
        "an unknown constructor is I013"
    );

    // Wrong count.
    for call in [
        "optional(int, word)",
        "lines()",
        "lines(int, int)",
        "csv(int, int)",
        "sep(\",\")",
        "one_of(\"a\", \"b\")",
    ] {
        let src = format!("let v = read {call}");
        assert!(
            has_input_error(&src),
            "`{call}` has the wrong number of arguments"
        );
    }

    // Wrong *kind* — the half a count can never see.
    for (call, why) in [
        ("sep(int, int)", "a parser where the separator belongs"),
        (
            "sep(\",\", \",\")",
            "a string where the element parser belongs",
        ),
        ("one_of(int)", "a parser where a character set belongs"),
        ("choice(int)", "a positional in a named-only constructor"),
        ("lines(\"x\")", "a string where a parser belongs"),
        (
            "sections(int, rules: lines(int))",
            "a positional beside named sections",
        ),
        (
            "chars(one_of(\"ab\"), fill: 0)",
            "a keyword `chars` does not take",
        ),
        ("grid(char, fill: 0)", "`fill:` without `ragged`"),
        ("grid(char, ragged)", "`ragged` without `fill:`"),
        (
            "chars(one_of(\"ab\"), skip: sideways)",
            "a skip policy that does not exist",
        ),
    ] {
        let src = format!("let v = read {call}");
        assert!(has_input_error(&src), "`{call}` is {why}");
    }

    // `block()` with nothing in it has no fields and consumes nothing.
    assert!(
        has_input_error("let v = read block()"),
        "a `block` needs at least one item"
    );
}

/// **IP-08.** A parser constructor's string literal used to be decoded by
/// `raw.trim_start_matches('"').trim_end_matches('"')` — a second decoder,
/// beside `lower::unquote_text`, which never unescaped and which stripped
/// *every* quote at each end rather than one.
#[test]
fn a_parser_string_literal_is_decoded_once_like_every_other_literal() {
    use praxis_ast::AstNode;
    use praxis_input_parser::ParserAst;

    /// The `ParserAst` a `read <expr>` in `src` converts to.
    fn parser_ast_of(src: &str) -> ParserAst {
        let map = SourceMap::new();
        let id = map.intern("parser_literal.px", src);
        let parsed = parse(id, src);
        let pe = parsed
            .tree
            .descendants()
            .find_map(praxis_ast::ParserExpr::cast)
            .expect("a parser expression");
        let mut diagnostics = Vec::new();
        crate::parser_lower::convert_parser_expr_for_test(&pe, id, &mut diagnostics)
            .unwrap_or_else(|| panic!("{src} converts: {diagnostics:?}"))
    }

    // `\t` is one tab, not the two characters `\` and `t`. This is the whole
    // finding: `sep("\t", int)` split on a backslash.
    match parser_ast_of(r#"let v = read sep("\t", int)"#) {
        ParserAst::Sep { separator, .. } => assert_eq!(separator.as_str(), "\t"),
        other => panic!("expected Sep, got {other:?}"),
    }

    // One quote, not zero: `trim_end_matches('"')` ate the escaped quote too.
    match parser_ast_of(r#"let v = read one_of("\"")"#) {
        ParserAst::OneOf { chars, .. } => assert_eq!(chars, "\""),
        other => panic!("expected OneOf, got {other:?}"),
    }

    // Both real quotes survive. `trim_start_matches`/`trim_end_matches` strip a
    // *run*, so this used to decode to the empty separator — the one IP-10 says
    // cannot exist.
    match parser_ast_of(r#"let v = read sep("\"\"", int)"#) {
        ParserAst::Sep { separator, .. } => assert_eq!(separator.as_str(), "\"\""),
        other => panic!("expected Sep, got {other:?}"),
    }

    // And an escape neither decoder knows is preserved exactly as
    // `unquote_text` preserves it — which is how the two are shown to be one.
    match parser_ast_of(r#"let v = read sep("\q", int)"#) {
        ParserAst::Sep { separator, .. } => assert_eq!(separator.as_str(), r"\q"),
        other => panic!("expected Sep, got {other:?}"),
    }
}

/// **IP-09's second half.** §7.5: "`repeated(parser)` may appear only as the
/// final named argument". Neither half of that was checked — a second tail
/// silently overwrote the first, and a tail written before other fields was
/// silently moved to the end, so the parser that ran was not the one written.
#[test]
fn a_repeated_tail_is_last_and_singular() {
    use praxis_source::DiagCode;

    // Misordered: `boards` consumes every remaining section, so `draws` after
    // it can never match. This used to compile into the *reordered* parser.
    assert!(
        reports_input_code(
            "let b = read sections(boards: repeated(matrix(int)), draws: csv(int))",
            DiagCode::MisplacedRepeatedTail
        ),
        "a tail before another field silently reordered the call"
    );

    // Two tails: the second used to overwrite the first, so `a` vanished.
    assert!(
        reports_input_code(
            "let b = read sections(a: repeated(int), b: repeated(int))",
            DiagCode::MisplacedRepeatedTail
        ),
        "`sections` takes at most one tail"
    );

    // Outside `sections` there is nothing to repeat over. This used to fall
    // through `Constructor::from_keyword`'s `?` and produce no diagnostic.
    assert!(
        reports_input_code(
            "let b = read repeated(int)",
            DiagCode::MisplacedRepeatedTail
        ),
        "a bare `repeated(...)` is not a parser"
    );

    // And the legal shape is still clean and still builds the tail last —
    // `tests/aoc-corpus/m9_bingo.px`'s own call.
    let legal = "let b = read sections(draws: csv(int), boards: repeated(matrix(int)))";
    assert!(!has_input_error(legal), "the ordered form is legal");
    assert_eq!(
        scheme_of(legal, "b").as_deref(),
        Some("{ draws: Vec[Int], boards: Vec[Grid[Int]] }"),
        "the tail is the last field and it is a Vec of the repeated parser's result"
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
            .find_map(|s| crate::stmt_exprs(s).find_map(find_closure))
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
            for e in crate::stmt_exprs(stmt) {
                walk_expr(e, found);
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

/// TY-32/D4 as the **rule**, not the four exit cases. The question a key has to
/// answer is not "can this be hashed" — a `Vec` hashes fine — it is "can this
/// still be found after the program changes it". The two used to be one
/// predicate (`supports_hash` was literally `supports_eq`), and that is what
/// admitted every mutable collection as a key.
#[test]
fn a_mutable_collection_is_not_a_key() {
    // Every mutable collection, in a `Map` key position.
    for ctor in ["Vec", "Set", "Deque", "MinHeap", "MaxHeap"] {
        let src = format!(
            "fn main() -> Unit {{\n  let key = {ctor}()\n  key.push(1)\n  let m = Map()\n  m.insert(key, 1)\n}}"
        );
        assert!(
            has_type_error(&src),
            "a {ctor} cannot be a Map key, but was accepted"
        );
    }
    // A `Set` element is a key too, and so is a `Counter`'s.
    assert!(has_type_error(
        "fn main() -> Unit {\n  let key = Vec()\n  key.push(1)\n  let s = Set()\n  s.insert(key)\n}"
    ));
    assert!(has_type_error(
        "fn main() -> Unit {\n  let key = Vec()\n  key.push(1)\n  let c = Counter()\n  c.inc(key)\n}"
    ));
}

/// …and the rule is mutability, not container-ness. Every immutable shape is
/// still a key, including a tuple of them — which is Python's `tuple` rule and
/// the one a grid-coordinate program depends on.
#[test]
fn an_immutable_value_is_still_a_key() {
    assert!(!has_type_error(
        "fn main() -> Unit { let m = Map(); m.insert(1, 2) }"
    ));
    assert!(!has_type_error(
        "fn main() -> Unit { let m = Map(); m.insert(\"k\", 2) }"
    ));
    // A tuple of scalars — the shape every grid-position map uses.
    assert!(!has_type_error(
        "fn main() -> Unit { let m = Map(); m.insert((1, 2), 3) }"
    ));
    // An enum, including the prelude's Option.
    assert!(!has_type_error(
        "enum Dir { N, S }\nfn main() -> Unit { let s = Set(); s.insert(N) }"
    ));
    // A tuple with a mutable component is not, though: one is enough.
    assert!(has_type_error(
        "fn main() -> Unit {\n  let v = Vec()\n  v.push(1)\n  let m = Map()\n  m.insert((1, v), 3)\n}"
    ));
}

/// A heap orders what it holds, so its element type must have an order. This is
/// the same channel as the key rule and a different capability — `Text` is
/// orderable and is not a key requirement, a `Vec` is neither.
#[test]
fn a_heap_element_must_be_orderable() {
    assert!(has_type_error(
        "fn id(x: Int) -> Int { x }\nfn main() -> Unit { let h = MinHeap(); h.push(id) }"
    ));
    assert!(has_type_error(
        "fn main() -> Unit { let h = MaxHeap(); h.push((1, 2)) }"
    ));
    // Int and Text both have a runtime `compare`, so both are legal elements.
    assert!(!has_type_error(
        "fn main() -> Unit { let h = MinHeap(); h.push(1) }"
    ));
    assert!(!has_type_error(
        "fn main() -> Unit { let h = MinHeap(); h.push(\"a\") }"
    ));
}

/// The requirement survives a generic function, which is the half a direct
/// `insert` cannot show: `store`'s key parameter is unconstrained, the
/// requirement is claimed by its scheme, and the *call site* is what chooses a
/// type that cannot be a key.
///
/// The map is built inside `store`, not passed in: a `Map` *parameter* is an
/// unresolved receiver, and constraining one of those is TY-30, which this
/// stage's own exit test still covers separately.
#[test]
fn a_key_requirement_reaches_through_a_generic_function() {
    let src = "fn store(k) -> Unit { let m = Map(); m.insert(k, 1) }\n\
               fn main() -> Unit {\n\
                 let key = Vec()\n\
                 key.push(1)\n\
                 store(key)\n\
               }";
    assert!(
        has_type_error(src),
        "the constraint must travel with `store`'s scheme to its call site"
    );
    // …and the same function is fine at a key type that is one.
    let ok = "fn store(k) -> Unit { let m = Map(); m.insert(k, 1) }\n\
              fn main() -> Unit { store(\"key\") }";
    assert!(!has_type_error(ok));
}

/// TY-25's other half, and the reason the finding looked like an inference bug:
/// `parse` is **syntax**, not a call of a binding. It used to be lexed into a
/// `PATH_EXPR` inside its own `PARSE_EXPR`, which made every use an `N001` — and
/// made `ParseExpr::text_expr` ("the first `Expr` child") answer with the
/// keyword's own path rather than the argument, so the text argument's type was
/// never looked at.
#[test]
fn parse_is_syntax_and_its_text_argument_is_the_one_checked() {
    // The name is not a binding, so a well-formed `parse` reports nothing.
    assert!(is_clean_with_lower(
        "fn main() -> Int { parse(\"12\", int) }"
    ));
    // …and the argument that is checked is the *first* one.
    let diags = analyze_and_lower_diags("fn main() -> Int { parse(1, int) }");
    assert_eq!(
        diags.len(),
        1,
        "one report, not a name error too: {diags:?}"
    );
    assert_eq!(diags[0].code().to_string(), "Y001");
    // A `Text`-typed expression, not only a literal, is accepted.
    assert!(is_clean_with_lower(
        "fn main() -> Int {\n  let s = \"12\"\n  parse(s, int)\n}"
    ));
}

/// TY-26 as the rule: negation follows the operand's **type**, and the
/// per-literal shortcut was only ever an approximation of it. A `Float`-typed
/// variable was negated as an `Int` and then failed to unify with itself.
#[test]
fn negation_follows_the_operands_type_not_its_spelling() {
    assert!(!has_type_error("fn negate(x: Float) -> Float { -x }"));
    assert!(!has_type_error("fn negate(x: Int) -> Int { -x }"));
    // The literal cases still work, in both directions.
    assert!(!has_type_error("fn f() -> Float { -3.5 }"));
    assert!(!has_type_error("fn f() -> Int { -3 }"));
    // …and negation still has a type: an Int operand does not produce a Float.
    assert!(has_type_error("fn f(x: Int) -> Float { -x }"));
    // A Float expression that is not a literal — the shape the old rule missed.
    assert!(!has_type_error("fn f(x: Float) -> Float { -(x + 1.0) }"));
}

/// TY-27: `%` has no `Float` lowering, and MIR's unsupported-operator fallback
/// mapped it to **addition** — so `5.0 % 2.0` computed `7.0`. There is no
/// operation to lower, so there is nothing to accept.
#[test]
fn float_remainder_has_no_operation_to_lower() {
    assert!(has_type_error("fn bad() -> Float { 5.0 % 2.0 }"));
    assert!(has_type_error(
        "fn bad(a: Float, b: Float) -> Float { a % b }"
    ));
    // The Int remainder is untouched, and so is every other Float operator.
    assert!(!has_type_error("fn ok() -> Int { 5 % 2 }"));
    for op in ["+", "-", "*", "/"] {
        let src = format!("fn ok() -> Float {{ 5.0 {op} 2.0 }}");
        assert!(!has_type_error(&src), "Float {op} is defined");
    }
}

/// TY-28: an `Int` is signed 64-bit (§4.3), so a literal outside that range
/// names a value the language cannot represent. It became `i64::MAX` silently,
/// on the theory that the arithmetic would fault — but a saturated literal is a
/// perfectly good `Int` and the program runs with a number nobody wrote.
#[test]
fn an_out_of_range_int_literal_is_reported_rather_than_saturated() {
    for src in [
        "fn main() -> Int { 9223372036854775808 }",
        "fn main() -> Int { 99999999999999999999999 }",
        // The separated spelling is the same literal and the same report. This
        // is REP-11's own reproduction: before the lexer accepted separators it
        // was `9` followed by the identifier `_223…`, so the mistake surfaced as
        // an `N001` about an undefined name.
        "fn main() -> Int { 9_223_372_036_854_775_808 }",
    ] {
        let diags = analyze_and_lower_diags(src);
        assert!(
            diags.iter().any(|d| d.code().to_string() == "Y013"),
            "{src} must report Y013, got {diags:?}"
        );
    }
    // The boundary itself is in range and reports nothing.
    assert!(is_clean_with_lower(
        "fn main() -> Int { 9223372036854775807 }"
    ));
    // A pattern is the same rule at another position.
    let diags = analyze_and_lower_diags(
        "fn f(n: Int) -> Int { match n { 99999999999999999999 => 1, _ => 0 } }",
    );
    assert!(diags.iter().any(|d| d.code().to_string() == "Y013"));
}

/// **REP-01, ADR-061.** A top-level `fn` in value position lowers to a *function
/// value*, and the typed tree says so.
///
/// It used to lower to `TypedExpr::Path`, whose symbol has no local slot, so MIR
/// answered `Unit` and `Inst::CallIndirect` read that Unit's payload as a
/// function pointer. The distinction is the symbol's **kind**: a `let` holding a
/// closure is a `Path` and has a `Func` type too, so the scheme cannot tell them
/// apart — the same reason `SymbolKind::EnumVariant` exists (HIR-03).
#[test]
fn a_fn_name_in_value_position_is_a_function_value() {
    use praxis_ast::AstNode;

    let src = "fn double(n: Int) -> Int { n * 2 }\n\
               fn main() -> Int {\n  let f = double\n  let g = |n| n * 3\n  f(1) + g(1)\n}\n";
    let map = SourceMap::new();
    let id = map.intern("fn_value.px", src);
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

    // `let f = double` — a function value naming `double`, typed as `double`'s
    // own signature.
    let crate::TypedStmt::Let { init, .. } = &main.body.stmts[0] else {
        panic!("expected `let f = double`, got {:?}", main.body.stmts[0]);
    };
    let crate::TypedExpr::FnValue {
        callee_name, ty, ..
    } = init
    else {
        panic!("a `fn` in value position is a function value, got {init:?}");
    };
    assert_eq!(callee_name, "double");
    assert_eq!(analysis.db.render(*ty), "(Int) -> Int");

    // …and a `let` holding a *closure* is still a closure literal, so the new
    // arm did not swallow the case it sits next to.
    let crate::TypedStmt::Let { init, .. } = &main.body.stmts[1] else {
        panic!("expected `let g = |n| n * 3`");
    };
    assert!(
        matches!(init, crate::TypedExpr::Closure { .. }),
        "a closure literal is unchanged: {init:?}"
    );
}

/// A **generic** `fn` used as a value is reported where it is written, not left
/// to fail in the backend.
///
/// Monomorphization is driven by call sites; a value has none, so the adapter
/// would call a clone-source the mono pass drops and the JIT would say
/// "unresolved user function `id`" — a Cranelift error for a program `praxis
/// check` accepted, which is TY-33's shape all over again. `Y018` names the
/// remedy instead, and the remedy works: a closure body *is* a call site.
#[test]
fn a_generic_fn_used_as_a_value_is_reported_rather_than_run() {
    let diags =
        analyze_and_lower_diags("fn id(x) { x }\nfn main() -> Int {\n  let f = id\n  f(3)\n}\n");
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y018"),
        "expected Y018, got {diags:?}"
    );
    // It is reported at *analysis*, so `praxis check` sees it — the asymmetry
    // REP-12 was about.
    assert!(has_type_error(
        "fn id(x) { x }\nfn main() -> Int {\n  let f = id\n  f(3)\n}\n"
    ));

    // The remedy compiles, and so does a monomorphic function used as a value.
    assert!(is_clean_with_lower(
        "fn id(x) { x }\nfn main() -> Int {\n  let f = |n| id(n)\n  f(3)\n}\n"
    ));
    assert!(is_clean_with_lower(
        "fn double(n: Int) -> Int { n * 2 }\nfn main() -> Int {\n  let f = double\n  f(3)\n}\n"
    ));
    // And *calling* the generic function directly is untouched — the report is
    // about value position only.
    assert!(is_clean_with_lower(
        "fn id(x) { x }\nfn main() -> Int { id(3) }\n"
    ));
}

/// **REP-06.** A `struct`/`enum` inside a function body is reported where it is
/// written, not left silent.
///
/// `register_top_level` walks the source file's own statements, so a nested
/// declaration got no symbol, no type and **no diagnostic**: declaring one was
/// accepted in silence, and using it was an `N001` about a name written two lines
/// above. It is `N005` now — the code a nested `fn` already uses (TY-23), because
/// it is the same mistake, and a *use* still reports its own `N001` exactly as it
/// does for a nested `fn`.
#[test]
fn a_nested_type_declaration_is_reported_at_the_declaration() {
    // Declared and never used: this was complete silence.
    for src in [
        "fn main() -> Int {\n  struct Inner { a: Int }\n  3\n}",
        "fn main() -> Int {\n  enum Inner { On, Off }\n  3\n}",
        // Nested inside a block inside a function, which is the same position.
        "fn main() -> Int {\n  if true {\n    struct Inner { a: Int }\n  }\n  3\n}",
    ] {
        let analysis = analyze(src);
        assert!(
            analysis
                .diagnostics
                .iter()
                .any(|d| d.kind() == praxis_source::DiagCode::NestedFunction),
            "{src} must report N005, got {:?}",
            analysis.diagnostics
        );
    }

    // …and the four top-level declaration kinds are untouched.
    assert!(is_clean_with_lower(
        "struct Point { x: Int, y: Int }\n\
         enum Flag { On, Off }\n\
         fn get(p: Point) -> Int { p.x }\n\
         fn main() -> Int { get(Point { x: 1, y: 2 }) }\n"
    ));
    // A top-level `let`/`var` beside them, so "top level" is the file and not
    // "the first statement". The `let` is read by a *top-level* statement here:
    // it used to be read by `main`, which is `N007` now that a `fn` reading a
    // binding around it is reported (REP-22) — a different check, and one this
    // test is not about.
    assert!(is_clean_with_lower(
        "let base = 1\nstruct Point { x: Int }\nvar total = base\nfn main() -> Int { 0 }\n"
    ));
}

/// **REP-05.** A pattern naming more sub-patterns than the variant holds is
/// reported; naming fewer is still the padding rule.
///
/// `match w { Wrap(a, b) => a }` against a one-slot variant **compiled and ran**,
/// answering the payload: `b` was lowered (so a mistake inside it still reported)
/// and then dropped. Truncating is strictly safer than the payload read past the
/// end it replaced — which is why it stays — but accepting is not the answer.
/// `Y122` and `Y123` covered the two neighbouring mistakes and this one had no
/// code; it is `Y124` now.
#[test]
fn a_pattern_naming_more_values_than_the_variant_holds_is_reported() {
    let diags = analyze_and_lower_diags(
        "enum W { Wrap(Int) }\n\
         fn main() -> Int {\n  let w = Wrap(7)\n  match w { Wrap(a, b) => a }\n}",
    );
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y124"),
        "expected Y124, got {diags:?}"
    );

    // A payload-less variant holds nothing, so *any* sub-pattern is too many.
    let diags = analyze_and_lower_diags(
        "enum W { Empty, Wrap(Int) }\n\
         fn main() -> Int {\n  let w = Empty\n  match w { Empty(a) => 1, Wrap(n) => n }\n}",
    );
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y124"),
        "a payload-less variant names zero values: {diags:?}"
    );

    // …and naming *fewer* is legal at every count — HIR-06's padding rule, which
    // is why the check is one-sided.
    for src in [
        "enum W { Wrap(Int) }\nfn main() -> Int { let w = Wrap(7)\n match w { Wrap => 1 } }",
        "enum W { Wrap(Int) }\nfn main() -> Int { let w = Wrap(7)\n match w { Wrap(_) => 1 } }",
        "enum W { Wrap(Int) }\nfn main() -> Int { let w = Wrap(7)\n match w { Wrap(n) => n } }",
        "enum P { Pair(Int, Int) }\nfn main() -> Int { let p = Pair(1, 2)\n match p { Pair(a) => a } }",
        "enum P { Pair(Int, Int) }\nfn main() -> Int { let p = Pair(1, 2)\n match p { Pair(a, b) => a + b } }",
    ] {
        assert!(is_clean_with_lower(src), "{src} must still be accepted");
    }
}

/// **REP-03.** A `for` over an unannotated parameter is generic in the
/// **iterable** and monomorphic in the **element** — the two are no longer one
/// variable.
///
/// `iter_item` answered an unresolved receiver with *itself*, so the loop
/// variable and the iterator came back as the same type. Two things followed, and
/// both were wrong:
///
/// - `t = t + i` pinned that one variable to `Int`, and the `for` then reported
///   `Y005` "values of type `Int` cannot be iterated" — about a parameter the
///   program never typed. A legal program rejected, identically for `Vec`,
///   `BitSet` and `Range`, which is why TY-34's gates all annotate.
/// - When nothing pinned it, the loop variable's recorded type was the
///   *collection's*: `fn copy(vs) { var o = Vec()\n for v in vs { o.push(v) }\n
///   o }` inferred `o: Vec[Vec[Int]]` and faulted at run time with "value does
///   not have the declared type" — out of a program `praxis check` accepted.
///
/// So the assertion is not only "accepted": it is what `total` *is*. `forall T.
/// (T) -> Int` — any iterable, of `Int` — where it used to be `(Int) -> Int`.
#[test]
fn an_unannotated_iterated_parameter_is_generic_in_the_iterable_not_its_element() {
    const TOTAL: &str = "fn total(r) { var t = 0\n for i in r { t = t + i }\n t }\n";

    // The rule, as the type. The iterable is quantified; the element is not,
    // because `t + i` said what it is.
    let scheme = scheme_of(TOTAL, "total").expect("total has a scheme");
    insta::assert_snapshot!(scheme, @"forall T. (T) -> Int");

    // …and it is satisfied by each of the three iterables the finding names.
    // `Vec` and `Range` also *run*; a `BitSet` has no element accessor in the
    // runtime, so this is the criterion's "accepted" and no more (see REP-15).
    for call in [
        "fn main() -> Int { var v = Vec()\n v.push(1)\n total(v) }",
        "fn main() -> Int { total(0..4) }",
        "fn main() -> Int { var b = BitSet()\n b.insert(3)\n total(b) }",
        // A `Deque` too, so "iterable" is a property and not a three-name list.
        "fn main() -> Int { var d = Deque()\n d.push_back(1)\n total(d) }",
    ] {
        assert!(
            !has_type_error(&format!("{TOTAL}{call}")),
            "must be accepted: {call}"
        );
    }

    // Two *different* iterable kinds at the same element type are two
    // instantiations of one signature, which is what "an iterable of `Int`"
    // means. A `Vec` and a `Range` in one program is not a disagreement.
    assert!(!has_type_error(&format!(
        "{TOTAL}fn main() -> Int {{ var v = Vec()\n v.push(1)\n total(v) + total(0..4) }}"
    )));

    // The loop variable is the **element**, not the collection: this is the half
    // that faulted at run time rather than being reported. `o` is a `Vec[Int]`
    // now, so returning it as one is accepted and returning `Vec[Vec[Int]]` is
    // not.
    const COPY: &str = "fn copy(vs) { var o = Vec()\n for v in vs { o.push(v) }\n o }\n";
    assert!(!has_type_error(&format!(
        "{COPY}fn main() -> Int {{ var s = Vec()\n s.push(1)\n let d: Vec[Int] = copy(s)\n d.len() }}"
    )));
    assert!(has_type_error(&format!(
        "{COPY}fn main() -> Int {{ var s = Vec()\n s.push(1)\n let d: Vec[Vec[Int]] = copy(s)\n d.len() }}"
    )));

    // An annotated parameter is untouched — the shape every existing gate uses.
    assert!(!has_type_error(
        "fn total(r: Vec[Int]) -> Int { var t = 0\n for i in r { t = t + i }\n t }"
    ));
    // …and a concrete non-iterable is still `Y005` where it is written.
    assert!(has_type_error(
        "fn main() -> Unit { for i in 3 { out(i) } }"
    ));
}

/// **REP-04.** An `Iterable` requirement is discharged by **unifying** the item,
/// so a receiver that iterates at the wrong element type is reported.
///
/// `capability::check` answers iterability as a yes/no — its failure shape is
/// "the offending type", and "iterates, but not at that element type" is a
/// *mismatch*, not that. So a constraint carried through generalization and
/// discharged at a differently-itemed iterable was silently accepted. This is the
/// half that has never had a test, and it could not have had one before REP-03:
/// on the unfixed tree the same program reports `Y005` at the `for`, because
/// pinning the element pinned the iterator with it.
///
/// The report goes to the **use site** with the `for` as its note (ADR-057
/// decision 2): `for i in r { t = t + i }` is correct for every other
/// instantiation of `total`.
#[test]
fn an_iterable_requirement_is_checked_at_the_element_type_the_body_needs() {
    const TOTAL: &str = "fn total(r) { var t = 0\n for i in r { t = t + i }\n t }\n";

    // A `Vec[Text]` iterates. It does not iterate `Int`s, which is what the body
    // requires — reported, with the note.
    let diags = analyze_and_lower_diags(&format!(
        "{TOTAL}fn main() -> Int {{ var names = Vec()\n names.push(\"a\")\n total(names) }}"
    ));
    let mismatch = diags
        .iter()
        .find(|d| d.code().to_string() == "Y001")
        .unwrap_or_else(|| panic!("expected Y001, got {diags:?}"));
    assert!(
        mismatch.message().contains("Int") && mismatch.message().contains("Text"),
        "the message names both element types: {}",
        mismatch.message()
    );
    assert!(
        !mismatch.notes().is_empty(),
        "the requirement's own span is the note"
    );

    // Every differently-itemed iterable is the same answer, whatever its ctor.
    for call in [
        "fn main() -> Int { var m = Map()\n m.insert(1, 2)\n total(m) }",
        "fn main() -> Int { var v = Vec()\n v.push(1.5)\n total(v) }",
        "fn main() -> Int { var s = Set()\n s.insert(\"a\")\n total(s) }",
    ] {
        assert!(
            has_type_error(&format!("{TOTAL}{call}")),
            "a differently-itemed receiver must be reported: {call}"
        );
    }

    // Not iterable **at all** is still the channel's own `Y005`, unchanged —
    // TY-29's gate, restated here because this is the function that now decides
    // both outcomes.
    let diags = analyze_and_lower_diags(&format!("{TOTAL}fn main() -> Int {{ total(1) }}"));
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y005"),
        "an Int is not iterable at any element type: {diags:?}"
    );

    // …and the right element type is accepted at every one of those ctors, so the
    // check is a real unification and not "reject the unfamiliar".
    for call in [
        "fn main() -> Int { var v = Vec()\n v.push(1)\n total(v) }",
        "fn main() -> Int { var s = Set()\n s.insert(1)\n total(s) }",
    ] {
        assert!(
            !has_type_error(&format!("{TOTAL}{call}")),
            "an Int-itemed receiver is accepted: {call}"
        );
    }
}

/// **REP-14.** A `struct`/`enum` that refers to itself is reported where it is
/// declared, rather than registered with a fresh variable in silence.
///
/// The declaration pass registers types in dependency order; a declaration in a
/// cycle never becomes ready, and the recursive member fell back to a fresh type
/// variable with no report. That is not merely silence — a variable unifies with
/// everything, so `struct Node { next: Node, value: Int }` accepted `Node { next:
/// 7, value: 1 }` and **ran** it. One unchecked member per recursive declaration.
///
/// D17's answer, as recommended: report it (`N006`), which supersedes ADR-052's
/// silence. Supporting recursive types is a language feature and stays out of
/// scope.
#[test]
fn a_self_referring_type_declaration_is_reported_rather_than_registered_as_a_variable() {
    let n006 = |src: &str| -> usize {
        analyze(src)
            .diagnostics
            .iter()
            .filter(|d| d.kind() == praxis_source::DiagCode::RecursiveTypeDeclaration)
            .count()
    };

    // Direct, in both declaration keywords.
    assert_eq!(n006("struct Node { next: Node, value: Int }"), 1);
    assert_eq!(n006("enum List { Nil, Cons(Int, List) }"), 1);
    // Through a collection. A `Vec[Node]` *is* representable — every Praxis field
    // holds a reference — so this is the same missing feature and not a different
    // one, and it had the same silent variable (its element's).
    assert_eq!(n006("struct Node { children: Vec[Node], value: Int }"), 1);
    assert_eq!(n006("struct Node { by_name: Map[Text, Node] }"), 1);
    // A mutual pair, and a three-cycle: each member is reported once.
    assert_eq!(n006("struct A { b: B }\nstruct B { a: A }"), 2);
    assert_eq!(
        n006("struct A { b: B }\nstruct B { c: C }\nstruct C { a: A }"),
        3
    );

    // The message names the way round, which is the only thing that tells a
    // mutual pair apart from two self-references.
    let diags = analyze("struct A { b: B }\nstruct B { a: A }").diagnostics;
    let a = diags
        .iter()
        .find(|d| d.message().starts_with("`A`"))
        .expect("A is reported");
    assert!(
        a.message().contains("through `B`"),
        "the cycle names its other member: {}",
        a.message()
    );

    // **A declaration that merely waits behind a cycle is not the mistake.**
    // `C` is written above the recursive pair, so the stalled pass used to leave
    // it in the remainder too: it got a fresh variable for `a` and accepted a
    // `Text` in it. It is unreported and its field is a real `A` now.
    let src = "struct C { a: A }\n\
               struct A { b: B }\n\
               struct B { a: A }\n\
               fn main() -> Unit { let c = C { a: \"not an A\" }\n out(c.a) }";
    assert_eq!(n006(src), 2, "only A and B are recursive");
    let diags = analyze(src);
    assert!(
        diags
            .diagnostics
            .iter()
            .any(|d| d.kind() == praxis_source::DiagCode::TypeMismatch),
        "C's field is a real `A`, so a Text in it is a mismatch: {:?}",
        diags.diagnostics
    );

    // A field or variant *named* like the type is not a reference to it: only
    // annotation tokens are in `type_refs`, which is what makes this precise.
    assert_eq!(n006("struct Node { node: Int }"), 0);
    assert_eq!(n006("enum E { E }"), 0);
    // …and a non-recursive forward reference still resolves, in both directions.
    // TY-10's gate owns the rule; this is the code that could break it.
    assert_eq!(
        n006("struct Outer { inner: Inner }\nstruct Inner { n: Int }"),
        0
    );
    assert!(!has_type_error(
        "struct Outer { inner: Inner }\n\
         struct Inner { n: Int }\n\
         fn f(o: Outer) -> Int { o.inner.n }"
    ));
    assert_eq!(
        n006("enum Shape { Round(Circle) }\nstruct Circle { r: Int }"),
        0
    );

    // Exactly one report per declaration, and no cascade about its uses: the
    // member is still a variable on purpose, because the declaration has already
    // been reported and a second report per use would say the same thing again.
    let diags = analyze(
        "struct Node { next: Node, value: Int }\n\
         fn main() -> Int { let n = Node { next: 7, value: 1 }\n n.value }",
    )
    .diagnostics;
    assert_eq!(
        diags.len(),
        1,
        "one report for the declaration and nothing else: {diags:?}"
    );
}

/// **REP-07.** `&&` and `||` take two `Bool`s and produce one, and a divergent
/// operand is absorbed rather than reported.
///
/// There is no truthiness, so the type rule is the whole rule and it is the same
/// for both operators — the short-circuit is MIR's. The `Never` half is what the
/// exit criterion's own example needs: `panic` is `Never`, so `false &&
/// panic("x")` unified `Never` with `Bool` and reported "expected Never, found
/// Bool" — a `Y001` about the operator rather than about the program. The
/// operands **join** now (TY-19/ADR-053), which is what every other branch point
/// in the language already does.
#[test]
fn the_logical_operators_take_two_bools_and_produce_one() {
    for op in ["&&", "||"] {
        // The rule.
        assert_eq!(
            expr_type(&format!("true {op} false")),
            "Bool",
            "{op} is Bool"
        );
        assert!(!has_type_error(&format!(
            "fn f(a: Bool, b: Bool) -> Bool {{ a {op} b }}"
        )));
        // Either operand may be an arbitrary `Bool` expression, including a
        // comparison — which is the shape the precedence exists for.
        assert!(!has_type_error(&format!(
            "fn f(x: Int, y: Int) -> Bool {{ x == 1 {op} y != 0 }}"
        )));
        // A non-`Bool` operand is refused, in either position. There is no
        // truthiness: an `Int` is not a condition.
        assert!(has_type_error(&format!("fn f() -> Bool {{ 1 {op} true }}")));
        assert!(has_type_error(&format!("fn f() -> Bool {{ true {op} 1 }}")));
        assert!(has_type_error(&format!(
            "fn f(v: Vec[Int]) -> Bool {{ v {op} true }}"
        )));
        // …and the result is a `Bool` and not the operand type.
        assert!(has_type_error(&format!(
            "fn f() -> Int {{ true {op} false }}"
        )));

        // A divergent operand is absorbed, in either position.
        assert!(
            !has_type_error(&format!("fn f() -> Bool {{ false {op} panic(\"x\") }}")),
            "a `Never` right operand is absorbed by {op}"
        );
        assert!(!has_type_error(&format!(
            "fn f() -> Bool {{ panic(\"x\") {op} false }}"
        )));
    }
}

/// **REP-08.** `p.0` reads a tuple element, and reading past the end — or off
/// something that is not a tuple — is `Y019` in *inference*.
///
/// A `(Int, Int)` was a legal value, a legal `Map` key and a legal graph state
/// (ADR-060) that **no function could read**: `p.0` was a `P001` at the dot, and
/// `tests/aoc-corpus/day10_bfs_shortest_distance.px` says so in a comment and
/// hand-encodes its adjacency around it.
///
/// The report is in inference and not at lowering, for `Y018`'s reason (ADR-061):
/// `praxis check` does not run lowering, so a program reported only there is clean
/// under `check` and fails under `run` — the asymmetry REP-12 was about. It is
/// also **not** `Y112` ("no field on this type"): a tuple has no field *names*.
#[test]
fn a_tuple_element_is_read_by_position_and_a_bad_index_is_reported() {
    let y019 = |src: &str| -> bool {
        analyze(src)
            .diagnostics
            .iter()
            .any(|d| d.kind() == praxis_source::DiagCode::NoTupleElement)
    };

    // Every position of every arity, and the element's *type* — a fresh variable
    // would accept all of these and prove nothing.
    assert_eq!(expr_type("(1, 2).0"), "Int");
    assert_eq!(expr_type("(1, \"a\").1"), "Text");
    assert_eq!(expr_type("(1, \"a\", true).2"), "Bool");
    assert_eq!(expr_type("(1, 2, 3, 4, 5).4"), "Int");
    // …and a nested one, which is the case the lexer had to be taught: `n.0.1`
    // is two indices and not an index and the float `0.1`.
    assert_eq!(expr_type("((1, \"a\"), 3).0.1"), "Text");

    // Through a binding, a parameter and a closure body.
    assert!(!has_type_error(
        "fn fst(p: (Int, Text)) -> Int { p.0 }\n\
         fn main() -> Int { let q = (1, \"a\")\n fst(q) }"
    ));
    assert!(!has_type_error(
        "fn main() -> Int { let f = |p: (Int, Int)| p.0 + p.1\n f((1, 2)) }"
    ));
    // The element type is enforced, not invented.
    assert!(has_type_error("fn f() -> Text { (1, 2).0 }"));

    // Past the end, at every arity — including an index no `usize` can hold,
    // which is out of range for every tuple.
    assert!(y019("fn f() -> Int { (1, 2).2 }"));
    assert!(y019("fn f() -> Int { (1, 2).5 }"));
    assert!(y019("fn f() -> Int { (1, 2).99999999999999999999 }"));
    // …and off something that is not a tuple at all: a scalar, a collection, a
    // record — each of which has its own way of not having elements.
    assert!(y019("fn f() -> Int { let n = 1\n n.0 }"));
    assert!(y019("fn f() -> Int { let t = \"ab\"\n t.0 }"));
    assert!(y019("fn f(v: Vec[Int]) -> Int { v.0 }"));
    assert!(y019("struct P { x: Int }\nfn f(p: P) -> Int { p.0 }"));

    // A *record* field is untouched — same syntax, different operation, and the
    // codes must not have swapped.
    assert!(!has_type_error(
        "struct P { x: Int }\nfn f(p: P) -> Int { p.x }"
    ));
    assert!(!y019("struct P { x: Int }\nfn f(p: P) -> Int { p.x }"));

    // An unresolved receiver says nothing: `p.0` on an unannotated parameter is
    // optimistic, as every capability question about a variable is.
    assert!(!y019("fn first(p) { p.0 }"));
}

/// **REP-16, the read form.** `m[key]` is the type the receiver holds at that
/// key, and which receivers index is the catalog's answer.
///
/// Six collections index and the arity is part of the operation, so this asserts
/// the *result* type at each — a subscript that returned the receiver, or the
/// element pattern's uninstantiated `?T`, would pass a "no diagnostic" test.
/// `Grid` is the reason arity is here at all: §6.4 spells it `grid[x, y]`, so
/// `grid[x]` is a mistake about a receiver that does index.
#[test]
fn a_subscript_reads_the_type_the_receiver_holds_at_that_key() {
    let y020 = |src: &str| -> bool {
        analyze(src)
            .diagnostics
            .iter()
            .any(|d| d.kind() == praxis_source::DiagCode::NotIndexable)
    };

    // Each of the six, at a result type that is not the receiver's own.
    assert_eq!(
        scheme_of(
            "fn f(v: Vec[Text]) -> Text { v[0] }\nlet _p = f(Vec())",
            "_p"
        ),
        Some("Text".to_string())
    );
    assert_eq!(
        scheme_of(
            "fn f(d: Deque[Text]) -> Text { d[0] }\nlet _p = f(Deque())",
            "_p"
        ),
        Some("Text".to_string())
    );
    assert_eq!(
        scheme_of(
            "fn f(m: Map[Text, Float]) -> Float { m[\"a\"] }\nlet _p = f(Map())",
            "_p"
        ),
        Some("Float".to_string())
    );
    // A `Counter`'s value is its count, whatever its key type is (§6.2).
    assert_eq!(
        scheme_of(
            "fn f(c: Counter[Text]) -> Int { c[\"a\"] }\nlet _p = f(Counter())",
            "_p"
        ),
        Some("Int".to_string())
    );
    assert_eq!(
        scheme_of(
            "fn f(g: Grid[Text]) -> Text { g[0, 0] }\nlet _p = f(Grid())",
            "_p"
        ),
        Some("Text".to_string())
    );
    // `Text` indexes to a char's scalar value, which is what `.get` answers too.
    assert_eq!(
        scheme_of("fn f(t: Text) -> Int { t[0] }\nlet _p = f(\"ab\")", "_p"),
        Some("Int".to_string())
    );

    // The key is checked, not accepted: a `Map[Text, Int]` indexed by an Int is a
    // mismatch, which is only visible because the row's `K` unified with it.
    assert!(has_type_error("fn f(m: Map[Text, Int]) -> Int { m[0] }"));
    assert!(!has_type_error(
        "fn f(m: Map[Text, Int]) -> Int { m[\"a\"] }"
    ));

    // A receiver with no subscript at all.
    assert!(y020("fn f(s: Set[Int]) -> Int { s[0] }"));
    assert!(y020("fn f(b: BitSet) -> Int { b[0] }"));
    assert!(y020("fn f(h: MinHeap[Int]) -> Int { h[0] }"));
    assert!(y020("fn f(n: Int) -> Int { n[0] }"));
    assert!(y020("struct P { x: Int }\nfn f(p: P) -> Int { p[0] }"));

    // …and the right receiver at the wrong arity, in both directions.
    assert!(y020("fn f(g: Grid[Int]) -> Int { g[0] }"));
    assert!(y020("fn f(v: Vec[Int]) -> Int { v[0, 1] }"));

    // `.get` is untouched — the two spellings are two rows, and `Map`'s differ on
    // purpose (§4.7), so a fix that made one the other would show here.
    assert!(!has_type_error(
        "fn f(m: Map[Text, Int]) -> Int { m.get(\"a\") }"
    ));
}

/// **REP-16 through the constraint channel.** A subscript on an unannotated
/// parameter defers and is answered by the call site, exactly as `values.sum()`
/// is (TY-30) — because a subscript dispatches through the same catalog.
#[test]
fn a_subscript_on_an_unannotated_parameter_is_answered_by_the_call_site() {
    // The requirement rides on the scheme: `first` is generic in its receiver.
    assert!(!has_type_error(
        "fn first(m, k) { m[k] }\nfn main() -> Int { let m = Map()\n m.insert(\"a\", 1)\n first(m, \"a\") }"
    ));
    // …and the *element* type is the answer, not a fresh variable: a `Text` result
    // used as an `Int` is a mismatch.
    assert!(has_type_error(
        "fn first(m, k) { m[k] }\nfn main() -> Int { let m = Map()\n m.insert(\"a\", \"x\")\n first(m, \"a\") }"
    ));
    // A call site whose receiver does not index at all is reported when the
    // requirement is discharged rather than accepted in silence.
    assert!(has_type_error(
        "fn first(m, k) { m[k] }\nfn main() -> Int { let s = Set()\n s.insert(1)\n first(s, 1) }"
    ));
    // A subscript is exactly as generic as a method call and no more: the
    // requirement **pins** its receiver (`pin_to_level`, TY-30/ADR-057), so one
    // function serves one receiver kind. Two kinds through one function is a
    // `Y001` about the two signatures — the same answer `fn size(c) { c.len() }`
    // gives, which is what makes this a property of the channel rather than of
    // subscripts.
    assert!(has_type_error(
        "fn first(c, k) { c[k] }\n\
         fn main() -> Int { let m = Map()\n m.insert(\"a\", 1)\n \
         let v = Vec()\n v.push(2)\n first(m, \"a\") + first(v, 0) }"
    ));
    assert!(has_type_error(
        "fn size(c) { c.len() }\n\
         fn main() -> Int { let m = Map()\n m.insert(\"a\", 1)\n \
         let v = Vec()\n v.push(2)\n size(m) + size(v) }"
    ));
    // One kind at two call sites is fine, which is the half that has to keep
    // working.
    assert!(!has_type_error(
        "fn first(c, k) { c[k] }\n\
         fn main() -> Int { let a = Vec()\n a.push(1)\n \
         let b = Vec()\n b.push(2)\n first(a, 0) + first(b, 0) }"
    ));
}

/// **REP-16, the store form.** `m[key] = v` and `counts[key] += 1` reach the
/// three collections that have a store, and an assignment whose left side names
/// no storage is `Y021` rather than a parse error.
#[test]
fn a_store_through_a_subscript_needs_a_receiver_that_has_one() {
    let y020 = |src: &str| -> bool {
        analyze(src)
            .diagnostics
            .iter()
            .any(|d| d.kind() == praxis_source::DiagCode::NotIndexable)
    };
    let y021 = |src: &str| -> bool {
        analyze(src)
            .diagnostics
            .iter()
            .any(|d| d.kind() == praxis_source::DiagCode::NotAnAssignmentTarget)
    };

    // The three that store, plain and compound.
    assert!(!has_type_error("fn f(m: Map[Text, Int]) { m[\"a\"] = 1 }"));
    assert!(!has_type_error(
        "fn f(m: Map[Text, Int]) { m[\"a\"] = 1\n m[\"a\"] += 2 }"
    ));
    assert!(!has_type_error("fn f(c: Counter[Text]) { c[\"a\"] += 1 }"));
    assert!(!has_type_error("fn f(g: Grid[Int]) { g[0, 1] = 7 }"));

    // The stored type is checked against the collection's, in both positions.
    assert!(has_type_error(
        "fn f(m: Map[Text, Int]) { m[\"a\"] = \"x\" }"
    ));
    assert!(has_type_error("fn f(m: Map[Text, Int]) { m[0] = 1 }"));
    assert!(has_type_error("fn f(g: Grid[Int]) { g[0, 1] = \"x\" }"));

    // A `Vec` reads through a subscript and has **no element store anywhere in
    // the language** — no `v[0] = x`, and no `.set` either — so this is reported
    // rather than given one silently.
    assert!(y020("fn f(v: Vec[Int]) { v[0] = 1 }"));
    assert!(!y020("fn f(v: Vec[Int]) -> Int { v[0] }"));
    // Nor a `Text`, which is immutable.
    assert!(y020("fn f(t: Text) { t[0] = 1 }"));
    // And a receiver with no subscript at all is the same code from either side.
    assert!(y020("fn f(s: Set[Int]) { s[0] = 1 }"));

    // A left side that names no storage. Each of these used to be a parse error
    // about a missing statement separator, which said nothing about the mistake.
    assert!(y021("fn g() -> Int { 1 }\nfn f() { g() = 3 }"));
    assert!(y021("struct P { x: Int }\nfn f(p: P) { p.x = 3 }"));
    assert!(y021("fn f(v: Vec[Int]) { v.len() += 1 }"));
    // …and the target's own mistakes are still reported, so the statement is not
    // simply discarded.
    assert!(has_name_error("fn f() { nope() = 3 }"));

    // A plain `var` assignment is untouched: it is a different statement kind and
    // `parse_stmt` still routes it there.
    assert!(!has_type_error("fn f() { var x = 1\n x = 2\n x += 3 }"));
    assert!(!y021("fn f() { var x = 1\n x = 2 }"));

    // A compound store still requires a numeric value (TY-15/TY-31 through the
    // subscript): `m[k] += true` is the mistake `flag += false` was.
    assert!(has_type_error(
        "fn f(m: Map[Text, Bool]) { m[\"a\"] = true\n m[\"a\"] += true }"
    ));
}

/// **REP-09.** `Counter[(Int, Int)]()` parses, and it means what the annotation
/// says: the element type is the written one, and a use that disagrees is a
/// `Y001`.
///
/// §3.3 writes the explicit form. The element type is inferred from use as well,
/// so `Counter()` already worked and the design doc's own spelling did not — which
/// is why the assertions here are about the *type*, not about the absence of a
/// diagnostic.
#[test]
fn a_constructors_written_type_arguments_say_what_it_constructs() {
    // The written argument is the element type, at each arity the ctors have.
    assert_eq!(
        scheme_of("let c = Counter[(Int, Int)]()", "c"),
        Some("Counter[(Int, Int)]".to_string())
    );
    assert_eq!(
        scheme_of("let v = Vec[Text]()", "v"),
        Some("Vec[Text]".to_string())
    );
    assert_eq!(
        scheme_of("let m = Map[Text, Vec[Int]]()", "m"),
        Some("Map[Text, Vec[Int]]".to_string())
    );
    // Without the annotation the element stays quantified, which is the
    // difference the form exists to make.
    assert_eq!(
        scheme_of("let v = Vec[Int]()", "v"),
        Some("Vec[Int]".to_string())
    );

    // A use that agrees is clean; a use that disagrees is a `Y001`. This is the
    // pair that says the annotation *constrains* rather than decorates.
    assert!(!has_type_error(
        "fn main() -> Int { let c = Counter[Text]()\n c.inc(\"a\")\n c.len() }"
    ));
    assert!(has_type_error(
        "fn main() -> Int { let c = Counter[Text]()\n c.inc(1)\n c.len() }"
    ));
    assert!(has_type_error(
        "fn main() -> Int { let m = Map[Text, Int]()\n m.insert(\"a\", \"b\")\n m.len() }"
    ));
    // …and through the subscript the same annotation now enables (REP-16).
    assert!(!has_type_error(
        "fn main() -> Int { let c = Counter[(Int, Int)]()\n c[(1, 2)] += 1\n c[(1, 2)] }"
    ));
    assert!(has_type_error(
        "fn main() -> Int { let c = Counter[(Int, Int)]()\n c[\"a\"] += 1\n c.len() }"
    ));

    // The wrong *number* of arguments is `Y007` — the code a written
    // `Vec[Int, Text]` annotation already gets, because it is the same mistake in
    // a second position.
    let y007 = |src: &str| -> bool {
        analyze(src)
            .diagnostics
            .iter()
            .any(|d| d.kind() == praxis_source::DiagCode::WrongTypeArgumentCount)
    };
    assert!(y007("let v = Vec[Int, Text]()"));
    assert!(y007("let m = Map[Text]()"));
    assert!(!y007("let m = Map[Text, Int]()"));

    // A type argument is an annotation, so its own mistakes are annotation
    // mistakes: an unknown name is `N002` and a *value* in type position `N003`.
    assert!(has_name_error("let v = Vec[Nope]()"));
    assert!(has_name_error(
        "fn f() -> Int { let n = 1\n let v = Vec[n]()\n 0 }"
    ));

    // An annotated binding and an annotated constructor agree, which is the
    // property that says the two spellings are one type language.
    assert!(!has_type_error("let c: Counter[Text] = Counter[Text]()"));
    assert!(has_type_error("let c: Counter[Int] = Counter[Text]()"));
}

/// The parser's closed list of type-constructor names and the compiler's are the
/// same list (REP-09).
///
/// The parser has to know the names to tell `Counter[T]()` from `m[key]`, and it
/// cannot ask the compiler — it does not depend on `praxis-stdlib`. So there are
/// two copies, and this is the test that keeps them from drifting: a name in only
/// one is either a constructor whose type arguments do not parse, or a binding
/// name that can never be subscripted.
#[test]
fn the_parsers_type_constructors_are_the_compilers() {
    for name in praxis_parser::TYPE_CONSTRUCTOR_NAMES {
        assert!(
            crate::decl::is_type_ctor_name(name),
            "`{name}` takes type arguments in the parser but is not a compiler type constructor"
        );
    }
    // …and the other direction, over the closed set the compiler recognizes.
    for name in [
        "Vec", "Deque", "Map", "Set", "Counter", "MinHeap", "MaxHeap", "BitSet", "Grid", "Range",
        "Option",
    ] {
        assert!(crate::decl::is_type_ctor_name(name), "`{name}`");
        assert!(
            praxis_parser::TYPE_CONSTRUCTOR_NAMES.contains(&name),
            "`{name}` is a compiler type constructor whose type arguments do not parse"
        );
    }
}

/// **REP-19's typed-tree shape** (ADR-067). A file's top-level statements are
/// lowered into one generated item, in source order, and a file with none has no
/// such item.
///
/// §3.2 has always said this — "top-level statements are wrapped in a generated
/// entry function" — and nothing wrapped them: `lower` walked the root looking
/// only for `fn`/`struct`/`enum` and dropped everything else with a comment
/// saying M4 only JITs `fn` items. So `out(1)` at top level type-checked and
/// then vanished between the typed tree and MIR.
#[test]
fn a_files_top_level_statements_become_one_generated_item() {
    use praxis_ast::AstNode;
    let lowered = |text: &str| {
        let map = SourceMap::new();
        let id = map.intern("entry_test.px", text);
        let parsed = parse(id, text);
        let mut analysis = analyze_root(id, &parsed.tree);
        let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
        crate::lower::lower(id, &root, &mut analysis)
    };
    let entry_of = |module: &crate::TypedModule| -> Option<usize> {
        module.items.iter().find_map(|item| match item {
            crate::TypedItem::Fn(f) if f.name == crate::ENTRY_NAME => Some(f.body.stmts.len()),
            _ => None,
        })
    };

    // Three statements, one item — and the declarations between them stay their
    // own items, because a `fn` inside a `fn` is `N005`: the entry point cannot
    // be a source transformation that wraps the file.
    let module = lowered("out(1)\nfn f() -> Int { 2 }\nlet x = f()\nout(x)\n");
    assert_eq!(entry_of(&module), Some(3), "the three top-level statements");
    assert!(
        module
            .items
            .iter()
            .any(|item| matches!(item, crate::TypedItem::Fn(f) if f.name == "f")),
        "`f` is still its own item"
    );

    // A `struct` and an `enum` are type-only and contribute nothing to run, so a
    // file of declarations alone has no entry item at all — which is what leaves
    // `fn main` as the host's fallback rather than a competing rule.
    let module = lowered("struct P { x: Int }\nenum E { A }\nfn main() { out(1) }\n");
    assert_eq!(entry_of(&module), None);

    // The generated item is a nullary `Unit` function, which is what makes a
    // file have no *value*: `out(overlaps(segments, false))` is a statement and
    // not a result the host would print a second time.
    let module = lowered("out(1)\n");
    let entry = module
        .items
        .iter()
        .find_map(|item| match item {
            crate::TypedItem::Fn(f) if f.name == crate::ENTRY_NAME => Some(f),
            _ => None,
        })
        .expect("an entry item");
    assert!(entry.params.is_empty());
    assert_eq!(entry.body.stmts.len(), 1);
    assert!(matches!(
        entry.body.tail,
        crate::TypedExpr::Lit {
            value: crate::Lit::Unit,
            ..
        }
    ));

    // Its name is not an identifier, so the parser cannot produce a second
    // definition of it (ADR-064's rule for the subscript rows, at the one other
    // name the compiler mints into this namespace).
    assert!(
        !crate::ENTRY_NAME
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_'),
        "`{}` must not be spellable",
        crate::ENTRY_NAME
    );

    // `entry_point`'s rule, at the level it lives: the generated item wins, a
    // declared `main` is the fallback, and a file with neither has none.
    assert_eq!(
        crate::entry_point(|n| n == crate::ENTRY_NAME || n == "main"),
        Some(crate::ENTRY_NAME)
    );
    assert_eq!(crate::entry_point(|n| n == "main"), Some("main"));
    assert_eq!(crate::entry_point(|_| false), None);
}

/// **REP-22.** A `fn` body that names a binding declared outside it is reported
/// (`N007`, ADR-068) rather than compiling and answering wrongly.
///
/// ```praxis
/// let x = 1
/// fn f() { x }
/// out(f())          // Unit
/// ```
///
/// It passed `praxis check` and printed `Unit`: the binding is a local of
/// whatever function encloses it, and a `fn` body has no slot for another
/// function's local. Through a closure it was worse — `fn g() { |n| n + x }`
/// captured a symbol with no slot, so `g()(1)` printed a nine-digit number.
///
/// The boundary is a **`fn` body**, not a closure body, because a closure *does*
/// capture (§4.10) and a function does not (§4.9). That asymmetry is the whole
/// check, and both halves of it are asserted here.
#[test]
fn a_fn_that_reads_a_binding_around_it_is_reported() {
    let reports_n007 = |src: &str| {
        analyze_and_lower_diags(src)
            .iter()
            .any(|d| d.kind() == praxis_source::DiagCode::FunctionReadsOuterBinding)
    };

    // Both forms, and the closure one is the one that answered with garbage.
    assert!(reports_n007("let x = 1\nfn f() -> Int { x }\nout(f())\n"));
    assert!(reports_n007(
        "let x = 5\nfn g() { |n| n + x }\nout(g()(1))\n"
    ));
    // A `var` and a read through an assignment target, so it is the binding kind
    // that decides and not the expression position.
    assert!(reports_n007("var x = 1\nfn f() -> Int { x }\n"));
    assert!(reports_n007("var x = 1\nfn f() { x = 2 }\n"));
    // Nested one level deeper than the body itself: the boundary is the
    // function, not the block.
    assert!(reports_n007(
        "let x = 1\nfn f() -> Int { if true { x } else { 0 } }\n"
    ));

    // A closure at the **top level** captures, and must keep doing so: after
    // ADR-067 both it and the binding are inside the generated entry, so there
    // is no boundary between them. This is §4.10's own example.
    assert!(!reports_n007(
        "let offset = 10\nlet v = Vec()\nv.push(1)\nout(v.map(|x| x + offset).sum())\n"
    ));
    // …and a closure inside a `fn` capturing that `fn`'s own locals is the same
    // rule from the other side.
    assert!(!reports_n007(
        "fn f(v) { let k = 10\n v.map(|x| x + k).sum() }\n"
    ));

    // Everything that is *not* a binding stays reachable from anywhere — which
    // is what makes this a check on the symbol's kind and not on where it was
    // declared alone.
    assert!(!reports_n007(
        "fn helper() -> Int { 1 }\nfn f() -> Int { helper() }\n"
    ));
    assert!(!reports_n007(
        "struct P { x: Int }\nfn f() -> Int { P { x: 1 }.x }\n"
    ));
    assert!(!reports_n007(
        "enum E { A, B }\nfn f() -> Int { match A { A => 1, B => 2 } }\n"
    ));
    assert!(!reports_n007("fn f(n: Int) -> Int { abs(n) }\n"));
    // A parameter and a local of the function itself, and recursion.
    assert!(!reports_n007(
        "fn f(n: Int) -> Int { let m = n + 1\n if n < 1 { m } else { f(n - 1) } }\n"
    ));

    // One report per use site, and no cascade: the reference is still recorded,
    // so inference types the body as written and adds nothing.
    let diags = analyze_and_lower_diags("let x = 1\nfn f() -> Int { x + x }\n");
    assert_eq!(
        diags
            .iter()
            .filter(|d| d.kind() == praxis_source::DiagCode::FunctionReadsOuterBinding)
            .count(),
        2,
        "one per use, and {diags:?} has nothing else"
    );
    assert_eq!(diags.len(), 2, "no cascade: {diags:?}");

    // A binding declared *after* the function is `N001`, not this: only `fn`,
    // `struct` and `enum` are pre-registered for forward reference, so the name
    // is genuinely not in scope yet and there is no binding to have crossed a
    // boundary. Saying which code it is keeps the two from being confused later.
    let forward = analyze_and_lower_diags("fn f() -> Int { x }\nlet x = 1\n");
    assert!(forward
        .iter()
        .any(|d| d.kind() == praxis_source::DiagCode::UnknownName));
    assert!(!forward
        .iter()
        .any(|d| d.kind() == praxis_source::DiagCode::FunctionReadsOuterBinding));
}

/// **REP-10.** A record pattern binds each field it names at *that field's*
/// type, and a tuple pattern binds each element at that element's.
///
/// `match p { P { x, y } => x }` was a `P001` and there was no way to take a
/// record or a tuple apart in a pattern at all. The assertion is the *types*
/// rather than the absence of a diagnostic: a pattern that bound every name at
/// the scrutinee's own type would also be clean, and it would be wrong at the
/// first arithmetic.
///
/// The record's fields differ in type on purpose, so binding by name is
/// observable — a lowering that paired fields by position rather than by name
/// would type `tag` as `Int` here.
#[test]
fn a_record_pattern_binds_a_field_at_the_fields_own_type() {
    const DECL: &str = "struct P { x: Int, tag: Text }\nlet p = P { x: 1, tag: \"a\" }\n";

    // Punned: `P { x }` binds `x` to the field `x`.
    let src = format!("{DECL}let r = match p {{ P {{ x, tag }} => x }}\n");
    assert_eq!(scheme_of(&src, "x").as_deref(), Some("Int"));
    assert_eq!(scheme_of(&src, "tag").as_deref(), Some("Text"));

    // Explicit: `P { x: n }` binds `n` to the field `x`, whatever it is called.
    let src = format!("{DECL}let r = match p {{ P {{ tag: s, x: n }} => n }}\n");
    assert_eq!(scheme_of(&src, "n").as_deref(), Some("Int"));
    assert_eq!(
        scheme_of(&src, "s").as_deref(),
        Some("Text"),
        "the field's type, not the position's — the fields are written swapped"
    );

    // A field the pattern does not name is simply not bound; naming fewer is
    // legal, which is HIR-06's padding rule at a second kind of composite.
    assert!(is_clean_with_lower(&format!(
        "{DECL}let r = match p {{ P {{ x }} => x }}\n"
    )));

    // The mistakes, each at the code the *literal* form already spends: the
    // record does not have that field, or the pattern names one twice — where
    // the second sub-pattern would silently replace the first.
    let diags = analyze_and_lower_diags(&format!("{DECL}let r = match p {{ P {{ z }} => 1 }}\n"));
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y114"),
        "expected Y114, got {diags:?}"
    );
    let diags = analyze_and_lower_diags(&format!(
        "{DECL}let r = match p {{ P {{ x, x: q }} => 1 }}\n"
    ));
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y115"),
        "expected Y115, got {diags:?}"
    );

    // The head is a *type* name, so it is checked twice over: against the
    // scrutinee, and against being a record at all.
    let diags = analyze_and_lower_diags(
        "struct P { x: Int }\nstruct Q { y: Int }\n\
         let p = P { x: 1 }\nlet r = match p { Q { y } => y }\n",
    );
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y001"),
        "a pattern for another record is a mismatch: {diags:?}"
    );
    let diags = analyze_and_lower_diags("let n = 1\nlet r = match n { Nope { y } => y }\n");
    assert!(
        diags.iter().any(|d| d.code().to_string() == "N001"),
        "an undefined head is an undefined name: {diags:?}"
    );
    let diags = analyze_and_lower_diags("let n = 1\nlet r = match n { Int { y } => y }\n");
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y123"),
        "a head that is not a record has no fields to match: {diags:?}"
    );
}

/// **REP-10.** A tuple pattern binds by position, and the scrutinee it is
/// matched against has to be a tuple of that arity.
#[test]
fn a_tuple_pattern_binds_by_position() {
    // Two differently-typed elements, so a pattern that bound both at one type
    // — or paired them the other way round — is a different answer.
    let src = "let t = (1, \"a\")\nlet r = match t { (n, s) => n }\n";
    assert_eq!(scheme_of(src, "n").as_deref(), Some("Int"));
    assert_eq!(scheme_of(src, "s").as_deref(), Some("Text"));

    // Nested, and mixed with the other composite forms.
    let src = "struct P { x: Int, tag: Text }\n\
               let t = (P { x: 1, tag: \"a\" }, 2)\n\
               let r = match t { (P { x, tag }, k) => x + k }\n";
    assert_eq!(scheme_of(src, "x").as_deref(), Some("Int"));
    assert_eq!(scheme_of(src, "tag").as_deref(), Some("Text"));
    assert_eq!(scheme_of(src, "k").as_deref(), Some("Int"));

    // A tuple pattern *pins* an unresolved scrutinee rather than accepting it:
    // the elements are fresh variables unified through the scrutinee's type.
    let scheme = scheme_of("fn first(t) { match t { (a, b) => a } }\n", "first")
        .expect("first has a scheme");
    insta::assert_snapshot!(scheme, @"forall T U. ((T, U)) -> T");

    // The shapes that do not fit. A non-tuple and a wrong arity are both the
    // ordinary mismatch, reported where the pattern is written.
    for src in [
        "let n = 1\nlet r = match n { (a, b) => a }\n",
        "let t = (1, 2)\nlet r = match t { (a, b, c) => a }\n",
    ] {
        let diags = analyze_and_lower_diags(src);
        assert!(
            diags.iter().any(|d| d.code().to_string() == "Y001"),
            "{src} must report a mismatch, got {diags:?}"
        );
    }

    // `(p)` is a one-element tuple pattern and there is no such type — the
    // parser has no grouping form for it to have meant, so it reports rather
    // than quietly matching whatever is inside.
    let diags = analyze_and_lower_diags("let n = 1\nlet r = match n { (a) => a }\n");
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y123"),
        "expected Y123, got {diags:?}"
    );
}

/// **REP-10's exit criterion.** A record and a tuple have **one constructor**,
/// so a `match` on one is exhaustive without a `_`.
///
/// Both were `Open` before, and only because no pattern could name them: the
/// matrix has always handled a `Closed` signature with a single constructor —
/// `exhaustive.rs` needed the two `Ctor` rows and nothing else.
#[test]
fn a_record_or_tuple_match_is_exhaustive_without_a_catch_all() {
    const DECL: &str = "struct P { x: Int, y: Int }\nlet p = P { x: 1, y: 2 }\nlet t = (1, 2)\n";

    // One arm, no `_`, and it covers everything.
    for arm in [
        "match p { P { x, y } => x + y }",
        "match p { P { x } => x }",
        "match p { P { x: a, y: b } => a }",
        "match t { (a, b) => a + b }",
        "match t { (a, _) => a }",
    ] {
        assert!(
            is_clean_with_lower(&format!("{DECL}let r = {arm}\n")),
            "{arm} must be exhaustive on its own"
        );
    }

    // …so a `_` after it is now *unreachable*, which is the other half of the
    // same fact and the regression a signature that stayed `Open` would hide.
    let diags = analyze_and_lower_diags(&format!(
        "{DECL}let r = match p {{ P {{ x, y }} => x, _ => 0 }}\n"
    ));
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y121"),
        "expected Y121, got {diags:?}"
    );

    // A component that does *not* cover its own type leaves the match
    // non-exhaustive, and the witness names the shape rather than a bare `_`:
    // the recursion goes through the new constructors, it does not stop at them.
    for (src, missing) in [
        (
            format!("{DECL}let r = match p {{ P {{ x: 1, y }} => y }}\n"),
            "P { x: _, y: _ }",
        ),
        (
            format!("{DECL}let r = match t {{ (1, b) => b }}\n"),
            "(_, _)",
        ),
    ] {
        let diags = analyze_and_lower_diags(&src);
        let y120 = diags
            .iter()
            .find(|d| d.code().to_string() == "Y120")
            .unwrap_or_else(|| panic!("expected Y120, got {diags:?}"));
        assert!(
            y120.message().contains(missing),
            "the witness must name `{missing}`: {}",
            y120.message()
        );
    }

    // An enum whose payload is a record or a tuple is exhaustive when the
    // payload's own components are covered — the two new constructors recurse
    // like a variant's, which is the whole of HIR-06 at a second shape.
    assert!(is_clean_with_lower(
        "struct P { x: Int, y: Int }\n\
         let o = Some(P { x: 1, y: 2 })\n\
         let r = match o { Some(P { x, y }) => x, None => 0 }\n"
    ));
}

/// **REP-21.** `min=` and `max=` are catalog rows of their own, on a `Map` whose
/// value type is bound to `Int`.
///
/// §6.2 writes `distance[key] min= candidate` and says "an absent entry accepts
/// the first value" — a semantics no read-modify-write over the subscript rows
/// can express, because a subscript read of an absent key *faults* (§4.7). So
/// they are rows, not desugarings, and the type rule is the row's.
#[test]
fn an_updating_store_is_a_row_on_a_map_of_ints() {
    // The value is an `Int` because the wrapper compares through `int_payload`,
    // and the bound **pins** it rather than merely permitting it: an
    // unannotated `Map()` becomes a `Map[?K, Int]` at the first `min=`.
    assert!(is_clean_with_lower(
        "let d = Map()\nd[\"a\"] min= 5\nd[\"a\"] min= 3\nout(d[\"a\"])\n"
    ));
    assert_eq!(
        expr_type("{ let d = Map()\n d[\"a\"] min= 5\n d }"),
        "Map[Text, Int]",
        "the row's bound pins the value type"
    );

    // A value that is not an `Int` is the ordinary mismatch.
    let diags = analyze_and_lower_diags("let d = Map()\nd[\"a\"] = \"v\"\nd[\"a\"] min= \"w\"\n");
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y001"),
        "expected Y001, got {diags:?}"
    );

    // A receiver with no updating store is `Y020`, and the message names the
    // operator — a `Counter` *can* be assigned through one index, so "cannot be
    // assigned through 1 index" would be false about the very receiver this is
    // most likely to be written for.
    for (src, op) in [
        ("let c = Counter()\nc[\"k\"] min= 1\n", "min="),
        ("let g = Grid()\ng[0, 0] max= 1\n", "max="),
        ("let v = Vec()\nv.push(1)\nv[0] min= 1\n", "min="),
    ] {
        let diags = analyze_and_lower_diags(src);
        let y020 = diags
            .iter()
            .find(|d| d.code().to_string() == "Y020")
            .unwrap_or_else(|| panic!("expected Y020 for {src}, got {diags:?}"));
        assert!(
            y020.message().contains(op),
            "the message must name `{op}`: {}",
            y020.message()
        );
    }

    // A target that is not a place at all is `Y021`, as it is for every other
    // assignment operator.
    let diags = analyze_and_lower_diags("var x = 1\nx min= 2\n");
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y021"),
        "expected Y021, got {diags:?}"
    );

    // The receiver may be an unannotated parameter: the row defers through
    // `HasMethod` exactly as a method call does (TY-30), and the call site
    // answers it — in both directions.
    assert!(is_clean_with_lower(
        "fn relax(d, k, v) { d[k] min= v }\n\
         let dist = Map()\ndist[\"a\"] = 10\nrelax(dist, \"a\", 4)\nout(dist[\"a\"])\n"
    ));
    let diags = analyze_and_lower_diags(
        "fn relax(d, k, v) { d[k] min= v }\nlet c = Counter()\nrelax(c, \"a\", 4)\n",
    );
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y020"),
        "the requirement is answered at the call: {diags:?}"
    );

    // …and `min`/`max` are still the prelude's own functions, which is what the
    // contextual grammar rule exists to protect.
    assert_eq!(expr_type("min(3, 4) + max(3, 4)"), "Int");
}

/// **REP-25.** A `for` binding is a pattern, and it must match **every** item.
///
/// `for (k, v) in m` was unspellable: the header took one `Ident`, so a `Map`'s
/// pair could only be named and then read with `kv.0`/`kv.1`. ADR-066 decision 3
/// left the destructuring half to REP-10's grammar; this is that grammar in the
/// one other position a binding appears.
#[test]
fn a_for_binding_is_a_pattern_and_must_match_every_item() {
    // Each name binds at its own component's type, which a binding that named
    // the whole item could not do.
    let src = "let m = Map()\nm[\"a\"] = 1\nfor (k, v) in m { out(k) out(v) }\n";
    assert_eq!(scheme_of(src, "k").as_deref(), Some("Text"));
    assert_eq!(scheme_of(src, "v").as_deref(), Some("Int"));

    // A record pattern in the header, at the fields' own types.
    let src = "struct P { x: Int, tag: Text }\nlet ps = Vec()\nps.push(P { x: 1, tag: \"a\" })\n\
               for P { x, tag } in ps { out(x) out(tag) }\n";
    assert_eq!(scheme_of(src, "x").as_deref(), Some("Int"));
    assert_eq!(scheme_of(src, "tag").as_deref(), Some("Text"));

    // A bare name still binds the whole item — the overwhelmingly common shape,
    // and the one every existing program is written with.
    let src = "let v = Vec()\nv.push((1, 2))\nfor kv in v { out(kv.0) }\n";
    assert_eq!(scheme_of(src, "kv").as_deref(), Some("(Int, Int)"));

    // The pattern is checked against the element type like any other, so a shape
    // the item cannot have is the ordinary mismatch.
    let diags = analyze_and_lower_diags("let v = Vec()\nv.push(1)\nfor (a, b) in v { out(a) }\n");
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y001"),
        "expected Y001, got {diags:?}"
    );

    // **A binding has no second arm**, so a pattern that can fail is `Y125` —
    // both spellings, and at any depth.
    for src in [
        "let v = Vec()\nv.push(Some(1))\nfor Some(n) in v { out(n) }\n",
        "let v = Vec()\nv.push((1, 2))\nfor (1, b) in v { out(b) }\n",
        "let v = Vec()\nv.push((1, (2, 3)))\nfor (a, (2, c)) in v { out(c) }\n",
    ] {
        let diags = analyze_and_lower_diags(src);
        assert!(
            diags.iter().any(|d| d.code().to_string() == "Y125"),
            "{src} must be Y125, got {diags:?}"
        );
    }

    // …and the irrefutable shapes are all still accepted, including the wildcard
    // and a partial record pattern.
    for src in [
        "let v = Vec()\nv.push(1)\nfor _ in v { out(0) }\n",
        "let v = Vec()\nv.push((1, 2))\nfor (a, _) in v { out(a) }\n",
        "struct P { x: Int, y: Int }\nlet ps = Vec()\nps.push(P { x: 1, y: 2 })\n\
         for P { x } in ps { out(x) }\n",
    ] {
        assert!(is_clean_with_lower(src), "{src} must be accepted");
    }
}
