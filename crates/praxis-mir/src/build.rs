//! HIR → MIR lowering (ADR-015).
//!
//! Walks a [`TypedModule`](praxis_hir::TypedModule) and emits one MIR
//! [`Function`] per source `fn`. Every language value is materialized as a
//! `GcRef` in a `Gc` slot; scalar payloads (`i64` out of an `Int`) live in
//! transient `Scalar` slots only between extraction and the next materialization,
//! never across a safepoint.
//!
//! The emitted MIR is **not** run through liveness here; callers invoke
//! [`crate::annotate`] to populate `live_roots`. Keeping the two phases separate
//! makes the builder easier to test in isolation.

#![allow(dead_code)] // Consumed by the Cranelift backend (Phase 4).

use praxis_hir::{
    AssignOp, BinOp, Lit, TypedExpr, TypedFn, TypedItem, TypedModule, TypedStmt, UnaryOp,
};
use praxis_types::{Type, TypeDb};

use crate::ir::{
    AllocKind, BlockId, CallTarget, CmpOp, Function, Inst, IntBinOp, LocalId, LocalKind,
    ScalarKind, Terminator,
};

/// Lower a typed module to a MIR function per source `fn` item.
#[must_use]
pub fn lower_module(module: &TypedModule, db: &mut TypeDb) -> Vec<Function> {
    module
        .items
        .iter()
        .map(|item| match item {
            TypedItem::Fn(f) => lower_fn(f, db),
        })
        .collect()
}

/// The (function-local) symbol id → local id map for the current frame, plus
/// the block we are currently appending to.
struct Builder<'a> {
    func: Function,
    /// Source binding (by symbol id) → its `Gc` slot.
    locals: std::collections::HashMap<praxis_hir::SymbolId, LocalId>,
    /// The current block being appended to.
    cur: BlockId,
    /// The fault continuation: the block to jump to on a pending fault.
    fault_block: BlockId,
    db: &'a TypeDb,
    /// Cached scalar type handles (these `TypeDb` constructors need `&mut`).
    int_ty: Type,
    bool_ty: Type,
    text_ty: Type,
    char_ty: Type,
    unit_ty: Type,
}

fn lower_fn(f: &TypedFn, db: &mut TypeDb) -> Function {
    // Cache scalar handles once.
    let int_ty = db.int();
    let bool_ty = db.bool();
    let text_ty = db.text();
    let char_ty = db.char();
    let unit_ty = db.unit();

    let mut func = Function {
        name: f.name.clone(),
        params: Vec::new(),
        return_local: LocalId(0),
        locals: Vec::new(),
        blocks: Vec::new(),
        debug_names: Vec::new(),
    };
    let entry = func.new_block();
    let fault = func.new_block();
    func.blocks[fault.0 as usize].term = Terminator::Fault;

    let mut b = Builder {
        func,
        locals: std::collections::HashMap::new(),
        cur: entry,
        fault_block: fault,
        db,
        int_ty,
        bool_ty,
        text_ty,
        char_ty,
        unit_ty,
    };

    // Parameters: one `Gc` slot each.
    for p in &f.params {
        let id = b.alloc_gc(p.ty, Some(p.name.clone()));
        b.locals.insert(p.symbol, id);
        b.func.params.push(id);
    }

    // The return slot.
    let ret = b.alloc_gc(f.return_type, None);
    b.func.return_local = ret;

    // Lower the body. The tail expression's value is the function's result.
    let tail = lower_block_body(&mut b, &f.body);
    // Materialize the tail into the return slot and return it.
    b.func.blocks[b.cur.0 as usize].insts.push(Inst::MoveGc {
        dst: ret,
        src: tail,
    });
    b.func.blocks[b.cur.0 as usize].term = Terminator::Return { value: ret };

    b.func
}

impl<'a> Builder<'a> {
    fn alloc_gc(&mut self, ty: Type, debug_name: Option<String>) -> LocalId {
        self.func.new_local(LocalKind::Gc, ty, debug_name)
    }

    fn alloc_scalar(&mut self, sk: ScalarKind) -> LocalId {
        // Scalar slots carry a placeholder Type; their ScalarKind is authoritative.
        self.func.new_local(LocalKind::Scalar(sk), Type(0), None)
    }

    fn push(&mut self, inst: Inst) {
        self.func.blocks[self.cur.0 as usize].insts.push(inst);
    }

