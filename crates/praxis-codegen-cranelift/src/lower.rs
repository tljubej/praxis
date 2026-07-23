//! MIR → Cranelift IR lowering for one function (§13.5, ADR-015).
//!
//! Each MIR [`Local`](praxis_mir::Local) becomes a Cranelift [`Variable`];
//! Cranelift turns the slot-based CFG into SSA automatically. Every language
//! value (`GcRef`) and scalar payload is carried as a Cranelift `i64` — `GcRef`
//! is pointer-sized and opaque to generated code, and `Int`/`Bool` payloads are
//! `i64`. Operations needing a real GC object (arithmetic, allocation) call the
//! `praxis_*` runtime wrappers, which allocate and fault-check (§10.4).

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use cranelift::codegen::ir::FuncRef;
use cranelift::codegen::ir::MemFlagsData as MemFlags;
use cranelift::codegen::isa::CallConv;
use cranelift::prelude::*;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{FuncId, Linkage, Module};
use praxis_mir::{
    AllocKind, CallTarget, CmpOp, Function as MirFunction, Inst, IntBinOp, LocalId, LocalKind,
    ScalarKind, Terminator,
};
use praxis_runtime::{ShadowFrame, MAX_SHADOW_SLOTS};

/// The uniform Cranelift type for a `GcRef` and every scalar payload: `i64`.
/// `GcRef` is `#[repr(transparent)]` over a pointer; `Int`/`Bool` payloads are
/// `i64`/`bool`. `i64` carries both faithfully on a 64-bit host.
const GC: types::Type = types::I64;

/// The byte offset of the `slots` array within a `ShadowFrame`. Generated code
/// writes root `GcRef`s into `frame_ptr + SLOTS_OFFSET + index*8` at safepoints.
/// Computed from the `#[repr(C)]` layout so it stays correct if the struct
/// evolves (and the ABI version check catches a drift that matters).
const SLOTS_OFFSET: i64 = core::mem::offset_of!(ShadowFrame, slots) as i64;

/// The `praxis_*` symbols the lowering references, each mapped to its name.
/// The `praxis_*` symbols the lowering references. Some (e.g. `IntNeg`) are
/// reserved ABI symbols the lowering does not yet emit directly (unary neg is
/// routed through `IntBinOp::Sub` in MIR) but remain here so the symbol table
/// stays complete for the wrappers that exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(dead_code)]
enum Symbol {
    AllocInt,
    AllocBool,
    AllocUnit,
    AllocText,
    IntLoad,
    BoolLoad,
    IntAdd,
    IntSub,
    IntMul,
    IntDiv,
    IntRem,
    IntNeg,
    IntEq,
    IntNe,
    IntLt,
    IntGt,
    IntLe,
    IntGe,
    CheckFault,
    /// Prologue helper: allocate + push a shadow-stack frame (ADR-019).
    PushShadowFrame,
    /// Epilogue helper: pop + free the shadow-stack frame (ADR-019).
    PopShadowFrame,
}

impl Symbol {
    fn name(self) -> &'static str {
        match self {
            Symbol::AllocInt => "praxis_alloc_int",
            Symbol::AllocBool => "praxis_alloc_bool",
            Symbol::AllocUnit => "praxis_alloc_unit",
            Symbol::AllocText => "praxis_alloc_text",
            Symbol::IntLoad => "praxis_int_load",
            Symbol::BoolLoad => "praxis_bool_load",
            Symbol::IntAdd => "praxis_int_add",
            Symbol::IntSub => "praxis_int_sub",
            Symbol::IntMul => "praxis_int_mul",
            Symbol::IntDiv => "praxis_int_div",
            Symbol::IntRem => "praxis_int_rem",
            Symbol::IntNeg => "praxis_int_neg",
            Symbol::IntEq => "praxis_int_eq",
            Symbol::IntNe => "praxis_int_ne",
            Symbol::IntLt => "praxis_int_lt",
            Symbol::IntGt => "praxis_int_gt",
            Symbol::IntLe => "praxis_int_le",
            Symbol::IntGe => "praxis_int_ge",
            Symbol::CheckFault => "praxis_check_fault",
            Symbol::PushShadowFrame => "praxis_push_shadow_frame",
            Symbol::PopShadowFrame => "praxis_pop_shadow_frame",
        }
    }
}

