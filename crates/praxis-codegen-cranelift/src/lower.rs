//! MIR → Cranelift IR lowering for one function (§13.5, ADR-015).
//!
//! Each MIR [`Local`](praxis_mir::Local) becomes a Cranelift [`Variable`];
//! Cranelift turns the slot-based CFG into SSA automatically. Every language
//! value (`GcRef`) and scalar payload is carried as a Cranelift `i64` — `GcRef`
//! is pointer-sized and opaque to generated code, and `Int`/`Bool` payloads are
//! `i64`. Operations needing a real GC object (arithmetic, allocation) call the
//! `praxis_*` runtime wrappers, which allocate and fault-check (§10.4).

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use cranelift::codegen::ir::condcodes::IntCC;
use cranelift::codegen::ir::FuncRef;
use cranelift::codegen::ir::MemFlagsData as MemFlags;
use cranelift::codegen::isa::{CallConv, TargetFrontendConfig};
use cranelift::prelude::*;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{FuncId, Linkage, Module};
use praxis_mir::{
    AllocKind, CallTarget, CmpOp, DebugSlots, FloatBinOp, Function as MirFunction, GcConst, Inst,
    IntBinOp, LocalId, LocalKind, MirType, Overflow, RootSlots, Terminator,
};
use praxis_runtime::{
    DebugLocalMeta, RuntimeContext, ShadowStackHeader, SlotCount, MAX_SHADOW_SLOTS,
};
use praxis_stdlib::abi::{AbiKind, AbiRet, RuntimeSymbol};

use crate::generation::Generation;

/// The uniform Cranelift type for a `GcRef` and every scalar payload: `i64`.
/// `GcRef` is `#[repr(transparent)]` over a pointer; `Int`/`Bool` payloads are
/// `i64`/`bool`. `i64` carries both faithfully on a 64-bit host.
const GC: types::Type = types::I64;

/// The byte offset of the shadow-stack header pointer within a
/// `RuntimeContext`. The prologue loads the header from here to bump-allocate
/// this frame's slots (ADR-101). Computed from the `#[repr(C)]` layout so it
/// stays correct if the struct evolves (and the ABI version check catches a
/// drift that matters).
const SHADOW_OFFSET: i64 = core::mem::offset_of!(RuntimeContext, shadow) as i64;

/// The byte offset of `top` within a slot-stack header. The whole prologue and
/// epilogue are a load and two stores against this displacement.
///
/// Read off the shadow instantiation, but it is the same for every one: a
/// `SlotStackHeader<T>` is three pointers with `top` first, whatever `T` is.
const SLOT_STACK_TOP_OFFSET: i32 = ShadowStackHeader::TOP_OFFSET;

/// The width of one shadow slot. Derived rather than written as `8` because it
/// is the stride the spill indexes by and the multiplier the prologue's bump
/// uses, and those two disagreeing would be a silent miscompile of every rooted
/// local in the language.
const SHADOW_SLOT_BYTES: i64 = core::mem::size_of::<*mut praxis_runtime::GcHeader>() as i64;

/// Zero more than this many slots with a `memset` rather than a run of stores.
/// Below it the call and its argument setup cost more than the stores; above
/// it — a function with dozens of live `Gc` locals — the stores are both slower
/// and a kilobyte of prologue.
///
/// Deliberately not `FunctionBuilder::emit_small_memset`, whose threshold is 4:
/// that would put a libc call in the prologue of any function with five `Gc`
/// locals, which is most of them, and the point of ADR-101 is that the common
/// prologue makes no calls at all.
const SLOT_ZERO_UNROLL_MAX: u32 = 32;

/// The byte offset of `recursion_depth` within a `RuntimeContext`. The prologue
/// guard reads it — *before* it pushes anything — to decide whether to branch to
/// the stack-overflow fault epilogue (§9.2, §17.4). Computed from the
/// `#[repr(C)]` layout, like `SHADOW_OFFSET`.
const RECURSION_DEPTH_OFFSET: i64 = core::mem::offset_of!(RuntimeContext, recursion_depth) as i64;

/// The byte offset of `unit_ref` within a `RuntimeContext`. A fault epilogue
/// loads the immortal Unit from here and returns it, because the ABI says a
/// Praxis function returns a valid `GcRef` — including when it unwinds.
const UNIT_REF_OFFSET: i64 = core::mem::offset_of!(RuntimeContext, unit_ref) as i64;

/// The byte offset of `small_ints` within a `RuntimeContext`: the base of the
/// interned small-`Int` table (`praxis_runtime::small_int`). `Inst::ConstGc`
/// loads it and then loads the element at a compile-time offset — two loads,
/// where an in-range `Int` literal used to be a call to `praxis_alloc_int`
/// preceded by a shadow-frame spill (docs/handovers/21-where-the-time-goes.md
/// §3.5). Computed from the `#[repr(C)]` layout, like `SLOTS_OFFSET`.
const SMALL_INTS_OFFSET: i64 = core::mem::offset_of!(RuntimeContext, small_ints) as i64;

/// The byte offsets of the two cached `Bool` immortals within a
/// `RuntimeContext`. A `Bool` literal is one load of one of these — the same
/// object `praxis_alloc_bool` would have answered, without the call.
const TRUE_REF_OFFSET: i64 = core::mem::offset_of!(RuntimeContext, true_ref) as i64;
const FALSE_REF_OFFSET: i64 = core::mem::offset_of!(RuntimeContext, false_ref) as i64;

/// The byte offset of `tag` within an `EnumPayload`. `match` reads it directly
/// rather than through a wrapper call, so it is baked into emitted code — and
/// it was baked in as a literal `0` until the payload gained a leading schema
/// pointer (RT-13) and moved the tag to offset 8. Computed from the `#[repr(C)]`
/// layout, like `SHADOW_OFFSET`, so the next reorder is a compile-time-derived
/// constant rather than a silent miscompile of every enum in the language.
const ENUM_TAG_OFFSET: i32 = core::mem::offset_of!(praxis_runtime::enums::EnumPayload, tag) as i32;

/// The byte offset of the descriptor pointer within a `GcHeader`.
/// `Inst::ExtractScalar` loads it and compares it against the scalar
/// descriptor's address before reading the payload inline (ADR-102).
///
/// Exported from `gc.rs` rather than reached for with `offset_of!` here: the
/// header's fields are private to that module by ADR-039 decision 1, and the
/// point of that decision is that the layout has one authority. Writing `0`
/// here would be the re-derived literal it exists to prevent.
const GC_DESCRIPTOR_OFFSET: i32 = praxis_runtime::GcHeader::DESCRIPTOR_OFFSET as i32;

/// The byte offset of `pending_fault` within a `RuntimeContext`, and of `kind`
/// within the `Fault` it points at. `Inst::CheckFault` is a load through each
/// and a branch, where it used to be a call to `praxis_check_fault` (ADR-102).
const PENDING_FAULT_OFFSET: i32 = core::mem::offset_of!(RuntimeContext, pending_fault) as i32;
const FAULT_KIND_OFFSET: i32 = praxis_runtime::Fault::KIND_OFFSET as i32;

/// The width the fault check loads at, asserted against the runtime's own
/// answer. A `#[repr(C)]` fieldless enum is the target's C `int` on every target
/// `Jit::check_target` accepts — but *assuming* that is how a repr change
/// becomes a silent three-bytes-of-something-else read instead of a build
/// failure. This is the build failure.
const _: () = assert!(
    praxis_runtime::Fault::KIND_SIZE == 4,
    "the inline fault check loads `Fault.kind` as an I32"
);

/// Lower one MIR function into a Cranelift function and define it in `module`.
pub(crate) fn lower_function<M: Module>(
    module: &mut M,
    fn_ctx: &mut FunctionBuilderContext,
    mir: &MirFunction,
    user_funcs: &HashMap<String, FuncId>,
    db: &mut praxis_types::TypeDb,
    generation: &Generation,
) -> Result<()> {
    // The ISA's pointer width and default call convention. Needed by the
    // prologue's slot-zeroing memset and by `finalize` at the end, and taken
    // here because both points hold a borrow that excludes `module`.
    let frontend_config = module.isa().frontend_config();
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
    // A frame wider than the shadow stack can hold is rejected here, and this
    // is the only place it can be: `SlotCount` is unconstructible above the cap,
    // and every consumer downstream — including the reservation-sizing argument
    // in `SHADOW_STACK_SLOTS` — assumes the bound rather than re-checking it.
    let slot_count = SlotCount::new(gc_count).ok_or_else(|| {
        anyhow!(
            "function `{}` has {gc_count} Gc locals, exceeding MAX_SHADOW_SLOTS \
             ({MAX_SHADOW_SLOTS})",
            mir.name
        )
    })?;

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

    let mut import_cache: HashMap<RuntimeSymbol, FuncRef> = HashMap::new();
    let mut user_func_cache: HashMap<String, FuncRef> = HashMap::new();

    // Recursion-depth guard (§9.2, §17.4), and it comes *first*.
    //
    // It used to sit after the shadow-frame push, because the push helper was
    // what bumped `ctx.recursion_depth` and the guard read the result back.
    // With the bump inline there is no reason to push before deciding, and two
    // reasons not to: the over-limit path then pushes nothing, so it pops
    // nothing (the third `emit_pop_shadow_frame` call site is gone rather than
    // rewritten), and "every prologue guards before it pushes" is the premise
    // that lets `SHADOW_STACK_SLOTS` be sized so shadow-stack exhaustion is
    // unrepresentable. Observable behaviour is unchanged — bodies still run at
    // nesting levels 1..MAX_RECURSION_DEPTH and the call past it still faults
    // `StackOverflow` — with one fewer frame ever pushed.
    //
    // Without the guard, deep recursion (e.g. `count(100000)`) overflows the
    // native stack and the host aborts (SIGABRT); with it, the call faults
    // cleanly and unwinds to the host like any other fault.
    //
    // Block 0's actual instructions run in `body_entry` (a fresh block), so the
    // `entry` block ends with this conditional branch.
    let body_entry = builder.create_block();
    let over_limit = builder.create_block();
    // The depth this call found, saved so the epilogue can restore it rather
    // than decrement. `entry` dominates every block, so this is defined
    // everywhere the epilogues can run.
    let saved_depth_var = builder.declare_var(types::I32);
    {
        // Load `(*ctx).recursion_depth` (u32) at its fixed `#[repr(C)]` offset.
        // `MemFlags::trusted()` is aligned + notrap: the context is live for the
        // whole call and the offset is in-bounds by construction.
        let depth = builder.ins().load(
            types::I32,
            MemFlags::trusted(),
            ctx_val,
            RECURSION_DEPTH_OFFSET as i32,
        );
        builder.def_var(saved_depth_var, depth);
        // Unsigned, and `>=` rather than `>`: the saturating add that used to
        // keep the counter non-negative lives in a helper that no longer
        // exists, and guarding before the bump means this frame is the
        // (depth+1)-th, so `depth == MAX` is already one too many.
        let over = builder.ins().icmp_imm_u(
            IntCC::UnsignedGreaterThanOrEqual,
            depth,
            i64::from(praxis_runtime::MAX_RECURSION_DEPTH),
        );
        builder.ins().brif(over, over_limit, &[], body_entry, &[]);
    }

    // The stack-overflow fault epilogue: raise the fault, snapshot, and return
    // the Unit sentinel. Mirrors `Terminator::Fault` below, minus the pops —
    // guard-first means this path pushed neither the shadow frame nor the debug
    // frame, so this is the one `return_` in the function that is not preceded
    // by an epilogue, and it is exactly the one path that skipped the prologue.
    //
    // The snapshot taken here now reflects the caller's chain (at depth
    // MAX_RECURSION_DEPTH) rather than the overflowing frame's — one frame
    // shallower than before, same fault, same kind.
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
        let unit = load_unit_sentinel(&mut builder, ctx_val);
        builder.ins().return_(&[unit]);
    }

    // Everything below is the prologue proper, and it runs only on the path the
    // guard let through. Block 0's body follows it in the same block.
    builder.switch_to_block(body_entry);

    // Prologue: claim this call's depth. Storing `depth + 1` rather than
    // incrementing in place is the same load the guard already did, reused.
    {
        let depth = builder.use_var(saved_depth_var);
        #[allow(deprecated)] // iadd_imm_s vs iadd_imm: the immediate is 1.
        let deeper = builder.ins().iadd_imm_s(depth, 1);
        builder.ins().store(
            MemFlags::trusted(),
            deeper,
            ctx_val,
            RECURSION_DEPTH_OFFSET as i32,
        );
    }

    // Prologue (cont.): claim this frame's run of shadow slots (ADR-101). The
    // run is the root set the collector scans during the automatic GC that
    // `praxis_alloc_*` wrappers trigger (§12.4, ADR-019). `frame_var` holds its
    // base, which is both what the spill indexes and what the epilogue stores
    // back as the stack's `top`.
    //
    // Emitted unconditionally, including when this function has no `Gc` locals.
    // `spill_roots` calls `use_var(frame_var)` whenever the root set is
    // non-empty, and a root set can be non-empty while `slot_of` yields nothing
    // (a `Scalar` local in the set — see the `continue` there), so a
    // `gc_count == 0` special case would leave `frame_var` undefined on a path
    // that reads it and panic Cranelift. One code path is worth two wasted
    // instructions in a case that essentially does not occur.
    let frame_var = builder.declare_var(GC);
    {
        let base = emit_slot_stack_push(
            &mut builder,
            ctx_val,
            SHADOW_OFFSET,
            slot_count,
            SHADOW_SLOT_BYTES,
            frontend_config,
        );
        builder.def_var(frame_var, base);
    }

    // Prologue (cont.): push a debug frame and keep its pointer in a Variable
    // (§9.3, ADR-021, M10-WS2). This frame is what the crash debugger reads for
    // `bt`/`locals`; the spill below keeps each `DebugLocal.value` fresh across
    // safepoints, parallel to the shadow-frame slots. The frame carries one
    // `DebugLocalMeta` per Gc local — in the same order as `gc_slot`, so a
    // local's shadow slot index doubles as its debug-local index.
    let debug_frame_var = builder.declare_var(GC);
    let debug_frame_ptr = {
        // Build the `[DebugLocalMeta]` for this function's Gc locals in the
        // generation arena. Each entry carries the source name (interned in the
        // same arena), a per-local symbol id placeholder, and the static type
        // descriptor resolved from the MIR local's Type.
        let (metas_ptr, _metas_len) = build_debug_local_metas(mir, db, generation);
        let meta_ptr_val = builder.ins().iconst(GC, metas_ptr as i64);
        // Embed the function name (ptr + len) for the frame, interned so the
        // same function lowered twice into one generation costs one copy.
        let name_static = generation.alloc_str(&mir.name);
        let name_ptr_val = builder.ins().iconst(GC, name_static.as_ptr() as i64);
        let name_len_val = builder.ins().iconst(GC, name_static.len() as i64);
        let count_val = builder.ins().iconst(GC, gc_count as i64);
        call_symbol(
            &mut builder,
            ctx_val,
            &[name_ptr_val, name_len_val, count_val, meta_ptr_val],
            RuntimeSymbol::PushDebugFrame,
            module,
            &mut import_cache,
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
            &mut import_cache,
        )?;
    }

    let spill = SpillCtx {
        frame_var,
        saved_depth_var,
        debug_frame_var,
        slot_of: &gc_slot,
    };

    // Lower each block. Blocks are sealed together after the whole CFG is built
    // so loop backedges resolve correctly. Block 0's body continues in
    // `body_entry` (the recursion guard's fall-through target, which the
    // prologue above is already filling), not the param-receiving `entry` block.
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
                generation,
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
    builder.finalize(frontend_config);

    // Resolve our FuncId (declared in the first pass) and define the function.
    let id = func_id_for(module, &mir.name)?;
    module.define_function(id, &mut ctx)?;
    module.clear_context(&mut ctx);
    Ok(())
}

