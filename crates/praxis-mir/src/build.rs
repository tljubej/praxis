//! HIR → MIR lowering (ADR-015).
//!
//! Walks a [`TypedModule`](praxis_hir::TypedModule) and emits one MIR
//! [`Function`] per source `fn`. Every language value is materialized as a
//! `GcRef` in a `Gc` slot; scalar payloads (`i64` out of an `Int`) live in
//! transient `Scalar` slots only between extraction and the next materialization,
//! never across a safepoint.
//!
//! The emitted MIR is **not** run through liveness here; callers invoke
//! [`crate::annotate`] to populate each safepoint's [`RootSlots`]/[`DebugSlots`].
//! Keeping the two phases separate makes the builder easier to test in
//! isolation — and the sets are *sealed*, so a builder site can only write
//! `unannotated()`. It used to write 61 hand-maintained root lists that the
//! pass then silently overwrote; several disagreed with it, and none of them
//! could be wrong in a way anything noticed.

#![allow(dead_code)] // Consumed by the Cranelift backend (Phase 4).

use praxis_hir::{
    capture::Capture, AssignOp, BinOp, Lit, TypedBlock, TypedExpr, TypedFn, TypedItem, TypedModule,
    TypedParam, TypedStmt, UnaryOp,
};
use praxis_stdlib::abi::RuntimeSymbol;
use praxis_types::{Type, TypeDb};

use crate::annot::{DebugSlots, RootSlots};
use crate::ir::{
    AllocKind, BlockId, CallTarget, CmpOp, FloatBinOp, Function, GcConst, Inst, IntBinOp,
    LocalDebugKind, LocalId, LocalKind, MirType, Overflow, ScalarKind, Terminator,
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
    // …and one adapter per *function value* (REP-01, ADR-061). Deduplicated by
    // name: every `let f = double` in the module shares `double`'s one adapter.
    let mut adapted: Vec<FnValueAdapter> = Vec::new();
    for item in &module.items {
        let TypedItem::Fn(tfn) = item;
        for adapter in collect_fn_values(&tfn.body) {
            if !adapted.iter().any(|a| a.target == adapter.target) {
                adapted.push(adapter);
            }
        }
    }
    for adapter in &adapted {
        funcs.push(lower_fn_value_adapter(adapter, db));
    }
    funcs
}

/// One top-level `fn` used as a value, and the adapter it needs (REP-01,
/// ADR-061).
///
/// A closure's synthetic function takes the closure itself as a hidden first
/// explicit argument — `fn(ctx, closure_self, args…)` — and a top-level `fn`
/// does not: it is `fn(ctx, args…)`. So a `fn`'s address cannot be handed to
/// `praxis_alloc_closure` directly, however empty the environment is; every
/// argument would land one slot to the left. The adapter is the one-instruction
/// function that bridges the two conventions.
struct FnValueAdapter {
    /// The user function being adapted, by name.
    target: String,
    /// Its type at the use site — a `Func`, whose parameter count is the
    /// adapter's arity and whose result is what it returns.
    fn_ty: Type,
}

impl FnValueAdapter {
    /// The adapter's MIR/symbol name. One per target, so two uses of the same
    /// function share it. Shaped like the other synthetic names (`__closure_0`,
    /// `__p_expr`) so it cannot be confused with a user function in a backtrace.
    fn name(target: &str) -> String {
        format!("__fnvalue_{target}")
    }
}

/// A closure literal lifted out of a body for synthetic-function emission.
/// Carries the pieces of `TypedExpr::Closure` needed by `lower_closure_fn`.
struct LiftedClosure {
    fn_name: String,
    params: Vec<TypedParam>,
    body: TypedBlock,
    captures: Vec<Capture>,
    /// The closure literal's own type — the type of the hidden `closure_self`
    /// parameter the synthetic function receives.
    self_ty: Type,
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
    // The sub-expression list is `praxis-hir`'s, for the reason the *child* list
    // is F20's: a statement field this walk forgot is a synthetic function that
    // never gets emitted.
    for e in praxis_hir::stmt_exprs(stmt) {
        collect_closures_expr(e, out);
    }
}

fn collect_closures_expr(e: &TypedExpr, out: &mut Vec<LiftedClosure>) {
    // Recurse first, so an inner closure is emitted before the outer one
    // (deterministic ordering for tests). The child list is F20's, written once
    // in `praxis-hir`: this used to be a 29-arm match of its own, and a closure
    // sitting in a field it forgot was a synthetic function never emitted.
    for child in e.children() {
        collect_closures_expr(child, out);
    }
    for block in e.blocks() {
        collect_closures_block(block, out);
    }
    if let TypedExpr::Closure {
        fn_name,
        params,
        body,
        captures,
        ty,
        ..
    } = e
    {
        out.push(LiftedClosure {
            fn_name: fn_name.clone(),
            params: params.clone(),
            body: (**body).clone(),
            captures: captures.clone(),
            self_ty: *ty,
        });
    }
}

/// Every `TypedExpr::FnValue` in a body, in source order (REP-01).
///
/// The same walk as [`collect_closures`] and for the same reason: the child list
/// is F20's, so a function value in a field this forgot cannot happen.
fn collect_fn_values(block: &TypedBlock) -> Vec<FnValueAdapter> {
    let mut out = Vec::new();
    collect_fn_values_block(block, &mut out);
    out
}

fn collect_fn_values_block(block: &TypedBlock, out: &mut Vec<FnValueAdapter>) {
    for stmt in &block.stmts {
        for e in praxis_hir::stmt_exprs(stmt) {
            collect_fn_values_expr(e, out);
        }
    }
    collect_fn_values_expr(&block.tail, out);
}

fn collect_fn_values_expr(e: &TypedExpr, out: &mut Vec<FnValueAdapter>) {
    for child in e.children() {
        collect_fn_values_expr(child, out);
    }
    for block in e.blocks() {
        collect_fn_values_block(block, out);
    }
    if let TypedExpr::FnValue {
        callee_name, ty, ..
    } = e
    {
        out.push(FnValueAdapter {
            target: callee_name.clone(),
            fn_ty: *ty,
        });
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
    /// The slot holding the loop's value, for a `loop` that produces one
    /// (TY-21): every `break` writes it before jumping, and the exit block
    /// reads it. `None` for a `while`/`for` and for the fused pipeline loops,
    /// which are `Unit`-valued and whose `break`s therefore carry nothing.
    result: Option<LocalId>,
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
            MirType::Known(p.ty),
            Some(p.name.clone()),
            LocalDebugKind::User,
            Some(f.span),
        );
        b.locals.insert(p.symbol, id);
        b.func.params.push(id);
    }

    // The return slot. A compiler temp; span-less.
    let ret = b.alloc_gc(
        MirType::Known(f.return_type),
        None,
        LocalDebugKind::Temp,
        None,
    );
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
        MirType::Known(closure.self_ty),
        Some("__closure_self".to_string()),
        LocalDebugKind::Temp,
        None,
    );
    b.func.params.push(self_local);

    // User params: one `Gc` slot each, after `self_local`.
    for p in &closure.params {
        let id = b.alloc_gc(
            MirType::Known(p.ty),
            Some(p.name.clone()),
            LocalDebugKind::User,
            None,
        );
        b.locals.insert(p.symbol, id);
        b.func.params.push(id);
    }

    // Prologue: load each captured value from the closure's env and bind it to
    // the capture's symbol. The capture index is an ABI immediate on
    // `Inst::LoadCapture`, not a value: boxing it as `ConstInt` + `MoveGc` into
    // a `Gc` slot (as this did before P0-03) put a small integer in a slot the
    // liveness pass may spill into the shadow stack, and the collector would
    // then dereference `0x1` as a `GcRef`.
    for (idx, cap) in closure.captures.iter().enumerate() {
        let dst = b.alloc_gc(
            MirType::Known(cap.ty),
            Some(cap.name.clone()),
            LocalDebugKind::User,
            None,
        );
        b.push(Inst::LoadCapture {
            dst,
            closure: self_local,
            index: idx as u32,
        });
        // No fault check: `praxis_closure_capture` is `Effect::Pure` — it reads
        // one env slot and can neither allocate nor set a fault.
        b.locals.insert(cap.symbol, dst);
    }

    // The return slot.
    let ret = b.alloc_gc(
        MirType::Known(closure.body.ty),
        None,
        LocalDebugKind::Temp,
        None,
    );
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

