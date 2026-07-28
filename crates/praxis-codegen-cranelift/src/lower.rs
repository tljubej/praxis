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
use cranelift::codegen::ir::condcodes::IntCC;
use cranelift::codegen::ir::FuncRef;
use cranelift::codegen::ir::MemFlagsData as MemFlags;
use cranelift::codegen::isa::CallConv;
use cranelift::prelude::*;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{FuncId, Linkage, Module};
use praxis_mir::{
    AllocKind, CallTarget, CmpOp, FloatBinOp, Function as MirFunction, Inst, IntBinOp, LocalId,
    LocalKind, ScalarKind, Terminator,
};
use praxis_runtime::{DebugLocalMeta, RuntimeContext, ShadowFrame, MAX_SHADOW_SLOTS};
use praxis_stdlib::abi::{AbiKind, AbiRet, RuntimeSymbol};

/// The uniform Cranelift type for a `GcRef` and every scalar payload: `i64`.
/// `GcRef` is `#[repr(transparent)]` over a pointer; `Int`/`Bool` payloads are
/// `i64`/`bool`. `i64` carries both faithfully on a 64-bit host.
const GC: types::Type = types::I64;

/// The byte offset of the `slots` array within a `ShadowFrame`. Generated code
/// writes root `GcRef`s into `frame_ptr + SLOTS_OFFSET + index*8` at safepoints.
/// Computed from the `#[repr(C)]` layout so it stays correct if the struct
/// evolves (and the ABI version check catches a drift that matters).
const SLOTS_OFFSET: i64 = core::mem::offset_of!(ShadowFrame, slots) as i64;

/// The byte offset of `recursion_depth` within a `RuntimeContext`. The prologue
/// guard reads it (after the shadow-frame push bumps it) to decide whether to
/// branch to the stack-overflow fault epilogue (§9.2, §17.4). Computed from the
/// `#[repr(C)]` layout, like `SLOTS_OFFSET`.
const RECURSION_DEPTH_OFFSET: i64 = core::mem::offset_of!(RuntimeContext, recursion_depth) as i64;

/// The byte offset of `unit_ref` within a `RuntimeContext`. A fault epilogue
/// loads the immortal Unit from here and returns it, because the ABI says a
/// Praxis function returns a valid `GcRef` — including when it unwinds.
const UNIT_REF_OFFSET: i64 = core::mem::offset_of!(RuntimeContext, unit_ref) as i64;

/// Lower one MIR function into a Cranelift function and define it in `module`.
pub(crate) fn lower_function<M: Module>(
    module: &mut M,
    fn_ctx: &mut FunctionBuilderContext,
    mir: &MirFunction,
    user_funcs: &HashMap<String, FuncId>,
    db: &mut praxis_types::TypeDb,
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
        let count_val = builder.ins().iconst(GC, gc_count as i64);
        call_symbol(
            &mut builder,
            ctx_val,
            &[count_val],
            RuntimeSymbol::PushShadowFrame,
            module,
            &mut HashMap::new(),
        )?
    };
    builder.def_var(frame_var, frame_ptr);

    // Prologue (cont.): push a debug frame and keep its pointer in a Variable
    // (§9.3, ADR-021, M10-WS2). This frame is what the crash debugger reads for
    // `bt`/`locals`; the spill below keeps each `DebugLocal.value` fresh across
    // safepoints, parallel to the shadow-frame slots. The frame carries one
    // `DebugLocalMeta` per Gc local — in the same order as `gc_slot`, so a
    // local's shadow slot index doubles as its debug-local index.
    let debug_frame_var = builder.declare_var(GC);
    let debug_frame_ptr = {
        // Build the &'static [DebugLocalMeta] for this function's Gc locals.
        // Each entry carries the source name (embedded as &'static str), a
        // per-local symbol id placeholder, and the static type descriptor
        // resolved from the MIR local's Type.
        let metas = build_debug_local_metas(mir, db);
        let meta_ptr_val = builder.ins().iconst(GC, metas.as_ptr() as i64);
        // Embed the function name as a &'static str (ptr + len) for the frame.
        let name_static = leak_static_str(&mir.name);
        let name_ptr_val = builder.ins().iconst(GC, name_static.as_ptr() as i64);
        let name_len_val = builder.ins().iconst(GC, name_static.len() as i64);
        let count_val = builder.ins().iconst(GC, gc_count as i64);
        call_symbol(
            &mut builder,
            ctx_val,
            &[name_ptr_val, name_len_val, count_val, meta_ptr_val],
            RuntimeSymbol::PushDebugFrame,
            module,
            &mut HashMap::new(),
        )?
    };
    builder.def_var(debug_frame_var, debug_frame_ptr);

    // Prologue (cont.): record this function's source span on the just-pushed
    // debug frame (§9.3 "current source span", M10-WS1). Threaded AST → HIR
    // `TypedFn` → MIR `Function.span` → here. The crash debugger's `source`
    // command renders the faulting function's extent from this. A `(0, 0)`
    // span (synthetic/closure functions) is a no-op: the setter still writes
    // it, and the debugger treats `(0, 0)` as "no span recorded".
    {
        let start = builder.ins().iconst(GC, mir.span.0 as i64);
        let end = builder.ins().iconst(GC, mir.span.1 as i64);
        call_symbol(
            &mut builder,
            ctx_val,
            &[start, end],
            RuntimeSymbol::SetFrameSourceSpan,
            module,
            &mut HashMap::new(),
        )?;
    }

    let mut import_cache: HashMap<RuntimeSymbol, FuncRef> = HashMap::new();
    let mut user_func_cache: HashMap<String, FuncRef> = HashMap::new();
    let spill = SpillCtx {
        frame_var,
        debug_frame_var,
        slot_of: &gc_slot,
    };

    // Recursion-depth guard (§9.2, §17.4). The shadow-frame push above bumped
    // `ctx.recursion_depth`; read it back and, if it exceeds MAX_RECURSION_DEPTH,
    // branch to a stack-overflow fault epilogue instead of executing the body.
    // Without this, deep recursion (e.g. `count(100000)`) overflows the native
    // stack and the host aborts (SIGABRT); with it, the call faults cleanly as
    // `FaultKind::StackOverflow` and unwinds to the host like any other fault.
    //
    // Block 0's actual instructions run in `body_entry` (a fresh block), so the
    // `entry` block ends with this conditional branch.
    let body_entry = builder.create_block();
    let over_limit = builder.create_block();
    {
        // Load `(*ctx).recursion_depth` (u32) at its fixed `#[repr(C)]` offset.
        #[allow(deprecated)] // iadd_imm_s vs iadd_imm: offset is a small positive imm.
        let depth_addr = builder.ins().iadd_imm_s(ctx_val, RECURSION_DEPTH_OFFSET);
        let mut depth_flags = MemFlags::trusted();
        depth_flags.set_notrap();
        let depth = builder.ins().load(types::I32, depth_flags, depth_addr, 0);
        // Compare against the limit; branch if depth > MAX (signed is fine — the
        // saturating add in the push helper keeps depth non-negative and bounded).
        let limit = builder
            .ins()
            .iconst(types::I32, praxis_runtime::MAX_RECURSION_DEPTH as i64);
        let over = builder.ins().icmp(IntCC::SignedGreaterThan, depth, limit);
        builder.ins().brif(over, over_limit, &[], body_entry, &[]);
    }

    // The stack-overflow fault epilogue: raise the fault, pop the shadow frame
    // (which also decrements recursion_depth, balancing the prologue bump), and
    // return the Unit sentinel. Mirrors `Terminator::Fault` below.
    {
        builder.switch_to_block(over_limit);
        call_symbol(
            &mut builder,
            ctx_val,
            &[],
            RuntimeSymbol::RaiseStackOverflow,
            module,
            &mut import_cache,
        )?;
        // Snapshot the (deep) debug-frame chain before unwinding (M10-WS3).
        emit_snapshot_debug_chain(&mut builder, ctx_val, module, &mut import_cache)?;
        emit_pop_shadow_frame(&mut builder, ctx_val, &spill, module, &mut import_cache)?;
        emit_pop_debug_frame(&mut builder, ctx_val, &spill, module, &mut import_cache)?;
        let unit = load_unit_sentinel(&mut builder, ctx_val);
        builder.ins().return_(&[unit]);
    }

    // Lower each block. Blocks are sealed together after the whole CFG is built
    // so loop backedges resolve correctly. Block 0's body runs in `body_entry`
    // (the recursion guard's fall-through target), not the param-receiving
    // `entry` block.
    builder.switch_to_block(body_entry);
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
                &blocks,
                module,
                &mut import_cache,
                user_funcs,
                &mut user_func_cache,
                db,
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
/// Variables holding the current shadow-frame and debug-frame pointers, and the
/// Gc-local → slot-index map. At safepoints the backend stores each live root's
/// value into its shadow-stack slot (ADR-019) **and** its debug-local `value`
/// field (§9.3, M10-WS2), so a crash snapshot reflects live state.
struct SpillCtx<'a> {
    frame_var: Variable,
    /// The debug frame pointer Variable (M10-WS2). The spill mirrors each root
    /// write into the corresponding `DebugLocal.value` so the crash debugger
    /// sees fresh values without a separate mechanism.
    debug_frame_var: Variable,
    slot_of: &'a HashMap<LocalId, u32>,
}

