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
    AllocKind, BlockId, CallTarget, CmpOp, FloatBinOp, Function, Inst, IntBinOp, LocalDebugKind,
    LocalId, LocalKind, ScalarKind, Terminator,
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
        TypedExpr::For { iter, body, .. } => {
            collect_closures_expr(iter, out);
            collect_closures_block(body, out);
        }
        TypedExpr::Loop { body, .. } => collect_closures_block(body, out),
        TypedExpr::Break { value, .. } => {
            if let Some(v) = value {
                collect_closures_expr(v, out);
            }
        }
        TypedExpr::Continue { .. } => {}
        TypedExpr::Return { value, .. } => {
            if let Some(v) = value {
                collect_closures_expr(v, out);
            }
        }
        TypedExpr::Call {
            args, callee_expr, ..
        } => {
            for a in args {
                collect_closures_expr(a, out);
            }
            if let Some(ce) = callee_expr {
                collect_closures_expr(ce, out);
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
    float_ty: Type,
    bool_ty: Type,
    text_ty: Type,
    char_ty: Type,
    unit_ty: Type,
    /// The set of `var` symbols captured by some closure in the module (escape
    /// analysis, M7-WS7b). These are boxed into a `VarCell` at their binding
    /// site, and reads/writes route through the cell so a closure shares the
    /// mutable storage. Empty when there are no captured `var`s.
    escaping_vars: &'a std::collections::HashSet<praxis_hir::SymbolId>,
    /// The stack of enclosing loops (M8-WS6, §4.11). `break` jumps to the top's
    /// `break_target`; `continue` jumps to the `continue_target` (the header for
    /// `while`/`for`, the body top for `loop`). Empty at the function's top level.
    loop_stack: Vec<LoopCtx>,
}

/// One frame of the loop-context stack (M8-WS6). Pushed on entry to a
/// `while`/`for`/`loop`, popped on exit. `break`/`continue` read the top frame.
#[derive(Clone, Copy)]
struct LoopCtx {
    /// The block `continue` jumps to (the loop header for `while`/`for`).
    continue_target: BlockId,
    /// The block `break` jumps to (the loop exit).
    break_target: BlockId,
}

fn lower_fn(
    f: &TypedFn,
    db: &mut TypeDb,
    escaping_vars: &std::collections::HashSet<praxis_hir::SymbolId>,
) -> Function {
    // Cache scalar handles once.
    let int_ty = db.int();
    let float_ty = db.float();
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
        debug_kinds: Vec::new(),
        debug_spans: Vec::new(),
        span: f.span,
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
        float_ty,
        bool_ty,
        text_ty,
        char_ty,
        unit_ty,
        escaping_vars,
        loop_stack: Vec::new(),
    };

    // Parameters: one `Gc` slot each. User locals: classified + span-less (a
    // param has no single materializing expression; its span is the fn's span).
    for p in &f.params {
        let id = b.alloc_gc(
            p.ty,
            Some(p.name.clone()),
            LocalDebugKind::User,
            Some(f.span),
        );
        b.locals.insert(p.symbol, id);
        b.func.params.push(id);
    }

    // The return slot. A compiler temp; span-less.
    let ret = b.alloc_gc(f.return_type, None, LocalDebugKind::Temp, None);
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
    let float_ty = db.float();
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
        debug_kinds: Vec::new(),
        debug_spans: Vec::new(),
        // Closures are lifted to synthetic functions; the `__p_expr` debugger
        // function is also span-less. The `source` command degrades to "no
        // span recorded" for these, which is acceptable (the faulting frame is
        // almost always a real source function).
        span: (0, 0),
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
        float_ty,
        bool_ty,
        text_ty,
        char_ty,
        unit_ty,
        escaping_vars,
        loop_stack: Vec::new(),
    };

    // Param 0 (MIR): the closure value itself (`closure_self`). It is the hidden
    // first explicit arg after the implicit ctx. Bound to a local so the prologue
    // can pass it to `praxis_closure_capture`. This is an internal ABI slot, not a
    // user-written binding, so it is classified as a temp (it would otherwise
    // surface as a confusing `__closure_self: T` user local in the debugger).
    let self_local = b.alloc_gc(
        Type(0),
        Some("__closure_self".to_string()),
        LocalDebugKind::Temp,
        None,
    );
    b.func.params.push(self_local);

    // User params: one `Gc` slot each, after `self_local`.
    for p in &closure.params {
        let id = b.alloc_gc(p.ty, Some(p.name.clone()), LocalDebugKind::User, None);
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
        let idx_gc = b.alloc_gc(int_ty, None, LocalDebugKind::Temp, None);
        b.push(Inst::MoveGc {
            dst: idx_gc,
            src: idx_scalar,
        });
        let dst = b.alloc_gc(cap.ty, Some(cap.name.clone()), LocalDebugKind::User, None);
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
    let ret = b.alloc_gc(closure.body.ty, None, LocalDebugKind::Temp, None);
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
    /// Allocate a `Gc` local. `debug_name` is the source name (for user
    /// bindings/params/captures); `debug_kind` classifies it for the debugger;
    /// `debug_span` is the materializing expression's span (for the debugger's
    /// `@ "expr"` provenance).
    fn alloc_gc(
        &mut self,
        ty: Type,
        debug_name: Option<String>,
        debug_kind: LocalDebugKind,
        debug_span: Option<(u32, u32)>,
    ) -> LocalId {
        self.func
            .new_local(LocalKind::Gc, ty, debug_name, debug_kind, debug_span)
    }

    /// Allocate a `Gc` local for a compiler temporary materializing `expr`'s
    /// span. Convenience for the many lowering sites that hold a `&TypedExpr`.
    fn alloc_temp(&mut self, ty: Type, expr: &TypedExpr) -> LocalId {
        self.alloc_gc(
            ty,
            None,
            LocalDebugKind::Temp,
            Some(praxis_hir::expr_span(expr)),
        )
    }

    fn alloc_scalar(&mut self, sk: ScalarKind) -> LocalId {
        // Scalar slots carry a placeholder Type; their ScalarKind is authoritative.
        // Scalar locals are never displayed by the debugger (the backend only emits
        // `Gc` locals' metadata), so they get the default temp classification and
        // no span.
        self.func.new_local(
            LocalKind::Scalar(sk),
            Type(0),
            None,
            LocalDebugKind::Temp,
            None,
        )
    }

    fn push(&mut self, inst: Inst) {
        self.func.blocks[self.cur.0 as usize].insts.push(inst);
    }

    /// Emit a fault check after a faultable instruction.
    fn check_fault(&mut self) {
        // `live_roots` is filled by the liveness pass (it is a debugger
        // safepoint: the backend spills these into the debug frame so a
        // snapshot on the fault path sees current values).
        self.push(Inst::CheckFault {
            on_fault: self.fault_block,
            live_roots: Vec::new(),
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
            symbol,
            name,
            init,
            span,
            ..
        } => {
            let v = lower_expr_gc(b, init);
            let slot = b.alloc_gc(
                expr_static_type(init),
                Some(name.clone()),
                LocalDebugKind::User,
                Some(*span),
            );
            b.push(Inst::MoveGc { dst: slot, src: v });
            b.locals.insert(*symbol, slot);
        }
        TypedStmt::Var {
            symbol,
            name,
            init,
            span,
            ..
        } => {
            let v = lower_expr_gc(b, init);
            if b.escaping_vars.contains(symbol) {
                // A captured `var` is boxed into a `VarCell` at its binding site
                // (M7-WS7b, §4.10). The local holds the cell; reads/writes route
                // through `praxis_var_cell_get`/`praxis_var_cell_set` so a
                // closure sharing the cell sees mutations.
                let cell = b.alloc_gc(
                    Type(0),
                    Some(format!("__cell_{name}")),
                    LocalDebugKind::User,
                    Some(*span),
                );
                b.push(Inst::Call {
                    dst: cell,
                    callee: CallTarget::Runtime("praxis_alloc_var_cell".to_string()),
                    args: vec![v],
                    live_roots: Vec::new(),
                });
                b.check_fault();
                b.locals.insert(*symbol, cell);
            } else {
                let slot = b.alloc_gc(
                    expr_static_type(init),
                    Some(name.clone()),
                    LocalDebugKind::User,
                    Some(*span),
                );
                b.push(Inst::MoveGc { dst: slot, src: v });
                b.locals.insert(*symbol, slot);
            }
        }
        TypedStmt::Assign {
            symbol,
            name: _,
            op,
            value,
            span,
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
                    let cur = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, Some(*span));
                    b.push(Inst::Call {
                        dst: cur,
                        callee: CallTarget::Runtime("praxis_var_cell_get".to_string()),
                        args: vec![dst],
                        live_roots: Vec::new(),
                    });
                    b.check_fault();
                    cur
                } else {
                    let cur = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, Some(*span));
                    b.push(Inst::MoveGc { dst: cur, src: dst });
                    cur
                };
                let rhs = lower_expr_gc(b, value);
                let result = lower_int_binop(b, op_to_int_binop(*op), cur, rhs);
                let materialized = lower_materialize(b, result, Some(*span));
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
    // The current expression's span — threaded into every result temp so the
    // debugger can show what each temp holds (`@ "0"`, `@ "x / 0"`, …).
    let espan = Some(praxis_hir::expr_span(e));
    match e {
        TypedExpr::Lit { value, .. } => lower_lit_gc(b, value, espan),
        TypedExpr::Path { symbol, ty, .. } => {
            match b.locals.get(symbol).copied() {
                Some(slot) => {
                    // An escaping `var`'s slot holds a `VarCell`; deref it.
                    if b.escaping_vars.contains(symbol) {
                        let value = b.alloc_temp(*ty, e);
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
                    lower_lit_gc(b, &Lit::Unit, espan)
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
                // Arithmetic: extract scalars, do the op, materialize. Int ops
                // are checked (fault on overflow/div-by-zero); Float ops are
                // unchecked (IEEE-754 inf/nan), so no fault check follows.
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                    let operand_ty = expr_static_type(lhs);
                    let is_float = matches!(
                        b.db.data(b.db.follow(operand_ty)),
                        praxis_types::data::TypeData::Scalar(praxis_types::ScalarType::Float)
                    );
                    if is_float {
                        // `%` is a type error for floats in inference; if it
                        // reaches here (e.g. from a malformed subtree), treat as
                        // Add defensively. binop_to_float maps Add/Sub/Mul/Div.
                        let fop = binop_to_float(*op);
                        let result = lower_float_binop(b, fop, l, r);
                        lower_materialize_float(b, result, espan)
                    } else {
                        let result = lower_int_binop(b, binop_to_int(*op), l, r);
                        lower_materialize(b, result, espan)
                    }
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
                        let operand_ty = expr_static_type(lhs);
                        let is_float = matches!(
                            b.db.data(b.db.follow(operand_ty)),
                            praxis_types::data::TypeData::Scalar(praxis_types::ScalarType::Float)
                        );
                        let bool_scalar = b.alloc_scalar(ScalarKind::Bool);
                        if is_float {
                            // IEEE-754 comparison via FloatCmp (NaN-aware).
                            let lf = lower_extract_float(b, l);
                            let rf = lower_extract_float(b, r);
                            b.push(Inst::FloatCmp {
                                op: binop_to_cmp(*op),
                                dst: bool_scalar,
                                lhs: lf,
                                rhs: rf,
                            });
                        } else {
                            let li = lower_extract_int(b, l);
                            let ri = lower_extract_int(b, r);
                            b.push(Inst::IntCmp {
                                op: binop_to_cmp(*op),
                                dst: bool_scalar,
                                lhs: li,
                                rhs: ri,
                            });
                        }
                        lower_materialize_bool(b, bool_scalar, espan)
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
                    // `0 - operand`. For a Float operand, this is `0.0 - x` as
                    // unchecked float subtraction (no fault); for Int it is the
                    // checked subtraction that faults on `Int::MIN` overflow.
                    let is_float = matches!(
                        b.db.data(b.db.follow(expr_static_type(operand))),
                        praxis_types::data::TypeData::Scalar(praxis_types::ScalarType::Float)
                    );
                    if is_float {
                        let zero = lower_lit_gc(b, &Lit::Float(0.0), espan);
                        let result = lower_float_binop(b, FloatBinOp::Sub, zero, o);
                        lower_materialize_float(b, result, espan)
                    } else {
                        let zero = lower_lit_gc(b, &Lit::Int(0), espan);
                        let result = lower_int_binop(b, IntBinOp::Sub, zero, o);
                        lower_materialize(b, result, espan)
                    }
                }
                UnaryOp::Not => {
                    // Logical not on Bool: `!x` is `x == false`.
                    lower_logical_not(b, o)
                }
            }
        }
        TypedExpr::Paren { inner, .. } => match inner {
            Some(e) => lower_expr_gc(b, e),
            None => lower_lit_gc(b, &Lit::Unit, espan),
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
            lower_lit_gc(b, &Lit::Unit, espan) // while yields Unit
        }
        TypedExpr::For {
            binding,
            iter,
            body,
            ..
        } => {
            lower_for(b, *binding, iter, body);
            lower_lit_gc(b, &Lit::Unit, espan) // for yields Unit
        }
        TypedExpr::Loop { body, .. } => {
            lower_loop(b, body);
            lower_lit_gc(b, &Lit::Unit, espan) // loop yields Unit (break-value is a refinement)
        }
        TypedExpr::Break { value, .. } => {
            lower_break(b, value);
            lower_lit_gc(b, &Lit::Unit, espan) // unreachable in a well-typed program
        }
        TypedExpr::Continue { .. } => {
            lower_continue(b);
            lower_lit_gc(b, &Lit::Unit, espan)
        }
        TypedExpr::Return { value, .. } => {
            lower_return(b, value);
            lower_lit_gc(b, &Lit::Unit, espan)
        }
        TypedExpr::Call {
            callee,
            callee_name,
            args,
            callee_expr,
            ty,
            ..
        } => {
            // Postfix call on an arbitrary expression (`expr(args)`, M8 §4.10):
            // the callee is a closure value produced by an expression (e.g.
            // `fs.get(0)` in `fs.get(0)(100)`). Lower the callee expression to a
            // GcRef local and emit an indirect call through its `fn_ptr`. This
            // bypasses the named-call paths below (collection construction,
            // `out`, named/indirect-via-local) — those only apply when the callee
            // is a name. Pre-fix this case fell through to `CallTarget::User("")`
            // (a nonsense direct call that SIGSEGV'd); now it lowers soundly.
            if let Some(ce) = callee_expr {
                let callee_local = lower_expr_gc(b, ce);
                let arg_locals: Vec<LocalId> = args.iter().map(|a| lower_expr_gc(b, a)).collect();
                let dst = b.alloc_gc(*ty, None, LocalDebugKind::Temp, None);
                b.push(Inst::CallIndirect {
                    dst,
                    callee: callee_local,
                    args: arg_locals,
                    live_roots: Vec::new(),
                });
                b.check_fault();
                return dst;
            }
            // Collection construction: `Vec[T]()`, `Deque[T]()`, etc. (M8 WS1,
            // §11.1/§11.2). The element type is extracted from the call's result
            // type (the collection type) and carried through `AllocKind::Collection`
            // so the codegen resolves the real element descriptor (closing the M7
            // null-descriptor carryover). `out`/`panic` and other builtins fall
            // through to the generic call path below.
            if let Some(alloc) = collection_alloc_kind(b, callee_name, *ty) {
                let dst = b.alloc_gc(*ty, None, LocalDebugKind::Temp, None);
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
                    .unwrap_or_else(|| lower_lit_gc(b, &Lit::Unit, espan));
                // The call's result temp materializes `e` (the whole call expr).
                let dst = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, espan);
                b.push(Inst::Call {
                    dst,
                    callee: CallTarget::Runtime("praxis_write_stdout".to_string()),
                    args: vec![arg_local],
                    live_roots: Vec::new(),
                });
                // out does not fault; no check_fault needed.
                return dst;
            }
            // Float constants `pi()`/`e()` (§4.12): direct runtime calls that
            // allocate a Float. No arguments; no fault.
            if callee_name == "pi" || callee_name == "e" {
                let sym = if callee_name == "pi" {
                    "praxis_float_pi"
                } else {
                    "praxis_float_e"
                };
                let dst = b.alloc_gc(*ty, None, LocalDebugKind::Temp, None);
                b.push(Inst::Call {
                    dst,
                    callee: CallTarget::Runtime(sym.to_string()),
                    args: vec![],
                    live_roots: Vec::new(),
                });
                return dst;
            }
            let arg_locals: Vec<LocalId> = args.iter().map(|a| lower_expr_gc(b, a)).collect();
            // Indirect call dispatch (M7, §4.10): if the callee resolves to a
            // local binding (a `let`/`var`/`param` holding a closure value), the
            // call is indirect — read the closure's `fn_ptr` and call through it.
            // Top-level `fn`s are never in `b.locals`, so this distinguishes the
            // two soundly.
            if let Some(callee_local) = b.locals.get(callee).copied() {
                let dst = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
                b.push(Inst::CallIndirect {
                    dst,
                    callee: callee_local,
                    args: arg_locals,
                    live_roots: Vec::new(),
                });
                b.check_fault();
                return dst;
            }
            // The call's result temp materializes `e` (the whole call expr).
            let dst = b.alloc_gc(
                Type(0),
                None,
                LocalDebugKind::Temp,
                Some(praxis_hir::expr_span(e)),
            );
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
            name,
            lowering_symbol,
            args,
            purity,
            ty,
            ..
        } => {
            // A method call lowers to a runtime-wrapper call. The receiver is
            // the first argument; the method's explicit args follow. The
            // catalog resolved `lowering_symbol` (e.g. `praxis_vec_push`); if
            // empty (an intrinsic), dispatch to the pipeline lowering. M8-WS11
            // first tries to recognize a *chain* and fuse it into one loop; if
            // that declines, fall back to the per-combinator eager lowerer
            // (M8-WS8) which handles single combinators and is the safe default.
            if lowering_symbol.is_empty() {
                // Reconstruct the MethodCall node so the recognizer can walk
                // the receiver chain.
                let call = TypedExpr::MethodCall {
                    receiver: receiver.clone(),
                    name: name.clone(),
                    lowering_symbol: lowering_symbol.clone(),
                    args: args.clone(),
                    purity: *purity,
                    ty: *ty,
                    span: praxis_hir::expr_span(e),
                };
                if let Some(plan) = recognize_pipeline(&call) {
                    return lower_pipeline(b, plan);
                }
                return lower_pipeline_combinator(b, receiver, name, args, *ty);
            }
            let mut arg_locals: Vec<LocalId> = Vec::with_capacity(args.len() + 1);
            arg_locals.push(lower_expr_gc(b, receiver));
            for a in args {
                arg_locals.push(lower_expr_gc(b, a));
            }
            // The call's result temp materializes `e` (the whole method-call
            // expression) — thread its span so the debugger can show
            // `@ "xs.get(99)"`.
            let dst = b.alloc_gc(
                Type(0),
                None,
                LocalDebugKind::Temp,
                Some(praxis_hir::expr_span(e)),
            );
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
        TypedExpr::Tuple { elements, ty, .. } => {
            // M7 Part 2: tuples now materialize as real objects. Lower each
            // element to a `Gc` local in positional order, then emit an `Alloc`
            // with `AllocKind::Tuple`. The codegen builds the `TupleSchema`
            // from the tuple's static type (the element-type sequence) and
            // embeds its address as an immediate in the allocation call.
            let element_locals: Vec<LocalId> =
                elements.iter().map(|el| lower_expr_gc(b, el)).collect();
            let dst = b.alloc_gc(
                Type(0),
                None,
                LocalDebugKind::Temp,
                Some(praxis_hir::expr_span(e)),
            );
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
                        .unwrap_or_else(|| lower_lit_gc(b, &Lit::Unit, espan))
                })
                .collect();
            // The closure value temp materializes `e` (the whole closure expr).
            let dst = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, espan);
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
    let input = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
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
    let idx_gc = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
    b.push(Inst::Alloc {
        dst: idx_gc,
        alloc: AllocKind::Int { value: idx_scalar },
        live_roots: Vec::new(),
    });
    // Call praxis_run_parser(ctx, idx, input) -> result.
    let dst = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
    b.push(Inst::Call {
        dst,
        callee: CallTarget::Runtime("praxis_run_parser".to_string()),
        args: vec![idx_gc, input],
        live_roots: Vec::new(),
    });
    b.check_fault();
    dst
}

