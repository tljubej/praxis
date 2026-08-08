//! Type-inference tests.
//!
//! These cover the §19-M2 acceptance criteria that inference owns: inferring
//! function parameter and return types from use (criterion 1), and rejecting
//! cross-type `var` reassignment (criterion 4). They also snapshot inferred
//! schemes/types (§17.1 "inference snapshots").

#![cfg(test)]

use praxis_source::DiagnosticCategory;

use crate::hir_tests::test_util::{
    analyze, analyze_and_lower, entry_fn, fn_named, parse_analyze_and_lower, parse_file,
};
use crate::{analyze_root, SymbolKind};

/// The rendered scheme of the user binding named `name` (a Var/Fn/Param), or
/// `None` if it has no scheme.
fn scheme_of(text: &str, name: &str) -> Option<String> {
    let analysis = analyze(text);
    analysis
        .names
        .all()
        .iter()
        .filter(|s| s.name == name && s.kind != SymbolKind::Builtin)
        .find_map(|s| s.scheme.as_ref().map(|sc| analysis.db.render_scheme(sc)))
}

/// The type of an expression, observed by binding it: `var _probe = <expr>`
/// and reading `_probe`'s scheme. Returns the rendered type.
fn expr_type(expr: &str) -> String {
    let src = format!("var _probe = {expr}");
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

/// Every error `text` produces, **lex and parse included**.
///
/// [`has_input_error`] only sees the `Input` category, and [`analyze`] only
/// runs the HIR pass — so a shape §7.5 documents could fail at the *grammar*
/// (`P001 expected a parser expression` on `grid(char, ragged, fill: 0)`) and
/// still pass an "is this accepted?" assertion written with either. "Accepted"
/// means no compiler says no.
fn errors_of(text: &str) -> Vec<String> {
    use praxis_source::Severity;
    let (id, parsed) = parse_file(text);
    parsed
        .diagnostics
        .iter()
        .chain(analyze_root(id, &parsed.tree).diagnostics.iter())
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| format!("{}: {}", d.code(), d.message()))
        .collect()
}

/// The `ParserAst` the first `read`/`parse` body in `src` converts to.
///
/// Much of a parser's meaning is invisible in the synthesized *type*: a decoded
/// separator, a capture's own parser, the shape of a `choice`'s cases. This is
/// how those are asserted.
fn parser_ast_of(src: &str) -> praxis_input_parser::ParserAst {
    use praxis_ast::AstNode;
    let (id, parsed) = parse_file(src);
    let pe = parsed
        .tree
        .descendants()
        .find_map(praxis_ast::ParserExpr::cast)
        .expect("a parser expression");
    let mut diagnostics = Vec::new();
    crate::parser_lower::convert_parser_expr_for_test(&pe, id, &mut diagnostics)
        .unwrap_or_else(|| panic!("{src} converts: {diagnostics:?}"))
}

/// The `fill` of the first `GridRagged` anywhere in `ast`, including under a
/// template capture — so one assertion covers both front ends.
fn ragged_fill_of(ast: &praxis_input_parser::ParserAst) -> Option<String> {
    use praxis_input_parser::{ParserAst, TemplatePart};
    match ast {
        ParserAst::GridRagged { fill, .. } => Some(fill.clone()),
        ParserAst::Template { parts, .. } => parts.iter().find_map(|p| match p {
            TemplatePart::Capture { parser, .. } => ragged_fill_of(parser),
            TemplatePart::Literal { .. } => None,
        }),
        _ => None,
    }
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
    let (analysis, module) = analyze_and_lower(text);
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
    let (analysis, module) = analyze_and_lower(text);
    analysis.diagnostics.is_empty() && module.diagnostics.is_empty()
}

/// Every diagnostic `analyze` + `lower` produce, in source order.
fn analyze_and_lower_diags(text: &str) -> Vec<praxis_source::Diagnostic> {
    let (analysis, module) = analyze_and_lower(text);
    let mut all = analysis.diagnostics;
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
    // `var a = 4` then `var a = "Foo"`: each binding keeps its own type.
    let src = "var a = 4\nvar a = \"Foo\"";
    let analysis = analyze(src);
    let a_schemes: Vec<_> = analysis
        .names
        .all()
        .iter()
        .filter(|s| s.name == "a" && s.kind == SymbolKind::Var)
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

/// A mismatch names the *requirement* as `expected` and what the program wrote
/// as `found` — at every site that reports one.
///
/// `TypeDb::unify` builds `Mismatch { expected, found }` in argument order, so
/// every caller must pass what the context requires first and what the program
/// wrote second.
///
/// The last row is the control. `var` reassignment is oriented `unify(existing,
/// rhs)`, so flipping the orientation inside `unify` itself rather than at the
/// call sites turns this one red.
#[test]
fn a_mismatch_names_the_requirement_as_expected_and_the_program_as_found() {
    for (src, want) in [
        // Binary arithmetic: the operator requires Int, the operand is a Bool.
        // Deliberately not a `Text` operand — a `Text` beside `+` is
        // concatenation (ADR-085), not a mismatch.
        ("var a = 1 + true", "expected Int, found Bool"),
        // Range bounds are Int only (ADR-059).
        ("var r = \"a\"..2", "expected Int, found Text"),
        // Unary `!` requires Bool, unary `-` requires a number.
        ("var c = !1", "expected Bool, found Int"),
        ("var c = -\"x\"", "expected Int, found Text"),
        // `parse(text, parser)` requires Text in its first argument.
        ("var v = parse(1, int)", "expected Text, found Int"),
        // The control: `var` reassignment is oriented `unify(existing, rhs)`.
        ("var x = 0\nx = \"hi\"", "expected Int, found Text"),
    ] {
        let errors = errors_of(src);
        assert!(
            errors.iter().any(|e| e.contains(want)),
            "`{src}` should report `{want}`, got {errors:?}"
        );
    }
}

/// **ADR-089.** A call with the wrong argument count names the counts (`Y024`),
/// rather than diffing two whole function types as a `Y001`.
///
/// A name in Praxis has exactly one signature — no arity-based overloading, no
/// optional or default parameters — so a count mismatch is never a candidate
/// for another overload and can be reported as the arithmetic mistake it is.
///
/// **`assert` is the first row because it is ADR-089's motivating case** — the
/// question was whether it should take a message, and the answer is that the
/// language does not have the mechanism a second signature would need.
#[test]
fn a_call_with_the_wrong_argument_count_names_the_counts() {
    for (src, want) in [
        // The motivating case: `assert` takes a condition, and the name that
        // carries words is `panic` (ADR-089 decision 2).
        (
            "assert(1 == 1, \"why\")",
            "this function takes 1 argument(s), but 2 were given",
        ),
        // A user function, both directions.
        (
            "fn add(a, b) { a + b }\nvar n = add(1)",
            "this function takes 2 argument(s), but 1 were given",
        ),
        (
            "fn one(a) { a }\nvar n = one(1, 2)",
            "this function takes 1 argument(s), but 2 were given",
        ),
        // A closure, which reaches the same `Func`-vs-`Func` unification — the
        // reason the error is raised in `unify` and not in `infer_call`.
        (
            "var f = |a, b| a + b\nvar n = f(1)",
            "this function takes 2 argument(s), but 1 were given",
        ),
    ] {
        let errors = errors_of(src);
        assert!(
            errors.iter().any(|e| e.contains(want)),
            "`{src}` should report `{want}`, got {errors:?}"
        );
    }
}

/// **ADR-085.** A `Text` operand makes `+` concatenation, and makes every other
/// arithmetic operator a `Y016`.
///
/// The rule is §4.12's own — "a Float operand makes the operation Float, an Int
/// operand makes it Int" — with a third type in it, which is why the mixed cases
/// are errors rather than coercions: `"a" + 1` gets the same answer `1 + 2.5`
/// gets, and for the same reason.
#[test]
fn a_text_operand_makes_plus_concatenation_and_nothing_else_legal() {
    // `+` on two Texts is a Text, in every spelling the operator has.
    assert_eq!(expr_type("\"a\" + \"b\""), "Text");
    assert_eq!(expr_type("\"a\" + \"b\" + \"c\""), "Text");
    assert!(!has_type_error(
        "var a = \"x\"\nvar b = \"y\"\nvar c = a + b"
    ));
    // …including through a binding whose type inference derived rather than read.
    assert_eq!(
        scheme_of("var a = \"x\"\nvar b = \"y\"\nvar c = a + b", "c").as_deref(),
        Some("Text")
    );
    // `+=` is the same operator, so a Text target needs no number (the
    // compound-assignment path requires `Numeric` for the other four).
    assert!(!has_type_error("var s = \"a\"\ns += \"b\""));
    assert!(has_type_error("var s = \"a\"\ns -= \"b\""));
    assert!(has_type_error("var n = 0\nn += \"b\""));

    // Nothing else is defined for Text. `Y016` is the code for an operation
    // whose operands agree but which has no meaning for that type — the same
    // code `%` on `Float` reports.
    for src in [
        "var x = \"a\" - \"b\"",
        "var x = \"a\" * \"b\"",
        "var x = \"a\" / \"b\"",
        "var x = \"a\" % \"b\"",
        // The repetition spelling other languages have. It is not one here, and
        // refusing it now is what keeps it free.
        "var x = \"ab\" * 3",
    ] {
        let errors = errors_of(src);
        assert!(
            errors.iter().any(|e| e.contains("Y016")),
            "`{src}` should be a Y016, got {errors:?}"
        );
    }

    // No implicit conversion, in either direction or either position.
    assert!(has_type_error("var x = \"a\" + 1"));
    assert!(has_type_error("var x = 1 + \"a\""));
    assert!(has_type_error("var x = \"a\" + 1.5"));
    assert!(has_type_error("var x = \"a\" + true"));

    // The recorded limitation (ADR-085): two unconstrained operands still
    // default to `Int`, because the target follows the operands' *known* types
    // and a type variable is not `Text`. Pinned so the day it changes is
    // deliberate rather than accidental.
    assert_eq!(
        scheme_of("fn f(a, b) { a + b }", "f").as_deref(),
        Some("(Int, Int) -> Int")
    );
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

// --- tuples ----------------------------------------------------------------

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

// --- generalization -------------------------------------------------------

#[test]
fn let_int_binding_is_monotype() {
    // A concrete binding is monomorphic.
    assert_eq!(scheme_of("var x = 1", "x").unwrap(), "Int");
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

// --- input-parser type synthesis (§7.8) ------------------------------------

#[test]
fn read_atomic_synthesizes_scalar_type() {
    // `read int` → Int; `read char` → Char.
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
    let src = "fn f(sample: Text) { var v = parse(sample, lines(int)); v }";
    assert!(!has_type_error(src));
}

#[test]
fn read_in_fn_then_method_call_typechecks() {
    // Full pipeline: read inside a fn, then call .len() on the result.
    let src = "fn main() -> Int {\n  var v = read lines(int)\n  v.len()\n}\n";
    let analysis = analyze(src);
    assert!(
        !has_type_error(src),
        "type errors: {:?}",
        analysis.diagnostics
    );
}

// --- structural equality capability (§5.5) ----------------------------------

#[test]
fn record_equality_typechecks() {
    // `==` on two records of the same type typechecks cleanly (no Y004).
    let src = "struct Point { x: Int, y: Int }\nfn main() -> Int {\n  var a = Point { x: 1, y: 2 }\n  var b = Point { x: 1, y: 2 }\n  if a == b { 1 } else { 0 }\n}\n";
    assert!(!has_type_error(src));
}

#[test]
fn tuple_equality_typechecks() {
    // `==` on two tuples of the same shape typechecks cleanly.
    let src =
        "fn main() -> Int {\n  var a = (1, 2)\n  var b = (1, 2)\n  if a == b { 1 } else { 0 }\n}\n";
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
    let src = "struct Box { f: (Int) -> Int }\nfn id(x: Int) -> Int { x }\nfn main() -> Int {\n  var a = Box { f: id }\n  var b = Box { f: id }\n  if a == b { 1 } else { 0 }\n}\n";
    assert!(has_type_error(src));
}

#[test]
fn int_equality_still_typechecks() {
    // `==` on Int typechecks.
    assert!(!has_type_error(
        "fn main() -> Int {\n  if 3 == 3 { 1 } else { 0 }\n}\n"
    ));
}

// --- exhaustiveness checking (Y120/Y121) ------------------------------------

#[test]
fn non_exhaustive_enum_match_is_rejected() {
    // Missing the Wall variant → Y120.
    let src = "enum Tile { Empty, Wall, Number(Int) }\nfn main() -> Int {\n  var t = Empty\n  match t {\n    Empty => 1\n    Number(n) => n\n  }\n}\n";
    assert!(has_type_error_with_lower(src));
}

#[test]
fn exhaustive_enum_match_is_ok() {
    // All three variants covered → no error.
    let src = "enum Tile { Empty, Wall, Number(Int) }\nfn main() -> Int {\n  var t = Empty\n  match t {\n    Empty => 1\n    Wall => 2\n    Number(n) => n\n  }\n}\n";
    assert!(!has_type_error_with_lower(src));
}

#[test]
fn enum_match_with_wildcard_is_ok() {
    // Wildcard catches remaining variants → exhaustive.
    let src = "enum Tile { Empty, Wall, Number(Int) }\nfn main() -> Int {\n  var t = Empty\n  match t {\n    Empty => 1\n    _ => 0\n  }\n}\n";
    assert!(!has_type_error_with_lower(src));
}

#[test]
fn int_match_without_wildcard_is_rejected() {
    // Int has infinitely many values; a literal-only match needs `_` → Y120.
    let src = "fn main() -> Int {\n  var n = 1\n  match n {\n    1 => 10\n    2 => 20\n  }\n}\n";
    assert!(has_type_error_with_lower(src));
}

#[test]
fn int_match_with_wildcard_is_ok() {
    // Int match with `_` → exhaustive.
    let src = "fn main() -> Int {\n  var n = 1\n  match n {\n    1 => 10\n    2 => 20\n    _ => 0\n  }\n}\n";
    assert!(!has_type_error_with_lower(src));
}

#[test]
fn bool_match_without_both_cases_is_rejected() {
    // Bool match missing `false` → Y120.
    let src = "fn main() -> Int {\n  var b = true\n  match b {\n    true => 1\n  }\n}\n";
    assert!(has_type_error_with_lower(src));
}

#[test]
fn bool_match_both_cases_is_ok() {
    // Both true and false covered → exhaustive.
    let src =
        "fn main() -> Int {\n  var b = true\n  match b {\n    true => 1\n    false => 0\n  }\n}\n";
    assert!(!has_type_error_with_lower(src));
}

#[test]
fn arm_after_wildcard_is_unreachable() {
    // An arm after `_` is unreachable → Y121 (a type-category diagnostic).
    let src = "enum Tile { Empty, Wall }\nfn main() -> Int {\n  var t = Empty\n  match t {\n    _ => 0\n    Empty => 1\n  }\n}\n";
    assert!(has_type_error_with_lower(src));
}

// --- closure type inference (§4.10) -----------------------------------------

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
    // A closure that captures an outer variable: `var o = 10; var f = |x| x + o`.
    let src = "fn main() -> Int {\n  var o = 10\n  var f = |x| x + o\n  0\n}\n";
    assert!(!has_type_error(src));
}

#[test]
fn closure_with_typed_param_typechecks() {
    // `|x: Int| x + 1` with an explicit param type.
    let src = "fn main() -> Int {\n  var f = |x: Int| x + 1\n  0\n}\n";
    assert!(!has_type_error(src));
}

#[test]
fn closure_by_value_capture_lowers_clean() {
    // The headline by-value-capture pipeline lowers without diagnostics. Nothing
    // reassigns `o`, so it is copied into the environment (ADR-125).
    let src = "fn main() -> Int {\n  var o = 10\n  var f = |x| x + o\n  f(5)\n}\n";
    assert!(!has_type_error_with_lower(src));
}

#[test]
fn mutable_capture_now_supported() {
    // A capture of a reassigned binding is supported (boxed into a `VarCell`,
    // ADR-027). It lowers without diagnostics.
    let src = "fn main() -> Int {\n  var c = 0\n  var f = |_| c\n  f(0)\n}\n";
    assert!(
        !has_type_error_with_lower(src),
        "mutable capture should be supported (WS7b)"
    );
}

// ===========================================================================
// Diagnostic span precision.
//
// A type mismatch must point at the offending sub-expression, not the enclosing
// statement or function. These tests pin the primary span's byte range to the
// exact expression at fault, so a change that re-coarsens the span fails loudly.
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
    // `out("kurac")` returns `Unit` but `main` is declared `-> Int`. The error
    // must underline `out("kurac")`, not the whole `fn main() -> Int { … }`.
    let src = "fn main() -> Int {\n    var depths = read lines(int)\n    out(\"kurac\")\n}\n";
    let expected = span_of(src, "out(\"kurac\")");
    let actual = first_mismatch_span(src).expect("expected a Y001 mismatch");
    assert_eq!(
        actual, expected,
        "return-type mismatch should point at the tail expression, got {actual:?} (expected {expected:?})",
    );
}

#[test]
fn let_annotation_mismatch_points_at_initializer() {
    // `var x: Int = "hello"` — the error underlines `"hello"`, not the whole binding.
    let src = "fn main() -> Int {\n    var x: Int = \"hello\"\n    0\n}\n";
    let expected = span_of(src, "\"hello\"");
    let actual = first_mismatch_span(src).expect("expected a Y001 mismatch");
    assert_eq!(
        actual, expected,
        "var-annotation mismatch should point at the initializer, got {actual:?}",
    );
}

#[test]
fn arithmetic_operand_mismatch_points_at_bad_operand() {
    // The error underlines the offending *operand*, not the whole binary
    // expression — and the span is the operand exactly, with no leading
    // whitespace in it.
    //
    // In `s + 1` over a `Text` `s`, the offender is `1`: a `Text` operand makes
    // `+` concatenation (ADR-085), so `s` is the one that fits — the same rule
    // that makes `1` the offender in `1 + 2.5`.
    //
    // Bound rather than left as the tail expression: a tail carries the
    // function's return type too, and `-> Int` over a `Text` concatenation adds
    // a *second* mismatch at the whole `s + 1`. That one is correct and is not
    // this test's subject.
    let src = "fn main() -> Int {\n    var s = \"hi\"\n    var t = s + 1\n    0\n}\n";
    let expected = span_of(src, "1\n    0");
    let expected = (expected.0, expected.0 + 1);
    let actual = first_mismatch_span(src).expect("expected a Y001 mismatch");
    assert_eq!(
        actual, expected,
        "the mismatch should point at `1`, the operand a concatenation cannot take, got {actual:?}",
    );

    // A shape no rule reinterprets: `+` on an Int and a Bool is Int arithmetic
    // either way, and `true` is the operand at fault.
    let src = "fn main() -> Int {\n    var n = 1\n    var t = n + true\n    0\n}\n";
    let expected = span_of(src, "true");
    let actual = first_mismatch_span(src).expect("expected a Y001 mismatch");
    assert_eq!(
        actual, expected,
        "the mismatch should point at `true`, got {actual:?}",
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
// Adversarial front-end contracts.
//
// These tests pin semantic contracts that cross AST accessors, resolution,
// inference, and typed-HIR lowering.
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
    // The initializer is an Int on purpose: initializing the field with a
    // function would type-check whether or not the annotation survived the
    // accessor, so only a mismatching initializer proves the annotation arrived.
    let src = "struct Box { f: (Int) -> Int }\n\
               fn main() -> Int { var value = Box { f: 1 }; 0 }";
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

/// The tests above all ask that a wrong use is *rejected*; this asks that the
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
        scheme_of("var p: (Int, Text) = (1, \"a\")", "p").as_deref(),
        Some("(Int, Text)"),
        "a tuple-annotated `var`"
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

/// `()` in a parameter group is *no* parameters, not one invented one.
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

/// The rule stated positively. A rejection test only asks that a `Tile`
/// parameter is not an `Int`, which a fresh variable also satisfies once
/// *something* pins it; this asks that the annotation *is* the named type.
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

/// Type declarations are registered in *dependency* order, not "types first".
/// A struct whose field names a struct declared below it needs the second
/// registered before the first, and neither is a `fn`.
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
/// A cycle member is not registered silently with a fresh variable: each one is
/// reported as `N006` (ADR-063), exactly once, which the count below pins.
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
    let src = "var Alias = 1\nvar value: Alias = \"text\"";
    assert!(
        has_name_error(src),
        "ordinary value bindings are not type declarations"
    );
}

/// Every value kind is rejected in type position, and the report is `N003` —
/// the name *is* known, so `N002 unknown type` would be a lie about which
/// mistake was made.
#[test]
fn a_value_in_type_position_is_reported_as_a_value() {
    for src in [
        "var Alias = 1\nvar value: Alias = 1",
        "var Alias = 1\nvar value: Alias = 1",
        "fn Alias() -> Int { 1 }\nvar value: Alias = 1",
        // A prelude value builtin: `out` resolves as a name, but is not a type.
        "var value: out = 1",
        // …at any depth inside a structural annotation.
        "var Alias = 1\nfn f(x: (Int, Alias)) -> Int { 0 }",
        "var Alias = 1\nfn f(x: Vec[Alias]) -> Int { 0 }",
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
        "var value: Int = 1",
        "var value: Text = \"a\"",
        "struct Point { x: Int }\nvar value: Point = Point { x: 1 }",
        "enum Tile { Empty }\nvar value: Tile = Empty",
        "var value: Vec[Int] = Vec()",
        "var value: Map[Text, Int] = Map()",
        "var value: Option[Int] = Some(1)",
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

/// There is no immutable binding form (ADR-125), so every binding is
/// assignable. That the assignment is still *type-checked* is
/// `local_var_reassignment_preserves_its_type` directly above.
#[test]
fn a_binding_is_assignable() {
    assert!(is_clean_with_lower(
        "fn main() -> Int { var x = 1; x = 2; x }"
    ));
}

/// **ADR-125's soundness gate.** §5.3's generalization rule is keyed on
/// `Symbol::reassigned` rather than on a binding keyword: there is only one
/// binding form, so what a binding *is* cannot carry the rule and what is done
/// to it must.
///
/// The failure it prevents is not a lost error message. Assignment
/// *instantiates* the target's scheme and unifies the copy, so a generalized
/// binding is not constrained by being written: `var f = |x| x` is a syntactic
/// value and generalizes to `forall T. T -> T`, and `f = |n| n + 1` would leave
/// it there — after which `f("s")` type-checks and calls the `Int` closure with
/// a `Text`. That is a wrong-type call the backend emits, not a diagnostic that
/// went missing.
#[test]
fn a_reassigned_binding_is_not_generalized() {
    assert!(
        has_type_error("var f = |x| x\nf = |n| n + 1\nout(f(\"s\"))\n"),
        "a reassigned binding must stay monomorphic, or the second use picks its own instance"
    );
    // …and the binding nothing writes is still generalized, which is the half
    // that would silently regress every polymorphic binding if the gate were
    // simply "never generalize".
    assert!(
        is_clean_with_lower("var id = |x| x\nout(id(1))\nout(id(\"two\"))\n"),
        "a binding nothing writes is used at two types, exactly as a `let` was"
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

/// An assignment constrains the binding the resolver picked for its left-hand
/// side, and no other — so a local shadowing a top-level name of the same
/// spelling constrains the local.
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
            "fn f() -> Int { var total = 0; var add = |n| { total += n }; add(1); total }"
        ),
        "a captured var may be assigned"
    );
    assert!(
        has_type_error("fn f() -> Int { var total = 0; var add = |n| { total = \"x\" }; 0 }"),
        "…and the capture is checked"
    );
}

/// ADR-125: a `var`, a parameter, a `for` variable and a name a pattern
/// introduces are one thing, and all four are writable.
///
/// The paired negative is the point of the test, not an afterthought — "it is
/// accepted" alone would also pass if assignment had stopped being checked at
/// all. Each source is asserted clean **and** its type-violating twin rejected,
/// so the property is "the type is what constrains a write", not "nothing does".
#[test]
fn every_binding_kind_is_assignable_and_still_type_checked() {
    for (ok, bad) in [
        (
            "fn f() -> Int { var x = 1; x = 2; x }",
            "fn f() -> Int { var x = 1; x = \"two\"; x }",
        ),
        (
            "fn f(p: Int) -> Int { p = 2; p }",
            "fn f(p: Int) -> Int { p = \"two\"; p }",
        ),
        (
            "fn f(v: Vec[Int]) -> Int { for x in v { x = 1 }; 0 }",
            "fn f(v: Vec[Int]) -> Int { for x in v { x = \"one\" }; 0 }",
        ),
        (
            "enum E { N(Int) }\nfn f(e: E) -> Int { match e { N(n) => { n = 1; n } } }",
            "enum E { N(Int) }\nfn f(e: E) -> Int { match e { N(n) => { n = \"one\"; n } } }",
        ),
    ] {
        assert!(
            is_clean_with_lower(ok),
            "every binding is assignable: `{ok}` reported {:?}",
            analyze_and_lower_diags(ok)
        );
        assert!(
            has_type_error(bad),
            "assignment still has to preserve the type: `{bad}` was accepted"
        );
    }
}

/// A compound assignment requires a numeric target: the rule is "numeric", not
/// "not Bool", and an unconstrained target is not yet a mistake.
///
/// A `Text` target is excused for `+=` **only** — `+=` on a `Text` is
/// concatenation (ADR-085), so what it requires is what `+` requires and no
/// number is involved. `-=` on one is still `Y010`.
#[test]
fn a_compound_assignment_needs_a_numeric_target() {
    for src in [
        "var flag = true\nflag += false",
        "fn f() -> Int { var pair = (1, 2); pair += (3, 4); 0 }",
        // A `Text` target is excused for `+=` only (ADR-085), not for these.
        "var name = \"a\"\nname -= \"b\"",
        "var name = \"a\"\nname *= \"b\"",
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
    // …and on Float. This is an inference assertion only: that `f += 0.5` also
    // *lowers* to Float arithmetic rather than to arithmetic on the IEEE-754 bit
    // pattern is gated by the `float_compound_assign` run fixture.
    assert!(!has_type_error("var x = 1.5\nx += 0.5"), "…and on Float");
    assert!(
        !has_type_error("var n = 0\nn = 1"),
        "a plain `=` is not arithmetic and needs no numeric target"
    );
    // ADR-085: `+=` on a `Text` needs no number, because it is not arithmetic.
    assert!(
        !has_type_error("var name = \"a\"\nname += \"b\""),
        "`+=` on a Text is concatenation"
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

/// An `if` with no `else` is Unit — unless its then branch *diverges*, which is
/// still legal, because then there is no value to have nowhere to come from.
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
    // …and a real value with no else is still a mismatch, whatever its type.
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

/// A bare `return` is `Unit`, an unannotated function has its result *pinned*
/// by its returns, a `return` nested in control flow is still checked, and a
/// `return` inside a closure means the closure.
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
        !has_type_error("fn f() -> Text { var g = |n| { return n }\n  g(\"a\") }"),
        "a closure's return is checked against the closure"
    );
    assert!(
        has_type_error("fn f() -> Text { var g = |n| { if true { return 1 }\n  \"t\" }\n  \"a\" }"),
        "…and it is still checked there"
    );
}

/// A body that diverges cannot disagree with the declared result type, because
/// it produces no value.
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
    let src = "fn bad() -> Int { 1; var x = 2 }";
    assert!(
        has_type_error(src),
        "inference and lowering must agree on the actual trailing expression"
    );
}

/// A block's value is its *last* statement, and only if that statement is an
/// expression. A pending tail is demoted by anything that follows it, whatever
/// kind it is.
#[test]
fn a_blocks_value_is_its_last_statement_and_only_if_it_is_an_expression() {
    // A trailing expression is the value.
    assert_eq!(scheme_of("var b = { 1 }", "b").as_deref(), Some("Int"));
    assert_eq!(
        scheme_of("var b = { var x = 1; 2 }", "b").as_deref(),
        Some("Int")
    );
    // Every non-expression kind demotes a pending tail.
    for src in [
        "var b = { 1; var x = 2 }",
        "var b = { 1; var x = 2 }",
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
        scheme_of("var b = { 1; var x = 2 }", "b").as_deref(),
        Some("Unit"),
        "a `var` after the expression makes the block Unit"
    );
    // Two expression statements: only the second is the value.
    assert_eq!(
        scheme_of("var b = { 1; \"two\" }", "b").as_deref(),
        Some("Text")
    );
    // An empty block is Unit.
    assert_eq!(scheme_of("var b = { }", "b").as_deref(), Some("Unit"));
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

/// The two terminator codes, and the boundaries that decide them. A closure is a
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
    assert!(codes("var x = 1\nreturn").contains(&"Y011".to_string()));
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
        codes("fn f(v: Vec[Int]) -> Int { for x in v { var g = |n| { break }\n  0 }\n  0 }")
            .contains(&"Y012".to_string()),
        "a `break` inside a closure cannot leave a loop outside it"
    );
    assert!(
        !codes("fn f() -> Int { var g = |n| { return n }\n  g(1) }").contains(&"Y011".to_string()),
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

/// A `loop` **is** the join of the values its `break`s carry. Merely asking
/// that such a program is accepted would also be satisfied by a `loop` that
/// stayed a fresh variable; these ask what the type actually *is*, and what
/// belongs to which loop.
#[test]
fn a_loop_is_the_join_of_the_values_its_breaks_carry() {
    // The value is the break's, not the body's: the body here is `Unit`.
    assert_eq!(
        scheme_of("var x = loop { break 42 }", "x").as_deref(),
        Some("Int")
    );
    assert_eq!(
        scheme_of("var x = loop { break \"done\" }", "x").as_deref(),
        Some("Text")
    );
    // Two `break`s that agree; and a `break` nested inside the body still counts.
    assert_eq!(
        scheme_of(
            "fn f(c: Bool) -> Int { var x = loop { if c { break 1 } else { break 2 } }\n  x }",
            "x"
        )
        .as_deref(),
        Some("Int")
    );
    // A bare `break` leaves the loop with nothing, so the loop is `Unit`…
    assert_eq!(
        scheme_of("var x = loop { break }", "x").as_deref(),
        Some("Unit")
    );
    // …and mixing the two spellings is a mismatch, not a coincidence.
    assert!(
        has_type_error("fn f(c: Bool) -> Int { var x = loop { if c { break }\n  break 1 }\n  0 }"),
        "a bare `break` contributes Unit and cannot agree with `break 1`"
    );
    assert!(
        has_type_error(
            "fn f(c: Bool) -> Int { var x = loop { if c { break 1 }\n  break \"two\" }\n  0 }"
        ),
        "two `break`s carrying different types disagree"
    );
    // A `break` belongs to the innermost loop: the inner one is `Int`, and the
    // outer one is `Text` rather than a join across both.
    assert_eq!(
        scheme_of(
            "var x = loop { var inner = loop { break 1 }\n  break \"outer\" }",
            "x"
        )
        .as_deref(),
        Some("Text")
    );
    assert_eq!(
        scheme_of(
            "var x = loop { var inner = loop { break 1 }\n  break \"outer\" }",
            "inner"
        )
        .as_deref(),
        Some("Int")
    );
}

/// A `loop` no `break` leaves produces nothing, so it is `Never` — the bottom
/// type, absorbed wherever branches meet.
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
            "fn f(c: Bool) -> Int { var x = loop { if c { return 1 } }\n  x }",
            "x"
        )
        .as_deref(),
        Some("Never"),
        "the bottom type, not Unit and not a fresh variable"
    );
}

/// Only a `loop` is an expression loop, and `Y017` says so. A `while`
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
    let (analysis, module) = analyze_and_lower(src);
    assert!(
        !analysis
            .diagnostics
            .iter()
            .any(|d| d.code().category() == DiagnosticCategory::Type),
        "Never is the bottom type and must not conflict with an Int branch"
    );
    let choose = fn_named(&module, "choose");
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

/// A divergent branch is absorbed wherever branches meet, in either position,
/// and a `match` whose every arm diverges is itself `Never` rather than
/// "whatever the first use wants".
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
            "enum E { A, B }\nfn f(e: E) -> Int { var m = match e { A => panic(\"x\"), B => panic(\"y\") }; 0 }",
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

/// Each of the three output/control names has the type §8.1/§9.1 gives it, and
/// the type is what makes each usable. `assert` refuses a non-`Bool`; `dbg` is
/// the identity, so it can wrap any subexpression without changing what the
/// program computes; `panic` is `Never`, so it satisfies any declared result.
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
/// rather than to a user function that does not exist. Typing alone would
/// accept a program that then fails the compile as an unresolved user function.
#[test]
fn each_control_builtin_reaches_the_backend() {
    assert!(is_clean_with_lower("fn main() -> Unit { panic(\"stop\") }"));
    assert!(is_clean_with_lower("fn main() -> Unit { assert(true) }"));
    assert!(is_clean_with_lower("fn main() -> Int { dbg(7) }"));
}

/// Each of §16.1's seven numeric helpers is `(Int, …) -> Int` (ADR-058), which
/// means each of the three things a fresh type variable could not do — reject a
/// wrong operand type, reject a wrong operand count, and be used where an `Int`
/// is required.
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

/// ADR-059: a range's bounds are `Int` and the range itself is the nullary
/// `Range` collection, whose element type is `Int`.
///
/// `Int` bounds **only**: `iter_item` says a range yields `Int`, and admitting
/// `Float` bounds would make that a lie with no step to fix it.
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
    // claims. Annotated, because an *unannotated* iterated parameter unifies
    // with its own element type — see `iter_item`'s optimism at `infer_for`,
    // which affects every iterable equally.
    assert!(!has_type_error(
        "fn total(r: Range) -> Int { var t = 0\n for i in r { t = t + i }\n t }\n\
         fn main() -> Int { total(0..4) }"
    ));
}