/// The byte offset of `locals` within a `DebugFrame`, and of `value` within a
/// `DebugLocal`. The spill writes a live root into debug frame slot `i` at
/// `frame.locals[i].value`. Computed from the `#[repr(C)]` layouts so they stay
/// correct if the structs evolve.
const DEBUG_LOCALS_OFFSET: i64 = core::mem::offset_of!(praxis_runtime::DebugFrame, locals) as i64;
const DEBUG_VALUE_OFFSET: i64 = core::mem::offset_of!(praxis_runtime::DebugLocal, value) as i64;
const DEBUG_LOCAL_SIZE: i64 = core::mem::size_of::<praxis_runtime::DebugLocal>() as i64;

impl SpillCtx<'_> {
    /// Emit stores for every live root in `roots` into the shadow frame and the
    /// debug frame, just before a safepoint. Each root's current Cranelift value
    /// is written to `frame_ptr + SLOTS_OFFSET + slot_index*8` (§12.3) and to
    /// `debug_frame.locals[slot_index].value` (§9.3, M10-WS2).
    fn emit_spill(&self, builder: &mut FunctionBuilder, roots: &[LocalId], vars: &[Variable]) {
        if roots.is_empty() {
            return;
        }
        let frame_ptr = builder.use_var(self.frame_var);
        let debug_frame_ptr = builder.use_var(self.debug_frame_var);
        for &local in roots {
            let Some(&slot) = self.slot_of.get(&local) else {
                continue; // a Scalar local slipped into live_roots; it has no slot.
            };
            let val = builder.use_var(vars[local.0 as usize]);
            // --- shadow-stack slot (ADR-019) ---
            let off = SLOTS_OFFSET + (slot as i64) * 8;
            // `iadd_imm` is deprecated in Cranelift 0.134 in favor of the
            // sign/zero-extended variants; the slot offset is always a small
            // positive immediate so the distinction is immaterial.
            #[allow(deprecated)]
            let slot_addr = builder.ins().iadd_imm_s(frame_ptr, off);
            // Store into the frame slot; these accesses never trap (the frame is
            // always live and the offset is in-bounds by construction).
            let mut flags = MemFlags::trusted();
            flags.set_notrap();
            builder.ins().store(flags, val, slot_addr, 0);

            // --- debug-local value (§9.3, M10-WS2) ---
            // debug_frame.locals is a *mut DebugLocal; slot i's DebugLocal is at
            // *(debug_frame.locals) + i*size, and `value` is at +DEBUG_VALUE_OFFSET
            // within it. Load the locals base pointer, then compute the address.
            let locals_base_flags = MemFlags::trusted();
            let locals_base = builder.ins().load(
                GC,
                locals_base_flags,
                debug_frame_ptr,
                DEBUG_LOCALS_OFFSET as i32,
            );
            let local_off = (slot as i64) * DEBUG_LOCAL_SIZE + DEBUG_VALUE_OFFSET;
            #[allow(deprecated)]
            let value_addr = builder.ins().iadd_imm_s(locals_base, local_off);
            let mut vflags = MemFlags::trusted();
            vflags.set_notrap();
            builder.ins().store(vflags, val, value_addr, 0);
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
    blocks: &[Block],
    module: &mut M,
    imports: &mut HashMap<RuntimeSymbol, FuncRef>,
    user_funcs: &HashMap<String, FuncId>,
    user_cache: &mut HashMap<String, FuncRef>,
    db: &praxis_types::TypeDb,
) -> Result<()> {
    match inst {
        Inst::ConstInt { dst, value } => {
            let v = builder.ins().iconst(GC, *value);
            builder.def_var(vars[dst.0 as usize], v);
        }
        Inst::ConstFloat { dst, bits } => {
            // A float constant is carried through the uniform i64 scalar channel
            // as its IEEE-754 bit pattern. The bits are an exact i64, so we emit
            // them directly with `iconst` — no f64 materialization needed here.
            // (The bit-cast to/from f64 happens at arithmetic/comparison points.)
            let v = builder.ins().iconst(GC, *bits);
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
                    let result = call_symbol(
                        builder,
                        ctx_val,
                        &[arg],
                        RuntimeSymbol::AllocInt,
                        module,
                        imports,
                    )?;
                    builder.def_var(vars[dst.0 as usize], result);
                }
                AllocKind::Bool { value } => {
                    let arg = builder.use_var(vars[value.0 as usize]);
                    let result = call_symbol(
                        builder,
                        ctx_val,
                        &[arg],
                        RuntimeSymbol::AllocBool,
                        module,
                        imports,
                    )?;
                    builder.def_var(vars[dst.0 as usize], result);
                }
                AllocKind::Unit => {
                    let result = call_symbol(
                        builder,
                        ctx_val,
                        &[],
                        RuntimeSymbol::AllocUnit,
                        module,
                        imports,
                    )?;
                    builder.def_var(vars[dst.0 as usize], result);
                }
                AllocKind::Text { value } => {
                    // Embed the string as a data object, then call praxis_alloc_text
                    // with (ptr, len).
                    let (ptr, len_val) = embed_text(builder, module, value)?;
                    let result = call_symbol(
                        builder,
                        ctx_val,
                        &[ptr, len_val],
                        RuntimeSymbol::AllocText,
                        module,
                        imports,
                    )?;
                    builder.def_var(vars[dst.0 as usize], result);
                }
                AllocKind::Char { value } => {
                    let arg = builder.use_var(vars[value.0 as usize]);
                    let result = call_symbol(
                        builder,
                        ctx_val,
                        &[arg],
                        RuntimeSymbol::AllocChar,
                        module,
                        imports,
                    )?;
                    builder.def_var(vars[dst.0 as usize], result);
                }
                AllocKind::Float { value } => {
                    // The scalar local holds the f64 bit pattern as i64; the
                    // runtime wrapper `praxis_alloc_float` reassembles the f64.
                    let arg = builder.use_var(vars[value.0 as usize]);
                    let result = call_symbol(
                        builder,
                        ctx_val,
                        &[arg],
                        RuntimeSymbol::AllocFloat,
                        module,
                        imports,
                    )?;
                    builder.def_var(vars[dst.0 as usize], result);
                }
                AllocKind::Record {
                    record_def_id,
                    fields,
                } => {
                    // Build (or fetch a cached) 'static RecordSchema from the
                    // def-id, leak it, embed its address, and call
                    // praxis_alloc_record(ctx, schema_ptr). Then fill in each
                    // field via praxis_record_set_field.
                    let schema_ptr = record_schema_for(db, *record_def_id);
                    let schema_imm = builder.ins().iconst(GC, schema_ptr as i64);
                    // praxis_alloc_record(ctx, schema_ptr) -> GcRef.
                    let record_ref = call_symbol(
                        builder,
                        ctx_val,
                        &[schema_imm],
                        RuntimeSymbol::AllocRecord,
                        module,
                        imports,
                    )?;
                    // Fill in each field in declaration order. The field locals
                    // are already spilled into the shadow frame by
                    // `emit_spill` above; here we pass them as call args.
                    for (idx, field_local) in fields.iter().enumerate() {
                        let field_val = builder.use_var(vars[field_local.0 as usize]);
                        let idx_val = builder.ins().iconst(GC, idx as i64);
                        call_symbol(
                            builder,
                            ctx_val,
                            &[record_ref, idx_val, field_val],
                            RuntimeSymbol::RecordSetField,
                            module,
                            imports,
                        )?;
                    }
                    builder.def_var(vars[dst.0 as usize], record_ref);
                }
                AllocKind::Enum {
                    enum_def_id: _,
                    variant_idx,
                    args,
                } => {
                    // praxis_alloc_enum(ctx, tag, arity) -> GcRef. Then fill in
                    // each payload via praxis_enum_set_payload.
                    let tag_val = builder.ins().iconst(GC, *variant_idx as i64);
                    let arity_val = builder.ins().iconst(GC, args.len() as i64);
                    let enum_ref = call_symbol(
                        builder,
                        ctx_val,
                        &[tag_val, arity_val],
                        RuntimeSymbol::AllocEnum,
                        module,
                        imports,
                    )?;
                    for (idx, arg_local) in args.iter().enumerate() {
                        let arg_val = builder.use_var(vars[arg_local.0 as usize]);
                        let idx_val = builder.ins().iconst(GC, idx as i64);
                        call_symbol(
                            builder,
                            ctx_val,
                            &[enum_ref, idx_val, arg_val],
                            RuntimeSymbol::EnumSetPayload,
                            module,
                            imports,
                        )?;
                    }
                    builder.def_var(vars[dst.0 as usize], enum_ref);
                }
                AllocKind::Tuple { ty, elements } => {
                    // Build (or fetch a cached) 'static TupleSchema from the
                    // tuple's static type, leak it, embed its address, and call
                    // praxis_alloc_tuple(ctx, schema_ptr). Then fill in each
                    // element via praxis_tuple_set.
                    let schema_ptr = tuple_schema_for(db, *ty);
                    let schema_imm = builder.ins().iconst(GC, schema_ptr as i64);
                    // praxis_alloc_tuple(ctx, schema_ptr) -> GcRef.
                    let tuple_ref = call_symbol(
                        builder,
                        ctx_val,
                        &[schema_imm],
                        RuntimeSymbol::AllocTuple,
                        module,
                        imports,
                    )?;
                    // Fill in each element in positional order.
                    for (idx, el_local) in elements.iter().enumerate() {
                        let el_val = builder.use_var(vars[el_local.0 as usize]);
                        let idx_val = builder.ins().iconst(GC, idx as i64);
                        call_symbol(
                            builder,
                            ctx_val,
                            &[tuple_ref, idx_val, el_val],
                            RuntimeSymbol::TupleSet,
                            module,
                            imports,
                        )?;
                    }
                    builder.def_var(vars[dst.0 as usize], tuple_ref);
                }
                AllocKind::Closure { fn_name, captures } => {
                    // M7, §4.10. Take the synthetic function's address (the
                    // symbol is declared in `Jit::compile`'s first pass since the
                    // synthetic fn is appended to the function list), then
                    // allocate the closure via `praxis_alloc_closure(ctx, fn_ptr,
                    // n)` and fill each capture slot via
                    // `praxis_closure_set_capture(ctx, closure, idx, value)`.
                    let fr = user_funcref(fn_name, user_funcs, user_cache, module, builder)?;
                    let fn_ptr_val = builder.ins().func_addr(GC, fr);
                    let n_val = builder.ins().iconst(GC, captures.len() as i64);
                    let closure_ref = call_symbol(
                        builder,
                        ctx_val,
                        &[fn_ptr_val, n_val],
                        RuntimeSymbol::AllocClosure,
                        module,
                        imports,
                    )?;
                    for (idx, cap_local) in captures.iter().enumerate() {
                        let cap_val = builder.use_var(vars[cap_local.0 as usize]);
                        let idx_val = builder.ins().iconst(GC, idx as i64);
                        call_symbol(
                            builder,
                            ctx_val,
                            &[closure_ref, idx_val, cap_val],
                            RuntimeSymbol::ClosureSetCapture,
                            module,
                            imports,
                        )?;
                    }
                    builder.def_var(vars[dst.0 as usize], closure_ref);
                }
                AllocKind::Collection { ctor, args } => {
                    // M8 WS1: `Vec[T]()`, `Grid[T]()`, etc. Resolve the real
                    // element descriptor (closing the M7 null-descriptor
                    // carryover) and call `praxis_<kind>_new`. The element
                    // descriptor is resolved recursively so nested collections
                    // (e.g. `Vec[Vec[Int]]`) dispatch eq/hash correctly.
                    use praxis_types::CollectionCtor;
                    match ctor {
                        CollectionCtor::Vec => {
                            let el_desc = collection_element_descriptor_for(db, args[0]);
                            let el_imm = builder.ins().iconst(GC, el_desc as i64);
                            let vec_ref = call_symbol(
                                builder,
                                ctx_val,
                                &[el_imm],
                                RuntimeSymbol::VecNew,
                                module,
                                imports,
                            )?;
                            builder.def_var(vars[dst.0 as usize], vec_ref);
                        }
                        CollectionCtor::Deque => {
                            // Deque mirrors Vec: a single element descriptor
                            // passed to praxis_deque_new (M8-WS2, §6.1).
                            let el_desc = collection_element_descriptor_for(db, args[0]);
                            let el_imm = builder.ins().iconst(GC, el_desc as i64);
                            let deque_ref = call_symbol(
                                builder,
                                ctx_val,
                                &[el_imm],
                                RuntimeSymbol::DequeNew,
                                module,
                                imports,
                            )?;
                            builder.def_var(vars[dst.0 as usize], deque_ref);
                        }
                        CollectionCtor::Map => {
                            // Map: pass the key descriptor to praxis_map_new.
                            // The value descriptor is adopted from the first
                            // inserted value at runtime (§11.3).
                            let key_desc = collection_element_descriptor_for(db, args[0]);
                            let key_imm = builder.ins().iconst(GC, key_desc as i64);
                            let map_ref = call_symbol(
                                builder,
                                ctx_val,
                                &[key_imm],
                                RuntimeSymbol::MapNew,
                                module,
                                imports,
                            )?;
                            builder.def_var(vars[dst.0 as usize], map_ref);
                        }
                        CollectionCtor::Set => {
                            // Set: pass the element descriptor.
                            let el_desc = collection_element_descriptor_for(db, args[0]);
                            let el_imm = builder.ins().iconst(GC, el_desc as i64);
                            let set_ref = call_symbol(
                                builder,
                                ctx_val,
                                &[el_imm],
                                RuntimeSymbol::SetNew,
                                module,
                                imports,
                            )?;
                            builder.def_var(vars[dst.0 as usize], set_ref);
                        }
                        CollectionCtor::Counter => {
                            // Counter: pass the key descriptor.
                            let key_desc = collection_element_descriptor_for(db, args[0]);
                            let key_imm = builder.ins().iconst(GC, key_desc as i64);
                            let counter_ref = call_symbol(
                                builder,
                                ctx_val,
                                &[key_imm],
                                RuntimeSymbol::CounterNew,
                                module,
                                imports,
                            )?;
                            builder.def_var(vars[dst.0 as usize], counter_ref);
                        }
                        CollectionCtor::MinHeap | CollectionCtor::MaxHeap => {
                            // Heaps: pass the element descriptor; the runtime
                            // selects min vs max by the construction symbol.
                            let el_desc = collection_element_descriptor_for(db, args[0]);
                            let el_imm = builder.ins().iconst(GC, el_desc as i64);
                            let sym = if *ctor == CollectionCtor::MinHeap {
                                RuntimeSymbol::MinHeapNew
                            } else {
                                RuntimeSymbol::MaxHeapNew
                            };
                            let heap_ref =
                                call_symbol(builder, ctx_val, &[el_imm], sym, module, imports)?;
                            builder.def_var(vars[dst.0 as usize], heap_ref);
                        }
                        CollectionCtor::BitSet => {
                            // BitSet is nullary (no element descriptor); elements
                            // are always Int. praxis_bitset_new takes only ctx.
                            let bs_ref = call_symbol(
                                builder,
                                ctx_val,
                                &[],
                                RuntimeSymbol::BitsetNew,
                                module,
                                imports,
                            )?;
                            builder.def_var(vars[dst.0 as usize], bs_ref);
                        }
                        CollectionCtor::Grid => {
                            // Grid construction from source `Grid()`: an empty
                            // 0×0 grid. (The input parser is the usual grid
                            // constructor; source construction is for manual
                            // grids filled via set.) praxis_grid_new takes
                            // (descriptor, width, height).
                            let el_desc = collection_element_descriptor_for(db, args[0]);
                            let el_imm = builder.ins().iconst(GC, el_desc as i64);
                            let w_imm = builder.ins().iconst(GC, 0);
                            let h_imm = builder.ins().iconst(GC, 0);
                            let grid_ref = call_symbol(
                                builder,
                                ctx_val,
                                &[el_imm, w_imm, h_imm],
                                RuntimeSymbol::GridNew,
                                module,
                                imports,
                            )?;
                            builder.def_var(vars[dst.0 as usize], grid_ref);
                        }
                        // Other collection ctors (Deque/Map/Set/Counter/MinHeap/
                        // MaxHeap/BitSet/Range/Seq) land in their own WS and add
                        // arms here. They are unreachable from source until then
                        // (collection_from_name resolves the *type*, but no
                        // `praxis_<kind>_new` wrapper exists yet).
                        _ => {
                            return Err(anyhow!(
                                "construction of {ctor:?} not yet implemented (M8 workstream)"
                            ));
                        }
                    }
                }
            }
        }
        Inst::ExtractScalar { dst, src, scalar } => {
            let src_val = builder.use_var(vars[src.0 as usize]);
            let sym = match scalar {
                ScalarKind::Int => RuntimeSymbol::IntLoad,
                ScalarKind::Bool => RuntimeSymbol::BoolLoad,
                ScalarKind::Char => RuntimeSymbol::CharLoad,
                // Float's payload is read as its f64 bit pattern (i64 channel).
                ScalarKind::Float => RuntimeSymbol::FloatLoad,
                // Byte is not yet wired (reserved); read as Int defensively.
                ScalarKind::Byte => RuntimeSymbol::IntLoad,
            };
            let result = call_symbol(builder, ctx_val, &[src_val], sym, module, imports)?;
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
            // A scalar payload re-boxed: Int → praxis_alloc_int, Bool → alloc_bool,
            // Char → praxis_alloc_char.
            let src_val = builder.use_var(vars[src.0 as usize]);
            let sym = match scalar {
                ScalarKind::Int => RuntimeSymbol::AllocInt,
                ScalarKind::Bool => RuntimeSymbol::AllocBool,
                ScalarKind::Char => RuntimeSymbol::AllocChar,
                // Float's bit pattern is boxed by praxis_alloc_float.
                ScalarKind::Float => RuntimeSymbol::AllocFloat,
                // Byte is not yet wired (reserved); box as Int defensively.
                ScalarKind::Byte => RuntimeSymbol::AllocInt,
            };
            let result = call_symbol(builder, ctx_val, &[src_val], sym, module, imports)?;
            builder.def_var(vars[dst.0 as usize], result);
        }
        Inst::StoreScalar { .. } => {
            // M4 scalars are immutable objects; StoreScalar is a no-op placeholder
            // for the future mutable-Int optimization.
        }
        Inst::IntBinOp { op, dst, lhs, rhs } => {
            // Native scalar arithmetic (§4.12). The operands are already raw
            // i64s in the scalar channel, so the operation is one Cranelift
            // instruction plus an inline overflow predicate.
            //
            // This replaces boxing both operands with `praxis_alloc_int`,
            // calling the wrapper, and `praxis_int_load`ing the result: two
            // allocations and three calls per arithmetic op. That shape also
            // carried a live memory bug — on fault the wrapper returns the Unit
            // sentinel, and the `int_load` ran *before* the fault check, reading
            // eight bytes past a size-0 Unit payload.
            //
            // Overflow is reported by calling a non-allocating raise wrapper
            // with the predicate rather than by branching around it: arithmetic
            // stays one basic block, and the `CheckFault` that MIR emits next
            // is what diverts to the fault epilogue.
            let l = builder.use_var(vars[lhs.0 as usize]);
            let r = builder.use_var(vars[rhs.0 as usize]);
            let result = match op {
                IntBinOp::Add => {
                    let sum = builder.ins().iadd(l, r);
                    // Signed overflow iff the operands agree in sign and the
                    // result disagrees with them: ((l ^ sum) & (r ^ sum)) < 0.
                    let a = builder.ins().bxor(l, sum);
                    let b = builder.ins().bxor(r, sum);
                    let both = builder.ins().band(a, b);
                    raise_if_negative(
                        builder,
                        ctx_val,
                        both,
                        RuntimeSymbol::RaiseIntOverflowIf,
                        module,
                        imports,
                    )?;
                    sum
                }
                IntBinOp::Sub => {
                    let diff = builder.ins().isub(l, r);
                    // Signed overflow iff the operands differ in sign and the
                    // result differs from the left: ((l ^ r) & (l ^ diff)) < 0.
                    let a = builder.ins().bxor(l, r);
                    let b = builder.ins().bxor(l, diff);
                    let both = builder.ins().band(a, b);
                    raise_if_negative(
                        builder,
                        ctx_val,
                        both,
                        RuntimeSymbol::RaiseIntOverflowIf,
                        module,
                        imports,
                    )?;
                    diff
                }
                IntBinOp::Mul => {
                    let product = builder.ins().imul(l, r);
                    // The full 128-bit product fits in 64 bits iff its high half
                    // is the sign extension of the low half.
                    let high = builder.ins().smulhi(l, r);
                    let sign = builder.ins().sshr_imm_u(product, 63);
                    let differs = builder.ins().icmp(IntCC::NotEqual, high, sign);
                    let flag = builder.ins().uextend(GC, differs);
                    raise_if_nonzero(
                        builder,
                        ctx_val,
                        flag,
                        RuntimeSymbol::RaiseIntOverflowIf,
                        module,
                        imports,
                    )?;
                    product
                }
                IntBinOp::Div | IntBinOp::Rem => {
                    // `sdiv`/`srem` trap on a zero divisor and on the one
                    // overflowing signed division, `i64::MIN / -1`. Neither may
                    // reach the instruction: a trap is a process abort, not a
                    // Praxis fault. Substituting a divisor of 1 in those cases
                    // keeps the instruction total; the value it produces is
                    // dead, because the `CheckFault` after this diverts.
                    let zero = builder.ins().iconst(GC, 0);
                    let by_zero = builder.ins().icmp(IntCC::Equal, r, zero);
                    let min = builder.ins().iconst(GC, i64::MIN);
                    let neg_one = builder.ins().iconst(GC, -1);
                    let l_is_min = builder.ins().icmp(IntCC::Equal, l, min);
                    let r_is_neg_one = builder.ins().icmp(IntCC::Equal, r, neg_one);
                    let overflows = builder.ins().band(l_is_min, r_is_neg_one);

                    let one = builder.ins().iconst(GC, 1);
                    let unsafe_divisor = builder.ins().bor(by_zero, overflows);
                    let divisor = builder.ins().select(unsafe_divisor, one, r);
                    let value = if matches!(op, IntBinOp::Div) {
                        builder.ins().sdiv(l, divisor)
                    } else {
                        builder.ins().srem(l, divisor)
                    };

                    let by_zero_flag = builder.ins().uextend(GC, by_zero);
                    raise_if_nonzero(
                        builder,
                        ctx_val,
                        by_zero_flag,
                        RuntimeSymbol::RaiseDivByZeroIf,
                        module,
                        imports,
                    )?;
                    let overflow_flag = builder.ins().uextend(GC, overflows);
                    raise_if_nonzero(
                        builder,
                        ctx_val,
                        overflow_flag,
                        RuntimeSymbol::RaiseIntOverflowIf,
                        module,
                        imports,
                    )?;
                    value
                }
            };
            builder.def_var(vars[dst.0 as usize], result);
            // Fault check after faultable arith.
            let _ = call_symbol(
                builder,
                ctx_val,
                &[],
                RuntimeSymbol::CheckFault,
                module,
                imports,
            )?;
        }
        Inst::IntCmp { op, dst, lhs, rhs } => {
            // Compare the scalar operands directly in Cranelift (no boxing). The
            // operands are i64 scalar values; Cranelift's `icmp` produces an i8
            // condition which we widen to i64 for uniformity. This avoids the
            // allocation safepoints that the boxed-comparison path would
            // introduce (which would require spilling live Gc roots not tracked
            // by the MIR liveness pass for non-safepoint instructions).
            let l = builder.use_var(vars[lhs.0 as usize]);
            let r = builder.use_var(vars[rhs.0 as usize]);
            let cond = match op {
                CmpOp::Eq => cranelift::codegen::ir::condcodes::IntCC::Equal,
                CmpOp::Neq => cranelift::codegen::ir::condcodes::IntCC::NotEqual,
                CmpOp::Lt => cranelift::codegen::ir::condcodes::IntCC::SignedLessThan,
                CmpOp::Gt => cranelift::codegen::ir::condcodes::IntCC::SignedGreaterThan,
                CmpOp::Le => cranelift::codegen::ir::condcodes::IntCC::SignedLessThanOrEqual,
                CmpOp::Ge => cranelift::codegen::ir::condcodes::IntCC::SignedGreaterThanOrEqual,
            };
            let cmp = builder.ins().icmp(cond, l, r);
            let widened = builder.ins().uextend(GC, cmp);
            builder.def_var(vars[dst.0 as usize], widened);
        }
        Inst::FloatBinOp { op, dst, lhs, rhs } => {
            // Float operands are carried as i64 bit patterns; bit-cast to f64,
            // apply the native Cranelift float op, then bit-cast the result back
            // to i64 for uniform storage. No fault check — IEEE-754 arithmetic
            // never faults (overflow → ±inf, div-by-zero → ±inf/NaN) (§4.12).
            let l_i = builder.use_var(vars[lhs.0 as usize]);
            let r_i = builder.use_var(vars[rhs.0 as usize]);
            let l = i64_to_f64(builder, l_i);
            let r = i64_to_f64(builder, r_i);
            let result_f = match op {
                FloatBinOp::Add => builder.ins().fadd(l, r),
                FloatBinOp::Sub => builder.ins().fsub(l, r),
                FloatBinOp::Mul => builder.ins().fmul(l, r),
                FloatBinOp::Div => builder.ins().fdiv(l, r),
            };
            let result = f64_to_i64(builder, result_f);
            builder.def_var(vars[dst.0 as usize], result);
        }
        Inst::FloatCmp { op, dst, lhs, rhs } => {
            // IEEE-754 comparison: bit-cast operands to f64, then `fcmp` with a
            // `FloatCC`. This gives NaN semantics for free (NaN compares unordered
            // to everything, so `NaN == NaN` and `NaN < x` are both false). The
            // i8 result is widened to i64 for uniformity, mirroring `IntCmp`.
            let l_i = builder.use_var(vars[lhs.0 as usize]);
            let r_i = builder.use_var(vars[rhs.0 as usize]);
            let l = i64_to_f64(builder, l_i);
            let r = i64_to_f64(builder, r_i);
            let cond = match op {
                CmpOp::Eq => cranelift::codegen::ir::condcodes::FloatCC::Equal,
                CmpOp::Neq => cranelift::codegen::ir::condcodes::FloatCC::NotEqual,
                CmpOp::Lt => cranelift::codegen::ir::condcodes::FloatCC::LessThan,
                CmpOp::Gt => cranelift::codegen::ir::condcodes::FloatCC::GreaterThan,
                CmpOp::Le => cranelift::codegen::ir::condcodes::FloatCC::LessThanOrEqual,
                CmpOp::Ge => cranelift::codegen::ir::condcodes::FloatCC::GreaterThanOrEqual,
            };
            let cmp = builder.ins().fcmp(cond, l, r);
            let widened = builder.ins().uextend(GC, cmp);
            builder.def_var(vars[dst.0 as usize], widened);
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
            let result = match callee {
                CallTarget::User(name) => {
                    let funcref = user_funcref(name, user_funcs, user_cache, module, builder)?;
                    let mut call_args = vec![ctx_val];
                    call_args.extend(arg_vals);
                    let call = builder.ins().call(funcref, &call_args);
                    builder.func.dfg.first_result(call)
                }
                // The wrapper's shape comes from its manifest row, not from the
                // argument count: `call_symbol` checks the arity and narrows
                // each argument to the width the row declares. `args` already
                // includes the receiver as its first element.
                CallTarget::Runtime(sym) => {
                    call_symbol(builder, ctx_val, &arg_vals, *sym, module, imports)?
                }
            };
            builder.def_var(vars[dst.0 as usize], result);
        }
        Inst::CallIndirect {
            dst,
            callee,
            args,
            live_roots,
        } => {
            // M7, §4.10 (Approach B). An indirect call through a closure value.
            // Spill live Gc roots (safepoint — the call may allocate/GC), read
            // the closure's `fn_ptr` via `praxis_closure_fn_ptr`, then emit a
            // Cranelift `call_indirect` with the signature
            // `fn(ctx, closure, args...) -> i64`. The closure is passed as the
            // hidden first explicit arg; the synthetic function loads its
            // captures at entry.
            spill.emit_spill(builder, live_roots, vars);
            let callee_val = builder.use_var(vars[callee.0 as usize]);
            let arg_vals: Vec<Value> = args
                .iter()
                .map(|a| builder.use_var(vars[a.0 as usize]))
                .collect();
            // fn_ptr = praxis_closure_fn_ptr(closure). (No ctx arg.)
            let fn_ptr = call_symbol(
                builder,
                ctx_val,
                &[callee_val],
                RuntimeSymbol::ClosureFnPtr,
                module,
                imports,
            )?;
            // Build the indirect-call signature: fn(ctx, closure, args...) -> i64.
            let mut sig = Signature::new(CallConv::Fast);
            sig.params.push(AbiParam::new(GC)); // ctx
            sig.params.push(AbiParam::new(GC)); // closure (self)
            for _ in &arg_vals {
                sig.params.push(AbiParam::new(GC));
            }
            sig.returns.push(AbiParam::new(GC));
            let sig_ref = builder.import_signature(sig);
            // call_indirect(sig, fn_ptr, [ctx, closure, args...])
            let mut call_args = vec![ctx_val, callee_val];
            call_args.extend(arg_vals);
            let call = builder.ins().call_indirect(sig_ref, fn_ptr, &call_args);
            let result = builder.func.dfg.first_result(call);
            builder.def_var(vars[dst.0 as usize], result);
        }
        Inst::StructEq {
            dst,
            lhs,
            rhs,
            live_roots,
        } => {
            // Structural equality via praxis_struct_eq(ctx, a, b) -> i64 (0/1).
            // The call may trigger GC → spill live Gc roots first (safepoint).
            spill.emit_spill(builder, live_roots, vars);
            let l = builder.use_var(vars[lhs.0 as usize]);
            let r = builder.use_var(vars[rhs.0 as usize]);
            let result = call_symbol(
                builder,
                ctx_val,
                &[l, r],
                RuntimeSymbol::StructEq,
                module,
                imports,
            )?;
            builder.def_var(vars[dst.0 as usize], result);
        }
        Inst::CheckFault {
            on_fault,
            live_roots,
        } => {
            // Divert to the fault block when a fault is pending (§10.4). The
            // faultable op just before this set `pending_fault` (or a callee
            // did); `praxis_check_fault` returns 1 iff a fault is pending. If so,
            // branch to the function's fault block — which pops the shadow frame
            // and returns the Unit sentinel, unwinding cleanly to the host. The
            // rest of this MIR block's instructions lower into a fresh
            // fall-through block, so the diversion does not strand them.
            //
            // This is load-bearing for faults that must propagate through
            // subsequent operations (notably StackOverflow: a deeply-recursive
            // caller receives the Unit sentinel from a child and would otherwise
            // feed it to an arithmetic wrapper before the host can observe the
            // fault). Branching here keeps every operand on the fault path valid.
            //
            // Spill live roots into the debug frame *before* the fault test:
            // CheckFault is a debugger (not GC) safepoint. Without this, a
            // snapshot taken on the fault path sees `<uninit>` for operands
            // computed since the last GC safepoint (e.g. the `0` divisor in
            // `x / 0`). The faulting op's own result is genuinely never
            // produced (the fault happens during it), so it stays `<uninit>`.
            spill.emit_spill(builder, live_roots, vars);
            let pending = call_symbol(
                builder,
                ctx_val,
                &[],
                RuntimeSymbol::CheckFault,
                module,
                imports,
            )?;
            let fault_block = blocks[on_fault.0 as usize];
            let fallthrough = builder.create_block();
            builder
                .ins()
                .brif(pending, fault_block, &[], fallthrough, &[]);
            builder.switch_to_block(fallthrough);
        }
        Inst::MoveGc { dst, src } => {
            let v = builder.use_var(vars[src.0 as usize]);
            builder.def_var(vars[dst.0 as usize], v);
        }
        Inst::LoadField {
            dst,
            src,
            field_idx,
        } => {
            // praxis_record_field(ctx, record, idx) -> GcRef. Not a safepoint.
            let record = builder.use_var(vars[src.0 as usize]);
            let idx_val = builder.ins().iconst(GC, *field_idx as i64);
            let field = call_symbol(
                builder,
                ctx_val,
                &[record, idx_val],
                RuntimeSymbol::RecordField,
                module,
                imports,
            )?;
            builder.def_var(vars[dst.0 as usize], field);
        }
        Inst::EnumTag { dst, src } => {
            // Read the tag directly from the EnumPayload. The payload starts at
            // gc_ref + GcHeader::payload_offset_for(align_of(EnumPayload)) —
            // the runtime's single object-layout authority, not a header size
            // this file re-derives. The tag is a u32 at offset 0 of the payload.
            let enum_ref = builder.use_var(vars[src.0 as usize]);
            let payload_offset =
                praxis_runtime::gc::GcHeader::payload_offset_for(core::mem::align_of::<
                    praxis_runtime::enums::EnumPayload,
                >()) as i64;
            let tag_ptr = builder.ins().iadd_imm_s(enum_ref, payload_offset);
            // Read just the u32 tag (not a full I64 — the 4 bytes of padding
            // after the tag are uninitialized bumpalo memory). In Cranelift
            // 0.134, uload32 returns an I64 with the upper 32 bits zeroed.
            let tag = builder.ins().uload32(MemFlags::trusted(), tag_ptr, 0);
            builder.def_var(vars[dst.0 as usize], tag);
        }
        Inst::EnumPayloadGet { dst, src, idx } => {
            // Read payload slot `idx` from the EnumPayload. The payload is
            // { tag: u32, items: Vec<GcRef> }. The Vec's data pointer is at
            // offset 8 of EnumPayload (after tag:u32 + 4 padding). Slot `idx`
            // is at data_ptr + idx * 8. We call praxis_enum_payload (a runtime
            // ABI wrapper) rather than reading the Vec's internal layout
            // directly, because Vec's field order is repr(Rust) and not stable.
            let enum_ref = builder.use_var(vars[src.0 as usize]);
            let idx_val = builder.ins().iconst(GC, *idx as i64);
            let slot_val = call_symbol(
                builder,
                ctx_val,
                &[enum_ref, idx_val],
                RuntimeSymbol::EnumPayload,
                module,
                imports,
            )?;
            builder.def_var(vars[dst.0 as usize], slot_val);
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
    imports: &mut HashMap<RuntimeSymbol, FuncRef>,
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
            // Epilogue: pop the shadow frame and debug frame before returning
            // (ADR-019, §9.3/M10-WS2).
            emit_pop_shadow_frame(builder, ctx_val, spill, module, imports)?;
            emit_pop_debug_frame(builder, ctx_val, spill, module, imports)?;
            let v = builder.use_var(vars[value.0 as usize]);
            builder.ins().return_(&[v]);
        }
        Terminator::Fault => {
            // Epilogue (fault path): snapshot the debug-frame chain BEFORE
            // popping, so the host can inspect the intact chain after unwind
            // (§9.3, M10-WS3). Idempotent: only the innermost frame's epilogue
            // (which runs first) captures; outer frames unwinding later skip.
            emit_snapshot_debug_chain(builder, ctx_val, module, imports)?;
            // Then pop the shadow frame and debug frame before unwinding.
            emit_pop_shadow_frame(builder, ctx_val, spill, module, imports)?;
            emit_pop_debug_frame(builder, ctx_val, spill, module, imports)?;
            // Unwind to the host: return the Unit sentinel (the caller checks
            // pending_fault). The fault block has no value of its own — but it
            // still returns a `GcRef`, so it must return a *valid* one. It used
            // to return `iconst 0`, a null reference across an ABI whose return
            // type is non-null: every caller that inspected the result before
            // checking the fault dereferenced null.
            let unit = load_unit_sentinel(builder, ctx_val);
            builder.ins().return_(&[unit]);
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
    imports: &mut HashMap<RuntimeSymbol, FuncRef>,
) -> Result<()> {
    let frame_ptr = builder.use_var(spill.frame_var);
    call_symbol(
        builder,
        ctx_val,
        &[frame_ptr],
        RuntimeSymbol::PopShadowFrame,
        module,
        imports,
    )?;
    Ok(())
}

/// Emit the `praxis_pop_debug_frame(ctx, frame)` epilogue call (§9.3, M10-WS2).
/// Mirrors [`emit_pop_shadow_frame`] for the debug-frame chain.
fn emit_pop_debug_frame<M: Module>(
    builder: &mut FunctionBuilder,
    ctx_val: Value,
    spill: &SpillCtx<'_>,
    module: &mut M,
    imports: &mut HashMap<RuntimeSymbol, FuncRef>,
) -> Result<()> {
    let frame_ptr = builder.use_var(spill.debug_frame_var);
    call_symbol(
        builder,
        ctx_val,
        &[frame_ptr],
        RuntimeSymbol::PopDebugFrame,
        module,
        imports,
    )?;
    Ok(())
}

/// Emit the `praxis_snapshot_debug_chain(ctx)` fault-epilogue call (§9.3,
/// M10-WS3). Must run BEFORE the debug-frame pop, while the chain is intact.
/// Idempotent at runtime: only the first (innermost) call captures.
fn emit_snapshot_debug_chain<M: Module>(
    builder: &mut FunctionBuilder,
    ctx_val: Value,
    module: &mut M,
    imports: &mut HashMap<RuntimeSymbol, FuncRef>,
) -> Result<()> {
    call_symbol(
        builder,
        ctx_val,
        &[],
        RuntimeSymbol::SnapshotDebugChain,
        module,
        imports,
    )?;
    Ok(())
}

// --- helpers: declare imports + emit calls -------------------------------

/// The Cranelift type one ABI parameter kind is passed as.
///
/// `RawU32` is the reason this function exists: it is the one kind narrower
/// than a machine word, and the arity-derived signature this replaces passed an
/// `i64` there.
fn abi_type(kind: AbiKind, pointer: types::Type) -> types::Type {
    match kind {
        AbiKind::Ctx | AbiKind::Gc | AbiKind::Ptr => pointer,
        AbiKind::RawI64 => types::I64,
        AbiKind::RawU32 => types::I32,
    }
}

/// The Cranelift signature for a runtime symbol, derived from its manifest row.
///
/// Call convention and pointer width come from the module's ISA rather than
/// from a literal, so a target whose conventions differ produces a wrong-target
/// error at `Jit::new` instead of a wrong signature here.
fn signature_for<M: Module>(sym: RuntimeSymbol, module: &M) -> Signature {
    let pointer = module.target_config().pointer_type();
    let abi = sym.sig();
    let mut sig = Signature::new(module.isa().default_call_conv());
    for &kind in abi.params {
        sig.params.push(AbiParam::new(abi_type(kind, pointer)));
    }
    match abi.ret {
        AbiRet::Gc | AbiRet::Ptr => sig.returns.push(AbiParam::new(pointer)),
        AbiRet::RawI64 => sig.returns.push(AbiParam::new(types::I64)),
        AbiRet::Void => {}
    }
    sig
}

/// Declare (lazily) and return the `FuncRef` for a runtime symbol.
///
/// The signature is the manifest's, never the call site's guess — that is what
/// makes "the compiler called a wrapper with the wrong shape" unrepresentable.
fn import<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    imports: &mut HashMap<RuntimeSymbol, FuncRef>,
    sym: RuntimeSymbol,
) -> Result<FuncRef> {
    if let Some(&fr) = imports.get(&sym) {
        return Ok(fr);
    }
    let sig = signature_for(sym, module);
    let id = match module.declare_function(sym.name(), Linkage::Import, &sig) {
        Ok(id) => id,
        Err(_) => func_id_for(module, sym.name())?,
    };
    let fr = module.declare_func_in_func(id, builder.func);
    imports.insert(sym, fr);
    Ok(fr)
}