/// Lower a literal to a `GcRef` local (allocating the object). `span` is the
/// materializing expression's span, threaded so the debugger can show what each
/// temp holds (`@ "0"`, `@ "x / 0"`, …); `None` for span-less synthetic lits.
fn lower_lit_gc(b: &mut Builder<'_>, value: &Lit, span: Option<(u32, u32)>) -> LocalId {
    match value {
        Lit::Int(n) => {
            let scalar = b.alloc_scalar(ScalarKind::Int);
            b.push(Inst::ConstInt {
                dst: scalar,
                value: *n,
            });
            let dst = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, span);
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
            let dst = b.alloc_gc(b.bool_ty, None, LocalDebugKind::Temp, span);
            b.push(Inst::Alloc {
                dst,
                alloc: AllocKind::Bool { value: scalar },
                live_roots: Vec::new(),
            });
            dst
        }
        Lit::Text(s) => {
            let dst = b.alloc_gc(b.text_ty, None, LocalDebugKind::Temp, span);
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
            let dst = b.alloc_gc(b.char_ty, None, LocalDebugKind::Temp, span);
            b.push(Inst::Alloc {
                dst,
                alloc: AllocKind::Char { value: scalar },
                live_roots: Vec::new(),
            });
            dst
        }
        Lit::Float(f) => {
            // Float's payload is an f64; ConstFloat carries it as f64::to_bits()
            // (an i64) so it rides the uniform scalar channel (§4.12).
            let scalar = b.alloc_scalar(ScalarKind::Float);
            b.push(Inst::ConstFloat {
                dst: scalar,
                bits: f.to_bits() as i64,
            });
            let dst = b.alloc_gc(b.float_ty, None, LocalDebugKind::Temp, span);
            b.push(Inst::Alloc {
                dst,
                alloc: AllocKind::Float { value: scalar },
                live_roots: Vec::new(),
            });
            dst
        }
        Lit::Unit => {
            // The Unit value (§4.3): allocate the immortal Unit singleton.
            let dst = b.alloc_gc(b.unit_ty, None, LocalDebugKind::Temp, span);
            b.push(Inst::Alloc {
                dst,
                alloc: AllocKind::Unit,
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

/// Extract a `Float` payload into a scalar local (carried as f64 bits, §4.12).
fn lower_extract_float(b: &mut Builder<'_>, src: LocalId) -> LocalId {
    let dst = b.alloc_scalar(ScalarKind::Float);
    b.push(Inst::ExtractScalar {
        dst,
        src,
        scalar: ScalarKind::Float,
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

/// Materialize an `Int` scalar into a fresh `GcRef`. `span` is the
/// materializing expression's span for debugger provenance.
fn lower_materialize(b: &mut Builder<'_>, scalar: LocalId, span: Option<(u32, u32)>) -> LocalId {
    let dst = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, span);
    b.push(Inst::Materialize {
        dst,
        src: scalar,
        scalar: ScalarKind::Int,
        live_roots: Vec::new(),
    });
    dst
}

/// Materialize a `Bool` scalar into a fresh `GcRef`. `span` is the
/// materializing expression's span for debugger provenance.
fn lower_materialize_bool(
    b: &mut Builder<'_>,
    scalar: LocalId,
    span: Option<(u32, u32)>,
) -> LocalId {
    let dst = b.alloc_gc(b.bool_ty, None, LocalDebugKind::Temp, span);
    b.push(Inst::Materialize {
        dst,
        src: scalar,
        scalar: ScalarKind::Bool,
        live_roots: Vec::new(),
    });
    dst
}

/// Lower an unchecked Float binary op on two `GcRef` operands, returning the
/// scalar (bit-pattern) result. No fault check — IEEE-754 arithmetic produces
/// inf/NaN rather than faulting (§4.12).
fn lower_float_binop(
    b: &mut Builder<'_>,
    op: FloatBinOp,
    lhs_gc: LocalId,
    rhs_gc: LocalId,
) -> LocalId {
    let lhs = lower_extract_float(b, lhs_gc);
    let rhs = lower_extract_float(b, rhs_gc);
    let dst = b.alloc_scalar(ScalarKind::Float);
    b.push(Inst::FloatBinOp { op, dst, lhs, rhs });
    dst
}

/// Materialize a `Float` scalar (bit-pattern) into a fresh `GcRef`. `span` is
/// the materializing expression's span for debugger provenance.
fn lower_materialize_float(
    b: &mut Builder<'_>,
    scalar: LocalId,
    span: Option<(u32, u32)>,
) -> LocalId {
    let dst = b.alloc_gc(b.float_ty, None, LocalDebugKind::Temp, span);
    b.push(Inst::Materialize {
        dst,
        src: scalar,
        scalar: ScalarKind::Float,
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
    lower_materialize_bool(b, bool_scalar, None)
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
    let result = b.alloc_gc(b.bool_ty, None, LocalDebugKind::Temp, None);
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
    let true_val = lower_lit_gc(b, &Lit::Bool(true), None);
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
    lower_materialize_bool(b, negated, None)
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

    let result = b.alloc_gc(then_block.ty, None, LocalDebugKind::Temp, None);
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
        None => lower_lit_gc(b, &Lit::Unit, None), // no else → Unit
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

    // Push the loop context so `break`/`continue` inside the body resolve.
    b.loop_stack.push(LoopCtx {
        continue_target: header,
        break_target: exit,
    });
    b.cur = body_blk;
    let _ = lower_block_body(b, body);
    b.loop_stack.pop();
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };

    b.cur = exit;
}

/// `for binding in iter { body }` (M8-WS6, §4.11). Lowers to an index loop over
/// the source: a header tests `i < len`, the body binds `iter.get(i)` to the
/// loop variable, runs, increments `i`, and jumps back. Generalizes across
/// Vec/Deque via the element-indexed `get` runtime symbol. (Map/Set/Grid
/// iteration via `for` is a follow-up; the common Vec/Deque case is wired here.)
fn lower_for(
    b: &mut Builder<'_>,
    binding: praxis_hir::SymbolId,
    iter: &TypedExpr,
    body: &praxis_hir::TypedBlock,
) {
    // Lower the iterator once; it lives in a Gc slot for the loop's duration.
    let iter_local = lower_expr_gc(b, iter);
    // The index lives in a Gc Int slot (not a scalar) so it persists across the
    // loop's block boundaries like other Gc values. Start at 0.
    let idx_gc = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
    let zero_scalar = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ConstInt {
        dst: zero_scalar,
        value: 0,
    });
    b.push(Inst::Materialize {
        dst: idx_gc,
        src: zero_scalar,
        scalar: ScalarKind::Int,
        live_roots: vec![iter_local],
    });

    let header = b.func.new_block();
    let body_blk = b.func.new_block();
    // The index increment gets its own block, and it — not the header — is
    // `continue`'s target. Jumping to the header from `continue` would skip the
    // increment and loop forever.
    let incr = b.func.new_block();
    let exit = b.func.new_block();

    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };
    b.cur = header;

    // `len = iter.len()`.
    let len_sym = len_symbol_for(b.db, iter);
    let len_dst = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
    b.push(Inst::Call {
        dst: len_dst,
        callee: CallTarget::Runtime(len_sym.to_string()),
        args: vec![iter_local],
        live_roots: vec![iter_local, idx_gc],
    });
    b.check_fault();
    let len_scalar = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ExtractScalar {
        dst: len_scalar,
        src: len_dst,
        scalar: ScalarKind::Int,
    });
    // Extract the index scalar for the comparison.
    let idx_scalar = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ExtractScalar {
        dst: idx_scalar,
        src: idx_gc,
        scalar: ScalarKind::Int,
    });
    // `i < len`
    let cond = b.alloc_scalar(ScalarKind::Bool);
    b.push(Inst::IntCmp {
        dst: cond,
        op: CmpOp::Lt,
        lhs: idx_scalar,
        rhs: len_scalar,
    });
    b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
        cond,
        then_block: body_blk,
        else_block: exit,
    };

    b.loop_stack.push(LoopCtx {
        continue_target: incr,
        break_target: exit,
    });
    b.cur = body_blk;
    // Bind the loop variable: `binding = iter.get(idx_gc)`.
    let get_sym = get_symbol_for(b.db, iter);
    let item_gc = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
    b.push(Inst::Call {
        dst: item_gc,
        callee: CallTarget::Runtime(get_sym.to_string()),
        args: vec![iter_local, idx_gc],
        live_roots: vec![iter_local, idx_gc],
    });
    b.check_fault();
    // The loop variable's slot: allocate one if the `for` binding has no slot
    // yet (it is introduced by the loop, not a `let` statement). Reads of the
    // binding inside the body resolve to this slot via `b.locals`.
    let slot = b
        .locals
        .get(&binding)
        .copied()
        .unwrap_or_else(|| b.alloc_gc(Type(0), None, LocalDebugKind::User, None));
    b.locals.insert(binding, slot);
    b.push(Inst::MoveGc {
        dst: slot,
        src: item_gc,
    });
    let _ = lower_block_body(b, body);
    b.loop_stack.pop();
    // Falling off the end of the body reaches the increment the same way
    // `continue` does.
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: incr };
    b.cur = incr;
    // `i = i + 1`: extract, add, re-materialize into the Gc slot.
    let cur_scalar = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ExtractScalar {
        dst: cur_scalar,
        src: idx_gc,
        scalar: ScalarKind::Int,
    });
    let one_scalar = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ConstInt {
        dst: one_scalar,
        value: 1,
    });
    let next_scalar = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::IntBinOp {
        dst: next_scalar,
        op: IntBinOp::Add,
        lhs: cur_scalar,
        rhs: one_scalar,
    });
    b.push(Inst::Materialize {
        dst: idx_gc,
        src: next_scalar,
        scalar: ScalarKind::Int,
        live_roots: vec![iter_local, idx_gc],
    });
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };

    b.cur = exit;
}