// ---------------------------------------------------------------------------
// Slot stacks (ADR-101).
//
// A slot stack is a contiguous, fixed-capacity region the runtime owns, with a
// `{ top, base, limit }` header reachable from the `RuntimeContext`. A frame is
// a run of slots inside it, claimed by bumping `top` and released by restoring
// the base the claim started from. The three functions below are the whole of
// the mechanism in generated code, and they are parameterised by the context
// field so a second stack — the crash debugger's per-frame locals — is one more
// field and the same two calls, not a second implementation.
// ---------------------------------------------------------------------------

/// The store/load displacement of slot `index` from a frame's base.
///
/// `MAX_SHADOW_SLOTS` slots of `SHADOW_SLOT_BYTES` is far inside `i32`, which
/// is what a Cranelift memory displacement is.
fn slot_displacement(index: u32) -> i32 {
    (i64::from(index) * SHADOW_SLOT_BYTES) as i32
}

/// Claim `count` zeroed slots from the slot stack whose header pointer lives at
/// `ctx + ctx_field_offset`, and answer this frame's base.
///
/// Four instructions and no call: load the header, load `top`, zero the claimed
/// run, store the bumped `top`. There is no bounds check because there is
/// nothing to check — see `SHADOW_STACK_SLOTS`, which is sized from the
/// recursion limit the prologue has already enforced by the time this runs.
///
/// The returned base is the caller's to keep for the whole call: it is both the
/// address the spill indexes and the value [`emit_slot_stack_pop`] stores back.
/// Hold it in a `Variable` — it cannot be recovered by re-reading `top`, since
/// every callee's own push and pop will have moved `top` and put it back.
fn emit_slot_stack_push(
    builder: &mut FunctionBuilder,
    ctx_val: Value,
    ctx_field_offset: i64,
    count: SlotCount,
    slot_bytes: i64,
    cfg: TargetFrontendConfig,
) -> Value {
    // Aligned + non-trapping: the header and the reservation are owned by the
    // runtime and live for as long as the context is.
    let flags = MemFlags::trusted();
    let header = builder
        .ins()
        .load(GC, flags, ctx_val, ctx_field_offset as i32);
    let base = builder.ins().load(GC, flags, header, SLOT_STACK_TOP_OFFSET);
    emit_zero_slots(builder, base, count, slot_bytes, cfg);
    #[allow(deprecated)] // iadd_imm_s vs iadd_imm: a small positive immediate.
    let new_top = builder
        .ins()
        .iadd_imm_s(base, i64::from(count.get()) * slot_bytes);
    builder
        .ins()
        .store(flags, new_top, header, SLOT_STACK_TOP_OFFSET);
    base
}

/// Release the frame [`emit_slot_stack_push`] claimed, by restoring the `top`
/// it started from.
///
/// Restoring the saved absolute rather than subtracting the frame's width is
/// deliberate: it cannot underflow (the extern pop this replaced needed a
/// `saturating_sub` for exactly that reason) and it is self-healing — an
/// imbalance introduced by anything that ran inside this frame is corrected
/// here rather than propagated to the caller.
///
/// The header is re-read from the context rather than carried from the push.
/// Carrying it would keep a second value live across the whole body, and at
/// `opt_level = "none"` a value live across a call is a native stack slot in
/// every frame — which is the budget deep recursion actually runs out of. The
/// reload is an L1 hit off a pointer that is live regardless.
fn emit_slot_stack_pop(
    builder: &mut FunctionBuilder,
    ctx_val: Value,
    ctx_field_offset: i64,
    base: Value,
) {
    let flags = MemFlags::trusted();
    let header = builder
        .ins()
        .load(GC, flags, ctx_val, ctx_field_offset as i32);
    builder
        .ins()
        .store(flags, base, header, SLOT_STACK_TOP_OFFSET);
}

/// Write `count` zero slots starting at `base`.
///
/// The zeroing is what makes a claimed slot mean "not yet written": the
/// collector scans every slot below `top`, and a slot the function has not
/// spilled into yet would otherwise hold whatever a previous, deeper call left
/// there — a pointer to an object that may since have been swept and its
/// storage reused (RT-01). Only the claimed run is zeroed, which is the whole
/// of finding §3.3: ADR-019 zeroed all `MAX_SHADOW_SLOTS` on every call.
fn emit_zero_slots(
    builder: &mut FunctionBuilder,
    base: Value,
    count: SlotCount,
    slot_bytes: i64,
    cfg: TargetFrontendConfig,
) {
    let n = count.get();
    if n == 0 {
        return;
    }
    if n <= SLOT_ZERO_UNROLL_MAX {
        let zero = builder.ins().iconst(GC, 0);
        for i in 0..n {
            builder.ins().store(
                MemFlags::trusted(),
                zero,
                base,
                (i64::from(i) * slot_bytes) as i32,
            );
        }
        return;
    }
    let ch = builder.ins().iconst(types::I8, 0);
    let size = builder
        .ins()
        .iconst(cfg.pointer_type(), i64::from(n) * slot_bytes);
    builder.call_memset(cfg, base, ch, size);
}

