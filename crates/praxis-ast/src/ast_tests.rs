//! Tests for the typed AST wrappers (ADR-009).
//!
//! Each test parses a small source fragment with `praxis-parser`, then casts the
//! root into typed wrappers and checks the accessors. These pin the node shape
//! the HIR will walk; a parser change that breaks a wrapper fails here.

#![cfg(test)]

use crate::{AstNode, ElseBranch, Expr, ExprStmt, SourceFile};
use praxis_parser::parse;
use praxis_source::{BytePos, SourceMap, Span};
use praxis_syntax::SyntaxKind;

fn root(text: &str) -> praxis_syntax::SyntaxNode {
    let map = SourceMap::new();
    let id = map.intern("ast_test.px", text);
    parse(id, text).tree
}

#[test]
fn cast_rejects_wrong_kind() {
    // The root is a SOURCE_FILE; casting it as a VarStmt must fail.
    let tree = root("var x = 1");
    assert!(crate::VarStmt::cast(tree.clone()).is_none());
    assert!(SourceFile::cast(tree).is_some());
}

#[test]
fn let_stmt_exposes_name_and_init() {
    let tree = root("var x = 1");
    let file = SourceFile::cast(tree).unwrap();
    let stmt = file.stmts().next().unwrap();
    let var_binding = crate::VarStmt::cast(stmt).unwrap();
    assert_eq!(var_binding.name().unwrap().text(), "x");
    // The initializer is a LITERAL expression.
    match var_binding.init() {
        Some(Expr::Literal(lit)) => {
            assert_eq!(lit.token().unwrap().kind(), SyntaxKind::IntLit);
        }
        other => panic!("expected literal init, got {other:?}"),
    }
}

#[test]
fn var_stmt_exposes_name_and_init() {
    let tree = root("var score = 0");
    let file = SourceFile::cast(tree).unwrap();
    let var_stmt = crate::VarStmt::cast(file.stmts().next().unwrap()).unwrap();
    assert_eq!(var_stmt.name().unwrap().text(), "score");
    assert!(matches!(var_stmt.init(), Some(Expr::Literal(_))));
}

#[test]
fn assign_stmt_exposes_name_op_and_value() {
    let tree = root("x = x + 1");
    let file = SourceFile::cast(tree).unwrap();
    let assign = crate::AssignStmt::cast(file.stmts().next().unwrap()).unwrap();
    assert_eq!(assign.name().unwrap().text(), "x");
    assert_eq!(assign.op().unwrap().kind(), SyntaxKind::EQ);
    assert!(matches!(assign.value(), Some(Expr::Bin(_))));
}

#[test]
fn fn_item_exposes_name_params_and_body() {
    let tree = root("fn add(a: Int, b: Int) -> Int { a + b }");
    let file = SourceFile::cast(tree).unwrap();
    let fn_item = crate::FnItem::cast(file.stmts().next().unwrap()).unwrap();
    assert_eq!(fn_item.name().unwrap().text(), "add");
    let params: Vec<_> = fn_item.param_list().unwrap().params().collect();
    assert_eq!(params.len(), 2);
    assert_eq!(params[0].name().unwrap().text(), "a");
    assert_eq!(params[1].name().unwrap().text(), "b");
    assert!(fn_item.body().is_some());
}

#[test]
fn path_expr_is_a_name_reference() {
    // `out(a)`: the argument `a` is a PathExpr naming `a`.
    let tree = root("out(a)");
    let file = SourceFile::cast(tree).unwrap();
    let stmt = file.stmts().next().unwrap();
    let expr_stmt = ExprStmt::cast(stmt).unwrap();
    let call = match expr_stmt.expr().unwrap() {
        Expr::Call(call) => call,
        other => panic!("expected call, got {other:?}"),
    };
    assert_eq!(call.callee().unwrap().name().unwrap().text(), "out");
    let args: Vec<_> = call.arg_list().unwrap().args().collect();
    assert_eq!(args.len(), 1);
    match &args[0] {
        Expr::Path(p) => assert_eq!(p.name().unwrap().text(), "a"),
        other => panic!("expected path arg, got {other:?}"),
    }
}

#[test]
fn bin_expr_exposes_op_and_operands() {
    let tree = root("1 + 2");
    let file = SourceFile::cast(tree).unwrap();
    let expr_stmt = ExprStmt::cast(file.stmts().next().unwrap()).unwrap();
    let bin = match expr_stmt.expr().unwrap() {
        Expr::Bin(b) => b,
        other => panic!("expected bin, got {other:?}"),
    };
    assert_eq!(bin.op().unwrap().kind(), SyntaxKind::PLUS);
    let (lhs, rhs) = bin.operands();
    assert!(matches!(lhs, Some(Expr::Literal(_))));
    assert!(matches!(rhs, Some(Expr::Literal(_))));
}