    /// Emit a fault check after a faultable instruction.
    fn check_fault(&mut self) {
        self.push(Inst::CheckFault {
            on_fault: self.fault_block,
        });
    }
}

/// Lower a block's statements then its tail, returning the `GcRef` local holding
/// the block's value. Statements execute for effect; the tail's value flows out.
fn lower_block_body(b: &mut Builder<'_>, block: &praxis_hir::TypedBlock) -> LocalId {
    for stmt in &block.stmts {
        lower_stmt(b, stmt);
    }
    lower_expr_gc(b, &block.tail)
}

fn lower_stmt(b: &mut Builder<'_>, stmt: &TypedStmt) {
    match stmt {
        TypedStmt::Let {
            symbol, name, init, ..
        }
        | TypedStmt::Var {
            symbol, name, init, ..
        } => {
            let v = lower_expr_gc(b, init);
            let slot = b.alloc_gc(expr_static_type(init), Some(name.clone()));
            b.push(Inst::MoveGc { dst: slot, src: v });
            b.locals.insert(*symbol, slot);
        }
        TypedStmt::Assign {
            symbol,
            name: _,
            op,
            value,
        } => {
            // Read the current binding's slot.
            let dst = match b.locals.get(symbol).copied() {
                Some(id) => id,
                None => return, // unresolved in HIR; skip (diagnostic already emitted)
            };
            if *op == AssignOp::Assign {
                let v = lower_expr_gc(b, value);
                b.push(Inst::MoveGc { dst, src: v });
            } else {
                // Compound assignment: dst = dst <op> value (Int arithmetic).
                let cur = b.alloc_gc(Type(0), None);
                b.push(Inst::MoveGc { dst: cur, src: dst });
                let rhs = lower_expr_gc(b, value);
                let result = lower_int_binop(b, op_to_int_binop(*op), cur, rhs);
                let materialized = lower_materialize(b, result);
                b.push(Inst::MoveGc {
                    dst,
                    src: materialized,
                });
            }
        }
        TypedStmt::Expr(e) => {
            let _ = lower_expr_gc(b, e);
        }
    }
}

