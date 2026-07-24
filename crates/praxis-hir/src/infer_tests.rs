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