/// `loop { body }` (M8-WS6, §4.11). An infinite loop; `break` is the only exit.
fn lower_loop(b: &mut Builder<'_>, body: &praxis_hir::TypedBlock) {
    let header = b.func.new_block();
    let exit = b.func.new_block();
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };
    b.cur = header;
    b.loop_stack.push(LoopCtx {
        continue_target: header,
        break_target: exit,
    });
    let _ = lower_block_body(b, body);
    b.loop_stack.pop();
    // Fall through the body → jump back to the header (infinite loop).
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };
    b.cur = exit;
}

/// `break [expr]` (M8-WS6, §4.11). Jump to the enclosing loop's break target.
/// The optional value is lowered for effect (value-producing loops are a
/// refinement; for now `break` exits with the loop's Unit value).
fn lower_break(b: &mut Builder<'_>, value: &Option<Box<TypedExpr>>) {
    if let Some(v) = value {
        let _ = lower_expr_gc(b, v);
    }
    if let Some(ctx) = b.loop_stack.last().copied() {
        b.func.blocks[b.cur.0 as usize].term = Terminator::Jump {
            target: ctx.break_target,
        };
        // A fresh unreachable block so subsequent lowering has somewhere to go.
        b.cur = b.func.new_block();
    }
}

/// `continue` (M8-WS6, §4.11). Jump to the enclosing loop's continue target.
fn lower_continue(b: &mut Builder<'_>) {
    if let Some(ctx) = b.loop_stack.last().copied() {
        b.func.blocks[b.cur.0 as usize].term = Terminator::Jump {
            target: ctx.continue_target,
        };
        b.cur = b.func.new_block();
    }
}

/// `return [expr]` (M8-WS6, §4.11). Write the value (or Unit) into the function
/// return slot, then terminate with `Return`.
fn lower_return(b: &mut Builder<'_>, value: &Option<Box<TypedExpr>>) {
    let ret = b.func.return_local;
    let val = match value {
        Some(v) => lower_expr_gc(b, v),
        None => lower_lit_gc(b, &Lit::Unit, None),
    };
    b.push(Inst::MoveGc { dst: ret, src: val });
    b.func.blocks[b.cur.0 as usize].term = Terminator::Return { value: ret };
    b.cur = b.func.new_block();
}

// ===========================================================================
// M8-WS11: cross-combinator pipeline fusion (§6.3).
//
// The whole pipeline chain is already visible as a tree at MIR-lowering time:
// each combinator's `receiver` is itself a `TypedExpr::MethodCall` (or a
// collection leaf). `recognize_pipeline` walks that tree to build a
// `PipelinePlan` of streaming `Stage`s terminated by a `Sink`; `lower_pipeline`
// emits *one* fused loop over the source threading each element through the
// stages and into the sink. `v.map(f).filter(p).sum()` → one loop, zero
// intermediate Vecs.
//
// Design note — inline emission. Each stage emits its branches directly into the
// current block rather than returning a control-flow enum. A stage returns
// `(item_after_stage, still_live)`: `still_live == false` means the stage
// already emitted a jump to the loop's continue/break target (e.g. `filter`
// dropped the element, `take_while` stopped the loop), so the caller must not
// lower anything more into the now-dead `b.cur` block. This keeps conditional
// behavior real (a Rust `bool` can't branch for us), and mirrors how the rest
// of the builder (if/while/match) emits straight-line MIR.
//
// The old per-combinator eager lowerers (`lower_pipeline_combinator` +
// `lower_seq_*`) are kept verbatim below as a fallback for any chain the
// recognizer declines, so a regression here can never break the eager path.
// ===========================================================================

/// A streaming pipeline stage (transform the element, possibly skip or stop).
#[derive(Clone)]
enum Stage {
    /// `(T) -> U` — replace the element with the closure's result.
    Map(Box<TypedExpr>),
    /// `(T) -> Bool` — drop the element if the predicate is false.
    Filter(Box<TypedExpr>),
    /// `(T) -> U` — map, then drop the result if it is Unit.
    FilterMap(Box<TypedExpr>),
    /// `(T) -> Vec<U>` — splice the closure's Vec into the sink element by
    /// element, then continue the outer loop.
    FlatMap(Box<TypedExpr>),
    /// Keep at most `n` leading elements, then stop.
    Take(i64),
    /// Drop the first `n` elements.
    Skip(i64),
    /// Stop at the first element that fails the predicate.
    TakeWhile(Box<TypedExpr>),
    /// Replace the element with `(index, element)` tuples.
    Enumerate,
    /// Pair each element with the corresponding element of `other`, stopping at
    /// the shorter length.
    Zip(Box<TypedExpr>),
}

impl Stage {
    /// Whether this stage carries a closure (or second source) that must be
    /// lowered once, outside the loop.
    fn has_arg(&self) -> bool {
        matches!(
            self,
            Stage::Map(_)
                | Stage::Filter(_)
                | Stage::FilterMap(_)
                | Stage::FlatMap(_)
                | Stage::TakeWhile(_)
                | Stage::Zip(_)
        )
    }
}

/// A terminal pipeline sink — produces the chain's final value.
#[derive(Clone)]
enum Sink {
    Sum,
    Product,
    Count,
    Min,
    Max,
    MinBy(Box<TypedExpr>),
    MaxBy(Box<TypedExpr>),
    Any(Box<TypedExpr>),
    All(Box<TypedExpr>),
    /// Index of the first element satisfying the predicate, or -1 on miss.
    Find(Box<TypedExpr>),
    /// Same semantics as `Find` (named alias per §6.3).
    Position(Box<TypedExpr>),
    Fold {
        init: Box<TypedExpr>,
        f: Box<TypedExpr>,
    },
    Reduce(Box<TypedExpr>),
    Collect,
}

/// A recognized pipeline: a source collection, zero or more streaming stages,
/// and a terminal sink. The whole chain lowers to a single fused loop.
struct PipelinePlan {
    source: Box<TypedExpr>,
    /// The element type flowing out of the source (before any stage). Used only
    /// as a slot-allocation hint; `Type(0)` (opaque) is the universal form.
    source_item_ty: Type,
    stages: Vec<Stage>,
    sink: Sink,
    /// The chain's overall result type (carried on the outermost `MethodCall`).
    result_ty: Type,
}

/// Classify a single `MethodCall` node as a streaming stage. `None` means "not a
/// recognized streaming op" — the recognizer treats the receiver eagerly.
fn classify_stage(name: &str, args: &[TypedExpr]) -> Option<Stage> {
    Some(match (name, args) {
        ("map", [f]) => Stage::Map(Box::new(f.clone())),
        ("filter", [p]) => Stage::Filter(Box::new(p.clone())),
        ("filter_map", [f]) => Stage::FilterMap(Box::new(f.clone())),
        ("flat_map", [f]) => Stage::FlatMap(Box::new(f.clone())),
        ("take_while", [p]) => Stage::TakeWhile(Box::new(p.clone())),
        ("enumerate", []) => Stage::Enumerate,
        ("zip", [other]) => Stage::Zip(Box::new(other.clone())),
        (
            "take",
            [TypedExpr::Lit {
                value: Lit::Int(n), ..
            }],
        ) => Stage::Take(*n),
        (
            "skip",
            [TypedExpr::Lit {
                value: Lit::Int(n), ..
            }],
        ) => Stage::Skip(*n),
        _ => return None,
    })
}