/// Lower an expression to a `GcRef`-holding local (materializing if needed).
fn lower_expr_gc(b: &mut Builder<'_>, e: &TypedExpr) -> LocalId {
    match e {
        TypedExpr::Lit { value, .. } => lower_lit_gc(b, value),
        TypedExpr::Path { symbol, .. } => b.locals.get(symbol).copied().unwrap_or_else(|| {
            // Unresolved: allocate a Unit placeholder so downstream lowering is sound.
            lower_lit_gc(b, &Lit::Int(0))
        }),
        TypedExpr::Bin { op, lhs, rhs, .. } => {
            // Short-circuit ops must not eagerly evaluate `rhs` — it is lowered
            // only on the path that needs it (inside `lower_logical_or`).
            if *op == BinOp::LogicalOr {
                let l = lower_expr_gc(b, lhs);
                return lower_logical_or(b, l, rhs);
            }
            let l = lower_expr_gc(b, lhs);
            let r = lower_expr_gc(b, rhs);
            match op {
                // Arithmetic: extract scalars, do a checked op, materialize.
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                    let result = lower_int_binop(b, binop_to_int(*op), l, r);
                    lower_materialize(b, result)
                }
                // Comparison: extract scalars, compare, materialize a Bool.
                BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                    let li = lower_extract_int(b, l);
                    let ri = lower_extract_int(b, r);
                    let bool_scalar = b.alloc_scalar(ScalarKind::Bool);
                    b.push(Inst::IntCmp {
                        op: binop_to_cmp(*op),
                        dst: bool_scalar,
                        lhs: li,
                        rhs: ri,
                    });
                    lower_materialize_bool(b, bool_scalar)
                }
                // LogicalOr is handled above (before eager rhs lowering).
                BinOp::LogicalOr => unreachable!(),
            }
        }
        TypedExpr::Unary { op, operand, .. } => {
            let o = lower_expr_gc(b, operand);
            match op {
                UnaryOp::Neg => {
                    // 0 - operand, as checked Int subtraction.
                    let zero = lower_lit_gc(b, &Lit::Int(0));
                    let result = lower_int_binop(b, IntBinOp::Sub, zero, o);
                    lower_materialize(b, result)
                }
                UnaryOp::Not => {
                    // Logical not on Bool: `!x` is `x == false`.
                    lower_logical_not(b, o)
                }
            }
        }
        TypedExpr::Paren { inner, .. } => match inner {
            Some(e) => lower_expr_gc(b, e),
            None => lower_lit_gc(b, &Lit::Int(0)),
        },
        TypedExpr::Block(blk) => lower_block_body(b, blk),
        TypedExpr::If {
            cond,
            then_block,
            else_block,
            ..
        } => lower_if(b, cond, then_block, else_block.as_deref()),
        TypedExpr::While { cond, body, .. } => {
            lower_while(b, cond, body);
            lower_lit_gc(b, &Lit::Int(0)) // while yields Unit
        }
        TypedExpr::Call {
            callee: _,
            callee_name,
            args,
            ..
        } => {
            // The `Vec()` builtin constructs an empty vector via praxis_vec_new.
            // For M5 the element type is Int (a real type-arg `Vec[T]()` is a
            // follow-up); pass a null descriptor and let the wrapper default.
            if callee_name == "Vec" {
                let dst = b.alloc_gc(Type(0), None);
                // Pass 0 (null descriptor) as the single arg; praxis_vec_new
                // defaults to INT when the descriptor pointer is null.
                let null_arg = b.alloc_scalar(ScalarKind::Int);
                b.push(Inst::ConstInt {
                    dst: null_arg,
                    value: 0,
                });
                let arg_gc = b.alloc_gc(Type(0), None);
                b.push(Inst::MoveGc {
                    dst: arg_gc,
                    src: null_arg,
                });
                b.push(Inst::Call {
                    dst,
                    callee: CallTarget::Runtime("praxis_vec_new".to_string()),
                    args: vec![arg_gc],
                    live_roots: Vec::new(),
                });
                b.check_fault();
                return dst;
            }
            // The `out(x)` builtin writes x to stdout via praxis_write_stdout.
            if callee_name == "out" {
                let arg_local = args
                    .first()
                    .map(|a| lower_expr_gc(b, a))
                    .unwrap_or_else(|| lower_lit_gc(b, &Lit::Int(0)));
                let dst = b.alloc_gc(Type(0), None);
                b.push(Inst::Call {
                    dst,
                    callee: CallTarget::Runtime("praxis_write_stdout".to_string()),
                    args: vec![arg_local],
                    live_roots: Vec::new(),
                });
                // out does not fault; no check_fault needed.
                return dst;
            }
            let arg_locals: Vec<LocalId> = args.iter().map(|a| lower_expr_gc(b, a)).collect();
            let dst = b.alloc_gc(Type(0), None);
            b.push(Inst::Call {
                dst,
                callee: CallTarget::User(callee_name.clone()),
                args: arg_locals,
                live_roots: Vec::new(),
            });
            b.check_fault();
            dst
        }
        TypedExpr::MethodCall {
            receiver,
            name: _,
            lowering_symbol,
            args,
            ..
        } => {
            // A method call lowers to a runtime-wrapper call. The receiver is
            // the first argument; the method's explicit args follow. The
            // catalog resolved `lowering_symbol` (e.g. `praxis_vec_push`); if
            // empty (an intrinsic not yet emitted), skip the call.
            if lowering_symbol.is_empty() {
                // No runtime symbol yet (M8 intrinsic): evaluate the receiver
                // and args for effect and return a Unit placeholder.
                let _ = lower_expr_gc(b, receiver);
                for a in args {
                    let _ = lower_expr_gc(b, a);
                }
                return lower_lit_gc(b, &Lit::Int(0));
            }
            let mut arg_locals: Vec<LocalId> = Vec::with_capacity(args.len() + 1);
            arg_locals.push(lower_expr_gc(b, receiver));
            for a in args {
                arg_locals.push(lower_expr_gc(b, a));
            }
            let dst = b.alloc_gc(Type(0), None);
            b.push(Inst::Call {
                dst,
                callee: CallTarget::Runtime(lowering_symbol.clone()),
                args: arg_locals,
                live_roots: Vec::new(),
            });
            // Method calls may fault (e.g. vec.get out of bounds); check after.
            b.check_fault();
            dst
        }
        TypedExpr::Tuple { elements, .. } => {
            // Lower each element for effect; tuples materialize in M5. Return a
            // Unit placeholder (the M4 acceptance corpus is Int-typed).
            for el in elements {
                let _ = lower_expr_gc(b, el);
            }
            lower_lit_gc(b, &Lit::Int(0))
        }
        // M6: `read`/`parse` lower to a runtime call against the parser plan.
        TypedExpr::Read { plan_index, .. } => lower_read(b, *plan_index),
        TypedExpr::Parse {
            text, plan_index, ..
        } => lower_parse(b, text, *plan_index),
        // M7: nominal record literal + field access.
        TypedExpr::RecordLit {
            record_def_id,
            fields,
            ..
        } => lower_record_lit(b, *record_def_id, fields),
        TypedExpr::FieldGet {
            receiver,
            field_idx,
            ..
        } => lower_field_get(b, receiver, *field_idx),
        // M7: enum variant construction.
        TypedExpr::EnumVariant {
            enum_def_id,
            variant_idx,
            args,
            ..
        } => lower_enum_variant(b, *enum_def_id, *variant_idx, args),
        // M7-WS5: match expression — lowered to a tag-compare branch chain.
        TypedExpr::Match {
            scrutinee, arms, ..
        } => lower_match(b, scrutinee, arms),
    }
}