/// The adapter function for a top-level `fn` used as a value (REP-01, ADR-061).
///
/// Its params are `[closure_self, p0 … pn]` — the closure convention — and its
/// body is one direct call to the target with `[p0 … pn]`, dropping the self
/// slot the target has no parameter for. `n` is the arity of the `Func` type at
/// the use site, which inference has already checked against the declaration, so
/// the adapter cannot disagree with either side about how many arguments there
/// are.
///
/// It is what "reuse `praxis_alloc_closure` with an empty environment" actually
/// requires: the env is empty (a top-level `fn` captures nothing) but the *call*
/// convention still has the extra hidden argument, so something has to absorb
/// it. Nothing else about the closure path changes — the allocation, the fn_ptr
/// read, the `call_indirect`, the rooting are all the existing ones.
fn lower_fn_value_adapter(adapter: &FnValueAdapter, db: &mut TypeDb) -> Function {
    let int_ty = db.int();
    let float_ty = db.float();
    let bool_ty = db.bool();
    let text_ty = db.text();
    let char_ty = db.char();
    let unit_ty = db.unit();

    // The parameter and result types are read off the `Func`. A use site whose
    // type is not a `Func` cannot occur — inference gives a `fn` name its
    // declared function type — but the fallback is a nullary adapter returning
    // `Unit`, which is a well-formed function rather than a malformed one.
    let (param_tys, result_ty) = match db.data(db.follow(adapter.fn_ty)) {
        praxis_types::TypeData::Func { params, result } => (params.clone(), *result),
        _ => (Vec::new(), unit_ty),
    };

    let mut func = Function {
        name: FnValueAdapter::name(&adapter.target),
        params: Vec::new(),
        return_local: LocalId(0),
        locals: Vec::new(),
        blocks: Vec::new(),
        debug_names: Vec::new(),
        debug_kinds: Vec::new(),
        debug_spans: Vec::new(),
        // Synthetic, like a lifted closure: no source span of its own.
        span: (0, 0),
    };
    let entry = func.new_block();
    let fault = func.new_block();
    func.blocks[fault.0 as usize].term = Terminator::Fault;

    let escaping = std::collections::HashSet::new();
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
        escaping_vars: &escaping,
        loop_stack: Vec::new(),
    };

    // Param 0: the closure value. Unused — the environment is empty — but it is
    // the ABI slot every `call_indirect` through a closure passes, and dropping
    // it here is the whole point of the adapter.
    let self_local = b.alloc_gc(
        MirType::Known(adapter.fn_ty),
        Some("__closure_self".to_string()),
        LocalDebugKind::Temp,
        None,
    );
    b.func.params.push(self_local);

    // The forwarded parameters, in order. No symbol binding: nothing in this
    // body names them, so they exist only as ABI slots.
    let forwarded: Vec<LocalId> = param_tys
        .iter()
        .map(|ty| {
            let id = b.alloc_gc(MirType::Known(*ty), None, LocalDebugKind::Temp, None);
            b.func.params.push(id);
            id
        })
        .collect();

    let ret = b.alloc_gc(MirType::Known(result_ty), None, LocalDebugKind::Temp, None);
    b.func.return_local = ret;
    // The target may fault, and the adapter is on the fault path's way out: a
    // fault raised inside it has to reach this frame's `Terminator::Fault`
    // rather than be carried past as a Unit sentinel. `call_user` always
    // checks, for exactly that reason.
    b.call_user(ret, adapter.target.clone(), forwarded);
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
        ty: MirType,
        debug_name: Option<String>,
        debug_kind: LocalDebugKind,
        debug_span: Option<(u32, u32)>,
    ) -> LocalId {
        self.func
            .new_local(LocalKind::Gc, ty, debug_name, debug_kind, debug_span)
    }

    /// Allocate a `Gc` local for a compiler temporary materializing `expr`'s
    /// span. Convenience for the many lowering sites that hold a `&TypedExpr`.
    fn alloc_temp(&mut self, ty: MirType, expr: &TypedExpr) -> LocalId {
        self.alloc_gc(
            ty,
            None,
            LocalDebugKind::Temp,
            Some(praxis_hir::expr_span(expr)),
        )
    }

    fn alloc_scalar(&mut self, sk: ScalarKind) -> LocalId {
        // A scalar slot has no language type: its `ScalarKind` is authoritative
        // and the backend never emits debugger metadata for it. `Opaque` says
        // that, where the old placeholder `Type(0)` claimed to be whatever type
        // the arena interned first.
        self.func.new_local(
            LocalKind::Scalar(sk),
            MirType::Opaque,
            None,
            LocalDebugKind::Temp,
            None,
        )
    }

    fn push(&mut self, inst: Inst) {
        self.func.blocks[self.cur.0 as usize].insts.push(inst);
    }

    /// Push an instruction and, iff it can fault, the check that observes it.
    ///
    /// **The one place lowering decides whether a fault check is emitted**
    /// (MIR-10, ADR-088). Before this the decision was made ~34 times, once per
    /// emit site, and about half of them were wrong: the fused `collect` sink
    /// pushed with no check (REP-52) while every method call checked whether or
    /// not its wrapper could fault (REP-53). [`Inst::can_fault`] derives the
    /// answer from the ABI manifest through the same instruction→symbol mapping
    /// the backend uses, and [`crate::verify`] rejects a body that disagrees in
    /// either direction — so a site that stops going through here fails the
    /// build rather than going quiet.
    ///
    /// The sites that still call [`Builder::check_fault`] by hand are the ones
    /// whose instruction is not built here (`Inst::IntBinOp`, `Inst::ValueCmp`),
    /// and they are checked by the same rule.
    fn emit(&mut self, inst: Inst) {
        let faults = inst.can_fault();
        self.push(inst);
        if faults {
            self.check_fault();
        }
    }

    /// Emit a call to a runtime wrapper (and its fault check, if its manifest
    /// row says it can fault).
    fn call_runtime(&mut self, dst: LocalId, sym: RuntimeSymbol, args: Vec<LocalId>) {
        self.emit(Inst::Call {
            dst,
            callee: CallTarget::Runtime(sym),
            args,
            roots: RootSlots::unannotated(),
            debug: DebugSlots::unannotated(),
        });
    }

    /// Emit a call to a user function. Always checked: a callee's body may
    /// raise any fault and there is no manifest row for a Praxis function.
    fn call_user(&mut self, dst: LocalId, name: String, args: Vec<LocalId>) {
        self.emit(Inst::Call {
            dst,
            callee: CallTarget::User(name),
            args,
            roots: RootSlots::unannotated(),
            debug: DebugSlots::unannotated(),
        });
    }

    /// Emit an indirect call through a closure value. Always checked, for
    /// [`Builder::call_user`]'s reason.
    fn call_indirect(&mut self, dst: LocalId, callee: LocalId, args: Vec<LocalId>) {
        self.emit(Inst::CallIndirect {
            dst,
            callee,
            args,
            roots: RootSlots::unannotated(),
            debug: DebugSlots::unannotated(),
        });
    }

    /// Emit an allocation (and its fault check, if any wrapper it reaches can
    /// fault — `praxis_alloc_text`, `praxis_alloc_char`, `praxis_grid_new`).
    fn alloc(&mut self, dst: LocalId, alloc: AllocKind) {
        self.emit(Inst::Alloc {
            dst,
            alloc,
            roots: RootSlots::unannotated(),
            debug: DebugSlots::unannotated(),
        });
    }

    /// Emit a fault check after a faultable instruction.
    fn check_fault(&mut self) {
        // `debug` is filled by the liveness pass. `CheckFault` carries *only*
        // that set: it is a debugger safepoint (the backend spills these into
        // the debug frame so a snapshot on the fault path sees current values)
        // and not a GC one — it allocates nothing, so it roots nothing.
        self.push(Inst::CheckFault {
            on_fault: self.fault_block,
            debug: DebugSlots::unannotated(),
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
                MirType::Known(expr_static_type(init)),
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
                    MirType::Opaque,
                    Some(format!("__cell_{name}")),
                    LocalDebugKind::User,
                    Some(*span),
                );
                b.call_runtime(cell, RuntimeSymbol::AllocVarCell, vec![v]);
                b.locals.insert(*symbol, cell);
            } else {
                let slot = b.alloc_gc(
                    MirType::Known(expr_static_type(init)),
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
                    b.call_runtime(dst, RuntimeSymbol::VarCellSet, vec![dst, v]);
                } else {
                    b.push(Inst::MoveGc { dst, src: v });
                }
            } else {
                // Compound assignment: `dst = dst <op> value`. Which arithmetic
                // that is follows the operand type, and the answer comes from
                // `arith_kind` — the same one the binary operators ask (REP-64).
                let cur = if escaping {
                    // Read the cell's current value.
                    let cur = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, Some(*span));
                    b.call_runtime(cur, RuntimeSymbol::VarCellGet, vec![dst]);
                    cur
                } else {
                    let cur = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, Some(*span));
                    b.push(Inst::MoveGc { dst: cur, src: dst });
                    cur
                };
                let rhs = lower_expr_gc(b, value);
                let operand_ty = expr_static_type(value);
                let Some(materialized) =
                    lower_compound_arith(b, *op, cur, rhs, operand_ty, Some(*span))
                else {
                    return;
                };
                if escaping {
                    b.call_runtime(dst, RuntimeSymbol::VarCellSet, vec![dst, materialized]);
                } else {
                    b.push(Inst::MoveGc {
                        dst,
                        src: materialized,
                    });
                }
            }
        }
        // `m[key] = v`, `counts[key] += 1` — a store through a subscript
        // (REP-16). The receiver and the indices are lowered **once**, into
        // locals reused by the read and the write, so a compound operator
        // evaluates its place exactly once: `m[f()] += 1` calls `f` once.
        //
        // Which wrappers to call is HIR's answer (`get`/`set`, from the catalog
        // rows inference resolved), not one re-derived here from the receiver's
        // static ctor — the mistake `get_symbol_for` makes for `for` and REP-15
        // is about.
        TypedStmt::IndexAssign {
            receiver,
            indices,
            get,
            set,
            op,
            value,
            span,
        } => {
            let recv = lower_expr_gc(b, receiver);
            let index_locals: Vec<LocalId> = indices.iter().map(|i| lower_expr_gc(b, i)).collect();
            let stored = if *op == AssignOp::Assign {
                lower_expr_gc(b, value)
            } else {
                // Read-modify-write. `get` is `Some` for every compound operator
                // (HIR drops the statement otherwise), and the arithmetic is the
                // same operation, under the same restriction, that a compound
                // assignment to a local has — which is why both go through
                // `lower_compound_arith` rather than each choosing (REP-64).
                let Some(get) = *get else { return };
                let cur = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, Some(*span));
                let mut args = Vec::with_capacity(index_locals.len() + 1);
                args.push(recv);
                args.extend(index_locals.iter().copied());
                // Which of these faults is the *symbol's* answer, not the
                // shape's: `VecGet`/`MapIndex`/`GridGet`/`TextGet` do,
                // `CounterGet` does not (a missing key counts zero).
                b.call_runtime(cur, get, args);
                let rhs = lower_expr_gc(b, value);
                let operand_ty = expr_static_type(value);
                let Some(stored) = lower_compound_arith(b, *op, cur, rhs, operand_ty, Some(*span))
                else {
                    return;
                };
                stored
            };
            // The store's arguments are the receiver, the indices, then the value
            // — the catalog row's parameter order.
            let dst = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, Some(*span));
            let mut args = Vec::with_capacity(index_locals.len() + 2);
            args.push(recv);
            args.extend(index_locals);
            args.push(stored);
            b.call_runtime(dst, *set, args);
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
                        let value = b.alloc_temp(MirType::Known(*ty), e);
                        b.call_runtime(value, RuntimeSymbol::VarCellGet, vec![slot]);
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
        // A top-level `fn` used as a value (REP-01, ADR-061): a closure over its
        // adapter with an empty environment. This arm is what a `Path` to a `fn`
        // used to fall through to the `None` above and answer `Unit` for — and
        // `Inst::CallIndirect` then read the Unit's payload as a function
        // pointer, which is a SIGBUS from a program `praxis check` accepted.
        TypedExpr::FnValue {
            callee_name, ty, ..
        } => {
            let dst = b.alloc_gc(MirType::Known(*ty), None, LocalDebugKind::Temp, espan);
            b.alloc(
                dst,
                AllocKind::Closure {
                    fn_name: FnValueAdapter::name(callee_name),
                    captures: Vec::new(),
                },
            );
            dst
        }
        // `a..b` / `a..=b` (§4.11, ADR-059). One runtime call per form: the
        // inclusiveness is a *symbol*, not a flag, because the choice is a
        // syntactic fact the builder already holds and a boolean threaded through
        // an `i64` parameter would have 2^64 spellings for two states.
        TypedExpr::Range {
            start,
            end,
            inclusive,
            ty,
            ..
        } => {
            let lo = lower_expr_gc(b, start);
            let hi = lower_expr_gc(b, end);
            let sym = if *inclusive {
                RuntimeSymbol::RangeNewInclusive
            } else {
                RuntimeSymbol::RangeNew
            };
            let dst = b.alloc_gc(MirType::Known(*ty), None, LocalDebugKind::Temp, espan);
            // Neither form faults: a descending range is empty rather than an
            // error, and `..=Int::MAX` saturates (ADR-059 D3). Both rows say
            // `Effect::Allocates`, so `call_runtime` emits no check.
            b.call_runtime(dst, sym, vec![lo, hi]);
            dst
        }
        TypedExpr::Bin {
            op, lhs, rhs, ty, ..
        } => {
            // Short-circuit ops must not eagerly evaluate `rhs` — it is lowered
            // only on the path that needs it, inside `lower_short_circuit`.
            if let BinOp::LogicalOr | BinOp::LogicalAnd = op {
                let l = lower_expr_gc(b, lhs);
                let skip_on = *op == BinOp::LogicalOr;
                return lower_short_circuit(b, l, rhs, skip_on);
            }
            let l = lower_expr_gc(b, lhs);
            let r = lower_expr_gc(b, rhs);
            match op {
                // Arithmetic: extract scalars, do the op, materialize. Int ops
                // are checked (fault on overflow/div-by-zero); Float ops are
                // unchecked (IEEE-754 inf/nan), so no fault check follows.
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                    // Which arithmetic, from the one place that answers that —
                    // the same `arith_kind` the compound-assignment paths ask
                    // (REP-64), so the two cannot drift apart again.
                    let via = arith_kind(b, expr_static_type(lhs));
                    if via == ArithVia::Text {
                        // `+` on `Text` is concatenation (ADR-085), and it is a
                        // runtime call rather than a scalar operation: a `Text`
                        // payload is a pointer-and-length structure, not a
                        // number, so the `lower_int_binop` path below would add
                        // two *pointers*. The other four operators are `Y016` in
                        // inference and cannot reach here — but `Add` is asked
                        // for explicitly rather than assumed, so a subtree that
                        // did reach here malformed builds nothing instead of
                        // silently concatenating.
                        if *op == BinOp::Add {
                            let dst =
                                b.alloc_gc(MirType::Known(*ty), None, LocalDebugKind::Temp, espan);
                            // `Allocates`, not `AllocatesAndFaults`: concatenating
                            // two UTF-8 payloads cannot produce anything else, so
                            // there is no fault to check for.
                            b.call_runtime(dst, RuntimeSymbol::TextConcat, vec![l, r]);
                            dst
                        } else {
                            b.alloc_gc(MirType::Known(*ty), None, LocalDebugKind::Temp, espan)
                        }
                    } else if via == ArithVia::Float {
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
                    // How the operands are compared follows their type, and the
                    // choice is the whole of P0-12's compiler half. `==`/`!=`
                    // on a composite GC value (record/tuple/enum/collection) is
                    // a structural-equality runtime call; a `Text` of either
                    // form goes through the descriptor (its payload is a
                    // pointer-and-length structure, not a number); every other
                    // scalar is compared natively *at its own width* — a `Char`
                    // payload is four bytes and a `Bool` one, so the old
                    // uniform `Int` extraction read past both.
                    let operand_ty = b.db.follow(expr_static_type(lhs));
                    let compare_as = compare_kind(b, operand_ty);
                    let is_equality = matches!(op, BinOp::Eq | BinOp::Neq);
                    match compare_as {
                        CompareVia::Descriptor if is_equality => {
                            // Structural equality via praxis_struct_eq(ctx, a, b) -> 0/1,
                            // which dispatches to the descriptor's `equals`.
                            // `!=` is `!(==)`.
                            let eq_bool = lower_struct_eq(b, l, r);
                            if *op == BinOp::Neq {
                                lower_logical_not(b, eq_bool)
                            } else {
                                eq_bool
                            }
                        }
                        // Ordering through the descriptor's `compare`, as
                        // `praxis_value_cmp(a, b) <op> 0`. `Text` is the type
                        // this exists for; a composite reaching here is a
                        // compiler bug (the type checker rejects it with
                        // `Y006`) and faults rather than reinterpreting a
                        // payload.
                        CompareVia::Descriptor => {
                            let ord = lower_value_cmp(b, l, r);
                            let zero = b.alloc_scalar(ScalarKind::Int);
                            b.push(Inst::ConstInt {
                                dst: zero,
                                value: 0,
                            });
                            let bool_scalar = b.alloc_scalar(ScalarKind::Bool);
                            b.push(Inst::IntCmp {
                                op: binop_to_cmp(*op),
                                dst: bool_scalar,
                                lhs: ord,
                                rhs: zero,
                            });
                            lower_materialize_bool(b, bool_scalar, espan)
                        }
                        CompareVia::Float => {
                            // IEEE-754 comparison via FloatCmp (NaN-aware).
                            let lf = lower_extract_float(b, l);
                            let rf = lower_extract_float(b, r);
                            let bool_scalar = b.alloc_scalar(ScalarKind::Bool);
                            b.push(Inst::FloatCmp {
                                op: binop_to_cmp(*op),
                                dst: bool_scalar,
                                lhs: lf,
                                rhs: rf,
                            });
                            lower_materialize_bool(b, bool_scalar, espan)
                        }
                        CompareVia::Scalar(kind) => {
                            let li = lower_extract_scalar(b, l, kind);
                            let ri = lower_extract_scalar(b, r, kind);
                            let bool_scalar = b.alloc_scalar(ScalarKind::Bool);
                            b.push(Inst::IntCmp {
                                op: binop_to_cmp(*op),
                                dst: bool_scalar,
                                lhs: li,
                                rhs: ri,
                            });
                            lower_materialize_bool(b, bool_scalar, espan)
                        }
                    }
                }
                // Both short-circuit operators are handled above, before `rhs`
                // is lowered — reaching here would mean it already had been.
                BinOp::LogicalOr | BinOp::LogicalAnd => unreachable!(),
            }
        }
        TypedExpr::Unary { op, operand, .. } => {
            let o = lower_expr_gc(b, operand);
            match op {
                UnaryOp::Neg => {
                    // A Float negation is IEEE-754 `negate` — the sign bit
                    // flipped and nothing else — and **not** `0.0 - x`, which
                    // is what this was: at `x = +0.0` that subtraction answers
                    // `+0.0`, so `-0.0` evaluated to `+0.0` and printed `0.0`,
                    // a rendering that does not read back as the Float it came
                    // from (REP-50; ADR-083 states the rule, ADR-045 already
                    // decided the two zeros are distinct values). An `Int`
                    // negation *is* `0 - x`: it is the checked subtraction, and
                    // faulting at `Int::MIN` is the right answer there.
                    if arith_kind(b, expr_static_type(operand)) == ArithVia::Float {
                        let result = lower_float_neg(b, o);
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
            item_ty,
            ..
        } => {
            lower_for(b, binding, iter, body, *item_ty);
            lower_lit_gc(b, &Lit::Unit, espan) // for yields Unit
        }
        // A `loop` yields what its `break`s carry (TY-21).
        TypedExpr::Loop { body, ty, .. } => lower_loop(b, body, *ty, espan),
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
                let dst = b.alloc_gc(MirType::Known(*ty), None, LocalDebugKind::Temp, None);
                b.call_indirect(dst, callee_local, arg_locals);
                return dst;
            }
            // Collection construction: `Vec[T]()`, `Deque[T]()`, etc. (M8 WS1,
            // §11.1/§11.2). The element type is extracted from the call's result
            // type (the collection type) and carried through `AllocKind::Collection`
            // so the codegen resolves the real element descriptor (closing the M7
            // null-descriptor carryover). `out`/`panic` and other builtins fall
            // through to the generic call path below.
            if let Some(alloc) = collection_alloc_kind(b, callee_name, *ty) {
                let dst = b.alloc_gc(MirType::Known(*ty), None, LocalDebugKind::Temp, None);
                // Only `Grid()` faults here: `praxis_grid_new` validates its
                // dimensions. The other eight `praxis_*_new` wrappers are
                // `Effect::Allocates`, and `alloc` reads that rather than
                // checking after all nine.
                b.alloc(dst, alloc);
                return dst;
            }
            // The `out(x)` builtin writes x to stdout via praxis_write_stdout.
            if callee_name == "out" {
                let arg_local = args
                    .first()
                    .map(|a| lower_expr_gc(b, a))
                    .unwrap_or_else(|| lower_lit_gc(b, &Lit::Unit, espan));
                // The call's result temp materializes `e` (the whole call expr),
                // so its type is the call's — which F15 now records at the call
                // site rather than re-instantiating from the callee's scheme.
                let dst = b.alloc_gc(MirType::Known(*ty), None, LocalDebugKind::Temp, espan);
                // `praxis_write_stdout` is `Effect::Pure`: no check.
                b.call_runtime(dst, RuntimeSymbol::WriteStdout, vec![arg_local]);
                return dst;
            }
            // `dbg(x)`, `panic(x)` and `assert(c)` — the rest of §16.1's
            // output/control group. Each is one runtime call with the argument
            // the program wrote; `panic`/`assert` raise a fault, so each is
            // followed by the usual fault check. Before this they fell through
            // to `CallTarget::User`, which typechecked and then failed the
            // compile with "unresolved user function `panic`" (TY-33).
            if let Some(sym) = control_builtin_symbol(callee_name) {
                let arg_local = args
                    .first()
                    .map(|a| lower_expr_gc(b, a))
                    .unwrap_or_else(|| lower_lit_gc(b, &Lit::Unit, espan));
                let dst = b.alloc_gc(MirType::Known(*ty), None, LocalDebugKind::Temp, espan);
                b.call_runtime(dst, sym, vec![arg_local]);
                return dst;
            }
            // The §16.1 numeric helpers: `abs`, `sign`, `min`, `max`, `clamp`,
            // `gcd`, `lcm`. Each is one runtime call taking the operands the
            // program wrote — the arity is the wrapper's, and inference already
            // rejected a call that does not match it, so the argument list is
            // passed through as it stands. Before this they fell through to
            // `CallTarget::User` and failed the compile with "unresolved user
            // function `abs`" (TY-33, ADR-058).
            if let Some(helper) = praxis_stdlib::numeric_helper(callee_name) {
                let arg_locals: Vec<LocalId> = args.iter().map(|a| lower_expr_gc(b, a)).collect();
                let dst = b.alloc_gc(MirType::Known(*ty), None, LocalDebugKind::Temp, espan);
                b.call_runtime(dst, helper.symbol, arg_locals);
                return dst;
            }
            // §6.5's graph helpers: `bfs`, `bfs_distance`, `dfs`, `dijkstra`,
            // `a_star`, `flood_fill`. Each is one runtime call taking the start
            // state and the closures the program wrote — the same one path as
            // the numeric helpers, because every one of them takes only `Gc`
            // operands and returns one. All six allocate and all six can fault
            // (a closure they call may), so each is followed by a fault check.
            // Before this they fell through to `CallTarget::User` and failed the
            // compile with "unresolved user function `bfs`" (TY-33, ADR-060).
            if let Some(helper) = praxis_stdlib::graph_helper(callee_name) {
                let arg_locals: Vec<LocalId> = args.iter().map(|a| lower_expr_gc(b, a)).collect();
                let dst = b.alloc_gc(MirType::Known(*ty), None, LocalDebugKind::Temp, espan);
                b.call_runtime(dst, helper.symbol, arg_locals);
                return dst;
            }
            // Float constants `pi()`/`e()` (§4.12): direct runtime calls that
            // allocate a Float. No arguments; no fault.
            if callee_name == "pi" || callee_name == "e" {
                let sym = if callee_name == "pi" {
                    RuntimeSymbol::FloatPi
                } else {
                    RuntimeSymbol::FloatE
                };
                let dst = b.alloc_gc(MirType::Known(*ty), None, LocalDebugKind::Temp, None);
                b.call_runtime(dst, sym, vec![]);
                return dst;
            }
            let arg_locals: Vec<LocalId> = args.iter().map(|a| lower_expr_gc(b, a)).collect();
            // Indirect call dispatch (M7, §4.10): if the callee resolves to a
            // local binding (a `let`/`var`/`param` holding a closure value), the
            // call is indirect — read the closure's `fn_ptr` and call through it.
            // Top-level `fn`s are never in `b.locals`, so this distinguishes the
            // two soundly.
            if let Some(callee_local) = b.locals.get(callee).copied() {
                let dst = b.alloc_gc(MirType::Known(*ty), None, LocalDebugKind::Temp, espan);
                b.call_indirect(dst, callee_local, arg_locals);
                return dst;
            }
            // The call's result temp materializes `e` (the whole call expr).
            let dst = b.alloc_gc(
                MirType::Known(*ty),
                None,
                LocalDebugKind::Temp,
                Some(praxis_hir::expr_span(e)),
            );
            b.call_user(dst, callee_name.clone(), arg_locals);
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
            // empty (an intrinsic), the pipeline recognizer owns it and fuses
            // the whole chain into one loop (ADR-071).
            //
            // There is no fallback lowering (REP-40). Inference only types this
            // call at all if a catalog row matched it, and every row lowered as
            // an `Intrinsic` is classified by `classify_link`/`classify_sink` —
            // so a decline here is a compiler bug, not a program's, and it says
            // so instead of answering the Unit singleton.
            let Some(symbol) = *lowering_symbol else {
                // Reconstruct the MethodCall node so the recognizer can walk
                // the receiver chain.
                let call = TypedExpr::MethodCall {
                    receiver: receiver.clone(),
                    name: name.clone(),
                    lowering_symbol: *lowering_symbol,
                    args: args.clone(),
                    purity: *purity,
                    ty: *ty,
                    span: praxis_hir::expr_span(e),
                };
                if let Some(plan) = recognize_pipeline(b.db, &call) {
                    return lower_pipeline(b, plan);
                }
                // Two ways to get here, and both are compiler bugs rather than
                // user errors — which is why this is an ICE and not a
                // diagnostic (ADR-093 deleted lowering's `Y110`; a wrong answer
                // or a type error at this point would misattribute a compiler
                // fault to the program).
                //
                // Either the catalog lowers `{name}` as an intrinsic and no
                // `classify_link`/`classify_sink` arm claims it, or inference
                // never resolved the call at all — in which case it reported
                // `Y110` and the front end should have stopped, or the receiver
                // was one no call site pinned and `monomorphize` should have
                // dropped the uncalled polymorphic original before MIR.
                panic!(
                    "internal compiler error: the pipeline recognizer declined \
                     `{name}`, and it carries no runtime symbol. Every intrinsic \
                     row must be classified by `classify_link` or \
                     `classify_sink` (see \
                     `intrinsics_are_all_recognized_so_there_is_no_second_lowering`), \
                     and every unresolved method call must have been reported by \
                     inference and dropped before here (ADR-093)."
                );
            };
            let mut arg_locals: Vec<LocalId> = Vec::with_capacity(args.len() + 1);
            arg_locals.push(lower_expr_gc(b, receiver));
            for a in args {
                arg_locals.push(lower_expr_gc(b, a));
            }
            // The call's result temp materializes `e` (the whole method-call
            // expression) — thread its span so the debugger can show
            // `@ "xs.get(99)"`.
            let dst = b.alloc_gc(
                MirType::Known(*ty),
                None,
                LocalDebugKind::Temp,
                Some(praxis_hir::expr_span(e)),
            );
            // **REP-53.** Some method calls fault (`v.get(i)` out of bounds);
            // most do not. This site used to check after *all* of them, which
            // put a `praxis_check_fault` call and a branch after every
            // `v.len()` — and made `praxis_runtime::abi`'s
            // `panic_fault_is_observable` premise false, since it reasons that
            // a `Pure`/`Allocates` wrapper is never followed by a check. The
            // symbol is in hand and its manifest row is the answer;
            // `call_runtime` reads it.
            b.call_runtime(dst, symbol, arg_locals);
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
                MirType::Known(*ty),
                None,
                LocalDebugKind::Temp,
                Some(praxis_hir::expr_span(e)),
            );
            b.alloc(
                dst,
                AllocKind::Tuple {
                    ty: MirType::Known(*ty),
                    elements: element_locals,
                },
            );
            dst
        }
        TypedExpr::ListLit { elements, ty, .. } => lower_list_lit(b, elements, *ty, e),
        // M6: `read`/`parse` lower to a runtime call against the parser plan.
        TypedExpr::Read { plan, ty, .. } => lower_read(b, *plan, *ty),
        TypedExpr::Parse { text, plan, ty, .. } => lower_parse(b, text, *plan, *ty),
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
        // `p.0` — a tuple element (REP-08). A different runtime symbol from a
        // record field's, which is why it is a different instruction.
        TypedExpr::TupleIndex {
            receiver,
            index,
            ty,
            ..
        } => {
            let src = lower_expr_gc(b, receiver);
            let dst = b.alloc_gc(MirType::Known(*ty), None, LocalDebugKind::Temp, None);
            b.push(Inst::LoadTupleElem {
                dst,
                src,
                index: *index,
            });
            dst
        }
        // M7: enum variant construction.
        TypedExpr::EnumVariant {
            enum_def_id,
            variant_idx,
            args,
            ty,
            ..
        } => lower_enum_variant(b, *enum_def_id, *variant_idx, *ty, args),
        // M7-WS5: match expression — lowered to a tag-compare branch chain.
        TypedExpr::Match {
            scrutinee, arms, ..
        } => lower_match(b, scrutinee, arms),
        // M7-WS7: closure literal — allocate the closure value. Each capture's
        // current value is the captured binding's local; the synthetic function
        // (emitted separately by `lower_module`) is named by `fn_name`.
        TypedExpr::Closure {
            fn_name,
            captures,
            ty,
            ..
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
            // The closure value temp materializes `e` (the whole closure expr),
            // whose type is its `Func`.
            let dst = b.alloc_gc(MirType::Known(*ty), None, LocalDebugKind::Temp, espan);
            b.alloc(
                dst,
                AllocKind::Closure {
                    fn_name: fn_name.clone(),
                    captures: cap_locals,
                },
            );
            dst
        }
    }
}

/// Lower a `read parser_expr`: get the input buffer, then run the plan.
fn lower_read(b: &mut Builder<'_>, plan: praxis_hir::PlanId, result_ty: Type) -> LocalId {
    // 1. Get the input buffer from the runtime context. This is where §7.10's
    //    "the first `read` lazily reads standard input once" happens (REP-51):
    //    the call reads the host's input if nothing has yet, so it allocates
    //    and — through `praxis_alloc_text`, on input that is not UTF-8 (§4.3) —
    //    it can fault. Its manifest row says both, and the check below is what
    //    makes the fault land here rather than at the next unrelated one.
    let input = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, None);
    b.call_runtime(input, RuntimeSymbol::GetInput, vec![]);
    // 2. Run the parser plan against it.
    run_parser_plan(b, plan, input, result_ty)
}

/// Lower a `parse(text, parser_expr)`: run the plan against the text argument.
fn lower_parse(
    b: &mut Builder<'_>,
    text: &TypedExpr,
    plan: praxis_hir::PlanId,
    result_ty: Type,
) -> LocalId {
    let input = lower_expr_gc(b, text);
    run_parser_plan(b, plan, input, result_ty)
}

/// Emit the call to `praxis_run_parser(ctx, plan_id, input) -> GcRef`, then
/// check for a parse fault. The id is boxed as an Int GcRef to match the
/// uniform ABI; the runtime wrapper reads its payload and validates it back
/// into a `PlanId` (a value that names no plan becomes a parse fault).
fn run_parser_plan(
    b: &mut Builder<'_>,
    plan: praxis_hir::PlanId,
    input: LocalId,
    result_ty: Type,
) -> LocalId {
    // Box the plan id as an Int GcRef.
    let idx_scalar = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ConstInt {
        dst: idx_scalar,
        value: i64::from(plan.get()),
    });
    let idx_gc = b.alloc_gc(MirType::Known(b.int_ty), None, LocalDebugKind::Temp, None);
    b.alloc(idx_gc, AllocKind::Int { value: idx_scalar });
    // Call praxis_run_parser(ctx, idx, input) -> result. The result's type is
    // the one the parser plan synthesizes, which the typed tree carries.
    let dst = b.alloc_gc(MirType::Known(result_ty), None, LocalDebugKind::Temp, None);
    b.call_runtime(dst, RuntimeSymbol::RunParser, vec![idx_gc, input]);
    dst
}

/// Lower a literal to a `GcRef` local (allocating the object). `span` is the
/// materializing expression's span, threaded so the debugger can show what each
/// temp holds (`@ "0"`, `@ "x / 0"`, …); `None` for span-less synthetic lits.
fn lower_lit_gc(b: &mut Builder<'_>, value: &Lit, span: Option<(u32, u32)>) -> LocalId {
    match value {
        Lit::Int(n) => {
            let dst = b.alloc_gc(MirType::Known(b.int_ty), None, LocalDebugKind::Temp, span);
            // **The one question asked here is the runtime's own.**
            // `small_int::index_of` is what `praxis_alloc_int` consults, so an
            // answer of `Some` is a guarantee that `ctx.small_ints` holds this
            // value — the compiler cannot emit a table read for a slot the
            // table does not have.
            if praxis_runtime::small_int::index_of(*n).is_some() {
                // Not a safepoint, and `b.push` rather than `b.emit` says so:
                // `Builder::emit` exists to decide whether a fault check
                // follows, and this instruction calls no wrapper, allocates
                // nothing and cannot fault. `verify` rejects a `CheckFault`
                // after it in so many words (ADR-088, both directions).
                b.push(Inst::ConstGc {
                    dst,
                    konst: GcConst::SmallInt(*n),
                });
            } else {
                // Out of range, so this is a real allocation and must stay a
                // safepoint: the wrapper can collect, and the collector must see
                // this frame (ADR-040). The `Gc` local, its type, its
                // `LocalDebugKind::Temp` and its span are the same either way,
                // so the debugger renders the literal identically whichever
                // branch produced it.
                let scalar = b.alloc_scalar(ScalarKind::Int);
                b.push(Inst::ConstInt {
                    dst: scalar,
                    value: *n,
                });
                b.alloc(dst, AllocKind::Int { value: scalar });
            }
            dst
        }
        Lit::Bool(v) => {
            // There are two `Bool` values and the runtime minted both at
            // startup, so a literal is a load and has been all along — what it
            // used to cost was the extern call and the shadow-frame spill that
            // `Inst::Alloc`'s unconditional safepoint status put in front of it,
            // at a point the manifest itself calls `Effect::Pure`.
            let dst = b.alloc_gc(MirType::Known(b.bool_ty), None, LocalDebugKind::Temp, span);
            b.push(Inst::ConstGc {
                dst,
                konst: GcConst::Bool(*v),
            });
            dst
        }
        Lit::Text(s) => {
            let dst = b.alloc_gc(MirType::Known(b.text_ty), None, LocalDebugKind::Temp, span);
            // **The rule has no carve-out here, on purpose (ADR-088, D-A).**
            // `praxis_alloc_text` is `AllocatesAndFaults`: it validates its
            // bytes and sets `INVALID_TEXT`. The bytes a *literal* hands it came
            // from a Rust `String`, so the fault cannot fire at this call site —
            // but claiming that in the verifier would put the exception in the
            // very first arm of a rule whose whole content is that it has none,
            // and nothing but the verifier would read the claim. The cost is one
            // check per text-literal evaluation. Moving the validation out of
            // the wrapper (so its row becomes `Allocates` and the instruction
            // genuinely cannot fault) is the right long-term answer and is
            // registered as its own row — it changes what a violated compiler
            // precondition *does*, which is ADR-017 territory.
            b.alloc(dst, AllocKind::Text { value: s.clone() });
            dst
        }
        Lit::Char(c) => {
            // Char's payload is a u32 Unicode scalar; ConstInt carries it as i64.
            let scalar = b.alloc_scalar(ScalarKind::Char);
            b.push(Inst::ConstInt {
                dst: scalar,
                value: *c as i64,
            });
            let dst = b.alloc_gc(MirType::Known(b.char_ty), None, LocalDebugKind::Temp, span);
            // `praxis_alloc_char` validates the Unicode scalar
            // (`INVALID_CHAR`), so this checks for `AllocKind::Text`'s reason.
            // There is no char-literal syntax today, so this arm is reached
            // only from a synthesized `Lit::Char`.
            b.alloc(dst, AllocKind::Char { value: scalar });
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
            let dst = b.alloc_gc(MirType::Known(b.float_ty), None, LocalDebugKind::Temp, span);
            b.alloc(dst, AllocKind::Float { value: scalar });
            dst
        }
        Lit::Unit => {
            // The Unit value (§4.3): the immortal singleton, read out of the
            // context. As for `Lit::Bool` — this never allocated; it only paid
            // for looking like it did.
            let dst = b.alloc_gc(MirType::Known(b.unit_ty), None, LocalDebugKind::Temp, span);
            b.push(Inst::ConstGc {
                dst,
                konst: GcConst::Unit,
            });
            dst
        }
    }
}

/// How a comparison of two values of a given type is carried out (ADR-045).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CompareVia {
    /// Natively, on the scalar channel, reading the payload at this width.
    Scalar(ScalarKind),
    /// Natively, as IEEE-754 floats (NaN unordered, §4.12).
    Float,
    /// Through the value's descriptor — `praxis_struct_eq` for `==`/`!=`,
    /// `praxis_value_cmp` for an ordering. The answer for every type whose
    /// payload is not a machine number: composites, `Text`, and anything whose
    /// static type is still unresolved.
    Descriptor,
}