/// A range is a first-class **value** (ADR-059), not only a `for`-header form:
/// it binds to a name, passes as an argument, comes back as a result, and —
/// because its bounds cannot change once it is built — is a legal `Map` key
/// (ADR-057 decision 3).
#[test]
fn a_range_is_an_ordinary_value() {
    assert!(!has_type_error(
        "fn f() -> Unit { var r = 0..5\n for i in r { out(i) } }"
    ));
    assert!(!has_type_error(
        "fn widen(r: Range) -> Range { r }\n\
         fn main() -> Range { widen(1..2) }"
    ));
    // A `Range` is hashable *and* immutable, so it is a key — the distinction
    // ADR-057 decision 3 turns on.
    assert!(!has_type_error(
        "fn main() -> Unit { var m = Map()\n m.insert(0..3, 1) }"
    ));
    assert!(!has_type_error(
        "fn main() -> Unit { var s = Set()\n s.insert(0..3) }"
    ));
    // …and it is equatable, so two ranges compare.
    assert!(!has_type_error("fn f() -> Bool { (0..3) == (0..3) }"));
    // It is not orderable: only the five scalars with a `compare` are (ADR-045).
    assert!(has_type_error("fn f() -> Bool { (0..3) < (1..4) }"));
}

/// A **bare** nullary collection name is the type it names. `Range` and `BitSet`
/// are the only two ctors with no type arguments, so they are the only names
/// that appear in type position without brackets — and the annotation has to
/// reach `collection_from_name` rather than resolving to a fresh variable.
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
        "fn f() -> Unit { var r: Range = 5\n out(r) }"
    ));

    // …and a ctor that *does* take arguments, written bare, is a `Y007` for a
    // wrong argument count — not a silent variable.
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
/// call, not to a `CallTarget::User` the backend cannot resolve.
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

// --- §6.5's graph helpers (ADR-060) -----------------------------------------

/// A neighbour function over `Int` states, for the tests below. Written once
/// because every graph helper takes one and the interesting part is never the
/// graph.
const STEPS: &str = "fn steps(n: Int) -> Vec[Int] { Vec() }\n";

/// Each of the six has the signature its contract needs: one state type, a
/// neighbour function of it, the result the helper's name promises — and the
/// arity, which a fresh type variable could not enforce at all.
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
/// element and a `Map` key — so ADR-057 decision 3 reaches it: a mutable
/// collection is refused, and it is refused **at the call**, which is the only
/// place that can name the type.
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

/// The state requirement rides the deferred-constraint channel (ADR-057) rather
/// than being decided at the call: a helper called on an *unannotated*
/// parameter defers the requirement, the enclosing function's scheme carries
/// it, and the caller that pins the type is where it is answered.
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
/// call, not to a `CallTarget::User` the backend cannot resolve.
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
               fn main() -> Int { var values = Vec(); values.push(1); total(values) }";
    assert!(
        !has_type_error_with_lower(src),
        "method use plus the call site should infer a concrete collection receiver"
    );
}

/// A method called on a receiver nothing has typed yet is a **requirement**,
/// and the use site answers it.
///
/// The answer is the one §5.2 states exactly — `total: Vec[Int] -> Int` — and
/// the resolution runs in both directions: the entry's *result* pins the call,
/// and the entry's *parameters* pin the arguments the deferred call passed.
#[test]
fn a_method_on_an_unannotated_receiver_is_resolved_by_the_use_site() {
    // §5.2's own answer, written down.
    let sum = "fn total(values) { values.sum() }\n\
               fn main() -> Int { var values = Vec(); values.push(1); total(values) }";
    assert_eq!(
        scheme_of(sum, "total").as_deref(),
        Some("(Vec[Int]) -> Int")
    );

    // Not a special case of `sum`: any catalog entry, on any receiver shape the
    // catalog models — including a scalar one.
    let len = "fn size(v) { v.len() }\n\
               fn main() -> Int { var v = Vec(); v.push(1); size(v) }";
    assert_eq!(scheme_of(len, "size").as_deref(), Some("(Vec[Int]) -> Int"));
    let text = "fn size(t) { t.len() }\nfn main() -> Int { size(\"abc\") }";
    assert_eq!(scheme_of(text, "size").as_deref(), Some("(Text) -> Int"));

    // The deferred entry pins the *arguments* too. `x` has no annotation and no
    // use of its own; `push`'s parameter is what says it is an `Int`.
    let arg = "fn add(v, x) { v.push(x) }\n\
               fn main() -> Unit { var v = Vec(); v.push(1); add(v, 2) }";
    assert_eq!(
        scheme_of(arg, "add").as_deref(),
        Some("(Vec[Int], Int) -> Unit")
    );
    assert!(!has_type_error_with_lower(arg));

    // …so an argument that disagrees with it is reported, which is the half a
    // "the program is accepted" test cannot reach.
    let bad = "fn add(v, x) { v.push(x) }\n\
               fn main() -> Unit { var v = Vec(); v.push(1); add(v, \"s\") }";
    assert!(has_type_error_with_lower(bad));

    // And the result is checked against the annotation the deferred function
    // wrote, rather than being whatever the annotation says.
    let wrong = "fn total(values) -> Text { values.sum() }\n\
                 fn main() -> Text { var v = Vec(); v.push(1); total(v) }";
    assert!(has_type_error_with_lower(wrong));
}

/// The receiver a method was called on is **pinned**, not quantified
/// (ADR-057 decision 5).
///
/// This is the contract, and it is why `pin_to_level` exists. There is one
/// lowered body per source function — monomorphization clones a tree lowering
/// already resolved — so one method call site carries one catalog entry and one
/// receiver type. Two receivers at one call site is not a shape the compiler can
/// lower, so it is a disagreement about `total`'s signature instead.
///
/// The second half is what keeps the rule from being "nothing generalizes": a
/// parameter no method was called on is quantified as usual.
#[test]
fn a_receiver_a_method_was_called_on_is_not_quantified() {
    let two = "fn total(values) { values.sum() }\n\
               fn main() -> Int {\n\
                 var a = Vec()\n\
                 a.push(1)\n\
                 var b = Vec()\n\
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
                   fn main() -> Int { var t = id(\"s\"); id(1) }";
    assert_eq!(
        scheme_of(generic, "id").as_deref(),
        Some("forall T. (T) -> T")
    );
    assert!(!has_type_error_with_lower(generic));
}

/// A requirement the receiver's *type* carries is checked once the receiver
/// resolves, and a deferred method call is no exception (ADR-057 decisions 3
/// and 5).
///
/// `store` never says what `m` is. The `insert` inside it is what makes it a
/// `Map`, and the key rule then applies to the key the call site chose — so the
/// mutable-collection-as-key refusal reaches through a function whose signature
/// was inferred entirely from a deferred method.
#[test]
fn a_deferred_method_still_carries_its_receivers_own_requirements() {
    let src = "fn store(m, k) -> Unit { m.insert(k, 1) }\n\
               fn main() -> Unit {\n\
                 var m = Map()\n\
                 var key = Vec()\n\
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
              fn main() -> Unit { var m = Map(); store(m, \"k\") }";
    assert!(!has_type_error_with_lower(ok));
}

// --- ADR-137: the constraint channel discharges to a fixpoint ---------------

/// The catalog rows inference actually selected, by method name, sorted.
///
/// This is the map lowering reads and nothing else, so it is the only place a
/// *silently* unresolved method call is visible from inference.
/// `has_type_error_with_lower` cannot see one: an unresolved call is not a
/// diagnostic, it is an absence, and the absence is what MIR turns into an
/// `internal compiler error`.
fn resolved_method_rows(text: &str) -> Vec<String> {
    let analysis = analyze(text);
    let mut names: Vec<String> = analysis
        .method_refs
        .values()
        .map(|m| m.entry.name.to_string())
        .collect();
    names.sort();
    names
}

/// **ADR-137.** A method on a value *derived* from an unannotated parameter
/// resolves exactly as one on the parameter itself.
///
/// `HasMethod`, `Iterable` and `HasField` are discharged by *producing* a type,
/// and that production is what resolves the receiver of the next link. Draining
/// a single batch of dischargeable constraints answers `t[i]` and leaves
/// `t[i][j]` unresolved — an absence `check` cannot see and MIR turns into an
/// ICE — which is why the channel runs to a fixpoint. Every row below is a
/// single call site at one concrete type, so none of them is asking for
/// polymorphism.
///
/// Every program here calls at the **top level**, deliberately: wrapping the
/// call in a later `fn` buys a second discharge round for free, because
/// `infer_fn` discharges once per body, and that would mask a channel that
/// stops short of a fixpoint.
#[test]
fn a_method_on_a_derived_receiver_resolves_like_one_on_the_parameter() {
    // A subscript of a subscript, and the answer inference should reach.
    let pick = "fn pick(t, i, j) { t[i][j] }\n\
                out(pick([[7, 8]], 0, 0))";
    assert_eq!(
        scheme_of(pick, "pick").as_deref(),
        Some("(Vec[Vec[Int]], Int, Int) -> Int")
    );
    assert_eq!(
        resolved_method_rows(pick),
        vec!["[]".to_string(), "[]".to_string()],
        "both subscripts must carry a catalog row, not just the first"
    );

    // Every derived-receiver shape, so a change that trades one for another is
    // caught here.
    for (src, rows) in [
        // Method on the parameter.
        ("fn f(v) { v.len() }\nout(f([1, 2, 3]))", vec!["len"]),
        // One subscript, result returned.
        ("fn f(v) { v[0] }\nout(f([1, 2, 3]))", vec!["[]"]),
        // Subscript of a subscript.
        ("fn f(v) { v[0][1] }\nout(f([[1, 2, 3]]))", vec!["[]", "[]"]),
        // Catalog method on a subscript result.
        (
            "fn f(v) { v[0].len() }\nout(f([[1, 2, 3]]))",
            vec!["[]", "len"],
        ),
        // Catalog method on a method result.
        (
            "fn f(v) { v.get(0).len() }\nout(f([[1, 2, 3]]))",
            vec!["get", "len"],
        ),
        // Catalog method on the `for` item — ADR-062's own claim, tested with a
        // method instead of arithmetic.
        (
            "fn f(v) -> Unit { for row in v { out(row.len()) } }\n\
             f([[1, 2, 3], [4, 5]])",
            vec!["len"],
        ),
        // Binding the intermediate first is the same program.
        (
            "fn pick(t, i, j) { var row = t[i]\n row[j] }\n\
             out(pick([[7, 8]], 0, 0))",
            vec!["[]", "[]"],
        ),
    ] {
        assert!(
            !has_type_error_with_lower(src),
            "a well-typed derived receiver must not be a diagnostic: {src:?}"
        );
        assert_eq!(
            resolved_method_rows(src),
            rows.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(),
            "every call site must carry the row lowering reads: {src:?}"
        );
    }
}

/// **ADR-137.** The channel runs to a *fixpoint*, not for a second round.
///
/// Three links deep, with the call site inside a *later function* so the free
/// extra `infer_fn` discharge is already spent. A fixed number of rounds cannot
/// pass this: two rounds resolve `v[0]` and `v[0][0]` and still drop the `len`,
/// exactly as one round drops the second `[]` of `t[i][j]`.
#[test]
fn a_chain_of_deferred_receivers_resolves_to_a_fixpoint_not_one_link() {
    let three = "fn f(v) { v[0][0].len() }\n\
                 fn g() -> Int { f([[[1, 2, 3]]]) }\n\
                 out(g())";
    assert_eq!(
        scheme_of(three, "f").as_deref(),
        Some("(Vec[Vec[Vec[Int]]]) -> Int")
    );
    assert_eq!(
        resolved_method_rows(three),
        vec!["[]".to_string(), "[]".to_string(), "len".to_string()]
    );

    // And deeper still, so the test is about the fixpoint rather than about the
    // number three.
    let five = "fn f(v) { v[0][0][0][0].len() }\n\
                out(f([[[[[1, 2, 3]]]]]))";
    assert!(!has_type_error_with_lower(five));
    assert_eq!(resolved_method_rows(five).len(), 5);
}

/// **ADR-137 × ADR-093/ADR-133.** A derived receiver that resolves to a type
/// without the row is *reported*, at `check`.
///
/// This is the half of the fixpoint that is not "more programs compile". The
/// element of a `Vec[Int]` is an `Int`, which has no `len`, so the `HasMethod`
/// on the subscript's result has to be re-examined and reported — otherwise
/// `check` exits 0 and `run` ICEs in MIR, the check/run asymmetry ADR-133
/// exists to close.
///
/// The second assertion is the ADR-093 pair: exactly one emitter, so a lowering
/// backstop that reported the same call again would turn it red.
#[test]
fn a_derived_receiver_that_resolves_to_a_type_without_the_row_is_reported() {
    for src in [
        // A subscript result.
        "fn f(v) { v[0].len() }\nout(f([1, 2, 3]))",
        // A method result.
        "fn f(v) { v.get(0).len() }\nout(f([1, 2, 3]))",
        // A `for` item.
        "fn f(v) -> Unit { for row in v { out(row.len()) } }\nf([1, 2, 3])",
    ] {
        let from_inference: Vec<String> = analyze(src)
            .diagnostics
            .iter()
            .map(|d| d.code().to_string())
            .collect();
        assert_eq!(
            from_inference,
            vec!["Y110".to_string()],
            "`praxis check` must report it, got {from_inference:?} for {src:?}"
        );
        let with_lowering: Vec<String> = analyze_and_lower_diags(src)
            .iter()
            .map(|d| d.code().to_string())
            .collect();
        assert_eq!(
            with_lowering,
            vec!["Y110".to_string()],
            "one emitter, not two: got {with_lowering:?} for {src:?}"
        );
    }

    // And it names the type it resolved to, so the report is about the element
    // rather than about the collection the program wrote.
    let diags = analyze("fn f(v) { v[0].len() }\nout(f([1, 2, 3]))").diagnostics;
    assert_eq!(
        diags.iter().map(|d| d.message()).collect::<Vec<_>>(),
        vec!["no method `len` on type `Int` taking 0 argument(s)"]
    );
}

/// **ADR-137 decision 2, the negative gate.** The fixpoint changes *when* a
/// constraint is examined and nothing about *which* variables are pinned.
///
/// ADR-057 decision 5 is untouched: there is one lowered body per source
/// function, so a receiver a method was called on is pinned to the declaration
/// group's level and `total` is the monotype `(Vec[Int]) -> Int`. Two element
/// types at one call site is still a `Y001`, produced by unifying the callee's
/// monotype in `infer_call` — no number of discharge rounds can reach it.
///
/// The second half is the other side of the same fence: a parameter no method
/// was called on still generalizes, so the fixpoint pins nothing new either.
#[test]
fn a_derived_receiver_does_not_make_the_function_generic() {
    let two = "fn total(values) { values.sum() }\n\
               fn main() -> Int {\n\
                 var a = Vec()\n\
                 a.push(1)\n\
                 var b = Vec()\n\
                 b.push(1.0)\n\
                 total(a)\n\
                 total(b)\n\
               }";
    assert!(
        has_type_error_with_lower(two),
        "ADR-057 decision 5 stands: one method call site carries one receiver"
    );
    assert_eq!(
        scheme_of(
            "fn total(values) { values.sum() }\n\
             fn main() -> Int { var v = Vec(); v.push(1); total(v) }",
            "total"
        )
        .as_deref(),
        Some("(Vec[Int]) -> Int"),
        "a pinned receiver is still a monotype after the fixpoint"
    );

    // More discharge rounds at the `infer_fn` point could in principle lower a
    // level and cost a quantifier. It does not: nothing a resolving discharge
    // touches was quantifiable in the first place.
    let generic = "fn id(x) { x }\n\
                   fn main() -> Int { var t = id(\"s\"); id(1) }";
    assert_eq!(
        scheme_of(generic, "id").as_deref(),
        Some("forall T. (T) -> T")
    );
    assert!(!has_type_error_with_lower(generic));
}

/// **ADR-137 decision 2.** The pin reaches the *derived* receiver too.
///
/// `require_method` pins the result variable alongside the receiver, so a
/// subscript's result is pinned exactly as the receiver is: `pick` refuses two
/// element types for the same reason `total` does. Quantifying a derived
/// receiver instead would accept this program.
#[test]
fn a_derived_receiver_is_pinned_too() {
    let two = "fn pick(t, i, j) { t[i][j] }\n\
               out(pick([[7, 8]], 0, 0))\n\
               out(pick([[\"a\"]], 0, 0))";
    assert!(
        has_type_error_with_lower(two),
        "a derived receiver is pinned, so one `pick` cannot serve two element types"
    );

    // The `for` item, ADR-062 decision 2's own variable, refuses the same way.
    let items = "fn widths(rows) -> Unit { for row in rows { out(row.len()) } }\n\
                 widths([[1, 2]])\n\
                 widths([\"ab\", \"c\"])";
    assert!(
        has_type_error_with_lower(items),
        "ADR-062 decision 2's item pin survives the fixpoint"
    );
}

/// **ADR-093.** A method that cannot resolve is reported by **inference**, and
/// only once.
///
/// `praxis check` runs inference and stops; only `praxis run` runs lowering. So
/// a `Y110` that only lowering emits is a `Y110` `praxis check` cannot see.
///
/// The two assertions are a pair and neither is redundant:
///
/// * `analyze` **alone** yields exactly one `Y110`, for all four shapes.
/// * `analyze` + `lower` still yields exactly one — a lowering backstop kept
///   alongside inference's report would yield two, and only this assertion
///   catches that.
///
/// Both of inference's doors are covered, because they are different code and
/// each was silent for its own reason.
#[test]
fn a_method_that_cannot_resolve_is_reported_by_inference_and_only_once() {
    for src in [
        // Concrete receiver, no such row — the call-site door.
        "var v = Vec[Int]()\nv.push(1)\nout(v.nope())",
        // Receiver a parameter the call site pins. `nope` is in no catalog row
        // at any receiver, so the call-site door refuses it without waiting.
        "fn f(x) { x.nope() }\nfn main() -> Unit { f(1) }",
        // Never pinned at all. Same door, same reason — and this is the shape
        // that could not have been reported any other way: the constraint would
        // sit in `pending_constraints` forever, because `take_dischargeable`
        // only ever returns constraints whose variable has resolved.
        "fn f(x) { x.nope() }\nfn main() -> Unit { }",
        // The **deferred** door: `push` exists (on `Vec[T]`, at arity 1), so the
        // requirement is made and rides the channel; the call site then pins the
        // receiver to `Int`, which has no such row. Reported at discharge.
        "fn f(x) -> Unit { x.push(1) }\nfn main() -> Unit { f(3) }",
    ] {
        let from_inference: Vec<String> = analyze(src)
            .diagnostics
            .iter()
            .map(|d| d.code().to_string())
            .collect();
        assert_eq!(
            from_inference,
            vec!["Y110".to_string()],
            "`praxis check` must report it, got {from_inference:?} for {src:?}"
        );
        let with_lowering: Vec<String> = analyze_and_lower_diags(src)
            .iter()
            .map(|d| d.code().to_string())
            .collect();
        assert_eq!(
            with_lowering,
            vec!["Y110".to_string()],
            "one emitter, not two: got {with_lowering:?} for {src:?}"
        );
    }
}

/// **ADR-093.** The report names the receiver type *and* the arity.
///
/// This is how "one emitter" becomes observable rather than asserted: a builder
/// that names the arity but not the type (``on this type``), or the type but not
/// the arity, cannot produce either text below.
#[test]
fn the_report_names_the_receiver_type() {
    let diags = analyze("var v = Vec[Int]()\nv.push(1)\nout(v.nope())").diagnostics;
    assert_eq!(
        diags.iter().map(|d| d.message()).collect::<Vec<_>>(),
        vec!["no method `nope` on type `Vec[Int]` taking 0 argument(s)"],
    );

    // The deferred door names the type the call site pinned, which is the whole
    // reason the report belongs to inference: at lowering the receiver here is
    // still a variable, so the message could only have said "this type".
    let diags = analyze("fn f(x) -> Unit { x.push(1) }\nfn main() -> Unit { f(3) }").diagnostics;
    assert_eq!(
        diags.iter().map(|d| d.message()).collect::<Vec<_>>(),
        vec!["no method `push` on type `Int` taking 1 argument(s)"],
    );

    // And the one shape with no receiver to name gets its own wording rather
    // than printing `?a` into a message §5.4 requires to be concrete.
    let diags = analyze("fn f(x) { x.nope() }\nfn main() -> Unit { }").diagnostics;
    assert_eq!(
        diags.iter().map(|d| d.message()).collect::<Vec<_>>(),
        vec!["no type has a method `nope` taking 0 argument(s)"],
    );
}

/// **ADR-093's boundary.** A name the catalog *does* hold is still deferred, and
/// the generic still generalizes.
///
/// The rule refuses a method call whose name no catalog row holds at that
/// arity, before anything says what the receiver is. The spelling of that
/// predicate matters: write `has_name_at_arity` as "no row matches this
/// receiver" instead of "no row holds this name at this arity" and §5.2's own
/// example — `fn total(values) { values.sum() }`, which the document prints as
/// `(Vec[Int]) -> Int` — is rejected outright with a `Y110`, uncalled, with
/// `sum` implemented and working.
///
/// The `run` half is asserted too: lowering must not report the still
/// unresolved receiver either, so both commands accept the program.
#[test]
fn a_name_the_catalog_holds_is_still_deferred() {
    let src = "fn total(values) { values.sum() }\nfn main() -> Unit { }";
    assert!(
        analyze(src).diagnostics.is_empty(),
        "an uncalled generic over a real catalog name must check clean: {:?}",
        analyze(src).diagnostics
    );
    assert!(
        is_clean_with_lower(src),
        "…and must stay clean once lowering runs, which it did not before ADR-093"
    );
    // The requirement is still on the channel: the receiver is pinned to the
    // declaration group's level (`require_method`), so `total` is a signature
    // with an open receiver and an open result rather than a scheme, and a call
    // site is what closes both. A rule that reported here would have to answer
    // this differently.
    assert_eq!(scheme_of(src, "total").as_deref(), Some("(?T) -> ?U"));
    // With a call site, the deferred requirement resolves.
    let called = "fn total(values) { values.sum() }\n\
                  fn main() -> Unit { var v = Vec[Int]()\nv.push(1)\nout(total(v)) }";
    assert!(is_clean_with_lower(called));
}

/// A barrier combinator declares what its element must be, and `analyze`
/// **alone** enforces it — so `praxis check` sees it.
///
/// `sorted` orders through the element descriptor's `compare` callback and
/// `frequencies` builds a `Counter` whose keys go through `hash`/`equals`.
/// Neither callback exists for every type, and neither rule is enforced anywhere
/// else for these rows: `require_collection_invariants` asks the key rule of a
/// method's **receiver**, and the receiver here is an ordinary `Vec` that is
/// entitled to hold anything at all. The rule belongs to the row, which is what
/// `Bound::Kind` is.
///
/// How it goes red: drop the `TypePattern::of_kind` from `seq_sorted`'s receiver
/// (leaving a bare element variable inside `TypePattern::iterable`) and the whole
/// program goes through — `praxis check` silent and exit 0, `praxis run` exit 1
/// with `error: program faulted: value does not have the declared type`, the
/// `TypeMismatch` `praxis_vec_sorted` raises when the element descriptor has no
/// `compare`. A fault from a program the checker accepted is the thing the bound
/// exists to prevent. The same deletion on `seq_frequencies` accepts a
/// `Vec[Vec[Int]]` key, which ADR-057 decision 3 forbids.
///
/// **Two** closures, not one, and that is not padding: the wrapper returns early
/// when there is nothing to compare, so a one-element `Vec` sorts happily
/// without ever reaching the missing callback and the deletion above looks
/// harmless.
///
/// The diagnostics are the language's own ordering and key wording, not `Y110`:
/// the method exists, and what is wrong is the element.
#[test]
fn a_barrier_declares_what_its_element_must_be() {
    // A closure is the canonical unorderable value (§5.5: function values have
    // no structural identity).
    let unorderable = "var v = Vec()\nv.push(|x| x + 1)\nv.push(|x| x + 2)\nout(v.sorted())";
    let codes: Vec<String> = analyze(unorderable)
        .diagnostics
        .iter()
        .map(|d| d.code().to_string())
        .collect();
    assert_eq!(
        codes,
        vec!["Y006".to_string()],
        "`sorted` on an unorderable element is the ordering diagnostic, at `check`"
    );

    // A `Vec` element can change after it is stored, so it cannot be a key —
    // and `frequencies`' result is keyed on exactly it.
    let unstable = "var inner = Vec[Int]()\ninner.push(1)\n\
                    var v = Vec()\nv.push(inner)\nout(v.frequencies())";
    let codes: Vec<String> = analyze(unstable)
        .diagnostics
        .iter()
        .map(|d| d.code().to_string())
        .collect();
    assert_eq!(
        codes,
        vec!["Y014".to_string()],
        "`frequencies` of a mutable element is the key diagnostic, at `check`"
    );

    // `unique` carries the same bound for the same reason — sameness is the
    // descriptor's `hash`/`equals`, so an element that changes is not found
    // again on the second pass.
    let unstable_unique = "var inner = Vec[Int]()\ninner.push(1)\n\
                           var v = Vec()\nv.push(inner)\nout(v.unique())";
    assert!(has_type_error(unstable_unique));

    // And the ordinary shapes are accepted, so the bounds refuse rather than
    // reject: a `Vec[Text]` sorts, a `Vec[Int]` counts.
    assert!(is_clean_with_lower(
        "var v = Vec[Text]()\nv.push(\"b\")\nout(v.sorted())"
    ));
    assert!(is_clean_with_lower(
        "var v = Vec[Int]()\nv.push(1)\nout(v.frequencies()[1])"
    ));
    assert!(is_clean_with_lower(
        "var v = Vec[Int]()\nv.push(1)\nout(v.unique())"
    ));
}

/// **ADR-149.** The two groupings answer `Vec[Vec[T]]` — the one catalog result
/// that nests a collection inside a collection — and the nesting has to survive
/// inference, not merely be written in the row.
///
/// The flattened answer is the failure this is aimed at: a row declaring
/// `Vec[T]` would make `[1, 2].chunks(1)` a `Vec[Int]`, every use of a group as
/// a sequence would then be a `Y110` or worse, and nothing about the row's own
/// text would look wrong. So the assertion is on the *rendered* type, at both
/// levels.
///
/// The element threads through from the receiver rather than being invented:
/// a `Text` receiver groups into `Vec[Vec[Char]]`, and a `Map` into pairs.
#[test]
fn a_grouping_answers_a_sequence_of_sequences() {
    for name in ["chunks", "windows"] {
        assert_eq!(
            expr_type(&format!("[1, 2, 3].{name}(2)")),
            "Vec[Vec[Int]]",
            "`{name}` groups without flattening"
        );
        // Every receiver, through the item a `for` would yield.
        assert_eq!(expr_type(&format!("\"ab\".{name}(1)")), "Vec[Vec[Char]]");
        assert_eq!(expr_type(&format!("(0..4).{name}(2)")), "Vec[Vec[Int]]");

        // A group is a sequence in its own right, so the pipeline continues on
        // it. This is the half a type assertion alone would not catch: the row
        // could render right and still not compose.
        assert!(is_clean_with_lower(&format!(
            "var v = Vec[Int]()\nv.push(1)\nout(v.{name}(1).map(|g| g.sum()).sum())"
        )));

        // The size is an `Int` and any `Int` expression will do — it is not a
        // literal-only parameter.
        assert!(is_clean_with_lower(&format!(
            "fn size() -> Int {{ 2 }}\nfn main() -> Unit {{ out([1, 2].{name}(size()).count()) }}"
        )));
        assert!(has_type_error(&format!("out([1, 2].{name}(\"two\"))")));
    }

    // No capability bound at all, which is `reversed`'s claim: a `Vec` of
    // closures groups where `sorted()` earns `Y006`.
    assert!(is_clean_with_lower(
        "var v = Vec()\nv.push(|x| x + 1)\nv.push(|x| x + 2)\nout(v.windows(2).count())"
    ));
}

/// The barrier's bound rides the **constraint channel**, so an element the
/// program has not named yet is answered when something names it.
///
/// This is why `Bound::Kind` goes through `require_cap` rather than calling
/// `capability::check` directly, and the difference is only visible in one
/// shape: `var v = Vec()` mints `Vec[?T]`, which is a *concrete* receiver with
/// an *open* element, so `v.sorted()` resolves its row immediately and asks
/// about `?T` before any `push` has said what `?T` is. `capability::check`
/// answers **yes** to every unresolved variable by design — deciding otherwise
/// would break polymorphic inference everywhere — so a direct call accepts the
/// program and nothing ever asks again.
///
/// How it goes red: replace the `require_cap` in `apply_bounds`'s `Bound::Kind`
/// arm with `capability::check` + `report_cap_failure`, and the first case below
/// is accepted with no diagnostic. The generic-function shape is *not* the
/// discriminator — a deferred `HasMethod` is discharged only after the receiver
/// resolves, so `apply_bounds` there already sees a concrete element and a
/// direct check catches it too.
#[test]
fn a_barrier_bound_is_checked_at_the_call_site_that_pins_it() {
    // Ordered before it is populated: the bound is asked about `?T` and has to
    // wait for the `push` two lines later.
    let later = "var v = Vec()\nvar s = v.sorted()\nv.push(|x| x + 1)\nout(1)";
    let codes: Vec<String> = analyze(later)
        .diagnostics
        .iter()
        .map(|d| d.code().to_string())
        .collect();
    assert_eq!(
        codes,
        vec!["Y006".to_string()],
        "an element pinned after the `sorted()` still has to satisfy the bound"
    );

    // The same shape with an orderable element is clean, so the refusal is the
    // bound and not "an open element cannot be sorted".
    assert!(is_clean_with_lower(
        "var v = Vec()\nvar s = v.sorted()\nv.push(7)\nout(s.len())"
    ));

    // And through a generic function, where the receiver itself is deferred.
    let ok = "fn top(v) { v.sorted() }\n\
              fn main() -> Unit { var v = Vec[Int]()\nv.push(2)\nout(top(v)) }";
    assert!(
        is_clean_with_lower(ok),
        "an orderable instantiation is fine"
    );
    let bad = "fn top(v) { v.sorted() }\n\
               fn main() -> Unit { var v = Vec()\nv.push(|x| x + 1)\nout(top(v)) }";
    assert!(
        has_type_error(bad),
        "the bound has to be answered where the call site pins the element"
    );
}

#[test]
fn sum_requires_int_elements() {
    let src = "fn main() -> Int {\n\
                 var values = Vec()\n\
                 values.push(true)\n\
                 values.sum()\n\
               }";
    assert!(
        has_type_error_with_lower(src),
        "sum/product/min/max lower as Int operations and must reject Bool elements"
    );
}

/// The four aggregating sinks are **Int** operations, and the catalog says so.
///
/// The bound is `Int` and not `Numeric` because `CapKind::Numeric` includes
/// `Float` and the lowering does not: a `Vec[Float].sum()` that type-checked
/// would add the floats' bit patterns as integers.
#[test]
fn the_int_sinks_require_int_elements() {
    for sink in ["sum", "product", "min", "max"] {
        for (elem, push) in [("Bool", "true"), ("Float", "1.5"), ("Text", "\"a\"")] {
            let src = format!("fn main() -> Int {{ var v = Vec(); v.push({push}); v.{sink}() }}");
            assert!(
                has_type_error_with_lower(&src),
                "`{sink}` on a Vec[{elem}] must be rejected"
            );
        }
        // …and Int is accepted, so the bound is the element type and not the
        // sink.
        let ok = format!("fn main() -> Int {{ var v = Vec(); v.push(1); v.{sink}() }}");
        assert!(!has_type_error_with_lower(&ok), "`{sink}` on Vec[Int]");
    }
}

/// A bound **pins** an element nothing has named yet — it does not merely
/// permit one.
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
        "fn main() -> Int { var v = Vec(); v.push(1); v.map(|x| \"s\").sum() }"
    ));
    // The same chain with an Int-returning closure is clean, so the rejection is
    // the element type and not the fusion.
    assert!(!has_type_error_with_lower(
        "fn main() -> Int { var v = Vec(); v.push(1); v.map(|x| x * 2).sum() }"
    ));
    // And through deferred resolution, where the receiver itself is a variable
    // when the method is written.
    assert!(has_type_error_with_lower(
        "fn total(values) { values.sum() }\n\
         fn main() -> Int { var v = Vec(); v.push(true); total(v) }"
    ));
}

/// `enumerate` and `zip` say what they build: the *pair*, not the receiver's
/// element type. `enumerate` on a `Vec[T]` is a `Vec[(Int, T)]`, and `zip`
/// pairs two sequences whose element types are independent of each other.
#[test]
fn enumerate_and_zip_report_the_pairs_they_build() {
    let e = "fn main() -> Unit { var v = Vec(); v.push(1); var pairs = v.enumerate(); out(pairs) }";
    assert_eq!(scheme_of(e, "pairs").as_deref(), Some("Vec[(Int, Int)]"));

    // A non-Int element, so "the index is an Int" is visible as a *separate*
    // fact from the element type.
    let text =
        "fn main() -> Unit { var v = Vec(); v.push(\"a\"); var pairs = v.enumerate(); out(pairs) }";
    assert_eq!(
        scheme_of(text, "pairs").as_deref(),
        Some("Vec[(Int, Text)]")
    );

    // `zip` pairs two *different* element types.
    let z = "fn main() -> Unit {\n\
               var a = Vec()\n\
               a.push(1)\n\
               var b = Vec()\n\
               b.push(\"s\")\n\
               var pairs = a.zip(b)\n\
               out(pairs)\n\
             }";
    assert_eq!(scheme_of(z, "pairs").as_deref(), Some("Vec[(Int, Text)]"));
    assert!(!has_type_error_with_lower(z));
}

/// A compound assignment's numeric requirement survives generalization
/// (`Y015`).
///
/// `a += 1` inside a generic function says nothing about `a` yet, and pinning it
/// to `Int` there would silently narrow every unannotated numeric binding. So
/// the requirement is deferred: it rides on the scheme, and the call site is
/// where it is answered.
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
               fn main() -> Unit { var map = Map(); map.insert(id, 1) }";
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
                 var key = Vec()\n\
                 key.push(1)\n\
                 var table = Map()\n\
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
                 var key = Vec()\n\
                 key.push(1)\n\
                 var values = Set()\n\
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
               fn main() -> Unit { var heap = MinHeap(); heap.push(id) }";
    assert!(
        has_type_error_with_lower(src),
        "function values cannot be ordered by a heap"
    );
}