/// Lower a `read parser_expr`: get the input buffer, then run the plan.
fn lower_read(b: &mut Builder<'_>, plan_index: u32) -> LocalId {
    // 1. Get the input buffer from the runtime context.
    let input = b.alloc_gc(Type(0), None);
    b.push(Inst::Call {
        dst: input,
        callee: CallTarget::Runtime("praxis_get_input".to_string()),
        args: vec![],
        live_roots: Vec::new(),
    });
    // 2. Run the parser plan against it.
    run_parser_plan(b, plan_index, input)
}

/// Lower a `parse(text, parser_expr)`: run the plan against the text argument.
fn lower_parse(b: &mut Builder<'_>, text: &TypedExpr, plan_index: u32) -> LocalId {
    let input = lower_expr_gc(b, text);
    run_parser_plan(b, plan_index, input)
}

/// Emit the call to `praxis_run_parser(ctx, plan_index, input) -> GcRef`, then
/// check for a parse fault. The plan_index is boxed as an Int GcRef to match the
/// uniform ABI; the runtime wrapper reads its payload.
fn run_parser_plan(b: &mut Builder<'_>, plan_index: u32, input: LocalId) -> LocalId {
    // Box the plan index as an Int GcRef.
    let idx_scalar = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ConstInt {
        dst: idx_scalar,
        value: plan_index as i64,
    });
    let idx_gc = b.alloc_gc(b.int_ty, None);
    b.push(Inst::Alloc {
        dst: idx_gc,
        alloc: AllocKind::Int { value: idx_scalar },
        live_roots: Vec::new(),
    });
    // Call praxis_run_parser(ctx, idx, input) -> result.
    let dst = b.alloc_gc(Type(0), None);
    b.push(Inst::Call {
        dst,
        callee: CallTarget::Runtime("praxis_run_parser".to_string()),
        args: vec![idx_gc, input],
        live_roots: Vec::new(),
    });
    b.check_fault();
    dst
}

/// Lower a literal to a `GcRef` local (allocating the object).
fn lower_lit_gc(b: &mut Builder<'_>, value: &Lit) -> LocalId {
    match value {
        Lit::Int(n) => {
            let scalar = b.alloc_scalar(ScalarKind::Int);
            b.push(Inst::ConstInt {
                dst: scalar,
                value: *n,
            });
            let dst = b.alloc_gc(b.int_ty, None);
            b.push(Inst::Alloc {
                dst,
                alloc: AllocKind::Int { value: scalar },
                live_roots: Vec::new(),
            });
            dst
        }
        Lit::Bool(v) => {
            let scalar = b.alloc_scalar(ScalarKind::Bool);
            b.push(Inst::ConstInt {
                dst: scalar,
                value: if *v { 1 } else { 0 },
            });
            let dst = b.alloc_gc(b.bool_ty, None);
            b.push(Inst::Alloc {
                dst,
                alloc: AllocKind::Bool { value: scalar },
                live_roots: Vec::new(),
            });
            dst
        }
        Lit::Text(s) => {
            let dst = b.alloc_gc(b.text_ty, None);
            b.push(Inst::Alloc {
                dst,
                alloc: AllocKind::Text { value: s.clone() },
                live_roots: Vec::new(),
            });
            dst
        }
        Lit::Char(c) => {
            // Char's payload is a u32 Unicode scalar; ConstInt carries it as i64.
            let scalar = b.alloc_scalar(ScalarKind::Char);
            b.push(Inst::ConstInt {
                dst: scalar,
                value: *c as i64,
            });
            let dst = b.alloc_gc(b.char_ty, None);
            b.push(Inst::Alloc {
                dst,
                alloc: AllocKind::Char { value: scalar },
                live_roots: Vec::new(),
            });
            dst
        }
    }
}