/// Lower one MIR function into a Cranelift function and define it in `module`.
pub(crate) fn lower_function<M: Module>(
    module: &mut M,
    fn_ctx: &mut FunctionBuilderContext,
    mir: &MirFunction,
    user_funcs: &HashMap<String, FuncId>,
) -> Result<()> {
    // Use the module's own Context (the 0.134 idiom): build into `ctx.func`,
    // then `define_function(id, &mut ctx)`.
    let mut ctx = module.make_context();
    // The function's signature: fn(ctx: i64, args: i64...) -> i64. Set it on the
    // context's function so block params are appended correctly.
    ctx.func.signature = abi_signature(mir);
    let mut builder = FunctionBuilder::new(&mut ctx.func, fn_ctx);

    // One Cranelift Variable per MIR Local. `declare_var` returns the Variable.
    let vars: Vec<Variable> = (0..mir.locals.len())
        .map(|_| builder.declare_var(GC))
        .collect();

    // One Cranelift block per MIR block.
    let blocks: Vec<Block> = (0..mir.blocks.len())
        .map(|_| builder.create_block())
        .collect();

    // Build the Gc-local → slot-index map (ADR-019). Only `Gc` locals get a
    // shadow-stack slot; `Scalar` locals are transient and must not survive a
    // safepoint (the builder re-materializes them). The slot index is the
    // local's position among Gc locals, *not* its MIR LocalId.
    let gc_slot: HashMap<LocalId, u32> = {
        let mut idx = 0u32;
        mir.locals
            .iter()
            .filter(|l| l.kind == LocalKind::Gc)
            .map(|l| {
                let i = idx;
                idx += 1;
                (l.id, i)
            })
            .collect()
    };
    let gc_count = gc_slot.len() as u32;
    anyhow::ensure!(
        gc_count as usize <= MAX_SHADOW_SLOTS,
        "function `{}` has {gc_count} Gc locals, exceeding MAX_SHADOW_SLOTS ({MAX_SHADOW_SLOTS})",
        mir.name
    );

    // Entry block: receive context + params, map params to their Variables.
    let entry = blocks[0];
    builder.append_block_params_for_function_params(entry);
    builder.switch_to_block(entry);
    // Do NOT seal blocks per-block: loop backedges make a header's last
    // predecessor arrive after the header is visited. Seal them all at the end.
    let ctx_val = builder.block_params(entry)[0];
    for (i, &param_local) in mir.params.iter().enumerate() {
        let arg = builder.block_params(entry)[i + 1];
        builder.def_var(vars[param_local.0 as usize], arg);
    }

    // Prologue: push a shadow frame and keep its pointer in a Variable. This
    // frame is the root set the collector walks during the automatic GC that
    // `praxis_alloc_*` wrappers trigger (§12.4, ADR-019).
    let frame_var = builder.declare_var(GC);
    let frame_ptr = {
        let fr = import(
            module,
            &mut builder,
            &mut HashMap::new(),
            Symbol::PushShadowFrame,
            &push_shadow_frame_sig(),
        )?;
        let count_val = builder.ins().iconst(GC, gc_count as i64);
        let call = builder.ins().call(fr, &[ctx_val, count_val]);
        builder.func.dfg.first_result(call)
    };
    builder.def_var(frame_var, frame_ptr);

    let mut import_cache: HashMap<Symbol, FuncRef> = HashMap::new();
    let mut user_func_cache: HashMap<String, FuncRef> = HashMap::new();
    let spill = SpillCtx {
        frame_var,
        slot_of: &gc_slot,
    };

    // Lower each block. Blocks are sealed together after the whole CFG is built
    // so loop backedges resolve correctly.
    for (blk_idx, mir_block) in mir.blocks.iter().enumerate() {
        let block = blocks[blk_idx];
        if blk_idx != 0 {
            builder.switch_to_block(block);
        }
        for inst in &mir_block.insts {
            lower_inst(
                &mut builder,
                inst,
                ctx_val,
                &vars,
                &spill,
                module,
                &mut import_cache,
                user_funcs,
                &mut user_func_cache,
            )?;
        }
        lower_terminator(
            &mut builder,
            &mir_block.term,
            &vars,
            &blocks,
            ctx_val,
            &spill,
            module,
            &mut import_cache,
            user_funcs,
            &mut user_func_cache,
        )?;
    }

    // All blocks are built (including loop backedges); seal them together so
    // Cranelift resolves the SSA joins, then finalize.
    builder.seal_all_blocks();
    let frontend_config = module.isa().frontend_config();
    builder.finalize(frontend_config);

    // Resolve our FuncId (declared in the first pass) and define the function.
    let id = func_id_for(module, &mir.name)?;
    module.define_function(id, &mut ctx)?;
    module.clear_context(&mut ctx);
    Ok(())
}