/// Classify a single `MethodCall` node as a terminal sink, or `None` if it's a
/// streaming stage / unrecognized method.
fn classify_sink(name: &str, args: &[TypedExpr]) -> Option<Sink> {
    Some(match (name, args) {
        ("sum", []) => Sink::Sum,
        ("product", []) => Sink::Product,
        ("count", []) => Sink::Count,
        ("min", []) => Sink::Min,
        ("max", []) => Sink::Max,
        ("min_by", [f]) => Sink::MinBy(Box::new(f.clone())),
        ("max_by", [f]) => Sink::MaxBy(Box::new(f.clone())),
        ("any", [p]) => Sink::Any(Box::new(p.clone())),
        ("all", [p]) => Sink::All(Box::new(p.clone())),
        ("find", [p]) => Sink::Find(Box::new(p.clone())),
        ("position", [p]) => Sink::Position(Box::new(p.clone())),
        ("fold", [init, f]) => Sink::Fold {
            init: Box::new(init.clone()),
            f: Box::new(f.clone()),
        },
        ("reduce", [f]) => Sink::Reduce(Box::new(f.clone())),
        ("collect", []) => Sink::Collect,
        _ => return None,
    })
}

/// Recognize a pipeline chain rooted at `expr`. Returns `Some(plan)` if `expr`
/// is a `MethodCall` whose outermost call is a recognized sink, a recognized
/// streaming stage (in which case an implicit `Collect` is appended so the chain
/// yields a Vec — mirroring the eager `v.map(f)` behavior), or `collect`; and
/// whose receiver chain is a sequence of recognized streaming stages. Any
/// non-pipeline `MethodCall` receiver (e.g. `.len()`, `.push(x)`) terminates the
/// walk — that inner call lowers eagerly via the existing path, and *its* result
/// becomes this chain's source (recursively fusing if it too is a pipeline).
fn recognize_pipeline(expr: &TypedExpr) -> Option<PipelinePlan> {
    let TypedExpr::MethodCall {
        receiver,
        name,
        args,
        ty: result_ty,
        ..
    } = expr
    else {
        return None;
    };
    // The outermost call is either a terminal sink, or a streaming stage that
    // needs an implicit collect to produce a Vec (e.g. `let out = v.map(f)`).
    let (outermost_stage, sink) = match classify_sink(name, args) {
        Some(s) => (None, s),
        None => {
            let stage = classify_stage(name, args)?;
            (Some(stage), Sink::Collect)
        }
    };
    // Walk the receiver chain collecting stages, outermost-first. `cur` is the
    // node under inspection; once it stops being a streaming `MethodCall` it is
    // the source leaf (whatever it is — `lower_expr_gc` will lower it).
    let mut stages: Vec<Stage> = Vec::new();
    if let Some(stage) = outermost_stage {
        stages.push(stage);
    }
    let mut cur: &TypedExpr = receiver;
    while let TypedExpr::MethodCall {
        receiver: inner_recv,
        name: inner_name,
        args: inner_args,
        ..
    } = cur
    {
        match classify_stage(inner_name, inner_args) {
            Some(stage) => {
                stages.push(stage);
                cur = inner_recv;
            }
            None => break, // Not a streaming stage — `cur` is our source.
        }
    }
    // Stages were collected outermost-first; reverse so the source-side stage
    // runs first inside the loop body.
    stages.reverse();
    Some(PipelinePlan {
        source: Box::new(cur.clone()),
        // The item type is only a slot-allocation hint; every existing lowerer
        // uses the opaque `Type(0)` form, so we match that for consistency.
        source_item_ty: Type(0),
        stages,
        sink,
        result_ty: *result_ty,
    })
}

/// Lower a recognized pipeline as a single fused loop (M8-WS11, §6.3). Emits the
/// loop scaffold directly (header / body / increment / exit) rather than reusing
/// `emit_index_loop`, so streaming stages can `continue` (jump to the increment)
/// and short-circuit sinks/stages can `break` (jump to exit) cleanly.
fn lower_pipeline(b: &mut Builder<'_>, plan: PipelinePlan) -> LocalId {
    let PipelinePlan {
        source,
        source_item_ty,
        stages,
        sink,
        result_ty,
    } = plan;

    // Lower the source Vec once; it lives for the loop's duration.
    let src = lower_expr_gc(b, &source);
    // A Gc Int index counter (persists across blocks, like the for-loop counter).
    let idx = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
    let zero = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ConstInt {
        dst: zero,
        value: 0,
    });
    b.push(Inst::Materialize {
        dst: idx,
        src: zero,
        scalar: ScalarKind::Int,
        live_roots: vec![src],
    });

    // Lower every stage/sink closure or second-source once, outside the loop.
    // `stage_args[i]` is the lowered Gc local for `stages[i]`'s argument (only
    // when `stages[i].has_arg()`); they're pulled in order via `arg_iter`.
    let mut stage_args: Vec<LocalId> = Vec::new();
    let mut loop_roots: Vec<LocalId> = vec![src, idx];
    for stage in &stages {
        if stage.has_arg() {
            let arg_expr: &TypedExpr = match stage {
                Stage::Map(f)
                | Stage::Filter(f)
                | Stage::FilterMap(f)
                | Stage::FlatMap(f)
                | Stage::TakeWhile(f) => f,
                Stage::Zip(other) => other,
                _ => unreachable!(),
            };
            let local = lower_expr_gc(b, arg_expr);
            loop_roots.push(local);
            stage_args.push(local);
        }
    }
    // Sink closure/init, lowered once.
    let (sink_init_slot, sink_closure_slot) = match &sink {
        Sink::Fold { init, f } => {
            let init_l = lower_expr_gc(b, init);
            loop_roots.push(init_l);
            let f_l = lower_expr_gc(b, f);
            loop_roots.push(f_l);
            (Some(init_l), Some(f_l))
        }
        Sink::MinBy(f)
        | Sink::MaxBy(f)
        | Sink::Reduce(f)
        | Sink::Any(f)
        | Sink::All(f)
        | Sink::Find(f)
        | Sink::Position(f) => {
            let f_l = lower_expr_gc(b, f);
            loop_roots.push(f_l);
            (None, Some(f_l))
        }
        _ => (None, None),
    };

    // Allocate the sink's accumulators up front.
    let (acc_scalar, acc_gc, seen_flag) = sink_alloc(b, &sink, sink_init_slot, &mut loop_roots);

    // The Collect sink needs a result Vec pushed into per element.
    let collect_vec = match &sink {
        Sink::Collect => {
            let v = alloc_empty_vec(b, loop_roots.clone());
            loop_roots.push(v);
            Some(v)
        }
        _ => None,
    };

    // ---- The loop scaffold: header / body / increment / exit ---------------
    let header = b.func.new_block();
    let body_blk = b.func.new_block();
    let incr_blk = b.func.new_block();
    let exit = b.func.new_block();
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };
    b.cur = header;

    // Header: `if idx < src.len() { body } else { exit }`.
    emit_bounds_check(b, src, idx, body_blk, exit, &loop_roots);

    // Body: load the element, thread it through the stages, run the sink.
    b.cur = body_blk;
    b.loop_stack.push(LoopCtx {
        continue_target: incr_blk, // filter-skip / flat-map-tail → increment
        break_target: exit,        // take / take_while / any / all / find → exit
    });
    let item = b.alloc_gc(source_item_ty, None, LocalDebugKind::Temp, None);
    b.push(Inst::Call {
        dst: item,
        callee: CallTarget::Runtime("praxis_vec_get".to_string()),
        args: vec![src, idx],
        live_roots: loop_roots.clone(),
    });
    b.check_fault();

    // Run the stages in order. Each stage either replaces the element and
    // leaves `b.cur` live, or emits a skip/stop and leaves a dead `b.cur`.
    // `FlatMap` is special: it produces a Vec whose elements must be spliced
    // into the *remainder* of the stage chain (all stages after flat_map) and
    // then the sink, consuming the outer element.
    let mut cur_item = item;
    let mut arg_iter = stage_args.into_iter();
    let mut alive = true;
    for (stage_idx, stage) in stages.iter().enumerate() {
        if !alive {
            break;
        }
        if let Stage::FlatMap(_) = stage {
            // f(cur_item) -> Vec<U>; for each inner element, run the remaining
            // stages (those after flat_map) and then the sink. The outer
            // element is fully consumed by the flat_map.
            let f = arg_iter.next().unwrap();
            let inner = invoke_closure(b, f, vec![cur_item], &loop_roots);
            let remaining: Vec<Stage> = stages[stage_idx + 1..].to_vec();
            emit_flat_map_inner(
                b,
                inner,
                &remaining,
                &mut arg_iter,
                &sink,
                idx,
                acc_scalar,
                acc_gc,
                seen_flag,
                collect_vec,
                sink_closure_slot,
                &loop_roots,
                incr_blk,
                exit,
            );
            // After splicing, continue to the next outer iteration.
            b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: incr_blk };
            b.cur = b.func.new_block(); // dead — nothing else lowers for this element
            alive = false;
            continue;
        }
        let (new_item, still_live) = run_stage(
            b,
            stage,
            &mut arg_iter,
            cur_item,
            idx,
            &loop_roots,
            incr_blk,
            exit,
        );
        cur_item = new_item;
        alive = still_live;
    }

    // If we're still on a live block, feed the element to the sink.
    if alive {
        emit_sink_body(
            b,
            &sink,
            cur_item,
            idx,
            acc_scalar,
            acc_gc,
            seen_flag,
            collect_vec,
            sink_closure_slot,
            &loop_roots,
        );
        // Normal sink completion: fall through to the increment block. (Sinks
        // that short-circuit — any/all/find — emit their own break and leave
        // `b.cur` dead, in which case this jump goes into a dead block, harm.)
        b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: incr_blk };
    }
    b.loop_stack.pop();

    // Increment block: `idx += 1`, jump to header.
    b.cur = incr_blk;
    emit_increment(b, idx, &loop_roots);
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };

    // Exit: materialize the sink's result out of its accumulator(s).
    b.cur = exit;
    sink_finish(b, &sink, acc_scalar, acc_gc, collect_vec, result_ty)
}

/// Run one streaming stage in-place. Emits branches directly into `b.cur` and
/// returns `(item_after_stage, still_live)`. `still_live == false` means the
/// stage emitted a jump to `incr_blk` (skip) or `exit` (stop) and left `b.cur`
/// on a dead block — the caller must not lower anything more.
///
/// `arg_iter` yields this stage's pre-lowered argument local (closure or second
/// source) in stage order, advancing only for stages with an argument.
#[allow(clippy::too_many_arguments)]
fn run_stage(
    b: &mut Builder<'_>,
    stage: &Stage,
    arg_iter: &mut std::vec::IntoIter<LocalId>,
    item: LocalId,
    idx: LocalId,
    loop_roots: &[LocalId],
    incr_blk: BlockId,
    exit: BlockId,
) -> (LocalId, bool) {
    match stage {
        Stage::Map(_) => {
            let f = arg_iter.next().unwrap();
            (invoke_closure(b, f, vec![item], loop_roots), true)
        }
        Stage::Filter(_) => {
            let p = arg_iter.next().unwrap();
            let keep = call_predicate(b, p, item, loop_roots);
            // On false → jump to incr_blk (skip this element); on true → fall
            // through to a fresh continuation block.
            let keep_blk = b.func.new_block();
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: keep,
                then_block: keep_blk,
                else_block: incr_blk,
            };
            b.cur = keep_blk;
            (item, true)
        }
        Stage::FilterMap(_) => {
            let f = arg_iter.next().unwrap();
            let mapped = invoke_closure(b, f, vec![item], loop_roots);
            // filter_map is modeled as "keep everything": in the catalog it is
            // typed `(T)->U` with non-Unit U, so there's no Unit to filter on.
            // (A precise Unit-drop needs a runtime tag check — see ADR-029.)
            (mapped, true)
        }
        Stage::FlatMap(_) => {
            // Handled inline in `lower_pipeline` before this function is called
            // (flat_map consumes the outer element by splicing an inner Vec
            // into the sink). This arm is unreachable; consume the arg to keep
            // `arg_iter` aligned for any (impossible) subsequent stage.
            let _ = arg_iter.next();
            let _ = (incr_blk, exit);
            unreachable!("flat_map is handled inline in lower_pipeline")
        }
        Stage::TakeWhile(_) => {
            let p = arg_iter.next().unwrap();
            let keep = call_predicate(b, p, item, loop_roots);
            let keep_blk = b.func.new_block();
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: keep,
                then_block: keep_blk,
                else_block: exit, // predicate false → stop the loop
            };
            b.cur = keep_blk;
            (item, true)
        }
        Stage::Take(n) => {
            // If idx >= n → stop (jump to exit); else fall through.
            let stop = idx_cmp_const(b, idx, *n, CmpOp::Ge, loop_roots);
            let keep_blk = b.func.new_block();
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: stop,
                then_block: exit,
                else_block: keep_blk,
            };
            b.cur = keep_blk;
            (item, true)
        }
        Stage::Skip(n) => {
            // If idx < n → skip (jump to incr_blk); else fall through.
            let skip = idx_cmp_const(b, idx, *n, CmpOp::Lt, loop_roots);
            let keep_blk = b.func.new_block();
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: skip,
                then_block: incr_blk,
                else_block: keep_blk,
            };
            b.cur = keep_blk;
            (item, true)
        }
        Stage::Enumerate => {
            // Replace item with (idx, item). idx is already a Gc Int slot; copy
            // it so the tuple owns a stable value.
            let idx_copy = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
            b.push(Inst::MoveGc {
                dst: idx_copy,
                src: idx,
            });
            let tup = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
            b.push(Inst::Alloc {
                dst: tup,
                alloc: AllocKind::Tuple {
                    // The tuple's precise type isn't known here; codegen derives
                    // the schema from runtime element tags (consistent with the
                    // opaque Type(0) used by the existing Tuple lowerer when the
                    // schema is reconstructed downstream).
                    ty: Type(0),
                    elements: vec![idx_copy, item],
                },
                live_roots: Vec::new(),
            });
            (tup, true)
        }
        Stage::Zip(_) => {
            let other = arg_iter.next().unwrap();
            // Stop if idx >= other.len(); else pair (item, other.get(idx)).
            let stop = idx_ge_len(b, other, idx, loop_roots);
            let pair_blk = b.func.new_block();
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: stop,
                then_block: exit,
                else_block: pair_blk,
            };
            b.cur = pair_blk;
            let other_item = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
            b.push(Inst::Call {
                dst: other_item,
                callee: CallTarget::Runtime("praxis_vec_get".to_string()),
                args: vec![other, idx],
                live_roots: loop_roots.to_vec(),
            });
            b.check_fault();
            let tup = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
            b.push(Inst::Alloc {
                dst: tup,
                alloc: AllocKind::Tuple {
                    ty: Type(0),
                    elements: vec![item, other_item],
                },
                live_roots: Vec::new(),
            });
            (tup, true)
        }
    }
}

