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
    // The root is a SOURCE_FILE; casting it as a LetStmt must fail.
    let tree = root("let x = 1");
    assert!(crate::LetStmt::cast(tree.clone()).is_none());
    assert!(SourceFile::cast(tree).is_some());
}

#[test]
fn let_stmt_exposes_name_and_init() {
    let tree = root("let x = 1");
    let file = SourceFile::cast(tree).unwrap();
    let stmt = file.stmts().next().unwrap();
    let let_stmt = crate::LetStmt::cast(stmt).unwrap();
    assert_eq!(let_stmt.name().unwrap().text(), "x");
    // The initializer is a LITERAL expression.
    match let_stmt.init() {
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
    let tree = root("let x = 1");
    let file = SourceFile::cast(tree).unwrap();
    let let_stmt = crate::LetStmt::cast(file.stmts().next().unwrap()).unwrap();
    assert_eq!(
        let_stmt.span(),
        Span::new(BytePos::from(0), BytePos::from(9))
    );
}