/// The spill context handed to every instruction/terminator lowering: the
/// Variable holding the current frame pointer, and the Gc-local → slot-index
/// map. At safepoints the backend stores each live root's value into its slot
/// (ADR-019).
struct SpillCtx<'a> {
    frame_var: Variable,
    slot_of: &'a HashMap<LocalId, u32>,
}

impl SpillCtx<'_> {
    /// Emit stores for every live root in `roots` into the shadow frame, just
    /// before a safepoint. Each root's current Cranelift value is written to
    /// `frame_ptr + SLOTS_OFFSET + slot_index*8` (§12.3).
    fn emit_spill(&self, builder: &mut FunctionBuilder, roots: &[LocalId], vars: &[Variable]) {
        if roots.is_empty() {
            return;
        }
        let frame_ptr = builder.use_var(self.frame_var);
        for &local in roots {
            let Some(&slot) = self.slot_of.get(&local) else {
                continue; // a Scalar local slipped into live_roots; it has no slot.
            };
            let val = builder.use_var(vars[local.0 as usize]);
            let off = SLOTS_OFFSET + (slot as i64) * 8;
            // `iadd_imm` is deprecated in Cranelift 0.134 in favor of the
            // sign/zero-extended variants; the slot offset is always a small
            // positive immediate so the distinction is immaterial.
            #[allow(deprecated)]
            let slot_addr = builder.ins().iadd_imm(frame_ptr, off);
            // Store into the frame slot; these accesses never trap (the frame is
            // always live and the offset is in-bounds by construction).
            let mut flags = MemFlags::trusted();
            flags.set_notrap();
            builder.ins().store(flags, val, slot_addr, 0);
        }
    }
}

/// The ABI signature for a MIR function: `fn(ctx: i64, args: i64...) -> i64`.
fn abi_signature(mir: &MirFunction) -> Signature {
    let mut sig = Signature::new(CallConv::Fast);
    sig.params.push(AbiParam::new(GC)); // hidden context pointer
    for _ in &mir.params {
        sig.params.push(AbiParam::new(GC));
    }
    sig.returns.push(AbiParam::new(GC));
    sig
}

/// Look up a previously-declared function's `FuncId` by name.
fn func_id_for<M: Module>(module: &M, name: &str) -> Result<FuncId> {
    match module.get_name(name) {
        Some(cranelift_module::FuncOrDataId::Func(id)) => Ok(id),
        Some(cranelift_module::FuncOrDataId::Data(_)) => {
            Err(anyhow!("`{name}` declared as data, not a function"))
        }
        None => Err(anyhow!("function `{name}` was not declared")),
    }
}