/// Which comparison lowering `ty`'s values take.
///
/// The unresolved and unexpected cases answer `Descriptor` rather than
/// `Scalar(Int)` deliberately: an eight-byte payload load was the fallback for
/// everything that was not a `Float`, and it is exactly how `Text` came to be
/// compared by its `TextPayload` discriminant — under which *every* pair of
/// owned strings was equal (P0-12). Dispatching through the descriptor is wrong
/// for no type: at worst it faults.
fn compare_kind(b: &Builder<'_>, ty: praxis_types::Type) -> CompareVia {
    use praxis_types::data::TypeData;
    use praxis_types::ScalarType;
    match b.db.data(ty) {
        TypeData::Scalar(ScalarType::Float) => CompareVia::Float,
        TypeData::Scalar(ScalarType::Bool) => CompareVia::Scalar(ScalarKind::Bool),
        TypeData::Scalar(ScalarType::Char) => CompareVia::Scalar(ScalarKind::Char),
        TypeData::Scalar(ScalarType::Int | ScalarType::UInt) => CompareVia::Scalar(ScalarKind::Int),
        // `Byte` has no `praxis_byte_load`; the scalar channel would read it as
        // an `Int`, eight bytes from a one-byte payload. The descriptor reads
        // it at its own width.
        TypeData::Scalar(ScalarType::Byte) => CompareVia::Descriptor,
        _ => CompareVia::Descriptor,
    }
}

/// Which arithmetic `+ - * / %` on values of a given type lower to (§4.12,
/// ADR-085).
///
/// The sibling of [`compare_kind`], and it exists for that function's reason:
/// the answer is a property of the operand *type*, so it belongs in one place
/// rather than being re-derived at each site that needs it. REP-64 is what the
/// second half of that rule not being followed cost — the binary operators
/// asked and the two compound-assignment paths did not, so `f += 2.0` added two
/// IEEE-754 **bit patterns** as integers and boxed the sum back as a `Float`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ArithVia {
    /// Checked `i64` arithmetic on the scalar channel: faults on overflow and
    /// on division or remainder by zero.
    Int,
    /// IEEE-754 binary64 on the scalar channel, unchecked (inf/NaN, never a
    /// fault). A `Float` rides the uniform `i64` scalar channel as its bit
    /// pattern (ADR-037), so an arithmetic site must bit-cast to `f64` and back
    /// — [`lower_extract_float`] and [`lower_materialize_float`] are that cast.
    /// A site that reaches for [`ArithVia::Int`] instead does integer arithmetic
    /// on two bit patterns and answers a number no program asked for.
    Float,
    /// `praxis_text_concat`, for `+` and nothing else (ADR-085). A `Text`
    /// payload is a pointer-and-length structure rather than a number, so the
    /// `Int` channel here would add two *pointers*.
    Text,
}

/// Which arithmetic lowering `ty`'s values take.
///
/// A type that is not one of the three named scalars answers `Int`, which is
/// the fallback the language's own typing rules make unreachable: inference
/// requires a `Numeric` operand of every arithmetic site, so anything else has
/// already been refused and no MIR is built for it.
fn arith_kind(b: &Builder<'_>, ty: praxis_types::Type) -> ArithVia {
    use praxis_types::data::TypeData;
    use praxis_types::ScalarType;
    match b.db.data(b.db.follow(ty)) {
        TypeData::Scalar(ScalarType::Float) => ArithVia::Float,
        TypeData::Scalar(ScalarType::Text) => ArithVia::Text,
        _ => ArithVia::Int,
    }
}

/// The arithmetic half of a compound assignment: `cur <op> rhs`, materialized
/// into a fresh `GcRef` ready to be stored back.
///
/// **One function for both compound-assignment paths** — `x += …` on a binding
/// and `m[k] += …` through a subscript ([`TypedStmt::IndexAssign`], ADR-064).
/// The choice the two make is the same choice, and REP-64 is what making it
/// twice cost: neither copy asked whether the operands were `Float`, so both
/// took the `Int` channel and `var f = 1.0; f += 2.0` printed
/// `9218868437227405312` — `f64::to_bits(1.0) + f64::to_bits(2.0)`, reinterpreted
/// as a `Float`. Every operator and both target shapes were affected; the plain
/// binary `+` was not, because it had asked since §4.12 landed.
///
/// `None` means there is nothing to lower and the statement is dropped: `%` is
/// not defined for `Float` (§4.12), and neither is any operator but `+` for
/// `Text` (ADR-085). Both are `Y016` before MIR is built, so this is the
/// defensive arm of a case inference has already refused — and answering `None`
/// is what keeps it from quietly becoming a *different* operation, which is the
/// mistake `binop_to_float`'s `_ => Add` fallback would make here.
fn lower_compound_arith(
    b: &mut Builder<'_>,
    op: AssignOp,
    cur: LocalId,
    rhs: LocalId,
    operand_ty: praxis_types::Type,
    span: Option<(u32, u32)>,
) -> Option<LocalId> {
    match arith_kind(b, operand_ty) {
        ArithVia::Int => {
            let result = lower_int_binop(b, op_to_int_binop(op), cur, rhs);
            Some(lower_materialize(b, result, span))
        }
        ArithVia::Float => {
            let result = lower_float_binop(b, op_to_float_binop(op)?, cur, rhs);
            Some(lower_materialize_float(b, result, span))
        }
        // `s += "x"` is `s = s + "x"` (ADR-085): the same runtime call `+`
        // lowers to, and the reason inference *excuses* a `Text` target from the
        // numeric requirement instead of exempting it from being checked.
        ArithVia::Text => {
            if op != AssignOp::AddAssign {
                return None;
            }
            let joined = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, span);
            b.call_runtime(joined, RuntimeSymbol::TextConcat, vec![cur, rhs]);
            Some(joined)
        }
    }
}

/// Extract a payload into a scalar local at the width `kind` names.
fn lower_extract_scalar(b: &mut Builder<'_>, src: LocalId, kind: ScalarKind) -> LocalId {
    let dst = b.alloc_scalar(kind);
    b.push(Inst::ExtractScalar {
        dst,
        src,
        scalar: kind,
    });
    dst
}

/// Extract an `Int` payload into a scalar local.
fn lower_extract_int(b: &mut Builder<'_>, src: LocalId) -> LocalId {
    lower_extract_scalar(b, src, ScalarKind::Int)
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
    b.push(Inst::IntBinOp {
        op,
        dst,
        lhs,
        rhs,
        overflow: Overflow::Checked,
    });
    b.check_fault();
    dst
}

/// Materialize an `Int` scalar into a fresh `GcRef`. `span` is the
/// materializing expression's span for debugger provenance.
fn lower_materialize(b: &mut Builder<'_>, scalar: LocalId, span: Option<(u32, u32)>) -> LocalId {
    let dst = b.alloc_gc(MirType::Known(b.int_ty), None, LocalDebugKind::Temp, span);
    b.push(Inst::Materialize {
        dst,
        src: scalar,
        scalar: ScalarKind::Int,
        roots: RootSlots::unannotated(),
        debug: DebugSlots::unannotated(),
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
    let dst = b.alloc_gc(MirType::Known(b.bool_ty), None, LocalDebugKind::Temp, span);
    b.push(Inst::Materialize {
        dst,
        src: scalar,
        scalar: ScalarKind::Bool,
        roots: RootSlots::unannotated(),
        debug: DebugSlots::unannotated(),
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

/// Lower a Float negation on a `GcRef` operand, returning the scalar
/// (bit-pattern) result. IEEE-754 `negate`: no fault, and — unlike a
/// subtraction from zero — exact at both zeros (REP-50).
fn lower_float_neg(b: &mut Builder<'_>, operand_gc: LocalId) -> LocalId {
    let src = lower_extract_float(b, operand_gc);
    let dst = b.alloc_scalar(ScalarKind::Float);
    b.push(Inst::FloatNeg { dst, src });
    dst
}

/// Materialize a `Float` scalar (bit-pattern) into a fresh `GcRef`. `span` is
/// the materializing expression's span for debugger provenance.
fn lower_materialize_float(
    b: &mut Builder<'_>,
    scalar: LocalId,
    span: Option<(u32, u32)>,
) -> LocalId {
    let dst = b.alloc_gc(MirType::Known(b.float_ty), None, LocalDebugKind::Temp, span);
    b.push(Inst::Materialize {
        dst,
        src: scalar,
        scalar: ScalarKind::Float,
        roots: RootSlots::unannotated(),
        debug: DebugSlots::unannotated(),
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
        roots: RootSlots::unannotated(),
        debug: DebugSlots::unannotated(),
    });
    lower_materialize_bool(b, bool_scalar, None)
}

/// Lower an ordering of two GC values through their descriptor (ADR-045),
/// returning the `Scalar(Int)` local holding `-1`/`0`/`1`.
///
/// `praxis_value_cmp` faults when the operands' runtime types disagree or the
/// type has no ordering, so a fault check follows. It allocates nothing, so the
/// call is not a safepoint and the instruction carries no root set.
fn lower_value_cmp(b: &mut Builder<'_>, lhs: LocalId, rhs: LocalId) -> LocalId {
    let dst = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ValueCmp { dst, lhs, rhs });
    b.check_fault();
    dst
}

/// Lower a short-circuiting logical operator: `lhs || rhs` or `lhs && rhs`
/// (REP-07).
///
/// Both are one shape with the *answer* on the skipping side flipped:
///
/// - `lhs || rhs` is `if lhs { true } else { rhs }`
/// - `lhs && rhs` is `if lhs { rhs } else { false }`
///
/// `skip_on` is the `lhs` value that decides the whole expression — `true` for
/// `||`, `false` for `&&` — and it is also the literal that arm produces, which
/// is why one function serves both. On that path `rhs` is **not evaluated**: its
/// side effects, its faults and its GC safepoints are all skipped, which is the
/// point of the operator and not an optimization. `false && panic("x")` must not
/// fault.
///
/// Both operands are `Bool` `GcRef`s; `lhs_gc` is already lowered and `rhs_expr`
/// is lowered into exactly one of the two blocks.
fn lower_short_circuit(
    b: &mut Builder<'_>,
    lhs_gc: LocalId,
    rhs_expr: &TypedExpr,
    skip_on: bool,
) -> LocalId {
    let lhs_bool = b.alloc_scalar(ScalarKind::Bool);
    b.push(Inst::ExtractScalar {
        dst: lhs_bool,
        src: lhs_gc,
        scalar: ScalarKind::Bool,
    });
    let result = b.alloc_gc(MirType::Known(b.bool_ty), None, LocalDebugKind::Temp, None);
    let true_blk = b.func.new_block();
    let false_blk = b.func.new_block();
    let join = b.func.new_block();
    b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
        cond: lhs_bool,
        then_block: true_blk,
        else_block: false_blk,
    };
    // The block `lhs == skip_on` reaches: the answer is `skip_on` itself, and
    // `rhs` is never lowered into it.
    let (short_blk, long_blk) = if skip_on {
        (true_blk, false_blk)
    } else {
        (false_blk, true_blk)
    };
    b.cur = short_blk;
    let lit = lower_lit_gc(b, &Lit::Bool(skip_on), None);
    b.push(Inst::MoveGc {
        dst: result,
        src: lit,
    });
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: join };
    // The other block: the answer is whatever `rhs` is.
    b.cur = long_blk;
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

    let result = b.alloc_gc(
        MirType::Known(then_block.ty),
        None,
        LocalDebugKind::Temp,
        None,
    );
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
        result: None, // a `while` is Unit; a value `break` in one is Y017
    });
    b.cur = body_blk;
    let _ = lower_block_body(b, body);
    b.loop_stack.pop();
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };

    b.cur = exit;
}

/// `for binding in iter { body }` (M8-WS6, §4.11). Lowers to an index loop over
/// the source: a header tests `i < len`, the body binds the member at `i` to the
/// loop variable, runs, increments `i`, and jumps back.
///
/// *What* it indexes is [`IterPlan`]'s answer (REP-15, ADR-066). Three iterables
/// index themselves; the other seven are walked through a **snapshot** taken
/// once before the header, so the loop body always indexes a `Vec`.
fn lower_for(
    b: &mut Builder<'_>,
    binding: &praxis_hir::TypedPattern,
    iter: &TypedExpr,
    body: &praxis_hir::TypedBlock,
    item_ty: Type,
) {
    // Lower the iterator once; it lives in a Gc slot for the loop's duration.
    let iter_source = lower_expr_gc(b, iter);
    // Take the snapshot before the header, so it is one call per loop and not
    // one per step — and so the loop walks a value nothing in the body can
    // mutate. `iter_local` is what the header and body index from here on; for
    // the paired plan it is the keys and `paired_values` holds the values.
    let plan = iter_plan(b.db, iter);
    let (iter_local, paired_values) = match plan {
        IterPlan::InPlace { .. } => (iter_source, None),
        IterPlan::Snapshot(items) => (snapshot(b, iter_source, items), None),
        IterPlan::Paired { keys, values } => (
            snapshot(b, iter_source, keys),
            Some(snapshot(b, iter_source, values)),
        ),
    };
    // The index lives in a Gc Int slot (not a scalar) so it persists across the
    // loop's block boundaries like other Gc values. Start at 0.
    let idx_gc = b.alloc_gc(MirType::Known(b.int_ty), None, LocalDebugKind::Temp, None);
    let zero_scalar = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ConstInt {
        dst: zero_scalar,
        value: 0,
    });
    b.push(Inst::Materialize {
        dst: idx_gc,
        src: zero_scalar,
        scalar: ScalarKind::Int,
        roots: RootSlots::unannotated(),
        debug: DebugSlots::unannotated(),
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
    let len_sym = plan.len_symbol();
    let len_dst = b.alloc_gc(MirType::Known(b.int_ty), None, LocalDebugKind::Temp, None);
    // Only `praxis_range_len` faults among the lengths the plan can name; a
    // `Vec`/`Deque`/`Text`/snapshot length does not, and this ran once per
    // iteration.
    b.call_runtime(len_dst, len_sym, vec![iter_local]);
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
        result: None, // a `for` is Unit; a value `break` in one is Y017
    });
    b.cur = body_blk;
    // Bind the loop variable: `binding = iter.get(idx_gc)`.
    let get_sym = plan.get_symbol();
    // The item and the loop variable are the iterator's element type, which the
    // typed tree carries on the `For` node (`item_ty`). Both slots used to be
    // `Opaque`, so the debugger showed a `for` binding with no type at all.
    let item_gc = b.alloc_gc(MirType::Known(item_ty), None, LocalDebugKind::Temp, None);
    // Every accessor an `IterPlan` can name faults on an out-of-range index, so
    // this check stays — but it is the row that says so, not the site.
    b.call_runtime(item_gc, get_sym, vec![iter_local, idx_gc]);
    // A keyed collection's member is the `(K, V)` pair `item_ty` names, and the
    // two halves arrive from two index-aligned snapshots. The pair is built
    // *here* rather than in the runtime because the tuple's schema is the
    // compiler's answer already — `AllocKind::Tuple` resolves it from `item_ty`,
    // the same way a `(a, b)` literal does — so no runtime schema interner has
    // to exist.
    let item_gc = match paired_values {
        None => item_gc,
        Some(values) => {
            let value_gc = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, None);
            b.call_runtime(value_gc, RuntimeSymbol::VecGet, vec![values, idx_gc]);
            let pair = b.alloc_gc(MirType::Known(item_ty), None, LocalDebugKind::Temp, None);
            b.alloc(
                pair,
                AllocKind::Tuple {
                    ty: MirType::Known(item_ty),
                    elements: vec![item_gc, value_gc],
                },
            );
            pair
        }
    };
    // The loop variable's slot: allocate one if the `for` binding has no slot
    // yet (it is introduced by the loop, not a `let` statement). Reads of the
    // binding inside the body resolve to this slot via `b.locals`.
    let named = match binding {
        praxis_hir::TypedPattern::Bind { symbol, .. } => Some(*symbol),
        _ => None,
    };
    let slot = named
        .and_then(|s| b.locals.get(&s).copied())
        .unwrap_or_else(|| b.alloc_gc(MirType::Known(item_ty), None, LocalDebugKind::User, None));
    if let Some(symbol) = named {
        b.locals.insert(symbol, slot);
    }
    b.push(Inst::MoveGc {
        dst: slot,
        src: item_gc,
    });
    // A destructuring binding reads its components out of that same slot
    // (REP-25). The pattern is irrefutable — HIR reported one that can fail — so
    // there is no test and no branch: this is the binding half of
    // `emit_pattern_test` with the testing half removed.
    bind_components(b, slot, binding);
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
    // `for`'s index bump: `idx` is bounded above by the collection's length.
    b.push(Inst::IntBinOp {
        dst: next_scalar,
        op: IntBinOp::Add,
        lhs: cur_scalar,
        rhs: one_scalar,
        overflow: Overflow::Bounded,
    });
    b.push(Inst::Materialize {
        dst: idx_gc,
        src: next_scalar,
        scalar: ScalarKind::Int,
        roots: RootSlots::unannotated(),
        debug: DebugSlots::unannotated(),
    });
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };

    b.cur = exit;
}

/// `loop { body }` (M8-WS6, §4.11). An infinite loop; `break` is the only exit.
///
/// Returns the slot holding the loop's value (TY-21). A `loop` whose `break`s
/// carry a value gets a result slot each of them writes before jumping, exactly
/// as an `if`'s branches write theirs; a `Unit`-valued loop keeps the literal it
/// always had, and a `Never`-valued one (no reachable `break`) has an
/// unreachable exit, so its "value" is a placeholder no execution reads.
fn lower_loop(
    b: &mut Builder<'_>,
    body: &praxis_hir::TypedBlock,
    ty: Type,
    espan: Option<(u32, u32)>,
) -> LocalId {
    let header = b.func.new_block();
    let exit = b.func.new_block();
    // A loop that produces a value needs somewhere to put it. `Unit` and `Never`
    // do not: the first is the literal every exit already yields, and the second
    // has no representation at all (a `Never` local would fail the compile at
    // its descriptor site).
    let result = match b.db.data(b.db.follow(ty)) {
        praxis_types::TypeData::Unit | praxis_types::TypeData::Never => None,
        _ => Some(b.alloc_gc(MirType::Known(ty), None, LocalDebugKind::Temp, None)),
    };
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };
    b.cur = header;
    b.loop_stack.push(LoopCtx {
        continue_target: header,
        break_target: exit,
        result,
    });
    let _ = lower_block_body(b, body);
    b.loop_stack.pop();
    // Fall through the body → jump back to the header (infinite loop).
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };
    b.cur = exit;
    result.unwrap_or_else(|| lower_lit_gc(b, &Lit::Unit, espan))
}

/// `break [expr]` (M8-WS6, §4.11). Jump to the enclosing loop's break target,
/// writing the loop's result slot on the way out when it has one (TY-21).
///
/// The enclosing loop is guaranteed: a `break` with none is `Y012`, reported by
/// inference (TY-20), and no MIR is built for a program that has one.
fn lower_break(b: &mut Builder<'_>, value: &Option<Box<TypedExpr>>) {
    let ctx = *b
        .loop_stack
        .last()
        .expect("`break` outside a loop is Y012, reported before MIR");
    match (ctx.result, value) {
        // A value-producing loop: every exit writes the slot, including a bare
        // `break` — which cannot happen in a well-typed program (its `Unit`
        // would not join), but leaves no unwritten slot if it does.
        (Some(result), _) => {
            let src = match value {
                Some(v) => lower_expr_gc(b, v),
                None => lower_lit_gc(b, &Lit::Unit, None),
            };
            b.push(Inst::MoveGc { dst: result, src });
        }
        // A `Unit`- or `Never`-valued loop: the value is lowered for effect.
        (None, Some(v)) => {
            let _ = lower_expr_gc(b, v);
        }
        (None, None) => {}
    }
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump {
        target: ctx.break_target,
    };
    // A fresh unreachable block so subsequent lowering has somewhere to go.
    b.cur = b.func.new_block();
}

