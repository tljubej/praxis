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
    capture::Capture, AssignOp, BinOp, Lit, TypedBlock, TypedExpr, TypedFn, TypedItem, TypedModule,
    TypedParam, TypedStmt, UnaryOp,
};
use praxis_types::{Type, TypeDb};

use crate::ir::{
    AllocKind, BlockId, CallTarget, CmpOp, Function, Inst, IntBinOp, LocalId, LocalKind,
    ScalarKind, Terminator,
};

/// Lower a typed module to MIR: one [`Function`] per source `fn` item, plus one
/// synthetic [`Function`] per closure literal (M7, §4.10). The synthetic closure
/// functions are appended after the source functions; they are referenced by name
/// from `AllocKind::Closure` at the allocation site.
#[must_use]
pub fn lower_module(module: &TypedModule, db: &mut TypeDb) -> Vec<Function> {
    let escaping = &module.escaping_vars;
    let mut funcs: Vec<Function> = module
        .items
        .iter()
        .map(|item| match item {
            TypedItem::Fn(f) => lower_fn(f, db, escaping),
        })
        .collect();
    // Collect every closure literal in the module (across all fn bodies) and
    // emit one synthetic function per closure, in source order.
    for item in &module.items {
        let TypedItem::Fn(tfn) = item;
        for closure in collect_closures(&tfn.body) {
            funcs.push(lower_closure_fn(&closure, db, escaping));
        }
    }
    funcs
}

/// A closure literal lifted out of a body for synthetic-function emission.
/// Carries the pieces of `TypedExpr::Closure` needed by `lower_closure_fn`.
struct LiftedClosure {
    fn_name: String,
    params: Vec<TypedParam>,
    body: TypedBlock,
    captures: Vec<Capture>,
}

/// Walk a typed block collecting every `TypedExpr::Closure` (depth-first, source
/// order) as a [`LiftedClosure`]. Nested closures are included — each becomes its
/// own synthetic function.
fn collect_closures(block: &TypedBlock) -> Vec<LiftedClosure> {
    let mut out = Vec::new();
    collect_closures_block(block, &mut out);
    out
}

fn collect_closures_block(block: &TypedBlock, out: &mut Vec<LiftedClosure>) {
    for stmt in &block.stmts {
        collect_closures_stmt(stmt, out);
    }
    collect_closures_expr(&block.tail, out);
}

fn collect_closures_stmt(stmt: &TypedStmt, out: &mut Vec<LiftedClosure>) {
    match stmt {
        TypedStmt::Let { init, .. } | TypedStmt::Var { init, .. } => {
            collect_closures_expr(init, out)
        }
        TypedStmt::Assign { value, .. } => collect_closures_expr(value, out),
        TypedStmt::Expr(e) => collect_closures_expr(e, out),
    }
}

fn collect_closures_expr(e: &TypedExpr, out: &mut Vec<LiftedClosure>) {
    match e {
        TypedExpr::Closure {
            fn_name,
            params,
            body,
            captures,
            ..
        } => {
            // Recurse into the closure's body first so inner closures are emitted
            // before the outer (deterministic ordering for tests).
            collect_closures_block(body, out);
            out.push(LiftedClosure {
                fn_name: fn_name.clone(),
                params: params.clone(),
                body: (**body).clone(),
                captures: captures.clone(),
            });
        }
        TypedExpr::Lit { .. } | TypedExpr::Path { .. } | TypedExpr::Read { .. } => {}
        TypedExpr::Bin { lhs, rhs, .. } => {
            collect_closures_expr(lhs, out);
            collect_closures_expr(rhs, out);
        }
        TypedExpr::Unary { operand, .. } => collect_closures_expr(operand, out),
        TypedExpr::Paren { inner, .. } => {
            if let Some(inner) = inner {
                collect_closures_expr(inner, out);
            }
        }
        TypedExpr::Block(b) => collect_closures_block(b, out),
        TypedExpr::If {
            cond,
            then_block,
            else_block,
            ..
        } => {
            collect_closures_expr(cond, out);
            collect_closures_block(then_block, out);
            if let Some(eb) = else_block.as_deref() {
                collect_closures_block(eb, out);
            }
        }
        TypedExpr::While { cond, body, .. } => {
            collect_closures_expr(cond, out);
            collect_closures_block(body, out);
        }
        TypedExpr::Call { args, .. } => {
            for a in args {
                collect_closures_expr(a, out);
            }
        }
        TypedExpr::MethodCall { receiver, args, .. } => {
            collect_closures_expr(receiver, out);
            for a in args {
                collect_closures_expr(a, out);
            }
        }
        TypedExpr::Tuple { elements, .. } => {
            for el in elements {
                collect_closures_expr(el, out);
            }
        }
        TypedExpr::Parse { text, .. } => collect_closures_expr(text, out),
        TypedExpr::RecordLit { fields, .. } => {
            for (_, init) in fields {
                collect_closures_expr(init, out);
            }
        }
        TypedExpr::FieldGet { receiver, .. } => collect_closures_expr(receiver, out),
        TypedExpr::EnumVariant { args, .. } => {
            for a in args {
                collect_closures_expr(a, out);
            }
        }
        TypedExpr::Match {
            scrutinee, arms, ..
        } => {
            collect_closures_expr(scrutinee, out);
            for arm in arms {
                collect_closures_expr(&arm.body, out);
            }
        }
    }
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
    /// The set of `var` symbols captured by some closure in the module (escape
    /// analysis, M7-WS7b). These are boxed into a `VarCell` at their binding
    /// site, and reads/writes route through the cell so a closure shares the
    /// mutable storage. Empty when there are no captured `var`s.
    escaping_vars: &'a std::collections::HashSet<praxis_hir::SymbolId>,
}