/// Emit `idx <op> n` as a Bool scalar and return it. Used by Take/Skip.
fn idx_cmp_const(
    b: &mut Builder<'_>,
    idx: LocalId,
    n: i64,
    op: CmpOp,
    _loop_roots: &[LocalId],
) -> LocalId {
    let idx_scalar = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ExtractScalar {
        dst: idx_scalar,
        src: idx,
        scalar: ScalarKind::Int,
    });
    let n_scalar = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ConstInt {
        dst: n_scalar,
        value: n,
    });
    let dst = b.alloc_scalar(ScalarKind::Bool);
    b.push(Inst::IntCmp {
        dst,
        op,
        lhs: idx_scalar,
        rhs: n_scalar,
    });
    dst
}

/// Emit `idx >= other.len()` as a Bool scalar (used by Zip's stop condition).
fn idx_ge_len(
    b: &mut Builder<'_>,
    other: LocalId,
    idx: LocalId,
    loop_roots: &[LocalId],
) -> LocalId {
    let len_dst = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
    b.push(Inst::Call {
        dst: len_dst,
        callee: CallTarget::Runtime("praxis_vec_len".to_string()),
        args: vec![other],
        live_roots: loop_roots.to_vec(),
    });
    b.check_fault();
    let len_scalar = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ExtractScalar {
        dst: len_scalar,
        src: len_dst,
        scalar: ScalarKind::Int,
    });
    let idx_scalar = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ExtractScalar {
        dst: idx_scalar,
        src: idx,
        scalar: ScalarKind::Int,
    });
    let dst = b.alloc_scalar(ScalarKind::Bool);
    b.push(Inst::IntCmp {
        dst,
        op: CmpOp::Ge,
        lhs: idx_scalar,
        rhs: len_scalar,
    });
    dst
}

/// Emit the header's bounds check: `if idx < src.len() { then } else { els }`.
fn emit_bounds_check(
    b: &mut Builder<'_>,
    src: LocalId,
    idx: LocalId,
    then_blk: BlockId,
    els_blk: BlockId,
    loop_roots: &[LocalId],
) {
    let len_dst = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
    b.push(Inst::Call {
        dst: len_dst,
        callee: CallTarget::Runtime("praxis_vec_len".to_string()),
        args: vec![src],
        live_roots: loop_roots.to_vec(),
    });
    b.check_fault();
    let len_scalar = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ExtractScalar {
        dst: len_scalar,
        src: len_dst,
        scalar: ScalarKind::Int,
    });
    let idx_scalar = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ExtractScalar {
        dst: idx_scalar,
        src: idx,
        scalar: ScalarKind::Int,
    });
    let cond = b.alloc_scalar(ScalarKind::Bool);
    b.push(Inst::IntCmp {
        dst: cond,
        op: CmpOp::Lt,
        lhs: idx_scalar,
        rhs: len_scalar,
    });
    b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
        cond,
        then_block: then_blk,
        else_block: els_blk,
    };
}

/// Invoke a closure `f(args)` via `CallIndirect` and return the result slot.
fn invoke_closure(
    b: &mut Builder<'_>,
    f: LocalId,
    args: Vec<LocalId>,
    loop_roots: &[LocalId],
) -> LocalId {
    let mut roots = vec![f];
    roots.extend(args.iter().copied());
    roots.extend(loop_roots.iter().copied());
    let dst = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
    b.push(Inst::CallIndirect {
        dst,
        callee: f,
        args,
        live_roots: roots,
    });
    b.check_fault();
    dst
}

/// Call a `(T)->Bool` predicate closure and extract the Bool scalar.
fn call_predicate(
    b: &mut Builder<'_>,
    p: LocalId,
    item: LocalId,
    loop_roots: &[LocalId],
) -> LocalId {
    let keep_gc = invoke_closure(b, p, vec![item], loop_roots);
    let keep = b.alloc_scalar(ScalarKind::Bool);
    b.push(Inst::ExtractScalar {
        dst: keep,
        src: keep_gc,
        scalar: ScalarKind::Bool,
    });
    keep
}

/// Allocate the sink's accumulators up front. Returns
/// `(acc_scalar, acc_gc, seen_flag)`:
/// - `acc_scalar` — the running Int/Bool scalar.
/// - `acc_gc` — a Gc slot for fold/reduce/min_by/max_by.
/// - `seen_flag` — a Bool "first element seen" flag for min/max/reduce.
fn sink_alloc(
    b: &mut Builder<'_>,
    sink: &Sink,
    sink_init_slot: Option<LocalId>,
    loop_roots: &mut Vec<LocalId>,
) -> (Option<LocalId>, Option<LocalId>, Option<LocalId>) {
    match sink {
        Sink::Sum | Sink::Product | Sink::Count | Sink::Find(_) | Sink::Position(_) => {
            let acc = b.alloc_scalar(ScalarKind::Int);
            let init = match sink {
                Sink::Product => 1,
                Sink::Find(_) | Sink::Position(_) => -1, // miss sentinel
                _ => 0,
            };
            b.push(Inst::ConstInt {
                dst: acc,
                value: init,
            });
            (Some(acc), None, None)
        }
        Sink::Min | Sink::Max => {
            let acc = b.alloc_scalar(ScalarKind::Int);
            b.push(Inst::ConstInt { dst: acc, value: 0 });
            let seen = b.alloc_scalar(ScalarKind::Bool);
            b.push(Inst::ConstInt {
                dst: seen,
                value: 0,
            }); // false
            (Some(acc), None, Some(seen))
        }
        Sink::Any(_) | Sink::All(_) => {
            let acc = b.alloc_scalar(ScalarKind::Bool);
            // any → false; all → true.
            let init = matches!(sink, Sink::All(_)) as i64;
            b.push(Inst::ConstInt {
                dst: acc,
                value: init,
            });
            (Some(acc), None, None)
        }
        Sink::MinBy(_) | Sink::MaxBy(_) => {
            // Hold the running best element in a Gc slot; the seen-flag gates
            // the first comparison.
            let acc = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
            if let Some(init) = sink_init_slot {
                b.push(Inst::MoveGc {
                    dst: acc,
                    src: init,
                });
            }
            loop_roots.push(acc);
            let seen = b.alloc_scalar(ScalarKind::Bool);
            b.push(Inst::ConstInt {
                dst: seen,
                value: 0,
            });
            (None, Some(acc), Some(seen))
        }
        Sink::Fold { .. } => {
            // acc = init (a Gc slot carrying a closure-produced value across
            // iterations). This closes the M8 `fold` stub.
            let acc = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
            if let Some(init) = sink_init_slot {
                b.push(Inst::MoveGc {
                    dst: acc,
                    src: init,
                });
            }
            loop_roots.push(acc);
            (None, Some(acc), None)
        }
        Sink::Reduce(_) => {
            // Seed from the first element; allocate an opaque Gc slot now.
            let acc = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
            loop_roots.push(acc);
            let seen = b.alloc_scalar(ScalarKind::Bool);
            b.push(Inst::ConstInt {
                dst: seen,
                value: 0,
            });
            (None, Some(acc), Some(seen))
        }
        Sink::Collect => (None, None, None),
    }
}

/// Emit `idx += 1` (extract, add, re-materialize into the Gc slot).
fn emit_increment(b: &mut Builder<'_>, idx: LocalId, loop_roots: &[LocalId]) {
    let cur = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ExtractScalar {
        dst: cur,
        src: idx,
        scalar: ScalarKind::Int,
    });
    let one = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ConstInt { dst: one, value: 1 });
    let next = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::IntBinOp {
        dst: next,
        op: IntBinOp::Add,
        lhs: cur,
        rhs: one,
    });
    b.push(Inst::Materialize {
        dst: idx,
        src: next,
        scalar: ScalarKind::Int,
        live_roots: loop_roots.to_vec(),
    });
}

/// Copy one scalar into another. There is no scalar-move Inst, so the idiom is
/// `dst = src + 0` (the existing lowerers use the same trick).
fn move_scalar(b: &mut Builder<'_>, dst: LocalId, src: LocalId) {
    let zero = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ConstInt {
        dst: zero,
        value: 0,
    });
    b.push(Inst::IntBinOp {
        dst,
        op: IntBinOp::Add,
        lhs: src,
        rhs: zero,
    });
}