/// `continue` (M8-WS6, §4.11). Jump to the enclosing loop's continue target.
/// As for `break`, the enclosing loop is guaranteed by TY-20's `Y012`.
fn lower_continue(b: &mut Builder<'_>) {
    let ctx = *b
        .loop_stack
        .last()
        .expect("`continue` outside a loop is Y012, reported before MIR");
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump {
        target: ctx.continue_target,
    };
    b.cur = b.func.new_block();
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
// `PipelinePlan` whose body is a *recursive* `Chain` of streaming stages
// terminated by a `Sink`; `lower_pipeline` emits *one* fused loop over the
// source threading each element through the chain and into the sink.
// `v.map(f).filter(p).sum()` → one loop, zero intermediate Vecs.
//
// Design note — the chain is recursive because the semantics are (MIR-06). A
// `flat_map` does not transform an element, it replaces the element with a
// *sequence*, and everything after it runs once per inner element. A flat
// `Vec<Stage>` cannot say that, so the emitter used to special-case the first
// `flat_map` and re-enter the element-wise stage loop for the remainder — which
// meant a *second* `flat_map` arrived at a stage arm that could not exist and
// panicked the compiler. `Chain::Splice { f, rest }` holds the remainder
// *inside* the splice and `emit_plan` recurses into it, so there is no
// element-wise arm left for a splice to fall into.
//
// Design note — inline emission. Each step emits its branches directly into the
// current block rather than returning a control-flow enum: a step that drops
// the element (`filter`) or stops the stream (`take`) emits its own jump and
// leaves `b.cur` on a dead block, which the caller then never appends to
// because `emit_plan` recurses *from* the live continuation. This keeps
// conditional behavior real (a Rust `bool` can't branch for us), and mirrors
// how the rest of the builder (if/while/match) emits straight-line MIR.
//
// Design note — an argument is a field, not a side channel. Every closure and
// second source is lowered once, before the loop, and stored *on* its `Plan`
// node. The emitter used to carry them in one positional iterator shared
// between the outer stage loop and the flat_map splice, with a written contract
// that each stage advance it by exactly the right amount; a chain that got that
// wrong would have mis-paired closures silently.
//
// Design note — there is no second lowerer. The per-combinator eager lowerers
// (`lower_pipeline_combinator` + the `lower_seq_*` family + `emit_index_loop`)
// stood here as ADR-029 decision 1's incremental-safety net, kept "as a fallback
// for any chain the recognizer declines". They are gone (REP-40). Every
// registered `MethodLowering::Intrinsic` name is classified by `classify_link`
// or `classify_sink` — `intrinsics_are_all_recognized_so_there_is_no_second_\
// lowering` walks the catalog and asserts exactly that — so the net caught
// nothing a well-typed program could fall into, and what it *did* hold was
// wrong: `lower_seq_fold` returned the seed without ever invoking the closure,
// and the `_` arm answered the Unit singleton. A net that gives a wrong answer
// in silence is worse than no net, because the failure it converts a compiler
// bug into is the program's. A declined chain is now an ICE that names the
// method (`lower_expr_gc`'s `MethodCall` arm), which is a compiler bug report
// rather than a wrong number.
// ===========================================================================

/// A streaming pipeline stage: transform *one* element, possibly skipping it or
/// stopping the stream.
///
/// `flat_map` is deliberately not a variant. It does not transform an element,
/// it replaces the element with a sequence and runs the rest of the chain once
/// per member — a nesting, which is [`Chain::Splice`]. Keeping it out of `Stage`
/// is what makes "a splice reached the element-wise emitter" unrepresentable
/// rather than an `unreachable!` (MIR-06).
#[derive(Clone)]
enum Stage {
    /// `(T) -> U` — replace the element with the closure's result.
    Map(Box<TypedExpr>),
    /// `(T) -> Bool` — drop the element if the predicate is false.
    Filter(Box<TypedExpr>),
    /// `(T) -> U` — map, then drop the result if it is Unit.
    FilterMap(Box<TypedExpr>),
    /// Keep at most `n` leading elements, then stop. `n` is any `Int`
    /// expression; the catalog types the parameter `Int` and says nothing about
    /// literals, so neither does this (MIR-03).
    Take(Box<TypedExpr>),
    /// Drop the first `n` elements. `n` is any `Int` expression.
    Skip(Box<TypedExpr>),
    /// Stop at the first element that fails the predicate.
    TakeWhile(Box<TypedExpr>),
    /// Replace the element with `(index, element)` tuples. `pair_ty` is the pair
    /// type, read off the call node (MIR-05).
    Enumerate { pair_ty: MirType },
    /// Pair each element with the corresponding element of `other`, stopping at
    /// the shorter length. `pair_ty` is the pair type.
    Zip {
        other: Box<TypedExpr>,
        pair_ty: MirType,
    },
}

/// One recognized link in a pipeline chain: an element-wise [`Stage`], or a
/// `flat_map` splice, which nests the rest of the chain inside itself.
enum Link {
    Stage(Stage),
    /// `(T) -> Vec[U]` — the rest of the chain runs once per member of the Vec
    /// the closure returns.
    Splice(Box<TypedExpr>),
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

/// A recognized pipeline chain, source-first. `Then` is one element-wise stage
/// followed by the rest of the chain; `Splice` is a `flat_map` whose `rest` runs
/// once per member of the inner Vec; `Sink` terminates.
///
/// The nesting is the point (MIR-06): what follows a `flat_map` is *inside* it,
/// which is exactly what the emitter needs to know and what a flat list of
/// stages could not express.
enum Chain {
    Then(Stage, Box<Chain>),
    Splice { f: Box<TypedExpr>, rest: Box<Chain> },
    Sink(Sink),
}

impl Chain {
    /// The sink every chain ends in. A `Chain` is finite and `Sink` is its only
    /// leaf, so this is total.
    fn sink(&self) -> &Sink {
        let mut cur = self;
        loop {
            cur = match cur {
                Chain::Then(_, rest) => rest,
                Chain::Splice { rest, .. } => rest,
                Chain::Sink(sink) => return sink,
            };
        }
    }
}

/// A recognized pipeline: a source collection and the chain applied to it. The
/// whole chain lowers to a single fused loop (plus one nested loop per splice).
struct PipelinePlan {
    source: Box<TypedExpr>,
    /// The element type flowing out of the source (before any stage). Used as
    /// the source item slot's type; `Known` whenever the source is a typed
    /// single-element collection (F15) and [`MirType::Opaque`] otherwise — a
    /// `Map`/`Counter` source, or one whose element type is still an inference
    /// variable.
    source_item_ty: MirType,
    chain: Chain,
    /// The chain's overall result type (carried on the outermost `MethodCall`).
    result_ty: Type,
}

/// Classify a single `MethodCall` node as one link of a streaming chain. `None`
/// means "not a recognized streaming op" — the recognizer treats the receiver
/// eagerly.
///
/// `ty` is the call node's own result type, which only the two pair-building
/// stages read (MIR-05): `enumerate`'s is `Vec[(Int, T)]` and `zip`'s is
/// `Vec[(T, U)]`, so the pair type they allocate is one element-of away.
fn classify_link(
    db: &praxis_types::TypeDb,
    name: &str,
    args: &[TypedExpr],
    ty: Type,
) -> Option<Link> {
    Some(match (name, args) {
        ("map", [f]) => Link::Stage(Stage::Map(Box::new(f.clone()))),
        ("filter", [p]) => Link::Stage(Stage::Filter(Box::new(p.clone()))),
        ("filter_map", [f]) => Link::Stage(Stage::FilterMap(Box::new(f.clone()))),
        ("flat_map", [f]) => Link::Splice(Box::new(f.clone())),
        ("take_while", [p]) => Link::Stage(Stage::TakeWhile(Box::new(p.clone()))),
        ("enumerate", []) => Link::Stage(Stage::Enumerate {
            pair_ty: pair_ty_of(db, ty),
        }),
        ("zip", [other]) => Link::Stage(Stage::Zip {
            other: Box::new(other.clone()),
            pair_ty: pair_ty_of(db, ty),
        }),
        ("take", [n]) => Link::Stage(Stage::Take(Box::new(n.clone()))),
        ("skip", [n]) => Link::Stage(Stage::Skip(Box::new(n.clone()))),
        _ => return None,
    })
}

/// The pair type a fused `enumerate`/`zip` allocates: the element of the call's
/// own `Vec[(…, …)]` result type (MIR-05).
///
/// [`MirType::Opaque`] when the call's result is not a single-argument
/// collection — which the catalog's rows make impossible for these two, but the
/// fallback is not decoration: a *half* of a `Known` pair may still be an
/// inference variable (a `Vec` that was never pushed to), and the backend
/// answers that with a **null schema slot** so the runtime reads the value's own
/// header. ADR-066 decision 5. It is also why the verifier's
/// `OpaqueAtDescriptorSite` rule stays off: refusing to compile an unresolved
/// element type would reject working programs.
fn pair_ty_of(db: &praxis_types::TypeDb, ty: Type) -> MirType {
    match b_db_element_of(db, ty) {
        Some(t) => MirType::Known(t),
        None => MirType::Opaque,
    }
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
fn recognize_pipeline(db: &praxis_types::TypeDb, expr: &TypedExpr) -> Option<PipelinePlan> {
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
    //
    // `count(pred)` is the one call that is **both** (REP-18, §3.3's
    // `counts.values().count(|n| n >= 2)`): it is exactly `filter(pred).count()`,
    // so it is recognized as that pair rather than as a sink of its own. No new
    // sink lowering, and it fuses into the same single loop the two-call spelling
    // does.
    let (outermost_link, sink) = match (name.as_str(), args.as_slice()) {
        ("count", [pred]) => (
            Some(Link::Stage(Stage::Filter(Box::new(pred.clone())))),
            Sink::Count,
        ),
        _ => match classify_sink(name, args) {
            Some(s) => (None, s),
            None => {
                let link = classify_link(db, name, args, *result_ty)?;
                (Some(link), Sink::Collect)
            }
        },
    };
    // Walk the receiver chain collecting links, outermost-first. `cur` is the
    // node under inspection; once it stops being a streaming `MethodCall` it is
    // the source leaf (whatever it is — `lower_expr_gc` will lower it).
    let mut links: Vec<Link> = Vec::new();
    if let Some(link) = outermost_link {
        links.push(link);
    }
    let mut cur: &TypedExpr = receiver;
    while let TypedExpr::MethodCall {
        receiver: inner_recv,
        name: inner_name,
        args: inner_args,
        ty: inner_ty,
        ..
    } = cur
    {
        match classify_link(db, inner_name, inner_args, *inner_ty) {
            Some(link) => {
                links.push(link);
                cur = inner_recv;
            }
            None => break, // Not a streaming stage — `cur` is our source.
        }
    }
    // Links were collected outermost-first, i.e. last-applied first, so wrapping
    // the sink in them one at a time yields the chain in execution order with no
    // reverse: the last link wrapped is the one nearest the source, and it ends
    // up outermost in the `Chain`.
    let mut chain = Chain::Sink(sink);
    for link in links {
        chain = match link {
            Link::Stage(stage) => Chain::Then(stage, Box::new(chain)),
            Link::Splice(f) => Chain::Splice {
                f,
                rest: Box::new(chain),
            },
        };
    }
    // The item flowing *out of the source* is the source collection's element
    // type, which the typed tree now carries (F15).
    let source_item_ty = match b_db_element_of(db, praxis_hir::expr_ty(cur)) {
        Some(t) => MirType::Known(t),
        None => MirType::Opaque,
    };
    Some(PipelinePlan {
        source: Box::new(cur.clone()),
        source_item_ty,
        chain,
        result_ty: *result_ty,
    })
}

/// A collection type's single element type, or `None` for anything else (a
/// `Map`/`Counter`, whose payload is a pair, or a type that is not a collection
/// at all). Used only to type the source item of a fused pipeline.
fn b_db_element_of(db: &praxis_types::TypeDb, t: Type) -> Option<Type> {
    use praxis_types::data::TypeData;
    match db.data(db.follow(t)) {
        TypeData::Collection { args, .. } if args.len() == 1 => Some(args[0]),
        _ => None,
    }
}

/// Lower a recognized pipeline as a single fused loop (M8-WS11, §6.3). Emits the
/// loop scaffold directly (header / body / increment / exit) rather than reusing
/// `emit_index_loop`, so streaming stages can `continue` (jump to the increment)
/// and short-circuit sinks/stages can `break` (jump to exit) cleanly.
fn lower_pipeline(b: &mut Builder<'_>, plan: PipelinePlan) -> LocalId {
    let PipelinePlan {
        source,
        source_item_ty,
        chain,
        result_ty,
    } = plan;
    let sink = chain.sink().clone();

    // Lower the source Vec once; it lives for the loop's duration.
    let src = lower_expr_gc(b, &source);
    // A Gc Int index counter (persists across blocks, like the for-loop counter).
    let idx = alloc_zeroed_counter(b);

    // Lower every stage closure / second source once, outside the loop, into the
    // `Plan` node that consumes it.
    let steps = lower_chain(b, &chain);

    // Sink closure/init, lowered once.
    let (sink_init_slot, sink_closure_slot) = match &sink {
        Sink::Fold { init, f } => {
            let init_l = lower_expr_gc(b, init);
            let f_l = lower_expr_gc(b, f);
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
            (None, Some(f_l))
        }
        _ => (None, None),
    };

    // Allocate the sink's accumulators up front.
    let (acc_scalar, acc_gc, seen_flag) = sink_alloc(b, &sink, sink_init_slot);

    // The Collect sink needs a result Vec pushed into per element.
    let collect_vec = match &sink {
        Sink::Collect => {
            let v = alloc_empty_vec(b, MirType::Known(result_ty));
            Some(v)
        }
        _ => None,
    };

    // `find`/`position` report a position, so like the stages that consume one
    // they get a dense counter — allocated here, before the scaffold, so a
    // splice cannot re-zero it (MIR-04, MIR-07).
    let sink_position = match &sink {
        Sink::Find(_) | Sink::Position(_) => Some(alloc_zeroed_counter(b)),
        _ => None,
    };

    // ---- The loop scaffold: header / body / increment / exit ---------------
    //
    // `exit` is *the* pipeline exit, and there is exactly one however deeply the
    // chain nests: a splice adds an inner header/body/increment, but never an
    // inner place to stop the stream (MIR-08). No `LoopCtx` is pushed for either
    // loop — a user `break`/`continue` cannot appear in a fused body, because
    // every expression the chain contains is lowered above this line (a closure
    // body is a separate MIR function), so a loop stack here would only be a
    // second, weaker way to name the targets the emitter already carries.
    let header = b.func.new_block();
    let body_blk = b.func.new_block();
    let incr_blk = b.func.new_block();
    let exit = b.func.new_block();
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };
    b.cur = header;

    // Header: `if idx < src.len() { body } else { exit }`.
    emit_bounds_check(b, src, idx, body_blk, exit);

    // Body: load the element, thread it through the chain, run the sink.
    b.cur = body_blk;
    let item = b.alloc_gc(source_item_ty, None, LocalDebugKind::Temp, None);
    b.call_runtime(item, RuntimeSymbol::VecGet, vec![src, idx]);

    let sink_plan = SinkPlan {
        sink: &sink,
        acc_scalar,
        acc_gc,
        seen_flag,
        collect_vec,
        closure: sink_closure_slot,
        position: sink_position,
    };
    emit_plan(b, &steps, item, &sink_plan, incr_blk, exit);

    // Increment block: `idx += 1`, jump to header.
    b.cur = incr_blk;
    emit_increment(b, idx);
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };

    // Exit: materialize the sink's result out of its accumulator(s).
    b.cur = exit;
    sink_finish(
        b,
        &sink,
        acc_scalar,
        acc_gc,
        seen_flag,
        collect_vec,
        result_ty,
    )
}

/// A pipeline stage with everything it needs already lowered to slots: the
/// closure or second source is a `LocalId`, not a position in a shared
/// iterator, and a stage that needs a position owns the counter that gives it
/// one.
///
/// **A stage's index is the sequence that reaches it** (MIR-04). Four stages
/// ask "which element is this?" and the honest answer is different for each of
/// them: after a `filter`, `take(2)` means the first two *surviving* elements,
/// and `enumerate` numbers 0, 1, 2 without gaps. There used to be one index —
/// the source cursor — and all four read it, so `v.filter(even).take(2)` took
/// whatever survived among source positions 0 and 1. Each of them carries its
/// own `count` slot now, bumped once per element that arrives, and the source
/// cursor is not in scope here at all.
enum Step {
    Map(LocalId),
    Filter(LocalId),
    FilterMap(LocalId),
    Take {
        bound: LocalId,
        count: LocalId,
    },
    Skip {
        bound: LocalId,
        count: LocalId,
    },
    TakeWhile(LocalId),
    Enumerate {
        count: LocalId,
        pair_ty: MirType,
    },
    Zip {
        other: LocalId,
        count: LocalId,
        pair_ty: MirType,
    },
}

/// The lowered mirror of [`Chain`]: the same shape, with every argument
/// resolved to the slot holding it.
enum Plan {
    Step(Step, Box<Plan>),
    Splice { f: LocalId, rest: Box<Plan> },
    Sink,
}

/// Everything the sink needs, allocated once before the loop. Passed by
/// reference through the recursive emitter so a splice's inner loop feeds the
/// *same* accumulators the outer loop does.
#[derive(Clone, Copy)]
struct SinkPlan<'a> {
    sink: &'a Sink,
    acc_scalar: Option<LocalId>,
    acc_gc: Option<LocalId>,
    seen_flag: Option<LocalId>,
    collect_vec: Option<LocalId>,
    closure: Option<LocalId>,
    /// `find`/`position` report an index, so they are position-consuming like
    /// the four stages that are, and they get a dense counter for the same
    /// reason: the answer is the position in the sequence that reached the
    /// sink, not in the source (MIR-04).
    position: Option<LocalId>,
}

/// Allocate a zeroed `Gc` Int slot: a loop cursor or a stage's dense counter.
/// `Gc` rather than `Scalar` because it is live across the `praxis_vec_get`
/// safepoints in the loop body and the collector has to see a real value there
/// (ADR-015 §10.3).
fn alloc_zeroed_counter(b: &mut Builder<'_>) -> LocalId {
    let slot = b.alloc_gc(MirType::Known(b.int_ty), None, LocalDebugKind::Temp, None);
    let zero = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ConstInt {
        dst: zero,
        value: 0,
    });
    b.push(Inst::Materialize {
        dst: slot,
        src: zero,
        scalar: ScalarKind::Int,
        roots: RootSlots::unannotated(),
        debug: DebugSlots::unannotated(),
    });
    slot
}

/// Lower every argument the chain evaluates outside the loop — each stage's
/// closure or second source — into a [`Plan`] of the same shape, and allocate
/// the dense counter of each stage that needs a position.
///
/// Order matters and is the chain's own: source-side stages first, exactly as
/// the eager spelling would evaluate them.
///
/// The counters are allocated **here**, which is before the loop scaffold, and
/// that placement is MIR-07 (there is nothing else to it). A counter zeroed
/// inside the outer loop body would restart for every inner Vec of a
/// `flat_map`, which is what the old inner index did: `take(1)` after a
/// `flat_map` kept the first element of *each* inner sequence. Zeroed once, a
/// counter counts the flattened stream by construction.
fn lower_chain(b: &mut Builder<'_>, chain: &Chain) -> Plan {
    match chain {
        Chain::Then(stage, rest) => {
            let step = match stage {
                Stage::Map(f) => Step::Map(lower_expr_gc(b, f)),
                Stage::Filter(p) => Step::Filter(lower_expr_gc(b, p)),
                Stage::FilterMap(f) => Step::FilterMap(lower_expr_gc(b, f)),
                Stage::TakeWhile(p) => Step::TakeWhile(lower_expr_gc(b, p)),
                Stage::Zip { other, pair_ty } => Step::Zip {
                    other: lower_expr_gc(b, other),
                    count: alloc_zeroed_counter(b),
                    pair_ty: *pair_ty,
                },
                Stage::Take(n) => Step::Take {
                    bound: lower_expr_gc(b, n),
                    count: alloc_zeroed_counter(b),
                },
                Stage::Skip(n) => Step::Skip {
                    bound: lower_expr_gc(b, n),
                    count: alloc_zeroed_counter(b),
                },
                Stage::Enumerate { pair_ty } => Step::Enumerate {
                    count: alloc_zeroed_counter(b),
                    pair_ty: *pair_ty,
                },
            };
            Plan::Step(step, Box::new(lower_chain(b, rest)))
        }
        Chain::Splice { f, rest } => Plan::Splice {
            f: lower_expr_gc(b, f),
            rest: Box::new(lower_chain(b, rest)),
        },
        Chain::Sink(_) => Plan::Sink,
    }
}

/// Emit the rest of the pipeline for one element.
///
/// Every path out of `plan` is terminated by the time this returns — to
/// `continue_target` (this element is done), to `exit` (the stream is done), or
/// into a splice's inner loop — so `b.cur` is left on a fresh dead block and the
/// caller appends nothing to it.
///
/// `continue_target` is the *innermost* increment block: a `Splice` rebinds it
/// to its own, because while a splice is running "advance to the next element"
/// means the next inner element.
///
/// Note what is **not** a parameter: the loop's index. A source cursor is the
/// business of the loop that owns it — the header's bounds check and the
/// `praxis_vec_get` that loads the element — and no stage may read it. Each
/// stage that needs a position carries its own dense counter (MIR-04), and
/// making the cursor unreachable from here is what keeps it that way.
fn emit_plan(
    b: &mut Builder<'_>,
    plan: &Plan,
    item: LocalId,
    sink: &SinkPlan<'_>,
    continue_target: BlockId,
    pipeline_exit: BlockId,
) {
    match plan {
        Plan::Step(step, rest) => {
            let next = emit_step(b, step, item, continue_target, pipeline_exit);
            emit_plan(b, rest, next, sink, continue_target, pipeline_exit);
        }
        Plan::Splice { f, rest } => {
            // f(item) -> Vec[U]; the rest of the chain runs once per member.
            let inner = invoke_closure(b, *f, vec![item]);
            emit_splice(b, inner, rest, sink, continue_target, pipeline_exit);
        }
        Plan::Sink => {
            emit_sink_body(b, sink, item, continue_target, pipeline_exit);
            // Normal sink completion: fall through to the increment block. (A
            // sink that short-circuits — any/all/find — emitted its own jump to
            // the pipeline exit and left `b.cur` dead, in which case this jump
            // lands in a dead block, harmlessly.)
            jump_and_go_dead(b, continue_target);
        }
    }
}

/// Emit one element-wise step. Returns the element flowing out of it, with
/// `b.cur` on the live continuation: a step that drops (`filter`, `skip`) or
/// stops (`take`, `take_while`, `zip` past the shorter length) has already
/// branched those paths away.
///
/// The two targets are different things, and MIR-08 is what happens when they
/// are conflated: `continue_target` advances the *innermost* sequence (a splice
/// rebinds it), while `pipeline_exit` ends the whole chain from any depth.
fn emit_step(
    b: &mut Builder<'_>,
    step: &Step,
    item: LocalId,
    continue_target: BlockId,
    pipeline_exit: BlockId,
) -> LocalId {
    match step {
        Step::Map(f) => invoke_closure(b, *f, vec![item]),
        Step::Filter(p) => {
            let keep = call_predicate(b, *p, item);
            // On false → jump to the continue target (skip this element); on
            // true → fall through to a fresh continuation block.
            let keep_blk = b.func.new_block();
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: keep,
                then_block: keep_blk,
                else_block: continue_target,
            };
            b.cur = keep_blk;
            item
        }
        Step::FilterMap(f) => {
            // `filter_map(f)` is a `filter` and a `map` at once, and the thing
            // it filters *on* is the closure's answer: `f` is `(T) -> Option[U]`
            // (REP-38), a `None` drops the element and a `Some` carries its
            // payload on down the chain.
            //
            // This used to be `invoke_closure` alone, with a comment saying
            // there was no way to tell the two apart — the row typed the
            // closure `(T) -> U` for an unconstrained `U`, so nothing at
            // runtime distinguished "mapped to nothing" from "mapped to
            // something". S18's `Option` is what closes that: the answer is a
            // two-variant enum, so the test is a tag compare, and it is the
            // same `EnumTag`/`EnumPayloadGet` pair a `match` on `Option`
            // emits — `emit_pattern_test`'s `EnumVariant` arm, unrolled for the
            // one variant set this stage knows statically.
            let opt = invoke_closure(b, *f, vec![item]);
            let tag = b.alloc_scalar(ScalarKind::Int);
            b.push(Inst::EnumTag { dst: tag, src: opt });
            let some_tag = b.alloc_scalar(ScalarKind::Int);
            b.push(Inst::ConstInt {
                dst: some_tag,
                value: praxis_runtime::enums::OPTION_SOME_TAG,
            });
            let is_some = b.alloc_scalar(ScalarKind::Bool);
            b.push(Inst::IntCmp {
                op: CmpOp::Eq,
                dst: is_some,
                lhs: tag,
                rhs: some_tag,
            });
            // `None` → advance the sequence without reaching the sink, exactly
            // as `Step::Filter` does on a false predicate.
            let keep_blk = b.func.new_block();
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: is_some,
                then_block: keep_blk,
                else_block: continue_target,
            };
            b.cur = keep_blk;
            // `Some(u)` → the element from here on is `u`, not the `Option`.
            let inner = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, None);
            b.push(Inst::EnumPayloadGet {
                dst: inner,
                src: opt,
                idx: 0,
            });
            inner
        }
        Step::TakeWhile(p) => {
            let keep = call_predicate(b, *p, item);
            let keep_blk = b.func.new_block();
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: keep,
                then_block: keep_blk,
                else_block: pipeline_exit, // predicate false → stop the stream
            };
            b.cur = keep_blk;
            item
        }
        Step::Take { bound, count } => {
            // If this stage has already passed `n` elements → stop the stream;
            // else fall through. A bound of zero or less stops before the first
            // element, and a negative `skip` drops nothing: same comparison the
            // literal-only path used, so the edge cases are the ones it had.
            let seen = take_position(b, *count);
            let stop = position_cmp_bound(b, seen, *bound, CmpOp::Ge);
            let keep_blk = b.func.new_block();
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: stop,
                then_block: pipeline_exit,
                else_block: keep_blk,
            };
            b.cur = keep_blk;
            item
        }
        Step::Skip { bound, count } => {
            // If this is among the first `n` elements to reach the stage → drop
            // it (jump to the continue target); else fall through.
            let seen = take_position(b, *count);
            let skip = position_cmp_bound(b, seen, *bound, CmpOp::Lt);
            let keep_blk = b.func.new_block();
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: skip,
                then_block: continue_target,
                else_block: keep_blk,
            };
            b.cur = keep_blk;
            item
        }
        Step::Enumerate { count, pair_ty } => {
            // Replace item with (n, item), where n is this element's position in
            // the sequence that reached `enumerate` — dense, so a `filter` in
            // front of it numbers 0, 1, 2 rather than leaving gaps. The counter
            // is a Gc Int slot already; copy it so the tuple owns a stable value.
            let idx_copy = b.alloc_gc(MirType::Known(b.int_ty), None, LocalDebugKind::Temp, None);
            b.push(Inst::MoveGc {
                dst: idx_copy,
                src: *count,
            });
            emit_increment(b, *count);
            let tup = b.alloc_gc(*pair_ty, None, LocalDebugKind::Temp, None);
            b.alloc(
                tup,
                AllocKind::Tuple {
                    // The catalog declares `enumerate`'s result `Vec[(Int, T)]`,
                    // so the pair's type is a fact the chain already carries and
                    // the backend can resolve a real element descriptor for each
                    // half (MIR-05). `Opaque` here now means only "the element
                    // type is still an inference variable", which ADR-066
                    // decision 5 answers with a null schema slot.
                    ty: *pair_ty,
                    elements: vec![idx_copy, item],
                },
            );
            tup
        }
        Step::Zip {
            other,
            count,
            pair_ty,
        } => {
            // Pair the nth element to reach `zip` with `other[n]`, stopping at
            // the shorter length. `n` is dense: after a `filter`, the surviving
            // elements pair with `other[0]`, `other[1]`, … rather than with
            // whatever the source positions happened to be.
            let stop = idx_ge_len(b, *other, *count);
            let pair_blk = b.func.new_block();
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: stop,
                then_block: pipeline_exit,
                else_block: pair_blk,
            };
            b.cur = pair_blk;
            let other_item = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, None);
            b.call_runtime(other_item, RuntimeSymbol::VecGet, vec![*other, *count]);
            emit_increment(b, *count);
            let tup = b.alloc_gc(*pair_ty, None, LocalDebugKind::Temp, None);
            b.alloc(
                tup,
                AllocKind::Tuple {
                    // As for `enumerate` above: the catalog declares `zip`'s
                    // result `Vec[(T, U)]`, so the pair's type is known.
                    ty: *pair_ty,
                    elements: vec![item, other_item],
                },
            );
            tup
        }
    }
}