/// Load the immortal Unit singleton out of the context (`ctx.unit_ref`).
///
/// This is the value every fault path returns. The runtime wrappers already do
/// the same thing (`unit_sentinel`), so a faulted call and a faulted function
/// now hand back the same object rather than one of them handing back null.
fn load_unit_sentinel(builder: &mut FunctionBuilder, ctx: Value) -> Value {
    builder
        .ins()
        .load(GC, MemFlags::trusted(), ctx, UNIT_REF_OFFSET as i32)
}

/// Report a fault when `predicate` is negative — the sign-bit form the
/// add/sub overflow tests produce.
fn raise_if_negative<M: Module>(
    builder: &mut FunctionBuilder,
    ctx: Value,
    predicate: Value,
    sym: RuntimeSymbol,
    module: &mut M,
    imports: &mut HashMap<RuntimeSymbol, FuncRef>,
) -> Result<()> {
    let flag = builder.ins().ushr_imm_u(predicate, 63);
    raise_if_nonzero(builder, ctx, flag, sym, module, imports)
}

/// Report a fault when `flag` is non-zero.
///
/// The call is unconditional and the wrapper decides: an arithmetic site stays
/// one basic block, and the wrapper allocates nothing, so the site is not a GC
/// safepoint and needs no root spill.
fn raise_if_nonzero<M: Module>(
    builder: &mut FunctionBuilder,
    ctx: Value,
    flag: Value,
    sym: RuntimeSymbol,
    module: &mut M,
    imports: &mut HashMap<RuntimeSymbol, FuncRef>,
) -> Result<()> {
    call_symbol(builder, ctx, &[flag], sym, module, imports)?;
    Ok(())
}