/// Emit the sink's per-element update into the current (live) body block.
#[allow(clippy::too_many_arguments)]
fn emit_sink_body(
    b: &mut Builder<'_>,
    sink: &Sink,
    item: LocalId,
    idx: LocalId,
    acc_scalar: Option<LocalId>,
    acc_gc: Option<LocalId>,
    seen_flag: Option<LocalId>,
    collect_vec: Option<LocalId>,
    sink_closure_slot: Option<LocalId>,
    loop_roots: &[LocalId],
) {
    match sink {
        Sink::Sum | Sink::Product => {
            let acc = acc_scalar.unwrap();
            let item_scalar = b.alloc_scalar(ScalarKind::Int);
            b.push(Inst::ExtractScalar {
                dst: item_scalar,
                src: item,
                scalar: ScalarKind::Int,
            });
            b.push(Inst::IntBinOp {
                dst: acc,
                op: if matches!(sink, Sink::Sum) {
                    IntBinOp::Add
                } else {
                    IntBinOp::Mul
                },
                lhs: acc,
                rhs: item_scalar,
            });
        }
        Sink::Count => {
            let acc = acc_scalar.unwrap();
            let one = b.alloc_scalar(ScalarKind::Int);
            b.push(Inst::ConstInt { dst: one, value: 1 });
            b.push(Inst::IntBinOp {
                dst: acc,
                op: IntBinOp::Add,
                lhs: acc,
                rhs: one,
            });
        }
        Sink::Min | Sink::Max => {
            let acc = acc_scalar.unwrap();
            let seen = seen_flag.unwrap();
            let item_scalar = b.alloc_scalar(ScalarKind::Int);
            b.push(Inst::ExtractScalar {
                dst: item_scalar,
                src: item,
                scalar: ScalarKind::Int,
            });
            // If !seen { acc = item; seen = true } else { if cmp { acc = item } }.
            let cmp_blk = b.func.new_block();
            let set_blk = b.func.new_block();
            let cont_blk = b.func.new_block();
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: seen,
                then_block: cmp_blk,
                else_block: set_blk,
            };
            // First element: seed.
            b.cur = set_blk;
            b.push(Inst::ConstInt {
                dst: seen,
                value: 1,
            });
            move_scalar(b, acc, item_scalar);
            b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: cont_blk };
            // Subsequent: compare and maybe update.
            b.cur = cmp_blk;
            let cond = b.alloc_scalar(ScalarKind::Bool);
            b.push(Inst::IntCmp {
                dst: cond,
                op: if matches!(sink, Sink::Min) {
                    CmpOp::Lt
                } else {
                    CmpOp::Gt
                },
                lhs: item_scalar,
                rhs: acc,
            });
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond,
                then_block: set_blk, // reuse: acc = item (seen already true)
                else_block: cont_blk,
            };
            b.cur = cont_blk;
        }
        Sink::MinBy(_) | Sink::MaxBy(_) => {
            let acc = acc_gc.unwrap();
            let seen = seen_flag.unwrap();
            let f = sink_closure_slot.unwrap();
            let cmp_blk = b.func.new_block();
            let set_blk = b.func.new_block();
            let cont_blk = b.func.new_block();
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: seen,
                then_block: cmp_blk,
                else_block: set_blk,
            };
            b.cur = set_blk;
            b.push(Inst::ConstInt {
                dst: seen,
                value: 1,
            });
            b.push(Inst::MoveGc {
                dst: acc,
                src: item,
            });
            b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: cont_blk };
            b.cur = cmp_blk;
            // The comparator is "less-than": f(a, b) = a < b. For min, item is
            // better when item < acc → f(item, acc). For max, item is better
            // when item > acc ⟺ acc < item → f(acc, item).
            let better_gc = if matches!(sink, Sink::MinBy(_)) {
                invoke_closure(b, f, vec![item, acc], loop_roots)
            } else {
                invoke_closure(b, f, vec![acc, item], loop_roots)
            };
            let better = b.alloc_scalar(ScalarKind::Bool);
            b.push(Inst::ExtractScalar {
                dst: better,
                src: better_gc,
                scalar: ScalarKind::Bool,
            });
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: better,
                then_block: set_blk,
                else_block: cont_blk,
            };
            b.cur = cont_blk;
        }
        Sink::Any(_) | Sink::All(_) => {
            let acc = acc_scalar.unwrap();
            let pred = sink_closure_slot.unwrap();
            let keep = call_predicate(b, pred, item, loop_roots);
            // any trips (short-circuits) on true; all trips on false.
            let trip_cond = if matches!(sink, Sink::Any(_)) {
                keep
            } else {
                invert_bool(b, keep)
            };
            let trip_blk = b.func.new_block();
            let cont_blk = b.func.new_block();
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: trip_cond,
                then_block: trip_blk,
                else_block: cont_blk,
            };
            // trip: set acc, break out of the loop.
            b.cur = trip_blk;
            let val = if matches!(sink, Sink::Any(_)) { 1 } else { 0 };
            b.push(Inst::ConstInt {
                dst: acc,
                value: val,
            });
            break_loop(b);
            b.cur = cont_blk;
        }
        Sink::Find(_) | Sink::Position(_) => {
            let acc = acc_scalar.unwrap();
            let pred = sink_closure_slot.unwrap();
            let keep = call_predicate(b, pred, item, loop_roots);
            let found_blk = b.func.new_block();
            let cont_blk = b.func.new_block();
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: keep,
                then_block: found_blk,
                else_block: cont_blk,
            };
            b.cur = found_blk;
            let idx_scalar = b.alloc_scalar(ScalarKind::Int);
            b.push(Inst::ExtractScalar {
                dst: idx_scalar,
                src: idx,
                scalar: ScalarKind::Int,
            });
            move_scalar(b, acc, idx_scalar);
            break_loop(b);
            b.cur = cont_blk;
        }
        Sink::Fold { .. } => {
            let acc = acc_gc.unwrap();
            let f = sink_closure_slot.unwrap();
            let new_acc = invoke_closure(b, f, vec![acc, item], loop_roots);
            b.push(Inst::MoveGc {
                dst: acc,
                src: new_acc,
            });
        }
        Sink::Reduce(_) => {
            let acc = acc_gc.unwrap();
            let seen = seen_flag.unwrap();
            let f = sink_closure_slot.unwrap();
            // If !seen { acc = item; seen = true; continue } else { acc = f(acc, item) }.
            let fold_blk = b.func.new_block();
            let seed_blk = b.func.new_block();
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: seen,
                then_block: fold_blk,
                else_block: seed_blk,
            };
            b.cur = seed_blk;
            b.push(Inst::ConstInt {
                dst: seen,
                value: 1,
            });
            b.push(Inst::MoveGc {
                dst: acc,
                src: item,
            });
            continue_loop(b);
            b.cur = fold_blk;
            let new_acc = invoke_closure(b, f, vec![acc, item], loop_roots);
            b.push(Inst::MoveGc {
                dst: acc,
                src: new_acc,
            });
        }
        Sink::Collect => {
            let result = collect_vec.unwrap();
            let unit = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
            let mut roots = vec![result, item];
            roots.extend(loop_roots.iter().copied());
            b.push(Inst::Call {
                dst: unit,
                callee: CallTarget::Runtime("praxis_vec_push".to_string()),
                args: vec![result, item],
                live_roots: roots,
            });
        }
    }
}

/// Emit the inner iteration for a `flat_map` stage: for each element of
/// `inner_vec`, run the remaining stages (those after the flat_map in the
/// chain) and then the sink body. The inner loop has its own index, header,
/// and exit (which falls through to the outer loop's continuation, set by the
/// caller). A nested `LoopCtx` is pushed so any stage `continue`/`break` or
/// sink short-circuit (`any`/`all`/`find`) scopes to this inner loop.
///
/// `remaining_args` yields the pre-lowered argument local for each remaining
/// stage that carries one, in stage order (same contract as the outer loop's
/// `arg_iter`). It is advanced only for stages with an argument.
#[allow(clippy::too_many_arguments)]
fn emit_flat_map_inner(
    b: &mut Builder<'_>,
    inner_vec: LocalId,
    remaining: &[Stage],
    remaining_args: &mut std::vec::IntoIter<LocalId>,
    sink: &Sink,
    outer_idx: LocalId,
    acc_scalar: Option<LocalId>,
    acc_gc: Option<LocalId>,
    seen_flag: Option<LocalId>,
    collect_vec: Option<LocalId>,
    sink_closure_slot: Option<LocalId>,
    loop_roots: &[LocalId],
    incr_blk: BlockId,
    exit: BlockId,
) {
    let inner_idx = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
    let zero = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ConstInt {
        dst: zero,
        value: 0,
    });
    b.push(Inst::Materialize {
        dst: inner_idx,
        src: zero,
        scalar: ScalarKind::Int,
        live_roots: loop_roots.to_vec(),
    });
    let mut roots = vec![inner_vec, inner_idx];
    roots.extend(loop_roots.iter().copied());

    let header = b.func.new_block();
    let body_blk = b.func.new_block();
    let inner_incr = b.func.new_block();
    let inner_exit = b.func.new_block();
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };
    b.cur = header;
    emit_bounds_check(b, inner_vec, inner_idx, body_blk, inner_exit, &roots);

    b.cur = body_blk;
    b.loop_stack.push(LoopCtx {
        continue_target: inner_incr,
        break_target: inner_exit,
    });
    let inner_item = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
    b.push(Inst::Call {
        dst: inner_item,
        callee: CallTarget::Runtime("praxis_vec_get".to_string()),
        args: vec![inner_vec, inner_idx],
        live_roots: roots.clone(),
    });
    b.check_fault();

    // Run the remaining stages on the inner element. Each stage emits its
    // branches inline; a stage that drops the element (filter) jumps to the
    // inner increment (continue), and one that stops (take/take_while) jumps to
    // the inner exit (break). A nested flat_map here is not supported (it would
    // require recursive emission); the recognizer does not produce nested
    // flat_maps within one chain because each flat_map consumes its outer
    // element, so `remaining` contains only non-flat_map stages.
    let mut cur_item = inner_item;
    let mut alive = true;
    for stage in remaining {
        if !alive {
            break;
        }
        let (new_item, still_live) = run_stage(
            b,
            stage,
            remaining_args,
            cur_item,
            inner_idx,
            &roots,
            inner_incr,
            inner_exit,
        );
        cur_item = new_item;
        alive = still_live;
    }

    // If still live, feed the inner element to the sink. The index reported to
    // the sink is the inner index (the position within the flat_map's output);
    // find/position thus report the inner position, matching eager semantics
    // where flat_map produces a flat sequence.
    if alive {
        emit_sink_body(
            b,
            sink,
            cur_item,
            inner_idx,
            acc_scalar,
            acc_gc,
            seen_flag,
            collect_vec,
            sink_closure_slot,
            &roots,
        );
        // Normal sink completion: fall through to the inner increment block
        // (NOT the header — jumping to the header skips the increment and
        // spins the loop, the same M8-WS11 bug that the outer loop guards
        // against).
        b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: inner_incr };
    }
    b.loop_stack.pop();

    // Inner increment block: `inner_idx += 1`, jump to header.
    b.cur = inner_incr;
    emit_increment(b, inner_idx, &roots);
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };
    b.cur = inner_exit;
    // The outer_idx, incr_blk, exit, and outer sink machinery are not used
    // here; they are parameters for signature symmetry with a future nested-
    // flat_map path. Suppress unused warnings.
    let _ = (outer_idx, incr_blk, exit);
}

/// Emit `!bool` as a fresh Bool scalar (Bool is `i8`; `== 0` inverts it).
fn invert_bool(b: &mut Builder<'_>, x: LocalId) -> LocalId {
    let zero = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ConstInt {
        dst: zero,
        value: 0,
    });
    let not_x = b.alloc_scalar(ScalarKind::Bool);
    b.push(Inst::IntCmp {
        dst: not_x,
        op: CmpOp::Eq,
        lhs: x,
        rhs: zero,
    });
    not_x
}

/// Jump to the enclosing loop's break target and leave `b.cur` on a fresh dead
/// block so subsequent lowering has somewhere to append.
fn break_loop(b: &mut Builder<'_>) {
    if let Some(ctx) = b.loop_stack.last().copied() {
        b.func.blocks[b.cur.0 as usize].term = Terminator::Jump {
            target: ctx.break_target,
        };
        b.cur = b.func.new_block();
    }
}

/// Jump to the enclosing loop's continue target and leave `b.cur` on a fresh
/// dead block.
fn continue_loop(b: &mut Builder<'_>) {
    if let Some(ctx) = b.loop_stack.last().copied() {
        b.func.blocks[b.cur.0 as usize].term = Terminator::Jump {
            target: ctx.continue_target,
        };
        b.cur = b.func.new_block();
    }
}

/// Materialize the sink's final result out of its accumulator(s).
fn sink_finish(
    b: &mut Builder<'_>,
    sink: &Sink,
    acc_scalar: Option<LocalId>,
    acc_gc: Option<LocalId>,
    collect_vec: Option<LocalId>,
    _result_ty: Type,
) -> LocalId {
    match sink {
        Sink::Collect => collect_vec.unwrap(),
        Sink::Fold { .. } | Sink::Reduce(_) | Sink::MinBy(_) | Sink::MaxBy(_) => acc_gc.unwrap(),
        Sink::Any(_) | Sink::All(_) => {
            let acc = acc_scalar.unwrap();
            let dst = b.alloc_gc(b.bool_ty, None, LocalDebugKind::Temp, None);
            b.push(Inst::Materialize {
                dst,
                src: acc,
                scalar: ScalarKind::Bool,
                live_roots: Vec::new(),
            });
            dst
        }
        Sink::Sum
        | Sink::Product
        | Sink::Count
        | Sink::Min
        | Sink::Max
        | Sink::Find(_)
        | Sink::Position(_) => {
            let acc = acc_scalar.unwrap();
            let dst = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
            b.push(Inst::Materialize {
                dst,
                src: acc,
                scalar: ScalarKind::Int,
                live_roots: Vec::new(),
            });
            dst
        }
    }
}