/// Extract an `Int` payload into a scalar local.
fn lower_extract_int(b: &mut Builder<'_>, src: LocalId) -> LocalId {
    let dst = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ExtractScalar {
        dst,
        src,
        scalar: ScalarKind::Int,
    });
    dst
}

/// Lower a checked Int binary op on two `GcRef` operands, returning the scalar
/// result local. Inserts a fault check for Div/Rem (div-by-zero) — Add/Sub/Mul
/// fault on overflow which the backend checks after the op.
fn lower_int_binop(b: &mut Builder<'_>, op: IntBinOp, lhs_gc: LocalId, rhs_gc: LocalId) -> LocalId {
    let lhs = lower_extract_int(b, lhs_gc);
    let rhs = lower_extract_int(b, rhs_gc);
    let dst = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::IntBinOp { op, dst, lhs, rhs });
    b.check_fault();
    dst
}

/// Materialize an `Int` scalar into a fresh `GcRef`.
fn lower_materialize(b: &mut Builder<'_>, scalar: LocalId) -> LocalId {
    let dst = b.alloc_gc(b.int_ty, None);
    b.push(Inst::Materialize {
        dst,
        src: scalar,
        scalar: ScalarKind::Int,
        live_roots: Vec::new(),
    });
    dst
}

/// Materialize a `Bool` scalar into a fresh `GcRef`.
fn lower_materialize_bool(b: &mut Builder<'_>, scalar: LocalId) -> LocalId {
    let dst = b.alloc_gc(b.bool_ty, None);
    b.push(Inst::Materialize {
        dst,
        src: scalar,
        scalar: ScalarKind::Bool,
        live_roots: Vec::new(),
    });
    dst
}

/// Lower short-circuiting logical or: `lhs || rhs`.
///
/// `lhs || rhs` is `if lhs { true } else { rhs }`: evaluate `lhs`; if it is
/// true the result is `true` and `rhs` is *not* evaluated (its side effects and
/// any GC safepoint are skipped). Otherwise the result is `rhs`. Both operands
/// are `Bool` `GcRef`s; `lhs_gc` is already lowered, `rhs_expr` is lowered only
/// on the false path.
fn lower_logical_or(b: &mut Builder<'_>, lhs_gc: LocalId, rhs_expr: &TypedExpr) -> LocalId {
    let lhs_bool = b.alloc_scalar(ScalarKind::Bool);
    b.push(Inst::ExtractScalar {
        dst: lhs_bool,
        src: lhs_gc,
        scalar: ScalarKind::Bool,
    });
    let result = b.alloc_gc(b.bool_ty, None);
    let true_blk = b.func.new_block();
    let false_blk = b.func.new_block();
    let join = b.func.new_block();
    b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
        cond: lhs_bool,
        then_block: true_blk,
        else_block: false_blk,
    };
    // lhs true → result = true.
    b.cur = true_blk;
    let true_val = lower_lit_gc(b, &Lit::Bool(true));
    b.push(Inst::MoveGc {
        dst: result,
        src: true_val,
    });
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: join };
    // lhs false → evaluate rhs.
    b.cur = false_blk;
    let rhs_val = lower_expr_gc(b, rhs_expr);
    b.push(Inst::MoveGc {
        dst: result,
        src: rhs_val,
    });
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: join };
    b.cur = join;
    result
}

/// Lower logical not: `!operand` flips a `Bool`. Implemented as an integer
/// comparison against 0 (false), yielding the negation.
fn lower_logical_not(b: &mut Builder<'_>, operand_gc: LocalId) -> LocalId {
    let operand_bool = b.alloc_scalar(ScalarKind::Bool);
    b.push(Inst::ExtractScalar {
        dst: operand_bool,
        src: operand_gc,
        scalar: ScalarKind::Bool,
    });
    let zero = b.alloc_scalar(ScalarKind::Bool);
    // Bool is represented as i8: 0 = false, 1 = true. `!x` is `x == 0`.
    b.push(Inst::ConstInt {
        dst: zero,
        value: 0,
    });
    let negated = b.alloc_scalar(ScalarKind::Bool);
    b.push(Inst::IntCmp {
        op: CmpOp::Eq,
        dst: negated,
        lhs: operand_bool,
        rhs: zero,
    });
    lower_materialize_bool(b, negated)
}