/// Emit a `flat_map` splice: an inner index loop over `inner_vec`, with the rest
/// of the chain — stages *and* sink — emitted inside its body.
///
/// The recursion is the whole point (MIR-06). `rest` may itself begin with
/// another splice, and it lands here again with its own inner loop; nothing in
/// the emitter special-cases "the first flat_map" and nothing has to claim that
/// a second one cannot occur.
///
/// When the inner loop runs out, control falls to `outer_continue` — the next
/// element of whatever sequence is feeding this splice.
fn emit_splice(
    b: &mut Builder<'_>,
    inner_vec: LocalId,
    rest: &Plan,
    sink: &SinkPlan<'_>,
    outer_continue: BlockId,
    pipeline_exit: BlockId,
) {
    let inner_idx = alloc_zeroed_counter(b);

    let header = b.func.new_block();
    let body_blk = b.func.new_block();
    let inner_incr = b.func.new_block();
    let inner_exit = b.func.new_block();
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };
    b.cur = header;
    emit_bounds_check(b, inner_vec, inner_idx, body_blk, inner_exit);

    b.cur = body_blk;
    let inner_item = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, None);
    b.call_runtime(
        inner_item,
        RuntimeSymbol::VecGet,
        vec![inner_vec, inner_idx],
    );
    // The rest of the chain, per inner element. "Continue" now means the inner
    // increment — the element that advances is the inner one — but the exit is
    // still the pipeline's: a `take` or an `any` that fires in here has answered
    // for the whole chain, not for this inner Vec (MIR-08). `inner_idx` is this
    // loop's own cursor and goes no further than its bounds check and its
    // `praxis_vec_get`; the stages downstream count the flattened stream through
    // counters that were zeroed before the outer loop (MIR-07).
    emit_plan(b, rest, inner_item, sink, inner_incr, pipeline_exit);

    // Inner increment block: `inner_idx += 1`, jump to the inner header. (NOT
    // the header directly from the body — jumping there skips the increment and
    // spins the loop, the same M8-WS11 bug the outer loop guards against.)
    b.cur = inner_incr;
    emit_increment(b, inner_idx);
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: header };

    // The splice is done with this outer element: advance the sequence feeding
    // it.
    b.cur = inner_exit;
    jump_and_go_dead(b, outer_continue);
}

/// Read a stage's dense counter and bump it, returning the value *before* the
/// bump as an Int scalar: the number of elements that reached this stage ahead
/// of this one, which is this element's position in the stage's own input
/// sequence (MIR-04).
///
/// Read-then-increment, so the first element to arrive is at position zero.
fn take_position(b: &mut Builder<'_>, count: LocalId) -> LocalId {
    let seen = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ExtractScalar {
        dst: seen,
        src: count,
        scalar: ScalarKind::Int,
    });
    emit_increment(b, count);
    seen
}

/// Emit `position <op> bound` as a Bool scalar and return it. Used by
/// Take/Skip.
///
/// `bound` is a `Gc` Int slot written once before the loop, and the payload is
/// re-extracted here on every iteration rather than hoisted into a scalar the
/// loop carries: a scalar live across the body's safepoints is exactly what
/// ADR-015 §10.3 says not to build, and the extract is a load.
///
/// This replaced an `idx <op> <literal>` helper, twice over. The bound used to
/// have to *be* a literal — `classify_stage` matched `take`/`skip` only against
/// `Lit::Int`, and any other well-typed `Int` expression made the recognizer
/// decline the whole chain, which fell through to an eager lowerer with no
/// `take` arm and returned the Unit singleton for the enclosing chain to
/// misread as a Vec (MIR-03). And the left-hand side used to be the source
/// cursor rather than the stage's own count (MIR-04).
fn position_cmp_bound(
    b: &mut Builder<'_>,
    position: LocalId,
    bound: LocalId,
    op: CmpOp,
) -> LocalId {
    let bound_scalar = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ExtractScalar {
        dst: bound_scalar,
        src: bound,
        scalar: ScalarKind::Int,
    });
    let dst = b.alloc_scalar(ScalarKind::Bool);
    b.push(Inst::IntCmp {
        dst,
        op,
        lhs: position,
        rhs: bound_scalar,
    });
    dst
}

/// Emit `idx >= other.len()` as a Bool scalar (used by Zip's stop condition,
/// where `idx` is `zip`'s own dense counter rather than the source cursor).
fn idx_ge_len(b: &mut Builder<'_>, other: LocalId, idx: LocalId) -> LocalId {
    let len_dst = b.alloc_gc(MirType::Known(b.int_ty), None, LocalDebugKind::Temp, None);
    b.call_runtime(len_dst, RuntimeSymbol::VecLen, vec![other]);
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
) {
    let len_dst = b.alloc_gc(MirType::Known(b.int_ty), None, LocalDebugKind::Temp, None);
    // `praxis_vec_len` is `Effect::Allocates` — it boxes its answer and cannot
    // fault. The check this used to emit ran once per element, and it is what
    // observed a fused `sum`'s overflow *one iteration late* (ADR-088).
    b.call_runtime(len_dst, RuntimeSymbol::VecLen, vec![src]);
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
fn invoke_closure(b: &mut Builder<'_>, f: LocalId, args: Vec<LocalId>) -> LocalId {
    let dst = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, None);
    b.call_indirect(dst, f, args);
    dst
}

/// Call a `(T)->Bool` predicate closure and extract the Bool scalar.
fn call_predicate(b: &mut Builder<'_>, p: LocalId, item: LocalId) -> LocalId {
    let keep_gc = invoke_closure(b, p, vec![item]);
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
) -> (Option<LocalId>, Option<LocalId>, Option<LocalId>) {
    match sink {
        Sink::Sum | Sink::Product | Sink::Count => {
            let acc = b.alloc_scalar(ScalarKind::Int);
            let init = i64::from(matches!(sink, Sink::Product));
            b.push(Inst::ConstInt {
                dst: acc,
                value: init,
            });
            (Some(acc), None, None)
        }
        // **REP-39, ADR-082.** `find` answers the matching *element* and
        // `position` its index — two different questions, which is why §6.3
        // lists them as two operations. They shared one arm and one `-1`
        // accumulator, which made `find` an exact duplicate of `position` and
        // put an in-band sentinel under both: `-1` is a legal element of a
        // `Vec[Int]` and a legal index of nothing, so a hit and a miss were
        // indistinguishable. Both carry a seen-flag now and answer `Option`,
        // and the accumulator differs because the answer does — a `Gc` slot for
        // the element, a scalar for the index.
        Sink::Find(_) => {
            let acc = seeded_gc_accumulator(b);
            let seen = b.alloc_scalar(ScalarKind::Bool);
            b.push(Inst::ConstInt {
                dst: seen,
                value: 0,
            });
            (None, Some(acc), Some(seen))
        }
        Sink::Position(_) => {
            let acc = b.alloc_scalar(ScalarKind::Int);
            b.push(Inst::ConstInt { dst: acc, value: 0 });
            let seen = b.alloc_scalar(ScalarKind::Bool);
            b.push(Inst::ConstInt {
                dst: seen,
                value: 0,
            });
            (Some(acc), None, Some(seen))
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
            let acc = seeded_gc_accumulator(b);
            if let Some(init) = sink_init_slot {
                b.push(Inst::MoveGc {
                    dst: acc,
                    src: init,
                });
            }
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
            let acc = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, None);
            if let Some(init) = sink_init_slot {
                b.push(Inst::MoveGc {
                    dst: acc,
                    src: init,
                });
            }
            (None, Some(acc), None)
        }
        Sink::Reduce(_) => {
            // Seeded from the first element; until then it holds Unit.
            let acc = seeded_gc_accumulator(b);
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

/// A `Gc` accumulator slot for a sink that seeds from the *first* element,
/// initialized to the Unit singleton (MIR-09).
///
/// The initializer is not decoration. `reduce`/`min_by`/`max_by` only ever
/// wrote this slot from inside the loop body, so on an empty sequence nothing
/// wrote it at all — and the slot is a `Gc` local, which means the liveness
/// pass roots it at the loop header's safepoints and the backend spills
/// whatever the register happened to hold into the shadow frame for the
/// collector to dereference. Holding a valid `GcRef` from the start makes that
/// unrepresentable; [`emit_empty_collection_guard`] is what turns "we never got
/// a first element" into an answer.
fn seeded_gc_accumulator(b: &mut Builder<'_>) -> LocalId {
    let acc = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, None);
    b.push(Inst::ConstGc {
        dst: acc,
        konst: GcConst::Unit,
    });
    acc
}

/// Raise [`FaultKind::EmptyCollection`] when `seen` is false (MIR-09).
///
/// Emitted at the exit of a sink that has no answer for an empty sequence.
/// The raise is followed by the ordinary fault check, so control leaves through
/// the function's fault epilogue — which returns the Unit sentinel — rather
/// than falling through to a result that was never computed. The jump out of
/// the empty block is therefore unreachable at runtime; it exists so the block
/// has a terminator and the two paths rejoin.
fn emit_empty_collection_guard(b: &mut Builder<'_>, seen: LocalId) {
    let empty = b.func.new_block();
    let have = b.func.new_block();
    b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
        cond: seen,
        then_block: have,
        else_block: empty,
    };

    b.cur = empty;
    let sentinel = b.alloc_gc(MirType::Known(b.unit_ty), None, LocalDebugKind::Temp, None);
    b.call_runtime(sentinel, RuntimeSymbol::RaiseEmptyCollection, Vec::new());
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: have };

    b.cur = have;
}

/// Emit `slot += 1` (extract, add, re-materialize into the Gc slot). Used for a
/// loop cursor and for a stage's dense counter alike.
fn emit_increment(b: &mut Builder<'_>, idx: LocalId) {
    let cur = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ExtractScalar {
        dst: cur,
        src: idx,
        scalar: ScalarKind::Int,
    });
    let one = b.alloc_scalar(ScalarKind::Int);
    b.push(Inst::ConstInt { dst: one, value: 1 });
    let next = b.alloc_scalar(ScalarKind::Int);
    // A bump of one per element the loop visits, and `Bounded` is a claim about
    // this site (ADR-044 decision 6): a cursor is bounded by its collection's
    // length, and a stage's dense counter by the number of elements the loop can
    // deliver — which a `flat_map` makes the length of the *flattened* stream,
    // still bounded by what the process can allocate. Making these `Checked`
    // would buy nothing and cost a fault test per element.
    b.push(Inst::IntBinOp {
        dst: next,
        op: IntBinOp::Add,
        lhs: cur,
        rhs: one,
        overflow: Overflow::Bounded,
    });
    b.push(Inst::Materialize {
        dst: idx,
        src: next,
        scalar: ScalarKind::Int,
        roots: RootSlots::unannotated(),
        debug: DebugSlots::unannotated(),
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
    // `dst = src + 0` — a scalar copy, which cannot overflow.
    b.push(Inst::IntBinOp {
        dst,
        op: IntBinOp::Add,
        lhs: src,
        rhs: zero,
        overflow: Overflow::Bounded,
    });
}

/// Emit the sink's per-element update into the current (live) body block.
///
/// `pipeline_exit` is where a sink that has its answer goes — `any` once true,
/// `all` once false, `find`/`position` on a hit. It is the *whole* chain's exit
/// even when this element arrived through a splice: a sink that stopped only the
/// inner loop would go on evaluating its predicate on elements after the answer
/// was decided, and would let a later match overwrite `find`'s (MIR-08).
/// `continue_target` is the innermost increment, which is where `reduce`'s
/// first-element seed goes: seeding advances the stream by one element, it does
/// not end it.
fn emit_sink_body(
    b: &mut Builder<'_>,
    plan: &SinkPlan<'_>,
    item: LocalId,
    continue_target: BlockId,
    pipeline_exit: BlockId,
) {
    let SinkPlan {
        sink,
        acc_scalar,
        acc_gc,
        seen_flag,
        collect_vec,
        closure: sink_closure_slot,
        position,
    } = *plan;
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
                overflow: Overflow::Checked,
            });
            // **The accumulator ADR-044 named as the reason this rule could not
            // exist.** It is `Checked` — a `sum` genuinely can overflow — and it
            // had no check, so the fault was observed one iteration later, by
            // the loop header's `praxis_vec_len` check, which REP-53 has now
            // deleted. Per-element is what §10.4's "immediately after" means,
            // and it is what makes the overflow divert *at the addition*: the
            // crash snapshot then shows the operands that overflowed rather than
            // the next element's. The cost is roughly the check just deleted
            // from the same loop's header.
            b.check_fault();
        }
        Sink::Count => {
            let acc = acc_scalar.unwrap();
            let one = b.alloc_scalar(ScalarKind::Int);
            b.push(Inst::ConstInt { dst: one, value: 1 });
            // `count += 1`, bounded by the source collection's length.
            b.push(Inst::IntBinOp {
                dst: acc,
                op: IntBinOp::Add,
                lhs: acc,
                rhs: one,
                overflow: Overflow::Bounded,
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
                invoke_closure(b, f, vec![item, acc])
            } else {
                invoke_closure(b, f, vec![acc, item])
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
            let keep = call_predicate(b, pred, item);
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
            // trip: set acc, end the pipeline.
            b.cur = trip_blk;
            let val = if matches!(sink, Sink::Any(_)) { 1 } else { 0 };
            b.push(Inst::ConstInt {
                dst: acc,
                value: val,
            });
            jump_and_go_dead(b, pipeline_exit);
            b.cur = cont_blk;
        }
        // **REP-39.** One search, two answers. Both stop at the first match and
        // both raise the seen-flag; what they record differs, and that is the
        // whole difference between the two operations §6.3 names.
        Sink::Find(_) | Sink::Position(_) => {
            let count = position.expect("find/position carry a dense counter");
            let seen = seen_flag.expect("find/position carry a seen flag");
            let pred = sink_closure_slot.unwrap();
            let keep = call_predicate(b, pred, item);
            let found_blk = b.func.new_block();
            let cont_blk = b.func.new_block();
            b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
                cond: keep,
                then_block: found_blk,
                else_block: cont_blk,
            };
            b.cur = found_blk;
            match sink {
                // `find` answers the element that matched.
                Sink::Find(_) => {
                    b.push(Inst::MoveGc {
                        dst: acc_gc.unwrap(),
                        src: item,
                    });
                }
                // `position` answers where it was: the counter's value before
                // this element was counted.
                _ => {
                    let position_scalar = b.alloc_scalar(ScalarKind::Int);
                    b.push(Inst::ExtractScalar {
                        dst: position_scalar,
                        src: count,
                        scalar: ScalarKind::Int,
                    });
                    move_scalar(b, acc_scalar.unwrap(), position_scalar);
                }
            }
            b.push(Inst::ConstInt {
                dst: seen,
                value: 1,
            });
            jump_and_go_dead(b, pipeline_exit);
            // On a miss, count the element and go on. (There is no bump on the
            // hit path because that path leaves the pipeline.)
            b.cur = cont_blk;
            emit_increment(b, count);
        }
        Sink::Fold { .. } => {
            let acc = acc_gc.unwrap();
            let f = sink_closure_slot.unwrap();
            let new_acc = invoke_closure(b, f, vec![acc, item]);
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
            jump_and_go_dead(b, continue_target);
            b.cur = fold_blk;
            let new_acc = invoke_closure(b, f, vec![acc, item]);
            b.push(Inst::MoveGc {
                dst: acc,
                src: new_acc,
            });
        }
        Sink::Collect => {
            // **REP-52.** `praxis_vec_push` is `AllocatesAndFaults`: it raises
            // `TYPE_MISMATCH` through `adopt_or_reject` when the pushed value's
            // descriptor disagrees with the Vec's. Every sibling sink arm and
            // the eager `v.push(x)` path checked; this one did not, and the
            // fault would have been observed at whatever the next check happened
            // to be. `call_runtime` reads the row.
            //
            // No source program reaches that fault today: `alloc_empty_vec`
            // gives the collect target a null element descriptor, so it adopts
            // the first pushed value's, and inference refuses a heterogeneous
            // chain (`Y001`). The gate for this is therefore the verifier rule,
            // not an end-to-end fault — see
            // `a_fused_collect_observes_its_push`.
            let result = collect_vec.unwrap();
            let unit = b.alloc_gc(MirType::Known(b.unit_ty), None, LocalDebugKind::Temp, None);
            b.call_runtime(unit, RuntimeSymbol::VecPush, vec![result, item]);
        }
    }
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

/// Terminate the current block with a jump to `target` and leave `b.cur` on a
/// fresh dead block, so a caller that keeps emitting has somewhere to append.
///
/// This replaced a pair of `break_loop`/`continue_loop` helpers that read
/// `b.loop_stack.last()`. Reading the *innermost* loop is exactly what a
/// pipeline must not do: a chain has one exit however deeply it nests, and the
/// innermost stack frame inside a `flat_map` splice named the inner loop's exit
/// (MIR-08). The emitter carries both targets explicitly now.
fn jump_and_go_dead(b: &mut Builder<'_>, target: BlockId) {
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target };
    b.cur = b.func.new_block();
}

/// Materialize the sink's final result out of its accumulator(s).
#[allow(clippy::too_many_arguments)]
fn sink_finish(
    b: &mut Builder<'_>,
    sink: &Sink,
    acc_scalar: Option<LocalId>,
    acc_gc: Option<LocalId>,
    seen_flag: Option<LocalId>,
    collect_vec: Option<LocalId>,
    ty: Type,
) -> LocalId {
    match sink {
        Sink::Collect => collect_vec.unwrap(),
        // `fold` always has an answer: its accumulator starts at `init`.
        Sink::Fold { .. } => acc_gc.unwrap(),
        // MIR-09. These three seed from the first element, so on an empty
        // sequence there is no answer and the accumulator was never written.
        // Fault rather than hand back an unwritten `Gc` slot.
        Sink::Reduce(_) | Sink::MinBy(_) | Sink::MaxBy(_) => {
            let acc = acc_gc.unwrap();
            emit_empty_collection_guard(b, seen_flag.expect("seeded sinks carry a seen flag"));
            acc
        }
        Sink::Any(_) | Sink::All(_) => {
            let acc = acc_scalar.unwrap();
            let dst = b.alloc_gc(MirType::Known(b.bool_ty), None, LocalDebugKind::Temp, None);
            b.push(Inst::Materialize {
                dst,
                src: acc,
                scalar: ScalarKind::Bool,
                roots: RootSlots::unannotated(),
                debug: DebugSlots::unannotated(),
            });
            dst
        }
        // **D1.** `min`/`max` are the scalar siblings of the three above and
        // share the empty case. Their accumulator is *seeded* with `0` rather
        // than left unwritten, so the empty sequence had a defined answer — and
        // `0` is a **wrong** answer, not a missing one: it is smaller than every
        // element of `[3, 4]` and larger than every element of `[-3, -4]`, so a
        // caller cannot tell it from a real minimum. D1 settled that they join
        // the three seeded sinks rather than becoming `Option`, because an empty
        // `min` is a caller mistake and not the ordinary absence §4.7 is about.
        Sink::Min | Sink::Max => {
            emit_empty_collection_guard(b, seen_flag.expect("min/max carry a seen flag"));
            let acc = acc_scalar.unwrap();
            let dst = b.alloc_gc(MirType::Known(b.int_ty), None, LocalDebugKind::Temp, None);
            b.push(Inst::Materialize {
                dst,
                src: acc,
                scalar: ScalarKind::Int,
                roots: RootSlots::unannotated(),
                debug: DebugSlots::unannotated(),
            });
            dst
        }
        // These three always have an answer on an empty sequence, and it is the
        // right one: `0`, `1`, `0`.
        Sink::Sum | Sink::Product | Sink::Count => {
            let acc = acc_scalar.unwrap();
            let dst = b.alloc_gc(MirType::Known(b.int_ty), None, LocalDebugKind::Temp, None);
            b.push(Inst::Materialize {
                dst,
                src: acc,
                scalar: ScalarKind::Int,
                roots: RootSlots::unannotated(),
                debug: DebugSlots::unannotated(),
            });
            dst
        }
        // **REP-39, ADR-082.** A search that found nothing answers `None`, not a
        // number. `-1` was in band for both: a legal element of a `Vec[Int]` and
        // a legal `Int` besides, so no program could tell a hit from a miss —
        // and `find`, whose element type is `Text` as often as not, could not
        // reach its answer at all.
        Sink::Find(_) => {
            let found = acc_gc.expect("find carries a Gc accumulator");
            emit_option_of(b, seen_flag.expect("find carries a seen flag"), found, ty)
        }
        Sink::Position(_) => {
            let acc = acc_scalar.expect("position carries a scalar accumulator");
            let idx = b.alloc_gc(MirType::Known(b.int_ty), None, LocalDebugKind::Temp, None);
            b.push(Inst::Materialize {
                dst: idx,
                src: acc,
                scalar: ScalarKind::Int,
                roots: RootSlots::unannotated(),
                debug: DebugSlots::unannotated(),
            });
            emit_option_of(b, seen_flag.expect("position carries a seen flag"), idx, ty)
        }
    }
}