/// A type the capability admits into a heap is one the runtime can actually
/// order (ADR-045). `SupportsOrd` admits `Text`, and `HeapEntry::cmp` dispatches
/// to `TEXT.compare` rather than reading the payload as an `i64`, so the two
/// halves agree. `a_text_heap_pops_in_lexicographic_order` (praxis-runtime
/// `heaps.rs`) is the other half — that the order is the right one.
#[test]
fn heap_element_orderability_agrees_with_the_runtime() {
    let src = "fn main() -> Unit {\n\
                 var heap = MinHeap()\n\
                 heap.push(\"z\")\n\
                 heap.push(\"a\")\n\
               }";
    assert!(
        !has_type_error_with_lower(src),
        "Text is orderable in both halves now: the capability and the descriptor"
    );
}

#[test]
fn map_get_returns_option() {
    let src = "fn lookup(map: Map[Text, Int]) -> Option[Int] { map.get(\"key\") }";
    assert!(
        !has_type_error(src),
        "normal map absence is represented by Option[V], not a dynamically typed Unit/V result"
    );
}

#[test]
fn lowered_polymorphic_call_result_uses_the_callsite_instantiation() {
    let src = "fn id(value) { value }\nfn main() -> Float { id(1.5) }";
    let (analysis, module) = analyze_and_lower(src);
    let main = fn_named(&module, "main");
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
    let src = "fn main() -> Float { var values = Vec(); values.push(1.5); values.get(0) }";
    let (analysis, module) = analyze_and_lower(src);
    let main = fn_named(&module, "main");
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
    let src = "enum E { A }\nfn main() -> Int { var A = 7; A }";
    let (analysis, module) = analyze_and_lower(src);
    let main = fn_named(&module, "main");
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

/// …and it is *reported*, not merely survived: `analyze` returning is not
/// enough — `N005` is what tells the programmer why the function they wrote
/// does not exist.
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
/// `var` shadowing a prelude name never was — `is_bound_here` is what keeps the
/// check from firing on either.
#[test]
fn shadowing_an_outer_name_is_not_a_redeclaration() {
    for src in [
        "fn main() -> Int { var out = 1\n out }",
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
               fn main() -> Int { var pair = Pair { left: 1 }; pair.right }";
    assert!(
        !is_clean_with_lower(src),
        "allocating a record with fewer payloads than its schema is invalid"
    );
}

#[test]
fn record_literal_rejects_unknown_fields() {
    let src = "struct Point { x: Int }\n\
               fn side_effect() -> Int { out(\"must not disappear\"); 2 }\n\
               fn main() -> Int { var point = Point { x: 1, typo: side_effect() }; point.x }";
    assert!(
        !is_clean_with_lower(src),
        "an unknown field must be diagnosed instead of deleting its initializer"
    );
}

#[test]
fn record_literal_rejects_duplicate_fields() {
    let src = "struct Point { x: Int }\n\
               fn main() -> Int { var point = Point { x: 1, x: 2 }; point.x }";
    assert!(
        !is_clean_with_lower(src),
        "each record field must be initialized exactly once"
    );
}

/// A wildcard binds nothing, so `_` is not readable as a value. `_` is its own
/// token with no expression form at all, so the parser — not the resolver — is
/// what rejects it where it stands.
#[test]
fn wildcard_pattern_does_not_bind_a_value_named_underscore() {
    let src = "fn main() -> Int { match 1 { _ => _ } }";
    let (_, parsed) = parse_file(src);
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

/// The other three wildcard positions: a binding a program deliberately does
/// not name is legal, introduces nothing, and still *runs* its initializer.
///
/// "Introduces nothing" is a claim about the language, not about the symbol
/// table: `_` introduces nothing **a program can read**, which is what is
/// asserted here — no reference anywhere resolves to a `_` symbol, because the
/// resolver never binds one into a scope. A wildcard *parameter* still owns an
/// anonymous slot, exactly as a destructuring parameter does, because the
/// argument has to arrive somewhere.
#[test]
fn a_wildcard_binder_is_legal_and_declares_nothing() {
    for src in [
        "fn main() -> Int { var _ = 1; 0 }",
        "fn g(_) -> Int { 0 }\nfn main() -> Int { g(1) }",
        "fn main() -> Int { var f = |_| 0; f(1) }",
    ] {
        assert!(is_clean_with_lower(src), "`{src}` should compile clean");
        let analysis = analyze(src);
        let anonymous: Vec<_> = analysis
            .names
            .all()
            .iter()
            .filter(|s| s.name == "_")
            .map(|s| s.id)
            .collect();
        assert!(
            !analysis
                .refs
                .values()
                .any(|r| anonymous.contains(&r.symbol)),
            "`{src}`: nothing may read a `_`"
        );
    }
}

#[test]
fn nested_enum_pattern_must_cover_payload_constructors() {
    let src = "enum Flag { On, Off }\n\
               enum Wrapped { Wrap(Flag) }\n\
               fn main() -> Int {\n\
                 var value = Wrap(On)\n\
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
                 var value = A\n\
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
               fn main() -> Int { var value = A; match value { Typo(payload) => 1 } }";
    assert!(
        !is_clean_with_lower(src),
        "a misspelled variant cannot silently become an exhaustive wildcard"
    );
}

// --- input-parser conversion preserves source structure ---------------------

#[test]
fn mixed_template_capture_kinds_are_preserved() {
    let src = "fn main() -> Int {\n\
                 var row = read `{name:word},{port:int}`\n\
                 row.port + 1\n\
               }";
    assert!(
        !has_type_error(src),
        "the `port` capture is Int even when an earlier capture is Word"
    );
    // The types themselves, not just the absence of a complaint: each capture
    // keeps its own kind rather than collapsing to the first recognizable one.
    assert_eq!(
        scheme_of("var row = read `{name:word},{port:int}`", "row").as_deref(),
        Some("{ name: Text, port: Int }")
    );
}

/// A capture body is a full parser expression — nested calls and a nested
/// template included — and the type it synthesizes is the body's own. §7.7's
/// monkey line is the first case.
#[test]
fn a_capture_body_is_a_full_parser_expression() {
    for (src, expected) in [
        (
            "var m = read `Starting items: {items:csv(int)}`",
            "{ items: Vec[Int] }",
        ),
        ("var m = read `{x:optional(int)}`", "{ x: Option[Int] }"),
        ("var m = read `{s:sep(\"-\", word)}`", "{ s: Vec[Text] }"),
        ("var m = read `{c:one_of(\"^v<>\")}`", "{ c: Char }"),
        // Anonymous captures keep §7.3's scalar/tuple rule, and the body's own
        // type is what fills it.
        ("var m = read `{csv(int)}`", "Vec[Int]"),
        (
            "var m = read `{a:int} {b:csv(word)}`",
            "{ a: Int, b: Vec[Text] }",
        ),
    ] {
        assert!(!has_input_error(src), "{src} must be accepted");
        assert_eq!(
            scheme_of(src, "m").as_deref(),
            Some(expected),
            "{src} must synthesize the body's own type"
        );
    }

    // A capture body may hold a template of its own, so the lexer must not
    // close the token at the first inner backtick. Asserted on the AST rather
    // than the rendered type, because an anonymous enum renders as `{ g:  }` —
    // a display gap that belongs to whoever owns `TypeDb::render`.
    {
        use praxis_input_parser::{ParserAst, TemplatePart};
        let src = "var m = read `{g:choice(Pt: `{x:int},{y:int}`, Name: word)}`";
        assert!(!has_input_error(src), "a nested template is a parser body");
        match parser_ast_of(src) {
            ParserAst::Template { parts, .. } => match &parts[0] {
                TemplatePart::Capture { name, parser, .. } => {
                    assert_eq!(name.as_ref().map(|n| n.as_str()), Some("g"));
                    match parser.as_ref() {
                        ParserAst::Choice { cases, .. } => {
                            assert_eq!(cases.len(), 2);
                            assert_eq!(cases[0].0, "Pt");
                            assert!(matches!(cases[0].1, ParserAst::Template { .. }));
                            assert!(matches!(cases[1].1, ParserAst::Atomic { .. }));
                        }
                        other => panic!("expected Choice, got {other:?}"),
                    }
                }
                other => panic!("expected a capture, got {other:?}"),
            },
            other => panic!("expected Template, got {other:?}"),
        }
    }

    // And a malformed body reports rather than being read as something else.
    for src in [
        "var m = read `{x:csv(int, int)}`",
        "var m = read `{x:frobnicate(int)}`",
        "var m = read `{x:sep(\"\", int)}`",
    ] {
        assert!(has_input_error(src), "{src} must report");
    }
}

#[test]
fn unknown_template_capture_parser_is_diagnosed() {
    let src = "var value = read `{value:intr}`";
    assert!(
        has_input_error(src),
        "a misspelled capture parser must not silently default to Int"
    );
    // Any `I0xx` satisfies the line above; only I012 satisfies ADR-051, which
    // allocated `UnknownCaptureKind` for exactly this rule.
    assert!(
        reports_input_code(src, praxis_source::DiagCode::UnknownCaptureKind),
        "the code ADR-051 allocated for this is I012, not the generic I030"
    );
}

/// A template scan error reports the code ADR-051 allocated for its own rule —
/// I011, I012, I013 — rather than being flattened into the generic
/// `TemplateScan` (I030).
#[test]
fn a_template_scan_error_reports_the_code_its_own_rule_was_given() {
    use praxis_source::DiagCode;

    for (src, code) in [
        ("var v = read `{9x:int}`", DiagCode::InvalidCaptureName),
        ("var v = read `{value:intr}`", DiagCode::UnknownCaptureKind),
        (
            "var v = read `{x:frobnicate(int)}`",
            DiagCode::UnknownConstructor,
        ),
        (
            "var v = read `{x:csv(int, int)}`",
            DiagCode::ConstructorArity,
        ),
        // The `I030` rows are the ones with no code of their own, which is what
        // makes them the control for the four above. Both reach it through a
        // *closed* template: ADR-094 ends a template at the line it opens on,
        // so an unclosed one is the lexer's `T002` and its interior is never
        // scanned at all.
        ("var v = read `{}`", DiagCode::TemplateScan),
        ("var v = read `bad\\q escape`", DiagCode::TemplateScan),
    ] {
        assert!(
            reports_input_code(src, code),
            "{src} must report {code:?}, not the generic template-scan code"
        );
    }
}

#[test]
fn unknown_parser_constructor_is_diagnosed() {
    let src = "var value = read frobnicate(int)";
    assert!(
        has_input_error(src),
        "unknown constructor conversion must emit I010-style feedback"
    );
}

#[test]
fn optional_rejects_extra_arguments() {
    let src = "var value = read optional(int, word)";
    assert!(
        has_input_error(src),
        "special constructors must validate source arity before discarding arguments"
    );
}

/// Every §7.5 constructor validates its arguments before it builds anything: a
/// wrong argument count, a wrong argument *kind*, and a name with no row at all
/// each report, rather than a truncated parser being built in silence.
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
        let src = format!("var value = read {call}");
        // **Every** error, not just the `Input` category: a shape like
        // `fill: 0` can fail at the *grammar* with `P001`, which an Input-only
        // filter cannot see.
        assert_eq!(
            errors_of(&src),
            Vec::<String>::new(),
            "`{call}` is §7.5's own shape and must be accepted"
        );
        assert_eq!(
            scheme_of(&src, "value").as_deref(),
            Some(expected),
            "`{call}` must build the parser it names, not a truncated one"
        );
    }

    // **The value, not the acceptance.** `grid(P, ragged, fill: v)`'s
    // synthesized type is `Grid[Char]` whatever `v` is, so the row above cannot
    // see a dropped fill. Both spellings §7.5 writes, both front ends, asserted
    // on the built AST.
    //
    // A quoted fill arrives decoded, like every other parser string literal, and
    // one containing the argument separator is still one value — the delimiter
    // search has to respect quoting rather than cutting `","` in half.
    for (fill_src, expected_fill) in [
        ("0", "0"),
        ("9", "9"),
        ("\"-\"", "-"),
        ("\" \"", " "),
        ("\",\"", ","),
    ] {
        let call = format!("grid(char, ragged, fill: {fill_src})");
        for src in [
            format!("var value = read {call}"),
            // The capture-body front end reaches the same builder through a
            // template, so identical text goes down both front ends.
            format!("var value = read `{{v:{call}}}`"),
        ] {
            assert_eq!(
                errors_of(&src),
                Vec::<String>::new(),
                "`{call}` is §7.5's own spelling of a ragged grid"
            );
            assert_eq!(
                ragged_fill_of(&parser_ast_of(&src)).as_deref(),
                Some(expected_fill),
                "`{src}` must carry its fill value into the AST"
            );
        }
    }

    // A name with no row at all.
    assert!(
        reports_input_code("var v = read frobnicate(int)", DiagCode::UnknownConstructor),
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
        let src = format!("var v = read {call}");
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
        let src = format!("var v = read {call}");
        assert!(has_input_error(&src), "`{call}` is {why}");
    }

    // `block()` with nothing in it has no fields and consumes nothing.
    assert!(
        has_input_error("var v = read block()"),
        "a `block` needs at least one item"
    );

    // **A keyword with no value at all.** The shape table sees `fill:` present
    // and is satisfied; only the builder can see that nothing followed the
    // colon, and a ragged grid padded with `""` is not what was written.
    for src in [
        "var v = read grid(char, ragged, fill:)",
        "var v = read `{g:grid(char, ragged, fill:)}`",
        "var v = read `{g:grid(char, ragged, fill: \"\")}`",
    ] {
        assert!(
            !errors_of(src).is_empty(),
            "`{src}` gives `fill:` no value to pad with"
        );
    }
}

/// The file-level half of the nested-template span rebase.
///
/// A capture body's spans are the scanner's, relative to the template's
/// interior; `convert_template` rebases them onto the file by `token_start + 1`.
/// A nested template's parts are two levels down — `shift_part_spans` reaches
/// the capture's parser, and if that parser is itself a `Template`, the
/// `Template` arm of `ParserAst::shift_spans` has to recurse into *its* parts.
///
/// Nothing else covers that recursion: the input-parser crate's own span gate
/// reaches the shifting machinery through `body::parse_expr` only, where the
/// shifted parts hold `Atomic` parsers and the `Template` arm never runs. So
/// deleting `shift_part_spans(parts, delta)` from that arm leaves the rest of
/// the suite green while every caret under a nested template in real source
/// moves back by `token_start + 1`.
///
/// This therefore asserts a **rendered caret**, from `analyze`, on the file
/// offsets — at one level of nesting and at two. Without the recursion both
/// spans come back short by exactly the base, naming `read `{a:` instead of the
/// call.
#[test]
fn a_caret_under_a_nested_template_names_the_text_it_points_at() {
    // A `choice` with a duplicate case is reported against the `choice` node's
    // own span, which is the span the rebase has to reach.
    let bad = "choice(P: int, P: word)";
    for src in [
        format!("var v = read `{{a:`{{b:{bad}}}`}}`"),
        format!("var v = read `{{a:`{{b:`{{c:{bad}}}`}}`}}`"),
    ] {
        let d = analyze(&src)
            .diagnostics
            .into_iter()
            .find(|d| d.code().category() == DiagnosticCategory::Input)
            .unwrap_or_else(|| panic!("{src} must report the duplicate case"));
        let span = d.primary().span;
        assert_eq!(
            &src[span.start().to_u32() as usize..span.end().to_u32() as usize],
            bad,
            "the caret must name the call it is about, in {src}"
        );
    }
}

/// A parser constructor's string literal goes through the one decoder every
/// other literal uses (`lower::unquote_text`), so escapes are decoded and
/// exactly one delimiting quote is stripped at each end.
#[test]
fn a_parser_string_literal_is_decoded_once_like_every_other_literal() {
    use praxis_input_parser::ParserAst;

    // `\t` is one tab, not the two characters `\` and `t`, so `sep("\t", int)`
    // splits on a tab.
    match parser_ast_of(r#"var v = read sep("\t", int)"#) {
        ParserAst::Sep { separator, .. } => assert_eq!(separator.as_str(), "\t"),
        other => panic!("expected Sep, got {other:?}"),
    }

    // One quote, not zero: the escaped quote is content, not a delimiter.
    match parser_ast_of(r#"var v = read one_of("\"")"#) {
        ParserAst::OneOf { chars, .. } => assert_eq!(chars, "\""),
        other => panic!("expected OneOf, got {other:?}"),
    }

    // Both real quotes survive: stripping a *run* of quotes would decode this
    // to the empty separator, which is not a legal one.
    match parser_ast_of(r#"var v = read sep("\"\"", int)"#) {
        ParserAst::Sep { separator, .. } => assert_eq!(separator.as_str(), "\"\""),
        other => panic!("expected Sep, got {other:?}"),
    }

    // And an unknown escape is preserved exactly as `unquote_text` preserves
    // it — which is how the two are shown to be one decoder.
    match parser_ast_of(r#"var v = read sep("\q", int)"#) {
        ParserAst::Sep { separator, .. } => assert_eq!(separator.as_str(), r"\q"),
        other => panic!("expected Sep, got {other:?}"),
    }
}

/// §7.5: "`repeated(parser)` may appear only as the final named argument".
/// Both halves are checked — at most one tail, and it must be written last —
/// because otherwise the parser that runs is not the one written.
#[test]
fn a_repeated_tail_is_last_and_singular() {
    use praxis_source::DiagCode;

    // Misordered: `boards` consumes every remaining section, so `draws` after
    // it can never match.
    assert!(
        reports_input_code(
            "var b = read sections(boards: repeated(matrix(int)), draws: csv(int))",
            DiagCode::MisplacedRepeatedTail
        ),
        "a tail before another field silently reordered the call"
    );

    // Two tails: `sections` takes at most one, and neither may be dropped.
    assert!(
        reports_input_code(
            "var b = read sections(a: repeated(int), b: repeated(int))",
            DiagCode::MisplacedRepeatedTail
        ),
        "`sections` takes at most one tail"
    );

    // Outside `sections` there is nothing to repeat over.
    assert!(
        reports_input_code(
            "var b = read repeated(int)",
            DiagCode::MisplacedRepeatedTail
        ),
        "a bare `repeated(...)` is not a parser"
    );

    // And the legal shape is clean and builds the tail last —
    // `tests/aoc-corpus/m9_bingo.px`'s own call.
    let legal = "var b = read sections(draws: csv(int), boards: repeated(matrix(int)))";
    assert!(!has_input_error(legal), "the ordered form is legal");
    assert_eq!(
        scheme_of(legal, "b").as_deref(),
        Some("{ draws: Vec[Int], boards: Vec[Grid[Int]] }"),
        "the tail is the last field and it is a Vec of the repeated parser's result"
    );
}

/// A keyword belongs to a *constructor*, so the constructor is what answers
/// whether a name is one (`Constructor::keyword_arg`) — never the argument's
/// name alone — and `CallArg::Keyword` and `CallArg::Named` do not collapse
/// onto one `ArgKind`.
///
/// If they did, `check_call` would accept `fill:` on a constructor with no such
/// keyword as a well-shaped named argument and the builders' `filter_map` would
/// throw it away: a `sections` field or a `block` item named `fill` or `skip`
/// gone from the record with **no diagnostic**.
#[test]
fn a_field_named_fill_or_skip_is_a_field_and_not_a_dropped_keyword() {
    use praxis_input_parser::{BlockItem, ParserAst};

    // `sections` has no keyword argument, so `fill:` is a field.
    let src = "var v = read sections(rules: lines(int), fill: lines(int))";
    assert!(!has_input_error(src), "`fill` is a section name here");
    assert_eq!(
        scheme_of(src, "v").as_deref(),
        Some("{ rules: Vec[Int], fill: Vec[Int] }"),
        "the `fill` field must be in the record"
    );

    // Same for a `block` item…
    let src = "var v = read block(`{id:int}`, fill: lines(int))";
    assert!(!has_input_error(src));
    match parser_ast_of(src) {
        ParserAst::Block { items, .. } => {
            assert_eq!(items.len(), 2, "the `fill` item must survive");
            assert!(matches!(&items[1], BlockItem::Named { name, .. } if name == "fill"));
        }
        other => panic!("expected Block, got {other:?}"),
    }

    // …and for a `choice` case.
    match parser_ast_of("var v = read choice(A: int, skip: word)") {
        ParserAst::Choice { cases, .. } => {
            assert_eq!(cases.len(), 2, "the `skip` case must survive");
            assert_eq!(cases[1].0, "skip");
        }
        other => panic!("expected Choice, got {other:?}"),
    }

    // And a keyword the constructor really does have still works, still
    // reaches the builder, and a wrong one is still refused.
    match parser_ast_of(r#"var v = read chars(one_of("ab"), skip: newlines)"#) {
        ParserAst::Characters { skip, .. } => {
            assert_eq!(skip, praxis_input_parser::SkipPolicy::Newlines);
        }
        other => panic!("expected Characters, got {other:?}"),
    }
    assert!(
        has_input_error(r#"var v = read chars(one_of("ab"), fill: 0)"#),
        "`chars` has no `fill:`"
    );
}

/// **ADR-073.** The two front ends — the HIR bridge walking rowan, and the
/// capture-body parser reading text — apply *one* shape check for the
/// `repeated(...)` tail marker: both call
/// `praxis_input_parser::build_repeated_tail`.
///
/// Every case is asserted through **both** spellings, and on the same code, so
/// the two cannot drift without failing here.
#[test]
fn both_front_ends_apply_one_repeated_tail_rule() {
    use praxis_input_parser::ParserAst;
    use praxis_source::DiagCode;

    for (call, code, why) in [
        (
            "sections(draws: csv(int), boards: repeated(matrix(int), word, int))",
            DiagCode::ConstructorArity,
            "the tail marker takes one argument; two were dropped in silence",
        ),
        (
            "sections(draws: csv(int), boards: repeated())",
            DiagCode::ConstructorArity,
            "an empty tail marker reported nothing at all",
        ),
        (
            r#"sections(a: lines(int), b: repeated("x"))"#,
            DiagCode::InvalidConstructorArgument,
            "the tail marker's argument must be a parser",
        ),
    ] {
        assert!(
            reports_input_code(&format!("var v = read {call}"), code),
            "rowan front end: `{call}` — {why}"
        );
        assert!(
            reports_input_code(&format!("var v = read `{{b:{call}}}`"), code),
            "capture-body front end: `{call}` — {why}"
        );
    }

    // And the legal call builds the tail it names, through both front ends —
    // so "reject the bad one" was not bought by rejecting the good one.
    let legal = "sections(draws: csv(int), boards: repeated(matrix(int)))";
    for src in [
        format!("var v = read {legal}"),
        format!("var v = read `{{b:{legal}}}`"),
    ] {
        let ast = parser_ast_of(&src);
        let sections = match &ast {
            ParserAst::SectionsNamed { .. } => ast.clone(),
            ParserAst::Template { parts, .. } => match &parts[0] {
                praxis_input_parser::TemplatePart::Capture { parser, .. } => (**parser).clone(),
                other => panic!("expected a capture, got {other:?}"),
            },
            other => panic!("expected SectionsNamed or Template, got {other:?}"),
        };
        match sections {
            ParserAst::SectionsNamed {
                fields,
                repeated_tail,
                ..
            } => {
                assert_eq!(fields.len(), 1, "{src}");
                let (name, tail) = repeated_tail.expect("a tail");
                assert_eq!(name, "boards", "{src}");
                assert!(matches!(*tail, ParserAst::Matrix { .. }), "{src}");
            }
            other => panic!("expected SectionsNamed, got {other:?}"),
        }
    }
}

/// **The lexer and the template scanner must agree about where a template
/// ends**, and the agreement is now structural: both call
/// `praxis_syntax::template::template_end` instead of implementing one rule
/// twice.
///
/// Two copies of the rule drift: a brace counter that counts `{`/`}` everywhere
/// disagrees with one that skips string literals, and `` `{c:one_of("{")}` `` —
/// legal §7.5, and accepted by the scanner — then leaves the lexer's counter
/// above zero at the closing backtick, which it reads as an *opener*: the rest
/// of the file in one token, plus a false `T002`.
///
/// This test lives here because it is the only place the two layers meet.
/// `praxis-input-parser` must not depend on `praxis-parser` (ADR-023 fixes that
/// direction) and `praxis-parser` knows nothing of the scanner, so neither
/// crate's own suite can drive both. This one drives the **same strings**
/// through the lexer, the scanner, and the whole compile pipeline.
#[test]
fn the_lexer_and_the_scanner_agree_on_where_a_template_ends() {
    use praxis_source::FileId;
    use praxis_syntax::SyntaxKind;

    for template in [
        // A delimiter inside a string literal is text, not structure.
        r#"`{c:one_of("{")}`"#,
        r#"`{c:one_of("}")}`"#,
        r#"`{s:sep("{", int)}`"#,
        r#"`{c:one_of("`")}`"#,
        // A nested template is part of the same token.
        "`{g:choice(A: `{x:int}`, B: word)}`",
        "`{a:choice(A: `{b:choice(C: `{c:int}`)}`)}`",
        // Ordinary shapes, so a rule that broke these would be caught too.
        "`{name:word},{port:int}`",
        r#"`He said "hi": {x:int}`"#,
    ] {
        // 1. The lexer: one token, covering exactly the template, no complaint.
        let src = format!("var p = {template}\nvar q = 1\n");
        let lexed = praxis_parser::lex(FileId::SYNTHETIC, &src);
        assert!(
            lexed.diagnostics.is_empty(),
            "{template}: the lexer reported {:?}",
            lexed.diagnostics
        );
        let tokens: Vec<_> = lexed
            .tokens
            .iter()
            .filter(|t| t.kind == SyntaxKind::BacktickTemplate)
            .collect();
        assert_eq!(tokens.len(), 1, "{template}: not one template token");
        let token_text = &src[tokens[0].span.start().to_usize()..tokens[0].span.end().to_usize()];
        assert_eq!(
            token_text, template,
            "{template}: the token is not the template"
        );

        // 2. The scanner, on the interior the lexer just delimited.
        let interior = token_text
            .strip_prefix('`')
            .and_then(|s| s.strip_suffix('`'))
            .expect("the token is delimited by backticks");
        assert!(
            praxis_input_parser::scan_template(interior).is_ok(),
            "{template}: the lexer accepts what the scanner refuses"
        );

        // 3. And end to end, which is what a user sees.
        assert!(
            !has_input_error(&format!("var v = read lines({template})")),
            "{template}: rejected by the pipeline"
        );
    }
}

// --- closure escape analysis ------------------------------------------------

#[test]
fn immediately_invoked_closure_boxes_its_mutable_capture() {
    let src = "fn main() -> Int { var count = 0; (|n| { count += n; count })(1) }";
    let (analysis, module) = analyze_and_lower(src);
    let count = analysis
        .names
        .all()
        .iter()
        .find(|s| s.name == "count" && s.kind == SymbolKind::Var)
        .expect("count var")
        .id;
    assert!(
        module.escaping_vars.contains(&count),
        "a closure in Call.callee_expr still requires its captured var to be boxed"
    );
}

/// A capture whose first sighting is an assignment *target* keeps the type of
/// the binding it names. Inference records a type at a name it *reads*; a write
/// leaves no such record, so the capture has to take the binding's type rather
/// than a fresh variable, or the env slot carries `?T` for a known `Int`.
#[test]
fn a_capture_first_seen_as_an_assignment_target_keeps_its_type() {
    let src =
        "fn main() -> Int { var total = 0\n  var add = |n| { total = n }\n  add(5)\n  total }";
    let (analysis, module) = analyze_and_lower(src);

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

/// The **outer** closure of `|a| |b| b + base` carries `base` as a capture,
/// with the binding's type and the right storage.
///
/// The type assertion is the load-bearing one. A missing capture does not fail
/// loudly — MIR fills the inner closure's env slot with `Unit` — so a captured
/// `Text` becomes a well-typed program that answers `Unit` at run time. A
/// capture whose `ty` renders as anything but the binding's type is that hole.
#[test]
fn a_curried_closures_outer_literal_carries_the_transitive_capture() {
    fn outer_closure(b: &crate::TypedBlock) -> &crate::TypedExpr {
        fn find(e: &crate::TypedExpr) -> Option<&crate::TypedExpr> {
            if matches!(e, crate::TypedExpr::Closure { .. }) {
                return Some(e);
            }
            e.children()
                .find_map(find)
                .or_else(|| e.blocks().find_map(find_in_block))
        }
        fn find_in_block(b: &crate::TypedBlock) -> Option<&crate::TypedExpr> {
            b.stmts
                .iter()
                .find_map(|s| crate::stmt_exprs(s).find_map(find))
                .or_else(|| find(&b.tail))
        }
        find_in_block(b).expect("the outer closure")
    }

    fn captures_of(src: &str) -> Vec<(String, String, crate::capture::CaptureKind)> {
        let (analysis, module) = analyze_and_lower(src);
        let crate::TypedItem::Fn(main) = &module.items[0];
        let crate::TypedExpr::Closure { captures, .. } = outer_closure(&main.body) else {
            unreachable!("outer_closure returns a closure")
        };
        captures
            .iter()
            .map(|c| (c.name.clone(), analysis.db.render(c.ty), c.kind))
            .collect()
    }

    let bare =
        captures_of("fn main() -> Int { var base = 10\n  var mk = |a| |b| b + base\n  mk(5)(1) }");
    assert_eq!(bare.len(), 1, "the outer closure captures `base`: {bare:?}");
    assert_eq!(bare[0].0, "base");
    assert_eq!(
        bare[0].1, "Int",
        "the capture carries the binding's type; a `Unit` here is the silent hole"
    );
    assert!(matches!(bare[0].2, crate::capture::CaptureKind::ByValue));

    // One pair of braces cannot change the environment.
    let braced = captures_of(
        "fn main() -> Int { var base = 10\n  var mk = |a| { |b| b + base }\n  mk(5)(1) }",
    );
    assert_eq!(bare, braced, "the two spellings lower to the same captures");

    // A reassigned binding is shared through a cell, transitively too — a
    // `Unit` in this slot is dereferenced as a `VarCell` and segfaults.
    let cell = captures_of(
        "fn main() -> Int { var base = 10\n  base = 20\n  var mk = |a| |b| b + base\n  mk(5)(1) }",
    );
    assert_eq!(cell.len(), 1);
    assert_eq!(cell[0].0, "base");
    assert!(
        matches!(cell[0].2, crate::capture::CaptureKind::ByCell),
        "a transitively captured reassigned `var` is shared through a cell: {cell:?}"
    );
}

/// The one child walker really does cover the enum. A closure is placed in
/// every expression *field* the macro lists, and the walk must find all of them
/// — a field left out of a variant's row loses its subtree silently.
///
/// The program is deliberately not type-correct in every position (a closure is
/// not an `Int`); lowering builds the nodes regardless, and it is the shape of
/// the tree this asks about, not its types.
#[test]
fn the_child_walker_reaches_every_expression_position() {
    // One closure per position, numbered so a failure names the missing one.
    let src = concat!(
        "struct R { f: Int }\n",
        "enum E { V(Int) }\n",
        "fn main(c: Bool, v: Vec[Int], k: Int) -> Int {\n",
        "  var a = |n| 1\n",                // Var init
        "  var b = |n| 2\n",                // Var init, reassigned below
        "  b = |n| 3\n",                    // Assign value
        "  var d = (|n| 4)(0)\n",           // Call.callee_expr
        "  out(|n| 5)\n",                   // Call.args
        "  var w = v.map(|n| 6)\n",         // MethodCall.args
        "  var y = v.map(|n| 7).len()\n",   // MethodCall.receiver
        "  var t = (|n| 8, |n| 9)\n",       // Tuple.elements
        "  var p = (|n| 10)\n",             // Paren.inner
        "  var u = !(|n| 11)\n",            // Unary.operand
        "  var z = (|n| 12) == (|n| 13)\n", // Bin.lhs / Bin.rhs
        "  var g = R { f: |n| 14 }.f\n",    // RecordLit.fields, FieldGet.receiver
        "  var e = V(|n| 15)\n",            // EnumVariant.args
        "  if (|n| 16)(0) { var h = |n| 17 } else { var i = |n| 18 }\n", // If cond + branches
        "  while c { var j = |n| 19 }\n",   // While.body
        "  for x in v { var l = |n| 20 }\n", // For.body
        "  var m = loop { var o = |n| 21\n  break |n| 22 }\n", // Loop.body, Break.value
        "  var q = match (|n| 23)(0) { _ => |n| 24 }\n", // Match.scrutinee + arms
        "  return |n| 25\n",                // Return.value
        "}\n"
    );
    let (parsed, _analysis, module) = parse_analyze_and_lower(src);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);

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

// --- type constructors validate their own arguments -------------------------

/// A wrong number of type arguments in an annotation is named where it was
/// written (`Y007`), not as a downstream `Y001` about a type the user never
/// wrote.
#[test]
fn a_wrong_type_argument_count_is_reported_at_the_annotation() {
    for src in [
        "fn main() -> Int { var m: Map[Text] = Map(); 0 }",
        "fn main() -> Int { var v: Vec[Int, Text] = Vec(); 0 }",
        // A nominal def has a parameter count too — `Option` is one definition
        // applied to arguments rather than a name stamped per site.
        "fn main() -> Int { var o: Option[Int, Text] = None; 0 }",
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
        "fn main() -> Int { var m: Map[Text, Int] = Map(); 0 }"
    ));
}

/// A declaration that names one member twice is rejected (`Y008`) rather than
/// registering a def whose second field no lookup can ever reach.
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
// The per-node inferred-type map, and lowering as its reader.
// ---------------------------------------------------------------------------

/// A program with a closure in each of the twenty-five expression positions the
/// child-walker gate enumerates, plus the shapes inference reaches without
/// going through `infer_expr` (branch, loop and function bodies).
///
/// Shared by the gates below: "every expression node has a type" is only worth
/// asserting over a tree that has every expression node in it.
const EVERY_EXPRESSION_POSITION: &str = concat!(
    "struct R { f: Int }\n",
    "enum E { V(Int) }\n",
    "fn main(c: Bool, v: Vec[Int], k: Int) -> Int {\n",
    "  var a = |n| 1\n",
    "  var b = |n| 2\n",
    "  b = |n| 3\n",
    "  var d = (|n| 4)(0)\n",
    "  out(|n| 5)\n",
    "  var w = v.map(|n| 6)\n",
    "  var y = v.map(|n| 7).len()\n",
    "  var t = (|n| 8, |n| 9)\n",
    "  var p = (|n| 10)\n",
    "  var u = !(|n| 11)\n",
    "  var z = (|n| 12) == (|n| 13)\n",
    "  var g = R { f: |n| 14 }.f\n",
    "  var e = V(|n| 15)\n",
    "  if (|n| 16)(0) { var h = |n| 17 } else { var i = |n| 18 }\n",
    "  while c { var j = |n| 19 }\n",
    "  for x in v { var l = |n| 20 }\n",
    "  var m = loop { var o = |n| 21\n  break |n| 22 }\n",
    "  var q = match (|n| 23)(0) { _ => |n| 24 }\n",
    "  return |n| 25\n",
    "}\n"
);

/// Inference records a type for *every* expression node it visits, and
/// it visits every expression node. The map is what lets lowering read instead
/// of re-deriving; a map with holes in it would just move the fresh-variable
/// fallback from lowering into whoever consumes the map.
#[test]
fn every_expression_node_has_a_recorded_type() {
    let (id, parsed) = parse_file(EVERY_EXPRESSION_POSITION);
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

/// Lowering never invents a type: it reads the map rather than falling back to
/// a fresh variable. `Y099` is what a miss looks like, and a program covering
/// every expression position produces none.
#[test]
fn lowering_invents_no_type_for_any_expression_position() {
    let (_, module) = analyze_and_lower(EVERY_EXPRESSION_POSITION);
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

/// A `NodeKey` is not a `TextRange`. A `PATH_EXPR` and the `Ident` token inside
/// it occupy the *same* range, which is why the per-node map cannot be keyed by
/// range beside `ref_types` — one would silently overwrite the other, exactly
/// where a name reference and its expression meet.
#[test]
fn a_node_key_separates_an_expression_from_the_name_inside_it() {
    use praxis_ast::AstNode;

    let src = "fn main() -> Int { var x = 1\n  x }\n";
    let (id, parsed) = parse_file(src);
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

/// A lowered branch carries the join inference computed, read from the map
/// rather than recomputed — recomputing gives a different answer whenever one
/// branch diverges.
#[test]
fn a_lowered_branch_carries_the_join_not_its_first_arm() {
    let src = concat!(
        "fn main(c: Bool, n: Int) -> Int {\n",
        "  var a = if c { panic(\"x\") } else { 1 }\n",
        "  var b = match n { 0 => panic(\"y\"), _ => 2 }\n",
        "  a + b\n",
        "}\n"
    );
    let (analysis, module) = analyze_and_lower(src);
    assert!(
        analysis.diagnostics.is_empty(),
        "{:?}",
        analysis.diagnostics
    );
    let main = fn_named(&module, "main");
    let mut rendered = Vec::new();
    for stmt in &main.body.stmts {
        if let crate::TypedStmt::Var { name, init, .. } = stmt {
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

/// A method name is not a name reference: it resolves to a catalog entry rather
/// than to a symbol. So it has no entry in `refs`, and its receiver and result
/// live in `method_refs` rather than in `ref_types` — the map that reference
/// consumers such as hover read.
#[test]
fn a_method_name_is_not_a_name_reference() {
    let src = "fn main(v: Vec[Int]) -> Int { v.len() }\n";
    let (id, parsed) = parse_file(src);
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

/// A constructor is a *symbol* with `SymbolKind::EnumVariant`, so every question
/// about "is this name a constructor" has one answer — including for the
/// prelude's `Some`/`None`, which no `enum` item declares.
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
    let src = "enum E { A }\nfn main() -> Int {\n  var A = 7\n  A\n}\n";
    let (_, module) = analyze_and_lower(src);
    let main = fn_named(&module, "main");
    assert!(
        matches!(main.body.tail, crate::TypedExpr::Path { .. }),
        "the local shadows the constructor: {:?}",
        main.body.tail
    );
    // The `var A = 7` must survive too: lowering the tail as a constructor
    // would discard the binding's value with it.
    assert_eq!(main.body.stmts.len(), 1, "the binding is still there");
}

/// A misspelled constructor is `Y122`; a constructor pattern against a type that
/// has no variants at all is `Y123`; and the arms that *are* right still work.
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

/// A non-exhaustive `match` is reported at the `match`, not at byte 0 of the
/// file — a span at byte 0 names neither of a program's two matches.
#[test]
fn a_non_exhaustive_match_is_reported_where_it_is_written() {
    let src = "enum E { A, B }\nfn main() -> Int {\n  var x = A\n  match x { A => 1 }\n}\n";
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

/// A record literal names every declared field exactly once and nothing else.
/// Each half has its own code, so a program with two mistakes reports two
/// things.
#[test]
fn a_record_literal_names_every_field_exactly_once() {
    let missing = analyze_and_lower_diags(
        "struct P { x: Int, y: Int }\nfn main() -> Int { var p = P { x: 1 }\n  p.x }",
    );
    assert!(
        missing.iter().any(|d| d.code().number() == 113),
        "a missing field is Y113: {missing:?}"
    );

    let unknown = analyze_and_lower_diags(
        "struct P { x: Int }\nfn main() -> Int { var p = P { x: 1, typo: 2 }\n  p.x }",
    );
    assert!(
        unknown.iter().any(|d| d.code().number() == 114),
        "an unknown field is Y114: {unknown:?}"
    );

    let duplicate = analyze_and_lower_diags(
        "struct P { x: Int }\nfn main() -> Int { var p = P { x: 1, x: 2 }\n  p.x }",
    );
    assert!(
        duplicate.iter().any(|d| d.code().number() == 115),
        "a duplicate field is Y115: {duplicate:?}"
    );

    let good = analyze_and_lower_diags(
        "struct P { x: Int, y: Int }\nfn main() -> Int { var p = P { y: 2, x: 1 }\n  p.x }",
    );
    assert!(
        good.is_empty(),
        "order is not the rule — every field, once: {good:?}"
    );
}

/// …and an unknown field's initializer is still *type-checked*, because it is
/// an expression the program wrote: skipping it would silently delete the call
/// in `Point { x: 1, typo: side_effect() }`.
#[test]
fn an_unknown_fields_initializer_is_still_checked() {
    let diags = analyze_and_lower_diags(
        "struct P { x: Int }\n\
         fn main() -> Int { var p = P { x: 1, typo: \"text\" + 1 }\n  p.x }",
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

/// Exhaustiveness is a question about *values*, so it is asked at every
/// position a value has — not only at the top. Comparing top-level variant
/// indices alone makes a one-variant enum exhaustive whatever its payload says.
#[test]
fn a_match_covers_every_payload_position_not_just_the_outer_constructor() {
    let enums = "enum Flag { On, Off }\nenum Wrapped { Wrap(Flag) }\n";

    // `Wrap` is the *only* variant of `Wrapped`, so a top-level check calls
    // this exhaustive. `Wrap(Off)` is a value it does not match.
    let one = analyze_and_lower_diags(&format!(
        "{enums}fn main() -> Int {{ var v = Wrap(On)\n  match v {{ Wrap(On) => 1 }} }}"
    ));
    assert!(
        one.iter().any(|d| d.code().number() == 120),
        "an uncovered payload constructor is Y120: {one:?}"
    );

    // …and covering both closes it, which is the half a blanket rejection of
    // nested patterns would also satisfy.
    let both = analyze_and_lower_diags(&format!(
        "{enums}fn main() -> Int {{ var v = Wrap(On)\n  \
         match v {{ Wrap(On) => 1, Wrap(Off) => 2 }} }}"
    ));
    assert!(both.is_empty(), "both payload cases are covered: {both:?}");

    // A wildcard payload covers every constructor under it, at any depth.
    let wild = analyze_and_lower_diags(&format!(
        "{enums}fn main() -> Int {{ var v = Wrap(On)\n  match v {{ Wrap(_) => 1 }} }}"
    ));
    assert!(wild.is_empty(), "`Wrap(_)` is all of Wrapped: {wild:?}");

    // …and so does a binding, which is the same constructor set by a different
    // pattern form.
    let bound = analyze_and_lower_diags(&format!(
        "{enums}fn main() -> Int {{ var v = Wrap(On)\n  match v {{ Wrap(f) => 1 }} }}"
    ));
    assert!(bound.is_empty(), "`Wrap(f)` is all of Wrapped: {bound:?}");

    // Two levels down, to show the recursion is not one special case.
    let deep = "enum Flag { On, Off }\nenum Inner { In(Flag) }\nenum Outer { Out(Inner) }\n";
    let nested = analyze_and_lower_diags(&format!(
        "{deep}fn main() -> Int {{ var v = Out(In(On))\n  match v {{ Out(In(On)) => 1 }} }}"
    ));
    assert!(
        nested.iter().any(|d| d.code().number() == 120),
        "the gap is two payloads deep: {nested:?}"
    );
}

/// An arm is unreachable when it matches no value the arms above it leave — a
/// coverage question, not the syntactic "is there a `_` above me".
#[test]
fn an_arm_is_unreachable_exactly_when_it_adds_no_coverage() {
    let y121 = |src: &str| {
        analyze_and_lower_diags(src)
            .iter()
            .filter(|d| d.code().number() == 121)
            .count()
    };

    // A repeated constructor, with no catch-all anywhere in the match.
    assert_eq!(
        y121(
            "enum E { A, B }\nfn main() -> Int { var v = A\n  match v { A => 1, A => 2, B => 3 } }"
        ),
        1,
        "the second `A` is dead, and only it"
    );

    // The purely syntactic case.
    assert_eq!(
        y121("enum E { A, B }\nfn main() -> Int { var v = A\n  match v { _ => 1, A => 2 } }"),
        1,
        "an arm after a catch-all"
    );

    // A payload an earlier arm already covered — invisible to a top-level
    // check, because both arms name the same single variant.
    assert_eq!(
        y121(
            "enum Flag { On, Off }\nenum Wrapped { Wrap(Flag) }\n\
             fn main() -> Int { var v = Wrap(On)\n  match v { Wrap(_) => 1, Wrap(On) => 2 } }"
        ),
        1,
        "`Wrap(On)` is inside `Wrap(_)`"
    );

    // …and the half a coverage check gets wrong in the other direction: arms
    // that each add something are all reachable.
    assert_eq!(
        y121(
            "enum Flag { On, Off }\nenum Wrapped { Wrap(Flag) }\n\
             fn main() -> Int { var v = Wrap(On)\n  match v { Wrap(On) => 1, Wrap(Off) => 2 } }"
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
         fn main() -> Int { var v = Wrap(On)\n  match v { Wrap(On) => 1 } }",
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

/// A constructor at every-wildcard and that constructor with its payload named
/// are the same test, because the builder pads a variant pattern to its payload
/// arity. The matrix pairs each column with a type; a row narrower than the
/// payload would pair them off by one.
///
/// The **bare** name is padded the same way — it still covers `Some` for
/// coverage purposes — but writing it is `Y124` (ADR-134); see
/// [`a_bare_name_for_a_variant_that_carries_a_payload_is_reported`].
#[test]
fn a_constructor_at_wildcards_is_that_constructor_at_any_payload() {
    for arm in ["Some(_)", "Some(n)"] {
        let src = format!(
            "fn main() -> Int {{ var o = Some(1)\n  match o {{ {arm} => 1, None => 0 }} }}"
        );
        assert!(
            is_clean_with_lower(&src),
            "`{arm}` plus `None` is all of Option[Int]"
        );
    }
    // …and without the `None` arm none of them is. Bare `Some` is in this half
    // too: it is padded before coverage runs, so `Y120` is still the answer
    // about what it leaves out, alongside the `Y124` about how it is written.
    for arm in ["Some", "Some(_)", "Some(n)"] {
        let src = format!("fn main() -> Int {{ var o = Some(1)\n  match o {{ {arm} => 1 }} }}");
        let diags = analyze_and_lower_diags(&src);
        assert!(
            diags.iter().any(|d| d.code().number() == 120),
            "`{arm}` alone leaves `None`: {diags:?}"
        );
    }
}

/// **ADR-134.** A bare variant name for a variant that *carries* a payload is
/// `Y124`, and `A(_)` is how "any payload" is written.
///
/// Padding a bare name to the variant's arity is enough for coverage, so
/// `match bla { A => …, B => …, C => … }` over `enum Bla { A(Int), B, C }` is
/// exhaustive — and its first arm reads exactly like `B` and `C`, which carry
/// nothing, to anyone who has not read the declaration. The code is what makes
/// the program say which it is.
///
/// Naming *fewer* inside parentheses is still the padding rule: `Pair(a)` on a
/// two-slot variant is legal, because the parentheses are the place the author
/// said what they were doing.
#[test]
fn a_bare_name_for_a_variant_that_carries_a_payload_is_reported() {
    for src in [
        "enum Bla { A(Int), B, C }\nvar bla = A(3)\nmatch bla { A => {} B => {} C => {} }",
        // At one slot and at two, and nested inside another pattern.
        "enum P { Pair(Int, Int) }\nfn main() -> Int { var p = Pair(1, 2)\n match p { Pair => 1 } }",
        "fn main() -> Int { var o = Some(Some(1))\n \
         match o { Some(Some) => 1, Some(None) => 2, None => 0 } }",
        // A `for` header is a pattern position too. (A closure parameter is not
        // reachable this way: `|Wrap|` is a parameter *named* `Wrap` — the
        // grammar reads a bare name in that position as a name, not a pattern.)
        "enum W { Wrap(Int) }\nfor Wrap in [Wrap(1)] { out(0) }",
    ] {
        let diags = analyze_and_lower_diags(src);
        assert!(
            diags.iter().any(|d| d.code().to_string() == "Y124"),
            "{src} must be Y124, got {diags:?}"
        );
    }

    // The fix is machine-applicable, and it names one `_` per slot (ADR-132).
    let diags = analyze_and_lower_diags(
        "enum P { Pair(Int, Int) }\nfn main() -> Int { var p = Pair(1, 2)\n match p { Pair => 1 } }",
    );
    let y124 = diags
        .iter()
        .find(|d| d.code().to_string() == "Y124")
        .expect("Y124");
    assert_eq!(
        y124.suggestions()
            .first()
            .and_then(|s| s.replacement.as_deref()),
        Some("Pair(_, _)"),
        "the fix writes a wildcard per slot: {y124:?}"
    );

    // A payload-*less* variant is untouched: there is nothing to name.
    assert!(is_clean_with_lower(
        "enum T { Empty, Wall }\nfn main() -> Int { var t = Empty\n match t { Empty => 1, Wall => 0 } }"
    ));
    // And a bare name that is *not* a variant of the scrutinee's enum is still
    // a binding, which is the binding rule and not this one.
    assert!(is_clean_with_lower(
        "enum T { Empty, Wall, Number(Int) }\n\
         fn main() -> Int { var t = Empty\n match t { Empty => 1, other => 2 } }"
    ));
}

/// The question a key has to answer is not "can this be hashed" — a `Vec`
/// hashes fine — it is "can this still be found after the program changes it"
/// (ADR-057 decision 3). Hashability and key-ness are two predicates, not one.
#[test]
fn a_mutable_collection_is_not_a_key() {
    // Every mutable collection, in a `Map` key position.
    for ctor in ["Vec", "Set", "Deque", "MinHeap", "MaxHeap"] {
        let src = format!(
            "fn main() -> Unit {{\n  var key = {ctor}()\n  key.push(1)\n  var m = Map()\n  m.insert(key, 1)\n}}"
        );
        assert!(
            has_type_error(&src),
            "a {ctor} cannot be a Map key, but was accepted"
        );
    }
    // A `Set` element is a key too, and so is a `Counter`'s.
    assert!(has_type_error(
        "fn main() -> Unit {\n  var key = Vec()\n  key.push(1)\n  var s = Set()\n  s.insert(key)\n}"
    ));
    assert!(has_type_error(
        "fn main() -> Unit {\n  var key = Vec()\n  key.push(1)\n  var c = Counter()\n  c.inc(key)\n}"
    ));
}

/// …and the rule is mutability, not container-ness. Every immutable shape is
/// still a key, including a tuple of them — which is Python's `tuple` rule and
/// the one a grid-coordinate program depends on.
#[test]
fn an_immutable_value_is_still_a_key() {
    assert!(!has_type_error(
        "fn main() -> Unit { var m = Map(); m.insert(1, 2) }"
    ));
    assert!(!has_type_error(
        "fn main() -> Unit { var m = Map(); m.insert(\"k\", 2) }"
    ));
    // A tuple of scalars — the shape every grid-position map uses.
    assert!(!has_type_error(
        "fn main() -> Unit { var m = Map(); m.insert((1, 2), 3) }"
    ));
    // An enum, including the prelude's Option.
    assert!(!has_type_error(
        "enum Dir { N, S }\nfn main() -> Unit { var s = Set(); s.insert(N) }"
    ));
    // A tuple with a mutable component is not, though: one is enough.
    assert!(has_type_error(
        "fn main() -> Unit {\n  var v = Vec()\n  v.push(1)\n  var m = Map()\n  m.insert((1, v), 3)\n}"
    ));
}

/// A heap orders what it holds, so its element type must have an order. This is
/// the same channel as the key rule and a different capability — `Text` is
/// orderable and is not a key requirement, a `Vec` is neither.
#[test]
fn a_heap_element_must_be_orderable() {
    assert!(has_type_error(
        "fn id(x: Int) -> Int { x }\nfn main() -> Unit { var h = MinHeap(); h.push(id) }"
    ));
    assert!(has_type_error(
        "fn main() -> Unit { var h = MaxHeap(); h.push((1, 2)) }"
    ));
    // Int and Text both have a runtime `compare`, so both are legal elements.
    assert!(!has_type_error(
        "fn main() -> Unit { var h = MinHeap(); h.push(1) }"
    ));
    assert!(!has_type_error(
        "fn main() -> Unit { var h = MinHeap(); h.push(\"a\") }"
    ));
}

/// The requirement survives a generic function, which is the half a direct
/// `insert` cannot show: `store`'s key parameter is unconstrained, the
/// requirement is claimed by its scheme, and the *call site* is what chooses a
/// type that cannot be a key.
///
/// The map is built inside `store`, not passed in: a `Map` *parameter* would be
/// an unresolved receiver, which is the deferred-method rule covered separately.
#[test]
fn a_key_requirement_reaches_through_a_generic_function() {
    let src = "fn store(k) -> Unit { var m = Map(); m.insert(k, 1) }\n\
               fn main() -> Unit {\n\
                 var key = Vec()\n\
                 key.push(1)\n\
                 store(key)\n\
               }";
    assert!(
        has_type_error(src),
        "the constraint must travel with `store`'s scheme to its call site"
    );
    // …and the same function is fine at a key type that is one.
    let ok = "fn store(k) -> Unit { var m = Map(); m.insert(k, 1) }\n\
              fn main() -> Unit { store(\"key\") }";
    assert!(!has_type_error(ok));
}

/// `parse` is **syntax**, not a call of a binding. The keyword is not a
/// `PATH_EXPR` inside the `PARSE_EXPR`, so there is no name to resolve and
/// `ParseExpr::text_expr` — "the first `Expr` child" — is the text argument
/// rather than the keyword's own path.
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
        "fn main() -> Int {\n  var s = \"12\"\n  parse(s, int)\n}"
    ));
}

/// Negation follows the operand's **type**, not the spelling of the literal
/// beneath it — a per-literal rule negates a `Float`-typed variable as an `Int`.
#[test]
fn negation_follows_the_operands_type_not_its_spelling() {
    assert!(!has_type_error("fn negate(x: Float) -> Float { -x }"));
    assert!(!has_type_error("fn negate(x: Int) -> Int { -x }"));
    // The literal cases still work, in both directions.
    assert!(!has_type_error("fn f() -> Float { -3.5 }"));
    assert!(!has_type_error("fn f() -> Int { -3 }"));
    // …and negation still has a type: an Int operand does not produce a Float.
    assert!(has_type_error("fn f(x: Int) -> Float { -x }"));
    // A Float expression that is not a literal.
    assert!(!has_type_error("fn f(x: Float) -> Float { -(x + 1.0) }"));
}

/// `%` has no `Float` lowering, so there is nothing to accept. MIR's
/// `FloatBinOp` has no `Rem` arm and `binop_to_float`'s defensive fallback is
/// `Add`, so an accepted `5.0 % 2.0` would compute `7.0`.
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

/// `%=` is the same operation as `%`, so it is the same `Y016` — in both
/// spellings a compound assignment has.
///
/// The numeric requirement beside it does not cover this: a `Float` *is*
/// numeric, so without this rule `f %= 2.0` passes `praxis check` while
/// `f % 2.0` is refused, and MIR is then asked for a float remainder that does
/// not exist.
#[test]
fn a_compound_remainder_on_a_float_is_the_same_y016_the_binary_one_is() {
    for src in [
        // The binding target.
        "var f = 5.0\nf %= 2.0",
        "fn bad(x: Float) -> Float { var f = x\nf %= 2.0\nf }",
        // The subscript target (ADR-064).
        "var m = Map()\nm[\"k\"] = 5.0\nm[\"k\"] %= 2.0",
    ] {
        let errors = errors_of(src);
        assert!(
            errors.iter().any(|e| e.contains("Y016")),
            "`{src}` should be a Y016 — `%` is not defined for Float (§4.12), \
             and `%=` is the same operation.\ngot {errors:?}"
        );
    }
    // The four operators that *are* defined stay defined, in both spellings…
    for op in ["+", "-", "*", "/"] {
        let binding = format!("var f = 5.0\nf {op}= 2.0");
        assert!(!has_type_error(&binding), "Float `{op}=` is defined");
        let subscript = format!("var m = Map()\nm[\"k\"] = 5.0\nm[\"k\"] {op}= 2.0");
        assert!(
            !has_type_error(&subscript),
            "Float `{op}=` through a subscript"
        );
    }
    // …and `%=` on an `Int` is defined, in both spellings.
    assert!(!has_type_error("var n = 7\nn %= 4"));
    assert!(!has_type_error(
        "var m = Map()\nm[\"k\"] = 7\nm[\"k\"] %= 4"
    ));
}

/// An `Int` is signed 64-bit (§4.3), so a literal outside that range names a
/// value the language cannot represent. Saturating to `i64::MAX` is not a
/// fallback: a saturated literal is a perfectly good `Int`, so the program runs
/// with a number nobody wrote instead of faulting.
#[test]
fn an_out_of_range_int_literal_is_reported_rather_than_saturated() {
    for src in [
        "fn main() -> Int { 9223372036854775808 }",
        "fn main() -> Int { 99999999999999999999999 }",
        // The separated spelling is the same literal and the same report.
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

/// **ADR-061.** A top-level `fn` in value position lowers to a *function value*,
/// and the typed tree says so.
///
/// A `TypedExpr::Path` would not do: its symbol has no local slot, so MIR
/// answers `Unit` and `Inst::CallIndirect` reads that Unit's payload as a
/// function pointer. The distinction is the symbol's **kind**: a `var` holding a
/// closure is a `Path` and has a `Func` type too, so the scheme cannot tell them
/// apart — the same reason `SymbolKind::EnumVariant` exists.
#[test]
fn a_fn_name_in_value_position_is_a_function_value() {
    let src = "fn double(n: Int) -> Int { n * 2 }\n\
               fn main() -> Int {\n  var f = double\n  var g = |n| n * 3\n  f(1) + g(1)\n}\n";
    let (analysis, module) = analyze_and_lower(src);
    let main = fn_named(&module, "main");

    // `var f = double` — a function value naming `double`, typed as `double`'s
    // own signature.
    let crate::TypedStmt::Var { init, .. } = &main.body.stmts[0] else {
        panic!("expected `var f = double`, got {:?}", main.body.stmts[0]);
    };
    let crate::TypedExpr::FnValue {
        callee_name, ty, ..
    } = init
    else {
        panic!("a `fn` in value position is a function value, got {init:?}");
    };
    assert_eq!(callee_name, "double");
    assert_eq!(analysis.db.render(*ty), "(Int) -> Int");

    // …and a `var` holding a *closure* is still a closure literal, so the
    // `FnValue` arm does not swallow the case it sits next to.
    let crate::TypedStmt::Var { init, .. } = &main.body.stmts[1] else {
        panic!("expected `var g = |n| n * 3`");
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
/// check` accepted. `Y018` names the remedy instead, and the remedy works: a
/// closure body *is* a call site.
#[test]
fn a_generic_fn_used_as_a_value_is_reported_rather_than_run() {
    let diags =
        analyze_and_lower_diags("fn id(x) { x }\nfn main() -> Int {\n  var f = id\n  f(3)\n}\n");
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y018"),
        "expected Y018, got {diags:?}"
    );
    // It is reported at *analysis*, so `praxis check` sees it.
    assert!(has_type_error(
        "fn id(x) { x }\nfn main() -> Int {\n  var f = id\n  f(3)\n}\n"
    ));

    // The remedy compiles, and so does a monomorphic function used as a value.
    assert!(is_clean_with_lower(
        "fn id(x) { x }\nfn main() -> Int {\n  var f = |n| id(n)\n  f(3)\n}\n"
    ));
    assert!(is_clean_with_lower(
        "fn double(n: Int) -> Int { n * 2 }\nfn main() -> Int {\n  var f = double\n  f(3)\n}\n"
    ));
    // And *calling* the generic function directly is untouched — the report is
    // about value position only.
    assert!(is_clean_with_lower(
        "fn id(x) { x }\nfn main() -> Int { id(3) }\n"
    ));
}

/// A builtin or a constructor named without being called is `Y022`, because
/// there is no value for the name to be.
///
/// `Y018`'s neighbour, one symbol kind over, and a blunter failure: a
/// monomorphic user `fn` at least *has* a value (a closure over its adapter,
/// ADR-061). A builtin has no adapter and a constructor is built at its call, so
/// both would otherwise lower to `Unit` — `out(pi)` printing `Unit`, and
/// `var h = abs` followed by `h(-3)` printing nothing at all and exiting 0.
#[test]
fn a_builtin_or_constructor_used_as_a_value_is_reported() {
    for src in [
        // The reported shape: `pi` is a nullary function, not a constant.
        "fn main() -> Unit { out(pi) }",
        "fn main() -> Int { var h = abs\n h(-3) }",
        "fn main() -> Unit { out(Some) }",
        "enum E { A(Int), B }\nfn main() -> Unit { var f = A\n out(f(1)) }",
        // Inside a composite, and as an argument.
        "fn main() -> Unit { out([pi]) }",
        "fn main() -> Unit { out([1, 2].map(abs)) }",
    ] {
        let diags = analyze(src).diagnostics;
        assert!(
            diags.iter().any(|d| d.code().to_string() == "Y022"),
            "{src} must be Y022, got {diags:?}"
        );
    }

    // The wording names the remedy, and which remedy depends on the arity.
    let nullary = analyze("fn main() -> Unit { out(pi) }").diagnostics;
    assert!(
        nullary[0].message().contains("call it: `pi()`"),
        "{}",
        nullary[0].message()
    );
    let unary = analyze("fn main() -> Unit { out(Some) }").diagnostics;
    assert!(
        unary[0].message().contains("write `|x| Some(x)`"),
        "{}",
        unary[0].message()
    );

    // Calling them is accepted, which is the whole point…
    for src in [
        "fn main() -> Unit { out(pi()) }",
        "fn main() -> Unit { out(abs(-3)) }",
        "fn main() -> Unit { out(Some(1)) }",
        "fn main() -> Unit { out([1, 2].map(|n| abs(n))) }",
    ] {
        assert!(!has_type_error(src), "{src} must be accepted");
    }
    // …and a **payload-less** variant is an ordinary value, not a function. The
    // type is what tells them apart, which is why `None` needs no exception.
    for src in [
        "fn main() -> Unit { out(None) }",
        "enum E { A(Int), B }\nfn main() -> Unit { var f = B\n out(f) }",
    ] {
        assert!(!has_type_error(src), "{src} must be accepted");
    }
}

/// A `struct`/`enum` inside a function body is reported where it is written,
/// not left silent.
///
/// `register_top_level` walks the source file's own statements, so a nested
/// declaration gets no symbol and no type: unreported, declaring one is accepted
/// in silence and *using* it is an `N001` about a name written two lines above.
/// The code is `N005` — the one a nested `fn` uses, because it is the same
/// mistake — and a use still reports its own `N001` as it does for a nested
/// `fn`.
#[test]
fn a_nested_type_declaration_is_reported_at_the_declaration() {
    // Declared and never used, so nothing else can report it.
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
    // A top-level `var` beside them, so "top level" is the file and not
    // "the first statement". The `var` is read by a *top-level* statement, not
    // by `main`: a `fn` reading a binding around it is `N007`, a different
    // check and one this test is not about.
    assert!(is_clean_with_lower(
        "var base = 1\nstruct Point { x: Int }\nvar total = base\nfn main() -> Int { 0 }\n"
    ));
}

/// A pattern naming more sub-patterns than the variant holds is reported
/// (`Y124`); naming fewer *inside parentheses* is still the padding rule.
///
/// The builder truncates the extra sub-patterns rather than reading the payload
/// past its end — `match w { Wrap(a, b) => a }` against a one-slot variant
/// lowers `b` and drops it — so the program would otherwise compile and run.
/// Truncating stays, because it is the safe lowering; accepting does not.
#[test]
fn a_pattern_naming_more_values_than_the_variant_holds_is_reported() {
    let diags = analyze_and_lower_diags(
        "enum W { Wrap(Int) }\n\
         fn main() -> Int {\n  var w = Wrap(7)\n  match w { Wrap(a, b) => a }\n}",
    );
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y124"),
        "expected Y124, got {diags:?}"
    );

    // A payload-less variant holds nothing, so *any* sub-pattern is too many.
    let diags = analyze_and_lower_diags(
        "enum W { Empty, Wrap(Int) }\n\
         fn main() -> Int {\n  var w = Empty\n  match w { Empty(a) => 1, Wrap(n) => n }\n}",
    );
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y124"),
        "a payload-less variant names zero values: {diags:?}"
    );

    // …and naming *fewer inside parentheses* is legal at every count — the
    // padding rule. The bare spelling is not in this list: it is the other half
    // of `Y124` (ADR-134).
    for src in [
        "enum W { Wrap(Int) }\nfn main() -> Int { var w = Wrap(7)\n match w { Wrap(_) => 1 } }",
        "enum W { Wrap(Int) }\nfn main() -> Int { var w = Wrap(7)\n match w { Wrap(n) => n } }",
        "enum P { Pair(Int, Int) }\nfn main() -> Int { var p = Pair(1, 2)\n match p { Pair(a) => a } }",
        "enum P { Pair(Int, Int) }\nfn main() -> Int { var p = Pair(1, 2)\n match p { Pair(a, b) => a + b } }",
    ] {
        assert!(is_clean_with_lower(src), "{src} must still be accepted");
    }
}

/// A `for` over an unannotated parameter is generic in the **iterable** and
/// monomorphic in the **element** — the two are not one variable.
///
/// If `iter_item` answered an unresolved receiver with *itself*, the loop
/// variable and the iterator would be the same type, and two things would
/// follow:
///
/// - `t = t + i` pins that one variable to `Int`, so the `for` reports `Y005`
///   "values of type `Int` cannot be iterated" about a parameter the program
///   never typed — a legal program rejected.
/// - With nothing to pin it, the loop variable's recorded type is the
///   *collection's*: `fn copy(vs) { var o = Vec()\n for v in vs { o.push(v) }\n
///   o }` infers `o: Vec[Vec[Int]]` and faults at run time with "value does not
///   have the declared type", out of a program `praxis check` accepted.
///
/// So the assertion is not only "accepted": it is what `total` *is*, `forall T.
/// (T) -> Int` — any iterable, of `Int`.
#[test]
fn an_unannotated_iterated_parameter_is_generic_in_the_iterable_not_its_element() {
    const TOTAL: &str = "fn total(r) { var t = 0\n for i in r { t = t + i }\n t }\n";

    // The rule, as the type. The iterable is quantified; the element is not,
    // because `t + i` said what it is.
    let scheme = scheme_of(TOTAL, "total").expect("total has a scheme");
    insta::assert_snapshot!(scheme, @"forall T. (T) -> Int");

    // …and it is satisfied by each of these iterables. `Vec` and `Range` also
    // *run*; a `BitSet` has no element accessor in the runtime, so for it this
    // is acceptance and no more.
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

    // The loop variable is the **element**, not the collection: `o` is a
    // `Vec[Int]`, so returning it as one is accepted and returning
    // `Vec[Vec[Int]]` is not.
    const COPY: &str = "fn copy(vs) { var o = Vec()\n for v in vs { o.push(v) }\n o }\n";
    assert!(!has_type_error(&format!(
        "{COPY}fn main() -> Int {{ var s = Vec()\n s.push(1)\n var d: Vec[Int] = copy(s)\n d.len() }}"
    )));
    assert!(has_type_error(&format!(
        "{COPY}fn main() -> Int {{ var s = Vec()\n s.push(1)\n var d: Vec[Vec[Int]] = copy(s)\n d.len() }}"
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

/// An `Iterable` requirement is discharged by **unifying** the item, so a
/// receiver that iterates at the wrong element type is reported.
///
/// `capability::check` answers iterability as a yes/no — its failure shape is
/// "the offending type", and "iterates, but not at that element type" is a
/// *mismatch*, not that. A constraint carried through generalization and
/// discharged by a yes/no at a differently-itemed iterable is silently accepted.
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

    // Not iterable **at all** is the channel's own `Y005`, restated here
    // because one function decides both outcomes.
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

/// A `struct`/`enum` that refers to itself is reported where it is declared
/// (`N006`, ADR-063, superseding ADR-052's silence), rather than registered with
/// a fresh variable.
///
/// The declaration pass registers types in dependency order (ADR-052 decision
/// 3), and a declaration in a cycle never becomes ready. Falling back to a fresh
/// type variable for the recursive member is not merely silence — a variable
/// unifies with everything, so `struct Node { next: Node, value: Int }` would
/// accept `Node { next: 7, value: 1 }` and **run** it: one unchecked member per
/// recursive declaration.
///
/// Supporting recursive types is a language feature and stays out of scope.
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
    // Through a collection. A `Vec[Node]` *is* representable — every Praxis
    // field holds a reference — so this is the same missing feature, reached
    // through the element type rather than the field type.
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
    // `C` is written above the recursive pair and names `A`, so a stalled pass
    // would leave it in the remainder too. It is unreported, and its field is a
    // real `A` — so a `Text` in it is a mismatch.
    let src = "struct C { a: A }\n\
               struct A { b: B }\n\
               struct B { a: A }\n\
               fn main() -> Unit { var c = C { a: \"not an A\" }\n out(c.a) }";
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
    // …and a non-recursive forward reference still resolves, in both
    // directions — the dependency-order rule this check must not break.
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
         fn main() -> Int { var n = Node { next: 7, value: 1 }\n n.value }",
    )
    .diagnostics;
    assert_eq!(
        diags.len(),
        1,
        "one report for the declaration and nothing else: {diags:?}"
    );
}

/// `&&` and `||` take two `Bool`s and produce one, and a divergent operand is
/// absorbed rather than reported.
///
/// There is no truthiness, so the type rule is the whole rule and it is the same
/// for both operators — the short-circuit is MIR's. The operands **join**
/// (ADR-053), as at every other branch point in the language: `panic` is
/// `Never`, and unifying instead would make `false && panic("x")` an "expected
/// Never, found Bool" `Y001` about the operator rather than about the program.
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

/// `p.0` reads a tuple element, and reading past the end — or off something
/// that is not a tuple — is `Y019` in *inference*.
///
/// The report is in inference and not at lowering, for `Y018`'s reason
/// (ADR-061): `praxis check` does not run lowering, so a program reported only
/// there is clean under `check` and fails under `run`. It is also **not** `Y112`
/// ("no field on this type"): a tuple has no field *names*.
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
    // …and a nested one, which the lexer has to split: `n.0.1` is two indices,
    // not an index followed by the float `0.1`.
    assert_eq!(expr_type("((1, \"a\"), 3).0.1"), "Text");

    // Through a binding, a parameter and a closure body.
    assert!(!has_type_error(
        "fn fst(p: (Int, Text)) -> Int { p.0 }\n\
         fn main() -> Int { var q = (1, \"a\")\n fst(q) }"
    ));
    assert!(!has_type_error(
        "fn main() -> Int { var f = |p: (Int, Int)| p.0 + p.1\n f((1, 2)) }"
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
    assert!(y019("fn f() -> Int { var n = 1\n n.0 }"));
    assert!(y019("fn f() -> Int { var t = \"ab\"\n t.0 }"));
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

/// The read form: `m[key]` is the type the receiver holds at that key, and
/// which receivers index is the catalog's answer.
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
            "fn f(v: Vec[Text]) -> Text { v[0] }\nvar _p = f(Vec())",
            "_p"
        ),
        Some("Text".to_string())
    );
    assert_eq!(
        scheme_of(
            "fn f(d: Deque[Text]) -> Text { d[0] }\nvar _p = f(Deque())",
            "_p"
        ),
        Some("Text".to_string())
    );
    assert_eq!(
        scheme_of(
            "fn f(m: Map[Text, Float]) -> Float { m[\"a\"] }\nvar _p = f(Map())",
            "_p"
        ),
        Some("Float".to_string())
    );
    // A `Counter`'s value is its count, whatever its key type is (§6.2).
    assert_eq!(
        scheme_of(
            "fn f(c: Counter[Text]) -> Int { c[\"a\"] }\nvar _p = f(Counter())",
            "_p"
        ),
        Some("Int".to_string())
    );
    assert_eq!(
        scheme_of(
            "fn f(g: Grid[Text]) -> Text { g[0, 0] }\nvar _p = f(Grid())",
            "_p"
        ),
        Some("Text".to_string())
    );
    // `Text` indexes to a char's scalar value, which is what `.get` answers too.
    assert_eq!(
        scheme_of("fn f(t: Text) -> Int { t[0] }\nvar _p = f(\"ab\")", "_p"),
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

    // The two spellings are two rows, and `Map`'s differ on purpose (§4.7): the
    // subscript is the assertion-like half and answers `V`, while `.get` is the
    // explicit-absence half and answers `Option[V]`. A change that made one
    // spelling the other would show here.
    assert!(has_type_error(
        "fn f(m: Map[Text, Int]) -> Int { m.get(\"a\") }"
    ));
    assert!(!has_type_error(
        "fn f(m: Map[Text, Int]) -> Option[Int] { m.get(\"a\") }"
    ));
}

/// A subscript on an unannotated parameter defers and is answered by the call
/// site, exactly as `values.sum()` is — because a subscript dispatches through
/// the same catalog.
#[test]
fn a_subscript_on_an_unannotated_parameter_is_answered_by_the_call_site() {
    // The requirement rides on the scheme: `first` is generic in its receiver.
    assert!(!has_type_error(
        "fn first(m, k) { m[k] }\nfn main() -> Int { var m = Map()\n m.insert(\"a\", 1)\n first(m, \"a\") }"
    ));
    // …and the *element* type is the answer, not a fresh variable: a `Text` result
    // used as an `Int` is a mismatch.
    assert!(has_type_error(
        "fn first(m, k) { m[k] }\nfn main() -> Int { var m = Map()\n m.insert(\"a\", \"x\")\n first(m, \"a\") }"
    ));
    // A call site whose receiver does not index at all is reported when the
    // requirement is discharged rather than accepted in silence.
    assert!(has_type_error(
        "fn first(m, k) { m[k] }\nfn main() -> Int { var s = Set()\n s.insert(1)\n first(s, 1) }"
    ));
    // A subscript is exactly as generic as a method call and no more: the
    // requirement **pins** its receiver (`pin_to_level`, ADR-057 decision 5),
    // so one function serves one receiver kind. Two kinds through one is a
    // `Y001` about the two signatures — the same answer `fn size(c) { c.len() }`
    // gives, which is what makes this a property of the channel rather than of
    // subscripts.
    assert!(has_type_error(
        "fn first(c, k) { c[k] }\n\
         fn main() -> Int { var m = Map()\n m.insert(\"a\", 1)\n \
         var v = Vec()\n v.push(2)\n first(m, \"a\") + first(v, 0) }"
    ));
    assert!(has_type_error(
        "fn size(c) { c.len() }\n\
         fn main() -> Int { var m = Map()\n m.insert(\"a\", 1)\n \
         var v = Vec()\n v.push(2)\n size(m) + size(v) }"
    ));
    // One kind at two call sites is fine, which is the half that has to keep
    // working.
    assert!(!has_type_error(
        "fn first(c, k) { c[k] }\n\
         fn main() -> Int { var a = Vec()\n a.push(1)\n \
         var b = Vec()\n b.push(2)\n first(a, 0) + first(b, 0) }"
    ));
}

/// The store form: `m[key] = v` and `counts[key] += 1` reach the five
/// collections that have a store, and an assignment whose left side names no
/// storage is `Y021` rather than a parse error.
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

    // The five that store, plain and compound.
    assert!(!has_type_error("fn f(m: Map[Text, Int]) { m[\"a\"] = 1 }"));
    assert!(!has_type_error(
        "fn f(m: Map[Text, Int]) { m[\"a\"] = 1\n m[\"a\"] += 2 }"
    ));
    assert!(!has_type_error("fn f(c: Counter[Text]) { c[\"a\"] += 1 }"));
    assert!(!has_type_error("fn f(g: Grid[Int]) { g[0, 1] = 7 }"));
    assert!(!has_type_error(
        "fn f(v: Vec[Int]) { v[0] = 1\n v[0] += 2 }"
    ));
    assert!(!has_type_error(
        "fn f(d: Deque[Int]) { d[0] = 1\n d[0] += 2 }"
    ));

    // The stored type is checked against the collection's, in both positions.
    assert!(has_type_error(
        "fn f(m: Map[Text, Int]) { m[\"a\"] = \"x\" }"
    ));
    assert!(has_type_error("fn f(m: Map[Text, Int]) { m[0] = 1 }"));
    assert!(has_type_error("fn f(g: Grid[Int]) { g[0, 1] = \"x\" }"));
    assert!(has_type_error("fn f(v: Vec[Int]) { v[0] = \"x\" }"));
    assert!(has_type_error("fn f(v: Vec[Int]) { v[\"a\"] = 1 }"));
    assert!(has_type_error("fn f(d: Deque[Int]) { d[0] = \"x\" }"));

    // A `Text` reads through a subscript and has **no element store**, because
    // it is immutable (§4.3) — so this is reported rather than given one
    // silently. It is the only reader without a store; `Vec` and `Deque` have
    // both.
    assert!(y020("fn f(t: Text) { t[0] = 1 }"));
    assert!(!y020("fn f(t: Text) -> Char { t[0] }"));
    // And a receiver with no subscript at all is the same code from either side.
    assert!(y020("fn f(s: Set[Int]) { s[0] = 1 }"));

    // A left side that names no storage: `Y021`, rather than a parse error
    // about a missing statement separator, which says nothing about the
    // mistake. A **field** is not among them — it is a place (§4.5); see
    // `a_store_through_a_field_writes_the_slot_the_read_would_have_read`.
    assert!(y021("fn g() -> Int { 1 }\nfn f() { g() = 3 }"));
    assert!(y021("fn f(v: Vec[Int]) { v.len() += 1 }"));
    // …and the target's own mistakes are still reported, so the statement is not
    // simply discarded.
    assert!(has_name_error("fn f() { nope() = 3 }"));

    // A plain `var` assignment is untouched: it is a different statement kind and
    // `parse_stmt` still routes it there.
    assert!(!has_type_error("fn f() { var x = 1\n x = 2\n x += 3 }"));
    assert!(!y021("fn f() { var x = 1\n x = 2 }"));

    // A compound store still requires a numeric value, through the subscript
    // too: `m[k] += true` is the same mistake as `flag += false`.
    assert!(has_type_error(
        "fn f(m: Map[Text, Bool]) { m[\"a\"] = true\n m[\"a\"] += true }"
    ));
}

/// **A field is a place.** `p.x = 5` stores into the slot `p.x` reads (§4.5),
/// rather than being a `Y021` that leaves rebuilding the whole record as the
/// only spelling.
///
/// The assertions are about what a *plausible-but-wrong* implementation would
/// get wrong: that the store checks the field's type rather than accepting any
/// value, that it goes through the same `HasField` requirement the read does (so
/// an unannotated receiver defers rather than being refused, and a missing field
/// is `Y112` and not `Y021`), and that the compound forms carry the numeric rule
/// a `var` target has — including its one `Text` exception.
#[test]
fn a_store_through_a_field_writes_the_slot_the_read_would_have_read() {
    let struct_p = "struct P { x: Int, y: Text }\n";
    let y021 = |src: &str| -> bool {
        analyze(src)
            .diagnostics
            .iter()
            .any(|d| d.kind() == praxis_source::DiagCode::NotAnAssignmentTarget)
    };
    let no_field = |src: &str| -> bool {
        analyze(src)
            .diagnostics
            .iter()
            .any(|d| d.kind() == praxis_source::DiagCode::NoFieldOnType)
    };

    // Plain and compound, on a nominal record.
    assert!(!has_type_error(&format!(
        "{struct_p}fn f(p: P) {{ p.x = 5 }}"
    )));
    assert!(!has_type_error(&format!(
        "{struct_p}fn f(p: P) {{ p.x += 1\n p.x -= 2\n p.x *= 3\n p.x /= 4\n p.x %= 5 }}"
    )));
    // `+=` on a `Text` field is concatenation and needs no number, exactly as it
    // does on a `var` (ADR-085). The other four still need one.
    assert!(!has_type_error(&format!(
        "{struct_p}fn f(p: P) {{ p.y += \"z\" }}"
    )));
    assert!(has_type_error(&format!(
        "{struct_p}fn f(p: P) {{ p.y *= \"z\" }}"
    )));

    // …and none of these is `Y021`: the target does name storage.
    assert!(!y021(&format!("{struct_p}fn f(p: P) {{ p.x = 5 }}")));
    assert!(!y021(&format!("{struct_p}fn f(p: P) {{ p.x += 1 }}")));

    // The stored type is the *field's*, not whatever the value happens to be.
    assert!(has_type_error(&format!(
        "{struct_p}fn f(p: P) {{ p.x = \"five\" }}"
    )));
    assert!(has_type_error(&format!(
        "{struct_p}fn f(p: P) {{ p.y = 5 }}"
    )));

    // A field the record does not have is `Y112` — the read's report, from the
    // read's requirement — and not `Y021`, which would say the target names no
    // storage when the mistake is which storage it names.
    assert!(no_field(&format!("{struct_p}fn f(p: P) {{ p.z = 5 }}")));
    assert!(!y021(&format!("{struct_p}fn f(p: P) {{ p.z = 5 }}")));

    // An **unannotated** receiver defers on the constraint channel and is
    // answered by the call site, so a store is exactly as generic as a read:
    // this is the assertion that fails if the store resolves the field itself
    // instead of going through `infer_field_get`.
    assert!(!has_type_error(&format!(
        "{struct_p}fn bump(q) {{ q.x += 1 }}\nfn f(p: P) {{ bump(p) }}"
    )));
    // …and a call site whose argument has no such field is still reported.
    assert!(has_type_error(
        "struct P { x: Int }\nstruct Q { y: Int }\n\
         fn bump(q) { q.x += 1 }\nfn f(q: Q) { bump(q) }"
    ));

    // A `var` binding may point at a mutable object (§4.2), so the store is
    // legal through one — the same standing `var v = Vec[Int]()` / `v.push(1)`
    // has. `Y009` is about rebinding the *name*, which this is not.
    assert!(!has_type_error(&format!(
        "{struct_p}fn f() {{ var p = P {{ x: 1, y: \"a\" }}\n p.x = 2 }}"
    )));

    // `min=`/`max=` are §6.2's map updates, and their semantics is about an entry
    // that may be absent. A field is always present, so it is `Y016` (an operator
    // this type does not have) rather than a silent accept.
    assert!(has_type_error(&format!(
        "{struct_p}fn f(p: P) {{ p.x min= 3 }}"
    )));
    assert!(has_type_error(&format!(
        "{struct_p}fn f(p: P) {{ p.x max= 3 }}"
    )));

    // A **tuple element** is not a place: `p.0 = 1` has no store form, and the
    // report is the one that says so.
    assert!(y021("fn f(p: (Int, Int)) { p.0 = 1 }"));
}

/// `Counter[(Int, Int)]()` parses, and it means what the annotation says: the
/// element type is the written one, and a use that disagrees is a `Y001`.
///
/// §3.3 writes the explicit form. The element type is inferred from use as
/// well, so a bare `Counter()` proves nothing — which is why the assertions here
/// are about the *type*, not about the absence of a diagnostic.
#[test]
fn a_constructors_written_type_arguments_say_what_it_constructs() {
    // The written argument is the element type, at each arity the ctors have.
    assert_eq!(
        scheme_of("var c = Counter[(Int, Int)]()", "c"),
        Some("Counter[(Int, Int)]".to_string())
    );
    assert_eq!(
        scheme_of("var v = Vec[Text]()", "v"),
        Some("Vec[Text]".to_string())
    );
    assert_eq!(
        scheme_of("var m = Map[Text, Vec[Int]]()", "m"),
        Some("Map[Text, Vec[Int]]".to_string())
    );
    // Without the annotation the element stays quantified, which is the
    // difference the form exists to make.
    assert_eq!(
        scheme_of("var v = Vec[Int]()", "v"),
        Some("Vec[Int]".to_string())
    );

    // A use that agrees is clean; a use that disagrees is a `Y001`. This is the
    // pair that says the annotation *constrains* rather than decorates.
    assert!(!has_type_error(
        "fn main() -> Int { var c = Counter[Text]()\n c.inc(\"a\")\n c.len() }"
    ));
    assert!(has_type_error(
        "fn main() -> Int { var c = Counter[Text]()\n c.inc(1)\n c.len() }"
    ));
    assert!(has_type_error(
        "fn main() -> Int { var m = Map[Text, Int]()\n m.insert(\"a\", \"b\")\n m.len() }"
    ));
    // …and through the subscript, which reads the same annotation.
    assert!(!has_type_error(
        "fn main() -> Int { var c = Counter[(Int, Int)]()\n c[(1, 2)] += 1\n c[(1, 2)] }"
    ));
    assert!(has_type_error(
        "fn main() -> Int { var c = Counter[(Int, Int)]()\n c[\"a\"] += 1\n c.len() }"
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
    assert!(y007("var v = Vec[Int, Text]()"));
    assert!(y007("var m = Map[Text]()"));
    assert!(!y007("var m = Map[Text, Int]()"));

    // A type argument is an annotation, so its own mistakes are annotation
    // mistakes: an unknown name is `N002` and a *value* in type position `N003`.
    assert!(has_name_error("var v = Vec[Nope]()"));
    assert!(has_name_error(
        "fn f() -> Int { var n = 1\n var v = Vec[n]()\n 0 }"
    ));

    // An annotated binding and an annotated constructor agree, which is the
    // property that says the two spellings are one type language.
    assert!(!has_type_error("var c: Counter[Text] = Counter[Text]()"));
    assert!(has_type_error("var c: Counter[Int] = Counter[Text]()"));
}

/// The parser's closed list of type-constructor names and the compiler's are
/// the same list.
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

/// **ADR-067.** A file's top-level statements are lowered into one generated
/// item, in source order, and a file with none has no such item.
///
/// §3.2: "top-level statements are wrapped in a generated entry function". A
/// `lower` that walked the root for `fn`/`struct`/`enum` alone would drop them
/// between the typed tree and MIR, after they had type-checked.
#[test]
fn a_files_top_level_statements_become_one_generated_item() {
    let lowered = |text: &str| analyze_and_lower(text).1;
    // Not `entry_fn`: the second case asserts there is **no** entry item, which
    // a panicking lookup could not say.
    let entry_of = |module: &crate::TypedModule| -> Option<usize> {
        module.items.iter().find_map(|item| match item {
            crate::TypedItem::Fn(f) if f.name == crate::ENTRY_NAME => Some(f.body.stmts.len()),
            _ => None,
        })
    };

    // Three statements, one item — and the declarations between them stay their
    // own items, because a `fn` inside a `fn` is `N005`: the entry point cannot
    // be a source transformation that wraps the file.
    let module = lowered("out(1)\nfn f() -> Int { 2 }\nvar x = f()\nout(x)\n");
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
    let entry = entry_fn(&module);
    assert!(entry.params.is_empty());
    assert_eq!(entry.body.stmts.len(), 1);
    assert!(matches!(
        entry.body.tail,
        crate::TypedExpr::Lit {
            value: crate::Lit::Unit,
            ..
        }
    ));
    // And that tail is **spanless**. The file's whole range is the one span in
    // the program that is never the answer to "where?": the crash debugger
    // renders a temp's provenance as `@ "expr"`, so carrying it would print
    // every statement in the file collapsed onto one line.
    assert!(
        matches!(entry.body.tail, crate::TypedExpr::Lit { span, .. } if span == (0, 0)),
        "the synthesized tail names no source text"
    );
    // The *function* still spans the file: that is what the debugger's `source`
    // command renders, and the two spans are different questions.
    assert_ne!(entry.span, (0, 0), "the entry item keeps the file's extent");

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

/// A block that ends in a statement has a synthesized `Unit` tail, and that tail
/// names **no source text** — at every block, not just the entry point's.
///
/// The block's own span is the widest in reach and the least useful. Two
/// consumers read a temp's span and a wide one harms both: the debugger prints
/// it as `@ "expr"` provenance, so a `{ … }` spanning twenty lines would render
/// as twenty lines on one row; and `praxis_debugger`'s `fault_span` picks the
/// *narrowest* unfinished temp to decide which line a frame faulted on, a
/// question a whole-block span can only answer wrongly.
#[test]
fn a_synthesized_block_tail_carries_no_span() {
    let src = "fn f() -> Unit {\n  var x = 1\n}\n";
    let (_, module) = analyze_and_lower(src);
    let f = fn_named(&module, "f");
    assert!(
        matches!(
            f.body.tail,
            crate::TypedExpr::Lit {
                value: crate::Lit::Unit,
                span: (0, 0),
                ..
            }
        ),
        "a statement-terminated body gets a spanless Unit tail"
    );
    // A tail the source *did* write keeps its own span — the rule is about what
    // the compiler invents, not about erasing provenance generally.
    let src = "fn g() -> Int {\n  var x = 1\n  x\n}\n";
    let (_, module) = analyze_and_lower(src);
    let g = fn_named(&module, "g");
    let span = match &g.body.tail {
        crate::TypedExpr::Path { span, .. } => *span,
        other => panic!("expected the written tail `x`, got {other:?}"),
    };
    assert_eq!(&src[span.0 as usize..span.1 as usize], "x");
}

/// A `fn` body that names a binding declared outside it is reported (`N007`,
/// ADR-068) rather than compiling and answering wrongly.
///
/// ```praxis
/// var x = 1
/// fn f() { x }
/// out(f())          // Unit
/// ```
///
/// Unreported, that passes `praxis check` and prints `Unit`: the binding is a
/// local of whatever function encloses it, and a `fn` body has no slot for
/// another function's local. Through a closure it is worse —
/// `fn g() { |n| n + x }` captures a symbol with no slot, so `g()(1)` prints a
/// nine-digit number.
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

    // Both forms, including the closure one.
    assert!(reports_n007("var x = 1\nfn f() -> Int { x }\nout(f())\n"));
    assert!(reports_n007(
        "var x = 5\nfn g() { |n| n + x }\nout(g()(1))\n"
    ));
    // A `var` and a read through an assignment target, so it is the binding kind
    // that decides and not the expression position.
    assert!(reports_n007("var x = 1\nfn f() -> Int { x }\n"));
    assert!(reports_n007("var x = 1\nfn f() { x = 2 }\n"));
    // Nested one level deeper than the body itself: the boundary is the
    // function, not the block.
    assert!(reports_n007(
        "var x = 1\nfn f() -> Int { if true { x } else { 0 } }\n"
    ));

    // A closure at the **top level** captures: under ADR-067 both it and the
    // binding are inside the generated entry, so there is no boundary between
    // them. This is §4.10's own example.
    assert!(!reports_n007(
        "var offset = 10\nvar v = Vec()\nv.push(1)\nout(v.map(|x| x + offset).sum())\n"
    ));
    // …and a closure inside a `fn` capturing that `fn`'s own locals is the same
    // rule from the other side.
    assert!(!reports_n007(
        "fn f(v) { var k = 10\n v.map(|x| x + k).sum() }\n"
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
        "fn f(n: Int) -> Int { var m = n + 1\n if n < 1 { m } else { f(n - 1) } }\n"
    ));

    // One report per use site, and no cascade: the reference is still recorded,
    // so inference types the body as written and adds nothing.
    let diags = analyze_and_lower_diags("var x = 1\nfn f() -> Int { x + x }\n");
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
    let forward = analyze_and_lower_diags("fn f() -> Int { x }\nvar x = 1\n");
    assert!(forward
        .iter()
        .any(|d| d.kind() == praxis_source::DiagCode::UnknownName));
    assert!(!forward
        .iter()
        .any(|d| d.kind() == praxis_source::DiagCode::FunctionReadsOuterBinding));
}

/// `N007` offers two ways out — a parameter or a closure — and for a
/// **recursive** `fn` the second is one the compiler itself refuses:
/// a closure cannot name itself, because a `var`'s initializer is resolved in the
/// preceding environment, so `var f = |n| … f(n - 1) …` is `N001`.
///
/// ```praxis
/// var memo = 1
/// fn fact(n: Int) -> Int { if n <= 1 { memo } else { n * fact(n - 1) } }
/// ```
///
/// Recursion is exactly the case where threading state through the parameter
/// list hurts — three AoC solves reached ten, seven and five parameters doing it
/// — so it is the case where the unfollowable half of the advice is most likely
/// to be taken. The recursive form drops it and says why; the far commoner
/// non-recursive form keeps both ways out.
#[test]
fn a_recursive_fn_is_not_told_to_use_a_closure() {
    let n007 = |src: &str| -> Vec<praxis_source::Diagnostic> {
        analyze_and_lower_diags(src)
            .into_iter()
            .filter(|d| d.kind() == praxis_source::DiagCode::FunctionReadsOuterBinding)
            .collect()
    };
    let only = |src: &str| -> praxis_source::Diagnostic {
        let mut ds = n007(src);
        assert_eq!(ds.len(), 1, "one report per use site: {ds:?}");
        ds.remove(0)
    };
    let advice = |d: &praxis_source::Diagnostic| -> Vec<String> {
        d.suggestions()
            .iter()
            .filter(|s| s.replacement.is_none())
            .map(|s| s.label.clone())
            .collect()
    };

    // Direct recursion: the closure clause is gone, and one advisory line says
    // which rule took it away.
    let d = only(
        "var memo = 1\nfn fact(n: Int) -> Int { if n <= 1 { memo } else { n * fact(n - 1) } }\n",
    );
    assert_eq!(
        d.message(),
        "`fact` cannot use `memo`: a function does not capture the bindings around it \
         (pass `memo` as a parameter)"
    );
    assert_eq!(
        advice(&d),
        vec![
            "`fact` calls itself, so a closure is not the way out: a closure cannot name itself \
             (`N001`)"
        ]
    );

    // Mutual recursion is the same cycle one edge longer, so it gets the same
    // form — and the help names the other member, the way `N006` names the other
    // declarations in a type cycle.
    let d = only(
        "var k = 2\nfn ping(n: Int) -> Int { if n <= 0 { k } else { pong(n - 1) } }\n\
         fn pong(n: Int) -> Int { ping(n - 1) }\n",
    );
    assert_eq!(
        advice(&d),
        vec![
            "`ping` calls itself through `pong`, so a closure is not the way out: a closure \
             cannot name itself (`N001`)"
        ]
    );

    // The common case, untouched: a `fn` that is not recursive has both ways out
    // and is told both. This is the assertion the book's `.err` files depend on.
    let d = only("var limit = 10\nfn over_limit(n: Int) -> Bool { n > limit }\n");
    assert_eq!(
        d.message(),
        "`over_limit` cannot use `limit`: a function does not capture the bindings around it \
         (pass `limit` as a parameter, or use a closure)"
    );
    assert!(d.suggestions().is_empty(), "no advice to add: {d:?}");

    // Calling a recursive function is not being one — one edge out is not a
    // cycle, and `caller` can perfectly well be written as a closure.
    let d = only(
        "var limit = 10\nfn fact(n: Int) -> Int { if n <= 1 { 1 } else { n * fact(n - 1) } }\n\
         fn caller(n: Int) -> Int { fact(n) + limit }\n",
    );
    assert!(d.message().contains("or use a closure"));
    assert!(d.suggestions().is_empty());

    // The boundary is the `fn`, not the closure inside it (ADR-068 decision 2),
    // so a closure in a recursive `fn` reading an outer binding is the recursive
    // form: the closure that would have to name itself is `f`, not this one.
    let d = only(
        "var k = 1\nfn f(n: Int) -> Int { if n <= 0 { [1].map(|x| x + k).sum() } \
         else { f(n - 1) } }\n",
    );
    assert!(!d.message().contains("or use a closure"));
    assert_eq!(advice(&d).len(), 1);

    // Source order is preserved: the wording is settled at the end of
    // resolution, but the report is still pushed at the use site, so the two
    // reports stay where the reads are and nothing cascades.
    let diags = analyze_and_lower_diags("var x = 1\nfn f() -> Int { x + x }\n");
    assert_eq!(diags.len(), 2, "no cascade: {diags:?}");
    assert!(
        diags[0].primary().span.start() < diags[1].primary().span.start(),
        "in source order: {diags:?}"
    );
}

/// A record pattern binds each field it names at *that field's* type, and a
/// tuple pattern binds each element at that element's.
///
/// The assertion is the *types* rather than the absence of a diagnostic: a
/// pattern that bound every name at the scrutinee's own type would also be
/// clean, and it would be wrong at the first arithmetic.
///
/// The record's fields differ in type on purpose, so binding by name is
/// observable — a lowering that paired fields by position rather than by name
/// would type `tag` as `Int` here.
#[test]
fn a_record_pattern_binds_a_field_at_the_fields_own_type() {
    const DECL: &str = "struct P { x: Int, tag: Text }\nvar p = P { x: 1, tag: \"a\" }\n";

    // Punned: `P { x }` binds `x` to the field `x`.
    let src = format!("{DECL}var r = match p {{ P {{ x, tag }} => x }}\n");
    assert_eq!(scheme_of(&src, "x").as_deref(), Some("Int"));
    assert_eq!(scheme_of(&src, "tag").as_deref(), Some("Text"));

    // Explicit: `P { x: n }` binds `n` to the field `x`, whatever it is called.
    let src = format!("{DECL}var r = match p {{ P {{ tag: s, x: n }} => n }}\n");
    assert_eq!(scheme_of(&src, "n").as_deref(), Some("Int"));
    assert_eq!(
        scheme_of(&src, "s").as_deref(),
        Some("Text"),
        "the field's type, not the position's — the fields are written swapped"
    );

    // A field the pattern does not name is simply not bound; naming fewer is
    // legal, which is the padding rule at a second kind of composite.
    assert!(is_clean_with_lower(&format!(
        "{DECL}var r = match p {{ P {{ x }} => x }}\n"
    )));

    // The mistakes, each at the code the *literal* form already spends: the
    // record does not have that field, or the pattern names one twice — where
    // the second sub-pattern would silently replace the first.
    let diags = analyze_and_lower_diags(&format!("{DECL}var r = match p {{ P {{ z }} => 1 }}\n"));
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y114"),
        "expected Y114, got {diags:?}"
    );
    let diags = analyze_and_lower_diags(&format!(
        "{DECL}var r = match p {{ P {{ x, x: q }} => 1 }}\n"
    ));
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y115"),
        "expected Y115, got {diags:?}"
    );

    // The head is a *type* name, so it is checked twice over: against the
    // scrutinee, and against being a record at all.
    let diags = analyze_and_lower_diags(
        "struct P { x: Int }\nstruct Q { y: Int }\n\
         var p = P { x: 1 }\nvar r = match p { Q { y } => y }\n",
    );
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y001"),
        "a pattern for another record is a mismatch: {diags:?}"
    );
    let diags = analyze_and_lower_diags("var n = 1\nvar r = match n { Nope { y } => y }\n");
    assert!(
        diags.iter().any(|d| d.code().to_string() == "N001"),
        "an undefined head is an undefined name: {diags:?}"
    );
    let diags = analyze_and_lower_diags("var n = 1\nvar r = match n { Int { y } => y }\n");
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y123"),
        "a head that is not a record has no fields to match: {diags:?}"
    );
}

/// A tuple pattern binds by position, and the scrutinee it is matched against
/// has to be a tuple of that arity.
#[test]
fn a_tuple_pattern_binds_by_position() {
    // Two differently-typed elements, so a pattern that bound both at one type
    // — or paired them the other way round — is a different answer.
    let src = "var t = (1, \"a\")\nvar r = match t { (n, s) => n }\n";
    assert_eq!(scheme_of(src, "n").as_deref(), Some("Int"));
    assert_eq!(scheme_of(src, "s").as_deref(), Some("Text"));

    // Nested, and mixed with the other composite forms.
    let src = "struct P { x: Int, tag: Text }\n\
               var t = (P { x: 1, tag: \"a\" }, 2)\n\
               var r = match t { (P { x, tag }, k) => x + k }\n";
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
        "var n = 1\nvar r = match n { (a, b) => a }\n",
        "var t = (1, 2)\nvar r = match t { (a, b, c) => a }\n",
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
    let diags = analyze_and_lower_diags("var n = 1\nvar r = match n { (a) => a }\n");
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y123"),
        "expected Y123, got {diags:?}"
    );
}

/// A record and a tuple have **one constructor**, so a `match` on one is
/// exhaustive without a `_`. Their signatures are `Closed` with that single
/// constructor, which `exhaustive.rs`'s matrix handles through its `Ctor` rows.
#[test]
fn a_record_or_tuple_match_is_exhaustive_without_a_catch_all() {
    const DECL: &str = "struct P { x: Int, y: Int }\nvar p = P { x: 1, y: 2 }\nvar t = (1, 2)\n";

    // One arm, no `_`, and it covers everything.
    for arm in [
        "match p { P { x, y } => x + y }",
        "match p { P { x } => x }",
        "match p { P { x: a, y: b } => a }",
        "match t { (a, b) => a + b }",
        "match t { (a, _) => a }",
    ] {
        assert!(
            is_clean_with_lower(&format!("{DECL}var r = {arm}\n")),
            "{arm} must be exhaustive on its own"
        );
    }

    // …so a `_` after it is *unreachable*, which is the other half of the same
    // fact and what an `Open` signature would hide.
    let diags = analyze_and_lower_diags(&format!(
        "{DECL}var r = match p {{ P {{ x, y }} => x, _ => 0 }}\n"
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
            format!("{DECL}var r = match p {{ P {{ x: 1, y }} => y }}\n"),
            "P { x: _, y: _ }",
        ),
        (
            format!("{DECL}var r = match t {{ (1, b) => b }}\n"),
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
    // payload's own components are covered — the record and tuple constructors
    // recurse exactly as a variant's does.
    assert!(is_clean_with_lower(
        "struct P { x: Int, y: Int }\n\
         var o = Some(P { x: 1, y: 2 })\n\
         var r = match o { Some(P { x, y }) => x, None => 0 }\n"
    ));
}

/// `min=` and `max=` are catalog rows of their own, on a `Map` whose value type
/// is bound to `Int`.
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
        "var d = Map()\nd[\"a\"] min= 5\nd[\"a\"] min= 3\nout(d[\"a\"])\n"
    ));
    assert_eq!(
        expr_type("{ var d = Map()\n d[\"a\"] min= 5\n d }"),
        "Map[Text, Int]",
        "the row's bound pins the value type"
    );

    // A value that is not an `Int` is the ordinary mismatch.
    let diags = analyze_and_lower_diags("var d = Map()\nd[\"a\"] = \"v\"\nd[\"a\"] min= \"w\"\n");
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y001"),
        "expected Y001, got {diags:?}"
    );

    // A receiver with no updating store is `Y020`, and the message names the
    // operator — a `Counter` *can* be assigned through one index, so "cannot be
    // assigned through 1 index" would be false about the very receiver this is
    // most likely to be written for.
    for (src, op) in [
        ("var c = Counter()\nc[\"k\"] min= 1\n", "min="),
        ("var g = Grid()\ng[0, 0] max= 1\n", "max="),
        ("var v = Vec()\nv.push(1)\nv[0] min= 1\n", "min="),
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
    // `HasMethod` exactly as a method call does, and the call site answers it —
    // in both directions.
    assert!(is_clean_with_lower(
        "fn relax(d, k, v) { d[k] min= v }\n\
         var dist = Map()\ndist[\"a\"] = 10\nrelax(dist, \"a\", 4)\nout(dist[\"a\"])\n"
    ));
    let diags = analyze_and_lower_diags(
        "fn relax(d, k, v) { d[k] min= v }\nvar c = Counter()\nrelax(c, \"a\", 4)\n",
    );
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y020"),
        "the requirement is answered at the call: {diags:?}"
    );

    // …and `min`/`max` are still the prelude's own functions, which is what the
    // contextual grammar rule exists to protect.
    assert_eq!(expr_type("min(3, 4) + max(3, 4)"), "Int");
}

/// A `for` binding is a pattern, and it must match **every** item.
///
/// The header takes a full pattern rather than one `Ident`, so a `Map`'s pair is
/// destructured as `for (k, v) in m` instead of being named and read with
/// `kv.0`/`kv.1` (ADR-066 decision 3). It is the pattern grammar in the one
/// other position a binding appears.
#[test]
fn a_for_binding_is_a_pattern_and_must_match_every_item() {
    // Each name binds at its own component's type, which a binding that named
    // the whole item could not do.
    let src = "var m = Map()\nm[\"a\"] = 1\nfor (k, v) in m { out(k) out(v) }\n";
    assert_eq!(scheme_of(src, "k").as_deref(), Some("Text"));
    assert_eq!(scheme_of(src, "v").as_deref(), Some("Int"));

    // A record pattern in the header, at the fields' own types.
    let src = "struct P { x: Int, tag: Text }\nvar ps = Vec()\nps.push(P { x: 1, tag: \"a\" })\n\
               for P { x, tag } in ps { out(x) out(tag) }\n";
    assert_eq!(scheme_of(src, "x").as_deref(), Some("Int"));
    assert_eq!(scheme_of(src, "tag").as_deref(), Some("Text"));

    // A bare name still binds the whole item — the overwhelmingly common shape,
    // and the one every existing program is written with.
    let src = "var v = Vec()\nv.push((1, 2))\nfor kv in v { out(kv.0) }\n";
    assert_eq!(scheme_of(src, "kv").as_deref(), Some("(Int, Int)"));

    // The pattern is checked against the element type like any other, so a shape
    // the item cannot have is the ordinary mismatch.
    let diags = analyze_and_lower_diags("var v = Vec()\nv.push(1)\nfor (a, b) in v { out(a) }\n");
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y001"),
        "expected Y001, got {diags:?}"
    );

    // **A binding has no second arm**, so a pattern that can fail is `Y125` —
    // both spellings, and at any depth.
    for src in [
        "var v = Vec()\nv.push(Some(1))\nfor Some(n) in v { out(n) }\n",
        "var v = Vec()\nv.push((1, 2))\nfor (1, b) in v { out(b) }\n",
        "var v = Vec()\nv.push((1, (2, 3)))\nfor (a, (2, c)) in v { out(c) }\n",
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
        "var v = Vec()\nv.push(1)\nfor _ in v { out(0) }\n",
        "var v = Vec()\nv.push((1, 2))\nfor (a, _) in v { out(a) }\n",
        "struct P { x: Int, y: Int }\nvar ps = Vec()\nps.push(P { x: 1, y: 2 })\n\
         for P { x } in ps { out(x) }\n",
    ] {
        assert!(is_clean_with_lower(src), "{src} must be accepted");
    }
}

/// A record literal's head must name a `struct`.
///
/// `infer_record_lit` has to ask what the head symbol *is*, not only what its
/// type is. Reading the type alone accepts `var x = 1` / `var p = x { a: 1 }`,
/// a program the checker takes and whose value has no representation.
#[test]
fn a_record_literals_head_must_name_a_struct() {
    // It is `N008` in **inference**, so `praxis check` sees it.
    let analysis = analyze("var x = 1\nvar p = x { a: 1 }\nout(p)\n");
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.code().to_string() == "N008"),
        "expected N008 from analysis alone, got {:?}",
        analysis.diagnostics
    );

    // Every kind of declaration that is not a `struct` answers the same way, and
    // the message names the kind — an `enum` is a perfectly good *type* and still
    // has no fields to initialize, so "not a type" would be a lie about it.
    for (src, kind) in [
        ("var x = 1\nvar p = x { a: 1 }\n", "a binding"),
        (
            "fn g(v: Vec[Int]) { for x in v { x { a: 1 } } }\n",
            "a binding",
        ),
        ("fn f() { 1 }\nvar p = f { a: 1 }\n", "a function"),
        ("enum E { A }\nvar p = E { a: 1 }\n", "an enum type"),
        ("enum E { A }\nvar p = A { a: 1 }\n", "an enum variant"),
        ("var p = out { a: 1 }\n", "a built-in name"),
        ("fn g(q) { q { a: 1 } }\n", "a parameter"),
    ] {
        let diags = analyze(src).diagnostics;
        let n008 = diags
            .iter()
            .find(|d| d.code().to_string() == "N008")
            .unwrap_or_else(|| panic!("expected N008 for {src}, got {diags:?}"));
        assert!(
            n008.message().contains(kind),
            "the message must name `{kind}`: {}",
            n008.message()
        );
    }

    // The literal answers a fresh variable rather than the head's own type, so
    // arithmetic on it has no `Int` to pretend to be.
    assert_ne!(
        scheme_of("var x = 1\nvar p = x { a: 1 }\n", "p").as_deref(),
        Some("Int"),
        "the literal must not keep the head's type"
    );

    // The initializers are still inferred, so a mistake inside one is still
    // reported rather than swallowed with the literal.
    let diags = analyze("var x = 1\nvar p = x { a: nope }\n").diagnostics;
    assert!(
        diags.iter().any(|d| d.code().to_string() == "N001"),
        "an initializer's own mistake survives: {diags:?}"
    );

    // …and an actual `struct` head is untouched, including one whose field is
    // initialized from an ordinary binding.
    for src in [
        "struct P { x: Int, y: Int }\nvar p = P { x: 1, y: 2 }\nout(p.x)\n",
        "struct P { x: Int }\nvar q = 5\nvar p = P { x: q }\nout(p.x)\n",
    ] {
        assert!(is_clean_with_lower(src), "{src} must be accepted");
    }

    // A head that resolves to *nothing* is still `N001` and only `N001`: there is
    // no symbol to have the wrong kind.
    let diags = analyze("var p = Nope { a: 1 }\n").diagnostics;
    assert!(
        diags.iter().any(|d| d.code().to_string() == "N001")
            && !diags.iter().any(|d| d.code().to_string() == "N008"),
        "an undefined head is N001 alone: {diags:?}"
    );
}

/// A field read constrains its receiver, so §4.9's own example compiles.
///
/// `infer_field_get` records a `HasField` requirement rather than answering an
/// unresolved receiver with a fresh variable — otherwise the parameter is
/// generalized with nothing to re-ask at the call. It is the deferred-method
/// rule at a third door, through `require_cap`.
///
/// The second half is that the requirement has to be able to *fail*. If
/// `infer_field_get` deferred only a variable receiver and
/// `resolve_deferred_field` returned silently when the receiver turned out to
/// have no such field, `Capability::HasField`'s rejection arm would be dead code
/// and every program below would be check-clean and run-broken. Each
/// `check_diags` assertion here is that half; each is a plain `analyze`, with no
/// lowering, because lowering is the pass `praxis check` does not run.
#[test]
fn a_field_read_requires_the_field_of_whatever_the_receiver_turns_out_to_be() {
    /// Only what `praxis check` sees: analysis, without lowering.
    fn check_diags(text: &str) -> Vec<praxis_source::Diagnostic> {
        analyze(text).diagnostics
    }
    fn has(diags: &[praxis_source::Diagnostic], code: &str) -> bool {
        diags.iter().any(|d| d.code().to_string() == code)
    }

    // §4.9's own example, clean at `check` and through lowering alike.
    assert!(is_clean_with_lower(
        "struct P { x: Int, y: Int }\n\
         fn dist(a) -> Int { a.x + a.y }\n\
         out(dist(P { x: 1, y: 2 }))\n"
    ));

    // **§4.9's fence as the document actually writes it**: no `struct`, no call
    // site, nothing to pin `a` or `b`. It has to compile, because an uncalled
    // generic function is not an error — `fn f(a) { a + 1 }` has always been
    // accepted, and a field read was singled out only because it needed a record
    // definition to produce an index — which lowering must not demand of an
    // unpinned receiver. `crates/praxis-cli/tests/design_doc.rs` drives the
    // byte-for-byte fence through the real binary; this is the same claim.
    assert!(is_clean_with_lower(
        "fn manhattan(a, b) {\n    abs(a.x - b.x) + abs(a.y - b.y)\n}\n"
    ));

    // A **concrete** receiver that is not a record is rejected at `check`, not at
    // lowering. `require_cap_as` decides it on the spot through
    // `crate::capability::check`, which is the only thing that ever reaches
    // `Capability::HasField`'s rejection arm from that door.
    assert!(
        has(&check_diags("var n = 1\nout(n.x)\n"), "Y112"),
        "a field of an `Int` is `check`'s to report"
    );

    // …and so is a concrete **record** that simply lacks the field: the other
    // half of the same arm, and the reason `capability::check` inspects the
    // record rather than stopping at "is it one".
    assert!(
        has(
            &check_diags("struct P { x: Int }\nvar p = P { x: 1 }\nout(p.z)\n"),
            "Y112"
        ),
        "a missing field of a known record is `check`'s to report"
    );

    // A **deferred** receiver that resolves to a non-record is rejected when the
    // call site resolves it — the solver door into the same arm. Lowering could
    // not report this one at all: by the time it runs, `a` is still a variable,
    // and an unresolved variable is nobody's to reject.
    assert!(
        has(
            &check_diags(
                "struct P { x: Int, y: Int }\n\
                 fn dist(a) -> Int { a.x + a.y }\nout(dist(3))\n"
            ),
            "Y112"
        ),
        "a call that pins the receiver to `Int` is `check`'s to report"
    );

    // …and one that resolves to a record **without** the field, likewise.
    assert!(
        has(
            &check_diags(
                "struct P { x: Int, y: Int }\nfn getz(a) { a.z }\nout(getz(P { x: 1, y: 2 }))\n"
            ),
            "Y112"
        ),
        "a call that pins the receiver to a record lacking the field is `check`'s"
    );

    // Through the closure channel: the parameter defers a `HasField` and then
    // unifies with `Int`.
    assert!(
        has(
            &check_diags("var v = Vec[Int]()\nv.push(1)\nout(v.map(|a| a.x).sum())\n"),
            "Y112"
        ),
        "a closure parameter pinned to `Int` is `check`'s to report"
    );

    // The call site is what says which record it is, and the parameter comes out
    // at that record — not `forall T. (T) -> …`.
    let src = "struct P { x: Int, y: Int }\n\
               fn getx(a) { a.x }\n\
               out(getx(P { x: 1, y: 2 }))\n";
    assert_eq!(scheme_of(src, "getx").as_deref(), Some("(P) -> Int"));

    // The field's own type is what the read produces, so the result follows the
    // field and not the arithmetic that happens to use it.
    let src = "struct N { name: Text, n: Int }\n\
               fn label(v) { v.name }\n\
               out(label(N { name: \"t\", n: 1 }))\n";
    assert_eq!(scheme_of(src, "label").as_deref(), Some("(N) -> Text"));

    // Chained: the outer read's discharge is what makes the inner one
    // dischargeable, so a two-deep read on an unannotated parameter resolves.
    assert!(is_clean_with_lower(
        "struct Inner { x: Int }\nstruct Outer { inner: Inner }\n\
         fn deep(o) -> Int { o.inner.x }\n\
         out(deep(Outer { inner: Inner { x: 42 } }))\n"
    ));

    // **The receiver is pinned, not quantified** (ADR-057 decision 5's rule at
    // this door): there is one lowered body per source function and
    // `lower_field_get` reads one record definition for the field's index, so two
    // call sites at two records are a disagreement about the signature.
    let diags = analyze_and_lower_diags(
        "struct P { x: Int, y: Int }\nstruct Q { x: Text }\n\
         fn getx(a) { a.x }\nout(getx(P { x: 1, y: 2 }))\nout(getx(Q { x: \"t\" }))\n",
    );
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y001"),
        "two records at one field-read site is Y001, got {diags:?}"
    );

    // A concrete record's own field read is untouched — the fast path is still
    // the fast path, and it never emits a requirement at all.
    assert!(is_clean_with_lower(
        "struct P { x: Int, y: Int }\nvar p = P { x: 1, y: 2 }\nout(p.x + p.y)\n"
    ));

    // The requirement rides the scheme rather than being decided once: a helper
    // that reads a field of *another* helper's parameter resolves through both.
    assert!(is_clean_with_lower(
        "struct P { x: Int, y: Int }\n\
         fn getx(a) { a.x }\nfn twice(b) { getx(b) + getx(b) }\n\
         out(twice(P { x: 4, y: 0 }))\n"
    ));
}

/// A closure parameter is a pattern, and it must match **every** argument.
///
/// Destructuring in binding position *is* a pattern, so `|(a, b)| abs(a - b)` is
/// the language; and `Y125` applies here as it does in a `for` header, because a
/// parameter has no second arm to send an argument that does not match.
#[test]
fn a_closure_parameter_is_a_pattern_and_must_match_every_argument() {
    // Each name binds at its own component's type, which a parameter that named
    // the whole argument could not do.
    let src = "var v = Vec()\nv.push((1, \"a\"))\nvar s = v.map(|(n, t)| t)\nout(s)\n";
    assert_eq!(scheme_of(src, "n").as_deref(), Some("Int"));
    assert_eq!(scheme_of(src, "t").as_deref(), Some("Text"));

    // A record pattern, at the fields' own types.
    let src = "struct P { x: Int, tag: Text }\n\
               var f = |P { x, tag }| tag\nout(f(P { x: 1, tag: \"a\" }))\n";
    assert_eq!(scheme_of(src, "x").as_deref(), Some("Int"));
    assert_eq!(scheme_of(src, "tag").as_deref(), Some("Text"));

    // A bare name still binds the whole argument — the shape every existing
    // program is written with, and the one `Param::name` still answers for.
    let src = "var v = Vec()\nv.push((1, 2))\nvar s = v.map(|kv| kv.0)\nout(s)\n";
    assert_eq!(scheme_of(src, "kv").as_deref(), Some("(Int, Int)"));

    // The annotation belongs to the whole argument, and it pins the components.
    let src = "var f = |(a, b): (Int, Text)| b\nout(f((1, \"z\")))\n";
    assert_eq!(scheme_of(src, "a").as_deref(), Some("Int"));
    assert_eq!(scheme_of(src, "b").as_deref(), Some("Text"));

    // **A parameter has no second arm**, so a pattern that can fail is `Y125` —
    // both spellings, and at any depth.
    for src in [
        "var f = |Some(n)| n\nout(f(Some(1)))\n",
        "var f = |(1, b)| b\nout(f((1, 2)))\n",
        "var f = |(a, (2, c))| c\nout(f((1, (2, 3))))\n",
        "struct P { x: Int }\nvar f = |P { x: 1 }| 0\nout(f(P { x: 1 }))\n",
    ] {
        let diags = analyze_and_lower_diags(src);
        assert!(
            diags.iter().any(|d| d.code().to_string() == "Y125"),
            "{src} must be Y125, got {diags:?}"
        );
    }

    // …and the irrefutable shapes are all accepted, including a wildcard
    // component, a partial record pattern, several parameters and nesting.
    for src in [
        "var f = |(a, b)| a + b\nout(f((1, 2)))\n",
        "var f = |(a, _)| a\nout(f((1, 2)))\n",
        "var f = |(a, b), c| a + b + c\nout(f((1, 2), 3))\n",
        "var f = |a, (b, c)| a + b + c\nout(f(1, (2, 3)))\n",
        "var f = |(a, (b, c))| a + b + c\nout(f((1, (2, 3))))\n",
        "struct P { x: Int, y: Int }\nvar f = |P { x }| x\nout(f(P { x: 1, y: 2 }))\n",
    ] {
        assert!(is_clean_with_lower(src), "{src} must be accepted");
    }

    // The pattern is checked against the parameter's type like any other, so an
    // argument the shape cannot have is the ordinary mismatch.
    let diags = analyze_and_lower_diags("var f = |(a, b)| a\nout(f(1))\n");
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y001"),
        "expected Y001, got {diags:?}"
    );

    // A destructured name is a binding like any other: it captures, and it is
    // assignable (ADR-125) — at its own type, which is the component's and not
    // the whole argument's.
    assert!(is_clean_with_lower(
        "var g = |(a, b)| { var h = |n| n + a + b\n h(1) }\nout(g((2, 3)))\n"
    ));
    assert!(is_clean_with_lower(
        "var f = |(a, b)| { a = 5\n b }\nout(f((1, 2)))\n"
    ));
    let diags = analyze_and_lower_diags("var f = |(a, b)| { a = \"x\"\n b }\nout(f((1, 2)))\n");
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y001"),
        "…and the component's type is what the write must match: {diags:?}"
    );
}

/// A zero-argument accessor is a **call**, and a bare `receiver.name` is a field
/// read and only that (ADR-077). `len`, `width` and `height` are catalog rows of
/// arity zero, not property reads.
///
/// The rule is load-bearing rather than merely tidy: a field read rides the
/// constraint channel, and a bare `.name` that could be *either* a field or a
/// nullary row would emit a requirement with two possible discharges and no way
/// to choose between them.
///
/// This is a characterization test: it pins a rule the tree already follows so
/// that nobody may quietly change it.
#[test]
fn a_zero_argument_accessor_is_a_call_and_a_bare_name_is_a_field() {
    // The call form is the one that works, on every receiver the doc writes it
    // for.
    assert!(is_clean_with_lower(
        "var v = Vec()\nv.push(1)\nout(v.len())\n"
    ));
    assert!(is_clean_with_lower(
        "var m = Map()\nm[1] = 2\nout(m.len())\n"
    ));

    // The property spelling is not a syntax this language has: it is a field
    // read of a name no record declares, so it is `Y112`. It comes from
    // *inference* — the receiver is concrete, so `require_cap_as` decides it —
    // which is why `analyze` alone is enough here.
    for src in [
        "var v = Vec()\nv.push(1)\nout(v.len)\n",
        "var m = Map()\nm[1] = 2\nout(m.len)\n",
        "var t = \"abc\"\nout(t.len)\n",
    ] {
        let diags = analyze(src).diagnostics;
        assert!(
            diags.iter().any(|d| d.code().to_string() == "Y112"),
            "{src} must be Y112 at `check`, got {diags:?}"
        );
    }

    // A **field** named like a catalog row is still a field, and the two spellings
    // stay apart: `p.len` reads the field, `p.len()` looks for a row a record does
    // not have.
    assert!(is_clean_with_lower(
        "struct P { len: Int }\nvar p = P { len: 7 }\nout(p.len)\n"
    ));
    // Asked of `analyze` alone, not of `analyze` + `lower`: a record carries no
    // catalog rows, `Y110` is inference's to report (ADR-093), and `check` is
    // where the user finds that out.
    let diags = analyze("struct P { len: Int }\nvar p = P { len: 7 }\nout(p.len())\n").diagnostics;
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y110"),
        "a record has no rows: {diags:?}"
    );

    // …and a deferred read resolves to the **field**, not to a row — which is
    // the property this rule exists to protect.
    let src = "struct P { len: Int }\nfn n(a) { a.len }\nout(n(P { len: 7 }))\n";
    assert_eq!(scheme_of(src, "n").as_deref(), Some("(P) -> Int"));
}

/// **ADR-084.** A backtick template outside `read`/`parse` is reported, not
/// silently reinterpreted.
///
/// Typing it `Text` and lowering it as a text literal of the raw interior makes
/// ``var t = `n = {int}` `` type-check and `out(t)` print `n = {int}` — the
/// capture emitted as characters rather than reported. §7.1 enters the
/// parser-expression sublanguage at `read` or `parse(text, …)` and nowhere else.
#[test]
fn a_parser_template_in_value_position_is_reported() {
    let diags = analyze_and_lower_diags("var t = `n = {int}`\nout(t)\n");
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y023"),
        "expected Y023 for a template in value position, got {diags:?}"
    );

    // The same template after `read` is the language, and is untouched: the
    // sublanguage is entered by the *word*, so the fix must not be about the
    // token.
    assert!(is_clean_with_lower("var n = read `n = {int}`\nout(n)\n"));

    // One mistake, one diagnostic. The template still parses to a literal node,
    // so nothing cascades off an unconsumed token — and the type it gets is a
    // fresh variable rather than `Text`, so no second error follows about the
    // use of a value that does not exist.
    let errors = diags
        .iter()
        .filter(|d| d.severity() == praxis_source::Severity::Error)
        .count();
    assert_eq!(errors, 1, "expected exactly one error, got {diags:?}");
}