/// Lower an `if` expression, returning the `GcRef` holding its value.
fn lower_if(
    b: &mut Builder<'_>,
    cond: &TypedExpr,
    then_block: &praxis_hir::TypedBlock,
    else_block: Option<&praxis_hir::TypedBlock>,
) -> LocalId {
    let cond_gc = lower_expr_gc(b, cond);
    let cond_scalar = b.alloc_scalar(ScalarKind::Bool);
    b.push(Inst::ExtractScalar {
        dst: cond_scalar,
        src: cond_gc,
        scalar: ScalarKind::Bool,
    });

    let result = b.alloc_gc(then_block.ty, None);
    let then_blk = b.func.new_block();
    let else_blk = b.func.new_block();
    let join = b.func.new_block();

    b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
        cond: cond_scalar,
        then_block: then_blk,
        else_block: else_blk,
    };

    // Then branch.
    b.cur = then_blk;
    let then_val = lower_block_body(b, then_block);
    b.push(Inst::MoveGc {
        dst: result,
        src: then_val,
    });
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: join };

    // Else branch.
    b.cur = else_blk;
    let else_val = match else_block {
        Some(blk) => lower_block_body(b, blk),
        None => lower_lit_gc(b, &Lit::Int(0)), // no else → Unit
    };
    b.push(Inst::MoveGc {
        dst: result,
        src: else_val,
    });
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: join };

    b.cur = join;
    result
}

/// Lower a `while` loop (for effect; yields Unit).
fn lower_while(b: &mut Builder<'_>, cond: &TypedExpr, body: &praxis_hir::TypedBlock) {
    let header = b.func.new_block();
    let body_blk = b.func.new_block();
    let exit = b.func.new_block();

    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };
    b.cur = header;

    let cond_gc = lower_expr_gc(b, cond);
    let cond_scalar = b.alloc_scalar(ScalarKind::Bool);
    b.push(Inst::ExtractScalar {
        dst: cond_scalar,
        src: cond_gc,
        scalar: ScalarKind::Bool,
    });
    b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
        cond: cond_scalar,
        then_block: body_blk,
        else_block: exit,
    };

    b.cur = body_blk;
    let _ = lower_block_body(b, body);
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };

    b.cur = exit;
}

// --- helpers --------------------------------------------------------------

fn op_to_int_binop(op: AssignOp) -> IntBinOp {
    match op {
        AssignOp::AddAssign => IntBinOp::Add,
        AssignOp::SubAssign => IntBinOp::Sub,
        AssignOp::MulAssign => IntBinOp::Mul,
        AssignOp::DivAssign => IntBinOp::Div,
        AssignOp::RemAssign => IntBinOp::Rem,
        AssignOp::Assign => IntBinOp::Add, // unused; Assign handled separately
    }
}

fn binop_to_int(op: BinOp) -> IntBinOp {
    match op {
        BinOp::Add => IntBinOp::Add,
        BinOp::Sub => IntBinOp::Sub,
        BinOp::Mul => IntBinOp::Mul,
        BinOp::Div => IntBinOp::Div,
        BinOp::Rem => IntBinOp::Rem,
        _ => IntBinOp::Add,
    }
}

fn binop_to_cmp(op: BinOp) -> CmpOp {
    match op {
        BinOp::Eq => CmpOp::Eq,
        BinOp::Neq => CmpOp::Neq,
        BinOp::Lt => CmpOp::Lt,
        BinOp::Gt => CmpOp::Gt,
        BinOp::Le => CmpOp::Le,
        BinOp::Ge => CmpOp::Ge,
        _ => CmpOp::Eq,
    }
}

/// The static type carried on a typed expression.
fn expr_static_type(e: &TypedExpr) -> Type {
    match e {
        TypedExpr::Lit { ty, .. }
        | TypedExpr::Path { ty, .. }
        | TypedExpr::Bin { ty, .. }
        | TypedExpr::Unary { ty, .. }
        | TypedExpr::Paren { ty, .. }
        | TypedExpr::If { ty, .. }
        | TypedExpr::While { ty, .. }
        | TypedExpr::Call { ty, .. }
        | TypedExpr::MethodCall { ty, .. }
        | TypedExpr::Tuple { ty, .. }
        | TypedExpr::Read { ty, .. }
        | TypedExpr::Parse { ty, .. }
        | TypedExpr::RecordLit { ty, .. }
        | TypedExpr::FieldGet { ty, .. }
        | TypedExpr::EnumVariant { ty, .. }
        | TypedExpr::Match { ty, .. } => *ty,
        TypedExpr::Block(blk) => blk.ty,
    }
}