/// `if seen { Some(value) } else { None }`, as the sink's `Option`-typed answer
/// (REP-39, ADR-082).
///
/// `result_ty` is the sink's own static type — `Option[Text]`, not `Option` —
/// because the backend resolves the `Some` payload's descriptor from it.
///
/// Both arms write one `Gc` slot rather than the two branches producing
/// separate locals: MIR is not SSA, and this is the shape `Sink::Fold` already
/// uses for an accumulator that several blocks assign.
fn emit_option_of(b: &mut Builder<'_>, seen: LocalId, value: LocalId, result_ty: Type) -> LocalId {
    let mir_ty = MirType::Known(result_ty);
    let def = b.db.option_def().to_u32();
    let dst = b.alloc_gc(mir_ty, None, LocalDebugKind::Temp, None);

    let some_blk = b.func.new_block();
    let none_blk = b.func.new_block();
    let join_blk = b.func.new_block();
    b.func.blocks[b.cur.0 as usize].term = Terminator::Branch {
        cond: seen,
        then_block: some_blk,
        else_block: none_blk,
    };

    b.cur = some_blk;
    b.alloc(
        dst,
        AllocKind::Enum {
            enum_def_id: def,
            variant_idx: OPTION_SOME_VARIANT,
            ty: mir_ty,
            args: vec![value],
        },
    );
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: join_blk };

    b.cur = none_blk;
    b.alloc(
        dst,
        AllocKind::Enum {
            enum_def_id: def,
            variant_idx: OPTION_NONE_VARIANT,
            ty: mir_ty,
            args: Vec::new(),
        },
    );
    b.func.blocks[b.cur.0 as usize].term = Terminator::Jump { target: join_blk };

    b.cur = join_blk;
    dst
}

/// `Some`'s and `None`'s discriminants in the prelude's one `Option` def, which
/// `TypeDb::new` registers in that order and the runtime's `OPTION_SOME_TAG` /
/// `OPTION_NONE_TAG` agree with.
const OPTION_SOME_VARIANT: u32 = praxis_runtime::enums::OPTION_SOME_TAG as u32;
const OPTION_NONE_VARIANT: u32 = praxis_runtime::enums::OPTION_NONE_TAG as u32;

/// Allocate the empty Vec a pipeline collects into. `result_ty` is the
/// pipeline's own result type when the lowering has one.
///
/// This is an `AllocKind::Collection`, like every other collection
/// construction. Before P0-03 it hand-rolled a `praxis_vec_new` call whose
/// element-descriptor argument was the integer `0` moved into a `Gc` slot — the
/// second and last site where a raw non-pointer word inhabited a rootable slot.
/// The element type stays [`MirType::Opaque`] here, which the backend turns
/// into the same null descriptor the wrapper already expects: the result Vec
/// adopts each pushed value's descriptor on first push.
///
/// The *reason* it is not derived from `result_ty` has changed, twice. S15
/// could not read it because the method catalog described `enumerate` and `zip`
/// wrongly — both rows declared `result: Vec[T]`, the receiver's own element
/// type — so a chain ending in either had a result type that would have named
/// the wrong element descriptor. TY-31 fixed the rows and S21's MIR-05 made the
/// fused lowering read them, so `result_ty` is both `Known` and *right* at every
/// call site now.
///
/// What is left is a smaller claim: the null descriptor is not a gap, it is how
/// a `Vec` is built. `praxis_vec_new` adopts the first pushed value's
/// descriptor, so an empty collect-target has no descriptor to state and the
/// element type would only be re-derived at the first push. Deriving it here
/// would be a change to collection construction — one that has to answer what
/// happens when the two disagree — and belongs with whatever makes the element
/// descriptor authoritative rather than adopted. See `praxis_mir::verify`'s
/// note on H10.
fn alloc_empty_vec(b: &mut Builder<'_>, result_ty: MirType) -> LocalId {
    let result = b.alloc_gc(result_ty, None, LocalDebugKind::Temp, None);
    b.alloc(
        result,
        AllocKind::Collection {
            ctor: praxis_types::CollectionCtor::Vec,
            args: vec![MirType::Opaque],
        },
    );
    result
}

/// How a `for` reaches the members of the thing it iterates (REP-15, ADR-066).
///
/// Only four of the eleven iterables answer "the member at `i`" in constant
/// time (`Vec`, `Deque`, `Range`, and `Text`). The rest have no nth member to
/// ask for at all: a `HashSet` would need a linear scan per step, a heap's
/// backing array is heap-ordered only at its root, and `MapGet`/`CounterGet`
/// take a *key*. So the loop takes a **snapshot** — one runtime call before the
/// header, answering a `Vec` — and indexes that.
///
/// The distinction this enum makes is the one the defect was made of: reading a
/// `Set`'s payload through `praxis_vec_get` was a wrong-type read that hung or
/// killed the process, and a `MinHeap`'s was a silently wrong answer. There is no
/// default arm here for the same reason — a new collection ctor is a compile
/// error until someone says how it iterates.
#[derive(Clone, Copy)]
enum IterPlan {
    /// The iterable indexes itself in constant time: `(len, get)`.
    InPlace {
        len: RuntimeSymbol,
        get: RuntimeSymbol,
    },
    /// One call materializes every member as a `Vec`, and the loop walks that.
    Snapshot(RuntimeSymbol),
    /// Two **index-aligned** calls materialize the keys and the values (REP-18's
    /// rows share one ordering, which is what makes them aligned), and the loop
    /// pairs them per step into the `(K, V)` tuple the item type names.
    Paired {
        keys: RuntimeSymbol,
        values: RuntimeSymbol,
    },
}

impl IterPlan {
    /// The symbol the header calls for the member count. A snapshot's count is
    /// its `Vec`'s, not the source collection's — they agree, and asking the
    /// `Vec` is what keeps the two sides of `i < len` about one object.
    fn len_symbol(self) -> RuntimeSymbol {
        match self {
            IterPlan::InPlace { len, .. } => len,
            IterPlan::Snapshot(_) | IterPlan::Paired { .. } => RuntimeSymbol::VecLen,
        }
    }

    /// The symbol the body calls for the member at the current index.
    fn get_symbol(self) -> RuntimeSymbol {
        match self {
            IterPlan::InPlace { get, .. } => get,
            IterPlan::Snapshot(_) | IterPlan::Paired { .. } => RuntimeSymbol::VecGet,
        }
    }
}

/// The [`IterPlan`] for an iterable expression, by its static collection ctor —
/// or, for the one iterable scalar, by its scalar type.
///
/// Any other static type cannot reach here from a well-typed program —
/// `capability::iter_item` answers `None` for one and the `for` is a `Y005` — so
/// the fallback is the shape MIR has always assumed rather than a panic.
fn iter_plan(db: &TypeDb, iter: &TypedExpr) -> IterPlan {
    use praxis_types::data::TypeData;
    use praxis_types::CollectionCtor as C;
    use praxis_types::ScalarType;
    let ty = expr_static_type(iter);
    let ctor = match db.data(db.follow(ty)) {
        TypeData::Collection { ctor, .. } => ctor,
        // A `Text` indexes itself, so it needs no snapshot: `praxis_text_len`
        // counts Unicode scalars and `praxis_text_get` answers the `Char` at one
        // (§4.13, ADR-086). They are the pair `t.len()` and `t[i]` already call,
        // which is what makes the loop and the subscript one answer.
        TypeData::Scalar(ScalarType::Text) => {
            return IterPlan::InPlace {
                len: RuntimeSymbol::TextLen,
                get: RuntimeSymbol::TextGet,
            }
        }
        _ => {
            return IterPlan::InPlace {
                len: RuntimeSymbol::VecLen,
                get: RuntimeSymbol::VecGet,
            }
        }
    };
    match ctor {
        C::Vec | C::Seq => IterPlan::InPlace {
            len: RuntimeSymbol::VecLen,
            get: RuntimeSymbol::VecGet,
        },
        C::Deque => IterPlan::InPlace {
            len: RuntimeSymbol::DequeLen,
            get: RuntimeSymbol::DequeGet,
        },
        C::Range => IterPlan::InPlace {
            len: RuntimeSymbol::RangeLen,
            get: RuntimeSymbol::RangeGet,
        },
        C::Set => IterPlan::Snapshot(RuntimeSymbol::SetItems),
        C::BitSet => IterPlan::Snapshot(RuntimeSymbol::BitsetItems),
        C::MinHeap => IterPlan::Snapshot(RuntimeSymbol::MinHeapItems),
        C::MaxHeap => IterPlan::Snapshot(RuntimeSymbol::MaxHeapItems),
        // A grid yields its cells, row-major (§6.4). `GridGet` takes `(x, y)`,
        // so there is no one-index accessor to walk in place.
        C::Grid => IterPlan::Snapshot(RuntimeSymbol::GridCells),
        C::Map => IterPlan::Paired {
            keys: RuntimeSymbol::MapKeys,
            values: RuntimeSymbol::MapValues,
        },
        C::Counter => IterPlan::Paired {
            keys: RuntimeSymbol::CounterKeys,
            values: RuntimeSymbol::CounterValues,
        },
    }
}

/// Emit `sym(source)` and answer the `Vec` local it lands in: one member-list
/// snapshot for [`IterPlan::Snapshot`] or [`IterPlan::Paired`].
///
/// The result is a `Gc` slot like any other, so the loop's own liveness roots it
/// for the loop's duration — which it must be, since the body allocates.
fn snapshot(b: &mut Builder<'_>, source: LocalId, sym: RuntimeSymbol) -> LocalId {
    let dst = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, None);
    // All nine member-list wrappers are `Effect::Allocates`: materializing a
    // collection's members into a `Vec` cannot fail. `call_runtime` says so.
    b.call_runtime(dst, sym, vec![source]);
    dst
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

/// Map a compound operator to its Float equivalent (§4.12).
///
/// `%=` answers `None`: `%` is not defined for `Float`, there is no MIR
/// instruction for it, and inference reports `Y016` before this is reached. The
/// `Assign` arm answers `None` for a different reason — a plain `=` is not an
/// arithmetic operation and is handled before any of this.
fn op_to_float_binop(op: AssignOp) -> Option<FloatBinOp> {
    Some(match op {
        AssignOp::AddAssign => FloatBinOp::Add,
        AssignOp::SubAssign => FloatBinOp::Sub,
        AssignOp::MulAssign => FloatBinOp::Mul,
        AssignOp::DivAssign => FloatBinOp::Div,
        AssignOp::RemAssign | AssignOp::Assign => return None,
    })
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
        | TypedExpr::FnValue { ty, .. }
        | TypedExpr::Bin { ty, .. }
        | TypedExpr::Range { ty, .. }
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
        | TypedExpr::ListLit { ty, .. }
        | TypedExpr::Read { ty, .. }
        | TypedExpr::Parse { ty, .. }
        | TypedExpr::RecordLit { ty, .. }
        | TypedExpr::FieldGet { ty, .. }
        | TypedExpr::TupleIndex { ty, .. }
        | TypedExpr::EnumVariant { ty, .. }
        | TypedExpr::Match { ty, .. }
        | TypedExpr::Closure { ty, .. } => *ty,
        TypedExpr::Block(blk) => blk.ty,
    }
}

/// The runtime symbol a §16.1 output/control builtin lowers to, or `None` for
/// any other callee name.
///
/// `out` is not here: it has its own path above because it returns the Unit
/// sentinel rather than its argument, and its result type is the call's. The
/// three that are here share one shape — one `GcRef` in, one `GcRef` out —
/// which is why they are one row rather than three branches.
fn control_builtin_symbol(callee_name: &str) -> Option<RuntimeSymbol> {
    match callee_name {
        "dbg" => Some(RuntimeSymbol::Dbg),
        "panic" => Some(RuntimeSymbol::Panic),
        "assert" => Some(RuntimeSymbol::Assert),
        _ => None,
    }
}

/// `[ a, b, … ]` — a `Vec` literal (§6.1). Allocates the `Vec`, then pushes each
/// element into it, left to right.
///
/// The allocation comes **first**, so the `Vec` is live across every element
/// expression and the loop's own liveness roots it — which it must be, since an
/// element may allocate (`[Point { x: 1 }, f()]`). Evaluation still runs left to
/// right, because each element is lowered immediately before its own push rather
/// than all of them up front.
///
/// This is `Vec[T]()` followed by `v.push(…)`, emitted directly: the same
/// `AllocKind::Collection` a constructor call builds and the same
/// `praxis_vec_push` a method call resolves to. A literal is a *spelling* for
/// those two operations, so it lowers to them rather than to anything new — no
/// runtime wrapper, no ABI change, and a `Vec` built either way is one kind of
/// object.
fn lower_list_lit(b: &mut Builder<'_>, elements: &[TypedExpr], ty: Type, e: &TypedExpr) -> LocalId {
    use praxis_types::data::TypeData;
    use praxis_types::CollectionCtor;
    // The element type, from the literal's own `Vec[T]`. An element type that is
    // still an inference variable — `let v = []`, whose use decides it — reaches
    // the backend as a null descriptor, which is what `praxis_vec_new`'s
    // "unknown element" contract already means (H10).
    let args: Vec<MirType> = match b.db.data(b.db.follow(ty)) {
        TypeData::Collection {
            ctor: CollectionCtor::Vec,
            args: a,
        } => a.iter().copied().map(MirType::Known).collect(),
        _ => Vec::new(),
    };
    let dst = b.alloc_gc(
        MirType::Known(ty),
        None,
        LocalDebugKind::Temp,
        Some(praxis_hir::expr_span(e)),
    );
    b.alloc(
        dst,
        AllocKind::Collection {
            ctor: CollectionCtor::Vec,
            args,
        },
    );
    for el in elements {
        let item = lower_expr_gc(b, el);
        let unit = b.alloc_gc(MirType::Known(b.unit_ty), None, LocalDebugKind::Temp, None);
        b.call_runtime(unit, RuntimeSymbol::VecPush, vec![dst, item]);
    }
    dst
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
    // shape (a malformed call), fall back to an empty arg list — the codegen
    // reads a missing argument as an unknown element type and passes a null
    // descriptor, same as before.
    let args: Vec<MirType> = match b.db.data(b.db.follow(result_ty)) {
        TypeData::Collection {
            ctor: c,
            args: ref a,
        } if *c == ctor => a.iter().copied().map(MirType::Known).collect(),
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
    let dst = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, None);
    b.alloc(
        dst,
        AllocKind::Record {
            record_def_id: record_def_id.to_u32(),
            fields: field_locals,
        },
    );
    dst
}

/// Lower a field access `receiver.field` (M7, §4.5). Emits a `LoadField`
/// instruction that reads the field's `GcRef` out of the record payload.
fn lower_field_get(b: &mut Builder<'_>, receiver: &TypedExpr, field_idx: u32) -> LocalId {
    let src = lower_expr_gc(b, receiver);
    let dst = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, None);
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
    ty: praxis_types::Type,
    args: &[TypedExpr],
) -> LocalId {
    let arg_locals: Vec<LocalId> = args.iter().map(|a| lower_expr_gc(b, a)).collect();
    let mir_ty = MirType::Known(ty);
    let dst = b.alloc_gc(mir_ty, None, LocalDebugKind::Temp, None);
    b.alloc(
        dst,
        AllocKind::Enum {
            enum_def_id: enum_def_id.to_u32(),
            variant_idx,
            ty: mir_ty,
            args: arg_locals,
        },
    );
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
    let result = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, None);
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
                // Both operands are read at the payload's *own* width. `Bool`
                // used to share the `Int` arm, which emitted an
                // `ExtractScalar { scalar: Int }` — `praxis_int_load`, an
                // eight-byte read — against a **one**-byte `BoolPayload`. The
                // other seven bytes are the block's alignment padding, so
                // `true` and `false` were told apart by uninitialized memory
                // and compared equal whenever two immortals happened to have
                // matching padding (REP-49; REP-37 is the same defect in the
                // graph oracle). `praxis_bool_load` reads the byte.
                Lit::Int(_) | Lit::Bool(_) => {
                    let kind = if matches!(value, Lit::Bool(_)) {
                        ScalarKind::Bool
                    } else {
                        ScalarKind::Int
                    };
                    let si = lower_extract_scalar(b, scrut, kind);
                    let li = lower_extract_scalar(b, lit_gc, kind);
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
            emit_subpattern_tests(
                b,
                scrut,
                subpatterns,
                Component::EnumPayload,
                0,
                on_success,
                on_fail,
            );
        }
        // `(a, b)` and `P { x, y }` (REP-10). Neither tests the value itself: a
        // tuple type and a record type have one constructor each, so the shape
        // always matches and the whole test is the components'. The only
        // difference between them is which instruction reads a component.
        TypedPattern::Tuple { subpatterns, .. } => {
            emit_subpattern_tests(
                b,
                scrut,
                subpatterns,
                Component::TupleElem,
                0,
                on_success,
                on_fail,
            );
        }
        TypedPattern::Record { subpatterns, .. } => {
            emit_subpattern_tests(
                b,
                scrut,
                subpatterns,
                Component::RecordField,
                0,
                on_success,
                on_fail,
            );
        }
    }
}

/// Which part of a composite value a sub-pattern is tested against, and so which
/// instruction reads it (REP-10).
///
/// The three are one walk because the chaining is identical — every component
/// must match, and any failure leaves by the same edge — and three copies of
/// that block structure is three places for a fall-through to go missing.
#[derive(Clone, Copy)]
enum Component {
    /// An enum variant's payload slot.
    EnumPayload,
    /// A tuple's element, by position.
    TupleElem,
    /// A record's field, by declaration index — which is what makes a record
    /// pattern's sub-patterns positional in HIR.
    RecordField,
}

impl Component {
    /// Read component `idx` of `src` into `dst`.
    fn load(self, dst: LocalId, src: LocalId, idx: u32) -> Inst {
        match self {
            Component::EnumPayload => Inst::EnumPayloadGet { dst, src, idx },
            Component::TupleElem => Inst::LoadTupleElem {
                dst,
                src,
                index: idx,
            },
            Component::RecordField => Inst::LoadField {
                dst,
                src,
                field_idx: idx,
            },
        }
    }
}

/// Bind the names an **irrefutable** pattern holds, reading whatever components
/// it names out of `src` (REP-25).
///
/// The binding half of [`emit_pattern_test`] with the testing half removed: a
/// `for` header has no second arm, so a pattern that can fail never reaches here
/// (HIR reports `Y125`) and there is nothing to branch on. A `Wildcard` binds
/// nothing and therefore reads nothing, exactly as it does in a match.
fn bind_components(b: &mut Builder<'_>, src: LocalId, pat: &praxis_hir::TypedPattern) {
    use praxis_hir::TypedPattern;
    let (subpatterns, component) = match pat {
        // The whole item, already in `src` — its slot is the caller's.
        TypedPattern::Wildcard | TypedPattern::Bind { .. } => return,
        // Refutable, and reported. Binding nothing is what keeps a reported
        // program from also reading a component of a value that may not have
        // one.
        TypedPattern::Lit { .. } | TypedPattern::EnumVariant { .. } => return,
        TypedPattern::Tuple { subpatterns, .. } => (subpatterns, Component::TupleElem),
        TypedPattern::Record { subpatterns, .. } => (subpatterns, Component::RecordField),
    };
    for (idx, sub) in subpatterns.iter().enumerate() {
        if matches!(sub, TypedPattern::Wildcard) {
            continue;
        }
        let idx = idx as u32;
        let component_local = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, None);
        let inst = component.load(component_local, src, idx);
        b.push(inst);
        match sub {
            // A name the loop body reads: its own slot, so the debugger shows it
            // as the user local it is.
            TypedPattern::Bind { symbol, .. } => {
                let slot = b.locals.get(symbol).copied().unwrap_or_else(|| {
                    b.alloc_gc(MirType::Opaque, None, LocalDebugKind::User, None)
                });
                b.locals.insert(*symbol, slot);
                b.push(Inst::MoveGc {
                    dst: slot,
                    src: component_local,
                });
            }
            _ => bind_components(b, component_local, sub),
        }
    }
}