/// A `choice(...)` payload record's fields are readable, because a variant
/// pattern's enum is the **scrutinee's**.
///
/// Reaching the enum through the constructor's resolved *symbol* instead
/// (`ctor.and_then(lookup_enum_variant)`) skips the arm whole for an anonymous
/// enum, which has no declaration and therefore no symbol: the scrutinee is
/// never unified, the payload never asked for, and `p` in `Mul(p)` keeps an
/// unbound variable — so `p.a` takes `infer_field_get`'s tolerance for an
/// unresolved receiver and lowers to `Unit`.
///
/// The assertions name the **rendered type** in the message rather than merely
/// checking that some diagnostic fired: naming `{ a: Int, b: Int }` is what
/// proves the anonymous payload record reached inference, rather than that
/// inference tripped over something else.
#[test]
fn a_bad_field_on_a_choice_payload_is_reported_at_check() {
    const READ: &str = "var ms = read scan(choice(Mul: `mul({a:int},{b:int})`, Do: `do()`))\n";

    // A field the payload record does not have, reported by `analyze` alone so
    // that `praxis check` sees it.
    let diags = analyze(&format!(
        "{READ}for m in ms {{ match m {{ Mul(p) => out(p.zzz), Do(_) => {{}} }} }}\n"
    ))
    .diagnostics;
    let y112 = diags
        .iter()
        .find(|d| d.code().to_string() == "Y112")
        .unwrap_or_else(|| panic!("expected Y112 from analysis alone, got {diags:?}"));
    assert!(
        y112.message().contains("{ a: Int, b: Int }"),
        "the message must name the payload record type: {}",
        y112.message()
    );

    // …and the field that *is* there types as the capture's own kind, which is
    // the positive half: a binding left at an unbound variable would render `?T`.
    let src = format!("{READ}for m in ms {{ match m {{ Mul(p) => out(p.a), Do(_) => {{}} }} }}\n");
    assert_eq!(
        scheme_of(&src, "p").as_deref(),
        Some("{ a: Int, b: Int }"),
        "the payload binding is the anonymous record, not a fresh variable"
    );
}