/// Lower a record literal `Name { field: expr, … }` (M7, §4.5). Lowers each
/// field initializer to a `Gc` local, then emits an `Alloc` with
/// `AllocKind::Record`. The codegen builds the `RecordSchema` from the def-id
/// and embeds its address as an immediate in the allocation call.
fn lower_record_lit(
    b: &mut Builder<'_>,
    record_def_id: praxis_types::RecordDefId,
    fields: &[(u32, TypedExpr)],
) -> LocalId {
    // Lower each field initializer in declaration order (already sorted by the
    // HIR lowerer).
    let field_locals: Vec<LocalId> = fields.iter().map(|(_, e)| lower_expr_gc(b, e)).collect();
    let dst = b.alloc_gc(Type(0), None);
    b.push(Inst::Alloc {
        dst,
        alloc: AllocKind::Record {
            record_def_id: record_def_id.to_u32(),
            fields: field_locals,
        },
        live_roots: Vec::new(),
    });
    dst
}

/// Lower a field access `receiver.field` (M7, §4.5). Emits a `LoadField`
/// instruction that reads the field's `GcRef` out of the record payload.
fn lower_field_get(b: &mut Builder<'_>, receiver: &TypedExpr, field_idx: u32) -> LocalId {
    let src = lower_expr_gc(b, receiver);
    let dst = b.alloc_gc(Type(0), None);
    b.push(Inst::LoadField {
        dst,
        src,
        field_idx,
    });
    dst
}

/// Lower an enum variant construction (M7, §4.6). Lowers payload args to `Gc`
/// locals, then emits an `Alloc` with `AllocKind::Enum`.
fn lower_enum_variant(
    b: &mut Builder<'_>,
    enum_def_id: praxis_types::EnumDefId,
    variant_idx: u32,
    args: &[TypedExpr],
) -> LocalId {
    let arg_locals: Vec<LocalId> = args.iter().map(|a| lower_expr_gc(b, a)).collect();
    let dst = b.alloc_gc(Type(0), None);
    b.push(Inst::Alloc {
        dst,
        alloc: AllocKind::Enum {
            enum_def_id: enum_def_id.to_u32(),
            variant_idx,
            args: arg_locals,
        },
        live_roots: Vec::new(),
    });
    dst
}