fn lower_fn(
    f: &TypedFn,
    db: &mut TypeDb,
    escaping_vars: &std::collections::HashSet<praxis_hir::SymbolId>,
) -> Function {
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
        escaping_vars,
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

/// Lower a closure literal to its synthetic MIR function (M7, §4.10). The
/// function's MIR params are `[closure_self, user_params...]` (ctx is the
/// implicit hidden first ABI param, as for every Praxis function). At entry, a
/// prologue loads each captured value via `praxis_closure_capture(ctx, self, i)`
/// and binds it to the capture's symbol in `b.locals`; the params are already
/// bound. Then the body is lowered as usual.
///
/// This is Approach B (the closure value is passed as a hidden first arg; the
/// synthetic function loads its captures at entry). The call site reads `fn_ptr`
/// and emits a `call_indirect` with the matching signature.
fn lower_closure_fn(
    closure: &LiftedClosure,
    db: &mut TypeDb,
    escaping_vars: &std::collections::HashSet<praxis_hir::SymbolId>,
) -> Function {
    let int_ty = db.int();
    let bool_ty = db.bool();
    let text_ty = db.text();
    let char_ty = db.char();
    let unit_ty = db.unit();

    let mut func = Function {
        name: closure.fn_name.clone(),
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
        escaping_vars,
    };

    // Param 0 (MIR): the closure value itself (`closure_self`). It is the hidden
    // first explicit arg after the implicit ctx. Bound to a local so the prologue
    // can pass it to `praxis_closure_capture`.
    let self_local = b.alloc_gc(Type(0), Some("__closure_self".to_string()));
    b.func.params.push(self_local);

    // User params: one `Gc` slot each, after `self_local`.
    for p in &closure.params {
        let id = b.alloc_gc(p.ty, Some(p.name.clone()));
        b.locals.insert(p.symbol, id);
        b.func.params.push(id);
    }

    // Prologue: load each captured value from the closure's env and bind it to
    // the capture's symbol. `praxis_closure_capture(ctx, closure, idx) -> GcRef`.
    // The `idx` arg is a raw integer carried in the uniform i64 ABI (like the
    // `Vec()` null-descriptor idiom): we ConstInt it into a scalar slot, then
    // MoveGc-copy that scalar's raw i64 into a Gc-typed slot so it flows through
    // the call as a plain integer, not a boxed Int pointer.
    for (idx, cap) in closure.captures.iter().enumerate() {
        let idx_scalar = b.alloc_scalar(ScalarKind::Int);
        b.push(Inst::ConstInt {
            dst: idx_scalar,
            value: idx as i64,
        });
        let idx_gc = b.alloc_gc(int_ty, None);
        b.push(Inst::MoveGc {
            dst: idx_gc,
            src: idx_scalar,
        });
        let dst = b.alloc_gc(cap.ty, Some(cap.name.clone()));
        b.push(Inst::Call {
            dst,
            callee: CallTarget::Runtime("praxis_closure_capture".to_string()),
            args: vec![self_local, idx_gc],
            live_roots: Vec::new(),
        });
        b.check_fault();
        b.locals.insert(cap.symbol, dst);
    }

    // The return slot.
    let ret = b.alloc_gc(closure.body.ty, None);
    b.func.return_local = ret;

    // Lower the body. Captures and params are bound in `b.locals`.
    let tail = lower_block_body(&mut b, &closure.body);
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
        } => {
            let v = lower_expr_gc(b, init);
            let slot = b.alloc_gc(expr_static_type(init), Some(name.clone()));
            b.push(Inst::MoveGc { dst: slot, src: v });
            b.locals.insert(*symbol, slot);
        }
        TypedStmt::Var {
            symbol, name, init, ..
        } => {
            let v = lower_expr_gc(b, init);
            if b.escaping_vars.contains(symbol) {
                // A captured `var` is boxed into a `VarCell` at its binding site
                // (M7-WS7b, §4.10). The local holds the cell; reads/writes route
                // through `praxis_var_cell_get`/`praxis_var_cell_set` so a
                // closure sharing the cell sees mutations.
                let cell = b.alloc_gc(Type(0), Some(format!("__cell_{name}")));
                b.push(Inst::Call {
                    dst: cell,
                    callee: CallTarget::Runtime("praxis_alloc_var_cell".to_string()),
                    args: vec![v],
                    live_roots: Vec::new(),
                });
                b.check_fault();
                b.locals.insert(*symbol, cell);
            } else {
                let slot = b.alloc_gc(expr_static_type(init), Some(name.clone()));
                b.push(Inst::MoveGc { dst: slot, src: v });
                b.locals.insert(*symbol, slot);
            }
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
            // For an escaping `var`, the slot holds a `VarCell`; a plain Assign
            // stores into the cell via `praxis_var_cell_set`. Compound assigns
            // read the cell first (get), compute, then write back (set).
            let escaping = b.escaping_vars.contains(symbol);
            if *op == AssignOp::Assign {
                let v = lower_expr_gc(b, value);
                if escaping {
                    b.push(Inst::Call {
                        dst,
                        callee: CallTarget::Runtime("praxis_var_cell_set".to_string()),
                        args: vec![dst, v],
                        live_roots: Vec::new(),
                    });
                    b.check_fault();
                } else {
                    b.push(Inst::MoveGc { dst, src: v });
                }
            } else {
                // Compound assignment: dst = dst <op> value (Int arithmetic).
                let cur = if escaping {
                    // Read the cell's current value.
                    let cur = b.alloc_gc(Type(0), None);
                    b.push(Inst::Call {
                        dst: cur,
                        callee: CallTarget::Runtime("praxis_var_cell_get".to_string()),
                        args: vec![dst],
                        live_roots: Vec::new(),
                    });
                    b.check_fault();
                    cur
                } else {
                    let cur = b.alloc_gc(Type(0), None);
                    b.push(Inst::MoveGc { dst: cur, src: dst });
                    cur
                };
                let rhs = lower_expr_gc(b, value);
                let result = lower_int_binop(b, op_to_int_binop(*op), cur, rhs);
                let materialized = lower_materialize(b, result);
                if escaping {
                    b.push(Inst::Call {
                        dst,
                        callee: CallTarget::Runtime("praxis_var_cell_set".to_string()),
                        args: vec![dst, materialized],
                        live_roots: Vec::new(),
                    });
                    b.check_fault();
                } else {
                    b.push(Inst::MoveGc {
                        dst,
                        src: materialized,
                    });
                }
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
        TypedExpr::Path { symbol, ty, .. } => {
            match b.locals.get(symbol).copied() {
                Some(slot) => {
                    // An escaping `var`'s slot holds a `VarCell`; deref it.
                    if b.escaping_vars.contains(symbol) {
                        let value = b.alloc_gc(*ty, None);
                        b.push(Inst::Call {
                            dst: value,
                            callee: CallTarget::Runtime("praxis_var_cell_get".to_string()),
                            args: vec![slot],
                            live_roots: Vec::new(),
                        });
                        b.check_fault();
                        value
                    } else {
                        slot
                    }
                }
                None => {
                    // Unresolved: allocate a Unit placeholder so downstream
                    // lowering is sound.
                    let _ = ty;
                    lower_lit_gc(b, &Lit::Int(0))
                }
            }
        }
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
                    // Decide scalar vs structural comparison by the operand type
                    // (§5.5). `==`/`!=` on a composite GC type (record/tuple/
                    // enum/collection) lower to a structural-equality runtime
                    // call; everything else (Int/Bool/Char/Text, and all
                    // ordering ops `<` `>` `<=` `>=` which are Int-only) uses
                    // the native scalar compare.
                    let operand_ty = expr_static_type(lhs);
                    let composite = matches!(
                        b.db.data(b.db.follow(operand_ty)),
                        praxis_types::data::TypeData::Record { .. }
                            | praxis_types::data::TypeData::Tuple(_)
                            | praxis_types::data::TypeData::Enum { .. }
                            | praxis_types::data::TypeData::Collection { .. }
                    );
                    if composite && matches!(op, BinOp::Eq | BinOp::Neq) {
                        // Structural equality via praxis_struct_eq(ctx, a, b) -> 0/1.
                        // `!=` is `!(==)`.
                        let eq_bool = lower_struct_eq(b, l, r);
                        if *op == BinOp::Neq {
                            lower_logical_not(b, eq_bool)
                        } else {
                            eq_bool
                        }
                    } else {
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
            callee,
            callee_name,
            args,
            ty,
            ..
        } => {
            // Collection construction: `Vec[T]()`, `Deque[T]()`, etc. (M8 WS1,
            // §11.1/§11.2). The element type is extracted from the call's result
            // type (the collection type) and carried through `AllocKind::Collection`
            // so the codegen resolves the real element descriptor (closing the M7
            // null-descriptor carryover). `out`/`panic` and other builtins fall
            // through to the generic call path below.
            if let Some(alloc) = collection_alloc_kind(b, callee_name, *ty) {
                let dst = b.alloc_gc(*ty, None);
                b.push(Inst::Alloc {
                    dst,
                    alloc,
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
            // Indirect call dispatch (M7, §4.10): if the callee resolves to a
            // local binding (a `let`/`var`/`param` holding a closure value), the
            // call is indirect — read the closure's `fn_ptr` and call through it.
            // Top-level `fn`s are never in `b.locals`, so this distinguishes the
            // two soundly.
            if let Some(callee_local) = b.locals.get(callee).copied() {
                let dst = b.alloc_gc(Type(0), None);
                b.push(Inst::CallIndirect {
                    dst,
                    callee: callee_local,
                    args: arg_locals,
                    live_roots: Vec::new(),
                });
                b.check_fault();
                return dst;
            }
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
        TypedExpr::Tuple { elements, ty } => {
            // M7 Part 2: tuples now materialize as real objects. Lower each
            // element to a `Gc` local in positional order, then emit an `Alloc`
            // with `AllocKind::Tuple`. The codegen builds the `TupleSchema`
            // from the tuple's static type (the element-type sequence) and
            // embeds its address as an immediate in the allocation call.
            let element_locals: Vec<LocalId> =
                elements.iter().map(|e| lower_expr_gc(b, e)).collect();
            let dst = b.alloc_gc(Type(0), None);
            b.push(Inst::Alloc {
                dst,
                alloc: AllocKind::Tuple {
                    ty: *ty,
                    elements: element_locals,
                },
                live_roots: Vec::new(),
            });
            dst
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
        // M7-WS7: closure literal — allocate the closure value. Each capture's
        // current value is the captured binding's local; the synthetic function
        // (emitted separately by `lower_module`) is named by `fn_name`.
        TypedExpr::Closure {
            fn_name, captures, ..
        } => {
            let cap_locals: Vec<LocalId> = captures
                .iter()
                .map(|cap| {
                    b.locals
                        .get(&cap.symbol)
                        .copied()
                        .unwrap_or_else(|| lower_lit_gc(b, &Lit::Int(0)))
                })
                .collect();
            let dst = b.alloc_gc(Type(0), None);
            b.push(Inst::Alloc {
                dst,
                alloc: AllocKind::Closure {
                    fn_name: fn_name.clone(),
                    captures: cap_locals,
                },
                live_roots: Vec::new(),
            });
            b.check_fault();
            dst
        }
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

/// Lower a structural-equality comparison `lhs == rhs` of two composite GC
/// values (records/tuples/enums/collections), returning the materialized `Bool`
/// `GcRef` (§5.5). Emits an `Inst::StructEq` that lowers to the
/// `praxis_struct_eq` runtime call (which dispatches through the descriptor).
/// Both operands are already-lowered `Gc` locals.
fn lower_struct_eq(b: &mut Builder<'_>, lhs: LocalId, rhs: LocalId) -> LocalId {
    let bool_scalar = b.alloc_scalar(ScalarKind::Bool);
    b.push(Inst::StructEq {
        dst: bool_scalar,
        lhs,
        rhs,
        live_roots: Vec::new(),
    });
    lower_materialize_bool(b, bool_scalar)
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
        | TypedExpr::Match { ty, .. }
        | TypedExpr::Closure { ty, .. } => *ty,
        TypedExpr::Block(blk) => blk.ty,
    }
}

/// Build an [`AllocKind::Collection`] for a `Name[T]()` construction call, if
/// `callee_name` is a known collection constructor. Returns `None` for
/// non-collection callees (the common case) so the generic call path handles
/// them. The element/key types are extracted from the call's result type
/// (`result_ty`), which inference has already pinned to e.g. `Vec[Int]` or
/// `Map[Text, Int]`.
///
/// `Seq` is intentionally rejected — it is compiler-internal and never
/// constructed from source (§6.3, M8 WS8).
fn collection_alloc_kind(b: &Builder<'_>, callee_name: &str, result_ty: Type) -> Option<AllocKind> {
    use praxis_types::data::TypeData;
    use praxis_types::CollectionCtor;
    let ctor = match callee_name {
        "Vec" => CollectionCtor::Vec,
        "Deque" => CollectionCtor::Deque,
        "Map" => CollectionCtor::Map,
        "Set" => CollectionCtor::Set,
        "Counter" => CollectionCtor::Counter,
        "MinHeap" => CollectionCtor::MinHeap,
        "MaxHeap" => CollectionCtor::MaxHeap,
        "BitSet" => CollectionCtor::BitSet,
        "Grid" => CollectionCtor::Grid,
        "Range" => CollectionCtor::Range,
        // Not a collection constructor; fall through to the generic call path.
        _ => return None,
    };
    // Extract the type arguments from the result type. For a nullary collection
    // (BitSet/Range) there are none; for Vec/Deque/Set/Heap/Grid/Counter there is
    // one; for Map there are two. If the result type does not match the ctor's
    // shape (a malformed call), fall back to an empty arg list — the codegen will
    // resolve descriptors defensively (defaulting to INT), same as before.
    let args: Vec<Type> = match b.db.data(b.db.follow(result_ty)) {
        TypeData::Collection {
            ctor: c,
            args: ref a,
        } if *c == ctor => a.clone(),
        _ => Vec::new(),
    };
    Some(AllocKind::Collection { ctor, args })
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

/// Lower a `match scrutinee { arms }` expression (M7, §4.6) to a decision tree
/// of tests. Handles the full recursive pattern grammar (§4.6): wildcard, literal,
/// variable bind, and enum variant with nested sub-patterns.
///
/// For each arm in order, emit the pattern's tests against the scrutinee local;
/// on success, bind pattern variables and lower the body; on failure, fall
/// through to the next arm. The final fall-through (non-exhaustive match) stores
/// a Unit default — the exhaustiveness checker (Y120) rejects this case at
/// compile time, so this default is defensive only.
fn lower_match(
    b: &mut Builder<'_>,
    scrutinee: &TypedExpr,
    arms: &[praxis_hir::TypedMatchArm],
) -> LocalId {
    let scrut_gc = lower_expr_gc(b, scrutinee);
    let result = b.alloc_gc(Type(0), None);
    let join = b.func.new_block();

    for arm in arms {
        let on_success = b.func.new_block();
        let on_fail = b.func.new_block();
        // Test this arm's pattern against the scrutinee; branch to on_success /
        // on_fail. Variable bindings are installed on the success path.
        emit_pattern_test(b, scrut_gc, &arm.pattern, on_success, on_fail);
        // Success: lower the body, store result, jump to join.
        b.cur = on_success;
        let body_val = lower_expr_gc(b, &arm.body);
        b.push(Inst::MoveGc {
            dst: result,
            src: body_val,
        });
        b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: join };
        // Continue testing the next arm from the failure block.
        b.cur = on_fail;
    }
    // Defensive fall-through (the exhaustiveness checker rejects non-exhaustive
    // matches at compile time; this is unreachable in well-typed code).
    let unit_val = lower_lit_gc(b, &Lit::Int(0));
    b.push(Inst::MoveGc {
        dst: result,
        src: unit_val,
    });
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: join };
    b.cur = join;
    result
}

/// Emit the test for one pattern against a scrutinee `Gc` local. On success,
/// jumps to `on_success` (having installed any variable bindings into
/// `b.locals`); on failure, jumps to `on_fail`. Both blocks are left unfinished
/// — the caller fills `on_success` with the arm body and continues from
/// `on_fail` with the next arm.
fn emit_pattern_test(
    b: &mut Builder<'_>,
    scrut: LocalId,
    pat: &praxis_hir::TypedPattern,
    on_success: BlockId,
    on_fail: BlockId,
) {
    use praxis_hir::TypedPattern;
    match pat {
        TypedPattern::Wildcard => {
            // Always matches.
            b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: on_success };
            b.cur = on_fail;
        }
        TypedPattern::Bind { symbol, .. } => {
            // Always matches; bind the scrutinee value to the symbol.
            b.locals.insert(*symbol, scrut);
            b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: on_success };
            b.cur = on_fail;
        }
        TypedPattern::Lit { value, .. } => {
            // Compare the scrutinee against the literal value. Int/Bool use a
            // native scalar compare; Text uses structural equality.
            let lit_gc = lower_lit_gc(b, value);
            match value {
                Lit::Int(_) | Lit::Bool(_) => {
                    let si = lower_extract_int(b, scrut);
                    let li = lower_extract_int(b, lit_gc);
                    let cmp = b.alloc_scalar(ScalarKind::Bool);
                    b.push(Inst::IntCmp {
                        op: CmpOp::Eq,
                        dst: cmp,
                        lhs: si,
                        rhs: li,
                    });
                    b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                        cond: cmp,
                        then_block: on_success,
                        else_block: on_fail,
                    };
                }
                Lit::Text(_) => {
                    // Structural equality via praxis_struct_eq.
                    let eq_bool = lower_struct_eq(b, scrut, lit_gc);
                    let eq_scalar = b.alloc_scalar(ScalarKind::Bool);
                    b.push(Inst::ExtractScalar {
                        dst: eq_scalar,
                        src: eq_bool,
                        scalar: ScalarKind::Bool,
                    });
                    b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                        cond: eq_scalar,
                        then_block: on_success,
                        else_block: on_fail,
                    };
                }
                Lit::Char(_) => {
                    // Char patterns aren't produced by the parser today; treat
                    // as a match (defensive).
                    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: on_success };
                }
            }
            b.cur = on_fail;
        }
        TypedPattern::EnumVariant {
            variant_idx,
            subpatterns,
            ..
        } => {
            // Read the scrutinee's tag and compare against the variant index.
            let tag = b.alloc_scalar(ScalarKind::Int);
            b.push(Inst::EnumTag {
                dst: tag,
                src: scrut,
            });
            let expected = b.alloc_scalar(ScalarKind::Int);
            b.push(Inst::ConstInt {
                dst: expected,
                value: *variant_idx as i64,
            });
            let tag_cmp = b.alloc_scalar(ScalarKind::Bool);
            b.push(Inst::IntCmp {
                op: CmpOp::Eq,
                dst: tag_cmp,
                lhs: tag,
                rhs: expected,
            });
            // If the tag matches, test sub-patterns; otherwise fail.
            let sub_ok = b.func.new_block();
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: tag_cmp,
                then_block: sub_ok,
                else_block: on_fail,
            };
            b.cur = sub_ok;
            // Test each sub-pattern against its payload slot. Chain them: all
            // must succeed. Extract each payload slot and recurse.
            emit_subpattern_tests(b, scrut, subpatterns, 0, on_success, on_fail);
        }
    }
}

/// Recursively test a chain of sub-patterns against consecutive payload slots
/// of `scrut` (an enum value), starting at `slot_idx`. All must succeed to
/// reach `on_success`; any failure jumps to `on_fail`.
fn emit_subpattern_tests(
    b: &mut Builder<'_>,
    scrut: LocalId,
    subpatterns: &[praxis_hir::TypedPattern],
    slot_idx: u32,
    on_success: BlockId,
    on_fail: BlockId,
) {
    if let Some(sub) = subpatterns.get(slot_idx as usize) {
        // Extract this payload slot into a local.
        let payload = b.alloc_gc(Type(0), None);
        b.push(Inst::EnumPayloadGet {
            dst: payload,
            src: scrut,
            idx: slot_idx,
        });
        // Test `sub` against `payload`. If it matches, continue to the next
        // sub-pattern; if not, fail.
        let next = b.func.new_block();
        emit_pattern_test(b, payload, sub, next, on_fail);
        b.cur = next;
        emit_subpattern_tests(b, scrut, subpatterns, slot_idx + 1, on_success, on_fail);
    } else {
        // All sub-patterns matched: success.
        b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: on_success };
        b.cur = on_fail;
    }
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