/// **ADR-091.** A record pattern needs no head, and a headless one pins its
/// record from the scrutinee.
///
/// A `choice(...)` payload record is *anonymous*, so no head can name it —
/// `Mul({a, b})` is the only spelling there is, and the pattern grammar has an
/// `L_BRACE` arm for it.
#[test]
fn a_record_pattern_needs_no_head_and_pins_from_the_scrutinee() {
    // Nested inside a variant pattern, over an anonymous payload record: the
    // shape the row exists for.
    let src = "var ms = read scan(choice(Mul: `mul({a:int},{b:int})`, Do: `do()`))\n\
               for m in ms { match m { Mul({a, b}) => out(a * b), Do(_) => {} } }\n";
    assert!(
        errors_of(src).is_empty(),
        "the headless form must parse and check: {:?}",
        errors_of(src)
    );
    assert_eq!(scheme_of(src, "a").as_deref(), Some("Int"));
    assert_eq!(scheme_of(src, "b").as_deref(), Some("Int"));

    // One production, so every pattern position gets it: a top-level match arm,
    // a `for` header, and a closure parameter. Against a *nominal* record too —
    // the head is optional, not forbidden.
    const DECL: &str = "struct P { x: Int, tag: Text }\n";
    let src =
        format!("{DECL}var p = P {{ x: 1, tag: \"a\" }}\nvar r = match p {{ {{x, tag}} => x }}\n");
    assert_eq!(scheme_of(&src, "x").as_deref(), Some("Int"));
    assert_eq!(scheme_of(&src, "tag").as_deref(), Some("Text"));
    let src = format!(
        "{DECL}var ps = Vec()\nps.push(P {{ x: 1, tag: \"a\" }})\n\
         for {{x, tag}} in ps {{ out(x) out(tag) }}\n"
    );
    assert_eq!(scheme_of(&src, "x").as_deref(), Some("Int"));
    assert_eq!(scheme_of(&src, "tag").as_deref(), Some("Text"));
    let src = format!("{DECL}var f = |q: P| match q {{ {{x, tag}} => x }}\n");
    assert_eq!(scheme_of(&src, "x").as_deref(), Some("Int"));

    // The mistakes inside it are the headed form's, unchanged: a field the
    // record does not have, and one named twice.
    for (arm, code) in [("{x, zzz}", "Y114"), ("{x, x: q}", "Y115")] {
        let src =
            format!("{DECL}var p = P {{ x: 1, tag: \"a\" }}\nvar r = match p {{ {arm} => 1 }}\n");
        let diags = analyze_and_lower_diags(&src);
        assert!(
            diags.iter().any(|d| d.code().to_string() == code),
            "{arm} must be {code}, got {diags:?}"
        );
    }
}