/// Lower a `match scrutinee { arms }` expression (M7, §4.6) to a tag-compare
/// branch chain. The scrutinee must be an enum value; the result is the unified
/// type of all arm bodies.
///
/// Strategy: read the scrutinee's tag via `praxis_enum_tag`, then for each arm
/// with a variant index, compare the tag and branch. Wildcard arms (None) are
/// the fall-through default. Each arm extracts its payload bindings and lowers
/// its body.
fn lower_match(
    b: &mut Builder<'_>,
    scrutinee: &TypedExpr,
    arms: &[praxis_hir::TypedMatchArm],
) -> LocalId {
    let scrut_gc = lower_expr_gc(b, scrutinee);
    let result = b.alloc_gc(Type(0), None);

    // Read the tag directly as a scalar (no allocation, no boxing).
    let tag_local = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::EnumTag {
        dst: tag_local,
        src: scrut_gc,
    });

    let join = b.func.new_block();
    // Create a block per arm. We chain them: each variant arm tests its tag and
    // either branches to its body or falls through to the next test. The last
    // wildcard arm is the default.
    for arm in arms {
        let arm_body_blk = b.func.new_block();
        let next_test_blk = b.func.new_block();
        match arm.variant_idx {
            Some(vidx) => {
                // Compare tag == vidx.
                let cmp_dst = b.alloc_scalar(ScalarKind::Bool);
                let expected = b.alloc_scalar(ScalarKind::Int);
                b.push(Inst::ConstInt {
                    dst: expected,
                    value: vidx as i64,
                });
                b.push(Inst::IntCmp {
                    op: CmpOp::Eq,
                    dst: cmp_dst,
                    lhs: tag_local,
                    rhs: expected,
                });
                b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                    cond: cmp_dst,
                    then_block: arm_body_blk,
                    else_block: next_test_blk,
                };
            }
            None => {
                // Wildcard/default arm: unconditional jump to body.
                b.func.blocks[b.cur.0 as usize].term = Terminator::Jump {
                    target: arm_body_blk,
                };
            }
        }
        // Arm body: extract payload bindings, lower the body, store result.
        b.cur = arm_body_blk;
        for (symbol, slot) in &arm.bindings {
            let payload_local = b.alloc_gc(Type(0), None);
            b.push(Inst::EnumPayloadGet {
                dst: payload_local,
                src: scrut_gc,
                idx: *slot,
            });
            b.locals.insert(*symbol, payload_local);
        }
        let body_val = lower_expr_gc(b, &arm.body);
        b.push(Inst::MoveGc {
            dst: result,
            src: body_val,
        });
        b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: join };
        // Continue to the next test.
        b.cur = next_test_blk;
    }
    // If no wildcard arm caught everything, the fall-through reaches here.
    // Push a Unit default so the join block always has a valid result.
    let unit_val = lower_lit_gc(b, &Lit::Int(0));
    b.push(Inst::MoveGc {
        dst: result,
        src: unit_val,
    });
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: join };
    b.cur = join;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_ast::AstNode;
    use praxis_hir::{analyze_root, lower};
    use praxis_parser::parse;
    use praxis_source::SourceMap;

    fn lower_src_to_mir(src: &str) -> (Vec<Function>, praxis_hir::Analysis) {
        let map = SourceMap::new();
        let file = map.intern("build_test.px", src);
        let parsed = parse(file, src);
        let mut analysis = analyze_root(file, &parsed.tree);
        let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
        let module = lower(file, &root, &mut analysis);
        let funcs = lower_module(&module, &mut analysis.db);
        (funcs, analysis)
    }

    #[test]
    fn lowers_constant_int_fn_to_one_function() {
        let (funcs, _analysis) = lower_src_to_mir("fn f() -> Int { 42 }");
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "f");
        assert!(!funcs[0].blocks.is_empty());
    }

    #[test]
    fn lowers_arithmetic_with_extract_and_materialize() {
        let (funcs, _analysis) = lower_src_to_mir("fn f(a: Int, b: Int) -> Int { a + b }");
        let f = &funcs[0];
        // Should contain ExtractScalar (for a, b) and a Materialize (the result).
        let has_extract = f.blocks.iter().any(|b| {
            b.insts
                .iter()
                .any(|i| matches!(i, Inst::ExtractScalar { .. }))
        });
        let has_materialize = f.blocks.iter().any(|b| {
            b.insts
                .iter()
                .any(|i| matches!(i, Inst::Materialize { .. }))
        });
        let has_binop = f.blocks.iter().any(|b| {
            b.insts.iter().any(|i| {
                matches!(
                    i,
                    Inst::IntBinOp {
                        op: IntBinOp::Add,
                        ..
                    }
                )
            })
        });
        assert!(has_extract, "should extract operands");
        assert!(has_binop, "should emit an IntBinOp::Add");
        assert!(has_materialize, "should materialize the result");
    }

    #[test]
    fn lowers_if_into_three_blocks() {
        let (funcs, _analysis) =
            lower_src_to_mir("fn f(n: Int) -> Int { if n > 0 { 1 } else { 2 } }");
        let f = &funcs[0];
        // entry + then + else + join = at least 4 blocks.
        assert!(
            f.blocks.len() >= 4,
            "expected >=4 blocks, got {}",
            f.blocks.len()
        );
        // There must be a Branch terminator somewhere.
        let has_branch = f
            .blocks
            .iter()
            .any(|b| matches!(b.term, Terminator::Branch { .. }));
        assert!(has_branch);
    }

    #[test]
    fn lowers_while_loop_with_backedge() {
        let (funcs, _analysis) =
            lower_src_to_mir("fn f(n: Int) -> Int { var i = 0; while i < n { i = i + 1 }; i }");
        let f = &funcs[0];
        // The body block jumps back to the header (a lower block id) — a backedge.
        let has_backedge = f.blocks.iter().any(|b| match b.term {
            Terminator::Jump { target } => target <= b.id,
            _ => false,
        });
        assert!(has_backedge, "expected a loop backedge");
    }

    #[test]
    fn lowers_recursive_call() {
        let (funcs, _analysis) = lower_src_to_mir("fn f(n: Int) -> Int { f(n) }");
        let f = &funcs[0];
        let has_call = f
            .blocks
            .iter()
            .any(|b| b.insts.iter().any(|i| matches!(i, Inst::Call { .. })));
        assert!(has_call, "should emit a Call instruction");
    }
}