#[test]
fn if_expr_exposes_cond_then_else() {
    let tree = root("if x { out(1) } else { out(2) }");
    let file = SourceFile::cast(tree).unwrap();
    let expr_stmt = ExprStmt::cast(file.stmts().next().unwrap()).unwrap();
    let if_expr = match expr_stmt.expr().unwrap() {
        Expr::If(e) => e,
        other => panic!("expected if, got {other:?}"),
    };
    assert!(matches!(if_expr.cond(), Some(Expr::Path(_))));
    assert!(if_expr.then_branch().is_some());
    let else_branch: ElseBranch = if_expr.else_branch().unwrap();
    assert!(matches!(else_branch.body(), Some(Expr::Block(_))));
}

#[test]
fn span_round_trips_via_bridge() {
    let tree = root("var x = 1");
    let file = SourceFile::cast(tree).unwrap();
    let var_binding = crate::VarStmt::cast(file.stmts().next().unwrap()).unwrap();
    assert_eq!(
        var_binding.span(),
        Span::new(BytePos::from(0), BytePos::from(9))
    );
}

/// TY-08, at the level it lived at: every position that can carry a written
/// type must *see* one, whichever of the three node kinds the parser chose.
///
/// `TypeRef::cast` used to accept only `TYPE_REF`, so a direct `TUPLE_TYPE` or
/// `FN_TYPE` — which is what `(Int, Text)` and `(Int) -> Int` produce — made
/// the accessor answer `None`. The annotation was not rejected; it was
/// invisible, and inference invented a fresh variable in its place.
#[test]
fn every_annotation_position_sees_a_tuple_or_function_type() {
    let src = "struct Boxed { f: (Int) -> Int }\n\
               enum Wrapped { Fn((Int) -> Int), Pair((Int, Text)) }\n\
               fn takes(x: (Int, Text), g: (Int) -> Int) -> (Int, Text) { x }\n\
               var pair: (Int, Text) = (1, \"a\")\n\
               var f: (Int) -> Int = |n| n";
    let file = SourceFile::cast(root(src)).unwrap();
    let mut stmts = file.stmts();

    let struct_item = crate::StructItem::cast(stmts.next().unwrap()).unwrap();
    let field = struct_item.field_list().unwrap().fields().next().unwrap();
    assert_eq!(
        field.ty().map(|t| t.syntax().kind()),
        Some(SyntaxKind::FN_TYPE),
        "a function-typed struct field"
    );
    // …and the field's *value* accessor must not mistake that type for one.
    assert!(field.expr().is_none(), "a declaration field has no value");

    let enum_item = crate::EnumItem::cast(stmts.next().unwrap()).unwrap();
    let payload_kinds: Vec<_> = enum_item
        .variants()
        .map(|v| {
            v.payload_types()
                .and_then(|ts| ts.first().map(|t| t.syntax().kind()))
        })
        .collect();
    assert_eq!(
        payload_kinds,
        vec![Some(SyntaxKind::FN_TYPE), Some(SyntaxKind::TUPLE_TYPE)],
        "function- and tuple-typed enum payloads"
    );

    let fn_item = crate::FnItem::cast(stmts.next().unwrap()).unwrap();
    let param_kinds: Vec<_> = fn_item
        .param_list()
        .unwrap()
        .params()
        .map(|p| p.ty().map(|t| t.syntax().kind()))
        .collect();
    assert_eq!(
        param_kinds,
        vec![Some(SyntaxKind::TUPLE_TYPE), Some(SyntaxKind::FN_TYPE)],
        "tuple- and function-typed parameters"
    );
    assert_eq!(
        fn_item.return_type().map(|t| t.syntax().kind()),
        Some(SyntaxKind::TUPLE_TYPE),
        "a tuple return type"
    );

    let var_binding = crate::VarStmt::cast(stmts.next().unwrap()).unwrap();
    assert_eq!(
        var_binding.ty().map(|t| t.syntax().kind()),
        Some(SyntaxKind::TUPLE_TYPE),
        "a tuple-annotated `var`"
    );
    assert!(
        var_binding.init().is_some(),
        "…and its initializer survives"
    );

    let var_stmt = crate::VarStmt::cast(stmts.next().unwrap()).unwrap();
    assert_eq!(
        var_stmt.ty().map(|t| t.syntax().kind()),
        Some(SyntaxKind::FN_TYPE),
        "a function-annotated `var`"
    );
    assert!(var_stmt.init().is_some(), "…and its initializer survives");
}

/// The kind set is one predicate, and it is the same one the wrapper uses. A
/// node kind that is not a type node must not become a `TypeRef`.
#[test]
fn only_the_three_type_node_kinds_are_annotations() {
    for kind in [
        SyntaxKind::TYPE_REF,
        SyntaxKind::TUPLE_TYPE,
        SyntaxKind::FN_TYPE,
    ] {
        assert!(kind.is_type_node(), "{kind:?} is a type node");
    }
    for kind in [
        SyntaxKind::TUPLE_EXPR,
        SyntaxKind::PAREN_EXPR,
        SyntaxKind::PARAM,
        SyntaxKind::SOURCE_FILE,
    ] {
        assert!(!kind.is_type_node(), "{kind:?} is not a type node");
    }
    // The expression `(1, 2)` is a TUPLE_EXPR, and casting it as an annotation
    // must fail — the wrapper is kind-checked, not shape-guessed.
    let file = SourceFile::cast(root("var x = (1, 2)")).unwrap();
    let var_binding = crate::VarStmt::cast(file.stmts().next().unwrap()).unwrap();
    assert!(
        var_binding.ty().is_none(),
        "an initializer is not an annotation"
    );
}

