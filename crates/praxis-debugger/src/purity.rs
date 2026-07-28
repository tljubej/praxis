//! The read-only gate for `p EXPR` / `type EXPR` (§9.5, §19.10, M10b-WS4).
//!
//! §9.5: "`p EXPR` […] Mutating expressions are rejected in the initial
//! debugger. This prevents changes to a state that cannot safely resume." And
//! §19.10's fifth acceptance criterion: "No command can mutate or resume a
//! faulted state in v1."
//!
//! This module implements that gate as a tree-walk over the typed expression
//! the `p EXPR` evaluator builds. It rejects any node that could mutate state,
//! consume input, diverge, or call into arbitrary user code. The accepted set
//! is the pure, terminating, read-only fragment of the expression language:
//!
//! - `Lit`, `Path` — values and local reads (the snapshot's locals).
//! - `Bin`, `Unary`, `Paren` — arithmetic/logic (may *fault* — overflow/div0
//!   — but never mutates; a fault is reported, not a mutation).
//! - `If`, `Block`, `Tuple`, `FieldGet`, `Match`, `RecordLit`, `EnumVariant`
//!   — pure structure.
//! - `MethodCall` — only when the catalog tagged it `Pure` (`v.len()`,
//!   `v.get(i)`, `text.len()`, …); impure methods (`v.push`, `v.set`) reject.
//!
//! Rejected: any user `Call` (cannot prove purity without a separate analysis),
//! `Read`/`Parse` (consume input + can fault on the cursor), `Closure` literals
//! (could capture and mutate), and the diverging nodes (`While`/`For`/`Loop`/
//! `Break`/`Continue`/`Return` — they don't yield a value in this context).
//!
//! Assignment is statement-only in Praxis (it cannot appear inside an
//! expression), so it never reaches this walk — a free win.

use praxis_stdlib::Purity;

use praxis_hir::{TypedBlock, TypedExpr};

/// Check that `expr` (and every sub-expression) is read-only / pure / non-
/// diverging. Returns `Ok(())` if the whole tree is acceptable for `p EXPR`,
/// or `Err(reason)` describing the first rejecting node found.
///
/// The walk is structural and conservative: anything not explicitly accepted
/// is rejected. This is the safest default for a read-only evaluator.
pub fn assert_read_only(expr: &TypedExpr) -> Result<(), String> {
    walk_expr(expr)
}

fn walk_expr(e: &TypedExpr) -> Result<(), String> {
    match e {
        // --- Accepted leaves ---
        TypedExpr::Lit { .. } | TypedExpr::Path { .. } => Ok(()),

        // --- Accepted recursion (pure structure) ---
        TypedExpr::Bin { lhs, rhs, .. } => {
            walk_expr(lhs)?;
            walk_expr(rhs)
        }
        TypedExpr::Unary { operand, .. } => walk_expr(operand),
        TypedExpr::Paren { inner, .. } => {
            if let Some(inner) = inner {
                walk_expr(inner)
            } else {
                Ok(())
            }
        }
        TypedExpr::Block(b) => walk_block(b),
        TypedExpr::If {
            cond,
            then_block,
            else_block,
            ..
        } => {
            walk_expr(cond)?;
            walk_block(then_block)?;
            if let Some(eb) = else_block.as_deref() {
                walk_block(eb)?;
            }
            Ok(())
        }
        TypedExpr::Tuple { elements, .. } => {
            for el in elements {
                walk_expr(el)?;
            }
            Ok(())
        }
        TypedExpr::RecordLit { fields, .. } => {
            for (_, init) in fields {
                walk_expr(init)?;
            }
            Ok(())
        }
        TypedExpr::FieldGet { receiver, .. } => walk_expr(receiver),
        TypedExpr::EnumVariant { args, .. } => {
            for a in args {
                walk_expr(a)?;
            }
            Ok(())
        }
        TypedExpr::Match {
            scrutinee, arms, ..
        } => {
            walk_expr(scrutinee)?;
            for arm in arms {
                walk_expr(&arm.body)?;
            }
            Ok(())
        }
        TypedExpr::MethodCall {
            receiver,
            args,
            purity,
            name,
            ..
        } => {
            // The gate: only Pure methods pass. Impure methods (push/set/…)
            // mutate the receiver or allocate visibly.
            if *purity == Purity::Impure {
                return Err(format!(
                    "method `{name}` is impure (may mutate state) — `p` rejects mutating expressions"
                ));
            }
            walk_expr(receiver)?;
            for a in args {
                walk_expr(a)?;
            }
            Ok(())
        }

        // --- Rejected: user calls, input consumption, divergence, closures ---
        TypedExpr::Call { callee_name, .. } => Err(format!(
            "call to `{callee_name}` — `p` cannot prove a user function is read-only"
        )),
        TypedExpr::Read { .. } => {
            Err("`read` consumes input and may fault — `p` rejects it".to_string())
        }
        TypedExpr::Parse { .. } => {
            Err("`parse` consumes input and may fault — `p` rejects it".to_string())
        }
        TypedExpr::Closure { .. } => {
            Err("closure literals may capture and mutate — `p` rejects them".to_string())
        }
        TypedExpr::While { .. } => {
            Err("`while` diverges — `p` evaluates a single value".to_string())
        }
        TypedExpr::For { .. } => Err("`for` diverges — `p` evaluates a single value".to_string()),
        TypedExpr::Loop { .. } => Err("`loop` diverges — `p` evaluates a single value".to_string()),
        TypedExpr::Break { .. } => {
            Err("`break` diverges — `p` evaluates a single value".to_string())
        }
        TypedExpr::Continue { .. } => {
            Err("`continue` diverges — `p` evaluates a single value".to_string())
        }
        TypedExpr::Return { .. } => {
            Err("`return` diverges — `p` evaluates a single value".to_string())
        }
    }
}