/// **ADR-091 decision 2.** A headless record pattern against a scrutinee nothing
/// has pinned is reported, and a non-record scrutinee is `Y123`.
///
/// Silence against an open scrutinee — by analogy with `infer_field_get`'s
/// tolerance for an unresolved receiver — is not the same trade. A field *read*
/// may be silent because lowering answers `Unit` too, consistently; a *binding*
/// may not. Inference would bind `x` and `y` to fresh variables, while lowering
/// — which reads the record off the scrutinee and by then knows it — would store
/// the fields at their real types: a program clean at `check` that dies under
/// `run`.
#[test]
fn a_headless_record_pattern_needs_a_record_it_can_see() {
    const DECL: &str = "struct P { x: Int, y: Int }\n";

    // Nothing pins the closure parameter or the unannotated function parameter,
    // and field names alone cannot construct a record type — the language has no
    // row variables. Reported in **inference**, so `praxis check` sees it.
    for src in [
        format!("{DECL}var f = |{{x, y}}| x + y\nout(f(P {{ x: 1, y: 2 }}))\n"),
        format!(
            "{DECL}fn g(q) {{ match q {{ {{x, y}} => x + y }} }}\nout(g(P {{ x: 1, y: 2 }}))\n"
        ),
    ] {
        let diags = analyze(&src).diagnostics;
        assert!(
            diags.iter().any(|d| d.code().to_string() == "Y123"),
            "{src} must be Y123 from analysis alone, got {diags:?}"
        );
    }

    // Naming the record, or annotating the value, is the answer the message
    // gives — and both are accepted.
    for src in [
        format!("{DECL}var f = |P {{x, y}}| x + y\nout(f(P {{ x: 1, y: 2 }}))\n"),
        format!(
            "{DECL}var f = |q: P| match q {{ {{x, y}} => x + y }}\nout(f(P {{ x: 1, y: 2 }}))\n"
        ),
    ] {
        assert!(is_clean_with_lower(&src), "{src} must be accepted");
    }

    // A scrutinee that is not a record at all is the shape error, `Y123`, and the
    // message spells the headless pattern as `{ … }` rather than interpolating a
    // head it does not have.
    let diags = analyze("var n = 1\nvar r = match n { {a, b} => a + b }\n").diagnostics;
    let y123 = diags
        .iter()
        .find(|d| d.code().to_string() == "Y123")
        .unwrap_or_else(|| panic!("expected Y123, got {diags:?}"));
    assert!(
        y123.message()
            .contains("`{ … }` is not a pattern for `Int`"),
        "the message must name the headless shape: {}",
        y123.message()
    );
}