/// Emit a call to `sym`, narrowing each argument to the width its manifest row
/// declares.
///
/// Every value in the lowering is carried as an `I64`; a parameter declared
/// `RawU32` therefore needs an explicit `ireduce`. Doing it here, from the
/// manifest, means no call site can forget — the arity-only path this replaces
/// fed the full 64-bit value into `praxis_record_field`'s `u32` index.
fn call_symbol<M: Module>(
    builder: &mut FunctionBuilder,
    ctx: Value,
    args: &[Value],
    sym: RuntimeSymbol,
    module: &mut M,
    imports: &mut HashMap<RuntimeSymbol, FuncRef>,
) -> Result<Value> {
    let abi = sym.sig();
    anyhow::ensure!(
        args.len() == abi.arity(),
        "runtime symbol `{sym}` takes {} arguments, {} were passed",
        abi.arity(),
        args.len()
    );
    let pointer = module.target_config().pointer_type();
    let fr = import(module, builder, imports, sym)?;
    let mut call_args = Vec::with_capacity(args.len() + 1);
    call_args.push(ctx);
    for (&arg, &kind) in args.iter().zip(&abi.params[1..]) {
        let want = abi_type(kind, pointer);
        let have = builder.func.dfg.value_type(arg);
        call_args.push(if have == want {
            arg
        } else if have.bits() > want.bits() {
            builder.ins().ireduce(want, arg)
        } else {
            builder.ins().uextend(want, arg)
        });
    }
    let call = builder.ins().call(fr, &call_args);
    // A void wrapper has no result; callers of those ignore the value, so hand
    // back the context pointer rather than complicating every call site.
    Ok(match abi.ret {
        AbiRet::Void => ctx,
        _ => builder.func.dfg.first_result(call),
    })
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

/// Build (and cache) a `'static RecordSchema` for record def `id`, returning
/// its address as a raw pointer the JIT embeds as an immediate. The schema is
/// `Box::leak`'d once per def-id (mirroring how text literals are leaked); the
/// field descriptors are resolved from the runtime's scalar/collection
/// descriptor table via a best-effort mapping (M7: scalar fields only; nested
/// records/collections default to the INT descriptor, which is sound for GC
/// tracing since every value is a GcRef).
fn record_schema_for(
    db: &praxis_types::TypeDb,
    id: u32,
) -> *const praxis_runtime::records::RecordSchema {
    use praxis_runtime::records::{RecordField, RecordSchema};
    use praxis_types::data::RecordDefId;
    use std::sync::Mutex;
    // A process-wide cache: def-id → leaked schema pointer. The schema is
    // immutable and 'static once built, so caching across compiles is sound.
    // The raw pointer is wrapped (SendPtr) so the Mutex can be shared across
    // threads (inference/compilation are single-threaded today, but the Mutex
    // satisfies the OnceLock's Sync requirement).
    struct SendPtr(*const RecordSchema);
    unsafe impl Send for SendPtr {}
    static CACHE: std::sync::OnceLock<Mutex<HashMap<u32, SendPtr>>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(p) = cache.lock().unwrap().get(&id) {
        return p.0;
    }
    let def = db.record_def(RecordDefId(id));
    let fields: Vec<RecordField> = def
        .fields
        .iter()
        .map(|f| RecordField {
            name: Box::leak(f.name.clone().into_boxed_str()),
            descriptor: descriptor_for_type(db, f.ty),
        })
        .collect();
    let leaked_fields: &'static [RecordField] = Box::leak(fields.into_boxed_slice());
    let schema = Box::leak(Box::new(RecordSchema {
        fields: leaked_fields,
    }));
    let ptr = SendPtr(schema as *const RecordSchema);
    let raw = ptr.0;
    cache.lock().unwrap().insert(id, ptr);
    raw
}

/// Build (and cache) a `'static TupleSchema` for the tuple type `ty`, returning
/// its address as a raw pointer the JIT embeds as an immediate. The schema is
/// `Box::leak`'d once per distinct tuple shape (mirroring `record_schema_for`).
///
/// The cache is keyed by the **resolved element-descriptor sequence**, not by
/// the static `Type` id: the type arena does not structurally intern tuples
/// (each `db.tuple(...)` call mints a fresh slot), so two `(Int, Int)` literals
/// get different `Type` ids but the same shape. Keying on the descriptor
/// pointers gives true structural de-duplication, so two same-shaped tuples
/// share one schema and compare structurally equal at runtime.
fn tuple_schema_for(
    db: &praxis_types::TypeDb,
    ty: praxis_types::Type,
) -> *const praxis_runtime::tuples::TupleSchema {
    use praxis_runtime::tuples::TupleSchema;
    use praxis_types::data::TypeData;
    use std::sync::Mutex;
    // Resolve the element types. A non-tuple type here is a misuse (the HIR
    // only lowers `TypedExpr::Tuple` here), but degrade defensively by
    // treating it as a zero-element tuple rather than panicking in the JIT.
    let element_types: Vec<praxis_types::Type> = match db.data(db.follow(ty)) {
        TypeData::Tuple(els) => els.clone(),
        _ => Vec::new(),
    };
    let descriptors: Vec<*const praxis_runtime::descriptor::TypeDescriptor> = element_types
        .iter()
        .map(|t| descriptor_for_type(db, *t))
        .collect();
    // A process-wide cache keyed by the descriptor sequence (structural shape).
    // SendPtr wraps the raw pointer so the Mutex can be shared across threads.
    struct SendPtr(*const TupleSchema);
    unsafe impl Send for SendPtr {}
    static CACHE: std::sync::OnceLock<Mutex<HashMap<Vec<usize>, SendPtr>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    // Use the descriptor pointers as addresses for the cache key — structurally
    // identical tuples resolve to the same descriptor pointers.
    let key: Vec<usize> = descriptors.iter().map(|p| *p as usize).collect();
    if let Some(p) = cache.lock().unwrap().get(&key) {
        return p.0;
    }
    let leaked_descriptors: &'static [*const praxis_runtime::descriptor::TypeDescriptor] =
        Box::leak(descriptors.into_boxed_slice());
    let schema = Box::leak(Box::new(TupleSchema {
        descriptors: leaked_descriptors,
    }));
    let ptr = SendPtr(schema as *const TupleSchema);
    let raw = ptr.0;
    cache.lock().unwrap().insert(key, ptr);
    raw
}

/// Build the `&'static [DebugLocalMeta]` for a function's `Gc` locals, in the
/// same order as the `gc_slot` map iterates them (so a local's shadow-slot
/// index doubles as its debug-local index). Each entry carries the source name
/// (embedded as `&'static str`, empty for temps), a per-local symbol-id
/// placeholder, the static type descriptor resolved from the MIR local's `Type`
/// (§9.3, M10-WS2), the user-vs-temp classification, and the source span.
///
/// The symbol id is a best-effort placeholder: MIR locals do not yet carry the
/// HIR `SymbolId`, so we use the local's position. This is sufficient for the
/// crash debugger to *display* locals; full shadow-disambiguation by real
/// symbol id is an M10b refinement once MIR threads the id.
///
/// Temps no longer get the old `"<tmp>"` name placeholder: the debugger now
/// classifies them structurally via `kind` and renders them as
/// `<tmp#N: Type> @ "expr"` using the symbol id and span threaded here.
fn build_debug_local_metas(
    mir: &MirFunction,
    db: &mut praxis_types::TypeDb,
) -> &'static [DebugLocalMeta] {
    use praxis_mir::ir::LocalDebugKind;
    let mut metas: Vec<DebugLocalMeta> = Vec::new();
    let mut symbol_id = 0u32;
    for local in &mir.locals {
        if local.kind != LocalKind::Gc {
            continue;
        }
        // The source name. User locals carry their written name; temps carry an
        // empty name (the debugger names them `<tmp#N>` via the symbol id).
        let name: &'static str = mir
            .debug_name(local.id)
            .map(|n| Box::leak(n.to_string().into_boxed_str()) as &'static str)
            .unwrap_or("");
        // Deep-resolve the local's type before capturing its id, so the id
        // points at a fully-concrete type (e.g. Vec[Int], not Vec[?T]). The
        // element/param vars of a composite are left untouched by `follow`
        // (top-level only); `deep_resolve` recurses and interns a resolved
        // copy. Idempotent on already-resolved types. (M10b-WS4)
        let resolved_ty = db.deep_resolve(local.ty);
        // Thread the user-vs-temp classification and source span from the MIR.
        let kind = match mir.debug_kind(local.id) {
            LocalDebugKind::User => praxis_runtime::LOCAL_KIND_USER,
            LocalDebugKind::Temp => praxis_runtime::LOCAL_KIND_TEMP,
        };
        let (span_start, span_end) = mir.debug_span(local.id).unwrap_or((0, 0));
        metas.push(DebugLocalMeta {
            source_name: name.as_ptr(),
            name_len: name.len() as u32,
            symbol_id,
            descriptor: descriptor_for_type(db, local.ty),
            // The full static `Type` id (M10-WS1b): `Type(u32)`. Lets the
            // debugger reconstruct the exact local type (incl. collection
            // element types / record shapes) the runtime `descriptor` loses.
            type_id: resolved_ty.0,
            kind,
            span_start,
            span_end,
        });
        symbol_id += 1;
    }
    Box::leak(metas.into_boxed_slice())
}

/// Leak a `&str` into `&'static str` for embedding in a runtime call (the
/// runtime reads it by raw pointer). Used by the prologue to pass the function
/// name to `praxis_push_debug_frame` (M10-WS2). The caller turns the result
/// into `(ptr, len)` iconsts through its own builder. Lighter than
/// [`embed_text`]: no GC `Text` allocation, just a process-static string.
fn leak_static_str(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// Best-effort mapping from a static `Type` to its runtime `TypeDescriptor`.
///
/// Scalars map to their descriptor; collections map to their single static
/// collection descriptor (the per-instance element type lives in the payload,
/// §11.2, so one descriptor serves all `Vec[T]`); records/enums/tuples map to
/// their top-level descriptor (field descriptors are resolved recursively here
/// and embedded into the payload at construction). Everything else defaults to
/// `INT` — sound for GC tracing because every value is a uniform `GcRef`, and
/// the descriptor's `trace` callback is only called on the top-level object, not
/// per-field.
///
/// **M8 WS1:** the `Collection` arm is what closes the M7 `Vec[T]()` null-
/// descriptor carryover. `Vec[Vec[Int]]` now resolves the outer descriptor to
/// `VEC` and (via the recursive call used by `collection_element_descriptor_for`)
/// the inner element descriptor to `VEC` as well, so structural equality/hashing
/// of nested collections dispatch correctly. The collection descriptor itself
/// is a process-static const; only the *element* descriptor (passed to
/// `praxis_<kind>_new` at construction) needs resolving per element type, and
/// that resolution is cached by `collection_element_descriptor_for`.
fn descriptor_for_type(
    db: &praxis_types::TypeDb,
    ty: praxis_types::Type,
) -> *const praxis_runtime::descriptor::TypeDescriptor {
    use praxis_runtime::descriptor::TypeDescriptor;
    use praxis_types::data::TypeData;
    use praxis_types::CollectionCtor;
    match db.data(db.follow(ty)) {
        TypeData::Scalar(s) => match s {
            praxis_types::ScalarType::Int | praxis_types::ScalarType::Never => {
                &praxis_runtime::scalars::INT as *const TypeDescriptor
            }
            praxis_types::ScalarType::Bool => {
                &praxis_runtime::scalars::BOOL as *const TypeDescriptor
            }
            praxis_types::ScalarType::Text => &praxis_runtime::text::TEXT as *const TypeDescriptor,
            praxis_types::ScalarType::Char => {
                &praxis_runtime::scalars::CHAR as *const TypeDescriptor
            }
            _ => &praxis_runtime::scalars::INT as *const TypeDescriptor,
        },
        // Tuples resolve to the TUPLE descriptor (M7 Part 2). Records/enums use
        // a single top-level descriptor per value (RECORD/ENUM), but their field
        // descriptors are resolved here; a nested record's field descriptor
        // defaulting to INT is sound for GC tracing since every value is a GcRef.
        TypeData::Tuple(_) => &praxis_runtime::tuples::TUPLE as *const TypeDescriptor,
        // Collections resolve to their single static descriptor const. The
        // per-instance element type lives in the payload (§11.2), so `VEC` serves
        // all `Vec[T]`, `MAP` all `Map[K,V]`, etc. The element descriptor is
        // resolved separately at construction via `collection_element_descriptor_for`.
        TypeData::Collection { ctor, .. } => match ctor {
            CollectionCtor::Vec => &praxis_runtime::collections::VEC as *const TypeDescriptor,
            CollectionCtor::Grid => &praxis_runtime::collections::GRID as *const TypeDescriptor,
            CollectionCtor::Deque => &praxis_runtime::collections::DEQUE as *const TypeDescriptor,
            CollectionCtor::Map => &praxis_runtime::maps::MAP as *const TypeDescriptor,
            CollectionCtor::Set => &praxis_runtime::maps::SET as *const TypeDescriptor,
            CollectionCtor::Counter => &praxis_runtime::maps::COUNTER as *const TypeDescriptor,
            CollectionCtor::MinHeap => &praxis_runtime::heaps::MIN_HEAP as *const TypeDescriptor,
            CollectionCtor::MaxHeap => &praxis_runtime::heaps::MAX_HEAP as *const TypeDescriptor,
            CollectionCtor::BitSet => &praxis_runtime::bitset::BITSET as *const TypeDescriptor,
            // Other collection ctors (MinHeap/MaxHeap/BitSet/Range/Seq) land in
            // their own workstreams and will add arms here. Until then they fall
            // through to INT — sound for GC tracing only; these types cannot yet
            // be constructed, so the arm is unreachable.
            _ => &praxis_runtime::scalars::INT as *const TypeDescriptor,
        },
        _ => &praxis_runtime::scalars::INT as *const TypeDescriptor,
    }
}

/// Resolve the element descriptor(s) for a collection's payload, for use at
/// construction (`praxis_<kind>_new`). Most collections carry one element
/// descriptor (Vec/Deque/Set/Heap/Grid); `Map[K,V]` and `Counter[T]` carry a
/// key descriptor (and Map also a value descriptor, passed as a second slot).
///
/// The descriptor is resolved recursively via [`descriptor_for_type`] so nested
/// collections (e.g. `Map[Vec[Int], Int]`) resolve the key descriptor to `VEC`,
/// making structural equality/hashing dispatch correctly on map keys (§11.3).
/// Cached by the element type's resolved descriptor pointer: two same-shaped
/// element types share one descriptor pointer, avoiding re-leak per call site
/// (mirrors the `tuple_schema_for` caching idiom, lower.rs:1047).
fn collection_element_descriptor_for(
    db: &praxis_types::TypeDb,
    element_type: praxis_types::Type,
) -> *const praxis_runtime::descriptor::TypeDescriptor {
    descriptor_for_type(db, element_type)
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

/// Memory flags for an in-register bit-cast between the uniform i64 scalar
/// channel and f64. Cranelift's `bitcast` instruction only accepts flags that
/// are *exactly* `new()` or `new()` plus a byte-order (big/little) flag — any
/// extra bits (notrap, aligned, …) are rejected by the verifier. Little-endian
/// matches every supported Praxis host.
fn bitcast_flags() -> MemFlags {
    MemFlags::new().with_endianness(cranelift::codegen::ir::Endianness::Little)
}

/// Reinterpret an `i64` scalar (an f64 bit pattern) as an `f64` value (§4.12).
fn i64_to_f64(builder: &mut FunctionBuilder, v: Value) -> Value {
    builder.ins().bitcast(types::F64, bitcast_flags(), v)
}

/// Reinterpret an `f64` value as its `i64` bit pattern (§4.12).
fn f64_to_i64(builder: &mut FunctionBuilder, v: Value) -> Value {
    builder.ins().bitcast(GC, bitcast_flags(), v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_jit::{JITBuilder, JITModule};

    fn test_module() -> JITModule {
        let builder = JITBuilder::new(cranelift_module::default_libcall_names())
            .expect("host target is supported");
        JITModule::new(builder)
    }

    /// P0-13: the signature comes from the manifest row, not from the argument
    /// count. The arity-derived path this replaces gave every parameter the
    /// pointer type, so `praxis_record_field`'s `u32` index received a full
    /// 64-bit value across a C ABI that declares 32 bits.
    #[test]
    fn narrow_parameters_are_declared_narrow() {
        let module = test_module();
        let sig = signature_for(RuntimeSymbol::RecordField, &module);
        let pointer = module.target_config().pointer_type();
        assert_eq!(
            sig.params.iter().map(|p| p.value_type).collect::<Vec<_>>(),
            vec![pointer, pointer, types::I32],
            "praxis_record_field(ctx, record, idx: u32)"
        );
        assert_eq!(sig.returns.len(), 1);
    }

    /// A wrapper that returns nothing must declare no results. The arity-only
    /// synthesis gave every symbol an `i64` return, so a call to
    /// `praxis_pop_shadow_frame` read a result register the callee never wrote.
    #[test]
    fn void_wrappers_declare_no_result() {
        let module = test_module();
        assert!(signature_for(RuntimeSymbol::PopShadowFrame, &module)
            .returns
            .is_empty());
        assert!(signature_for(RuntimeSymbol::SetFrameSourceSpan, &module)
            .returns
            .is_empty());
        assert!(signature_for(RuntimeSymbol::SnapshotDebugChain, &module)
            .returns
            .is_empty());
    }

    /// Call convention and pointer width are the ISA's, not literals. Reading
    /// them from the module is what lets `Jit::check_target` be the single
    /// place a wrong target is rejected.
    #[test]
    fn signatures_take_their_conventions_from_the_isa() {
        let module = test_module();
        let sig = signature_for(RuntimeSymbol::AllocInt, &module);
        assert_eq!(sig.call_conv, module.isa().default_call_conv());
        assert_eq!(
            sig.params[0].value_type,
            module.target_config().pointer_type()
        );
    }

    /// Every symbol in the manifest produces a well-formed signature, and its
    /// parameter count matches the row. This is the standing check that the
    /// manifest and the backend cannot disagree about shape.
    #[test]
    fn every_symbol_has_a_derivable_signature() {
        let module = test_module();
        for &sym in RuntimeSymbol::ALL {
            let sig = signature_for(sym, &module);
            assert_eq!(sig.params.len(), sym.sig().params.len(), "{sym}");
            let want_result = !matches!(sym.sig().ret, AbiRet::Void);
            assert_eq!(sig.returns.len(), usize::from(want_result), "{sym}");
        }
    }
}