/// Walk a block's statements + tail. Statement-level `Let`/`Var`/`Assign` can
/// appear inside a `Block` or `If` arm; `Assign` and `Var` (reassignable) are
/// mutations and must reject. (`Let` is a pure binding; allowed.)
fn walk_block(b: &TypedBlock) -> Result<(), String> {
    for stmt in &b.stmts {
        use praxis_hir::TypedStmt;
        match stmt {
            TypedStmt::Let { init, .. } => walk_expr(init)?,
            TypedStmt::Var { .. } => {
                return Err("`var` declares a mutable binding — `p` rejects mutation".to_string());
            }
            TypedStmt::Assign { .. } => {
                return Err("assignment mutates — `p` rejects mutating expressions".to_string());
            }
            TypedStmt::Expr(e) => walk_expr(e)?,
        }
    }
    walk_expr(&b.tail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_hir::{BinOp, Lit, TypedExpr};
    use praxis_stdlib::Purity;
    use praxis_types::TypeData;

    /// Build a minimal `TypeDb` + helpers to mint typed exprs for the walk.
    fn mk_db() -> praxis_types::TypeDb {
        praxis_types::TypeDb::new()
    }

    fn lit_int(db: &mut praxis_types::TypeDb) -> TypedExpr {
        TypedExpr::Lit {
            value: Lit::Int(1),
            ty: db.int(),
            span: (0, 0),
        }
    }

    fn bin_int(db: &mut praxis_types::TypeDb, lhs: TypedExpr, rhs: TypedExpr) -> TypedExpr {
        TypedExpr::Bin {
            op: BinOp::Add,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            ty: db.int(),
            span: (0, 0),
        }
    }

    #[test]
    fn accepts_lit_and_path() {
        let mut db = mk_db();
        assert!(assert_read_only(&lit_int(&mut db)).is_ok());
        let path = TypedExpr::Path {
            symbol: praxis_hir::SymbolId(0),
            ty: db.int(),
            span: (0, 0),
        };
        assert!(assert_read_only(&path).is_ok());
    }

    #[test]
    fn accepts_pure_arithmetic() {
        let mut db = mk_db();
        let one = lit_int(&mut db);
        let two = TypedExpr::Lit {
            value: Lit::Int(2),
            ty: db.int(),
            span: (0, 0),
        };
        let expr = bin_int(&mut db, one, two);
        assert!(assert_read_only(&expr).is_ok());
    }

    #[test]
    fn rejects_impure_method_call() {
        let mut db = mk_db();
        let receiver = TypedExpr::Path {
            symbol: praxis_hir::SymbolId(0),
            ty: db.int(),
            span: (0, 0),
        };
        let arg = lit_int(&mut db);
        let call = TypedExpr::MethodCall {
            receiver: Box::new(receiver),
            name: "push".to_string(),
            lowering_symbol: Some(praxis_stdlib::abi::RuntimeSymbol::VecPush),
            args: vec![arg],
            purity: Purity::Impure,
            ty: db.unit(),
            span: (0, 0),
        };
        let err = assert_read_only(&call).unwrap_err();
        assert!(err.contains("impure"), "{err}");
        assert!(err.contains("push"), "{err}");
    }

    #[test]
    fn accepts_pure_method_call() {
        let mut db = mk_db();
        let receiver = TypedExpr::Path {
            symbol: praxis_hir::SymbolId(0),
            ty: db.int(),
            span: (0, 0),
        };
        let call = TypedExpr::MethodCall {
            receiver: Box::new(receiver),
            name: "len".to_string(),
            lowering_symbol: Some(praxis_stdlib::abi::RuntimeSymbol::VecLen),
            args: vec![],
            purity: Purity::Pure,
            ty: db.int(),
            span: (0, 0),
        };
        assert!(assert_read_only(&call).is_ok());
    }

    #[test]
    fn rejects_impure_method_inside_pure_arithmetic() {
        // `1 + v.push(2)` — the impure call is nested; the walk must find it.
        let mut db = mk_db();
        let one = lit_int(&mut db);
        let receiver = TypedExpr::Path {
            symbol: praxis_hir::SymbolId(0),
            ty: db.int(),
            span: (0, 0),
        };
        let impure = TypedExpr::MethodCall {
            receiver: Box::new(receiver),
            name: "push".to_string(),
            lowering_symbol: Some(praxis_stdlib::abi::RuntimeSymbol::VecPush),
            args: vec![lit_int(&mut db)],
            purity: Purity::Impure,
            ty: db.unit(),
            span: (0, 0),
        };
        let expr = bin_int(&mut db, one, impure);
        assert!(assert_read_only(&expr).is_err());
    }

    #[test]
    fn rejects_read_and_parse() {
        let mut db = mk_db();
        // Any real plan id will do — the gate rejects the *node*, not the plan.
        // `0` is no longer spellable here (IP-12): `PlanId` is a `NonZeroU32`,
        // which is what stopped the HIR's failure sentinel from naming the
        // first plan any program registered.
        let plan = praxis_hir::PlanId::from_raw(1).expect("1 is a plan id");
        let read = TypedExpr::Read {
            plan,
            ty: db.int(),
            span: (0, 0),
        };
        assert!(assert_read_only(&read).unwrap_err().contains("read"));
        let parse = TypedExpr::Parse {
            text: Box::new(lit_int(&mut db)),
            plan,
            ty: db.int(),
            span: (0, 0),
        };
        assert!(assert_read_only(&parse).unwrap_err().contains("parse"));
    }

    #[test]
    fn rejects_diverging_nodes() {
        let mut db = mk_db();
        // Build a few diverging-shape exprs with minimal fields; the walk
        // rejects on the variant before inspecting fields.
        let ty = db.int();
        let loop_ = TypedExpr::Loop {
            body: Box::new(praxis_hir::TypedBlock {
                stmts: vec![],
                tail: TypedExpr::Lit {
                    value: Lit::Int(0),
                    ty,
                    span: (0, 0),
                },
                ty,
            }),
            ty,
            span: (0, 0),
        };
        assert!(assert_read_only(&loop_).unwrap_err().contains("loop"));
    }

    #[test]
    fn type_data_scalar_int_round_trips() {
        // Sanity: confirm the TypeDb renders Int as "Int" (the printer the
        // evaluator uses to synthesize param annotations). Guards against a
        // regression in praxis-types::pretty.
        let mut db = mk_db();
        let i = db.int();
        assert!(matches!(db.data(db.follow(i)), TypeData::Scalar(_)));
        assert_eq!(db.render(i), "Int");
    }
}