/// Lower a pipeline combinator intrinsic (M8-WS8, §6.3) over a Vec receiver
/// into a fused loop. Each combinator allocates its own loop here; true cross-
/// combinator fusion (one loop for `v.map(f).filter(p).sum()`) is the next
/// refinement — this single-combinator form already delivers the seamless
/// experience for the common `v.sum()` / `v.count()` / `v.map(f)` cases.
///
/// `name` is the combinator; `args` are its explicit args (the closure/init).
/// `ty` is the call's result type (used for the result slot's type id).
fn lower_pipeline_combinator(
    b: &mut Builder<'_>,
    receiver: &TypedExpr,
    name: &str,
    args: &[TypedExpr],
    ty: Type,
) -> LocalId {
    // Lower the receiver Vec once; it lives for the loop's duration.
    let src = lower_expr_gc(b, receiver);
    // A Gc Int index counter (persists across blocks, like the for-loop counter).
    let idx = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
    let zero = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ConstInt {
        dst: zero,
        value: 0,
    });
    b.push(Inst::Materialize {
        dst: idx,
        src: zero,
        scalar: ScalarKind::Int,
        live_roots: vec![src],
    });
    match name {
        "sum" => lower_seq_sum(b, src, idx, ty),
        "count" => lower_seq_count(b, src, idx, ty),
        "map" if !args.is_empty() => lower_seq_map(b, src, idx, &args[0], ty),
        "filter" if !args.is_empty() => lower_seq_filter(b, src, idx, &args[0], ty),
        "collect" => lower_seq_collect(b, src, idx, ty),
        "fold" if args.len() >= 2 => lower_seq_fold(b, src, idx, &args[0], &args[1], ty),
        _ => {
            // Unknown intrinsic: defensively return Unit.
            lower_lit_gc(b, &Lit::Unit, None)
        }
    }
}

/// `v.sum()`: loop, accumulate `acc += item`, materialize.
fn lower_seq_sum(b: &mut Builder<'_>, src: LocalId, idx: LocalId, _ty: Type) -> LocalId {
    let acc = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ConstInt { dst: acc, value: 0 });
    emit_index_loop(b, src, idx, vec![acc], |b, item, locals| {
        let item_scalar = b.alloc_scalar(ScalarKind::Int);
        b.push(Inst::ExtractScalar {
            dst: item_scalar,
            src: item,
            scalar: ScalarKind::Int,
        });
        b.push(Inst::IntBinOp {
            dst: locals[0],
            op: IntBinOp::Add,
            lhs: locals[0],
            rhs: item_scalar,
        });
    });
    let result = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
    b.push(Inst::Materialize {
        dst: result,
        src: acc,
        scalar: ScalarKind::Int,
        live_roots: Vec::new(),
    });
    result
}

/// `v.count()`: loop, `acc += 1`, materialize.
fn lower_seq_count(b: &mut Builder<'_>, src: LocalId, idx: LocalId, _ty: Type) -> LocalId {
    let acc = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ConstInt { dst: acc, value: 0 });
    emit_index_loop(b, src, idx, vec![acc], |b, _item, locals| {
        let one = b.alloc_scalar(ScalarKind::Int);
        b.push(Inst::ConstInt { dst: one, value: 1 });
        b.push(Inst::IntBinOp {
            dst: locals[0],
            op: IntBinOp::Add,
            lhs: locals[0],
            rhs: one,
        });
    });
    let result = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
    b.push(Inst::Materialize {
        dst: result,
        src: acc,
        scalar: ScalarKind::Int,
        live_roots: Vec::new(),
    });
    result
}

/// Allocate an empty Vec via praxis_vec_new with a null (default INT) element
/// descriptor. The result Vec adopts each pushed value's descriptor on first
/// push (mirrors the construction-time adoption). Returns the result local.
fn alloc_empty_vec(b: &mut Builder<'_>, live: Vec<LocalId>) -> LocalId {
    let null_desc = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ConstInt {
        dst: null_desc,
        value: 0,
    });
    let null_gc = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
    b.push(Inst::MoveGc {
        dst: null_gc,
        src: null_desc,
    });
    let result = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
    b.push(Inst::Call {
        dst: result,
        callee: CallTarget::Runtime("praxis_vec_new".to_string()),
        args: vec![null_gc],
        live_roots: live,
    });
    b.check_fault();
    result
}

/// `v.map(f)`: allocate a result Vec, loop, push `f(item)` for each.
fn lower_seq_map(
    b: &mut Builder<'_>,
    src: LocalId,
    idx: LocalId,
    closure: &TypedExpr,
    _ty: Type,
) -> LocalId {
    let f = lower_expr_gc(b, closure);
    let result = alloc_empty_vec(b, vec![src, f]);
    emit_index_loop(b, src, idx, vec![f, result], |b, item, locals| {
        // Invoke f(item) via the closure (Inst::CallIndirect, M7).
        let mapped = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
        b.push(Inst::CallIndirect {
            dst: mapped,
            callee: locals[0],
            args: vec![item],
            live_roots: vec![locals[0], item, locals[1]],
        });
        b.check_fault();
        // Push the mapped value into the result Vec.
        let unit = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
        b.push(Inst::Call {
            dst: unit,
            callee: CallTarget::Runtime("praxis_vec_push".to_string()),
            args: vec![locals[1], mapped],
            live_roots: vec![locals[1], mapped],
        });
    });
    result
}

/// `v.filter(p)`: allocate a result Vec, loop, push `item` when `p(item)`.
fn lower_seq_filter(
    b: &mut Builder<'_>,
    src: LocalId,
    idx: LocalId,
    closure: &TypedExpr,
    _ty: Type,
) -> LocalId {
    let p = lower_expr_gc(b, closure);
    let result = alloc_empty_vec(b, vec![src, p]);
    emit_index_loop(b, src, idx, vec![p, result], |b, item, locals| {
        // Call p(item) → Bool via the closure.
        let keep_gc = b.alloc_gc(b.bool_ty, None, LocalDebugKind::Temp, None);
        b.push(Inst::CallIndirect {
            dst: keep_gc,
            callee: locals[0],
            args: vec![item],
            live_roots: vec![locals[0], item, locals[1]],
        });
        b.check_fault();
        let keep = b.alloc_scalar(ScalarKind::Bool);
        b.push(Inst::ExtractScalar {
            dst: keep,
            src: keep_gc,
            scalar: ScalarKind::Bool,
        });
        let push_blk = b.func.new_block();
        let cont_blk = b.func.new_block();
        b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
            cond: keep,
            then_block: push_blk,
            else_block: cont_blk,
        };
        b.cur = push_blk;
        let unit = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
        b.push(Inst::Call {
            dst: unit,
            callee: CallTarget::Runtime("praxis_vec_push".to_string()),
            args: vec![locals[1], item],
            live_roots: vec![locals[1], item],
        });
        b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: cont_blk };
        b.cur = cont_blk;
    });
    result
}

/// `v.collect()`: copy all elements into a fresh Vec (no predicate).
fn lower_seq_collect(b: &mut Builder<'_>, src: LocalId, idx: LocalId, _ty: Type) -> LocalId {
    let result = alloc_empty_vec(b, vec![src]);
    emit_index_loop(b, src, idx, vec![result], |b, item, locals| {
        let unit = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
        b.push(Inst::Call {
            dst: unit,
            callee: CallTarget::Runtime("praxis_vec_push".to_string()),
            args: vec![locals[0], item],
            live_roots: vec![locals[0], item],
        });
    });
    result
}

/// `v.fold(init, f)`: loop, threading an accumulator through `f(acc, item)`.
fn lower_seq_fold(
    b: &mut Builder<'_>,
    _src: LocalId,
    _idx: LocalId,
    _init: &TypedExpr,
    _closure: &TypedExpr,
    _ty: Type,
) -> LocalId {
    // Fold requires closure invocation (CallIndirect); deferred to the closure-
    // invocation refinement. Return the init value lowered for now.
    lower_expr_gc(b, _init)
}

/// Emit an index loop over `src` (a Vec) calling `body(b, item_local, locals)`
/// for each element, where `locals` are the caller-provided locals that persist
/// across iterations (e.g. an accumulator or the result Vec). The `idx` Gc Int
/// counter is incremented each iteration. Reuses the for-loop block structure.
fn emit_index_loop<F>(
    b: &mut Builder<'_>,
    src: LocalId,
    idx: LocalId,
    locals: Vec<LocalId>,
    body: F,
) where
    F: FnOnce(&mut Builder<'_>, LocalId, &[LocalId]),
{
    let header = b.func.new_block();
    let body_blk = b.func.new_block();
    let exit = b.func.new_block();
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };
    b.cur = header;

    // `len = src.len()`
    let mut roots = vec![src, idx];
    roots.extend(locals.iter().copied());
    let len_dst = b.alloc_gc(b.int_ty, None, LocalDebugKind::Temp, None);
    b.push(Inst::Call {
        dst: len_dst,
        callee: CallTarget::Runtime("praxis_vec_len".to_string()),
        args: vec![src],
        live_roots: roots.clone(),
    });
    b.check_fault();
    let len_scalar = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ExtractScalar {
        dst: len_scalar,
        src: len_dst,
        scalar: ScalarKind::Int,
    });
    let idx_scalar = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ExtractScalar {
        dst: idx_scalar,
        src: idx,
        scalar: ScalarKind::Int,
    });
    let cond = b.alloc_scalar(ScalarKind::Bool);
    b.push(Inst::IntCmp {
        dst: cond,
        op: CmpOp::Lt,
        lhs: idx_scalar,
        rhs: len_scalar,
    });
    b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
        cond,
        then_block: body_blk,
        else_block: exit,
    };

    b.cur = body_blk;
    // `item = src.get(idx)`
    let item = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
    b.push(Inst::Call {
        dst: item,
        callee: CallTarget::Runtime("praxis_vec_get".to_string()),
        args: vec![src, idx],
        live_roots: roots.clone(),
    });
    b.check_fault();
    body(b, item, &locals);
    // `idx += 1`
    let cur = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ExtractScalar {
        dst: cur,
        src: idx,
        scalar: ScalarKind::Int,
    });
    let one = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ConstInt { dst: one, value: 1 });
    let next = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::IntBinOp {
        dst: next,
        op: IntBinOp::Add,
        lhs: cur,
        rhs: one,
    });
    b.push(Inst::Materialize {
        dst: idx,
        src: next,
        scalar: ScalarKind::Int,
        live_roots: roots,
    });
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };
    b.cur = exit;
}

/// Pick the `praxis_<kind>_len` runtime symbol for an iterable expression by its
/// static collection ctor. Defaults to `praxis_vec_len` (the common Vec case).
fn len_symbol_for(db: &TypeDb, iter: &TypedExpr) -> &'static str {
    use praxis_types::data::TypeData;
    let ty = expr_static_type(iter);
    match db.data(db.follow(ty)) {
        TypeData::Collection { ctor, .. } => match ctor {
            praxis_types::CollectionCtor::Vec => "praxis_vec_len",
            praxis_types::CollectionCtor::Deque => "praxis_deque_len",
            praxis_types::CollectionCtor::Map => "praxis_map_len",
            praxis_types::CollectionCtor::Set => "praxis_set_len",
            praxis_types::CollectionCtor::Counter => "praxis_counter_len",
            _ => "praxis_vec_len",
        },
        _ => "praxis_vec_len",
    }
}

/// Pick the `praxis_<kind>_get` runtime symbol for element access.
fn get_symbol_for(db: &TypeDb, iter: &TypedExpr) -> &'static str {
    use praxis_types::data::TypeData;
    let ty = expr_static_type(iter);
    match db.data(db.follow(ty)) {
        TypeData::Collection { ctor, .. } => match ctor {
            praxis_types::CollectionCtor::Vec => "praxis_vec_get",
            praxis_types::CollectionCtor::Deque => "praxis_deque_get",
            _ => "praxis_vec_get",
        },
        _ => "praxis_vec_get",
    }
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