/// **ADR-091 decision 3.** `P {}` is a record pattern, not a binding named `P`.
///
/// `Pattern::kind()` decides the record shape from the *brace*, with the
/// `PATTERN_FIELD` child only as a second witness. `P {}` has no field child, so
/// deciding on that child alone falls through to `PatternKind::Name("P")` — a
/// **binding**, which matches anything, so a record pattern naming the wrong
/// record would silently cover every value.
#[test]
fn a_record_pattern_with_a_head_and_no_fields_is_still_a_record_pattern() {
    const DECL: &str = "struct Q { z: Int }\nstruct P { a: Int }\nvar q = Q { z: 1 }\n";

    // The head names another record, so it is the ordinary mismatch — and it is
    // a mismatch at all only because the pattern is read as a record pattern.
    let diags = analyze_and_lower_diags(&format!("{DECL}var r = match q {{ P {{}} => 1 }}\n"));
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y001"),
        "expected Y001, got {diags:?}"
    );

    // …and against its *own* record it is legal and binds nothing: `P {}` is
    // `Some` beside `Some(_)`, one test with no names taken out of it.
    assert!(is_clean_with_lower(&format!(
        "{DECL}var p = P {{ a: 1 }}\nvar r = match p {{ P {{}} => 1 }}\n"
    )));

    // The headless `{}` is a *parse* error instead (ADR-091 Decision 3): it binds
    // nothing and names no record, so it tests nothing and covers everything.
    let errors = errors_of(&format!("{DECL}var r = match q {{ {{}} => 1 }}\n"));
    assert!(
        errors.iter().any(|e| e.starts_with("P001")),
        "an empty headless record pattern is a parse error: {errors:?}"
    );
}

/// **ADR-091 decision 5.** A variant a concrete enum does not have is `Y122` at
/// **inference**, not only at lowering — otherwise `praxis check` exits 0 on the
/// program below while `praxis run` reports it, for *every* enum and not only
/// the anonymous ones. `praxis check` is the command that is supposed to know.
#[test]
fn an_unknown_variant_is_reported_at_check() {
    // A nominal enum…
    let diags = analyze(
        "enum Move { Step(Int), Stay }\nvar m = Stay\nvar r = match m { Bogus(n) => n, _ => 0 }\n",
    )
    .diagnostics;
    assert!(
        diags.iter().any(|d| d.code().to_string() == "Y122"),
        "expected Y122 from analysis alone, got {diags:?}"
    );

    // …and an anonymous one, whose rendering is what makes the message readable
    // at all (ADR-091 Decision 4).
    let diags = analyze(
        "var ms = read scan(choice(Mul: `mul({a:int},{b:int})`, Do: `do()`))\n\
         for m in ms { match m { Bogus(p) => out(1), _ => {} } }\n",
    )
    .diagnostics;
    let y122 = diags
        .iter()
        .find(|d| d.code().to_string() == "Y122")
        .unwrap_or_else(|| panic!("expected Y122 from analysis alone, got {diags:?}"));
    assert!(
        y122.message()
            .contains("`{ Mul({ a: Int, b: Int }) | Do(Unit) }` has no variant `Bogus`"),
        "the anonymous enum must render its variants: {}",
        y122.message()
    );

    // The constructor symbol is still consulted when the scrutinee is *not*
    // pinned — it is the only thing that can pin it, and this is what makes an
    // unannotated parameter matched against a nominal enum infer at all.
    let scheme = scheme_of(
        "enum Move { Step(Int), Stay }\nfn score(m) { match m { Step(n) => n, Stay => 0 } }\n",
        "score",
    )
    .expect("score has a scheme");
    assert_eq!(scheme, "(Move) -> Int");
}