#[allow(clippy::too_many_arguments)]
fn lower_inst<M: Module>(
    builder: &mut FunctionBuilder,
    inst: &Inst,
    ctx_val: Value,
    vars: &[Variable],
    spill: &SpillCtx<'_>,
    module: &mut M,
    imports: &mut HashMap<Symbol, FuncRef>,
    user_funcs: &HashMap<String, FuncId>,
    user_cache: &mut HashMap<String, FuncRef>,
) -> Result<()> {
    match inst {
        Inst::ConstInt { dst, value } => {
            let v = builder.ins().iconst(GC, *value);
            builder.def_var(vars[dst.0 as usize], v);
        }
        Inst::Alloc {
            dst,
            alloc,
            live_roots,
        } => {
            // Spill live Gc roots into the shadow frame *before* the allocating
            // call: the wrapper may trigger a collection (§12.4), and the
            // collector walks the frame (ADR-019).
            spill.emit_spill(builder, live_roots, vars);
            match alloc {
                AllocKind::Int { value } => {
                    let arg = builder.use_var(vars[value.0 as usize]);
                    let result = call_alloc_int(builder, ctx_val, arg, module, imports)?;
                    builder.def_var(vars[dst.0 as usize], result);
                }
                AllocKind::Bool { value } => {
                    let arg = builder.use_var(vars[value.0 as usize]);
                    let result =
                        call_symbol1(builder, ctx_val, arg, Symbol::AllocBool, module, imports)?;
                    builder.def_var(vars[dst.0 as usize], result);
                }
                AllocKind::Unit => {
                    let result =
                        call_symbol0(builder, ctx_val, Symbol::AllocUnit, module, imports)?;
                    builder.def_var(vars[dst.0 as usize], result);
                }
                AllocKind::Text { value } => {
                    // Embed the string as a data object, then call praxis_alloc_text
                    // with (ptr, len).
                    let (ptr, len_val) = embed_text(builder, module, value)?;
                    let result = call_alloc_text(builder, ctx_val, ptr, len_val, module, imports)?;
                    builder.def_var(vars[dst.0 as usize], result);
                }
            }
        }
        Inst::ExtractScalar { dst, src, scalar } => {
            let src_val = builder.use_var(vars[src.0 as usize]);
            let sym = match scalar {
                ScalarKind::Int => Symbol::IntLoad,
                ScalarKind::Bool => Symbol::BoolLoad,
                ScalarKind::Byte | ScalarKind::Char => Symbol::IntLoad,
            };
            let result = call_symbol1(builder, ctx_val, src_val, sym, module, imports)?;
            builder.def_var(vars[dst.0 as usize], result);
        }
        Inst::Materialize {
            dst,
            src,
            scalar,
            live_roots,
        } => {
            // Materialize re-boxes a scalar → it allocates → safepoint.
            spill.emit_spill(builder, live_roots, vars);
            // A scalar payload re-boxed: Int → praxis_alloc_int, Bool → alloc_bool.
            let src_val = builder.use_var(vars[src.0 as usize]);
            let sym = match scalar {
                ScalarKind::Int => Symbol::AllocInt,
                ScalarKind::Bool => Symbol::AllocBool,
                ScalarKind::Byte | ScalarKind::Char => Symbol::AllocInt,
            };
            let result = call_symbol1(builder, ctx_val, src_val, sym, module, imports)?;
            builder.def_var(vars[dst.0 as usize], result);
        }
        Inst::StoreScalar { .. } => {
            // M4 scalars are immutable objects; StoreScalar is a no-op placeholder
            // for the future mutable-Int optimization.
        }
        Inst::IntBinOp { op, dst, lhs, rhs } => {
            let l = builder.use_var(vars[lhs.0 as usize]);
            let r = builder.use_var(vars[rhs.0 as usize]);
            let sym = match op {
                IntBinOp::Add => Symbol::IntAdd,
                IntBinOp::Sub => Symbol::IntSub,
                IntBinOp::Mul => Symbol::IntMul,
                IntBinOp::Div => Symbol::IntDiv,
                IntBinOp::Rem => Symbol::IntRem,
            };
            // praxis_int_* take (ctx, lhs_gc, rhs_gc) — but the operands here are
            // already scalar i64s extracted previously. The wrappers expect GcRef
            // operands. To bridge, re-alloc each scalar first.
            // (The builder emits Extract then BinOp; here we re-materialize to
            //  match the wrapper's GcRef ABI. A future pass can fold the extract.)
            let l_gc = call_symbol1(builder, ctx_val, l, Symbol::AllocInt, module, imports)?;
            let r_gc = call_symbol1(builder, ctx_val, r, Symbol::AllocInt, module, imports)?;
            let result = call_symbol2(builder, ctx_val, l_gc, r_gc, sym, module, imports)?;
            // The result is a GcRef; load it back to a scalar for the dst slot.
            let scalar = call_symbol1(builder, ctx_val, result, Symbol::IntLoad, module, imports)?;
            builder.def_var(vars[dst.0 as usize], scalar);
            // Fault check after faultable arith.
            let _ = call_check_fault(builder, ctx_val, module, imports)?;
        }
        Inst::IntCmp { op, dst, lhs, rhs } => {
            let l = builder.use_var(vars[lhs.0 as usize]);
            let r = builder.use_var(vars[rhs.0 as usize]);
            // Re-materialize operands to GcRefs to match the wrapper ABI.
            let l_gc = call_symbol1(builder, ctx_val, l, Symbol::AllocInt, module, imports)?;
            let r_gc = call_symbol1(builder, ctx_val, r, Symbol::AllocInt, module, imports)?;
            let sym = match op {
                CmpOp::Eq => Symbol::IntEq,
                CmpOp::Neq => Symbol::IntNe,
                CmpOp::Lt => Symbol::IntLt,
                CmpOp::Gt => Symbol::IntGt,
                CmpOp::Le => Symbol::IntLe,
                CmpOp::Ge => Symbol::IntGe,
            };
            let bool_gc = call_symbol2(builder, ctx_val, l_gc, r_gc, sym, module, imports)?;
            let bool_scalar =
                call_symbol1(builder, ctx_val, bool_gc, Symbol::BoolLoad, module, imports)?;
            builder.def_var(vars[dst.0 as usize], bool_scalar);
        }
        Inst::Call {
            dst,
            callee,
            args,
            live_roots,
        } => {
            // A call may allocate (and M4 user functions allocate freely) →
            // safepoint. Spill the live Gc roots before the call.
            spill.emit_spill(builder, live_roots, vars);
            let arg_vals: Vec<Value> = args
                .iter()
                .map(|a| builder.use_var(vars[a.0 as usize]))
                .collect();
            let funcref = match callee {
                CallTarget::User(name) => {
                    user_funcref(name, user_funcs, user_cache, module, builder)?
                }
                CallTarget::Runtime(name) => {
                    // Runtime wrapper signature: fn(ctx, args...) -> i64.
                    // args already includes the receiver as the first element.
                    runtime_funcref(name, builder, module, imports, args.len())?
                }
            };
            let mut call_args = vec![ctx_val];
            call_args.extend(arg_vals);
            let call = builder.ins().call(funcref, &call_args);
            let result = builder.func.dfg.first_result(call);
            builder.def_var(vars[dst.0 as usize], result);
        }
        Inst::CheckFault { on_fault: _ } => {
            // CheckFault is modeled by the faultable ops themselves (they set
            // pending_fault); a dedicated praxis_check_fault could branch to the
            // fault block here. For M4's acceptance tests the aggregate result
            // is read back by the host. (Full per-check branching is a follow-up.)
            let _ = call_check_fault(builder, ctx_val, module, imports)?;
        }
        Inst::MoveGc { dst, src } => {
            let v = builder.use_var(vars[src.0 as usize]);
            builder.def_var(vars[dst.0 as usize], v);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn lower_terminator<M: Module>(
    builder: &mut FunctionBuilder,
    term: &Terminator,
    vars: &[Variable],
    blocks: &[Block],
    ctx_val: Value,
    spill: &SpillCtx<'_>,
    module: &mut M,
    imports: &mut HashMap<Symbol, FuncRef>,
    _user_funcs: &HashMap<String, FuncId>,
    _user_cache: &mut HashMap<String, FuncRef>,
) -> Result<()> {
    match term {
        Terminator::Branch {
            cond,
            then_block,
            else_block,
        } => {
            let c = builder.use_var(vars[cond.0 as usize]);
            // brif: if c != 0 → then, else → else.
            let then_b = blocks[then_block.0 as usize];
            let else_b = blocks[else_block.0 as usize];
            builder.ins().brif(c, then_b, &[], else_b, &[]);
        }
        Terminator::Jump { target } => {
            builder.ins().jump(blocks[target.0 as usize], &[]);
        }
        Terminator::Return { value } => {
            // Epilogue: pop the shadow frame before returning (ADR-019).
            emit_pop_shadow_frame(builder, ctx_val, spill, module, imports)?;
            let v = builder.use_var(vars[value.0 as usize]);
            builder.ins().return_(&[v]);
        }
        Terminator::Fault => {
            // Epilogue (fault path): pop the shadow frame before unwinding.
            emit_pop_shadow_frame(builder, ctx_val, spill, module, imports)?;
            // Unwind to the host: return the Unit sentinel (the caller checks
            // pending_fault). The fault block has no value of its own.
            let zero = builder.ins().iconst(GC, 0);
            builder.ins().return_(&[zero]);
        }
    }
    Ok(())
}

/// Emit the `praxis_pop_shadow_frame(ctx, frame)` epilogue call (ADR-019).
fn emit_pop_shadow_frame<M: Module>(
    builder: &mut FunctionBuilder,
    ctx_val: Value,
    spill: &SpillCtx<'_>,
    module: &mut M,
    imports: &mut HashMap<Symbol, FuncRef>,
) -> Result<()> {
    let fr = import(
        module,
        builder,
        imports,
        Symbol::PopShadowFrame,
        &pop_shadow_frame_sig(),
    )?;
    let frame_ptr = builder.use_var(spill.frame_var);
    builder.ins().call(fr, &[ctx_val, frame_ptr]);
    Ok(())
}

// --- helpers: declare imports + emit calls -------------------------------

/// Declare (lazily) and return the `FuncRef` for a `praxis_*` `Symbol`.
fn import<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    imports: &mut HashMap<Symbol, FuncRef>,
    sym: Symbol,
    sig: &Signature,
) -> Result<FuncRef> {
    if let Some(&fr) = imports.get(&sym) {
        return Ok(fr);
    }
    let id = match module.declare_function(sym.name(), Linkage::Import, sig) {
        Ok(id) => id,
        Err(_) => func_id_for(module, sym.name())?,
    };
    let fr = module.declare_func_in_func(id, builder.func);
    imports.insert(sym, fr);
    Ok(fr)
}

#[allow(clippy::too_many_arguments)]
fn call_alloc_int<M: Module>(
    builder: &mut FunctionBuilder,
    ctx: Value,
    arg: Value,
    module: &mut M,
    imports: &mut HashMap<Symbol, FuncRef>,
) -> Result<Value> {
    let fr = import(
        module,
        builder,
        imports,
        Symbol::AllocInt,
        &unary_wrapped_sig(),
    )?;
    let call = builder.ins().call(fr, &[ctx, arg]);
    Ok(builder.func.dfg.first_result(call))
}

#[allow(clippy::too_many_arguments)]
fn call_symbol0<M: Module>(
    builder: &mut FunctionBuilder,
    ctx: Value,
    sym: Symbol,
    module: &mut M,
    imports: &mut HashMap<Symbol, FuncRef>,
) -> Result<Value> {
    let fr = import(module, builder, imports, sym, &ctx_only_sig())?;
    let call = builder.ins().call(fr, &[ctx]);
    Ok(builder.func.dfg.first_result(call))
}

#[allow(clippy::too_many_arguments)]
fn call_symbol1<M: Module>(
    builder: &mut FunctionBuilder,
    ctx: Value,
    arg: Value,
    sym: Symbol,
    module: &mut M,
    imports: &mut HashMap<Symbol, FuncRef>,
) -> Result<Value> {
    let fr = import(module, builder, imports, sym, &unary_wrapped_sig())?;
    let call = builder.ins().call(fr, &[ctx, arg]);
    Ok(builder.func.dfg.first_result(call))
}

#[allow(clippy::too_many_arguments)]
fn call_symbol2<M: Module>(
    builder: &mut FunctionBuilder,
    ctx: Value,
    a: Value,
    b: Value,
    sym: Symbol,
    module: &mut M,
    imports: &mut HashMap<Symbol, FuncRef>,
) -> Result<Value> {
    let fr = import(module, builder, imports, sym, &binary_wrapped_sig())?;
    let call = builder.ins().call(fr, &[ctx, a, b]);
    Ok(builder.func.dfg.first_result(call))
}

#[allow(clippy::too_many_arguments)]
fn call_alloc_text<M: Module>(
    builder: &mut FunctionBuilder,
    ctx: Value,
    ptr: Value,
    len: Value,
    module: &mut M,
    imports: &mut HashMap<Symbol, FuncRef>,
) -> Result<Value> {
    let fr = import(
        module,
        builder,
        imports,
        Symbol::AllocText,
        &text_alloc_sig(),
    )?;
    let call = builder.ins().call(fr, &[ctx, ptr, len]);
    Ok(builder.func.dfg.first_result(call))
}

fn call_check_fault<M: Module>(
    builder: &mut FunctionBuilder,
    ctx: Value,
    module: &mut M,
    imports: &mut HashMap<Symbol, FuncRef>,
) -> Result<Value> {
    let fr = import(
        module,
        builder,
        imports,
        Symbol::CheckFault,
        &check_fault_sig(),
    )?;
    let call = builder.ins().call(fr, &[ctx]);
    Ok(builder.func.dfg.first_result(call))
}

/// Resolve a user-function call target to a FuncRef declared in the current func.
fn user_funcref<M: Module>(
    name: &str,
    user_funcs: &HashMap<String, FuncId>,
    cache: &mut HashMap<String, FuncRef>,
    module: &mut M,
    builder: &mut FunctionBuilder,
) -> Result<FuncRef> {
    if let Some(&fr) = cache.get(name) {
        return Ok(fr);
    }
    let id = user_funcs
        .get(name)
        .copied()
        .ok_or_else(|| anyhow!("unresolved user function `{name}`"))?;
    let fr = module.declare_func_in_func(id, builder.func);
    cache.insert(name.to_string(), fr);
    Ok(fr)
}

/// Resolve a runtime-wrapper call target (`praxis_vec_push`, …) to a FuncRef.
/// Runtime wrappers are variadic, so the signature is built from the arg count:
/// `fn(ctx: i64, args: i64...) -> i64`. The symbol is already registered in the
/// JIT module's symbol table (module.rs); here we declare an *import* with the
/// matching signature so Cranelift can call it.
fn runtime_funcref<M: Module>(
    name: &str,
    builder: &mut FunctionBuilder,
    module: &mut M,
    imports: &mut HashMap<Symbol, FuncRef>,
    // The caller passes the total arg count (excluding ctx) so the signature
    // matches. We encode the runtime symbol as a synthetic Symbol keyed by name;
    // to keep the cache keyed by Symbol, we intern by declaring fresh each call
    // (runtime calls are rare in a function, so this is fine).
    arg_count_excluding_ctx: usize,
) -> Result<FuncRef> {
    // Build a signature: fn(ctx, arg0, arg1, ...) -> i64.
    let mut sig = Signature::new(CallConv::Fast);
    sig.params.push(AbiParam::new(GC)); // ctx
    for _ in 0..arg_count_excluding_ctx {
        sig.params.push(AbiParam::new(GC));
    }
    sig.returns.push(AbiParam::new(GC));
    // Declare the import. `declare_function` with `Linkage::Import` resolves
    // through the JIT's registered symbol table at finalize time.
    let id = match module.declare_function(name, Linkage::Import, &sig) {
        Ok(id) => id,
        Err(_) => {
            // Already declared (e.g. a prior call in the same module); fetch it.
            module
                .get_name(name)
                .and_then(|f| match f {
                    cranelift_module::FuncOrDataId::Func(id) => Some(id),
                    _ => None,
                })
                .ok_or_else(|| anyhow!("runtime symbol `{name}` not declared"))?
        }
    };
    let fr = module.declare_func_in_func(id, builder.func);
    let _ = imports; // runtime symbols are not cached by Symbol (they're name-keyed)
    Ok(fr)
}

// --- signatures ----------------------------------------------------------

fn unary_wrapped_sig() -> Signature {
    // fn(ctx: i64, a: i64) -> i64
    let mut sig = Signature::new(CallConv::Fast);
    sig.params.push(AbiParam::new(GC));
    sig.params.push(AbiParam::new(GC));
    sig.returns.push(AbiParam::new(GC));
    sig
}

fn binary_wrapped_sig() -> Signature {
    let mut sig = Signature::new(CallConv::Fast);
    sig.params.push(AbiParam::new(GC));
    sig.params.push(AbiParam::new(GC));
    sig.params.push(AbiParam::new(GC));
    sig.returns.push(AbiParam::new(GC));
    sig
}

fn ctx_only_sig() -> Signature {
    let mut sig = Signature::new(CallConv::Fast);
    sig.params.push(AbiParam::new(GC));
    sig.returns.push(AbiParam::new(GC));
    sig
}

fn text_alloc_sig() -> Signature {
    let mut sig = Signature::new(CallConv::Fast);
    sig.params.push(AbiParam::new(GC));
    sig.params.push(AbiParam::new(GC));
    sig.params.push(AbiParam::new(GC));
    sig.returns.push(AbiParam::new(GC));
    sig
}

fn check_fault_sig() -> Signature {
    // fn(ctx: i64) -> i64
    let mut sig = Signature::new(CallConv::Fast);
    sig.params.push(AbiParam::new(GC));
    sig.returns.push(AbiParam::new(GC));
    sig
}

/// `fn(ctx: i64, slot_count: i64) -> i64` — returns the frame pointer.
fn push_shadow_frame_sig() -> Signature {
    let mut sig = Signature::new(CallConv::Fast);
    sig.params.push(AbiParam::new(GC)); // ctx
    sig.params.push(AbiParam::new(GC)); // slot_count
    sig.returns.push(AbiParam::new(GC)); // *mut ShadowFrame
    sig
}

/// `fn(ctx: i64, frame: i64) -> void`.
fn pop_shadow_frame_sig() -> Signature {
    let mut sig = Signature::new(CallConv::Fast);
    sig.params.push(AbiParam::new(GC)); // ctx
    sig.params.push(AbiParam::new(GC)); // frame
    sig
}

/// Embed a string literal as a leaked `&'static str` and produce (ptr, len)
/// `iconst` values for `praxis_alloc_text`. The leak is bounded by the JIT
/// generation's lifetime (one `run`); a `JitGeneration` arena (§10.5) reclaims
/// these in watch/debugger mode (M-later).
fn embed_text<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    value: &str,
) -> Result<(Value, Value)> {
    let leaked: &'static str = Box::leak(value.to_string().into_boxed_str());
    let ptr_val = leaked.as_ptr() as i64;
    let len_val = leaked.len() as i64;
    let _ = module;
    Ok((
        builder.ins().iconst(GC, ptr_val),
        builder.ins().iconst(GC, len_val),
    ))
}