/// Recursively test a chain of sub-patterns against consecutive components of
/// `scrut`, starting at `slot_idx`. All must succeed to reach `on_success`; any
/// failure jumps to `on_fail`.
fn emit_subpattern_tests(
    b: &mut Builder<'_>,
    scrut: LocalId,
    subpatterns: &[praxis_hir::TypedPattern],
    component: Component,
    slot_idx: u32,
    on_success: BlockId,
    on_fail: BlockId,
) {
    if let Some(sub) = subpatterns.get(slot_idx as usize) {
        // A wildcard asks nothing of the component, so the component is not
        // read: the row lowering padded to arity costs nothing, and `Some` reads
        // no payload where `Some(n)` reads one. Only a `Wildcard` may be skipped
        // — a `Bind` matches anything too, but it needs the value.
        if matches!(sub, praxis_hir::TypedPattern::Wildcard) {
            emit_subpattern_tests(
                b,
                scrut,
                subpatterns,
                component,
                slot_idx + 1,
                on_success,
                on_fail,
            );
            return;
        }
        // Extract this component into a local.
        let payload = b.alloc_gc(MirType::Opaque, None, LocalDebugKind::Temp, None);
        let inst = component.load(payload, scrut, slot_idx);
        b.push(inst);
        // Test `sub` against `payload`. If it matches, continue to the next
        // sub-pattern; if not, fail.
        let next = b.func.new_block();
        emit_pattern_test(b, payload, sub, next, on_fail);
        b.cur = next;
        emit_subpattern_tests(
            b,
            scrut,
            subpatterns,
            component,
            slot_idx + 1,
            on_success,
            on_fail,
        );
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

    /// **P0-02, the half F15 unblocked.** A `for` binding's slot and the item
    /// it holds are the iterator's element type. Both were `MirType::Opaque` —
    /// the honest answer while lowering had no per-use type to give them, and a
    /// `for` binding the debugger showed with no type at all.
    #[test]
    fn a_for_bindings_slot_carries_the_iterators_element_type() {
        let (funcs, analysis) = lower_src_to_mir(
            "fn f(v: Vec[Int]) -> Int {\n  var s = 0\n  for x in v { s = s + x }\n  s\n}",
        );
        let f = &funcs[0];
        let named: Vec<String> = f
            .locals
            .iter()
            .filter(|l| matches!(l.kind, LocalKind::Gc))
            .filter_map(|l| l.ty.known())
            .map(|t| analysis.db.render(t))
            .collect();
        assert!(
            named.iter().any(|t| t == "Int"),
            "the item and the binding slot are `Int`: {named:?}"
        );
        // The binding's own slot is one of them: `x` reads as `Int` in the
        // debugger, where it used to have no type column at all.
        let ints = named.iter().filter(|t| *t == "Int").count();
        assert!(
            ints >= 2,
            "the item temp and the binding slot both: {named:?}"
        );
    }

    /// **P0-02.** A closure value's local is its `Func` type, and an indirect
    /// call's result is the call's. Neither had a type before F15 recorded one.
    #[test]
    fn a_closure_and_its_indirect_call_carry_their_types() {
        let (funcs, analysis) =
            lower_src_to_mir("fn f() -> Int {\n  let g = |n| n + 1\n  g(41)\n}");
        let rendered: Vec<String> = funcs[0]
            .locals
            .iter()
            .filter_map(|l| l.ty.known())
            .map(|t| analysis.db.render(t))
            .collect();
        assert!(
            rendered.iter().any(|t| t == "(Int) -> Int"),
            "the closure value's own type: {rendered:?}"
        );
    }

    /// **REP-01, ADR-061.** A top-level `fn` in value position allocates a
    /// closure over an adapter, and the adapter is one per function however many
    /// times it is used.
    ///
    /// The adapter is the part the plan's sketch did not have: a closure's
    /// synthetic function takes the closure as a hidden first argument and a
    /// top-level `fn` does not, so handing the `fn`'s own address to
    /// `praxis_alloc_closure` would shift every argument one slot left. Its
    /// params are `[closure_self, forwarded…]` and its body is one direct call.
    #[test]
    fn a_fn_used_as_a_value_gets_one_adapter_per_function() {
        let (funcs, analysis) = lower_src_to_mir(
            "fn double(n: Int) -> Int { n * 2 }\n\
             fn main() -> Int {\n  let f = double\n  let g = double\n  f(1) + g(2)\n}",
        );
        let adapters: Vec<&Function> = funcs
            .iter()
            .filter(|f| f.name.starts_with("__fnvalue_"))
            .collect();
        assert_eq!(
            adapters.len(),
            1,
            "two uses of one function share its adapter: {:?}",
            funcs.iter().map(|f| &f.name).collect::<Vec<_>>()
        );
        let adapter = adapters[0];
        assert_eq!(adapter.name, "__fnvalue_double");
        // `[closure_self, n]` — the self slot the convention requires, plus the
        // one parameter `double` actually declares.
        assert_eq!(
            adapter.params.len(),
            2,
            "the hidden self slot plus one forwarded argument"
        );
        // The body forwards to the real function, and forwards only the
        // parameters — the self slot is dropped, which is the whole job.
        let calls: Vec<&Inst> = adapter
            .blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .filter(|i| matches!(i, Inst::Call { .. }))
            .collect();
        assert_eq!(calls.len(), 1, "one direct call: {calls:?}");
        let Inst::Call { callee, args, .. } = calls[0] else {
            unreachable!()
        };
        assert!(
            matches!(callee, CallTarget::User(n) if n == "double"),
            "the adapter calls the function it adapts: {callee:?}"
        );
        assert_eq!(args, &vec![adapter.params[1]], "the forwarded parameter");

        // And the use site is a closure allocation with an empty environment,
        // not the `Unit` a `Path` to a `fn` used to lower to.
        let main = funcs.iter().find(|f| f.name == "main").expect("main");
        let allocs: Vec<&AllocKind> = main
            .blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .filter_map(|i| match i {
                Inst::Alloc { alloc, .. } => Some(alloc),
                _ => None,
            })
            .filter(|a| matches!(a, AllocKind::Closure { .. }))
            .collect();
        assert_eq!(allocs.len(), 2, "one per use: {allocs:?}");
        for a in allocs {
            let AllocKind::Closure { fn_name, captures } = a else {
                unreachable!()
            };
            assert_eq!(fn_name, "__fnvalue_double");
            assert!(captures.is_empty(), "a top-level `fn` captures nothing");
        }
        let _ = analysis;
    }

    /// …and a *direct* call is still a direct call. `lower_call` resolves a named
    /// callee itself, so it never comes through the path lowering — if it did,
    /// every call in every program would allocate a closure first.
    #[test]
    fn a_direct_call_does_not_go_through_a_function_value() {
        let (funcs, _analysis) =
            lower_src_to_mir("fn double(n: Int) -> Int { n * 2 }\nfn main() -> Int { double(21) }");
        assert!(
            !funcs.iter().any(|f| f.name.starts_with("__fnvalue_")),
            "no adapter: {:?}",
            funcs.iter().map(|f| &f.name).collect::<Vec<_>>()
        );
        let main = funcs.iter().find(|f| f.name == "main").expect("main");
        assert!(
            main.blocks.iter().flat_map(|b| b.insts.iter()).any(|i| {
                matches!(i, Inst::Call { callee: CallTarget::User(n), .. } if n == "double")
            }),
            "still one direct call to `double`"
        );
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

    /// An `Int` literal the runtime interns lowers to [`Inst::ConstGc`], and
    /// that instruction is **not** a GC safepoint.
    ///
    /// This is §3.5's whole fix at the MIR level: `1` in a loop body used to be
    /// `ConstInt` + `Alloc { Int }`, and the `Alloc` made the site a safepoint —
    /// a call to `praxis_alloc_int` with every live root spilled into the shadow
    /// frame before it, on every iteration.
    #[test]
    fn a_small_int_literal_is_a_const_gc_and_not_a_safepoint() {
        let (funcs, _analysis) = lower_src_to_mir("fn main() -> Int { 1 }");
        let f = &funcs[0];

        let konst = f.blocks.iter().find_map(|b| {
            b.insts.iter().find_map(|i| match i {
                Inst::ConstGc { dst, konst } => Some((*dst, *konst)),
                _ => None,
            })
        });
        let (dst, konst) = konst.expect("an in-range Int literal lowers to ConstGc");
        assert_eq!(konst, GcConst::SmallInt(1));
        // The value still lands in a `Gc` slot: `ConstGc` is a `Gc`-destination
        // constant, not the `ConstInt`-into-a-rootable-slot shape P0-03 was.
        assert_eq!(f.locals[dst.0 as usize].kind, LocalKind::Gc);

        // Nothing allocates an Int here any more...
        assert!(
            !f.blocks.iter().any(|b| b.insts.iter().any(|i| matches!(
                i,
                Inst::Alloc {
                    alloc: AllocKind::Int { .. },
                    ..
                }
            ))),
            "the literal must not also allocate"
        );
        // ...and the annotated function has no safepoint at all in its body, so
        // there is nothing for the backend to spill.
        let mut annotated = lower_src_to_mir("fn main() -> Int { 1 }").0;
        crate::annotate(&mut annotated[0]);
        crate::verify(&annotated[0]).expect("an interned literal verifies");
        assert!(
            !annotated[0].blocks.iter().any(|b| b.insts.iter().any(|i| {
                matches!(
                    i,
                    Inst::Alloc { .. } | Inst::Materialize { .. } | Inst::Call { .. }
                )
            })),
            "a body that is one interned literal has no safepoint left"
        );
    }

    /// The other half of the branch, and the one a regression would silently
    /// delete: a literal outside the interned range still allocates, still
    /// carries a `ConstInt` feeding it, and is still an annotated safepoint.
    #[test]
    fn a_large_int_literal_still_allocates_and_is_still_a_safepoint() {
        let src = format!(
            "fn main() -> Int {{ {} }}",
            praxis_runtime::SMALL_INT_MAX + 1
        );
        let (mut funcs, _analysis) = lower_src_to_mir(&src);
        crate::annotate(&mut funcs[0]);
        crate::verify(&funcs[0]).expect("an allocated literal verifies");
        let f = &funcs[0];

        assert!(
            !f.blocks
                .iter()
                .any(|b| b.insts.iter().any(|i| matches!(i, Inst::ConstGc { .. }))),
            "an out-of-range literal must not read the interned table"
        );
        let allocs = f
            .blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .find_map(|i| match i {
                Inst::Alloc {
                    alloc: AllocKind::Int { value },
                    roots,
                    ..
                } => Some((*value, roots)),
                _ => None,
            });
        let (value, roots) = allocs.expect("an out-of-range literal still allocates");
        // The scalar feeding it is a real `ConstInt` in a `Scalar(Int)` slot.
        assert_eq!(
            f.locals[value.0 as usize].kind,
            LocalKind::Scalar(ScalarKind::Int)
        );
        // And the safepoint is annotated — `verify` rejects an unannotated one,
        // but assert it directly so the reason is visible here.
        assert!(
            roots.is_annotated(),
            "an allocating literal is a GC safepoint"
        );
    }

    /// The interned range's boundary, at the lowering that reads it: the exact
    /// endpoints take the `ConstGc` branch and one step outside either takes the
    /// allocating one. `build` and the runtime must agree about where the table
    /// ends, or generated code reads past it.
    #[test]
    fn the_lowering_branch_falls_exactly_on_the_interned_range() {
        for (value, interned) in [
            (praxis_runtime::SMALL_INT_MIN - 1, false),
            (praxis_runtime::SMALL_INT_MIN, true),
            (praxis_runtime::SMALL_INT_MAX, true),
            (praxis_runtime::SMALL_INT_MAX + 1, false),
        ] {
            // A negative literal is unary negation of a positive one, which is
            // not a `Lit::Int` — so the negative cases go through a `let` whose
            // initializer is the positive magnitude and are checked for the
            // *positive* value's treatment. Only the two positive rows below
            // exercise the ceiling; the floor is covered by `small_int`'s own
            // boundary test plus the runtime's.
            if value < 0 {
                continue;
            }
            let (funcs, _analysis) = lower_src_to_mir(&format!("fn main() -> Int {{ {value} }}"));
            let has_const_gc = funcs[0]
                .blocks
                .iter()
                .any(|b| b.insts.iter().any(|i| matches!(i, Inst::ConstGc { .. })));
            assert_eq!(
                has_const_gc,
                interned,
                "{value} should {} be interned",
                if interned { "" } else { "not" }
            );
        }
    }

    #[test]
    fn lowers_unit_literal_to_the_unit_singleton() {
        // A `Unit`-returning `main` with an empty body synthesizes a `Lit::Unit`
        // tail. That tail must produce the immortal `Unit` into a slot that
        // carries the Unit type — not an `Int(0)` masquerading as Unit. This is
        // the MIR-side guard for the type-lie fix: a `Unit`-typed expression
        // holds a genuine Unit value.
        //
        // It used to be an `Alloc { AllocKind::Unit }`, which never allocated
        // (`praxis_alloc_unit` answers `ctx.unit_ref` and its manifest row is
        // `Effect::Pure`) but was still a call and still spilled the shadow
        // frame, because `is_gc_safepoint` matches `Inst::Alloc` unconditionally.
        // It is an `Inst::ConstGc` now. What this test is about — the *type* of
        // the slot — is unchanged.
        let (funcs, analysis) = lower_src_to_mir("fn main() -> Unit { let x = 1 }");
        let f = &funcs[0];

        let const_unit = f.blocks.iter().find_map(|b| {
            b.insts.iter().find_map(|i| match i {
                Inst::ConstGc {
                    dst,
                    konst: GcConst::Unit,
                } => Some(*dst),
                _ => None,
            })
        });
        let dst = const_unit.expect("a Unit-returning body should produce the Unit singleton");

        // The destination slot must be a Gc local typed Unit (not Int(0):Unit).
        // `TypeData` isn't `PartialEq`, so compare via `matches!` on the
        // resolved representative (canonical pattern used elsewhere in the crate).
        let slot = &f.locals[dst.0 as usize];
        assert_eq!(slot.kind, LocalKind::Gc, "Unit value lives in a Gc slot");
        let slot_ty = slot.ty.known().expect("the Unit slot has a static type");
        assert!(
            matches!(
                analysis.db.data(analysis.db.follow(slot_ty)),
                praxis_types::TypeData::Unit
            ),
            "the Unit value's slot must carry the Unit type"
        );
    }

    #[test]
    fn closure_capture_indices_never_flow_through_gc_locals() {
        // Runtime ABI indices are raw integers, not GcRefs. Moving a scalar
        // capture index into a `LocalKind::Gc` slot makes an illegal state
        // representable: liveness can then spill e.g. integer `1` as if it
        // were a heap pointer, and a later collection will dereference 0x1.
        //
        // Both raw-word sites are covered: the closure prologue's capture
        // index, and the pipeline's `praxis_vec_new` null element descriptor
        // (the integer `0` moved into a `Gc` slot to ride the argument list).
        for src in [
            "fn main() -> Int {\n  let a = 10\n  let b = 20\n  let f = |x| x + a + b\n  f(12)\n}\n",
            "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.map(|x| x * 2).sum()\n}\n",
        ] {
            let (funcs, _analysis) = lower_src_to_mir(src);

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
                "raw scalar values must never inhabit GC-rootable locals in {src:?}: {bad_moves:?}"
            );
        }
    }

    #[test]
    fn a_closure_prologue_reads_its_captures_by_immediate_index() {
        // The capture index is an instruction immediate, like `LoadField`'s
        // field index — not a value that has to be built in a local first. This
        // is what makes P0-03's illegal state unconstructible rather than
        // merely absent: there is no longer a slot for the index to live in.
        let (funcs, _analysis) = lower_src_to_mir(
            "fn main() -> Int {\n  let a = 10\n  let b = 20\n  let f = |x| x + a + b\n  f(12)\n}\n",
        );
        let closure_fn = funcs
            .iter()
            .find(|f| f.name != "main")
            .expect("the closure literal lifts to a synthetic function");

        let indices: Vec<u32> = closure_fn
            .blocks
            .iter()
            .flat_map(|block| &block.insts)
            .filter_map(|inst| match inst {
                Inst::LoadCapture { index, .. } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(indices, vec![0, 1], "one LoadCapture per capture, in order");

        assert!(
            !closure_fn
                .blocks
                .iter()
                .flat_map(|block| &block.insts)
                .any(|inst| matches!(
                    inst,
                    Inst::Call {
                        callee: CallTarget::Runtime(RuntimeSymbol::ClosureCapture),
                        ..
                    }
                )),
            "the capture load is not a generic call with a boxed index argument"
        );
    }

    /// Whether `inst` produces the `Unit` singleton, in **either** of the two
    /// forms lowering can take.
    ///
    /// `Lit::Unit` used to be an `Alloc { AllocKind::Unit }` and is an
    /// `Inst::ConstGc { GcConst::Unit }` now. The two tests below are looking
    /// for a Unit that should not be there at all, so a predicate that knew only
    /// the old form would pass vacuously and stop guarding anything — which is
    /// exactly the failure mode a "no X appears" assertion has.
    fn produces_unit(inst: &Inst) -> bool {
        matches!(
            inst,
            Inst::Alloc {
                alloc: AllocKind::Unit,
                ..
            } | Inst::ConstGc {
                konst: GcConst::Unit,
                ..
            }
        )
    }

    #[test]
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
            .filter(|inst| produces_unit(inst))
            .collect();

        assert!(
            unit_fallbacks.is_empty(),
            "a well-typed `take(n)` pipeline must not lower through a Unit fallback"
        );
    }

    #[test]
    fn dynamic_skip_argument_does_not_silently_lower_to_unit() {
        let (funcs, _analysis) = lower_src_to_mir(
            "fn main() -> Int {\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  let n = 2\n  v.skip(n).sum()\n}\n",
        );
        let main = funcs.iter().find(|f| f.name == "main").expect("main MIR");
        let unit_fallbacks: Vec<_> = main
            .blocks
            .iter()
            .flat_map(|block| &block.insts)
            .filter(|inst| produces_unit(inst))
            .collect();

        assert!(
            unit_fallbacks.is_empty(),
            "a well-typed `skip(n)` pipeline must not lower through a Unit fallback"
        );
    }

    /// The predicate the two tests above rely on really does recognize both
    /// forms — including the one lowering emits today.
    ///
    /// Without this, `produces_unit` could quietly stop matching what `build`
    /// produces and both tests would keep passing on an empty list. Lowering a
    /// program that *does* contain a `Lit::Unit` is what pins it.
    #[test]
    fn the_unit_predicate_recognizes_what_lowering_emits() {
        let (funcs, _analysis) = lower_src_to_mir("fn main() -> Unit { let x = 1 }");
        assert!(
            funcs[0]
                .blocks
                .iter()
                .flat_map(|b| &b.insts)
                .any(produces_unit),
            "a Unit-returning body emits a Unit the predicate must see"
        );
    }

    /// **MIR-03's evaluation order.** A `take`/`skip` bound is an expression, so
    /// *when* it runs is part of the contract: once, before the loop, like every
    /// other pipeline argument — not once per element.
    ///
    /// No behavioural test can see this for a pure bound, which is why it is
    /// asserted on the MIR: one call to the user function, and it is emitted
    /// before the loop's first `praxis_vec_len` (the header's bounds check), so
    /// it cannot be inside the body.
    #[test]
    fn a_take_bound_is_evaluated_once_before_the_loop() {
        for method in ["take", "skip"] {
            let (funcs, _analysis) = lower_src_to_mir(&format!(
                "fn bound() -> Int {{ 2 }}\nfn main() -> Int {{\n  let v = Vec()\n  v.push(1)\n  v.push(2)\n  v.push(3)\n  v.{method}(bound()).sum()\n}}\n"
            ));
            let main = funcs.iter().find(|f| f.name == "main").expect("main MIR");
            // Blocks are appended in emission order, and so are the instructions
            // in each, so a flat walk is the emission sequence.
            let calls: Vec<&str> = main
                .blocks
                .iter()
                .flat_map(|block| &block.insts)
                .filter_map(|inst| match inst {
                    Inst::Call {
                        callee: CallTarget::User(name),
                        ..
                    } if name == "bound" => Some("bound"),
                    Inst::Call {
                        callee: CallTarget::Runtime(RuntimeSymbol::VecLen),
                        ..
                    } => Some("len"),
                    _ => None,
                })
                .collect();

            assert_eq!(
                calls.iter().filter(|c| **c == "bound").count(),
                1,
                "`{method}(bound())` must evaluate its bound exactly once, got {calls:?}"
            );
            assert_eq!(
                calls.first(),
                Some(&"bound"),
                "the bound must be evaluated before the loop's bounds check, got {calls:?}"
            );
        }
    }

    #[test]
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
            let local_ty = main.locals[dst.0 as usize].ty;
            let known = local_ty
                .known()
                .unwrap_or_else(|| panic!("the {callee:?} result slot {dst:?} is Opaque"));
            assert!(
                matches!(
                    analysis.db.data(analysis.db.follow(known)),
                    praxis_types::TypeData::Scalar(praxis_types::ScalarType::Int)
                ),
                "the {callee:?} result is statically Int but local {dst:?} carries {local_ty:?}"
            );
        }
    }

    #[test]
    fn pipeline_runtime_call_destinations_retain_vec_and_unit_types() {
        // The eager pipeline lowerers used to type every slot they minted `Int`:
        // the result Vec, and the Unit each `praxis_vec_push` returns. Both are
        // statically known, and a slot that lies about its type feeds the wrong
        // descriptor into debug metadata and schema construction.
        //
        // The Vec half moved from a hand-rolled `praxis_vec_new` call to an
        // `AllocKind::Collection` when P0-03 removed the null-descriptor
        // integer from its `Gc` argument slot, so the assertion covers the
        // allocation form rather than the call form.
        let (funcs, analysis) =
            lower_src_to_mir("fn main() {\n  let v = Vec()\n  v.push(1)\n  v.map(|x| x)\n}\n");
        let main = funcs.iter().find(|f| f.name == "main").expect("main MIR");

        let mut saw_vec = false;
        let mut saw_push = false;
        for inst in main.blocks.iter().flat_map(|block| &block.insts) {
            let (dst, what) = match inst {
                Inst::Alloc {
                    dst,
                    alloc:
                        AllocKind::Collection {
                            ctor: praxis_types::CollectionCtor::Vec,
                            ..
                        },
                    ..
                } => (*dst, "a Vec allocation"),
                Inst::Call {
                    dst,
                    callee: CallTarget::Runtime(RuntimeSymbol::VecPush),
                    ..
                } => (*dst, "praxis_vec_push"),
                _ => continue,
            };
            let local_ty = main.locals[dst.0 as usize].ty;
            let known = local_ty
                .known()
                .unwrap_or_else(|| panic!("{what} must define a typed local, got {local_ty:?}"));
            let data = analysis.db.data(analysis.db.follow(known));
            if what == "praxis_vec_push" {
                saw_push = true;
                assert!(
                    matches!(data, praxis_types::TypeData::Unit),
                    "praxis_vec_push must define a Unit-typed local, got {local_ty:?}"
                );
            } else {
                saw_vec = true;
                assert!(
                    matches!(
                        data,
                        praxis_types::TypeData::Collection {
                            ctor: praxis_types::CollectionCtor::Vec,
                            ..
                        }
                    ),
                    "a Vec allocation must define a Vec-typed local, got {local_ty:?}"
                );
            }
        }
        assert!(saw_vec, "the pipeline allocates its result Vec");
        assert!(saw_push, "the pipeline pushes into its result Vec");
    }

    /// **ADR-099 decision 1.** A list literal lowers to the allocation a `Vec()`
    /// emits plus one `praxis_vec_push` per element — no new instruction, no new
    /// wrapper — and the `Vec` is allocated **before** any element is evaluated.
    ///
    /// The ordering is the half a run test cannot see on its own: a lowering
    /// that evaluated every element first and then allocated would produce the
    /// same answers and leave the elements unrooted across each other's
    /// allocations. Here it is the `Vec` that is live across them, which is what
    /// makes the loop's own liveness root it.
    #[test]
    fn a_list_literal_allocates_a_vec_then_pushes_each_element() {
        let (funcs, analysis) = lower_src_to_mir("fn main() {\n  let v = [1, 2, 3]\n}\n");
        let main = funcs.iter().find(|f| f.name == "main").expect("main MIR");

        // The instruction sequence, in order: the allocation first, then one
        // push per element and no more.
        let mut alloc_at = None;
        let mut pushes = Vec::new();
        for (i, inst) in main
            .blocks
            .iter()
            .flat_map(|block| &block.insts)
            .enumerate()
        {
            match inst {
                Inst::Alloc {
                    dst,
                    alloc:
                        AllocKind::Collection {
                            ctor: praxis_types::CollectionCtor::Vec,
                            ..
                        },
                    ..
                } => {
                    assert!(alloc_at.is_none(), "one Vec allocation, not two");
                    alloc_at = Some((i, *dst));
                }
                Inst::Call {
                    dst,
                    callee: CallTarget::Runtime(RuntimeSymbol::VecPush),
                    args,
                    ..
                } => pushes.push((i, *dst, args.clone())),
                _ => {}
            }
        }
        let (alloc_i, vec_local) = alloc_at.expect("a list literal allocates a Vec");
        assert_eq!(pushes.len(), 3, "one push per element");

        for (push_i, unit_dst, args) in &pushes {
            assert!(
                *push_i > alloc_i,
                "the Vec is allocated before anything is pushed into it"
            );
            // Every push targets *that* Vec, so a literal cannot build one
            // object and answer another.
            assert_eq!(args[0], vec_local, "each push targets the allocated Vec");
            // …and each slot says what it holds, which is what feeds the debug
            // metadata and the schema construction.
            let unit_ty = main.locals[unit_dst.0 as usize].ty.known();
            assert!(
                matches!(
                    unit_ty.map(|t| analysis.db.data(analysis.db.follow(t))),
                    Some(praxis_types::TypeData::Unit)
                ),
                "praxis_vec_push defines a Unit-typed local, got {unit_ty:?}"
            );
        }
        let vec_ty = main.locals[vec_local.0 as usize]
            .ty
            .known()
            .expect("the literal's slot is typed");
        assert!(
            matches!(
                analysis.db.data(analysis.db.follow(vec_ty)),
                praxis_types::TypeData::Collection {
                    ctor: praxis_types::CollectionCtor::Vec,
                    ..
                }
            ),
            "a list literal defines a Vec-typed local, got {}",
            analysis.db.render(vec_ty)
        );

        // The empty literal is the allocation and nothing else.
        let (funcs, _) = lower_src_to_mir("fn main() {\n  let v: Vec[Int] = []\n}\n");
        let main = funcs.iter().find(|f| f.name == "main").expect("main MIR");
        assert_eq!(
            main.blocks
                .iter()
                .flat_map(|block| &block.insts)
                .filter(|inst| matches!(
                    inst,
                    Inst::Call {
                        callee: CallTarget::Runtime(RuntimeSymbol::VecPush),
                        ..
                    }
                ))
                .count(),
            0,
            "`[]` pushes nothing"
        );
    }

    /// **ADR-099 decision 5.** A `for` over a `Text` walks it **in place**,
    /// through the same accessors `t.len()` and `t[i]` call.
    ///
    /// The plan is the assertion: a `Text` that fell through to `iter_plan`'s
    /// non-collection fallback would read a `Text`'s payload through
    /// `praxis_vec_get` — the wrong-type read ADR-066 exists because of, and one
    /// that no type error would report. A snapshot would be wrong differently:
    /// correct answers, and a `Vec` materialized per loop that nothing needs.
    #[test]
    fn a_for_over_a_text_names_the_text_accessors_and_takes_no_snapshot() {
        let (funcs, _analysis) =
            lower_src_to_mir("fn main() {\n  let t = \"ab\"\n  for c in t { out(c) }\n}\n");
        let main = funcs.iter().find(|f| f.name == "main").expect("main MIR");
        let called: Vec<RuntimeSymbol> = main
            .blocks
            .iter()
            .flat_map(|block| &block.insts)
            .filter_map(|inst| match inst {
                Inst::Call {
                    callee: CallTarget::Runtime(name),
                    ..
                } => Some(*name),
                _ => None,
            })
            .collect();
        assert!(
            called.contains(&RuntimeSymbol::TextLen),
            "the header reads the Text's own length, got {called:?}"
        );
        assert!(
            called.contains(&RuntimeSymbol::TextGet),
            "the body reads the Text's own character, got {called:?}"
        );
        assert!(
            !called.contains(&RuntimeSymbol::VecLen) && !called.contains(&RuntimeSymbol::VecGet),
            "a Text is not read through a Vec's accessors, got {called:?}"
        );
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
                        } if *name == RuntimeSymbol::VecLen
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
    fn enumerate_tuple_allocation_carries_a_real_two_element_type() {
        // Codegen builds TupleSchema from AllocKind::Tuple.ty. `MirType::Opaque`
        // does not make codegen infer a schema from runtime values; it creates a
        // zero-field tuple and all tuple_set calls become no-ops. Assert the
        // MIR/codegen boundary carries the actual shape.
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

        let known = tuple_ty
            .known()
            .expect("enumerate's tuple allocation must carry a real type");
        assert!(
            matches!(
                analysis.db.data(analysis.db.follow(known)),
                praxis_types::TypeData::Tuple(elements) if elements.len() == 2
            ),
            "enumerate must carry a two-element tuple type into codegen, got {tuple_ty:?}"
        );
    }

    #[test]
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

        let known = tuple_ty
            .known()
            .expect("zip's tuple allocation must carry a real type");
        assert!(
            matches!(
                analysis.db.data(analysis.db.follow(known)),
                praxis_types::TypeData::Tuple(elements) if elements.len() == 2
            ),
            "zip must carry a two-element tuple type into codegen, got {tuple_ty:?}"
        );
    }
    /// **REP-16's MIR shape.** A compound store through a subscript emits one
    /// read, one store, and lowers its receiver and indices **once**.
    ///
    /// The instruction counts are the assertion a behavioural test cannot make:
    /// a desugaring into `c[k] = c[k] + 1` produces the same *answer* for a
    /// side-effect-free index, and twice the calls.
    #[test]
    fn a_compound_store_through_a_subscript_reads_once_and_writes_once() {
        let (funcs, _) = lower_src_to_mir(
            "fn main() -> Int {\n  let c = Counter()\n  c[\"k\"] += 1\n  c[\"k\"]\n}",
        );
        let main = funcs.iter().find(|f| f.name == "main").expect("main");
        let runtime_calls = |sym: RuntimeSymbol| -> usize {
            main.blocks
                .iter()
                .flat_map(|b| b.insts.iter())
                .filter(|i| {
                    matches!(
                        i,
                        Inst::Call {
                            callee: CallTarget::Runtime(s),
                            ..
                        } if *s == sym
                    )
                })
                .count()
        };
        // Two reads in the program: the `+=`'s own, and the trailing `c["k"]`.
        assert_eq!(runtime_calls(RuntimeSymbol::CounterGet), 2);
        assert_eq!(runtime_calls(RuntimeSymbol::CounterSet), 1);
        // `inc` is never involved: `+= 1` is a read-modify-write, not
        // `praxis_counter_inc` in disguise — which would give the right answer
        // here and the wrong one for `+= 2`.
        assert_eq!(runtime_calls(RuntimeSymbol::CounterInc), 0);

        // A plain store reads nothing.
        let (funcs, _) =
            lower_src_to_mir("fn main() -> Int {\n  let m = Map()\n  m[\"k\"] = 1\n  m.len()\n}");
        let main = funcs.iter().find(|f| f.name == "main").expect("main");
        let map_index = main
            .blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .filter(|i| {
                matches!(
                    i,
                    Inst::Call {
                        callee: CallTarget::Runtime(RuntimeSymbol::MapIndex),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(map_index, 0, "`m[k] = v` performs no read");
    }

    /// **REP-64's MIR shape.** A compound assignment whose operands are
    /// `Float`s emits `FloatBinOp`, never `IntBinOp` — at every operator and
    /// through every target shape the language has.
    ///
    /// The instruction kind is the assertion, and it is the one a behavioural
    /// test states only indirectly: a `Float` rides the uniform `i64` scalar
    /// channel as its **bit pattern** (ADR-037), so an `IntBinOp` here is not a
    /// type error anywhere downstream — it is arithmetic on the pattern, and the
    /// answer is a perfectly well-formed `Float` that no program asked for.
    /// `Materialize` moves with it: an `Int` materialization boxes the result
    /// with `Int`'s descriptor, so the value was mislabelled as well as wrong.
    #[test]
    fn a_float_compound_assignment_lowers_to_float_arithmetic() {
        // (source, the target shape it exercises)
        let shapes = [
            ("var f = 1.0\nf {}= 2.0\nout(f)", "a binding"),
            (
                "var f = 1.0\nlet c = || {{ f {}= 2.0 }}\nc()\nout(f)",
                "a captured binding (VarCell)",
            ),
            (
                "var m = Map()\nm[\"k\"] = 1.0\nm[\"k\"] {}= 2.0\nout(m[\"k\"])",
                "a subscript store",
            ),
        ];
        for op in ['+', '-', '*', '/'] {
            for (template, shape) in shapes {
                let src = template.replace("{}", &op.to_string());
                let (funcs, _) = lower_src_to_mir(&src);
                let insts: Vec<&Inst> = funcs
                    .iter()
                    .flat_map(|f| f.blocks.iter())
                    .flat_map(|b| b.insts.iter())
                    .collect();
                assert!(
                    insts.iter().any(|i| matches!(i, Inst::FloatBinOp { .. })),
                    "`{op}=` through {shape} must lower to FloatBinOp\nsrc: {src}"
                );
                assert!(
                    !insts.iter().any(|i| matches!(i, Inst::IntBinOp { .. })),
                    "`{op}=` through {shape} lowered to IntBinOp — that is \
                     integer arithmetic on two IEEE-754 bit patterns \
                     (REP-64)\nsrc: {src}"
                );
                assert!(
                    insts.iter().any(|i| matches!(
                        i,
                        Inst::Materialize {
                            scalar: ScalarKind::Float,
                            ..
                        }
                    )),
                    "`{op}=` through {shape} must box its result as a Float\nsrc: {src}"
                );
                assert!(
                    !insts.iter().any(|i| matches!(
                        i,
                        Inst::Materialize {
                            scalar: ScalarKind::Int,
                            ..
                        }
                    )),
                    "`{op}=` through {shape} boxed an Int (REP-64)\nsrc: {src}"
                );
            }
        }

        // The control: the same shapes on `Int` operands are still `IntBinOp`,
        // and `+=` on a `Text` binding is still the concatenation call.
        for (template, shape) in shapes {
            let src = template
                .replace("{}", "+")
                .replace("1.0", "1")
                .replace("2.0", "2");
            let (funcs, _) = lower_src_to_mir(&src);
            let has_int = funcs
                .iter()
                .flat_map(|f| f.blocks.iter())
                .flat_map(|b| b.insts.iter())
                .any(|i| matches!(i, Inst::IntBinOp { .. }));
            assert!(
                has_int,
                "Int `+=` through {shape} is Int arithmetic\nsrc: {src}"
            );
        }
        let (funcs, _) = lower_src_to_mir("var s = \"a\"\ns += \"b\"\nout(s)");
        let concats = funcs
            .iter()
            .flat_map(|f| f.blocks.iter())
            .flat_map(|b| b.insts.iter())
            .filter(|i| {
                matches!(
                    i,
                    Inst::Call {
                        callee: CallTarget::Runtime(RuntimeSymbol::TextConcat),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            concats, 1,
            "`s += \"b\"` is one `praxis_text_concat` (ADR-085)"
        );
    }

    /// **REP-15's MIR shape** (ADR-066). A `for` over a collection that cannot
    /// index itself takes **one** snapshot, **before** the loop — and a `for`
    /// over one that can takes none.
    ///
    /// The instruction counts are the assertion a behavioural test cannot make.
    /// A snapshot per step gives the same answer for every program in the suite
    /// and turns each loop quadratic in its allocations; a snapshot on a `Vec`
    /// gives the same answer too, and copies every vector any program iterates.
    #[test]
    fn a_for_snapshots_once_before_the_loop_or_not_at_all() {
        let runtime_calls = |f: &Function, sym: RuntimeSymbol| -> usize {
            f.blocks
                .iter()
                .flat_map(|b| b.insts.iter())
                .filter(|i| {
                    matches!(
                        i,
                        Inst::Call {
                            callee: CallTarget::Runtime(s),
                            ..
                        } if *s == sym
                    )
                })
                .count()
        };

        // Six members, so "once per step" and "once" are different numbers.
        let (funcs, _) = lower_src_to_mir(
            "fn main() -> Int {\n  let s = Set()\n  var i = 0\n  \
             while i < 6 { s.insert(i)\n i = i + 1 }\n  \
             var t = 0\n  for x in s { t = t + x }\n  t\n}",
        );
        let main = funcs.iter().find(|f| f.name == "main").expect("main");
        assert_eq!(runtime_calls(main, RuntimeSymbol::SetItems), 1);
        // …and it is not in the loop: the snapshot's block dominates the header,
        // so it holds no `Terminator::Branch` back-edge target. The cheap
        // structural form of that: the block holding it also holds the `for`'s
        // index initialization, which runs once by construction.
        let snapshot_block = main
            .blocks
            .iter()
            .position(|b| {
                b.insts.iter().any(|i| {
                    matches!(
                        i,
                        Inst::Call {
                            callee: CallTarget::Runtime(RuntimeSymbol::SetItems),
                            ..
                        }
                    )
                })
            })
            .expect("the snapshot is emitted");
        // The `for`'s header is the block that reads the snapshot's length; the
        // `while` earlier in the program has a header too, so "the first block
        // that branches" would find the wrong one.
        let header_block = main
            .blocks
            .iter()
            .position(|b| {
                b.insts.iter().any(|i| {
                    matches!(
                        i,
                        Inst::Call {
                            callee: CallTarget::Runtime(RuntimeSymbol::VecLen),
                            ..
                        }
                    )
                })
            })
            .expect("the loop has a header");
        assert!(
            snapshot_block < header_block,
            "the snapshot must precede the header, not sit inside the loop"
        );
        // The walk itself is the `Vec` accessor pair, on the snapshot.
        assert_eq!(runtime_calls(main, RuntimeSymbol::VecLen), 1);
        assert_eq!(runtime_calls(main, RuntimeSymbol::VecGet), 1);
        assert_eq!(
            runtime_calls(main, RuntimeSymbol::SetLen),
            0,
            "the header counts the snapshot, so both sides of `i < len` are one object"
        );

        // A keyed collection snapshots **twice**, and pairs the two per step —
        // one `AllocKind::Tuple` inside the body, not one per collection.
        let (funcs, _) = lower_src_to_mir(
            "fn main() -> Int {\n  let m = Map()\n  m.insert(1, 2)\n  \
             var t = 0\n  for kv in m { t = t + kv.1 }\n  t\n}",
        );
        let main = funcs.iter().find(|f| f.name == "main").expect("main");
        assert_eq!(runtime_calls(main, RuntimeSymbol::MapKeys), 1);
        assert_eq!(runtime_calls(main, RuntimeSymbol::MapValues), 1);
        assert_eq!(
            runtime_calls(main, RuntimeSymbol::VecGet),
            2,
            "one read per snapshot per step"
        );
        let tuples = main
            .blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .filter(|i| {
                matches!(
                    i,
                    Inst::Alloc {
                        alloc: AllocKind::Tuple { .. },
                        ..
                    }
                )
            })
            .count();
        assert_eq!(tuples, 1, "the pair is built once per step, in the body");

        // The regression a careless fix would cause: an indexable collection
        // must not be copied to be walked.
        for (src, snapshot_free) in [
            (
                "let v = Vec()\n  v.push(1)\n  for x in v { t = t + x }",
                "Vec",
            ),
            (
                "let d = Deque()\n  d.push_back(1)\n  for x in d { t = t + x }",
                "Deque",
            ),
            ("for x in 0..3 { t = t + x }", "Range"),
        ] {
            let src = format!("fn main() -> Int {{\n  var t = 0\n  {src}\n  t\n}}");
            let (funcs, _) = lower_src_to_mir(&src);
            let main = funcs.iter().find(|f| f.name == "main").expect("main");
            for sym in [
                RuntimeSymbol::SetItems,
                RuntimeSymbol::BitsetItems,
                RuntimeSymbol::MinHeapItems,
                RuntimeSymbol::MaxHeapItems,
                RuntimeSymbol::GridCells,
                RuntimeSymbol::MapKeys,
                RuntimeSymbol::CounterKeys,
            ] {
                assert_eq!(
                    runtime_calls(main, sym),
                    0,
                    "a {snapshot_free} is walked in place, not copied ({sym})"
                );
            }
        }
    }

    /// **REP-10.** A record pattern reads a field and a tuple pattern reads an
    /// element — and neither tests a tag first.
    ///
    /// This is the assertion a behavioural test cannot make. Both composites have
    /// **one constructor**, so the shape always matches and the whole test is the
    /// components'; an `EnumTag` compare here would be a branch on a word that is
    /// not a tag. And the two readers are different runtime symbols, so a walk
    /// that reused the enum's `EnumPayloadGet` for either would read a record's
    /// header as a payload slot.
    #[test]
    fn a_record_pattern_reads_fields_and_a_tuple_pattern_reads_elements() {
        /// What one program's `main` reads, by instruction: the field indices a
        /// `LoadField` names, then the counts of the other three readers.
        struct Reads {
            fields: Vec<u32>,
            elems: usize,
            tags: usize,
            payloads: usize,
        }
        let reads = |src: &str| -> Reads {
            let (funcs, _) = lower_src_to_mir(src);
            let main = funcs.iter().find(|f| f.name == "main").expect("main");
            let all = || main.blocks.iter().flat_map(|b| b.insts.iter());
            Reads {
                fields: all()
                    .filter_map(|i| match i {
                        Inst::LoadField { field_idx, .. } => Some(*field_idx),
                        _ => None,
                    })
                    .collect(),
                elems: all()
                    .filter(|i| matches!(i, Inst::LoadTupleElem { .. }))
                    .count(),
                tags: all().filter(|i| matches!(i, Inst::EnumTag { .. })).count(),
                payloads: all()
                    .filter(|i| matches!(i, Inst::EnumPayloadGet { .. }))
                    .count(),
            }
        };

        let record = reads(
            "struct P { x: Int, y: Int }\n\
             fn main() -> Int {\n  let p = P { x: 1, y: 2 }\n  \
             match p { P { x, y } => x + y }\n}",
        );
        assert_eq!(
            record.fields,
            vec![0, 1],
            "one read per named field, in order"
        );
        assert_eq!(record.elems, 0);
        assert_eq!(
            record.tags, 0,
            "a record has one constructor: there is no tag to compare"
        );

        let tuple = reads("fn main() -> Int {\n  let t = (1, 2)\n  match t { (a, b) => a + b }\n}");
        assert_eq!(tuple.elems, 2, "one read per element");
        assert!(tuple.fields.is_empty());
        assert_eq!(tuple.tags, 0);

        // A field the pattern does not name is not read at all — the wildcard
        // that pads the row costs nothing — and the ones it does name are read
        // at their own declared index.
        let partial = reads(
            "struct P { a: Int, b: Int, c: Int }\n\
             fn main() -> Int {\n  let p = P { a: 1, b: 2, c: 3 }\n  match p { P { c } => c }\n}",
        );
        assert_eq!(partial.fields, vec![2], "the third field, and only it");

        // An enum payload still tests its tag, and reads through its own
        // instruction: the three readers are chosen per composite, not per depth.
        let nested = reads(
            "struct P { x: Int, y: Int }\n\
             fn main() -> Int {\n  let o = Some((P { x: 1, y: 2 }, 3))\n  \
             match o { Some((P { x, y }, k)) => x + y + k, None => 0 }\n}",
        );
        assert_eq!(nested.tags, 2, "`Some` and `None` each compare a tag");
        assert_eq!(nested.payloads, 1);
        assert_eq!(nested.elems, 2);
        assert_eq!(nested.fields, vec![0, 1]);
    }

    /// **REP-49's gate.** A `Bool` pattern reads its scrutinee at a `Bool`'s
    /// width.
    ///
    /// `Lit::Bool` shared the `Lit::Int` arm, so `match b { true => … }` emitted
    /// `ExtractScalar { scalar: Int }` — `praxis_int_load`, an **eight**-byte
    /// read — against a payload that is **one** byte. The other seven are the
    /// block's alignment padding, which the allocator never writes, so the two
    /// immortal singletons were told apart by whatever malloc had left there.
    ///
    /// This is the assertion a behavioural test cannot make. `match true`
    /// answers correctly whenever the two paddings *differ*, and comparing
    /// `true` against itself reads one address twice and is right for the wrong
    /// reason — so the observable answer is right on most runs and wrong on the
    /// ones where the padding happens to match. The instruction is the fact.
    #[test]
    fn a_bool_pattern_reads_its_scrutinee_at_a_bools_width() {
        let extracts = |src: &str| -> Vec<ScalarKind> {
            let (funcs, _) = lower_src_to_mir(src);
            let main = funcs.iter().find(|f| f.name == "main").expect("main");
            main.blocks
                .iter()
                .flat_map(|b| b.insts.iter())
                .filter_map(|i| match i {
                    Inst::ExtractScalar { scalar, .. } => Some(*scalar),
                    _ => None,
                })
                .collect()
        };

        // Two arms, two literals, four reads — scrutinee and literal per arm —
        // and every one of them at `Bool`.
        let bools =
            extracts("fn main() -> Int {\n  let b = true\n  match b { true => 1, false => 0 }\n}");
        assert!(
            !bools.is_empty(),
            "a Bool pattern compares payloads, so it extracts them"
        );
        assert!(
            bools.iter().all(|k| *k == ScalarKind::Bool),
            "a Bool payload is one byte and `praxis_int_load` reads eight: {bools:?}"
        );

        // The `Int` half of the same arm is unchanged — the fix is a width, not
        // a rewrite of literal matching.
        let ints = extracts("fn main() -> Int {\n  let n = 1\n  match n { 1 => 10, _ => 0 }\n}");
        assert!(
            ints.iter().all(|k| *k == ScalarKind::Int),
            "an Int pattern still reads an Int: {ints:?}"
        );
    }

    /// **REP-21.** An updating store is **one** call and no read.
    ///
    /// The assertion a behavioural test cannot make: a read of a *present* key
    /// would succeed and leave the answer right, so nothing would show that
    /// `d[k] min= v` had quietly become the read-modify-write §6.2 says it is
    /// not. On an absent key the same read faults (§4.7) — which is the bug this
    /// pins, one step before it is observable.
    #[test]
    fn an_updating_store_is_one_call_and_reads_nothing() {
        let calls = |src: &str, sym: RuntimeSymbol| -> usize {
            let (funcs, _) = lower_src_to_mir(src);
            let main = funcs.iter().find(|f| f.name == "main").expect("main");
            main.blocks
                .iter()
                .flat_map(|b| b.insts.iter())
                .filter(|i| {
                    matches!(
                        i,
                        Inst::Call {
                            callee: CallTarget::Runtime(s),
                            ..
                        } if *s == sym
                    )
                })
                .count()
        };
        const MIN: &str = "fn main() -> Int {\n  let d = Map()\n  d[\"a\"] min= 5\n  d[\"a\"]\n}";
        const MAX: &str = "fn main() -> Int {\n  let b = Map()\n  b[\"a\"] max= 5\n  b[\"a\"]\n}";

        assert_eq!(calls(MIN, RuntimeSymbol::MapUpdateMin), 1);
        assert_eq!(calls(MAX, RuntimeSymbol::MapUpdateMax), 1);
        // Neither the plain store nor the read is involved. The trailing `d["a"]`
        // is one `MapIndex`; a read-modify-write would make it two.
        for src in [MIN, MAX] {
            assert_eq!(calls(src, RuntimeSymbol::MapInsert), 0, "no plain store");
            assert_eq!(
                calls(src, RuntimeSymbol::MapIndex),
                1,
                "only the read the program wrote"
            );
        }
        // …and the two operators do not share a wrapper.
        assert_eq!(calls(MIN, RuntimeSymbol::MapUpdateMax), 0);
        assert_eq!(calls(MAX, RuntimeSymbol::MapUpdateMin), 0);

        // A `+=` through the same subscript still reads first — the compound
        // operators are untouched, which is the regression a shared path would
        // cause.
        let compound =
            "fn main() -> Int {\n  let d = Map()\n  d[\"a\"] = 1\n  d[\"a\"] += 2\n  d[\"a\"]\n}";
        assert_eq!(
            calls(compound, RuntimeSymbol::MapIndex),
            2,
            "the `+=`'s read and the program's"
        );
        assert_eq!(calls(compound, RuntimeSymbol::MapInsert), 2);
        assert_eq!(calls(compound, RuntimeSymbol::MapUpdateMin), 0);
    }

    /// **REP-40.** There is no second pipeline lowering, and this test is what
    /// makes deleting the first one safe.
    ///
    /// The eager per-combinator lowerers stood beside the fused recognizer as
    /// ADR-029's "safety net for any chain the recognizer declines". What the
    /// net actually held was `lower_seq_fold`, which returned the seed and never
    /// invoked the closure, and a `_` arm that answered the Unit singleton — so
    /// a chain that reached it got a *wrong answer in silence*, which is the one
    /// failure mode a net must not add. Deleting it moves the obligation here:
    /// a row the catalog lowers as an intrinsic has no runtime symbol, so the
    /// recognizer is its only lowering, and a row the recognizer does not
    /// classify has none at all.
    ///
    /// The recognizer classifies on the name and arity, so the arguments are
    /// stand-ins; what is under test is that no `Intrinsic` row falls through.
    #[test]
    fn intrinsics_are_all_recognized_so_there_is_no_second_lowering() {
        let mut db = TypeDb::new();
        let unit = db.unit();
        let catalog = praxis_stdlib::builtin_catalog();
        let dummy = || TypedExpr::Lit {
            value: Lit::Unit,
            ty: unit,
            span: (0, 0),
        };
        let mut checked = 0usize;
        for entry in catalog.entries() {
            if !matches!(entry.lowering, praxis_stdlib::MethodLowering::Intrinsic(_)) {
                continue;
            }
            let call = TypedExpr::MethodCall {
                receiver: Box::new(dummy()),
                name: entry.name.to_string(),
                lowering_symbol: None,
                args: (0..entry.arity()).map(|_| dummy()).collect(),
                purity: entry.purity,
                ty: unit,
                span: (0, 0),
            };
            assert!(
                recognize_pipeline(&db, &call).is_some(),
                "`{}` at arity {} lowers as an intrinsic and no runtime symbol, \
                 but the pipeline recognizer declines it — it has no lowering",
                entry.name,
                entry.arity(),
            );
            checked += 1;
        }
        // A catalog that stopped registering intrinsics would make the loop
        // vacuous, and the assertion above would then prove nothing.
        assert!(
            checked >= 40,
            "expected the pipeline combinators to be intrinsic rows; saw {checked}"
        );
    }
}