/// Map a source `BinOp` to its Float equivalent (§4.12). `%` has no float form
/// (it is a type error in inference); the defensive fallback is `Add`.
fn binop_to_float(op: BinOp) -> FloatBinOp {
    match op {
        BinOp::Add => FloatBinOp::Add,
        BinOp::Sub => FloatBinOp::Sub,
        BinOp::Mul => FloatBinOp::Mul,
        BinOp::Div => FloatBinOp::Div,
        _ => FloatBinOp::Add,
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
        | TypedExpr::For { ty, .. }
        | TypedExpr::Loop { ty, .. }
        | TypedExpr::Break { ty, .. }
        | TypedExpr::Continue { ty, .. }
        | TypedExpr::Return { ty, .. }
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
    let dst = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
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
    let dst = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
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
    let dst = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
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
    let result = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
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
    let unit_val = lower_lit_gc(b, &Lit::Unit, None);
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
            let lit_gc = lower_lit_gc(b, value, None);
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
                Lit::Float(_) => {
                    // Compare two Float scalars for equality with IEEE-754
                    // semantics (NaN never matches, matching `==` on values).
                    let sf = lower_extract_float(b, scrut);
                    let lf = lower_extract_float(b, lit_gc);
                    let cmp = b.alloc_scalar(ScalarKind::Bool);
                    b.push(Inst::FloatCmp {
                        op: CmpOp::Eq,
                        dst: cmp,
                        lhs: sf,
                        rhs: lf,
                    });
                    b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                        cond: cmp,
                        then_block: on_success,
                        else_block: on_fail,
                    };
                }
                Lit::Unit => {
                    // Unit patterns aren't produced by the parser today; treat
                    // as a match (defensive). Unit is a singleton, so any Unit
                    // scrutinee equals the (sole) Unit literal.
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
        let payload = b.alloc_gc(Type(0), None, LocalDebugKind::Temp, None);
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
        assert!(
            parsed.diagnostics.is_empty(),
            "parse diagnostics: {:?}",
            parsed.diagnostics
        );
        let mut analysis = analyze_root(file, &parsed.tree);
        assert!(
            analysis.diagnostics.is_empty(),
            "analysis diagnostics: {:?}",
            analysis.diagnostics
        );
        let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
        let module = lower(file, &root, &mut analysis);
        assert!(
            module.diagnostics.is_empty(),
            "lowering diagnostics: {:?}",
            module.diagnostics
        );
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

    #[test]
    fn lowers_unit_literal_to_alloc_unit() {
        // A `Unit`-returning `main` with an empty body synthesizes a `Lit::Unit`
        // tail. That tail must lower to an `Alloc { AllocKind::Unit }` whose
        // destination slot carries the Unit type — not an `Int(0)` masquerading
        // as Unit. This is the MIR-side guard for the type-lie fix: a
        // `Unit`-typed expression holds a genuine Unit value.
        let (funcs, analysis) = lower_src_to_mir("fn main() -> Unit { let x = 1 }");
        let f = &funcs[0];

        // Find an Alloc-Unit instruction and inspect its destination slot.
        let alloc_unit = f.blocks.iter().find_map(|b| {
            b.insts.iter().find_map(|i| match i {
                Inst::Alloc {
                    dst,
                    alloc: AllocKind::Unit,
                    ..
                } => Some(*dst),
                _ => None,
            })
        });
        let dst = alloc_unit.expect("a Unit-returning body should emit an AllocKind::Unit");

        // The destination slot must be a Gc local typed Unit (not Int(0):Unit).
        // `TypeData` isn't `PartialEq`, so compare via `matches!` on the
        // resolved representative (canonical pattern used elsewhere in the crate).
        let slot = &f.locals[dst.0 as usize];
        assert_eq!(slot.kind, LocalKind::Gc, "Unit value lives in a Gc slot");
        assert!(
            matches!(
                analysis.db.data(analysis.db.follow(slot.ty)),
                praxis_types::TypeData::Unit
            ),
            "the Unit value's slot must carry the Unit type"
        );
    }

    #[test]
    #[ignore = "known bug: closure capture indices are moved through rootable Gc locals"]
    fn closure_capture_indices_never_flow_through_gc_locals() {
        // Runtime ABI indices are raw integers, not GcRefs. Moving a scalar
        // capture index into a `LocalKind::Gc` slot makes an illegal state
        // representable: liveness can then spill e.g. integer `1` as if it
        // were a heap pointer, and a later collection will dereference 0x1.
        let (funcs, _analysis) = lower_src_to_mir(
            "fn main() -> Int {\n  let a = 10\n  let b = 20\n  let f = |x| x + a + b\n  f(12)\n}\n",
        );

        let bad_moves: Vec<(&str, LocalId, LocalId)> = funcs
            .iter()
            .flat_map(|f| {
                f.blocks.iter().flat_map(move |block| {
                    block.insts.iter().filter_map(move |inst| match inst {
                        Inst::MoveGc { dst, src }
                            if f.locals[dst.0 as usize].kind == LocalKind::Gc
                                && matches!(
                                    f.locals[src.0 as usize].kind,
                                    LocalKind::Scalar(_)
                                ) =>
                        {
                            Some((f.name.as_str(), *dst, *src))
                        }
                        _ => None,
                    })
                })
            })
            .collect();

        assert!(
            bad_moves.is_empty(),
            "raw scalar values must never inhabit GC-rootable locals: {bad_moves:?}"
        );
    }

    #[test]
    #[ignore = "known bug: dynamic take arguments fall through to a Unit intrinsic stub"]
    fn dynamic_take_argument_does_not_silently_lower_to_unit() {
        // `take` is typed to accept any Int expression, not literals only. The
        // intrinsic fallback must preserve that contract instead of returning
        // Unit and letting the outer pipeline reinterpret Unit as a Vec.
        let (funcs, _analysis) = lower_src_to_mir(
            "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  let n = 2\n  v.take(n).sum()\n}\n",
        );
        let main = funcs.iter().find(|f| f.name == "main").expect("main MIR");
        let unit_fallbacks: Vec<_> = main
            .blocks
            .iter()
            .flat_map(|block| &block.insts)
            .filter(|inst| {
                matches!(
                    inst,
                    Inst::Alloc {
                        alloc: AllocKind::Unit,
                        ..
                    }
                )
            })
            .collect();

        assert!(
            unit_fallbacks.is_empty(),
            "a well-typed `take(n)` pipeline must not lower through a Unit fallback"
        );
    }

    #[test]
    #[ignore = "known bug: dynamic skip arguments fall through to a Unit intrinsic stub"]
    fn dynamic_skip_argument_does_not_silently_lower_to_unit() {
        let (funcs, _analysis) = lower_src_to_mir(
            "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  let n = 2\n  v.skip(n).sum()\n}\n",
        );
        let main = funcs.iter().find(|f| f.name == "main").expect("main MIR");
        let unit_fallbacks: Vec<_> = main
            .blocks
            .iter()
            .flat_map(|block| &block.insts)
            .filter(|inst| {
                matches!(
                    inst,
                    Inst::Alloc {
                        alloc: AllocKind::Unit,
                        ..
                    }
                )
            })
            .collect();

        assert!(
            unit_fallbacks.is_empty(),
            "a well-typed `skip(n)` pipeline must not lower through a Unit fallback"
        );
    }

    #[test]
    #[ignore = "known bug: call results use Type(0) as an untyped sentinel"]
    fn call_result_locals_retain_their_inferred_static_types() {
        let (funcs, analysis) = lower_src_to_mir(
            "fn id(n: Int) -> Int { n }\nfn main() -> Int {\n  let v = Vec()\n  let n = v.len()\n  id(n)\n}\n",
        );
        let main = funcs.iter().find(|f| f.name == "main").expect("main MIR");
        let call_results: Vec<_> = main
            .blocks
            .iter()
            .flat_map(|block| &block.insts)
            .filter_map(|inst| match inst {
                Inst::Call { dst, callee, .. } => Some((*dst, callee)),
                _ => None,
            })
            .collect();
        assert!(!call_results.is_empty(), "expected len and id calls");
        for (dst, callee) in call_results {
            assert!(
                matches!(
                    analysis
                        .db
                        .data(analysis.db.follow(main.locals[dst.0 as usize].ty)),
                    praxis_types::TypeData::Scalar(praxis_types::ScalarType::Int)
                ),
                "the {callee:?} result is statically Int but local {dst:?} carries {:?}",
                main.locals[dst.0 as usize].ty
            );
        }
    }

    #[test]
    #[ignore = "known bug: fused pipeline Vec/Unit result slots are typed as Int"]
    fn pipeline_runtime_call_destinations_retain_vec_and_unit_types() {
        let (funcs, analysis) =
            lower_src_to_mir("fn main() {\n  let v = Vec()\n  v.push(1)\n  v.map(|x| x)\n}\n");
        let main = funcs.iter().find(|f| f.name == "main").expect("main MIR");

        for inst in main.blocks.iter().flat_map(|block| &block.insts) {
            let Inst::Call {
                dst,
                callee: CallTarget::Runtime(name),
                ..
            } = inst
            else {
                continue;
            };
            let local_ty = main.locals[dst.0 as usize].ty;
            match name.as_str() {
                "praxis_vec_new" => assert!(
                    matches!(
                        analysis.db.data(analysis.db.follow(local_ty)),
                        praxis_types::TypeData::Collection {
                            ctor: praxis_types::CollectionCtor::Vec,
                            ..
                        }
                    ),
                    "praxis_vec_new must define a Vec-typed local, got {local_ty:?}"
                ),
                "praxis_vec_push" => assert!(
                    matches!(
                        analysis.db.data(analysis.db.follow(local_ty)),
                        praxis_types::TypeData::Unit
                    ),
                    "praxis_vec_push must define a Unit-typed local, got {local_ty:?}"
                ),
                _ => {}
            }
        }
    }

    #[test]
    fn for_continue_targets_the_increment_block_not_the_header() {
        let (funcs, _analysis) = lower_src_to_mir(
            "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  for x in v { continue }\n  0\n}\n",
        );
        let main = funcs.iter().find(|f| f.name == "main").expect("main MIR");
        let (header, body) = main
            .blocks
            .iter()
            .find_map(|block| {
                let has_len_call = block.insts.iter().any(|inst| {
                    matches!(
                        inst,
                        Inst::Call {
                            callee: CallTarget::Runtime(name),
                            ..
                        } if name == "praxis_vec_len"
                    )
                });
                match (&block.term, has_len_call) {
                    (Terminator::Branch { then_block, .. }, true) => Some((block.id, *then_block)),
                    _ => None,
                }
            })
            .expect("for-loop header and body");
        let Terminator::Jump { target } = &main.blocks[body.0 as usize].term else {
            panic!("continue should terminate the for body with a jump");
        };
        assert_ne!(
            *target, header,
            "jumping straight to the header leaves the index unchanged and loops forever"
        );
        assert!(
            main.blocks[target.0 as usize].insts.iter().any(|inst| {
                matches!(
                    inst,
                    Inst::Materialize {
                        scalar: ScalarKind::Int,
                        ..
                    }
                )
            }),
            "the continue target must increment and re-materialize the loop index"
        );
    }

    #[test]
    #[ignore = "known bug: enumerate emits an opaque Type(0) tuple schema"]
    fn enumerate_tuple_allocation_carries_a_real_two_element_type() {
        // Codegen builds TupleSchema from AllocKind::Tuple.ty. Supplying the
        // opaque Type(0) does not make codegen infer a schema from runtime
        // values; it creates a zero-field tuple and all tuple_set calls become
        // no-ops. Assert the MIR/codegen boundary carries the actual shape.
        let (funcs, analysis) =
            lower_src_to_mir("fn main() {\n  let v = Vec()\n  v.push(10)\n  v.enumerate()\n}\n");
        let main = funcs.iter().find(|f| f.name == "main").expect("main MIR");
        let tuple_ty = main
            .blocks
            .iter()
            .flat_map(|block| &block.insts)
            .find_map(|inst| match inst {
                Inst::Alloc {
                    alloc: AllocKind::Tuple { ty, .. },
                    ..
                } => Some(*ty),
                _ => None,
            })
            .expect("enumerate should allocate an (index, item) tuple");

        assert!(
            matches!(
                analysis.db.data(analysis.db.follow(tuple_ty)),
                praxis_types::TypeData::Tuple(elements) if elements.len() == 2
            ),
            "enumerate must carry a two-element tuple type into codegen, got {tuple_ty:?}"
        );
    }

    #[test]
    #[ignore = "known bug: zip emits an opaque Type(0) tuple schema"]
    fn zip_tuple_allocation_carries_a_real_two_element_type() {
        let (funcs, analysis) = lower_src_to_mir(
            "fn main() {\n  let lhs = Vec()\n  lhs.push(10)\n  let rhs = Vec()\n  rhs.push(20)\n  lhs.zip(rhs)\n}\n",
        );
        let main = funcs.iter().find(|f| f.name == "main").expect("main MIR");
        let tuple_ty = main
            .blocks
            .iter()
            .flat_map(|block| &block.insts)
            .find_map(|inst| match inst {
                Inst::Alloc {
                    alloc: AllocKind::Tuple { ty, .. },
                    ..
                } => Some(*ty),
                _ => None,
            })
            .expect("zip should allocate an (left, right) tuple");

        assert!(
            matches!(
                analysis.db.data(analysis.db.follow(tuple_ty)),
                praxis_types::TypeData::Tuple(elements) if elements.len() == 2
            ),
            "zip must carry a two-element tuple type into codegen, got {tuple_ty:?}"
        );
    }
}