/// The spill context handed to every instruction/terminator lowering: the
/// Variables the prologue defined, and the Gc-local → slot-index map.
///
/// **Two spills, not one** (MIR-16). There used to be a single `emit_spill`
/// writing one root list into both frames, which is why the two frames could
/// not disagree — and why making the GC root set exact would have silently
/// emptied the debugger's view. [`SpillCtx::spill_roots`] serves the collector
/// and takes the exact [`RootSlots`]; [`SpillCtx::spill_debug`] serves the
/// crash debugger and takes the over-approximate [`DebugSlots`].
struct SpillCtx<'a> {
    /// The base of this frame's run of slots inside the one contiguous shadow
    /// stack (ADR-101) — not a frame object. The spill indexes it directly, and
    /// the epilogue stores it back as the stack's `top`.
    frame_var: Variable,
    /// `ctx.recursion_depth` as this call found it. The epilogue stores it back
    /// rather than decrementing.
    saved_depth_var: Variable,
    /// The debug frame pointer Variable (M10-WS2). Written only by
    /// [`SpillCtx::spill_debug`].
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
    /// The GC spill, emitted just before a safepoint (§12.3, ADR-019): write
    /// each live root's current value into `frame_base + slot_index*8`, and
    /// write **null** into every slot the liveness pass marked dead.
    ///
    /// One store per slot, with the index as the store's own displacement.
    /// Under ADR-019's frame objects this was an `iadd_imm_s` plus a store,
    /// because the slot array sat at a fixed offset *inside* the frame and the
    /// two displacements had to be summed at runtime — and at `opt_level =
    /// "none"` Cranelift does not fold that add into the store, so every
    /// spilled root cost an extra address computation. A frame's base is now the
    /// address of slot 0, so there is nothing to add.
    ///
    /// The null stores are MIR-01. A slot written at one safepoint and not live
    /// at the next used to keep its old value forever: the collector reads the
    /// whole frame, so a dead slot kept its object reachable for the rest of the
    /// call — and, once RT-01 made swept storage reusable, a slot could name a
    /// live object of an entirely different type.
    fn spill_roots(&self, builder: &mut FunctionBuilder, roots: &RootSlots, vars: &[Variable]) {
        if roots.live().is_empty() && roots.dead().is_empty() {
            return;
        }
        let frame_base = builder.use_var(self.frame_var);
        // Aligned + non-trapping: the frame's slots are live for the whole call
        // and in-bounds by construction.
        let flags = MemFlags::trusted();
        for &local in roots.live() {
            let Some(&slot) = self.slot_of.get(&local) else {
                continue; // a Scalar local in the root set; it has no slot.
            };
            let val = builder.use_var(vars[local.0 as usize]);
            builder
                .ins()
                .store(flags, val, frame_base, slot_displacement(slot));
        }
        if roots.dead().is_empty() {
            return;
        }
        let null = builder.ins().iconst(GC, 0);
        for &local in roots.dead() {
            let Some(&slot) = self.slot_of.get(&local) else {
                continue;
            };
            builder
                .ins()
                .store(flags, null, frame_base, slot_displacement(slot));
        }
    }

    /// The debugger spill (§9.3, M10-WS2): write each visible local's current
    /// value into `debug_frame.locals[slot_index].value`.
    ///
    /// Separate from [`SpillCtx::spill_roots`] and driven by a separate,
    /// deliberately over-approximate set. Nothing here is cleared: the slot's
    /// `Option<GcRef>` starts `None` and a value that has been produced stays
    /// renderable, which is what `locals` in the crash REPL is for.
    fn spill_debug(&self, builder: &mut FunctionBuilder, debug: &DebugSlots, vars: &[Variable]) {
        if debug.visible().is_empty() {
            return;
        }
        let debug_frame_ptr = builder.use_var(self.debug_frame_var);
        // debug_frame.locals is a *mut DebugLocal; slot i's DebugLocal is at
        // *(debug_frame.locals) + i*size, and `value` is at +DEBUG_VALUE_OFFSET
        // within it. Load the locals base pointer once, then index it.
        let locals_base = builder.ins().load(
            GC,
            MemFlags::trusted(),
            debug_frame_ptr,
            DEBUG_LOCALS_OFFSET as i32,
        );
        let mut flags = MemFlags::trusted();
        flags.set_notrap();
        for &local in debug.visible() {
            let Some(&slot) = self.slot_of.get(&local) else {
                continue;
            };
            let val = builder.use_var(vars[local.0 as usize]);
            let local_off = (slot as i64) * DEBUG_LOCAL_SIZE + DEBUG_VALUE_OFFSET;
            #[allow(deprecated)]
            let value_addr = builder.ins().iadd_imm_s(locals_base, local_off);
            // A non-null `GcRef` written into an `Option<GcRef>` slot *is*
            // `Some(v)`: the niche makes the two the same word (F18). The
            // all-zero word the frame starts with is `None`.
            builder.ins().store(flags, val, value_addr, 0);
        }
    }

    /// The pair emitted at a GC safepoint: the collector's exact root set and
    /// the debugger's over-approximate view of the same point.
    fn spill_safepoint(
        &self,
        builder: &mut FunctionBuilder,
        roots: &RootSlots,
        debug: &DebugSlots,
        vars: &[Variable],
    ) {
        self.spill_roots(builder, roots, vars);
        self.spill_debug(builder, debug, vars);
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
    db: &mut praxis_types::TypeDb,
    generation: &Generation,
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
        Inst::ConstGc { dst, konst } => {
            // **No `spill.spill_safepoint` here, and that is the point.** This
            // instruction reads a reference the runtime minted before `main`
            // ran; nothing it emits can collect, so there is no frame for the
            // collector to see and no fault for the debugger to divert on
            // (`Inst::fault_reason` answers `None`, and `liveness` gives it no
            // slot sets to spill).
            let v = load_gc_const(builder, ctx_val, *konst);
            builder.def_var(vars[dst.0 as usize], v);
        }
        Inst::Alloc {
            dst,
            alloc,
            roots,
            debug,
        } => {
            // Spill live Gc roots into the shadow frame *before* the allocating
            // call: the wrapper may trigger a collection (§12.4), and the
            // collector walks the frame (ADR-019).
            spill.spill_safepoint(builder, roots, debug, vars);
            // **One authority for the instruction→symbol mapping** (MIR-10).
            // Which wrapper creates the object, and which fills one slot of a
            // composite, is `AllocKind`'s answer — and it is the same answer
            // `Inst::can_fault` reads to decide whether the MIR verifier
            // requires a `CheckFault` after this instruction. Written out here
            // as well, the two would be separate statements of one fact, and
            // the next person to change a symbol in this match would make the
            // verifier lie about which calls can fault.
            let Some(ctor_sym) = alloc.constructor() else {
                // The only allocation with no constructor is a collection
                // whose ctor has no `praxis_*_new` wrapper — `Range` (built
                // from its endpoints by `praxis_range_new`, an `Inst::Call`)
                // and the compiler-internal `Seq`. Both are unreachable from
                // source: `collection_from_name` resolves the *type*, but no
                // construction lowering exists.
                return Err(anyhow!(
                    "construction of {alloc:?} not yet implemented (M8 workstream)"
                ));
            };
            // The slot-filler, for the four composites the backend builds in
            // two phases (allocate, then set each slot).
            let filler = || {
                alloc
                    .filler()
                    .expect("a composite allocation names its slot-filler")
            };
            match alloc {
                AllocKind::Int { value }
                | AllocKind::Bool { value }
                | AllocKind::Char { value }
                | AllocKind::Float { value } => {
                    // The four scalar boxes are one shape: pass the payload
                    // word, take back the `GcRef`. `Float`'s scalar local
                    // holds the f64 bit pattern as an i64 and
                    // `praxis_alloc_float` reassembles the f64; `Char`'s is a
                    // u32 Unicode scalar the wrapper validates.
                    let arg = builder.use_var(vars[value.0 as usize]);
                    let result = call_symbol(builder, ctx_val, &[arg], ctor_sym, module, imports)?;
                    builder.def_var(vars[dst.0 as usize], result);
                }
                AllocKind::Unit => {
                    let result = call_symbol(builder, ctx_val, &[], ctor_sym, module, imports)?;
                    builder.def_var(vars[dst.0 as usize], result);
                }
                AllocKind::Text { value } => {
                    // Embed the string as a data object, then call praxis_alloc_text
                    // with (ptr, len).
                    let (ptr, len_val) = embed_text(builder, generation, value);
                    let result =
                        call_symbol(builder, ctx_val, &[ptr, len_val], ctor_sym, module, imports)?;
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
                    let schema_ptr = record_schema_for(db, *record_def_id, generation)?;
                    let schema_imm = builder.ins().iconst(GC, schema_ptr as i64);
                    // praxis_alloc_record(ctx, schema_ptr) -> GcRef.
                    let record_ref =
                        call_symbol(builder, ctx_val, &[schema_imm], ctor_sym, module, imports)?;
                    // Fill in each field in declaration order. The field locals
                    // are already spilled into the shadow frame by
                    // `emit_spill` above; here we pass them as call args.
                    let set_field = filler();
                    for (idx, field_local) in fields.iter().enumerate() {
                        let field_val = builder.use_var(vars[field_local.0 as usize]);
                        let idx_val = builder.ins().iconst(GC, idx as i64);
                        call_symbol(
                            builder,
                            ctx_val,
                            &[record_ref, idx_val, field_val],
                            set_field,
                            module,
                            imports,
                        )?;
                    }
                    builder.def_var(vars[dst.0 as usize], record_ref);
                }
                AllocKind::Enum {
                    enum_def_id,
                    variant_idx,
                    ty,
                    args,
                } => {
                    // Build (or fetch a cached) 'static EnumSchema for this
                    // enum type, embed its address, and call
                    // praxis_alloc_enum(ctx, schema_ptr, tag) -> GcRef. The
                    // arity is read from the schema rather than passed here, so
                    // an allocation whose arity disagrees with its shape is not
                    // expressible. Then fill in each payload via
                    // praxis_enum_set_payload.
                    let schema_ptr = enum_schema_for(db, *enum_def_id, *ty, generation)?;
                    let schema_imm = builder.ins().iconst(GC, schema_ptr as i64);
                    let tag_val = builder.ins().iconst(GC, *variant_idx as i64);
                    let enum_ref = call_symbol(
                        builder,
                        ctx_val,
                        &[schema_imm, tag_val],
                        ctor_sym,
                        module,
                        imports,
                    )?;
                    let set_payload = filler();
                    for (idx, arg_local) in args.iter().enumerate() {
                        let arg_val = builder.use_var(vars[arg_local.0 as usize]);
                        let idx_val = builder.ins().iconst(GC, idx as i64);
                        call_symbol(
                            builder,
                            ctx_val,
                            &[enum_ref, idx_val, arg_val],
                            set_payload,
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
                    // The arity comes from the *elements* when the type does not
                    // give one (REP-23). A zero-arity schema sizes the payload
                    // to zero, so both `praxis_tuple_set` calls below wrote into
                    // nothing and `[10, 20].enumerate()` answered `[(), ()]` —
                    // back when a fused `enumerate`/`zip` pair carried
                    // `MirType::Opaque` (MIR-05 gave it the catalog's type).
                    let schema_ptr = tuple_schema_for(db, *ty, elements.len(), generation)?;
                    let schema_imm = builder.ins().iconst(GC, schema_ptr as i64);
                    // praxis_alloc_tuple(ctx, schema_ptr) -> GcRef.
                    let tuple_ref =
                        call_symbol(builder, ctx_val, &[schema_imm], ctor_sym, module, imports)?;
                    // Fill in each element in positional order.
                    let set_elem = filler();
                    for (idx, el_local) in elements.iter().enumerate() {
                        let el_val = builder.use_var(vars[el_local.0 as usize]);
                        let idx_val = builder.ins().iconst(GC, idx as i64);
                        call_symbol(
                            builder,
                            ctx_val,
                            &[tuple_ref, idx_val, el_val],
                            set_elem,
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
                        ctor_sym,
                        module,
                        imports,
                    )?;
                    let set_capture = filler();
                    for (idx, cap_local) in captures.iter().enumerate() {
                        let cap_val = builder.use_var(vars[cap_local.0 as usize]);
                        let idx_val = builder.ins().iconst(GC, idx as i64);
                        call_symbol(
                            builder,
                            ctx_val,
                            &[closure_ref, idx_val, cap_val],
                            set_capture,
                            module,
                            imports,
                        )?;
                    }
                    builder.def_var(vars[dst.0 as usize], closure_ref);
                }
                AllocKind::Collection { ctor, args } => {
                    // M8 WS1: `Vec[T]()`, `Grid[T]()`, etc. `ctor_sym` is the
                    // `praxis_<kind>_new` wrapper; what differs per ctor is only
                    // the *arguments*, and resolving the element descriptor
                    // recursively is what makes a nested collection (e.g.
                    // `Vec[Vec[Int]]`) dispatch eq/hash correctly.
                    use praxis_types::CollectionCtor;
                    let call_args: Vec<Value> = match ctor {
                        // BitSet is nullary (no element descriptor); elements
                        // are always Int. praxis_bitset_new takes only ctx.
                        CollectionCtor::BitSet => Vec::new(),
                        // Grid construction from source `Grid()`: an empty
                        // 0×0 grid. (The input parser is the usual grid
                        // constructor; source construction is for manual
                        // grids filled via set.) praxis_grid_new takes
                        // (descriptor, width, height).
                        CollectionCtor::Grid => {
                            let el_desc = collection_element_descriptor_for(db, args, 0)?;
                            vec![
                                builder.ins().iconst(GC, el_desc as i64),
                                builder.ins().iconst(GC, 0),
                                builder.ins().iconst(GC, 0),
                            ]
                        }
                        // Everything else takes one descriptor: the element
                        // type, or — for `Map` and `Counter` — the *key* type.
                        // A Map's value descriptor is adopted from the first
                        // inserted value at runtime (§11.3).
                        _ => {
                            let desc = collection_element_descriptor_for(db, args, 0)?;
                            vec![builder.ins().iconst(GC, desc as i64)]
                        }
                    };
                    let result =
                        call_symbol(builder, ctx_val, &call_args, ctor_sym, module, imports)?;
                    builder.def_var(vars[dst.0 as usize], result);
                }
            }
        }
        Inst::ExtractScalar { dst, src, scalar } => {
            // Which wrapper reads this payload width is `ScalarKind`'s answer,
            // stated once in `praxis_mir::ir` (MIR-10). It used to be written
            // here and nowhere else, which meant the MIR verifier's "can this
            // instruction fault" would have had to restate it — two statements
            // of one fact, and the next edit to this match would have made the
            // verifier lie.
            //
            // Since ADR-102 that wrapper is the *cold path* rather than the
            // whole lowering: `emit_scalar_load` proves the descriptor inline
            // and loads the payload inline, and branches to the wrapper when
            // the proof fails. `load_symbol()` is still the one authority for
            // which wrapper that is, so `Inst::fault_reason` still reads the
            // same function the backend does.
            let src_val = builder.use_var(vars[src.0 as usize]);
            emit_scalar_load(
                builder,
                ctx_val,
                src_val,
                vars[dst.0 as usize],
                *scalar,
                module,
                imports,
            )?;
        }
        Inst::Materialize {
            dst,
            src,
            scalar,
            roots,
            debug,
        } => {
            // Materialize re-boxes a scalar → it allocates → safepoint.
            spill.spill_safepoint(builder, roots, debug, vars);
            // A scalar payload re-boxed: Int → praxis_alloc_int, Bool →
            // alloc_bool, Char → praxis_alloc_char. The mapping is
            // `ScalarKind::alloc_symbol`, for `ExtractScalar`'s reason above.
            let src_val = builder.use_var(vars[src.0 as usize]);
            let result = call_symbol(
                builder,
                ctx_val,
                &[src_val],
                scalar.alloc_symbol(),
                module,
                imports,
            )?;
            builder.def_var(vars[dst.0 as usize], result);
        }
        Inst::StoreScalar { .. } => {
            // M4 scalars are immutable objects; StoreScalar is a no-op placeholder
            // for the future mutable-Int optimization.
        }
        Inst::IntBinOp {
            op,
            dst,
            lhs,
            rhs,
            overflow,
        } => {
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
            // Overflow is reported by branching to a cold block that calls a
            // non-allocating raise wrapper — see `raise_on_cold_path`, which
            // carries the argument for why a branch beats the unconditional
            // call this used to emit. The site is still not a GC safepoint and
            // still spills no roots; the `CheckFault` that MIR emits next is
            // still what diverts to the fault epilogue, and it lowers into the
            // block both arms of the diamond converge on.
            //
            // `Overflow::Bounded` sites — a `for` index bump, a `count`
            // accumulator — skip the test entirely: their operands are bounded
            // by a collection's length, so the predicate is provably false and
            // computing it cost two instructions and a call per iteration.
            let l = builder.use_var(vars[lhs.0 as usize]);
            let r = builder.use_var(vars[rhs.0 as usize]);
            if matches!(overflow, Overflow::Bounded) {
                let bare = match op {
                    IntBinOp::Add => builder.ins().iadd(l, r),
                    IntBinOp::Sub => builder.ins().isub(l, r),
                    IntBinOp::Mul => builder.ins().imul(l, r),
                    // Unreachable: `verify` rejects a bounded division, because
                    // no bound on the operands rules out a zero divisor, and
                    // `sdiv`/`srem` *trap* on one.
                    IntBinOp::Div | IntBinOp::Rem => {
                        anyhow::bail!("`{op:?}` cannot be lowered as a bounded operation")
                    }
                };
                builder.def_var(vars[dst.0 as usize], bare);
                return Ok(());
            }
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
                    raise_on_cold_path(
                        builder,
                        ctx_val,
                        differs,
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

                    // Two diamonds in sequence, in this order. The conditions
                    // are mutually exclusive (`r == 0` versus `r == -1`), so
                    // neither kind can overwrite the other — as when both
                    // raises were straight-line calls.
                    raise_on_cold_path(
                        builder,
                        ctx_val,
                        by_zero,
                        RuntimeSymbol::RaiseDivByZeroIf,
                        module,
                        imports,
                    )?;
                    raise_on_cold_path(
                        builder,
                        ctx_val,
                        overflows,
                        RuntimeSymbol::RaiseIntOverflowIf,
                        module,
                        imports,
                    )?;
                    value
                }
            };
            builder.def_var(vars[dst.0 as usize], result);
            // No fault check here. There used to be a bare
            // `praxis_check_fault` call at this point whose result was
            // discarded and which no branch followed — a leftover from before
            // MIR carried `Inst::CheckFault`, costing one call per checked
            // arithmetic op and diverting nothing. The `Inst::CheckFault` the
            // builder emits next is what actually observes the raise, and the
            // MIR verifier now requires it to be there (MIR-10).
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
        Inst::FloatNeg { dst, src } => {
            // IEEE-754 `negate`: flip the sign bit, change nothing else. A
            // negation is not a subtraction from zero — `0.0 - x` answers
            // `+0.0` at `x = +0.0`, which is what lost the `-0.0` literal
            // (REP-50) — so the sign flip is the instruction and Cranelift's
            // `fneg` is it.
            let s_i = builder.use_var(vars[src.0 as usize]);
            let s = i64_to_f64(builder, s_i);
            let negated = builder.ins().fneg(s);
            let result = f64_to_i64(builder, negated);
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
            roots,
            debug,
        } => {
            // A call may allocate (and M4 user functions allocate freely) →
            // safepoint. Spill the live Gc roots before the call.
            spill.spill_safepoint(builder, roots, debug, vars);
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
            roots,
            debug,
        } => {
            // M7, §4.10 (Approach B). An indirect call through a closure value.
            // Spill live Gc roots (safepoint — the call may allocate/GC), read
            // the closure's `fn_ptr` via `praxis_closure_fn_ptr`, then emit a
            // Cranelift `call_indirect` with the signature
            // `fn(ctx, closure, args...) -> i64`. The closure is passed as the
            // hidden first explicit arg; the synthetic function loads its
            // captures at entry.
            spill.spill_safepoint(builder, roots, debug, vars);
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
            roots,
            debug,
        } => {
            // Structural equality via praxis_struct_eq(ctx, a, b) -> i64 (0/1).
            // The call may trigger GC → spill live Gc roots first (safepoint).
            spill.spill_safepoint(builder, roots, debug, vars);
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
        Inst::ValueCmp { dst, lhs, rhs } => {
            // Ordering through the descriptor's compare callback:
            // praxis_value_cmp(ctx, a, b) -> -1/0/1 (ADR-045). The wrapper is
            // `Effect::Faults` — it allocates nothing — so this is not a
            // safepoint and there is nothing to spill; the `CheckFault` the
            // builder emits next observes a type mismatch.
            let l = builder.use_var(vars[lhs.0 as usize]);
            let r = builder.use_var(vars[rhs.0 as usize]);
            let result = call_symbol(
                builder,
                ctx_val,
                &[l, r],
                RuntimeSymbol::ValueCmp,
                module,
                imports,
            )?;
            builder.def_var(vars[dst.0 as usize], result);
        }
        Inst::CheckFault { on_fault, debug } => {
            // Divert to the fault block when a fault is pending (§10.4). The
            // faultable op just before this set `pending_fault` (or a callee
            // did). If a fault is pending, branch to the function's fault block
            // — which restores the shadow stack and returns the Unit sentinel,
            // unwinding cleanly to the host. The rest of this MIR block's
            // instructions lower into a fresh fall-through block, so the
            // diversion does not strand them.
            //
            // The test is two loads and a branch (ADR-102), where it was a call
            // to `praxis_check_fault` — a guarded call, two null tests, an
            // `is_pending()` and a `Result` discriminant, to read one word. It
            // runs once per faultable instruction, which after ADR-088 is once
            // per checked arithmetic op, per user call and per faulting wrapper
            // call, so it was among the most-executed instructions in the
            // language.
            //
            // Neither load tests for null, and neither needs to. ADR-017's
            // Consequences state the invariant: "`pending_fault` is always
            // non-null once wired; the pending state lives in the `Fault` slot,
            // not in pointer-nullness." `Runtime::context` is the only producer
            // of a context generated code ever sees and wires the slot;
            // `RuntimeContext::placeholder`, the one null-wiring constructor, is
            // `unsafe` and test-only. `a_wired_context_has_a_fault_slot` pins
            // it. Nor is `ctx` itself a new assumption: the prologue's recursion
            // guard already loads through it unconditionally.
            //
            // The branch tests the loaded kind directly, which is
            // `Fault::is_pending()` because `FaultKind::None` is 0 and no other
            // kind is (`the_fault_record_is_one_kind_at_offset_zero`).
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
            spill.spill_debug(builder, debug, vars);
            // Plain `trusted()` — `notrap + aligned`, and deliberately *not*
            // `readonly` or `can_move`. Cranelift's alias analysis treats a call
            // as clobbering memory, which is exactly what must stay true here:
            // the whole point of this instruction is to observe a write a callee
            // (or the raise wrapper on a cold path) just made. Claiming the load
            // is readonly would let two `CheckFault`s collapse into one and a
            // fault go unobserved — as an intermittent wrong answer, not a
            // compile error.
            let flags = MemFlags::trusted();
            let slot = builder.ins().load(GC, flags, ctx_val, PENDING_FAULT_OFFSET);
            let pending = builder
                .ins()
                .load(types::I32, flags, slot, FAULT_KIND_OFFSET);
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
        Inst::LoadCapture {
            dst,
            closure,
            index,
        } => {
            // praxis_closure_capture(ctx, closure, index) -> GcRef. The index is
            // an immediate here, not a value read out of a local: it is a raw
            // ABI word, and the manifest declares the parameter `RawI64`. Not a
            // safepoint (the wrapper is `Effect::Pure`), so no spill.
            let closure_val = builder.use_var(vars[closure.0 as usize]);
            let idx_val = builder.ins().iconst(GC, *index as i64);
            let capture = call_symbol(
                builder,
                ctx_val,
                &[closure_val, idx_val],
                RuntimeSymbol::ClosureCapture,
                module,
                imports,
            )?;
            builder.def_var(vars[dst.0 as usize], capture);
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
        Inst::LoadTupleElem { dst, src, index } => {
            // praxis_tuple_get(ctx, tuple, idx) -> GcRef. `Pure`, so not a
            // safepoint — the element is already allocated inside the tuple.
            let tuple = builder.use_var(vars[src.0 as usize]);
            let idx_val = builder.ins().iconst(GC, *index as i64);
            let elem = call_symbol(
                builder,
                ctx_val,
                &[tuple, idx_val],
                RuntimeSymbol::TupleGet,
                module,
                imports,
            )?;
            builder.def_var(vars[dst.0 as usize], elem);
        }
        Inst::EnumTag { dst, src } => {
            // Read the tag directly from the EnumPayload. The payload starts at
            // gc_ref + GcHeader::payload_offset_for(align_of(EnumPayload)) —
            // the runtime's single object-layout authority, not a header size
            // this file re-derives. The tag sits at `ENUM_TAG_OFFSET` within
            // the payload — derived from the `#[repr(C)]` struct, not written
            // out as a literal, because it moved when the payload gained its
            // schema pointer (RT-13).
            let enum_ref = builder.use_var(vars[src.0 as usize]);
            let payload_offset =
                praxis_runtime::gc::GcHeader::payload_offset_for(core::mem::align_of::<
                    praxis_runtime::enums::EnumPayload,
                >()) as i64;
            let tag_ptr = builder.ins().iadd_imm_s(enum_ref, payload_offset);
            // Read just the u32 tag (not a full I64 — the 4 bytes of padding
            // after the tag are uninitialized bumpalo memory). In Cranelift
            // 0.134, uload32 returns an I64 with the upper 32 bits zeroed.
            let tag = builder
                .ins()
                .uload32(MemFlags::trusted(), tag_ptr, ENUM_TAG_OFFSET);
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
            emit_pop_shadow_frame(builder, ctx_val, spill);
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
            emit_pop_shadow_frame(builder, ctx_val, spill);
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

/// Emit the shadow-stack epilogue (ADR-019, ADR-101): give this frame's slots
/// back and restore the call depth. Two stores, no call.
///
/// Both restore an absolute the prologue saved rather than undoing an
/// increment. The extern helper this replaced decremented `recursion_depth`
/// with a `saturating_sub` precisely because a fault path could otherwise
/// underflow it; there is nothing to saturate when the value being written is
/// the one this call found on entry, and an imbalance introduced below this
/// frame cannot leak upward past it.
fn emit_pop_shadow_frame(builder: &mut FunctionBuilder, ctx_val: Value, spill: &SpillCtx<'_>) {
    let saved_depth = builder.use_var(spill.saved_depth_var);
    builder.ins().store(
        MemFlags::trusted(),
        saved_depth,
        ctx_val,
        RECURSION_DEPTH_OFFSET as i32,
    );
    let base = builder.use_var(spill.frame_var);
    emit_slot_stack_pop(builder, ctx_val, SHADOW_OFFSET, base);
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
        // `GcUnit` is a `GcRef` like any other at the machine level — the
        // distinction it draws is about what the *value* means, not how it
        // travels (RT-14).
        AbiRet::Gc | AbiRet::GcUnit | AbiRet::Ptr => sig.returns.push(AbiParam::new(pointer)),
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

/// Load a [`GcConst`] — a reference the runtime minted at startup — out of the
/// live context. [`load_unit_sentinel`]'s shape, one indirection deeper.
///
/// The address is read from `ctx` at run time rather than baked in as an
/// `iconst`, which is what makes it correct for whichever `Runtime` the code is
/// executed against: there is no heap at compile time, and a debugger session
/// replaces its `Jit` while keeping its `Runtime`. See [`GcConst`].
fn load_gc_const(builder: &mut FunctionBuilder, ctx: Value, konst: GcConst) -> Value {
    match konst {
        GcConst::SmallInt(n) => {
            // `praxis-mir` only emits this for a value `small_int::index_of`
            // accepted, so the index exists — the `expect` documents that the
            // two agree rather than guarding against them disagreeing, and it
            // would fire at compile time, not in a running program.
            let index = praxis_runtime::small_int::index_of(n)
                .expect("MIR emits ConstGc::SmallInt only for an interned value");
            let elem_offset = (index * praxis_runtime::SMALL_INT_STRIDE) as i32;
            // Two loads and no adds: the table base is at a fixed offset in the
            // context and the element offset is a constant well inside `i32`
            // (the whole table is `SMALL_INT_COUNT` × 8 bytes).
            let base = builder
                .ins()
                .load(GC, MemFlags::trusted(), ctx, SMALL_INTS_OFFSET as i32);
            builder
                .ins()
                .load(GC, MemFlags::trusted(), base, elem_offset)
        }
        // One load: `Unit` and the two `Bool`s are cached in the context
        // directly, not behind a table, because there is a fixed, tiny number of
        // them.
        GcConst::Unit => load_unit_sentinel(builder, ctx),
        GcConst::Bool(b) => {
            let offset = if b { TRUE_REF_OFFSET } else { FALSE_REF_OFFSET };
            builder
                .ins()
                .load(GC, MemFlags::trusted(), ctx, offset as i32)
        }
    }
}

/// How a scalar payload is read once its descriptor has been proved.
///
/// One variant per payload *width*, not per `ScalarKind`, because that is what
/// the load instruction depends on — and because REP-37 was exactly a `Bool`
/// read at an `Int`'s width. The kinds that share a width still get their own
/// row in [`inline_scalar_load_of`]; they differ in which descriptor is proved.
enum ScalarLoad {
    /// Eight bytes, straight into the uniform `i64` channel. `Int`'s payload,
    /// and `Float`'s — a float rides the channel as `f64::to_bits()`, which is
    /// what `praxis_float_load` returns, so the same load answers both.
    Word,
    /// Four bytes, zero-extended. `Char`'s payload is a `u32` code point and
    /// `i64::from(code)` is what `praxis_char_load` answers.
    HalfWord,
    /// One byte, then `!= 0`. Not a shortcut for a one-byte load: a Rust `bool`
    /// whose byte is neither 0 nor 1 is an *invalid value*, so `praxis_bool_load`
    /// reads the byte and compares rather than materializing a `bool`
    /// (`BoolPayload` is a `u8` for that reason). The inline form reproduces the
    /// wrapper's answer, including for a byte the wrapper would not trust.
    BoolByte,
}

/// The descriptor, payload alignment and load width for a scalar kind whose
/// payload generated code may read inline — or `None` for one it may not.
///
/// A new [`praxis_mir::ScalarKind`] variant fails to compile here, which is the
/// point: this is the second statement of a mapping whose first is
/// `ScalarKind::load_symbol` (MIR-10), and the two must not drift. They cannot
/// disagree about *which* type is being read, because the cold path below calls
/// `load_symbol()` — this only adds which descriptor proves it and how wide the
/// read is.
fn inline_scalar_load_of(
    scalar: praxis_mir::ScalarKind,
) -> Option<(&'static praxis_runtime::TypeDescriptor, usize, ScalarLoad)> {
    use praxis_mir::ScalarKind;
    use praxis_runtime::scalars;
    Some(match scalar {
        ScalarKind::Int => (
            &scalars::INT,
            core::mem::align_of::<scalars::IntPayload>(),
            ScalarLoad::Word,
        ),
        ScalarKind::Bool => (
            &scalars::BOOL,
            core::mem::align_of::<scalars::BoolPayload>(),
            ScalarLoad::BoolByte,
        ),
        ScalarKind::Char => (
            &scalars::CHAR,
            core::mem::align_of::<scalars::CharPayload>(),
            ScalarLoad::HalfWord,
        ),
        ScalarKind::Float => (
            &scalars::FLOAT,
            core::mem::align_of::<scalars::FloatPayload>(),
            ScalarLoad::Word,
        ),
        // `Byte` is reserved and unwired, and its `load_symbol()` is `IntLoad`
        // — an eight-byte read of a one-byte payload, chosen "defensively" when
        // nothing emitted it. Giving that an inline form would be REP-37 by
        // construction: the descriptor check would prove `INT` of a value that
        // is not one, or prove nothing at all. Keep the call, which at least
        // refuses inside `int_payload`'s bounded reader.
        ScalarKind::Byte => return None,
    })
}

/// Read a scalar payload out of `src_val` into `dst`: an inline load guarded by
/// an inline descriptor check, with the existing wrapper as the cold path.
///
/// # The check is the proof, and the static type is not one
///
/// This does **not** trust the MIR local's type, and there is no version of this
/// that could. `Scalar` locals are `MirType::Opaque` by construction, the `src`
/// here is frequently a `Gc` local lowering allocated as `Opaque`, and — the
/// part that settles it — where a type *is* known it has been wrong. REP-56 is a
/// program that passes `praxis check` and emits `ExtractScalar { scalar: Int }`
/// against a value whose descriptor is `Unit`, zero bytes wide; a release build
/// answered an ASLR-varying number off an eight-byte out-of-bounds read. REP-49
/// and REP-37 are the same defect from two other directions.
///
/// So the check survives inlining, and it survives it in the form
/// `int_payload`'s doc insists on: **unconditional, in every profile**. What
/// moves is only where the refusal lives — from a never-taken branch inside a
/// `#[cold]` callee to a never-taken branch to a Cranelift cold block. The
/// refusal itself is bit-for-bit what it was, because the cold block *calls the
/// same wrapper*: `praxis_int_load` re-runs `read_scalar`, fails, panics,
/// `abi_guard!` catches it, and — its manifest row being `Effect::Pure`, so the
/// panic fault is not observable — it prints that message and aborts. Same
/// message, same exit, no new wrapper, no manifest change.
///
/// # What else the check buys
///
/// The payload offset is folded from `GcHeader::payload_offset_for`, exactly as
/// `Inst::EnumTag` folds it. That is correct *only because* the header records
/// what the allocator computed from **that descriptor's** alignment (ADR-039
/// decision 1) — so proving the descriptor is also what makes the constant
/// offset the offset the allocator actually used. And ADR-039 decision 3's
/// poisoning falls out for free: a swept header has a null descriptor, which
/// fails the comparison and routes to the wrapper, whose `GcHeader::descriptor()`
/// panics "descriptor read from a poisoned (swept) GcHeader" — again the same
/// refusal as before.
///
/// `Inst::EnumTag` inlines a payload read with no check at all. It is a weaker
/// precedent than it looks: what licenses it is ADR-091 (a variant pattern's
/// enum is the scrutinee's, so the static type reaches the read), and that is
/// the same class of argument REP-56 falsified here. Whether the tag read should
/// acquire this check too is a real question, and not one this answers.
#[allow(clippy::too_many_arguments)] // The lowering context, as `lower_inst` carries it.
fn emit_scalar_load<M: Module>(
    builder: &mut FunctionBuilder,
    ctx_val: Value,
    src_val: Value,
    dst: Variable,
    scalar: praxis_mir::ScalarKind,
    module: &mut M,
    imports: &mut HashMap<RuntimeSymbol, FuncRef>,
) -> Result<()> {
    let sym = scalar.load_symbol();
    let Some((descriptor, payload_align, load)) = inline_scalar_load_of(scalar) else {
        let result = call_symbol(builder, ctx_val, &[src_val], sym, module, imports)?;
        builder.def_var(dst, result);
        return Ok(());
    };

    // `MemFlags::trusted()` is `notrap + aligned`, which is what the prologue's
    // context reads use and all this needs: the header is live and its fields
    // are in bounds by construction. It deliberately does **not** claim
    // `readonly`, so Cranelift's alias analysis keeps treating calls as
    // clobbering this memory — `set_mark_color` and the sweep write headers.
    let flags = MemFlags::trusted();
    let have = builder.ins().load(GC, flags, src_val, GC_DESCRIPTOR_OFFSET);
    let want = builder.ins().iconst(GC, descriptor as *const _ as i64);
    let ok = builder.ins().icmp(IntCC::Equal, have, want);

    let fast = builder.create_block();
    let slow = builder.create_block();
    let merge = builder.create_block();
    builder.set_cold_block(slow);
    builder.ins().brif(ok, fast, &[], slow, &[]);

    builder.switch_to_block(fast);
    {
        // The one object-layout authority, folded to an immediate — not a
        // header size this file re-derives (ADR-039 decision 1).
        let offset = praxis_runtime::GcHeader::payload_offset_for(payload_align) as i32;
        let value = match load {
            ScalarLoad::Word => builder.ins().load(GC, flags, src_val, offset),
            // `uload32` returns an I64 with the upper half zeroed, which *is*
            // `i64::from(code)`.
            ScalarLoad::HalfWord => builder.ins().uload32(flags, src_val, offset),
            ScalarLoad::BoolByte => {
                let byte = builder.ins().uload8(GC, flags, src_val, offset);
                let set = builder.ins().icmp_imm_u(IntCC::NotEqual, byte, 0);
                builder.ins().uextend(GC, set)
            }
        };
        builder.def_var(dst, value);
        builder.ins().jump(merge, &[]);
    }

    builder.switch_to_block(slow);
    {
        let result = call_symbol(builder, ctx_val, &[src_val], sym, module, imports)?;
        // The wrapper does not return: every kind with an inline form refuses a
        // wrong descriptor by panicking. `def_var` anyway, because Cranelift
        // needs the variable defined on every path into `merge` — proving the
        // path is dead is the optimizer's business, not the lowering's.
        builder.def_var(dst, result);
        builder.ins().jump(merge, &[]);
    }

    // `def_var` in both arms rather than a block parameter: `FunctionBuilder`'s
    // SSA construction inserts the join itself, which is the idiom every other
    // arm of `lower_inst` already uses.
    builder.switch_to_block(merge);
    Ok(())
}

/// Report a fault when `predicate` is negative — the sign-bit form the
/// add/sub overflow tests produce.
///
/// The sign test is the branch's own condition, not a value: this used to
/// `ushr_imm_u(predicate, 63)` to shape an `i64` argument for the call, and
/// with the call gone there is nothing to shape it for.
fn raise_if_negative<M: Module>(
    builder: &mut FunctionBuilder,
    ctx: Value,
    predicate: Value,
    sym: RuntimeSymbol,
    module: &mut M,
    imports: &mut HashMap<RuntimeSymbol, FuncRef>,
) -> Result<()> {
    let cond = builder
        .ins()
        .icmp_imm_s(IntCC::SignedLessThan, predicate, 0);
    raise_on_cold_path(builder, ctx, cond, sym, module, imports)
}

/// Report a fault when `cond` is non-zero, by branching to a cold block that
/// calls `sym` — not by calling `sym` unconditionally and letting it decide.
///
/// # Why this is a branch now, and what the old comment had right
///
/// It read: *"The call is unconditional and the wrapper decides: an arithmetic
/// site stays one basic block, and the wrapper allocates nothing, so the site is
/// not a GC safepoint and needs no root spill."* Both clauses are true and the
/// second survives untouched — the cold block calls the same `Effect::Faults`
/// wrapper, which still allocates nothing, so this is still not a safepoint and
/// still spills no roots.
///
/// The first clause is the one that was mispriced. What a single basic block
/// buys is not having to keep values live across a CFG edge — but **a branch
/// does not clobber registers and a call does**. Cranelift must treat every
/// caller-saved register as dead across `bl praxis_raise_int_overflow_if`, so at
/// `opt_level=none` a loop doing one checked add per iteration spilled and
/// reloaded its counter, its accumulator and `ctx` around a call that a
/// non-faulting program never needs. A not-taken `cbz` is one instruction and
/// essentially always predicted; the hot path also gets *shorter*, because the
/// `ushr_imm_u` and the `uextend`s that existed only to shape an `i64` argument
/// are gone with the argument.
///
/// # Why the wrapper still takes a condition
///
/// The cold block passes a constant `1`, which is honest — it is reached only
/// when the predicate held — and keeps `praxis_raise_int_overflow_if`'s
/// `if condition != 0` a true statement rather than dead code. Adding an
/// unconditional `praxis_raise_int_overflow` to mirror
/// `praxis_raise_stack_overflow` would be tidier and costs two manifest rows,
/// two address-table arms and a doc rewrite; it is not worth that here.
///
/// # ADR-088 is untouched
///
/// The rule that a faulting instruction is observed by the next one is a
/// property of *MIR* (`verify::check_fault_observed`), and this emits no MIR.
/// Both arms of the diamond converge at `cont` before the `Inst::CheckFault`
/// that MIR requires next lowers, so the check runs on the raising path and the
/// non-raising path alike — as it did when the raise was straight-line.
fn raise_on_cold_path<M: Module>(
    builder: &mut FunctionBuilder,
    ctx: Value,
    cond: Value,
    sym: RuntimeSymbol,
    module: &mut M,
    imports: &mut HashMap<RuntimeSymbol, FuncRef>,
) -> Result<()> {
    let cold = builder.create_block();
    let cont = builder.create_block();
    // Cold-block placement runs in the machine-independent lowering
    // (`BlockLoweringOrder` reads `Layout::is_cold`), not in the mid-end, so it
    // applies at `opt_level=none` too — which is the level this change was
    // measured at.
    builder.set_cold_block(cold);
    builder.ins().brif(cond, cold, &[], cont, &[]);

    builder.switch_to_block(cold);
    let one = builder.ins().iconst(GC, 1);
    call_symbol(builder, ctx, &[one], sym, module, imports)?;
    builder.ins().jump(cont, &[]);

    // No block parameters: the raise wrapper returns `Void`, so no value crosses
    // the join. Everything the caller computed before the branch was defined in
    // a block that dominates `cont`, so it is readable there without one.
    builder.switch_to_block(cont);
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

/// Build (and cache) a `RecordSchema` for record def `id` in this JIT
/// generation, returning its address as a raw pointer the JIT embeds as an
/// immediate.
///
/// **The cache belongs to the generation, and its key carries the generation
/// id.** A `RecordDefId` is a *per-`TypeDb` positional index*: the debugger
/// mints a fresh `TypeDb` per `p EXPR` and per `reload`, so `RecordDefId(0)` in
/// one program names a different struct than in the next. The process-global
/// map this replaces was keyed on the bare `u32` and handed the second program
/// the first program's schema — whose field descriptors then read a `Text`
/// header as an `i64` (MIR-12, DBG-06).
///
/// Every field descriptor is resolved through the F11 bridge and every one must
/// resolve: the schema is what `equals`/`hash`/`format` dispatch through, so a
/// field mislabelled `Int` reads an `f64` or a `Text` header as an `i64`
/// (P0-11). A field whose type has no runtime object fails the compile.
fn record_schema_for(
    db: &praxis_types::TypeDb,
    id: u32,
    generation: &Generation,
) -> Result<*const praxis_runtime::records::RecordSchema> {
    use praxis_runtime::records::{RecordField, SchemaIdentity};
    use praxis_types::data::RecordDefId;
    let def = db.record_def(RecordDefId(id));
    // A schema is built from the def's *field* types, so a def with parameters
    // would resolve descriptors for its parameters rather than for whatever an
    // instance supplies (F12). The language cannot declare one — there is no
    // `struct P[T]` syntax — and a `TypedExpr::RecordLit` carries no arguments
    // to substitute, so this refuses rather than emitting a wrong layout.
    if !def.params.is_empty() {
        anyhow::bail!(
            "record `{}` is generic; a monomorphized instance is required before codegen",
            def.name.as_deref().unwrap_or("<anonymous>")
        );
    }
    // A declared record is its name; a structural one (§5.6) is its shape
    // (RT-12). The name is copied into the generation so the schema outlives
    // this `TypeDb` — the debugger's does not survive the command.
    let identity = match &def.name {
        Some(name) => SchemaIdentity::Nominal(generation.alloc_str(name)),
        None => SchemaIdentity::Anonymous,
    };
    generation.record_schema(id, identity, || {
        def.fields
            .iter()
            .map(|f| {
                Ok(RecordField {
                    name: generation.alloc_str(&f.name),
                    descriptor: descriptor_for_type(db, f.ty)
                        .with_context(|| format!("record field `{}`", f.name))?,
                })
            })
            .collect::<Result<Vec<_>>>()
    })
}

/// Build (and cache) an `EnumSchema` for enum def `id` at the instantiation
/// `ty` in this JIT generation, returning its address as a raw pointer the JIT
/// embeds as an immediate.
///
/// The schema is what gives a runtime enum value its nominal identity (RT-13):
/// without one, `Colour::Red` and `Light::Red` were the same value and no enum
/// could render its variant name.
///
/// **`record_schema_for`'s refusal of a generic def is deliberately not copied
/// here.** A record def with parameters cannot arrive at codegen — there is no
/// `struct P[T]` syntax and a `RecordLit` carries no arguments to substitute —
/// but `Option` *is* generic (F12) and is the one enum every `Map.get`,
/// `Grid.find` and graph walk answers, so refusing it would refuse the feature.
/// `variant_payload_of` substitutes the instance's arguments instead, which is
/// what `ty` is carried through the MIR for.
///
/// A payload slot whose type is still an inference *variable* — or a whole
/// instantiation the lowering had no type for — resolves to a **null**
/// descriptor rather than failing the compile, the same exception
/// `tuple_schema_for` makes for the same reason (HIR-01/MONO-01). The value's
/// own descriptor answers for it, and it is read off the object's header, so it
/// is never the wrong one.
fn enum_schema_for(
    db: &mut praxis_types::TypeDb,
    id: u32,
    ty: MirType,
    generation: &Generation,
) -> Result<*const praxis_runtime::enums::EnumSchema> {
    use praxis_runtime::records::SchemaIdentity;
    use praxis_types::data::{EnumDefId, TypeData};
    let def_id = EnumDefId(id);
    let def = db.enum_def(def_id).clone();
    // A declared enum is its name; the input parser's `choice` (§7.5) produces
    // an anonymous one, whose identity is its variant shape. The name is copied
    // into the generation so the schema outlives this `TypeDb`.
    let identity = match &def.name {
        Some(name) => SchemaIdentity::Nominal(generation.alloc_str(name)),
        None => SchemaIdentity::Anonymous,
    };
    // The instance's type arguments, when the lowering had a type for the
    // value. `MirType::Opaque` and a non-enum type both mean "no arguments to
    // substitute"; the def's own payload types are then resolved directly, and
    // a generic def's parameters resolve to null slots.
    let args: Vec<praxis_types::Type> = match ty.known().map(|t| db.data(db.follow(t))) {
        Some(TypeData::Enum { def: d, args }) if *d == def_id => args.to_vec(),
        _ => Vec::new(),
    };
    let mut variants: Vec<(
        &'static str,
        Vec<*const praxis_runtime::descriptor::TypeDescriptor>,
    )> = Vec::with_capacity(def.variants.len());
    for (idx, variant) in def.variants.iter().enumerate() {
        let payload_types: Vec<praxis_types::Type> = if args.is_empty() {
            variant.payload.clone()
        } else {
            db.variant_payload_of(def_id, &args, idx)
        };
        let descriptors = payload_types
            .iter()
            .enumerate()
            .map(|(slot, t)| match praxis_repr::descriptor_for_type(db, *t) {
                Ok(d) => Ok(d as *const _),
                Err(e) if e.is_unresolved() => Ok(std::ptr::null()),
                Err(e) => Err(anyhow!(
                    "enum variant `{}` payload {slot}: cannot emit a runtime descriptor for `{}`: {}",
                    variant.name,
                    db.render(*t),
                    e.reason
                )),
            })
            .collect::<Result<Vec<_>>>()?;
        variants.push((generation.alloc_str(&variant.name), descriptors));
    }
    Ok(generation.enum_schema(id, identity, variants))
}

/// Build (and cache) a `TupleSchema` for the tuple type `ty` in this JIT
/// generation, returning its address as a raw pointer the JIT embeds as an
/// immediate.
///
/// The cache is keyed by the **resolved element-descriptor sequence**, not by
/// the static `Type` id: the type arena does not structurally intern tuples
/// (each `db.tuple(...)` call mints a fresh slot), so two `(Int, Int)` literals
/// get different `Type` ids but the same shape. Keying on the descriptor
/// pointers gives true structural de-duplication, so two same-shaped tuples
/// share one schema and compare structurally equal at runtime.
fn tuple_schema_for(
    db: &praxis_types::TypeDb,
    ty: MirType,
    arity: usize,
    generation: &Generation,
) -> Result<*const praxis_runtime::tuples::TupleSchema> {
    use praxis_types::data::TypeData;
    // Resolve the element types. `Opaque` means the lowering had no tuple type;
    // a non-tuple type is a misuse (the HIR only lowers `TypedExpr::Tuple`
    // here). Both degrade to a schema of `arity` **unknown** slots rather than
    // panicking in the JIT — but only the second is a surprise now, because the
    // first says so in the MIR. Since MIR-05 every tuple-building site supplies
    // a type, including the fused `enumerate`/`zip` pairs that were the standing
    // exception; what an `Opaque` reaches here through today is a builder with
    // genuinely nothing to say.
    //
    // Unknown, and not *absent* (REP-23): the arity sizes the payload, so a
    // zero-slot schema for a two-element tuple made `praxis_tuple_set` drop both
    // values and `[10, 20].enumerate()` answer `[(), ()]`. The values' own
    // descriptors answer for the slots, which is ADR-066's decision 5 — the type
    // gap stays MIR-05's, and the silent dropping stops being anyone's.
    let element_types: Vec<Option<praxis_types::Type>> =
        match ty.known().map(|t| db.data(db.follow(t))) {
            Some(TypeData::Tuple(els)) => els.iter().copied().map(Some).collect(),
            _ => vec![None; arity],
        };
    // Every slot that has a type must resolve to *that* type's descriptor. The
    // schema is what tuple equality, hashing and formatting dispatch through, so
    // a `Unit` or `Enum` element mislabelled `Int` reads its payload as an `i64`
    // (P0-11).
    //
    // A slot that is still an inference *variable* is the one exception, and it
    // is the same one `collection_arg_descriptor` makes for the same reason
    // (HIR-01/MONO-01, hazard H10): `let m = Map()` generalizes at the `let`, so
    // a `for kv in m` whose body never looks inside the pair leaves K and V
    // unresolved, and failing the compile there rejects a working program. The
    // null slot says "no static type" and the runtime reads the value's own
    // descriptor off its header — which is never the wrong one.
    let descriptors: Vec<*const praxis_runtime::descriptor::TypeDescriptor> = element_types
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let Some(t) = t else {
                return Ok(std::ptr::null());
            };
            match praxis_repr::descriptor_for_type(db, *t) {
                Ok(d) => Ok(d as *const _),
                Err(e) if e.is_unresolved() => Ok(std::ptr::null()),
                Err(e) => Err(anyhow!(
                    "tuple element {i}: cannot emit a runtime descriptor for `{}`: {}",
                    db.render(*t),
                    e.reason
                )),
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(generation.tuple_schema(&descriptors))
}

/// Build the `[DebugLocalMeta]` array for a function's `Gc` locals, in the
/// same order as the `gc_slot` map iterates them (so a local's shadow-slot
/// index doubles as its debug-local index), and store it in the generation
/// arena. Each entry carries the source name (interned in the same arena, empty
/// for temps), a per-local symbol-id placeholder, the static type descriptor
/// resolved from the MIR local's `Type` (§9.3, M10-WS2), the user-vs-temp
/// classification, and the source span.
///
/// The array is deduplicated by content: a function lowered twice into one
/// generation — which is what a debugger session does on every `p EXPR` —
/// yields the same metadata and pays for it once (DBG-05, MIR-13).
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
    generation: &Generation,
) -> (*const DebugLocalMeta, usize) {
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
            .map(|n| generation.alloc_str(n))
            .unwrap_or("");
        // Deep-resolve the local's type before capturing its id, so the id
        // points at a fully-concrete type (e.g. Vec[Int], not Vec[?T]). The
        // element/param vars of a composite are left untouched by `follow`
        // (top-level only); `deep_resolve` recurses and interns a resolved
        // copy. Idempotent on already-resolved types. (M10b-WS4)
        // `Opaque` locals (a pipeline accumulator, a scalar's slot) have no
        // static type to thread: emit a null descriptor and `NO_STATIC_TYPE`
        // rather than the descriptor and id of whichever type the arena
        // interned first, which is what the old `Type(0)` placeholder produced.
        // The debugger renders these without a type column (P0-02).
        let resolved_ty = local.ty.known().map(|t| db.deep_resolve(t));
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
            descriptor: resolved_ty
                .map(|t| debug_descriptor_for_type(db, t))
                .unwrap_or(std::ptr::null()),
            // The full static `Type` id (M10-WS1b): `Type(u32)`. Lets the
            // debugger reconstruct the exact local type (incl. collection
            // element types / record shapes) the runtime `descriptor` loses.
            type_id: resolved_ty.map_or(praxis_runtime::debug::NO_STATIC_TYPE, |t| t.to_u32()),
            kind,
            span_start,
            span_end,
        });
        symbol_id += 1;
    }
    generation.debug_local_metas(metas)
}

/// The runtime descriptor for values of type `ty`, or a compile error.
///
/// A thin wrapper over [`praxis_repr::descriptor_for_type`], which is the single
/// exhaustive map (F11). This function exists only to turn its
/// [`NoRuntimeRepr`](praxis_repr::NoRuntimeRepr) into the `anyhow` error the
/// lowering already propagates, with the offending type rendered.
///
/// **Failing the compile is the decision** (D9). The predecessor had three
/// `_ => INT` arms, so `Float`, `Unit`, `Record`, `Enum`, a closure, a `Range`
/// and an unresolved variable all became the `Int` descriptor — and a record
/// schema built from them dispatched `Int`'s equality callback against an `f64`
/// payload (P0-11). Reaching a type with no runtime object at a
/// descriptor-producing site is an upstream compiler bug; refusing to emit is
/// how it stays visible instead of becoming a wrong payload read.
fn descriptor_for_type(
    db: &praxis_types::TypeDb,
    ty: praxis_types::Type,
) -> Result<*const praxis_runtime::descriptor::TypeDescriptor> {
    praxis_repr::descriptor_for_type(db, ty)
        .map(|d| d as *const _)
        .map_err(|e| {
            anyhow!(
                "cannot emit a runtime descriptor for `{}`: {}",
                db.render(ty),
                e.reason
            )
        })
}

/// The runtime descriptor for `ty` if it has one, and a *null* descriptor if it
/// does not.
///
/// Only for debug metadata, where absence is already representable and already
/// rendered: `MirType::Opaque` locals emit a null descriptor plus
/// `NO_STATIC_TYPE`, and the debugger omits the type column for both (P0-02).
/// A `Never`-typed local — the result of a `return` or a `panic()` — is the
/// common case, and refusing to compile a working program because its debug
/// info is incomplete is not what D9 decided.
///
/// This is *not* a fallback for a dispatch-producing site. There, absence has no
/// honest encoding and [`descriptor_for_type`] fails the compile.
fn debug_descriptor_for_type(
    db: &praxis_types::TypeDb,
    ty: praxis_types::Type,
) -> *const praxis_runtime::descriptor::TypeDescriptor {
    praxis_repr::descriptor_for_type(db, ty).map_or(std::ptr::null(), |d| d as *const _)
}

/// Resolve the element descriptor(s) for a collection's payload, for use at
/// construction (`praxis_<kind>_new`). Most collections carry one element
/// descriptor (Vec/Deque/Set/Heap/Grid); `Map[K,V]` and `Counter[T]` carry a
/// key descriptor (and Map also a value descriptor, passed as a second slot).
///
/// The descriptor is resolved recursively via [`descriptor_for_type`] so nested
/// collections (e.g. `Map[Vec[Int], Int]`) resolve the key descriptor to `VEC`,
/// making structural equality/hashing dispatch correctly on map keys (§11.3).
///
/// A *null* descriptor is the honest encoding of "this lowering has no static
/// element type", and every `praxis_*_new` wrapper reads it that way. Two
/// situations produce it, and neither is P0-11's fallback:
///
/// - `MirType::Opaque` — a fused pipeline's result Vec, which genuinely has no
///   type until MIR-05 (S21), or a construction whose result type did not match
///   its ctor.
/// - a `Known` type that is still an inference *variable* — `let xs = Vec()`
///   generalizes at the `let`, so the construction site's own element type is
///   never resolved. That is HIR-01/MONO-01 (S15), and failing the compile on it
///   would reject working programs, which hazard H10 exists to prevent.
///
/// A `Known` type that *cannot* have a runtime object — `Vec[Range]`,
/// `Vec[Never]` — is still a compile error. That distinction is what
/// [`praxis_repr::NoReprCause`] records.
fn collection_element_descriptor_for(
    db: &praxis_types::TypeDb,
    args: &[MirType],
    index: usize,
) -> Result<*const praxis_runtime::descriptor::TypeDescriptor> {
    let Some(MirType::Known(element_type)) = args.get(index).copied() else {
        return Ok(std::ptr::null());
    };
    match praxis_repr::descriptor_for_type(db, element_type) {
        Ok(d) => Ok(d as *const _),
        Err(e) if e.is_unresolved() => Ok(std::ptr::null()),
        Err(e) => Err(anyhow!(
            "collection type argument {index}: cannot emit a runtime descriptor for `{}`: {}",
            db.render(element_type),
            e.reason
        )),
    }
}

/// Embed a string literal in the generation arena and produce (ptr, len)
/// `iconst` values for `praxis_alloc_text`.
///
/// The bytes live exactly as long as the JIT generation that compiled the
/// literal, and are interned — a literal repeated across functions, or a
/// program recompiled into the same generation, costs one copy. This used to be
/// a `Box::leak` with a comment promising a `JitGeneration` arena "M-later";
/// that arena is [`Generation`] and this is its call site (MIR-13).
fn embed_text(
    builder: &mut FunctionBuilder,
    generation: &Generation,
    value: &str,
) -> (Value, Value) {
    let stored = generation.alloc_str(value);
    let ptr_val = stored.as_ptr() as i64;
    let len_val = stored.len() as i64;
    (
        builder.ins().iconst(GC, ptr_val),
        builder.ins().iconst(GC, len_val),
    )
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
    /// `praxis_pop_debug_frame` read a result register the callee never wrote.
    #[test]
    fn void_wrappers_declare_no_result() {
        let module = test_module();
        assert!(signature_for(RuntimeSymbol::PopDebugFrame, &module)
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

    /// P0-02: an `Opaque` MIR local produces *no* static type in the debug
    /// metadata. The old `Type(0)` placeholder was a valid arena index, so the
    /// crash debugger rendered every untyped temp as whichever type the program
    /// happened to intern first — usually `Int`.
    #[test]
    fn an_opaque_local_carries_neither_a_descriptor_nor_a_type_id() {
        use praxis_mir::{ir::LocalDebugKind, Function as MirFn};
        let mut db = praxis_types::TypeDb::new();
        let int = db.int();
        let mut f = MirFn {
            name: "f".into(),
            params: Vec::new(),
            return_local: LocalId(0),
            locals: Vec::new(),
            blocks: Vec::new(),
            debug_names: Vec::new(),
            debug_kinds: Vec::new(),
            debug_spans: Vec::new(),
            span: (0, 0),
        };
        f.new_local(
            LocalKind::Gc,
            MirType::Known(int),
            Some("n".into()),
            LocalDebugKind::User,
            None,
        );
        f.new_local(
            LocalKind::Gc,
            MirType::Opaque,
            None,
            LocalDebugKind::Temp,
            None,
        );

        let generation = Generation::new();
        let (ptr, len) = build_debug_local_metas(&f, &mut db, &generation);
        assert_eq!(len, 2);
        // SAFETY: `build_debug_local_metas` returns `len` initialized entries
        // owned by `generation`, which outlives this borrow.
        let metas = unsafe { std::slice::from_raw_parts(ptr, len) };
        assert!(
            !metas[0].descriptor.is_null(),
            "a Known local keeps its descriptor"
        );
        assert_eq!(
            metas[0].type_id,
            int.to_u32(),
            "a Known local keeps its type id"
        );
        assert!(
            metas[1].descriptor.is_null(),
            "an Opaque local has no descriptor to thread"
        );
        assert_eq!(
            metas[1].type_id,
            praxis_runtime::debug::NO_STATIC_TYPE,
            "an Opaque local must not name a type it does not have"
        );
    }

    /// H9's resolution in the backend: `Opaque` emits no descriptor. The
    /// wrappers read a null element descriptor as "unknown element type" and
    /// adopt the first inserted value's, which is what a fused pipeline's
    /// result Vec needs; a missing argument (a construction whose result type
    /// did not match its ctor) takes the same path instead of panicking.
    #[test]
    fn an_opaque_element_type_resolves_to_no_descriptor() {
        let mut db = praxis_types::TypeDb::new();
        let int = db.int();
        assert!(
            collection_element_descriptor_for(&db, &[MirType::Opaque], 0)
                .unwrap()
                .is_null()
        );
        assert!(collection_element_descriptor_for(&db, &[], 0)
            .unwrap()
            .is_null());
        assert!(core::ptr::eq(
            collection_element_descriptor_for(&db, &[MirType::Known(int)], 0).unwrap(),
            &praxis_runtime::scalars::INT
        ));
    }

    /// P0-11's other half at this boundary: an element type with no runtime
    /// object is a compile error, not a null descriptor. `Opaque` means "no
    /// static type here"; `Vec[Seq[Int]]` means "a type that cannot exist", and
    /// conflating the two is how the wrappers ended up adopting whatever was
    /// pushed first.
    ///
    /// **Rewritten** for TY-34: the offending element used to be `Range`, which
    /// had no runtime object at all. It has one now (ADR-059), so the type that
    /// cannot exist is `Seq` — the compiler's lazy pipeline source, which is
    /// fused away before codegen and never materializes (§6.3). The property is
    /// unchanged; only the witness moved, and `Seq` is now the *last* ctor with
    /// no runtime object, which is what makes it the right witness.
    #[test]
    fn a_known_element_type_with_no_descriptor_fails_the_compile() {
        let mut db = praxis_types::TypeDb::new();
        let int = db.int();
        let seq = db.unary_collection(praxis_types::CollectionCtor::Seq, int);
        let err = collection_element_descriptor_for(&db, &[MirType::Known(seq)], 0)
            .expect_err("Seq has no runtime object");
        assert!(
            err.to_string().contains("Seq"),
            "the diagnostic must name the offending type: {err}"
        );
    }

    /// …and the type that used to be that witness now *has* a descriptor, which
    /// is the half of TY-34 this boundary can see: a `Vec[Range]` compiles, and
    /// its element descriptor is the one `Range` object.
    #[test]
    fn a_range_element_has_the_range_descriptor() {
        let mut db = praxis_types::TypeDb::new();
        let range = db
            .collection(
                praxis_types::CollectionCtor::Range,
                praxis_types::CollectionArgs::Nullary,
            )
            .expect("Range is nullary");
        let desc = collection_element_descriptor_for(&db, &[MirType::Known(range)], 0)
            .expect("Range has a runtime object now");
        assert!(core::ptr::eq(desc, &praxis_runtime::range::RANGE));
    }

    /// A tuple allocation with no static type keeps its **arity** and leaves
    /// every slot unknown (REP-23, ADR-066 decision 5).
    ///
    /// This test asserted the opposite — an *empty* schema — and that assertion
    /// was the defect written down. `praxis_alloc_tuple` sizes the payload from
    /// the schema, so a zero-slot schema for a two-element tuple made both
    /// `praxis_tuple_set` calls write into nothing: `[10, 20].enumerate()`
    /// answered `[(), ()]`, with both halves of every pair dropped. The type gap
    /// is still MIR-05's (S21 supplies the real fused-pipeline tuple types); what
    /// changed is that "no static type" is now a slot the runtime answers from
    /// the value's own header rather than a slot that does not exist.
    #[test]
    fn an_opaque_tuple_type_keeps_its_arity_with_unknown_slots() {
        let db = praxis_types::TypeDb::new();
        let generation = Generation::new();
        let schema =
            tuple_schema_for(&db, MirType::Opaque, 2, &generation).expect("no elements to resolve");
        // SAFETY: the schema is owned by `generation`, which outlives the read.
        let schema = unsafe { &*schema };
        assert_eq!(schema.arity(), 2, "the payload is sized from this");
        assert!(
            schema.descriptors.iter().all(|d| d.is_null()),
            "and every slot says it has no static type"
        );
    }

    // -----------------------------------------------------------------------
    // ADR-102: the shapes the inlined sites emit.
    //
    // These read the emitted Cranelift IR, and they have to. "The instruction
    // is the fact": a behavioural test cannot tell an inline load from a call
    // to a wrapper that does the same load, so nothing that runs a program can
    // see the difference this change makes — or see it being undone. The one
    // that matters most is the descriptor check: it is the thing a later "the
    // type is known, drop the check" edit would remove, and REP-56 is what
    // that costs.
    // -----------------------------------------------------------------------

    /// A scratch function with one `i64` parameter (standing in for `ctx`) and
    /// one `Variable`, plus a `JITModule` to import wrappers into.
    ///
    /// Returns the emitted IR as text, and the entry block's own text — the
    /// split matters because "the entry block contains no `call`" is a
    /// different claim from "the function contains no `call`", and the second
    /// would be false by design: the cold blocks call.
    fn emitted_ir(
        build: impl FnOnce(&mut FunctionBuilder, Value, Variable, &mut JITModule) -> Result<()>,
    ) -> (String, String) {
        let mut module = test_module();
        let mut ctx = module.make_context();
        let mut sig = Signature::new(module.isa().default_call_conv());
        sig.params.push(AbiParam::new(GC));
        sig.returns.push(AbiParam::new(GC));
        ctx.func.signature = sig;

        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let ctx_val = builder.block_params(entry)[0];
        let dst = builder.declare_var(GC);

        build(&mut builder, ctx_val, dst, &mut module).expect("emission");

        // Terminate whatever block we ended up in, then seal and read back.
        let out = builder.use_var(dst);
        builder.ins().return_(&[out]);
        builder.seal_all_blocks();
        builder.finalize(module.isa().frontend_config());

        let all = ctx.func.display().to_string();
        let entry_text = block_text(&all, entry);
        (all, entry_text)
    }

    /// The lines of `ir` belonging to `block`, from its label up to the next
    /// block label.
    ///
    /// A header is one of `blockN:`, `blockN(v0: i64):` or `blockN cold:`, so
    /// the name is the leading run of non-`(`, non-space, non-`:` characters —
    /// matching the whole line would silently miss the cold ones, which are
    /// exactly the blocks two of these tests are about.
    fn block_text(ir: &str, block: Block) -> String {
        let label = format!("{block}");
        let mut out = String::new();
        let mut inside = false;
        for line in ir.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("block") && trimmed.ends_with(':') {
                let name: &str = trimmed.split(['(', ' ', ':']).next().unwrap_or("");
                inside = name == label;
                continue;
            }
            if inside {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    /// The one instruction in `ir` that reads at the payload displacement.
    ///
    /// The *width* of this line is the whole of REP-37, and it has to be picked
    /// out by displacement rather than by opcode: the descriptor check reads a
    /// pointer, so "the IR contains a `load.i64`" is true of every kind and
    /// says nothing. The payload sits at `payload_offset_for(align)`, which is
    /// 24 for all four scalars — the same number `Inst::EnumTag` folds — and
    /// Cranelift prints the displacement as `+24`.
    fn payload_load(ir: &str, align: usize) -> String {
        let displacement = format!("+{}", praxis_runtime::GcHeader::payload_offset_for(align));
        let mut hits = ir
            .lines()
            .filter(|l| l.contains(&displacement) && !l.trim_start().starts_with(';'));
        let line = hits
            .next()
            .unwrap_or_else(|| panic!("no instruction reads at {displacement}:\n{ir}"))
            .trim()
            .to_string();
        assert!(
            hits.next().is_none(),
            "more than one instruction reads at {displacement}:\n{ir}"
        );
        line
    }

    /// An `Int` extract loads the payload inline — and proves the descriptor
    /// first, in the same block, before the load can happen.
    ///
    /// The `icmp` is the assertion that matters. Dropping it would leave a
    /// green suite and a faster benchmark and REP-56 back: a `praxis check`-
    /// clean program extracting an `Int` from a zero-byte `Unit` payload, which
    /// in a release build read eight bytes past the object and answered a
    /// different number every run.
    #[test]
    fn an_int_extract_loads_the_payload_and_proves_its_descriptor() {
        let (all, entry) = emitted_ir(|b, ctx, dst, m| {
            let src = b.ins().iconst(GC, 0x1000);
            emit_scalar_load(
                b,
                ctx,
                src,
                dst,
                praxis_mir::ScalarKind::Int,
                m,
                &mut HashMap::new(),
            )
        });

        assert!(
            entry.contains("icmp eq"),
            "the descriptor check must be an ordinary compare in the hot block, \
             not something a profile can compile out (REP-56):\n{all}"
        );
        assert!(
            entry.contains("brif"),
            "and it must branch on the result:\n{all}"
        );
        assert!(
            !entry.contains("call "),
            "the fast path calls nothing; the wrapper is the cold block:\n{all}"
        );
        let payload = payload_load(
            &all,
            core::mem::align_of::<praxis_runtime::scalars::IntPayload>(),
        );
        assert!(
            payload.contains("load.i64"),
            "an Int payload is one eight-byte load, at the offset \
             `payload_offset_for` computes: {payload}\n{all}"
        );
        assert!(
            all.contains("call "),
            "and the cold path still calls the wrapper, which is what keeps the \
             refusal byte-for-byte what it was:\n{all}"
        );
    }

    /// REP-49 and REP-37, moved to the inline path: a `Bool` payload is one
    /// byte and a `Char`'s is four, and neither is read as eight.
    ///
    /// The `Bool` shape is three instructions rather than one on purpose — the
    /// byte is compared against zero rather than materialized as a Rust `bool`,
    /// reproducing `praxis_bool_load` rather than shortcutting it.
    #[test]
    fn a_bool_extract_reads_one_byte_and_a_char_four() {
        let shape = |scalar| {
            emitted_ir(move |b, ctx, dst, m| {
                let src = b.ins().iconst(GC, 0x1000);
                emit_scalar_load(b, ctx, src, dst, scalar, m, &mut HashMap::new())
            })
            .0
        };

        use praxis_runtime::scalars;

        let bools = shape(praxis_mir::ScalarKind::Bool);
        let read = payload_load(&bools, core::mem::align_of::<scalars::BoolPayload>());
        assert!(
            read.contains("uload8"),
            "a Bool payload is one byte, never an Int's eight: {read}\n{bools}"
        );
        assert!(
            bools.contains("icmp ne"),
            "and it is compared against zero, not materialized as a `bool`:\n{bools}"
        );

        let chars = shape(praxis_mir::ScalarKind::Char);
        let read = payload_load(&chars, core::mem::align_of::<scalars::CharPayload>());
        assert!(
            read.contains("uload32"),
            "a Char payload is four bytes, zero-extended: {read}\n{chars}"
        );

        // Float rides the uniform i64 channel as its bit pattern, which is
        // exactly what `praxis_float_load` returns.
        let floats = shape(praxis_mir::ScalarKind::Float);
        let read = payload_load(&floats, core::mem::align_of::<scalars::FloatPayload>());
        assert!(
            read.contains("load.i64"),
            "a Float payload is its eight-byte bit pattern: {read}\n{floats}"
        );
    }

    /// `ScalarKind::Byte` has no inline form, and that is not an oversight.
    #[test]
    fn a_reserved_byte_scalar_keeps_its_call() {
        assert!(
            inline_scalar_load_of(praxis_mir::ScalarKind::Byte).is_none(),
            "`Byte`'s load_symbol() is `IntLoad` — an eight-byte read of a \
             one-byte payload, chosen defensively while nothing emitted it. \
             Inlining that would be REP-37 by construction."
        );
        for wired in [
            praxis_mir::ScalarKind::Int,
            praxis_mir::ScalarKind::Bool,
            praxis_mir::ScalarKind::Char,
            praxis_mir::ScalarKind::Float,
        ] {
            assert!(
                inline_scalar_load_of(wired).is_some(),
                "{wired:?} is wired and must have an inline form"
            );
        }
    }

    /// The inline check proves **exactly** what the wrapper would prove.
    ///
    /// This is the seam the whole change rests on. The fast path reads the
    /// payload because a comparison said the descriptor is `D`; the cold path
    /// calls a wrapper that reads it because `read_scalar` said the descriptor
    /// is the one behind `scalars::…_PAYLOAD`. If those two descriptors were
    /// ever different, the site would have two contradictory notions of what
    /// this value is: the fast path could accept a value the wrapper refuses
    /// (an out-of-bounds read at the wrong width, REP-37) or refuse one it
    /// accepts (an abort on a correct program). Asserting the identity is what
    /// makes "the refusal is byte-for-byte what it was" a checked claim rather
    /// than a comment.
    #[test]
    fn the_inline_check_proves_exactly_what_the_wrapper_would() {
        use praxis_mir::ScalarKind;
        use praxis_runtime::scalars;
        for (kind, handle_descriptor) in [
            (ScalarKind::Int, scalars::INT_PAYLOAD.descriptor()),
            (ScalarKind::Bool, scalars::BOOL_PAYLOAD.descriptor()),
            (ScalarKind::Char, scalars::CHAR_PAYLOAD.descriptor()),
            (ScalarKind::Float, scalars::FLOAT_PAYLOAD.descriptor()),
        ] {
            let (inline_descriptor, align, _) =
                inline_scalar_load_of(kind).expect("a wired scalar has an inline form");
            assert!(
                core::ptr::eq(inline_descriptor, handle_descriptor),
                "{kind:?}: the inline check proves `{}` but the wrapper it falls \
                 back to proves `{}`",
                inline_descriptor.name,
                handle_descriptor.name
            );
            // And the width the fast path reads at is the descriptor's own, not
            // one this file picked: `payload_offset_for` places the payload from
            // the alignment, and the descriptor records the size.
            assert_eq!(
                align,
                inline_descriptor.align(),
                "{kind:?}: the payload offset is folded from an alignment that \
                 is not the descriptor's"
            );
        }
    }

    /// The overflow report is a branch to a cold block, not a call per op.
    #[test]
    fn an_overflow_report_is_a_branch_to_a_cold_block() {
        let mut module = test_module();
        let mut ctx = module.make_context();
        let mut sig = Signature::new(module.isa().default_call_conv());
        sig.params.push(AbiParam::new(GC));
        ctx.func.signature = sig;

        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let ctx_val = builder.block_params(entry)[0];
        let cond = builder.ins().iconst(types::I8, 0);
        raise_on_cold_path(
            &mut builder,
            ctx_val,
            cond,
            RuntimeSymbol::RaiseIntOverflowIf,
            &mut module,
            &mut HashMap::new(),
        )
        .expect("emission");
        builder.ins().return_(&[]);
        builder.seal_all_blocks();
        builder.finalize(module.isa().frontend_config());

        let all = ctx.func.display().to_string();
        let entry_text = block_text(&all, entry);
        assert!(entry_text.contains("brif"), "{all}");
        assert!(
            !entry_text.contains("call "),
            "the arithmetic site branches; it does not call on the hot path:\n{all}"
        );

        let cold: Vec<_> = ctx
            .func
            .layout
            .blocks()
            .filter(|&b| ctx.func.layout.is_cold(b))
            .collect();
        assert_eq!(cold.len(), 1, "exactly the raise block is cold:\n{all}");
        assert!(
            block_text(&all, cold[0]).contains("call "),
            "and the cold block is the one that calls the wrapper:\n{all}"
        );
    }

    /// A fault check is two loads and a branch: through `ctx.pending_fault`,
    /// then the kind, then `brif`. No call, and no null test on either — see
    /// the `Inst::CheckFault` arm for why neither is needed.
    #[test]
    fn a_fault_check_is_two_loads_and_a_branch() {
        let module = test_module();
        let mut ctx = module.make_context();
        let mut sig = Signature::new(module.isa().default_call_conv());
        sig.params.push(AbiParam::new(GC));
        ctx.func.signature = sig;

        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let ctx_val = builder.block_params(entry)[0];

        let flags = MemFlags::trusted();
        let slot = builder.ins().load(GC, flags, ctx_val, PENDING_FAULT_OFFSET);
        let pending = builder
            .ins()
            .load(types::I32, flags, slot, FAULT_KIND_OFFSET);
        let on_fault = builder.create_block();
        let fallthrough = builder.create_block();
        builder.ins().brif(pending, on_fault, &[], fallthrough, &[]);
        builder.switch_to_block(on_fault);
        builder.ins().return_(&[]);
        builder.switch_to_block(fallthrough);
        builder.ins().return_(&[]);
        builder.seal_all_blocks();
        builder.finalize(module.isa().frontend_config());

        let all = ctx.func.display().to_string();
        assert!(all.contains("load.i64"), "the slot pointer:\n{all}");
        assert!(
            all.contains("load.i32"),
            "the kind, at the width the runtime says it is:\n{all}"
        );
        assert!(all.contains("brif"), "{all}");
        assert!(
            !all.contains("call "),
            "reading one word must not cost a guarded call:\n{all}"
        );
        // The load flags must stay `notrap+aligned` and nothing more. `readonly`
        // would let Cranelift's alias analysis hoist the kind read across the
        // call that wrote it, collapsing two checks into one and losing a fault
        // — as an intermittent wrong answer, not a compile error.
        assert!(
            !all.contains("readonly") && !all.contains("can_move"),
            "the pending-fault load must not claim the memory is stable:\n{all}"
        );
    }
}