/// A pattern is a **record** pattern because of its brace (ADR-091 Decision 3).
///
/// `Pattern::kind()` used to decide the record shape from the presence of a
/// `PATTERN_FIELD` child and, before that, from a direct `Ident` token. Both
/// tests are silently wrong at one end each, and both failures are the *same*
/// failure: the pattern becomes something that matches everything.
///
/// **Observed red with the brace test removed but the grammar kept**: a headless
/// `{a, b}` has no direct `Ident` (its names are inside `PATTERN_FIELD` nodes),
/// so `kind()` reaches the final `PatternKind::Wildcard` fallthrough and the arm
/// becomes an irrefutable catch-all — HIR-07's defect, shipped by a grammar-only
/// fix. `P {}` has no `PATTERN_FIELD` at all, so it read as
/// `PatternKind::Name("P")`, a *binding*: `match q { P {} => 1 }` where `q` is a
/// `Q` ran the arm and returned 1 (REP-66). This test is the gate on both.
#[test]
fn a_patterns_brace_is_what_makes_it_a_record_pattern() {
    use crate::{Pattern, PatternKind};

    fn kind_of(arm: &str) -> PatternKind {
        let tree = root(&format!("var r = match s {{ {arm} => 1 }}"));
        tree.descendants()
            .find_map(Pattern::cast)
            .unwrap_or_else(|| panic!("`{arm}` produced no PATTERN node"))
            .kind()
    }

    // Headless: a record pattern with no name, and emphatically not a wildcard.
    assert_eq!(kind_of("{a, b}"), PatternKind::Record(None));
    // A head with empty braces: still a record pattern, not a binding named `P`.
    assert_eq!(kind_of("P {}"), PatternKind::Record(Some("P".into())));
    // The shapes it must stay apart from — one `Ident`, three meanings.
    assert_eq!(kind_of("P {a}"), PatternKind::Record(Some("P".into())));
    assert_eq!(kind_of("P(a)"), PatternKind::Variant("P".into()));
    assert_eq!(kind_of("P"), PatternKind::Name("P".into()));
    assert_eq!(kind_of("(a, b)"), PatternKind::Tuple);
    assert_eq!(kind_of("_"), PatternKind::Wildcard);

    // A headless pattern has no head token to read, which is what the `Option`
    // in `Record` records — resolution must not go looking for one.
    let tree = root("var r = match s { {a, b} => 1 }");
    let pat = tree.descendants().find_map(Pattern::cast).unwrap();
    assert!(
        pat.name_token().is_none(),
        "a headless record pattern has no head token"
    );
    assert_eq!(pat.fields().count(), 2, "…and its fields are still its own");
}

/// A list literal's elements come out of its `ARG_LIST`, in source order, at
/// every arity the grammar admits — including none.
///
/// The wrapper reaches *through* the `ARG_LIST` the way `IndexExpr::indices`
/// does, and that is why it is worth a test: `syntax.children()` on a
/// `LIST_EXPR` finds the arg list and no expressions at all, so a wrapper
/// written like `TupleExpr`'s — whose elements are direct children — would
/// answer "empty" for every list ever written.
#[test]
fn a_list_literals_elements_are_its_arg_lists() {
    fn elements(src: &str) -> Vec<String> {
        let tree = root(src);
        let list = tree
            .descendants()
            .find_map(crate::ListExpr::cast)
            .expect("a LIST_EXPR");
        list.elements()
            .iter()
            .map(|e| e.syntax().text().to_string())
            .collect()
    }

    assert_eq!(elements("var v = [1, 2, 3]"), ["1", "2", "3"]);
    assert_eq!(elements("var v = [1]"), ["1"]);
    assert!(elements("var v = []").is_empty());
    // A trailing comma adds no element (REP-17).
    assert_eq!(elements("var v = [1, 2,]"), ["1", "2"]);
    // Order is source order, and an element is a whole expression.
    assert_eq!(elements("var v = [a + 1, f(2)]"), ["a + 1", "f(2)"]);

    // The outer list of a nested one holds the inner lists, not their elements.
    assert_eq!(elements("var v = [[1, 2], [3]]"), ["[1, 2]", "[3]"]);

    // And the enum casts to the right variant, which is what every walk
    // dispatches on.
    let tree = root("var v = [1]");
    let list = tree.descendants().find_map(crate::ListExpr::cast).unwrap();
    assert!(matches!(
        Expr::cast(list.syntax().clone()),
        Some(Expr::List(_))
    ));
}