/// A list literal is a `Vec` of its elements' one type, and its elements are
/// checked against **each other** rather than against nothing.
///
/// The empty `[]` is the case worth pinning: it has no element to take a type
/// from, so it is `Vec[?T]` and the *use* decides `?T` — the same answer `Vec()`
/// gives. A rule written as "the first element's type" has no answer here at
/// all, which is why the element type is a fresh variable unified with each
/// element in turn.
#[test]
fn a_list_literal_is_a_vec_of_one_element_type() {
    // The type, at each element type and each arity.
    assert_eq!(expr_type("[1, 2, 3]"), "Vec[Int]");
    assert_eq!(expr_type("[1]"), "Vec[Int]");
    assert_eq!(expr_type("[\"a\", \"b\"]"), "Vec[Text]");
    assert_eq!(expr_type("[[1], [2]]"), "Vec[Vec[Int]]");
    assert_eq!(expr_type("[(1, \"a\")]"), "Vec[(Int, Text)]");
    // An element is an arbitrary expression, so its type is inferred and not
    // read off a literal.
    assert_eq!(expr_type("[1 + 1, 2 * 3]"), "Vec[Int]");

    // An empty list is generic in its element, and the use pins it. `[]` is not
    // generalized — a `Vec` is mutable, so the value restriction applies to it
    // exactly as it does to `Vec()`.
    assert!(!has_type_error(
        "fn main() -> Int { var v: Vec[Int] = []\n v.len() }"
    ));
    assert!(!has_type_error(
        "fn main() -> Int { var v = []\n v.push(1)\n v.len() }"
    ));

    // Elements that disagree are reported. The message names the type
    // established so far as `expected`, so the *offending* element is `found`.
    assert!(has_type_error("var v = [1, \"a\"]"));
    assert!(has_type_error("var v = [\"a\", 1]"));
    assert!(has_type_error("var v = [[1], [\"a\"]]"));
    let diags = analyze("var v = [1, \"a\"]").diagnostics;
    let y001 = diags
        .iter()
        .find(|d| d.code().to_string() == "Y001")
        .unwrap_or_else(|| panic!("expected Y001, got {diags:?}"));
    assert!(
        y001.message().contains("expected Int, found Text"),
        "the element written second is what is wrong: {}",
        y001.message()
    );

    // …and the literal's own type is checked against its context like any other
    // expression.
    assert!(has_type_error(
        "fn main() -> Int { var v: Vec[Text] = [1, 2]\n v.len() }"
    ));
}

/// A list literal is iterable, subscriptable and a method receiver — it is a
/// `Vec`, not a shape that only `for` understands.
#[test]
fn a_list_literal_is_a_vec_everywhere_a_vec_goes() {
    for src in [
        "fn main() -> Unit { for x in [1, 2, 3] { out(x) } }",
        "fn main() -> Int { [1, 2, 3].len() }",
        "fn main() -> Int { [1, 2, 3][0] }",
        "fn main() -> Int { var v: Vec[Int] = [1]\n v.len() }",
        // Passed to a function that iterates an unannotated parameter: a list
        // literal is one of the iterables that instantiates it.
        "fn total(r) { var t = 0\n for i in r { t = t + i }\n t }\n\
         fn main() -> Int { total([1, 2, 3]) }",
    ] {
        assert!(!has_type_error(src), "must be accepted: {src}");
    }

    // The element type reaches the loop variable, so a body that uses it at the
    // wrong type is reported.
    assert!(has_type_error(
        "fn main() -> Unit { for x in [1, 2] { var t: Text = x\n out(t) } }"
    ));
}

/// **A `Text` is iterable and yields `Char`** (§4.13, ADR-086).
///
/// The item type is the assertion, not merely that the loop is accepted: a
/// `Text` that yielded `Text` would accept every body a `Char` does not.
#[test]
fn a_text_is_iterable_and_yields_char() {
    assert!(!has_type_error(
        "fn main() -> Unit { for c in \"abc\" { out(c) } }"
    ));
    // The loop variable is a `Char`, so `Char`'s own method resolves on it…
    assert!(!has_type_error(
        "fn main() -> Int { var n = 0\n for c in \"abc\" { n = n + c.to_int() }\n n }"
    ));
    // …and it is not a `Text`.
    assert!(has_type_error(
        "fn main() -> Unit { for c in \"abc\" { var t: Text = c\n out(t) } }"
    ));
    // The same answer through a binding and through a parameter, so it is the
    // type that is iterable and not the literal.
    assert!(!has_type_error(
        "fn main() -> Unit { var s = \"abc\"\n for c in s { out(c) } }"
    ));
    assert!(!has_type_error(
        "fn each(t: Text) -> Unit { for c in t { out(c) } }\nfn main() -> Unit { each(\"ab\") }"
    ));
    // A `Char` is what iterating a `Text` produces, and is not itself iterable.
    assert!(has_type_error(
        "fn main() -> Unit { for c in \"ab\"[0] { out(c) } }"
    ));
    // The other scalars are unchanged: `Y005` where they are written.
    assert!(has_type_error(
        "fn main() -> Unit { for i in 3 { out(i) } }"
    ));
}

/// **ADR-127 decision 1.** A pipeline's receiver is anything a `for` loop can
/// walk, and it yields what the `for` loop's variable would bind.
///
/// The pipeline entry is one feature registered against every receiver rather
/// than a feature per collection: `capability::iter_item` answers "what does
/// this yield" for eleven collections plus `Text`, and the entry reads that
/// answer.
#[test]
fn a_pipeline_walks_every_iterable_and_binds_what_the_for_loop_would() {
    // The five sequence-shaped receivers and the two nullary ones.
    for src in [
        "fn main() -> Int { var s = Set()\n s.insert(1)\n s.map(|x| x * 2).sum() }",
        "fn main() -> Int { var d = Deque()\n d.push_back(1)\n d.map(|x| x + 1).sum() }",
        "fn main() -> Int { var h = MinHeap()\n h.push(1)\n h.count() }",
        "fn main() -> Int { var h = MaxHeap()\n h.push(1)\n h.filter(|x| x > 0).count() }",
        "fn main() -> Int { var b = BitSet()\n b.insert(1)\n b.map(|n| n * n).sum() }",
        "fn main() -> Int { (0..10).map(|x| x * 2).sum() }",
        "fn main() -> Int { \"hello\".count(|c| c == \"l\"[0]) }",
    ] {
        assert!(!has_type_error(src), "{src}");
    }

    // A `Map`'s item is the `(K, V)` pair, and a `Counter`'s is `(T, Int)` —
    // the same pairs `for kv in m` binds.
    assert!(!has_type_error(
        "fn main() -> Int { var m = Map()\n m[\"a\"] = 1\n m.map(|kv| kv.1).sum() }"
    ));
    assert!(!has_type_error(
        "fn main() -> Int { var c = [\"a\"].frequencies()\n c.filter(|p| p.1 > 0).count() }"
    ));

    // **`Grid` is excluded, and `grid.map` is why** (§6.4 asks for the
    // shape-preserving row by name). It is a `Y110`.
    assert!(has_type_error(
        "fn main() -> Unit { var g = Grid()\n out(g.map(|c| c)) }"
    ));
    // …but a grid's own pipeline entry is unchanged.
    assert!(!has_type_error(
        "fn main() -> Int { var g = Grid()\n g.cells().count() }"
    ));

    // The deferred door resolves the same way: `v` is still a variable when the
    // body is inferred, so this goes through `resolve_deferred_method` rather
    // than through the call site. A second copy of the binding rule is what that
    // path is at risk of not having.
    assert!(!has_type_error(
        "fn total(v) { v.map(|x| x * 2).sum() }\n\
         fn main() -> Int { var s = Set()\n s.insert(1)\n total(s) }"
    ));
}

/// **ADR-127 decision 4.** A conversion says what it accepts in its *receiver*,
/// so a wrong item shape is an ordinary unification report at the method name.
///
/// The alternative — a row that matches anything and faults at runtime — is what
/// writing the pair shape in prose would produce. `lookup` accepts the receiver
/// (it is one of the ten); the item unification is what reports.
#[test]
fn a_conversion_reports_a_wrong_item_shape_at_the_method_name() {
    // `[1, 2]` yields an `Int`, and `to_map` wants a pair.
    let errs = errors_of("fn main() -> Unit { out([1, 2].to_map()) }");
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(
        errs[0].starts_with("Y001") && errs[0].contains("found Int"),
        "a wrong item shape is a type error, not a missing method: {errs:?}"
    );

    // `to_bitset` says `Int` the same way.
    let errs = errors_of("fn main() -> Unit { out([\"a\"].to_bitset()) }");
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(
        errs[0].starts_with("Y001") && errs[0].contains("found Text"),
        "{errs:?}"
    );

    // And the shapes that do fit resolve, on receivers that are not `Vec`s.
    for src in [
        "fn main() -> Int { var m = Map()\n m[\"a\"] = 1\n m.to_vec().count() }",
        "fn main() -> Int { var s = Set()\n s.insert(1)\n s.to_vec().sum() }",
        "fn main() -> Int { var m = Map()\n m[\"a\"] = 1\n m.to_map().len() }",
        "fn main() -> Int { [\"a\"].frequencies().to_counter().len() }",
        "fn main() -> Int { (0..3).to_deque().len() }",
        "fn main() -> Int { [1, 2].to_min_heap().pop() }",
        "fn main() -> Int { [1, 2].to_max_heap().pop() }",
        "fn main() -> Int { [1, 2].to_bitset().len() }",
        "fn main() -> Int { \"ab\".to_vec().count() }",
    ] {
        assert!(!has_type_error(src), "{src}");
    }
}

/// **ADR-127.** The receiver generalizes; `zip`'s argument and `flat_map`'s
/// closure result do not.
///
/// The fused loop indexes each of those with `praxis_vec_len`/`praxis_vec_get`
/// directly — neither has an `IterPlan` in scope, because neither is the source
/// — so generalizing them would put a `SetPayload` under `praxis_vec_get`, which
/// is the exact wrong-type read `IterPlan` exists to prevent. The honest
/// boundary is that the pipeline generalizes over what it *walks*.
#[test]
fn a_pipelines_second_source_is_a_vec_and_the_spelling_is_to_vec() {
    // The receiver may be a `Set`…
    assert!(!has_type_error(
        "fn main() -> Int { var s = Set()\n s.insert(1)\n s.zip([1, 2]).count() }"
    ));
    // …and the argument may not.
    assert!(has_type_error(
        "fn main() -> Int { var s = Set()\n s.insert(1)\n [1, 2].zip(s).count() }"
    ));
    // `to_vec` is the spelling, which is a second thing it earns its place for.
    assert!(!has_type_error(
        "fn main() -> Int { var s = Set()\n s.insert(1)\n [1, 2].zip(s.to_vec()).count() }"
    ));
    // Same at `flat_map`'s closure result.
    assert!(!has_type_error(
        "fn main() -> Int { [1, 2].flat_map(|x| [x, x]).sum() }"
    ));
    assert!(has_type_error(
        "fn main() -> Int { var s = Set()\n s.insert(1)\n [1, 2].flat_map(|x| s).sum() }"
    ));
}

/// **ADR-127 decision 5.** `sorted_by_key` puts the `Ord` bound on the extracted
/// key, so the composite-ordering question ADR-045 deferred stays deferred.
///
/// `pairs.sorted()` is `Y006` — "values of type `(Text, Int)` cannot be ordered"
/// — and that is correct: MIR has one integer compare, so `(1, 2) < (1, 3)`
/// would compare two schema pointers. `sorted_by_key` is what gives "the five
/// most common values" a spelling.
#[test]
fn sorted_by_key_orders_what_sorted_cannot() {
    let pairs = "var m = Map()\n m[\"a\"] = 1\n var pairs = m.keys().zip(m.values())\n";
    assert!(
        has_type_error(&format!(
            "fn main() -> Unit {{ {pairs} out(pairs.sorted()) }}"
        )),
        "a composite is not orderable (ADR-045)"
    );
    assert!(
        !has_type_error(&format!(
            "fn main() -> Unit {{ {pairs} out(pairs.sorted_by_key(|p| p.1)) }}"
        )),
        "…but the key it carries is"
    );
    // The bound is on the key and is still a bound: a key with no order is
    // `Y006`, the same code `sorted` gives.
    assert!(has_type_error(
        "fn main() -> Unit { out([1, 2].sorted_by_key(|x| |y| y)) }"
    ));
    // And the receiver is generic like every other barrier's.
    assert!(!has_type_error(
        "fn main() -> Unit { var s = Set()\n s.insert(\"a\")\n out(s.sorted_by_key(|t| t.len())) }"
    ));
}

/// `reduce`'s accumulator *is* the element type, so its closure is `(T, T) -> T`
/// and a body that answers anything else is `Y001`.
///
/// It may not share `fold`'s `(Acc, T) -> Acc` shape: the free `Acc` that `fold`
/// needs — its seed is a separate argument and may be a separate type — leaves
/// the closure's first parameter untied to the element. Then
/// `["ab", "c"].reduce(|a, b| a.len())` type-checks with `a` an unpinned
/// variable, `len` resolved against no receiver at all, and the closure
/// answering `Int` while `reduce` answers `Text`.
#[test]
fn reduces_accumulator_is_the_element_type() {
    // A method body, a literal body and a comparison body are one mistake.
    for src in [
        "fn main() -> Unit { out([\"ab\", \"c\"].reduce(|a, b| a.len())) }",
        "fn main() -> Unit { out([\"ab\", \"c\"].reduce(|a, b| 1)) }",
        "fn main() -> Unit { out([1, 2].reduce(|a, b| a > b)) }",
    ] {
        assert!(has_type_error(src), "{src} must be Y001");
    }

    // The receiver is *pinned*, which is the other half: an unknown method on it
    // names the type it is not on, rather than "no type has a method `sqrt`".
    //
    // The probe has to be a name the catalog holds at this arity and `Int` does
    // not, or `has_name_at_arity` refuses it before a receiver is ever in hand
    // — `sqrt` is `Float`'s alone.
    let diags = analyze("fn main() -> Unit { out([1, 2].reduce(|a, b| a.sqrt())) }").diagnostics;
    let y110 = diags
        .iter()
        .find(|d| d.code().to_string() == "Y110")
        .unwrap_or_else(|| panic!("expected Y110, got {diags:?}"));
    assert!(
        y110.message().contains("on type `Int`"),
        "the receiver is pinned by the element type: {}",
        y110.message()
    );

    // …and the shapes that are right are still right, at two element types.
    for src in [
        "fn main() -> Unit { out([1, 2, 7].reduce(|a, b| a + b)) }",
        "fn main() -> Unit { out([1, 2, 7].reduce(|a, b| max(a, b))) }",
        "fn main() -> Unit { out([\"a\", \"b\"].reduce(|a, b| a + b)) }",
    ] {
        assert!(!has_type_error(src), "{src} must be accepted");
    }

    // `fold` keeps the separate accumulator — that is the whole difference
    // between them, and it is why the seed is an argument.
    assert!(!has_type_error(
        "fn main() -> Unit { out([1, 2, 7].fold(\"\", |acc, n| acc)) }"
    ));
}

/// `let` is a retired keyword, not a typo, and it gets `N009` with the fix it
/// actually needs.
///
/// Left to the generic near-miss suggester it is an `N001` with
/// `help: did you mean `Set`?` — the suggestion budget is `max(1, len/3)`, `let`
/// is three characters, so the budget is 1 and `Set` is one edit away. The
/// budget is rustc's rule and it is right in general; a word whose replacement
/// is known exactly should not be put to it.
#[test]
fn the_retired_let_keyword_is_named_rather_than_guessed_at() {
    for src in ["let x = 1\n", "fn f() -> Int {\n    let y = 2\n    y\n}\n"] {
        let diags = analyze(src).diagnostics;
        let n009 = diags
            .iter()
            .find(|d| d.code().to_string() == "N009")
            .unwrap_or_else(|| panic!("{src} must be N009, got {diags:?}"));
        assert!(
            n009.message().contains("written with `var`"),
            "{}",
            n009.message()
        );
        assert_eq!(
            n009.suggestions()
                .first()
                .and_then(|s| s.replacement.as_deref()),
            Some("var"),
            "the fix is machine-applicable"
        );
        assert!(
            !diags
                .iter()
                .any(|d| d.code().to_string() == "N001" && d.message().contains("`let`")),
            "and it replaces the N001 rather than joining it: {diags:?}"
        );
    }

    // `let` stays a legal **identifier**, which is what makes the head-of-a-
    // statement condition necessary: this declares one and reads it.
    assert!(!has_name_error("var let = 5\nout(let)\n"));

    // An ordinary near miss is untouched — the budget rule is not what was
    // wrong.
    assert!(has_name_error("out(lets)\n"));
}

// --- the character literal (ADR-141) ---

#[test]
fn a_char_literal_is_a_char() {
    assert_eq!(expr_type("'a'"), "Char");
    assert_eq!(expr_type("'\\n'"), "Char");
    // Above the interned table and outside the BMP — the *type* does not care
    // where the value lives.
    assert_eq!(expr_type("'é'"), "Char");
    assert_eq!(expr_type("'😀'"), "Char");
}

/// A `Char` literal pattern is the spelling that fits a `Char` scrutinee:
/// `match c { "#" => … }` is `expected Char, found Text`.
#[test]
fn a_char_literal_pattern_unifies_with_a_char_scrutinee() {
    assert!(!has_type_error(
        "fn f(c: Char) -> Int { match c { '#' => 1, '.' => 2, _ => 0 } }"
    ));
    assert!(!has_type_error(
        "fn f(t: Text) -> Int { match t[0] { '#' => 1, _ => 0 } }"
    ));
}

/// **The direct gate on `pattern.rs`'s literal type.** An arm that answered
/// `scrutinee_ty` would make a `Char` pattern agree with whatever it is asked
/// about, so this program would type-check and then compare an `Int` payload
/// against a `Char`'s.
#[test]
fn a_char_pattern_against_an_int_scrutinee_is_y001() {
    let errs = errors_of("fn f(n: Int) -> Int { match n { 'a' => 1, _ => 0 } }");
    assert!(
        errs.iter().any(|e| e.contains("Y001")),
        "expected a type error, got {errs:?}"
    );
    // …and the other direction, so the arm is not merely refusing everything.
    assert!(has_type_error(
        "fn f(c: Char) -> Int { match c { 1 => 1, _ => 0 } }"
    ));
}

/// `'#'` and `"#"[0]` are the same value written two ways (ADR-086, ADR-141) —
/// the equivalence that makes migrating a program from one spelling to the other
/// safe, and the reason the literal is a spelling change and not a new type.
#[test]
fn a_char_literal_and_a_text_subscript_are_the_same_type() {
    assert!(!has_type_error(
        "fn main() -> Unit { out('#' == \"#\"[0]) }"
    ));
    assert!(!has_type_error("fn main() -> Unit { out('a' < 'b') }"));
    // A `Char` is not a `Text`.
    assert!(has_type_error("fn main() -> Unit { out('a' == \"a\") }"));
}

/// A `Char` literal lowers to a `Lit::Char` carrying the code point — the same
/// node the input parser's `grid(char)` produces.
#[test]
fn a_char_literal_lowers_to_a_lit_char() {
    // `'a'` is U+0061, and the type on the node is the one inference decided.
    let (analysis, module) = analyze_and_lower("var c = 'a'\nout(c)\n");
    let entry = entry_fn(&module);
    let init = match &entry.body.stmts[0] {
        crate::TypedStmt::Var { init, .. } => init,
        other => panic!("expected a var statement, got {other:?}"),
    };
    match init {
        crate::TypedExpr::Lit {
            value: crate::Lit::Char(code),
            ty,
            ..
        } => {
            assert_eq!(*code, 0x61);
            assert_eq!(analysis.db.render(*ty), "Char");
        }
        other => panic!("expected Lit::Char, got {other:?}"),
    }

    // The escape and the multi-byte scalar decode to their code points and not
    // to their first byte — through `praxis-syntax`'s decoder, the same one the
    // lexer asks for the literal's length.
    for (src, code) in [("'\\n'", 0x0A_u32), ("'é'", 0xE9), ("'😀'", 0x1_F600)] {
        let (_, module) = analyze_and_lower(&format!("var c = {src}\n"));
        let entry = entry_fn(&module);
        let init = match &entry.body.stmts[0] {
            crate::TypedStmt::Var { init, .. } => init,
            other => panic!("expected a var statement, got {other:?}"),
        };
        assert!(
            matches!(init, crate::TypedExpr::Lit { value: crate::Lit::Char(c), .. } if *c == code),
            "{src} must lower to U+{code:04X}, got {init:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// ADR-146: a collection constructor's arity is its shape.
// ---------------------------------------------------------------------------

/// The fill decides the element type, with nothing written down. This is the
/// property that makes the sized form worth having as an *arity* of `Vec`
/// rather than as a second name: one fresh variable is shared between the fill
/// parameter and the result, so unification carries it out.
#[test]
fn a_sized_constructor_takes_its_element_type_from_the_fill() {
    assert_eq!(expr_type("Vec(3, false)"), "Vec[Bool]");
    assert_eq!(expr_type("Vec(3, 0)"), "Vec[Int]");
    assert_eq!(expr_type("Vec(3, \"a\")"), "Vec[Text]");
    assert_eq!(expr_type("Vec(3, '#')"), "Vec[Char]");
    assert_eq!(expr_type("Grid(2, 3, 0)"), "Grid[Int]");
    assert_eq!(expr_type("Grid(2, 3, false)"), "Grid[Bool]");
    assert_eq!(expr_type("Grid(2, 3, '.')"), "Grid[Char]");
    // A composite fill too — the case `praxis_grid_new` has no zero value for.
    assert_eq!(expr_type("Grid(2, 2, Vec[Int]())"), "Grid[Vec[Int]]");
    // And the empty forms are untouched, which is the half that says the
    // seeded scheme still generalizes.
    assert_eq!(expr_type("Vec[Int]()"), "Vec[Int]");
    assert_eq!(expr_type("Grid[Int]()"), "Grid[Int]");
}

/// ADR-065's bracket form composes with the sized one, because written type
/// arguments are applied to the callee's *result*. Agreement is silent;
/// disagreement is `Y001` on the fill, not on the whole function type.
#[test]
fn a_written_type_argument_constrains_a_sized_constructor() {
    assert_eq!(expr_type("Vec[Bool](3, false)"), "Vec[Bool]");
    assert_eq!(expr_type("Grid[Char](2, 2, '.')"), "Grid[Char]");
    assert!(has_type_error("var v = Vec[Int](3, false)\n"));
    assert!(has_type_error("var g = Grid[Bool](2, 2, 0)\n"));
    // The extents are `Int`s and nothing else.
    assert!(has_type_error("var v = Vec(\"a\", 0)\n"));
    assert!(has_type_error("var g = Grid(2, false, 0)\n"));
}

/// The wrong count reports the arity **the count selected**: `Vec(3)` is
/// measured against the sized form's two, not against the empty form's zero.
#[test]
fn a_sized_constructor_with_the_wrong_count_names_its_own_arity() {
    for (src, expected) in [
        (
            "var v = Vec(3)\n",
            "Y024: this function takes 2 argument(s), but 1 were given",
        ),
        (
            "var v = Vec(1, 2, 3)\n",
            "Y024: this function takes 2 argument(s), but 3 were given",
        ),
        (
            "var g = Grid(2, 3)\n",
            "Y024: this function takes 3 argument(s), but 2 were given",
        ),
        (
            "var g = Grid(1)\n",
            "Y024: this function takes 3 argument(s), but 1 were given",
        ),
    ] {
        assert!(
            errors_of(src).iter().any(|e| e == expected),
            "{src:?} must report {expected:?}, got {:?}",
            errors_of(src)
        );
    }
}

/// **The negative gate that keeps ADR-089 decision 1 intact everywhere else.**
/// ADR-146 carves out exactly two names; every other constructor still takes
/// nothing, and the diagnostic still says zero.
#[test]
fn only_vec_and_grid_are_sized_and_the_rest_still_take_nothing() {
    for src in [
        "var s = Set(3, 0)\n",
        "var m = Map(1, 2)\n",
        "var d = Deque(3, 0)\n",
        "var c = Counter(3, 0)\n",
        "var h = MinHeap(3, 0)\n",
        "var h = MaxHeap(3, 0)\n",
        "var b = BitSet(3)\n",
    ] {
        let errors = errors_of(src);
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("Y024: this function takes 0 argument(s)")),
            "{src:?} must still be measured against the nullary form, got {errors:?}"
        );
    }
    // And the nullary calls of all of them still typecheck.
    for src in [
        "var s = Set[Int]()\n",
        "var m = Map[Int, Int]()\n",
        "var d = Deque[Int]()\n",
        "var b = BitSet()\n",
        "var v = Vec[Int]()\n",
        "var g = Grid[Int]()\n",
    ] {
        assert!(errors_of(src).is_empty(), "{src:?}: {:?}", errors_of(src));
    }
}

/// A binding that shadows a constructor name is called, not constructed: the
/// symbol's kind decides. The sized table is consulted only for a `Builtin`
/// resolution, so the closure's own signature is what the call is checked
/// against — including its arity.
#[test]
fn a_shadowed_constructor_name_is_not_a_sized_constructor() {
    let shadowed = "var Vec = |n: Int, f: Bool| n\nvar probe = Vec(3, false)\n";
    assert_eq!(
        scheme_of(shadowed, "probe").as_deref(),
        Some("Int"),
        "the shadow's own return type, not `Vec[Bool]`"
    );
    // And its arity is the closure's, so a two-argument shadow refuses three.
    assert!(
        errors_of("var Vec = |n: Int, f: Bool| n\nvar p = Vec(1, true, 2)\n")
            .iter()
            .any(|e| e.starts_with("Y024"))
    );
}

/// **The fact ADR-146 decision 2 leans on.** A collection constructor has no
/// function value, so the second of ADR-089's two grounds — that an overloaded
/// name has no single closure value — cannot reach it. If this ever stops being
/// `Y022`, the carve-out needs rearguing.
#[test]
fn a_constructor_still_has_no_function_value() {
    for name in ["Vec", "Grid", "Set", "Map"] {
        let errors = errors_of(&format!("var f = {name}\n"));
        assert!(
            errors.iter().any(|e| e.starts_with("Y022")),
            "`{name}` in value position must still be Y022, got {errors:?}"
        );
    }
}

// --- string interpolation (§8.1, ADR-147) ---------------------------------

/// **ADR-147 decision 3.** A hole and `+` are *complements*: a hole is a
/// rendering site the program wrote, and `+` is not one.
///
/// So ADR-085 decision 2's refusal of an implicit conversion to `Text` for `+`
/// stands beside a universal rendering inside holes, rather than being settled
/// by it.
///
/// Both halves are asserted from **one** source file, so an edit that "unifies"
/// the two by relaxing `+` cannot pass this by only being run against the half
/// it did not change.
#[test]
fn text_plus_an_int_is_still_y001_beside_a_hole_that_renders_it() {
    let src = "fn main() {\n    var n = 3\n    out(\"n = {n}\")\n    out(\"n = \" + n)\n}\n";
    let errors = errors_of(src);
    assert_eq!(
        errors.len(),
        1,
        "the hole is clean and the `+` is not: {errors:?}"
    );
    assert!(
        errors[0].starts_with("Y001") && errors[0].contains("expected Text, found Int"),
        "`+` must still refuse an Int operand (ADR-085 decision 2), got {errors:?}"
    );
}

/// A hole imposes **no** requirement on what it holds (ADR-147 decision 2).
/// Every one of these is a type that has no `to_text()` row and never will, so
/// this also pins that the feature is not the desugar-through-`to_text()` route
/// ADR-143 decision 5 proposed.
#[test]
fn a_hole_accepts_any_type() {
    for src in [
        "fn main() { var v = [1, 2, 3]\n    out(\"{v}\") }",
        "fn main() { var t = (1, \"x\")\n    out(\"{t}\") }",
        "fn main() { var b = true\n    out(\"{b}\") }",
        "fn main() { var f = 1.5\n    out(\"{f}\") }",
        "fn main() { var c = '#'\n    out(\"{c}\") }",
        "fn main() { var s = Set[Int]()\n    out(\"{s}\") }",
        "fn main() { var u = ()\n    out(\"{u}\") }",
    ] {
        assert!(errors_of(src).is_empty(), "{src}: {:?}", errors_of(src));
    }
}

/// An interpolated literal is `Text`, so it composes with everything `Text`
/// composes with — `+`, a `Text`-annotated binding, a `Text` parameter.
#[test]
fn an_interpolated_literal_is_text() {
    assert_eq!(
        scheme_of("var n = 1\nvar s = \"n = {n}\"\n", "s").as_deref(),
        Some("Text")
    );
    assert!(errors_of("var n = 1\nvar s: Text = \"{n}\" + \"!\"\n").is_empty());
}

/// A hole is inferred **for its own sake**, so a mistake inside one is reported
/// where it is written rather than swallowed by the universal rendering. This is
/// the half "a hole accepts any type" could be mistaken for removing.
#[test]
fn a_mistake_inside_a_hole_is_still_reported() {
    // An unknown name is `N001`, from the resolver walking into the hole.
    assert!(has_name_error("fn main() { out(\"{nope}\") }"));
    // And a type error inside the hole is the type error it would be anywhere.
    let errors = errors_of("fn main() { var n = 1\n    out(\"{n + true}\") }");
    assert!(
        errors.iter().any(|e| e.starts_with("Y001")),
        "expected the hole's own mismatch, got {errors:?}"
    );
}
