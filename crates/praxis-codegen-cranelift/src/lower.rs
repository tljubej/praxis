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
    AllocKind, BlockId, CallTarget, CmpOp, FloatBinOp, Function as MirFunction, GcConst, Inst,
    IntBinOp, LocalId, LocalKind, MirType, Overflow, RootSlots, ScalarKind, Terminator,
};
use praxis_runtime::{
    descriptor::BuiltinTypeId, DebugFrameEntry, DebugLocalMeta, DebugSlotCount, DebugSlotKind,
    FunctionDebugMeta, RuntimeContext, ShadowStackHeader, SlotCount, MAX_DEBUG_VALUE_SLOTS,
    MAX_SHADOW_SLOTS,
};
use praxis_stdlib::abi::{AbiKind, AbiRet, RuntimeSymbol};

use crate::dump;
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

/// The byte offsets of the crash debugger's two slot-stack headers within a
/// `RuntimeContext` (ADR-104). The prologue claims from both and the epilogue
/// restores both, with the same three helpers the shadow stack uses — which is
/// the whole reason ADR-101 parameterised them by the context field.
const DEBUG_VALUES_OFFSET: i64 = core::mem::offset_of!(RuntimeContext, debug_values) as i64;
const DEBUG_FRAMES_OFFSET: i64 = core::mem::offset_of!(RuntimeContext, debug_frames) as i64;

/// The width of one debug value slot: one machine word, sized as an
/// `Option<GcRef>` because that is what the slot holds for every local whose
/// box survives compilation — the `NonNull` niche (F18) — and because a local
/// whose box ADR-120's forwarding elided stores its raw scalar payload into the
/// *same* slot at the same stride. Derived rather than written as `8` for the
/// reason `SHADOW_SLOT_BYTES` is.
///
/// Asserted equal to `SHADOW_SLOT_BYTES` because one [`slot_displacement`] serves
/// both stacks and [`emit_zero_slots`] writes one `GC`-typed word per slot on
/// either. **Not** because the two stacks share an index — they did until
/// ADR-128 decision 3, and the old wording of this assertion said so; the index
/// spaces are now separate and only the stride is common.
const DEBUG_VALUE_SLOT_BYTES: i64 = core::mem::size_of::<Option<praxis_runtime::GcRef>>() as i64;
const _: () = assert!(
    DEBUG_VALUE_SLOT_BYTES == SHADOW_SLOT_BYTES,
    "both slot stacks are one machine word per slot, which is what lets one \
     displacement helper and one zeroing emitter serve both"
);

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

/// Zero more than this many slots with an inline loop rather than a run of
/// stores. **This is a code-size budget, and nothing else** (ADR-128 decision 1).
///
/// It was 32, and that number was never measured: it appears in one commit
/// (`488330f`, ADR-101's handover) and in no ADR and no handover. Its doc comment
/// claimed a run of stores becomes *slower* than a call somewhere around 32
/// slots, which has nothing behind it — a 264-byte `memset` is a call (argument
/// setup, a branch to a stub, libc's own size dispatch) against 33 straight
/// stores with no branch at all, and ADR-101's own figure priced the old
/// prologue memset at ~32 ns per call.
///
/// So the threshold answers the one question that *is* real, and the measurements
/// say which question that is. **Below it, a straight run beats both
/// alternatives; above the widths any *hot* function reaches, nothing else can
/// be measured at all.**
///
/// Three shapes were A/B'd at `is_prime`'s 33-slot debug claim, colouring held
/// constant, `benchmarks/ab.py`:
///
/// | zeroing at 33 slots | against a straight run |
/// |---|---:|
/// | `%Memset` call | −0.3% ± 0.4% — unresolved |
/// | **straight run of word stores** | — |
/// | inline word loop | **−2.7% ± 0.3%** — resolved, and worse |
///
/// The loop is the *slowest* of the three at that width: 33 branches against 33
/// stores a superscalar core pipelines with no branch at all. So the ceiling must
/// sit above every hot function's width, and this one does — the widest claim in
/// any function called more than a handful of times is `tree`'s `build` at 45:
///
/// | function | calls per run | dense claim |
/// |---|---:|---:|
/// | `tree`'s `build` | 131,071 | **45** |
/// | `primes`' `is_prime` | 1.6 M | 33 |
/// | `tree`'s `walk` | 131,071 | 25 |
/// | `bfs`'s `open_cell` | ~40,000 | 14 |
/// | `pipeline`'s closures | 1 M | 7–12 |
/// | every `<entry>` (69–340) | **1** | — |
///
/// Everything wider than about 50 in this tree is an `<entry>`, claimed once per
/// program run, where the prologue's cost is unmeasurable and its *size* is the
/// only thing left to argue about. Hence a code-size budget, and hence 64: it
/// clears the widest hot claim with headroom and caps a prologue's zeroing at
///
/// | slots | inline stores | code, both stacks |
/// |---:|---:|---:|
/// | 45 (`build`, the widest hot one) | 90 | 360 B |
/// | 64 (this ceiling) | 128 | **512 B** |
/// | 185 (`vm`'s `<entry>`) | — | *loops* |
/// | 340 (`adr127_pipeline_over_every_iterable`'s `main`) | — | *loops* |
/// | 4096 (`MAX_DEBUG_VALUE_SLOTS`) | 8192 | 32 KB if it did not |
///
/// The last row is why "always unroll, no threshold" is not the answer. 256 was
/// the first value tried and is a **4× larger** code-size budget for no measured
/// gain: 256 against 64 over the whole suite is `1.001×`, every one of eight rows
/// unresolved, because nothing hot crosses either line.
///
/// Deliberately not `FunctionBuilder::emit_small_memset`, whose threshold is 4:
/// that would put a libc call in the prologue of any function with five `Gc`
/// locals, which is most of them, and the point of ADR-101 is that the common
/// prologue makes no calls at all. Since ADR-128 that sentence is true rather
/// than aspirational — see [`emit_zero_slots`], which no longer has a call in it
/// at any width.
///
/// Under `adr128-d1-arm-a` this is 32 again and [`emit_zero_slots`] calls
/// `%Memset` above it — the measurement arm, and the whole of that toggle.
#[cfg(not(feature = "adr128-d1-arm-a"))]
const SLOT_ZERO_UNROLL_MAX: u32 = 64;
#[cfg(feature = "adr128-d1-arm-a")]
const SLOT_ZERO_UNROLL_MAX: u32 = 32;

/// ADR-128 decision 1's toggle: zero a run wider than [`SLOT_ZERO_UNROLL_MAX`]
/// with a `%Memset` call, as ADR-101 left it, rather than with an inline loop.
///
/// A `const bool` and a runtime `if` rather than a `#[cfg]` block, which is the
/// idiom every other measurement arm in this file uses
/// ([`INLINE_COLLECTION_PRIMITIVES`], [`INLINE_SCALAR_CLAIM`]): the dead arm
/// still type-checks in both builds, so an edit cannot rot the arm nobody
/// compiles.
#[cfg(not(feature = "adr128-d1-arm-a"))]
const ZERO_SLOTS_WITH_MEMSET: bool = false;
#[cfg(feature = "adr128-d1-arm-a")]
const ZERO_SLOTS_WITH_MEMSET: bool = true;

/// ADR-128 decision 2's toggle: assign a shadow slot by the local's *position*
/// among `Gc` locals, as ADR-019 did, rather than by colouring the interference
/// relation. See [`RootSlotMap`].
#[cfg(not(feature = "adr128-d2-arm-a"))]
const POSITIONAL_ROOT_SLOTS: bool = false;
#[cfg(feature = "adr128-d2-arm-a")]
const POSITIONAL_ROOT_SLOTS: bool = true;

/// The byte offset of `stack_left` within a `RuntimeContext`. The prologue guard
/// reads it — *before* it pushes anything — to decide whether what is left of the
/// native-stack budget covers this frame, and branches to the stack-overflow
/// fault epilogue when it does not (§9.2, §17.4, ADR-105). Computed from the
/// `#[repr(C)]` layout, like `SHADOW_OFFSET`.
const STACK_LEFT_OFFSET: i64 = core::mem::offset_of!(RuntimeContext, stack_left) as i64;

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

/// The literal's table and the *runtime* value's table are the same table.
///
/// `Inst::ConstGc` reaches it through the offset above, computed here from the
/// public `#[repr(C)]` struct; `Inst::Materialize { Int }` reaches it through
/// [`praxis_runtime::small_int::INLINE_INTERN_SITE`], which the runtime mints
/// beside the range constants (ADR-113). Two spellings of one field is exactly
/// what `small_int`'s module doc forbids for the *bounds*, so it is asserted
/// rather than assumed for the *base*: a reorder of `RuntimeContext` that moved
/// one and not the other would put a literal `0` and a computed `0` in different
/// places, and nothing else in the tree would notice.
const _: () = assert!(
    SMALL_INTS_OFFSET as usize == praxis_runtime::small_int::INLINE_INTERN_SITE.table_offset(),
    "the interned-Int table has one base, and both readers must name it"
);

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

/// The displacement of `id`'s slot in `RuntimeContext.descriptors`, as a
/// Cranelift memory offset (ADR-116).
///
/// **This is the whole of what the backend knows about a descriptor now.** It
/// names a [`BuiltinTypeId`] — an enum discriminant, the same one
/// `BUILTINS[i]`'s registry is indexed by — and the address it proves against
/// is whatever the runtime wrote into that slot. Where the compiler used to
/// carry `&scalars::INT` across the ABI as an immediate, there is no descriptor
/// address in emitted code at all, so a compiler and a runtime cannot disagree
/// about one.
///
/// `RuntimeContext::descriptor_offset` is the authority for the arithmetic,
/// including the stride: computing `offset_of!(…, descriptors) + i * 8` here
/// would be a second statement of a layout that has an owner (ADR-039
/// decision 1's discipline, applied to the context).
// Arm A of ADR-116's measurement toggle emits the immediate instead, so it has
// no caller. Kept compiled rather than `#[cfg]`ed out so the toggle stays the
// two lines in `emit_scalar_load` that it claims to be.
#[cfg_attr(feature = "adr116-arm-a", allow(dead_code))]
fn descriptor_slot_offset(id: BuiltinTypeId) -> i32 {
    RuntimeContext::descriptor_offset(id) as i32
}

/// Every slot is within a Cranelift memory offset's reach, so the cast above is
/// total. The table's last slot bounds all 22 — `descriptor_offset` is affine
/// in the discriminant — and the context is a few hundred bytes, so this asserts
/// a property that is nowhere near its limit rather than one that might drift
/// into it. It exists because the alternative to a `const` assert is an
/// `expect` in the lowering for a state that cannot arise.
const _: () = assert!(
    RuntimeContext::descriptor_offset(BuiltinTypeId::Range) <= i32::MAX as usize,
    "a descriptor slot's displacement must fit a Cranelift memory offset"
);

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
    lower_function_capturing(module, fn_ctx, mir, user_funcs, db, generation, None)
}

/// [`lower_function`], with the defined function's Cranelift IR cloned into
/// `captured` when one is supplied — the post-`define_function` text, which at
/// the tree's `opt_level = "speed"` is the builder's output *after*
/// `Context::optimize`, egraph mid-end included (`dump::emit` has the list).
///
/// The compile path supplies none. This exists because `define_function` is the
/// last point at which the lowered function exists at all — `clear_context`
/// wipes `ctx.func` on the next line — so a test that wants to assert something
/// about a *whole* lowered function, rather than about one hand-driven emit
/// closure, has nothing to read afterwards. `dump::emit` answers the same
/// question for a human on stderr; this is the answer a test can hold.
fn lower_function_capturing<M: Module>(
    module: &mut M,
    fn_ctx: &mut FunctionBuilderContext,
    mir: &MirFunction,
    user_funcs: &HashMap<String, FuncId>,
    db: &mut praxis_types::TypeDb,
    generation: &Generation,
    captured: Option<&mut codegen::ir::Function>,
) -> Result<()> {
    // The ISA's pointer width and default call convention, needed by `finalize`
    // at the end and taken here because that point holds a borrow excluding
    // `module`. The prologue's slot zeroing still takes it, but only
    // `adr128-d1-arm-a` reads it: the `call_memset` ADR-128 decision 1 removed is
    // the one thing in this backend that ever needed a call convention.
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

    // The debugger's index space: one slot per `Gc` local, in MIR local order
    // (ADR-104, kept by ADR-128 decision 3). `Scalar` locals are transient and
    // must not survive a safepoint (the builder re-materializes them), so they
    // have no slot on either stack. The index is the local's position among `Gc`
    // locals, *not* its MIR LocalId — and it is the same walk
    // `build_function_debug_meta` makes, which is what lets a `DebugLocalMeta`
    // at index `i` describe the word at displacement `i`.
    //
    // **Dense on purpose.** The crash debugger must render a local the program
    // has finished with — that is the whole content of `DebugSlots` being
    // deliberately over-approximate — so these cannot be colored the way the root
    // slots below are, and `FunctionDebugMeta` resolves a slot to a name and a
    // type *statically*, once per function.
    let debug_slot: HashMap<LocalId, u32> = {
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
    let gc_count = debug_slot.len() as u32;

    // The collector's index space: a *color*, not a position (ADR-128 decision
    // 2). Two locals that are never live at one safepoint share a slot, and a
    // local that is live at no safepoint gets none at all.
    let root_slots = RootSlotMap::color(mir);

    // The inverse of `Function::debug_scalar_source` (ADR-120 part 2): for each
    // `Scalar` local that stands in for a box the forwarding elided, the debug
    // slots its definition must write.
    //
    // Inverted here because `store_debug_defs` is driven by "what does this
    // instruction define", and what it defines is the *scalar*; the MIR records
    // the link against the *box*, because that is the local that owns the slot
    // and the `symbol_id`. A `Vec` per scalar rather than one slot, because two
    // boxes of one scalar are representable (`Materialize` twice) and both slots
    // are then the same value — a fact worth writing down rather than an
    // `unwrap` waiting to be wrong.
    //
    // These are **debug** slots: the store they stand in for is
    // `store_debug_local`'s, into the debug value stack. Sorted because the map
    // they are read out of is a `HashMap`, and an unsorted `Vec` here would make
    // the *order of two stores* depend on hash seeding — same MIR in, different
    // CLIF out, which is what the snapshot suites and `PRAXIS_DUMP_CLIF` cannot
    // have.
    let elided_box_slots: HashMap<LocalId, Vec<u32>> = {
        let mut map: HashMap<LocalId, Vec<u32>> = HashMap::new();
        for (&boxed, &slot) in &debug_slot {
            if let Some((scalar, _)) = mir.debug_scalar_source(boxed) {
                map.entry(scalar).or_default().push(slot);
            }
        }
        for slots in map.values_mut() {
            slots.sort_unstable();
        }
        map
    };

    // The two widths, each checked against its own cap, and this is the only
    // place either can be: both count types are unconstructible above their
    // bound, and every consumer downstream — including the reservation-sizing
    // arguments in `SHADOW_STACK_SLOTS` and `DEBUG_VALUE_STACK_SLOTS` — assumes
    // the bound rather than re-checking it.
    //
    // The message names `Gc` locals in both cases because that is what a
    // programmer wrote; only the second is a limit they can realistically reach.
    let root_count = SlotCount::new(root_slots.width()).ok_or_else(|| {
        anyhow!(
            "function `{}` needs {} simultaneously-live Gc roots, exceeding \
             MAX_SHADOW_SLOTS ({MAX_SHADOW_SLOTS})",
            mir.name,
            root_slots.width(),
        )
    })?;
    let debug_count = DebugSlotCount::new(gc_count).ok_or_else(|| {
        anyhow!(
            "function `{}` has {gc_count} Gc locals, exceeding \
             MAX_DEBUG_VALUE_SLOTS ({MAX_DEBUG_VALUE_SLOTS}); split it",
            mir.name
        )
    })?;

    // `PRAXIS_DUMP_SLOTS`, which exists so that ADR-128's measurement table is
    // something a later reader re-runs rather than re-derives (dump::SLOTS_VAR).
    // Answered from the MIR and the two maps, before a single instruction is
    // emitted.
    //
    // The `wants_slots` test is outside the call and not inside it, which is the
    // whole difference between "one relaxed load and a branch per compiled
    // function" and "two more walks of every instruction in the program on every
    // `praxis run`" — Rust evaluates the argument first, so a guard inside
    // `emit_slots` would not be a guard at all. `wants_vcode` is placed the same
    // way and for the same reason.
    if dump::wants_slots(&mir.name) {
        dump::emit_slots(&mir.name, &slot_census(mir, &root_slots, gc_count));
    }

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

    // Native-stack budget guard (§9.2, §17.4, ADR-105), and it comes *first*.
    //
    // It used to sit after the shadow-frame push, because the push helper was
    // what bumped the counter and the guard read the result back. With the bump
    // inline there is no reason to push before deciding, and two reasons not to:
    // the over-limit path then pushes nothing, so it pops nothing (the third
    // `emit_pop_shadow_frame` call site is gone rather than rewritten), and
    // "every prologue guards before it pushes" is the premise that lets
    // `SHADOW_STACK_SLOTS` be sized so shadow-stack exhaustion is
    // unrepresentable.
    //
    // Two things about it are new (ADR-105).
    //
    // It spends this frame's *cost* — `FRAME_BYTES_BASE + 2 * slots` — and not a
    // flat 1. A call count is calibrated for one frame width, and the native
    // frames of real functions differ by a factor of three; counted as calls,
    // the widest legal frame aborted the host at a depth the narrowest one
    // survived, which is exactly the failure this guard exists to prevent.
    // The width is known here, so the cost folds to one immediate.
    //
    // **The width it spends is the dense count of `Gc` locals, not the colored
    // root count** (ADR-128 decision 4), and the temptation to charge the smaller
    // number is obvious and wrong. `FRAME_BYTES_PER_SLOT` is not rent on a shadow
    // slot; it is a calibrated proxy for the *native* frame. Charging the colored
    // count would under-report a function whose native frame is large, and the
    // failure mode of under-reporting is SIGABRT with no diagnostic — precisely
    // what ADR-105 was written to remove. Two more reasons: the dense count is
    // also the debug value stack's width, and *that* stack's reservation is
    // bounded by exactly this charge; and at `opt_level = "speed"` the native
    // frame already tracks live ranges, so charging the dense count is now
    // conservative. Erring high here is free; erring low is a crash.
    //
    // And it counts *down*, against a budget the context arrived carrying. The
    // limit is therefore not in generated code at all — `Runtime::context` is
    // the one place a stack size enters the system, and this function does not
    // need to know which stack it will run on.
    //
    // Without the guard, deep recursion (e.g. `count(100000)`) overflows the
    // native stack and the host aborts (SIGABRT); with it, the call faults
    // cleanly and unwinds to the host like any other fault.
    //
    // Block 0's actual instructions run in `body_entry` (a fresh block), so the
    // `entry` block ends with this conditional branch.
    let body_entry = builder.create_block();
    let over_limit = builder.create_block();
    // The budget this call found, saved so the epilogue can restore it rather
    // than add this frame's cost back. `entry` dominates every block, so this is
    // defined everywhere the epilogues can run.
    let saved_left_var = builder.declare_var(types::I32);
    // What this frame spends. One immediate, folded at compile time; also what
    // the frame-size audit after `define_function` checks Cranelift against.
    let this_frame_cost = praxis_runtime::frame_cost(debug_count.get());
    {
        // Load `(*ctx).stack_left` (u32) at its fixed `#[repr(C)]` offset.
        // `MemFlags::trusted()` is aligned + notrap: the context is live for the
        // whole call and the offset is in-bounds by construction.
        let left = builder.ins().load(
            types::I32,
            MemFlags::trusted(),
            ctx_val,
            STACK_LEFT_OFFSET as i32,
        );
        builder.def_var(saved_left_var, left);
        // Unsigned, and `<` rather than `<=`: a budget that exactly covers this
        // frame buys it. Comparing before subtracting is also what keeps the
        // arithmetic in range — the subtraction below only happens on the branch
        // that has just proved it cannot underflow.
        let exhausted =
            builder
                .ins()
                .icmp_imm_u(IntCC::UnsignedLessThan, left, i64::from(this_frame_cost));
        builder
            .ins()
            .brif(exhausted, over_limit, &[], body_entry, &[]);
    }

    // The stack-overflow fault epilogue: raise the fault, snapshot, and return
    // the Unit sentinel. Mirrors `Terminator::Fault` below, minus the pops —
    // guard-first means this path pushed neither the shadow frame nor the debug
    // frame, so this is the one `return_` in the function that is not preceded
    // by an epilogue, and it is exactly the one path that skipped the prologue.
    //
    // The snapshot taken here reflects the caller's chain — at whatever depth
    // exhausted the budget — rather than the overflowing frame's: one frame
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

    // Prologue: spend this call's share of the budget. Reached only from the
    // branch that proved `left >= cost`, so the subtraction cannot wrap.
    {
        let left = builder.use_var(saved_left_var);
        #[allow(deprecated)] // iadd_imm_s vs iadd_imm: a negative immediate is
        // what this actually means, and the signed form says so.
        let remaining = builder.ins().iadd_imm_s(left, -i64::from(this_frame_cost));
        builder.ins().store(
            MemFlags::trusted(),
            remaining,
            ctx_val,
            STACK_LEFT_OFFSET as i32,
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
    // non-empty, and a root set can be non-empty while `root_slot_of` yields
    // nothing (a `Scalar` local in the set — see the `continue` there), so a
    // `root_count == 0` special case would leave `frame_var` undefined on a path
    // that reads it and panic Cranelift. One code path is worth two wasted
    // instructions in a case that essentially does not occur — and since ADR-128
    // decision 2 that case is *common* rather than hypothetical: a function whose
    // locals are never live across a safepoint claims no root slots at all, which
    // is `is_prime`, `bfs`'s `open_cell` and every closure in `pipeline`.
    let frame_var = builder.declare_var(GC);
    {
        let base = emit_slot_stack_push(
            &mut builder,
            ctx_val,
            SHADOW_OFFSET,
            root_count.get(),
            SHADOW_SLOT_BYTES,
            frontend_config,
        );
        builder.def_var(frame_var, base);
    }

    // Prologue (cont.): claim this call's debug frame (§9.3, ADR-021, ADR-104).
    // This is what the crash debugger reads for `bt`/`locals`, and it is now two
    // more claims on two more slot stacks rather than two extern calls and two
    // to three mallocs:
    //
    //  - `debug_values_var` holds the base of this call's run of one word per
    //    `Gc` local, in the same order as `debug_slot` — which is
    //    `build_function_debug_meta`'s own walk, so a `DebugLocalMeta` at index
    //    `i` describes the word at displacement `i`. Zeroed on claim, which is
    //    what makes an unwritten slot render as `<uninit>` (F18). The word is a
    //    `GcRef` for every local whose box survives and a raw scalar payload for
    //    one whose box ADR-120's forwarding elided; the `DebugLocalMeta` beside
    //    the slot is what says which, so nothing here has to know.
    //
    //    **This claim is a different width from the shadow one above** since
    //    ADR-128 decision 3. ADR-104 built the two index-parallel "for free",
    //    because a local's shadow slot index doubled as its debug-local index;
    //    colouring the root slots by live range ends that, and the two spaces now
    //    answer different questions. What ADR-104 built is otherwise kept whole,
    //    `FunctionDebugMeta`'s layout included.
    //  - `debug_frame_var` holds this call's one `DebugFrameEntry`, which pairs
    //    the function's *static* `FunctionDebugMeta` with that base. Claimed
    //    without zeroing: both words are written immediately below, in
    //    straight-line code with nothing between, so the only reader — a fault
    //    epilogue's `praxis_snapshot_debug_chain`, far downstream — can never
    //    observe the gap.
    //
    // The whole of what used to be `praxis_push_debug_frame`'s four arguments
    // and `praxis_set_frame_source_span`'s two is the one immediate below.
    let debug_values_var = builder.declare_var(GC);
    let debug_frame_var = builder.declare_var(GC);
    {
        let values_base = emit_slot_stack_push(
            &mut builder,
            ctx_val,
            DEBUG_VALUES_OFFSET,
            debug_count.get(),
            DEBUG_VALUE_SLOT_BYTES,
            frontend_config,
        );
        builder.def_var(debug_values_var, values_base);

        let meta_ptr = build_function_debug_meta(mir, db, generation);
        // The one equality the runtime reads this claim under, and the reason
        // decision 3 is safe: `crash_snapshot::copy_stack` and
        // `DebugFrameStackHeader::clear_reclaimed` both walk
        // `0..meta.local_count` over *this* run of slots. A `local_count` larger
        // than the claim reads — and, in `clear_reclaimed`, writes — past it into
        // the next frame's slots. The two are built by two walks of `mir.locals`
        // with the same filter, so they agree; this is what says so out loud, and
        // it is the assertion decision 5 has to keep true when it starts denying
        // locals a slot.
        //
        // SAFETY: `build_function_debug_meta` just returned this pointer out of
        // the generation arena, which outlives the compilation.
        debug_assert_eq!(
            unsafe { (*meta_ptr).local_count },
            debug_count.get(),
            "`{}`'s debug metadata describes a different number of locals than \
             its frame claims slots for",
            mir.name
        );
        let entry = emit_slot_stack_claim(
            &mut builder,
            ctx_val,
            DEBUG_FRAMES_OFFSET,
            DebugFrameEntry::SIZE,
        );
        builder.def_var(debug_frame_var, entry);
        let meta_val = builder.ins().iconst(GC, meta_ptr as i64);
        let flags = MemFlags::trusted();
        builder
            .ins()
            .store(flags, meta_val, entry, DebugFrameEntry::META_OFFSET);
        builder
            .ins()
            .store(flags, values_base, entry, DebugFrameEntry::VALUES_OFFSET);
    }

    let spill = SpillCtx {
        frame_var,
        saved_left_var,
        debug_values_var,
        debug_frame_var,
        root_slot_of: &root_slots,
        debug_slot_of: &debug_slot,
        elided_box_slots: &elided_box_slots,
    };

    // Prologue (cont.): the parameters are the one set of `Gc` locals no
    // instruction defines, so nothing in the block loop below would store them.
    // They are defined by `def_var` in the entry block above, which dominates
    // everything, so the store belongs here — after the frame exists.
    for &param_local in &mir.params {
        spill.store_debug_local(&mut builder, param_local, &vars);
    }

    // Lower each block. Blocks are sealed together after the whole CFG is built
    // so loop backedges resolve correctly. Block 0's body continues in
    // `body_entry` (the recursion guard's fall-through target, which the
    // prologue above is already filling), not the param-receiving `entry` block.
    for (blk_idx, mir_block) in mir.blocks.iter().enumerate() {
        let block = blocks[blk_idx];
        if blk_idx != 0 {
            builder.switch_to_block(block);
        }
        for step in steps(&mir_block.insts) {
            match step.kind {
                StepKind::Lone => lower_inst(
                    &mut builder,
                    &step.insts[0],
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
                )?,
                // The fused pair (ADR-117): the raise's cold blocks jump to the
                // fault epilogue and the `Inst::CheckFault` at `step.insts[1]`
                // is never handed to `lower_inst`, so it emits nothing. It is
                // still covered by the step, which is what keeps the debugger's
                // store below a walk over every instruction of the block.
                StepKind::RaiseIntoFault {
                    op,
                    dst,
                    lhs,
                    rhs,
                    on_fault,
                } => lower_int_binop(
                    &mut builder,
                    op,
                    dst,
                    lhs,
                    rhs,
                    OverflowReport::Checked(RaiseExit::Folded(blocks[on_fault.0 as usize])),
                    ctx_val,
                    &vars,
                    module,
                    &mut import_cache,
                )?,
            }
            // The debugger's view is written *here*, once per definition, not
            // at every safepoint over the whole visible set (ADR-104). See
            // `SpillCtx::store_debug_defs` for why the two produce the same
            // slot contents at every point a snapshot can be taken.
            //
            // Over the step's instructions rather than one, so a fused pair is
            // not a pair of instructions one of which nobody asked about. It
            // emits nothing either way today: a `CheckFault` defines no local,
            // and an `IntBinOp`'s `dst` is a `Scalar` local, which has no debug
            // slot. If it ever did, folding would move that store off the
            // raising path — which is the direction ADR-104 already argues for:
            // the faulting operation's result was never produced, so `<uninit>`
            // is the honest rendering and the converging shape stored the
            // wrapped value instead.
            for inst in step.insts {
                spill.store_debug_defs(&mut builder, inst, &vars);
            }
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
    // The disassembly has to be *asked for* before compilation: `define_function`
    // hands `want_disasm` straight to the ISA and there is no way to request it
    // afterwards. `dump::wants_vcode` and `dump::emit` read the same field, so
    // they cannot disagree about whether it was asked for.
    ctx.set_disasm(dump::wants_vcode(&mir.name));
    module.define_function(id, &mut ctx)?;
    audit_frame_cost(&ctx, &mir.name, this_frame_cost);
    // Between here and `clear_context` is the only window in which the lowered
    // function exists, and the next line drops it. What `define_function` did to
    // `ctx.func` on the way through *is* optimization: at `opt_level = "speed"`
    // (module::CRANELIFT_FLAGS) `Context::optimize` runs unreachable-block
    // elimination, constant-block-parameter removal, alias resolution **and the
    // egraph mid-end**. This comment said the mid-end was gated off, which was
    // true until the fifth measurement in `module.rs` moved the flag; ADR-128's
    // aside is what caught it. See `dump::emit` for the whole of it, and for why
    // a package quoting an instruction count off this text has to care.
    dump::emit(&mir.name, &ctx);
    if let Some(out) = captured {
        *out = ctx.func.clone();
    }
    module.clear_context(&mut ctx);
    Ok(())
}

/// Check the prologue's byte model against the frame Cranelift actually laid
/// out (ADR-105).
///
/// `frame_cost` is a *measurement* — `112 + 2 × slots` bytes, fitted by
/// bisecting the abort depth of recursive programs under `ulimit -s` and rounded
/// up. Measurements go stale: a Cranelift upgrade, a new target, or a lowering
/// that spills more can widen the real frame past what the guard charged for it,
/// and the symptom would be the SIGABRT this whole change exists to remove —
/// appearing years later, in a build nobody connected to a codegen bump.
///
/// So the model is not trusted, it is audited. Cranelift knows the exact frame
/// size once it has compiled the function, and this compares the two on every
/// function of every program a debug build compiles — which is the entire test
/// suite. A `debug_assert` rather than a hard error because the release compiler
/// should not pay for it and because the charge is deliberately generous: being
/// *over* is the safe direction and the assert only fires when it is under.
///
/// `MachBufferFrameLayout::frame_to_fp_offset` is Cranelift's own words for
/// "offset from bottom of frame to FP (near top of frame)" — so it covers the
/// clobber saves, the spill slots and the outgoing arguments, which is
/// everything that varies with the function. What sits *above* FP is the setup
/// area: the return address and the caller's saved frame pointer, one word each
/// on both targets this backend supports.
fn audit_frame_cost(ctx: &codegen::Context, name: &str, charged: u32) {
    // Cheap in release (the whole body compiles away), so no `cfg` needed.
    if !cfg!(debug_assertions) {
        return;
    }
    /// The return address and the saved frame pointer, which `frame_to_fp_offset`
    /// measures *up to* rather than including.
    const SETUP_AREA_BYTES: u32 = 16;
    let Some(layout) = ctx
        .compiled_code()
        .and_then(|cc| cc.buffer.frame_layout().cloned())
    else {
        // No layout means no claim to check. Not an error: a target or a
        // Cranelift version that does not publish one leaves the model
        // unaudited, which is where it started.
        return;
    };
    let actual = layout.frame_to_fp_offset.saturating_add(SETUP_AREA_BYTES);
    debug_assert!(
        actual <= charged,
        "`{name}`'s native frame is {actual} bytes but its prologue only spends \
         {charged} of the stack budget, so deep recursion through it would \
         exhaust the native stack before the guard fires — which is the abort \
         ADR-105 removed. Raise FRAME_BYTES_BASE / FRAME_BYTES_PER_SLOT to fit \
         the frames this backend now emits."
    );
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
    count: u32,
    slot_bytes: i64,
    cfg: TargetFrontendConfig,
) -> Value {
    let base = emit_slot_stack_claim(
        builder,
        ctx_val,
        ctx_field_offset,
        i64::from(count) * slot_bytes,
    );
    emit_zero_slots(builder, base, count, slot_bytes, cfg);
    base
}

/// Claim `bytes` from the slot stack at `ctx + ctx_field_offset` **without
/// zeroing them**, and answer the base of the claimed run.
///
/// The half of [`emit_slot_stack_push`] that moves the cursor. Separated because
/// a caller that writes every byte it claims, in straight-line code, before
/// anything can read them does not need the zeroing and should not pay for it —
/// the debug frame entry (ADR-104) is the one such caller. Zeroing exists so
/// that a slot the function has *not* written yet reads as "nothing here"; a
/// run with no such slot has nothing to say.
fn emit_slot_stack_claim(
    builder: &mut FunctionBuilder,
    ctx_val: Value,
    ctx_field_offset: i64,
    bytes: i64,
) -> Value {
    // Aligned + non-trapping: the header and the reservation are owned by the
    // runtime and live for as long as the context is.
    let flags = MemFlags::trusted();
    let header = builder
        .ins()
        .load(GC, flags, ctx_val, ctx_field_offset as i32);
    let base = builder.ins().load(GC, flags, header, SLOT_STACK_TOP_OFFSET);
    #[allow(deprecated)] // iadd_imm_s vs iadd_imm: a small positive immediate.
    let new_top = builder.ins().iadd_imm_s(base, bytes);
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
/// Carrying it would keep a second value live across the whole body, which at
/// `opt_level = "none"` — the level this was decided at — meant a native stack
/// slot in every frame, and the native frame is the budget deep recursion
/// actually runs out of.
///
/// **That premise is stale and the conclusion is kept on a different one**
/// (ADR-128's aside). The tree has been at `"speed"` since the fifth measurement
/// in `module.rs`, where the register allocator assigns stack slots by live range
/// and would not necessarily spend one on this. What justifies the reload now is
/// the second half of the old sentence, which never depended on the level: it is
/// an L1 hit off a pointer that is live regardless, against a value that would
/// otherwise be live across every call in the body. Re-measuring it means
/// comparing the `sub sp, sp, #N` immediate in a `PRAXIS_DUMP_VCODE` prologue
/// with and without the threading — no benchmark and no clock — and it is not
/// this record's to spend.
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
/// of finding §3.3: ADR-019 zeroed all `MAX_SHADOW_SLOTS` on every call. The
/// crash debugger's value slots (ADR-104) are zeroed for the neighbouring
/// reason: an unwritten slot must read `None` so `locals` renders `<uninit>`
/// rather than a value from a deeper call that has since returned.
///
/// # This run is never a `memset`, and cannot be one
///
/// It used to call one above [`SLOT_ZERO_UNROLL_MAX`], and that was the wrong
/// shape rather than a wrong threshold (ADR-128 decision 1). `memset`'s cost is
/// mostly *generality*: dispatch on a byte count that may be anything, fix-ups
/// for a ragged head and tail, size-class branches. Every one of those cases is
/// unreachable here. This run is always a whole number of slots, contiguous, and
/// starting word-aligned — a slot is one machine word on both stacks, which is
/// not an assumption added here but the one the assertion below already makes.
///
/// A run of word stores at word-aligned addresses needs none of `memset`'s
/// machinery and needs nothing arranged to be correct: the element type's
/// alignment already covers the access, so `MemFlags::trusted()`'s `aligned` bit
/// stays honest with no work. So above the ceiling this emits an inline **loop**
/// of the same word stores — bounded code, no libc, at any width. That is what
/// makes ADR-101's "the common prologue makes no calls at all" true of *every*
/// prologue rather than of the common one.
///
/// **The small case is untouched, deliberately.** For a four-slot frame the four
/// stores are already optimal and a loop — counter setup, a branch per iteration
/// — would regress nearly every function in the language.
///
/// Widths are computed in *words* rather than in the literal 8: the figures in
/// [`SLOT_ZERO_UNROLL_MAX`] are the 64-bit targets in play, and nothing in
/// ADR-128 should be the reason a 32-bit port is a rewrite.
///
/// # Panics
/// If a slot is not one machine word wide. Both paths below write one `GC`-typed
/// zero per slot, so a wider slot would leave its tail untouched — silently, and
/// only for the slots a function never writes. Both instantiations are
/// pointer-width today; a third that is not must extend this rather than discover
/// it in a profile.
fn emit_zero_slots(
    builder: &mut FunctionBuilder,
    base: Value,
    count: u32,
    slot_bytes: i64,
    // Read only on `adr128-d1-arm-a`'s `call_memset` path, which is compiled in
    // both arms (a `const bool` and a runtime `if`, not a `#[cfg]`), so this is
    // a live use either way and needs no `allow`.
    cfg: TargetFrontendConfig,
) {
    assert_eq!(
        slot_bytes,
        i64::from(GC.bytes()),
        "emit_zero_slots writes one word per slot"
    );
    let n = count;
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

    if ZERO_SLOTS_WITH_MEMSET {
        // ADR-128 decision 1 arm A: the `%Memset` call this decision removed,
        // restored for the measurement and for nothing else. The zero constant
        // is minted inside each arm rather than hoisted above them, so this path
        // emits exactly what the pre-ADR-128 tree emitted and the arm is a
        // measurement of the call rather than of the call plus a dead `iconst`.
        let ch = builder.ins().iconst(types::I8, 0);
        let size = builder
            .ins()
            .iconst(cfg.pointer_type(), i64::from(n) * slot_bytes);
        builder.call_memset(cfg, base, ch, size);
        return;
    }

    let zero = builder.ins().iconst(GC, 0);

    // The loop, for the widths a straight run would make kilobytes of prologue.
    //
    // Shaped as a do-while over the *cursor* rather than over an index: `n >= 1`
    // is established above, so the body runs before the test and there is no
    // entry guard to emit. The block parameter is the cursor, which is the only
    // value that changes across the back edge — Cranelift's SSA builder gets one
    // phi and the register allocator one register.
    //
    // `end` is `base + n*slot_bytes`, the same one-past-the-end address
    // `emit_slot_stack_claim` already stored back as the stack's `top`. It is
    // recomputed here rather than threaded from there because the two are one
    // `iadd_imm_s` of a folded immediate, and threading it would keep a second
    // value live across the loop.
    let body = builder.create_block();
    let done = builder.create_block();
    let cursor = builder.append_block_param(body, GC);
    #[allow(deprecated)] // iadd_imm_s vs iadd_imm: a positive immediate, and the
    // signed form is what the width arithmetic actually produces.
    let end = builder.ins().iadd_imm_s(base, i64::from(n) * slot_bytes);
    builder.ins().jump(body, &[base.into()]);

    builder.switch_to_block(body);
    builder.ins().store(MemFlags::trusted(), zero, cursor, 0);
    #[allow(deprecated)] // as above: one slot forward.
    let next = builder.ins().iadd_imm_s(cursor, slot_bytes);
    let more = builder.ins().icmp(IntCC::UnsignedLessThan, next, end);
    builder.ins().brif(more, body, &[next.into()], done, &[]);

    builder.switch_to_block(done);
}

/// A `Gc` local's shadow-slot index, which is a **colour and not a position**
/// (ADR-128 decision 2).
///
/// # What the colouring is
///
/// Two `Gc` locals *interfere* when they both appear in one safepoint's
/// [`RootSlots::live`] — that is, when the collector may have to see both at
/// once. Locals that never interfere may share one slot, and a local that
/// appears in no live set needs no slot at all. The frame's width is the number
/// of colours used, and that is the count that becomes the [`SlotCount`], the
/// shadow claim and the run the prologue zeroes.
///
/// Everything a call pays scales with the width it *declares*: the prologue
/// zeroes it, `frame_cost` charges for it, and `push_roots` walks it at every
/// collection, since the scan is one linear pass over `[base, top)` and a null
/// slot is skipped but still read. Before this, the width was the count of `Gc`
/// locals — 33 in `primes`'s `is_prime`, whose largest co-live root set is
/// **one**. Over all 71 functions of `tests/aoc-corpus` the widest frame was 110
/// and the largest co-live root set is 11, which is `REFERENCE_FRAME_SLOTS`.
///
/// # Why it is sound
///
/// **A shadow slot is write-only from generated code.** The collector is
/// non-moving, so nothing is ever loaded back out of a slot: the backend spills
/// before a safepoint and re-reads its Cranelift `Variable` afterwards, and
/// [`SpillCtx::spill_roots`] is the only writer. So two locals sharing a slot
/// have no reload hazard to create — whichever of them is live at a safepoint
/// writes the slot on the way in, and the other is by construction not live
/// there.
///
/// The same observation, one layer down: at `opt_level = "speed"` Cranelift's
/// register allocator already assigns *native* stack slots by live range, on
/// these same locals. The shadow stack was the last array in the pipeline handed
/// out by name.
///
/// **The second premise, which is Cranelift's and not ours: there is no
/// dead-store elimination.** Sharing a slot creates store→store pairs to one
/// address with no load between them — a spill of `a` at one safepoint, a spill
/// of `b` into the same slot at the next — and a compiler that eliminated the
/// first as dead would delete a root the collector needed to see in between. It
/// is not a hazard today, and by Cranelift's own account not soon:
/// `cranelift-codegen`'s `alias_analysis.rs` says of dead-store elimination
/// "Because this is so complex, and the conditions for doing it correctly when
/// post-trap state must be correct likely reduce the potential benefit, we don't
/// yet do this." Written down here rather than assumed, because a Cranelift
/// upgrade that added it would drop roots silently and no test in the tree would
/// catch it.
///
/// # Why the map is a type
///
/// The property "no two locals of one live set share an index" is the one whose
/// violation is a silent wrong answer from the collector — a root written into a
/// slot another live root then overwrites, so the collector never sees it, so it
/// is swept while reachable. That is not a comment's job. [`RootSlotMap::color`]
/// is the only constructor, it *derives* the assignment from the live sets
/// rather than accepting one, and it re-checks the result against those same
/// sets. Every consumer downstream assumes the property, the way `SlotCount`'s
/// bound is assumed.
struct RootSlotMap {
    /// Only the locals that need a slot. A `Gc` local live at no safepoint is
    /// absent, which is the difference between this and the debugger's dense map.
    of: HashMap<LocalId, u32>,
    /// The number of colours used, which is this frame's shadow width.
    width: u32,
}

impl RootSlotMap {
    /// Colour `mir`'s `Gc` locals by the interference relation its safepoints
    /// define.
    ///
    /// Degree-ordered greedy, which is sufficient: over every function measured
    /// for ADR-128 it reached the largest co-live set exactly, so nothing here
    /// needs an optimal colourer.
    ///
    /// **Deterministic — same MIR in, same slots out.** `PRAXIS_DUMP_CLIF` output
    /// and the snapshot suites depend on it, so every ordering below is by
    /// `LocalId` or by degree with `LocalId` breaking the tie, and nothing
    /// iterates a `HashMap`.
    fn color(mir: &MirFunction) -> RootSlotMap {
        use std::collections::{BTreeMap, BTreeSet};

        if POSITIONAL_ROOT_SLOTS {
            // ADR-128 decision 2 arm A: the positional map this replaced — one
            // slot per `Gc` local, the local's position among them, which is
            // also the debugger's index space. That the two then coincide is the
            // whole of what the arm reverts; decisions 3 and 4 still hold,
            // because holding them constant is what makes this a measurement of
            // the colouring and of nothing else.
            let mut idx = 0u32;
            let of: HashMap<LocalId, u32> = mir
                .locals
                .iter()
                .filter(|l| l.kind == LocalKind::Gc)
                .map(|l| {
                    let i = idx;
                    idx += 1;
                    (l.id, i)
                })
                .collect();
            return RootSlotMap { of, width: idx };
        }

        // Only `Gc` locals get a slot, and this filter is the map's own rather
        // than another crate's. The positional map this replaces filtered
        // `mir.locals` by `LocalKind::Gc` and so could not answer for a `Scalar`
        // local at all; a map built from the *root sets* could, because
        // `RootSlots::live` is a list of locals and nothing in its type says
        // they are `Gc`. That they are is `VerifyError::RootIsNotGc`'s job, in
        // `praxis-mir` — and a backend that stored a raw scalar payload into the
        // collector's scan region because a verifier in another crate stopped
        // running is not a backend that is safe on its own terms.
        let is_gc = |l: LocalId| {
            mir.locals
                .get(l.0 as usize)
                .is_some_and(|loc| loc.kind == LocalKind::Gc)
        };

        // The interference graph, over exactly the locals some safepoint roots.
        // `BTreeMap`/`BTreeSet` throughout: this is the input to a greedy
        // assignment whose answer depends on the order it visits.
        let mut adj: BTreeMap<LocalId, BTreeSet<LocalId>> = BTreeMap::new();
        let live_sets: Vec<Vec<LocalId>> = mir
            .blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .filter_map(praxis_mir::roots_of)
            .map(|r| r.live().iter().copied().filter(|&l| is_gc(l)).collect())
            .filter(|live: &Vec<LocalId>| !live.is_empty())
            .collect();
        for live in &live_sets {
            for &a in live {
                let entry = adj.entry(a).or_default();
                for &b in live {
                    if a != b {
                        entry.insert(b);
                    }
                }
            }
        }

        // Degree-ordered greedy: colour the most constrained local first, so the
        // colours it forces onto its neighbours are chosen while the most
        // choices remain. Ties break on `LocalId`, ascending.
        let mut order: Vec<LocalId> = adj.keys().copied().collect();
        order.sort_by_key(|l| (std::cmp::Reverse(adj[l].len()), *l));

        let mut of: HashMap<LocalId, u32> = HashMap::with_capacity(order.len());
        let mut width = 0u32;
        for local in order {
            // The smallest colour no already-coloured neighbour holds.
            let taken: BTreeSet<u32> = adj[&local]
                .iter()
                .filter_map(|n| of.get(n).copied())
                .collect();
            let color = (0u32..).find(|c| !taken.contains(c)).unwrap_or(0);
            width = width.max(color + 1);
            of.insert(local, color);
        }

        let map = RootSlotMap { of, width };
        map.debug_assert_disjoint(&live_sets);
        map
    }

    /// Re-check the colouring against the live sets it was derived from.
    ///
    /// Correct by construction, and checked anyway: the cost of being wrong here
    /// is a root the collector cannot see, which presents as a use-after-free in
    /// an unrelated part of a program that ran for a while first. `debug_assert`
    /// rather than `assert` because this is `O(Σ |live|²)` over every safepoint
    /// and the release compiler is on the critical path of every `praxis run`.
    fn debug_assert_disjoint(&self, live_sets: &[Vec<LocalId>]) {
        debug_assert!(
            live_sets.iter().all(|live| {
                let mut seen: Vec<u32> = live.iter().filter_map(|&l| self.get(l)).collect();
                let n = seen.len();
                seen.sort_unstable();
                seen.dedup();
                seen.len() == n && n == live.len()
            }),
            "two locals live at one safepoint were given the same shadow slot, \
             or a live root was given none: the collector would see only one of \
             them, and sweep the other while it is still reachable"
        );
    }

    /// This local's slot, if some safepoint roots it.
    fn get(&self, local: LocalId) -> Option<u32> {
        self.of.get(&local).copied()
    }

    /// The number of slots this frame claims.
    fn width(&self) -> u32 {
        self.width
    }
}

/// Count the columns of ADR-128's measurement table for one function.
///
/// Everything here is read off the MIR and the colouring, so it is what the
/// backend *will* claim rather than a second estimate of it: `dense` and
/// `colored` are the two `emit_slot_stack_push` widths verbatim.
fn slot_census(mir: &MirFunction, roots: &RootSlotMap, dense: u32) -> dump::SlotCensus {
    let mut live_max = 0u32;
    let mut debug_visible_max = 0u32;
    for inst in mir.blocks.iter().flat_map(|b| &b.insts) {
        let (r, d) = praxis_mir::slot_sets(inst);
        if let Some(r) = r {
            live_max = live_max.max(r.live().len() as u32);
        }
        if let Some(d) = d {
            debug_visible_max = debug_visible_max.max(d.visible().len() as u32);
        }
    }
    // Decision 5's candidate set: the `Gc` locals the debugger can say least
    // about — no source name, no span. Counted rather than acted on; ADR-128
    // keeps that decision separate and gated on the snapshot suites.
    let nameless: Vec<LocalId> = mir
        .locals
        .iter()
        .filter(|l| l.kind == LocalKind::Gc)
        .filter(|l| mir.debug_name(l.id).is_none() && mir.debug_span(l.id).is_none())
        .map(|l| l.id)
        .collect();

    // And the subset that passes decision 5's own gate. A local whose debug slot
    // nothing ever stores into reads `None` forever, and `render_frame_locals`
    // drops a temp with neither a value nor a span — so it is invisible today and
    // dropping it cannot lose a line. Three ways a slot gets written, and a
    // candidate must be excluded by all three: an instruction defines the local,
    // it is a parameter (the prologue stores those), or it is an elided box whose
    // scalar's definition writes its slot (ADR-120 part 2).
    let mut written: std::collections::HashSet<LocalId> = mir.params.iter().copied().collect();
    for inst in mir.blocks.iter().flat_map(|b| &b.insts) {
        written.extend(praxis_mir::defs(inst));
    }
    let unrenderable = nameless
        .iter()
        .filter(|&&l| !written.contains(&l))
        .filter(|&&l| mir.debug_scalar_source(l).is_none())
        .count() as u32;

    dump::SlotCensus {
        dense,
        colored: roots.width(),
        live_max,
        debug_visible_max,
        nameless: nameless.len() as u32,
        unrenderable,
    }
}

/// The spill context handed to every instruction/terminator lowering: the
/// Variables the prologue defined, and the two Gc-local → slot-index maps.
///
/// **Two writers, not one** (MIR-16). There used to be a single `emit_spill`
/// writing one root list into both frames, which is why the two frames could
/// not disagree — and why making the GC root set exact would have silently
/// emptied the debugger's view. [`SpillCtx::spill_roots`] serves the collector
/// and takes the exact [`RootSlots`] at each safepoint;
/// [`SpillCtx::store_debug_defs`] serves the crash debugger and runs at every
/// *definition*, which is where the over-approximate [`DebugSlots`] contract is
/// actually realized (ADR-104).
struct SpillCtx<'a> {
    /// The base of this frame's run of slots inside the one contiguous shadow
    /// stack (ADR-101) — not a frame object. The spill indexes it directly, and
    /// the epilogue stores it back as the stack's `top`.
    frame_var: Variable,
    /// `ctx.stack_left` as this call found it. The epilogue stores it back
    /// rather than adding this frame's cost back on (ADR-105).
    saved_left_var: Variable,
    /// The base of this call's run of slots inside the contiguous debug value
    /// stack (ADR-104). Written only by [`SpillCtx::store_debug_local`], and
    /// stored back as that stack's `top` by the epilogue.
    debug_values_var: Variable,
    /// This call's one `DebugFrameEntry` inside the contiguous debug frame
    /// stack, written in the prologue and stored back as that stack's `top` by
    /// the epilogue.
    debug_frame_var: Variable,
    /// The collector's index space: a `Gc` local's *color* under ADR-128
    /// decision 2's interference colouring, feeding [`SpillCtx::spill_roots`] and
    /// the shadow claim. A local live at no safepoint is absent from it.
    root_slot_of: &'a RootSlotMap,
    /// The debugger's index space: a `Gc` local's position among `Gc` locals,
    /// feeding [`SpillCtx::store_debug_local`], [`SpillCtx::store_debug_defs`],
    /// `elided_box_slots` and the debug value claim. Every `Gc` local is in it.
    ///
    /// **Two maps, not one, and no consumer needs to know there was ever one.**
    /// Mixing them is the mistake that would put a local's value in another
    /// local's display, or root the wrong slot; each store site below names the
    /// one it means.
    debug_slot_of: &'a HashMap<LocalId, u32>,
    /// The debug slots each `Scalar` local must write because the box that
    /// used to write them is gone (ADR-120 part 2). Empty for every function
    /// the forwarding pass did not touch, which is what makes this cost nothing
    /// where it buys nothing.
    elided_box_slots: &'a HashMap<LocalId, Vec<u32>>,
}

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
    ///
    /// ### The one subtlety colouring introduces
    ///
    /// [`RootSlots::dead`] is a set of **locals** whose slots may be stale
    /// (MIR-01). Translated naively to slots it is wrong: under ADR-128 decision
    /// 2 a dead local may share a slot with a live one, and nulling it would null
    /// the live one's value — a root the collector then does not see, which is a
    /// use-after-free the type system cannot catch and no test of a *dead* local
    /// would notice.
    ///
    /// The translation is therefore a set difference at the *slot* level:
    ///
    /// ```text
    /// dead_slots = { slot(l) : l ∈ roots.dead() } \ { slot(l) : l ∈ roots.live() }
    /// ```
    ///
    /// which is correct by construction — a slot occupied by a live root is
    /// written with that root's value in the loop above, and writing it *is* the
    /// erasure of whatever dead local shared it. The difference also dedupes: two
    /// dead locals sharing one slot are one null store, not two.
    fn spill_roots(&self, builder: &mut FunctionBuilder, roots: &RootSlots, vars: &[Variable]) {
        if roots.live().is_empty() && roots.dead().is_empty() {
            return;
        }
        let frame_base = builder.use_var(self.frame_var);
        // Aligned + non-trapping: the frame's slots are live for the whole call
        // and in-bounds by construction.
        let flags = MemFlags::trusted();
        // The slots this safepoint's live roots occupy, collected as they are
        // stored so the dead set below can be differenced against them.
        //
        // The *stores* are already deterministic without any of this: they are
        // emitted in `roots.live()` order, which `annot` documents and `liveness`
        // delivers as ascending `LocalId`. The `sort_unstable` further down is
        // for the `binary_search`, not for the emission order — a distinction
        // worth keeping straight, because sorting this earlier would change the
        // order of the emitted stores for no reason.
        let mut live_slots: Vec<u32> = Vec::with_capacity(roots.live().len());
        for &local in roots.live() {
            let Some(slot) = self.root_slot_of.get(local) else {
                // A `Scalar` local in the root set — it holds a payload, not a
                // pointer, so there is nothing for the collector to see and no
                // slot to write. The verifier rejects such MIR
                // (`VerifyError::RootIsNotGc`) and `RootSlotMap::color` filters
                // it out, so this is the *only* way to get here.
                //
                // It is a `debug_assert` and not a `continue` alone because the
                // other way to reach it would be a colouring that missed a live
                // `Gc` root, and that is a root the collector never sees: swept
                // while reachable, presenting later as a use-after-free
                // somewhere else entirely.
                // `debug_slot_of`'s keys are exactly this function's `Gc`
                // locals, so it answers "is this one" without a second map.
                debug_assert!(
                    !self.debug_slot_of.contains_key(&local),
                    "live `Gc` root {local:?} has no shadow slot; the colouring \
                     dropped a root the collector must see"
                );
                continue;
            };
            live_slots.push(slot);
            let val = builder.use_var(vars[local.0 as usize]);
            builder
                .ins()
                .store(flags, val, frame_base, slot_displacement(slot));
        }
        if roots.dead().is_empty() {
            return;
        }
        live_slots.sort_unstable();
        let mut dead_slots: Vec<u32> = roots
            .dead()
            .iter()
            .filter_map(|&local| self.root_slot_of.get(local))
            .filter(|slot| live_slots.binary_search(slot).is_err())
            .collect();
        dead_slots.sort_unstable();
        dead_slots.dedup();
        if dead_slots.is_empty() {
            return;
        }
        let null = builder.ins().iconst(GC, 0);
        for slot in dead_slots {
            builder
                .ins()
                .store(flags, null, frame_base, slot_displacement(slot));
        }
    }

    /// The debugger's write (§9.3, M10-WS2, ADR-104): store every `Gc` local
    /// `inst` defines into `debug_frame.locals[slot].value`, at the definition.
    ///
    /// # Why one store per definition is the same view as a store per safepoint
    ///
    /// This replaces a `spill_debug` that ran at every GC safepoint *and* at
    /// every `Inst::CheckFault`, writing the whole `DebugSlots::visible()` set
    /// each time — `Σ_points |visible|` stores, against `Σ 1 per definition`
    /// here. The two produce identical slot contents everywhere a snapshot can
    /// be taken, and the argument is short:
    ///
    /// A debug slot is never cleared, so its content is *the value the most
    /// recently executed store to it wrote*. The old spill wrote
    /// `builder.use_var(vars[L])`, which is by definition the value of the most
    /// recently executed `def_var` of `L` — or Cranelift's zero for a path
    /// where none executed (`cranelift-frontend`'s SSA builder zero-initializes
    /// a variable that is undefined along an incoming edge). Writing at every
    /// `def_var` therefore leaves exactly that same value in the slot, and a
    /// frame's slots start `None`, which is the same zero. Loop back-edges and
    /// redefinitions are covered by the same sentence: "most recently executed"
    /// is a property of the run, not of the CFG.
    ///
    /// So the change cannot lose a value. It *gains* a few: a local defined at
    /// the end of a block and dead at the top of the next was in no debug
    /// point's `visible()` and so was never written at all, and now shows the
    /// value it was given. That is MIR-16's contract — "a value that has been
    /// produced stays renderable" — being met more completely, not less.
    ///
    /// `DebugSlots` is unchanged and stays exactly what ADR-044 defines. It
    /// stops being the *emission driver* and remains the *contract*: whatever a
    /// point's `visible()` names, this has already stored.
    fn store_debug_defs(&self, builder: &mut FunctionBuilder, inst: &Inst, vars: &[Variable]) {
        // `praxis_mir::defs` rather than a match here. ADR-044's Consequences
        // fix the count of exhaustive matches over `Inst` at five, and the
        // liveness pass's own answer to "what does this define" is one of them;
        // a sixth copy could drift, and the drift would present as a local the
        // debugger silently stops showing.
        for local in praxis_mir::defs(inst) {
            self.store_debug_local(builder, local, vars);
            self.store_elided_boxes_of(builder, local, vars);
        }
    }

    /// Store `local`'s raw scalar word into the debug slot of every box the
    /// forwarding pass elided in its favour (ADR-120 part 2).
    ///
    /// The same one store [`SpillCtx::store_debug_local`] emits, at the same
    /// kind of point, into the same run of slots — the difference is entirely in
    /// what the word *means*, and that is recorded once per function in the
    /// slot's [`DebugLocalMeta::slot_kind`] rather than per store. Generated
    /// code emits no tag and no branch: a scalar slot costs exactly what a
    /// reference slot costs.
    ///
    /// No conversion is needed for any kind. Every MIR local is one Cranelift
    /// `Variable` of type `GC` (`I64`), so a `Scalar(Float)` local already holds
    /// `f64::to_bits()` — which is what `ScalarKind::Float`'s own doc says the
    /// scalar channel carries — and a `Scalar(Bool)` holds the zero-extended
    /// byte. `DebugSlotKind` decodes each on the way out.
    ///
    /// ### Why this is not on the raising path, which is ADR-117's doing
    ///
    /// A definition of `s` in `s = a + b` can *fault*, and the box this store
    /// stands in for was the instruction after the `Inst::CheckFault`: a program
    /// that overflowed never reached it, so the temp rendered `<uninit>` and
    /// that is the honest answer for a value that was never produced. The store
    /// below keeps that answer, because the caller emits it after the whole
    /// **step** rather than after the instruction, and ADR-117 folds a checked
    /// `IntBinOp` and its check into one step whose raise block leaves for the
    /// fault epilogue. So the overflowing path diverts before reaching this
    /// store, exactly as it diverted before reaching the box.
    ///
    /// The call-site comment in `lower_fn`'s block loop predicted this — "if it
    /// ever did, folding would move that store off the raising path" — one wave
    /// before there was a store to move. At `RaiseExit::Observed`, where the
    /// raise converges instead, this would store the wrapped value;
    /// `an_overflowing_temp_is_not_given_the_wrapped_value_it_never_produced`
    /// is the test that says which of the two shapes is in the tree.
    fn store_elided_boxes_of(
        &self,
        builder: &mut FunctionBuilder,
        local: LocalId,
        vars: &[Variable],
    ) {
        let Some(slots) = self.elided_box_slots.get(&local) else {
            return;
        };
        let values_base = builder.use_var(self.debug_values_var);
        let val = builder.use_var(vars[local.0 as usize]);
        for &slot in slots {
            builder.ins().store(
                MemFlags::trusted(),
                val,
                values_base,
                slot_displacement(slot),
            );
        }
    }

    /// Store `local`'s current value into its debug slot, if it has one.
    ///
    /// Non-`Gc` locals (a scalar payload) have no slot and are skipped. Every
    /// `Gc` local has one, which is where this differs from
    /// [`SpillCtx::spill_roots`] since ADR-128 decision 3: the root map may have
    /// nothing for a local that is live at no safepoint, and the debugger must
    /// still be able to render it.
    ///
    /// One store, with the slot index as the store's own displacement, exactly
    /// like [`SpillCtx::spill_roots`]. Under ADR-021's heap frame this was a
    /// load of `frame.locals`, an `iadd_imm_s` over a 48-byte `DebugLocal`
    /// stride, and a store at zero.
    fn store_debug_local(&self, builder: &mut FunctionBuilder, local: LocalId, vars: &[Variable]) {
        let Some(&slot) = self.debug_slot_of.get(&local) else {
            return;
        };
        let values_base = builder.use_var(self.debug_values_var);
        let val = builder.use_var(vars[local.0 as usize]);
        // A non-null `GcRef` written into an `Option<GcRef>` slot *is* `Some(v)`:
        // the niche makes the two the same word (F18). The all-zero word the
        // claim starts with is `None`.
        builder.ins().store(
            MemFlags::trusted(),
            val,
            values_base,
            slot_displacement(slot),
        );
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

/// One lowering step of a MIR block: a run of its instructions that emit code
/// together.
///
/// There is exactly one run longer than a single instruction, and it is
/// ADR-117's: a checked `IntBinOp` and the `Inst::CheckFault` that ADR-088 puts
/// after it. The raise's cold block branches straight to the fault epilogue and
/// the check emits nothing.
///
/// **Why the pair is a step rather than a look-ahead inside [`lower_inst`].**
/// The fold has two halves that have to agree — the raise diverts, *and* the
/// check is silent — and as two independent match arms consulting the same
/// neighbourhood they can disagree in two ways: a check skipped after a raise
/// that still converged (a fault that is never observed, i.e. a program that
/// keeps running past an overflow) and a raise that diverts under a check that
/// also fires (harmless, but dead code nobody would find). Grouping leaves
/// neither spellable. The check is a member of the pair and of no other step,
/// so it cannot be lowered twice or forgotten separately; and
/// [`StepKind::RaiseIntoFault`] carries the *operation*, not an `&Inst`, so the
/// diverting form cannot be handed an instruction that does not emit its own
/// fault path.
struct Step<'a> {
    /// Every MIR instruction this step covers, in block order.
    ///
    /// The debugger's per-definition store (ADR-104) runs over all of them, so
    /// grouping two instructions cannot change what a snapshot renders — which
    /// is what makes this field the group's definition rather than a
    /// convenience.
    insts: &'a [Inst],
    /// What the step emits.
    kind: StepKind,
}

/// What a [`Step`] emits.
enum StepKind {
    /// The step's one instruction, lowered as itself by [`lower_inst`].
    Lone,
    /// Checked `Int` arithmetic whose overflow — and, for `Div`/`Rem`, whose
    /// zero divisor — branches straight to `on_fault` (ADR-117).
    ///
    /// The second instruction of the step is the `Inst::CheckFault` this
    /// replaces. It is covered by the step and emits nothing.
    RaiseIntoFault {
        op: IntBinOp,
        dst: LocalId,
        lhs: LocalId,
        rhs: LocalId,
        on_fault: BlockId,
    },
}

/// Group a MIR block's instructions into [`Step`]s.
///
/// **The whole of ADR-117's applicability test is the match below**, and it is
/// a match on two *adjacent* instructions because that is precisely what
/// ADR-088 decision 1 makes locally decidable: the check is in the same block
/// and at the next index, or `verify::check_fault_observed` rejects the
/// function. Nothing here depends on the verifier having run, though — an
/// unfused checked `IntBinOp` lowers to the converging diamond it always did,
/// so a hypothetical caller that skipped `verify` gets slower code and not
/// wrong code.
///
/// Checked `Int` arithmetic is the only candidate there can be. It is the one
/// instruction in the language whose fault path the *lowering* emits (a cold
/// block calling `praxis_raise_*_if`); every other faulting instruction is a
/// call into a wrapper that sets `pending_fault` and returns, and the only way
/// to learn that from generated code is to read the slot — which is what
/// `Inst::CheckFault` is.
fn steps(insts: &[Inst]) -> Vec<Step<'_>> {
    let mut out = Vec::with_capacity(insts.len());
    let mut i = 0;
    while i < insts.len() {
        // ADR-117's single toggle, and the only reader of the feature. Off, the
        // pair below is one step; on, every instruction is its own and the
        // backend emits exactly what it emitted before ADR-117 — which is what
        // makes the A/B two builds of one branch rather than two branches.
        let fused = if cfg!(feature = "unfolded-check-fault") {
            None
        } else {
            match (&insts[i], insts.get(i + 1)) {
                (
                    Inst::IntBinOp {
                        op,
                        dst,
                        lhs,
                        rhs,
                        overflow: Overflow::Checked,
                    },
                    Some(Inst::CheckFault { on_fault, .. }),
                ) => Some(StepKind::RaiseIntoFault {
                    op: *op,
                    dst: *dst,
                    lhs: *lhs,
                    rhs: *rhs,
                    on_fault: *on_fault,
                }),
                _ => None,
            }
        };
        let width = if fused.is_some() { 2 } else { 1 };
        out.push(Step {
            insts: &insts[i..i + width],
            kind: fused.unwrap_or(StepKind::Lone),
        });
        i += width;
    }
    out
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
            // **No `spill.spill_roots` here, and that is the point.** This
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
            // The debugger's view is written at definitions now, not here
            // (ADR-104). The field stays in the pattern so the set this arm
            // carries is still visible at the point that used to consume it.
            debug: _,
        } => {
            // Spill live Gc roots into the shadow frame *before* the allocating
            // call: the wrapper may trigger a collection (§12.4), and the
            // collector walks the frame (ADR-019).
            spill.spill_roots(builder, roots, vars);
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
                AllocKind::Bool { value } => {
                    // Two loads and a `select`, not a call (ADR-110). See
                    // `emit_inline_bool`: `praxis_alloc_bool` has not allocated
                    // since ADR-040 Decision 4 and its row is `Effect::Pure`.
                    let arg = builder.use_var(vars[value.0 as usize]);
                    let result = emit_inline_bool(builder, ctx_val, arg);
                    builder.def_var(vars[dst.0 as usize], result);
                }
                AllocKind::Int { value } => {
                    // An intern-table probe behind an inline pacing test, with
                    // `praxis_alloc_int` on the cold path (ADR-113). This arm is
                    // rare — `lower_lit_gc` routes every *in-range* `Int`
                    // literal to `Inst::ConstGc`, so what reaches here is the
                    // out-of-range literal, which takes the cold path by
                    // construction. It shares the helper with
                    // `Inst::Materialize` anyway, because two spellings of one
                    // sequence is how the pacing test goes missing from one of
                    // them.
                    let arg = builder.use_var(vars[value.0 as usize]);
                    emit_inline_intern(
                        builder,
                        ctx_val,
                        arg,
                        vars[dst.0 as usize],
                        praxis_runtime::small_int::INLINE_INTERN_SITE,
                        praxis_runtime::scalars::INT_CLAIM_SITE,
                        ctor_sym,
                        module,
                        imports,
                    )?;
                }
                AllocKind::Float { value } => {
                    // A pacing test and an inline bitmap claim, with
                    // `praxis_alloc_float` cold behind both (ADR-119). There is
                    // no intern table in front of it: `Float` has none, so the
                    // claim is the whole inline form.
                    let arg = builder.use_var(vars[value.0 as usize]);
                    if INLINE_SCALAR_CLAIM {
                        emit_inline_claim_box(
                            builder,
                            ctx_val,
                            arg,
                            vars[dst.0 as usize],
                            praxis_runtime::scalars::FLOAT_CLAIM_SITE,
                            BuiltinTypeId::Float,
                            ctor_sym,
                            module,
                            imports,
                        )?;
                    } else {
                        let result =
                            call_symbol(builder, ctx_val, &[arg], ctor_sym, module, imports)?;
                        builder.def_var(vars[dst.0 as usize], result);
                    }
                }
                AllocKind::Char { value } => {
                    // Pass the u32 Unicode scalar, take back the `GcRef`.
                    //
                    // `Char` has an intern table too (`small_char`, ADR-107) and
                    // could take the `Int` arm above, and a claim sequence its
                    // block layout admits — but `AllocChar`'s manifest row is
                    // `AllocatesAndFaults`, so an inline path must also let an
                    // invalid code point reach the wrapper that raises
                    // `InvalidChar`, and handover 23's P-4a may move the
                    // validation into the table's bounds anyway. It is its own
                    // item, not a rider on this one — ADR-113 said so and
                    // ADR-119 does not change it.
                    let arg = builder.use_var(vars[value.0 as usize]);
                    let result = call_symbol(builder, ctx_val, &[arg], ctor_sym, module, imports)?;
                    builder.def_var(vars[dst.0 as usize], result);
                }
                AllocKind::Unit => {
                    // One load, not a call (ADR-110). `praxis_alloc_unit` reads
                    // `ctx.unit_ref` and returns it; its row is `Effect::Pure`
                    // and `load_unit_sentinel` is the same load every fault
                    // epilogue in the backend already emits.
                    let result = load_unit_sentinel(builder, ctx_val);
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
            // The debugger's view is written at definitions now, not here
            // (ADR-104). The field stays in the pattern so the set this arm
            // carries is still visible at the point that used to consume it.
            debug: _,
        } => {
            // Materialize re-boxes a scalar → it allocates → safepoint.
            //
            // The spill stays ahead of every arm below, including the two that
            // no longer call anything. `Inst::Materialize` is an unconditional
            // safepoint in MIR, which is a MIR-level property about which
            // instructions the collector may run at — not a backend arm's to
            // narrow from what it happens to emit (ADR-110). It is also what
            // makes the cold arms correct without further thought: the roots are
            // in the shadow frame before the branch, so the wrapper may collect.
            spill.spill_roots(builder, roots, vars);
            let src_val = builder.use_var(vars[src.0 as usize]);
            match scalar {
                ScalarKind::Bool => {
                    // Two loads and a `select`, no branch (ADR-110).
                    // `praxis_alloc_bool`'s row is `Effect::Pure` and it has not
                    // allocated since ADR-040 decision 4.
                    let result = emit_inline_bool(builder, ctx_val, src_val);
                    builder.def_var(vars[dst.0 as usize], result);
                }
                ScalarKind::Int => {
                    // **The hot allocating instruction in the language.** Every
                    // loop counter, every accumulator, every fused-pipeline sink
                    // arrives here, and for a value in `small_int`'s range the
                    // answer is an object the runtime minted before `main` ran.
                    // An inline pacing test and an inline table probe, with
                    // `praxis_alloc_int` cold behind both (ADR-113).
                    emit_inline_intern(
                        builder,
                        ctx_val,
                        src_val,
                        vars[dst.0 as usize],
                        praxis_runtime::small_int::INLINE_INTERN_SITE,
                        praxis_runtime::scalars::INT_CLAIM_SITE,
                        scalar.alloc_symbol(),
                        module,
                        imports,
                    )?;
                }
                ScalarKind::Float if INLINE_SCALAR_CLAIM => {
                    // The pacing test and the inline claim (ADR-119).
                    // `mandelbrot` is the only benchmark that reaches here, and
                    // W8-S0 already took eight of its ten float boxes — which is
                    // exactly why ADR-119's headline is not measured on it.
                    emit_inline_claim_box(
                        builder,
                        ctx_val,
                        src_val,
                        vars[dst.0 as usize],
                        praxis_runtime::scalars::FLOAT_CLAIM_SITE,
                        BuiltinTypeId::Float,
                        scalar.alloc_symbol(),
                        module,
                        imports,
                    )?;
                }
                ScalarKind::Char | ScalarKind::Float | ScalarKind::Byte => {
                    // A scalar payload re-boxed: Char → praxis_alloc_char, Float
                    // → praxis_alloc_float. The mapping is
                    // `ScalarKind::alloc_symbol`, for `ExtractScalar`'s reason
                    // above — and it is what refuses `Byte`, which is reserved
                    // and unwired, rather than this match deciding it.
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
            }
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
            // Unfused, which is what a *lone* `IntBinOp` means: this site's
            // raise converges on the join, and the `Inst::CheckFault` the
            // builder emits next observes it — ADR-102's shape. The fused form
            // is `StepKind::RaiseIntoFault`, which the block loop lowers
            // through the same function with `RaiseExit::Folded` (ADR-117).
            let report = match overflow {
                Overflow::Bounded => OverflowReport::Bare,
                Overflow::Checked => OverflowReport::Checked(RaiseExit::Observed),
            };
            lower_int_binop(
                builder, *op, *dst, *lhs, *rhs, report, ctx_val, vars, module, imports,
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
            // The debugger's view is written at definitions now, not here
            // (ADR-104). The field stays in the pattern so the set this arm
            // carries is still visible at the point that used to consume it.
            debug: _,
        } => {
            // A call may allocate (and M4 user functions allocate freely) →
            // safepoint. Spill the live Gc roots before the call.
            spill.spill_roots(builder, roots, vars);
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
                    // Two of the collection reads have an inline form that
                    // proves the receiver's descriptor and then reads the
                    // payload directly (ADR-118 part 2). The emitter defines
                    // `dst` on every path and leaves the builder at its merge
                    // block, exactly as `emit_scalar_load` does, so there is
                    // nothing left for this arm to do.
                    if emit_inline_collection_read(
                        builder,
                        ctx_val,
                        &arg_vals,
                        *sym,
                        vars[dst.0 as usize],
                        module,
                        imports,
                    )? {
                        return Ok(());
                    }
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
            // The debugger's view is written at definitions now, not here
            // (ADR-104). The field stays in the pattern so the set this arm
            // carries is still visible at the point that used to consume it.
            debug: _,
        } => {
            // M7, §4.10 (Approach B). An indirect call through a closure value.
            // Spill live Gc roots (safepoint — the call may allocate/GC), read
            // the closure's `fn_ptr` via `praxis_closure_fn_ptr`, then emit a
            // Cranelift `call_indirect` with the signature
            // `fn(ctx, closure, args...) -> i64`. The closure is passed as the
            // hidden first explicit arg; the synthetic function loads its
            // captures at entry.
            spill.spill_roots(builder, roots, vars);
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
            // The debugger's view is written at definitions now, not here
            // (ADR-104). The field stays in the pattern so the set this arm
            // carries is still visible at the point that used to consume it.
            debug: _,
        } => {
            // Structural equality via praxis_struct_eq(ctx, a, b) -> i64 (0/1).
            // The call may trigger GC → spill live Gc roots first (safepoint).
            spill.spill_roots(builder, roots, vars);
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
        Inst::BitsetContains { dst, set, member } => {
            // ADR-118 decision 6. `praxis_bitset_contains` is `Effect::Pure` —
            // it allocates nothing and a `BitSet` query is total — so this is
            // not a safepoint, there is nothing to spill, and no `CheckFault`
            // follows. The narrowing is MIR's (`liveness::is_gc_safepoint` does
            // not match this variant); this arm only emits what MIR decided.
            let s = builder.use_var(vars[set.0 as usize]);
            let m = builder.use_var(vars[member.0 as usize]);
            emit_bitset_contains(
                builder,
                ctx_val,
                s,
                m,
                vars[dst.0 as usize],
                module,
                imports,
            )?;
        }
        Inst::CheckFault {
            on_fault,
            // The debug set stays on the instruction — it is the contract for
            // what a snapshot taken on this fault path must be able to render
            // (MIR-16), and the verifier still checks it is annotated. It is no
            // longer an emission driver: see below.
            debug: _,
        } => {
            // Divert to the fault block when a fault is pending (§10.4). The
            // faultable op just before this set `pending_fault` (or a callee
            // did). If a fault is pending, branch to the function's fault block
            // — which restores the shadow stack and returns the Unit sentinel,
            // unwinding cleanly to the host. The rest of this MIR block's
            // instructions lower into a fresh fall-through block, so the
            // diversion does not strand them.
            //
            // **This arm is not reached for every `Inst::CheckFault`.** One
            // whose predecessor is checked `Int` arithmetic is fused into it by
            // `steps` and emits nothing: that instruction's own raise already
            // branched, on the same predicate, to the same fault block, so the
            // load-load-branch below would be re-deciding what the branch above
            // it decided (ADR-117). Every other faulting instruction is a call
            // into a wrapper that returns normally after setting the slot, and
            // reading the slot is the only way generated code can learn it — so
            // for those this arm is still the whole mechanism.
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
            // **Nothing is spilled here any more** (ADR-104). This used to
            // write the whole `DebugSlots::visible()` set before the fault
            // test, because a snapshot taken on the fault path would otherwise
            // show `<uninit>` for operands computed since the last GC safepoint
            // — the `0` divisor in `x / 0` being the case that motivated it.
            // Those operands are `Gc` locals produced by an `Alloc`, a
            // `Materialize` or a `ConstGc` *earlier in this block*, and
            // `SpillCtx::store_debug_defs` has already written each of them at
            // its own definition, so the fault path sees them without a spill
            // here. The faulting op's own result is still genuinely never
            // produced — the fault happened during it — so it still reads
            // `<uninit>` for the arithmetic case, and still reads the wrapper's
            // fault-path return for a call, exactly as before.
            //
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
        Inst::StoreField {
            record,
            field_idx,
            value,
        } => {
            // praxis_record_set_field(ctx, record, idx, value) -> GcRef. The
            // answer is the receiver, which the caller already holds, so it is
            // dropped rather than given a local: this instruction defines none
            // (`verify::defines`), the way `StoreScalar` defines none.
            let record_val = builder.use_var(vars[record.0 as usize]);
            let idx_val = builder.ins().iconst(GC, *field_idx as i64);
            let value_val = builder.use_var(vars[value.0 as usize]);
            let _ = call_symbol(
                builder,
                ctx_val,
                &[record_val, idx_val, value_val],
                RuntimeSymbol::RecordSetField,
                module,
                imports,
            )?;
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
            // Epilogue: give this call's shadow and debug frames back before
            // returning (ADR-019, ADR-101, ADR-104).
            emit_pop_shadow_frame(builder, ctx_val, spill);
            emit_pop_debug_frame(builder, ctx_val, spill);
            let v = builder.use_var(vars[value.0 as usize]);
            builder.ins().return_(&[v]);
        }
        Terminator::Fault => {
            // Epilogue (fault path): snapshot the debug frames BEFORE popping,
            // so the host can inspect them after the unwind (§9.3, M10-WS3).
            // Idempotent: only the innermost frame's epilogue (which runs first)
            // captures; outer frames unwinding later skip.
            emit_snapshot_debug_chain(builder, ctx_val, module, imports)?;
            // Then give both back before unwinding.
            emit_pop_shadow_frame(builder, ctx_val, spill);
            emit_pop_debug_frame(builder, ctx_val, spill);
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
/// back and restore the stack budget. Two stores, no call.
///
/// Both restore an absolute the prologue saved rather than undoing an
/// increment. The extern helper this replaced decremented the counter with a
/// `saturating_sub` precisely because a fault path could otherwise underflow
/// it; there is nothing to saturate when the value being written is the one
/// this call found on entry, and an imbalance introduced below this frame
/// cannot leak upward past it. That is also why ADR-105's variable-width charge
/// costs the epilogue nothing: restoring an absolute does not need to know what
/// was added.
fn emit_pop_shadow_frame(builder: &mut FunctionBuilder, ctx_val: Value, spill: &SpillCtx<'_>) {
    let saved_left = builder.use_var(spill.saved_left_var);
    builder.ins().store(
        MemFlags::trusted(),
        saved_left,
        ctx_val,
        STACK_LEFT_OFFSET as i32,
    );
    let base = builder.use_var(spill.frame_var);
    emit_slot_stack_pop(builder, ctx_val, SHADOW_OFFSET, base);
}

/// Emit the crash debugger's epilogue (§9.3, ADR-104): give this call's frame
/// entry and value slots back. Two stores, no call — where
/// `praxis_pop_debug_frame` was a guarded extern call that freed two boxes.
///
/// Mirrors [`emit_pop_shadow_frame`], and restores saved absolutes for the same
/// reasons: no subtraction to underflow, and an imbalance introduced below this
/// frame is corrected here rather than propagated to the caller.
fn emit_pop_debug_frame(builder: &mut FunctionBuilder, ctx_val: Value, spill: &SpillCtx<'_>) {
    let values_base = builder.use_var(spill.debug_values_var);
    emit_slot_stack_pop(builder, ctx_val, DEBUG_VALUES_OFFSET, values_base);
    let entry = builder.use_var(spill.debug_frame_var);
    emit_slot_stack_pop(builder, ctx_val, DEBUG_FRAMES_OFFSET, entry);
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

/// Materialize a `Bool` from a scalar payload word, inline (ADR-110).
///
/// `praxis_alloc_bool`'s whole body is `ctx.true_ref` / `ctx.false_ref` — it has
/// not allocated since ADR-040 Decision 4 stopped twenty-four wrappers minting a
/// fresh immortal per call, and its manifest row has said `Effect::Pure` ever
/// since. So the call around it was buying a `bl`, a `catch_unwind` landing pad
/// and a return, in front of two loads and a `select`.
///
/// No branch and no cold block: unlike the interned-`Int` probe there is no
/// range to test, because there are exactly two `Bool`s and both are always in
/// the context. Both loads are unconditional and the `select` picks one, which
/// is branchless and cheaper than the `brif` a two-block form would emit.
///
/// `value` is the payload word MIR carries a `Bool` scalar in, so "true" is
/// `!= 0` rather than `== 1` — the same test `praxis_alloc_bool` applies, and
/// deliberately not `== 1`: a byte that is neither is an invalid `bool`, and
/// `ScalarLoad::BoolByte` documents why the runtime never materializes one.
fn emit_inline_bool(builder: &mut FunctionBuilder, ctx: Value, value: Value) -> Value {
    let t = builder
        .ins()
        .load(GC, MemFlags::trusted(), ctx, TRUE_REF_OFFSET as i32);
    let f = builder
        .ins()
        .load(GC, MemFlags::trusted(), ctx, FALSE_REF_OFFSET as i32);
    // `_u` vs `_s` is immaterial for a zero immediate; the unsigned form is the
    // one the rest of this file reaches for.
    let is_true = builder.ins().icmp_imm_u(IntCC::NotEqual, value, 0);
    builder.ins().select(is_true, t, f)
}

/// The address of `id`'s descriptor, as ADR-116 says to obtain it: one load
/// from `RuntimeContext.descriptors` at a folded displacement.
///
/// **One function rather than the two lines it used to be inside
/// `emit_scalar_load`**, because ADR-118 part 2 added three more proof sites
/// and a second spelling of "where a descriptor address comes from" is exactly
/// what ADR-116 removed. The `adr116-arm-a` toggle is still those two lines and
/// still reverts every proof site in the tree, which is what its `Cargo.toml`
/// comment claims.
///
/// The `iconst` arm is what this replaced: on aarch64 a `static` in this binary
/// lives above 2³², so its address is `movz`+`movk`+`movk` — three instructions
/// per proof site, re-materialized rather than kept live because that is what
/// the register allocator does with constants (handover 25 §3's `opt_level`
/// measurement).
fn descriptor_address(builder: &mut FunctionBuilder, ctx_val: Value, id: BuiltinTypeId) -> Value {
    #[cfg(not(feature = "adr116-arm-a"))]
    {
        builder
            .ins()
            .load(GC, MemFlags::trusted(), ctx_val, descriptor_slot_offset(id))
    }
    #[cfg(feature = "adr116-arm-a")]
    {
        builder.ins().iconst(GC, id.descriptor() as *const _ as i64)
    }
}

/// Prove that `obj` carries `id`'s descriptor: ADR-102's inline type check, as
/// a condition value the caller branches on.
///
/// `MemFlags::trusted()` is `notrap + aligned` and deliberately **not**
/// `readonly`, for [`emit_scalar_load`]'s reason: `set_mark_color` and the
/// sweep write headers, so Cranelift's alias analysis must go on treating a
/// call as clobbering this word.
fn prove_descriptor(
    builder: &mut FunctionBuilder,
    ctx_val: Value,
    obj: Value,
    id: BuiltinTypeId,
) -> Value {
    let have = builder
        .ins()
        .load(GC, MemFlags::trusted(), obj, GC_DESCRIPTOR_OFFSET);
    let want = descriptor_address(builder, ctx_val, id);
    builder.ins().icmp(IntCC::Equal, have, want)
}

/// Whether the collection primitives get their inline form.
///
/// **This pair of lines is ADR-118 part 2's whole toggle**; see the
/// `adr118-arm-a` feature's comment in this crate's `Cargo.toml`. With the
/// feature on, `emit_bitset_contains` and `emit_inline_collection_read` fall
/// straight through to the wrapper call and every other line in the tree is
/// byte-for-byte identical — including the MIR shape change, which is not part
/// of this toggle and is measured by instruction census instead.
#[cfg(not(feature = "adr118-arm-a"))]
const INLINE_COLLECTION_PRIMITIVES: bool = true;
#[cfg(feature = "adr118-arm-a")]
const INLINE_COLLECTION_PRIMITIVES: bool = false;

/// `bs.contains(x)` — the `0`/`1` [`Inst::BitsetContains`] answers, inline.
///
/// ```text
/// prove:  brif  descriptor(bs) == BITSET, prove_int, slow
/// prove_int:
///         brif  descriptor(x) == INT,     probe,     slow
/// probe:  i     = load.i64 [x  + int_payload_offset]
///         word  = ushr_imm i, 6
///         len   = load.i64 [bs + site.len_offset()]
///         brif  icmp ult word, len,       read,      absent
/// read:   base  = load.i64 [bs + site.elements_offset()]
///         w     = load.i64 [base + (word << 3)]
///         r     = band_imm (ushr w, (i & 63)), 1
/// absent: r     = 0
/// slow:   (cold) r = call praxis_bitset_contains(ctx, bs, x)
/// ```
///
/// # There is no range test, and that is a decision rather than an omission
///
/// The wrapper is `BitIndex::new(i).is_some_and(|b| p.contains(b))` — a
/// membership test against `0..=BitIndex::MAX` and then a word probe. The
/// inline form emits only the word probe, because the probe **subsumes** the
/// range test: `BitIndex::MAX_WORDS` is `2^26`, a word count can never reach
/// it, and every `i64` outside the range shifts down to at least `2^26` under
/// the *unsigned* shift — negatives to `2^57` and up. That is the argument;
/// `the_word_probe_generated_code_emits_answers_contains`, in the module that
/// owns the range, is the check, over both extremes of the type and a dense
/// sweep. It is `small_int`'s
/// `the_unsigned_range_test_generated_code_emits_answers_index_of` in a second
/// place, and the reason to write it is the same: an exact identity is the kind
/// of thing that gets believed until a range constant changes.
///
/// # `absent` is inline and not a bail
///
/// A word past the end is a *correct answer*, not a refusal: an empty or short
/// `BitSet` answers `false`, and in `bfs` the set is short for the whole first
/// level of every search. Routing it to the cold block would make the common
/// early state pay a call.
///
/// # What the cold block is for
///
/// A receiver whose descriptor is not `BitSet`, or a member that is not an
/// `Int`. Both are compiler bugs by the time they reach here (§4.3's uniform
/// model plus inference), and the cold arm calls the same wrapper, which reads
/// the payload through the same `read_scalar` refusal it always did — so the
/// diagnosis of a compiler bug is bit-for-bit what it was (ADR-102).
#[allow(clippy::too_many_arguments)] // The lowering context, as `lower_inst` carries it.
fn emit_bitset_contains<M: Module>(
    builder: &mut FunctionBuilder,
    ctx_val: Value,
    set: Value,
    member: Value,
    dst: Variable,
    module: &mut M,
    imports: &mut HashMap<RuntimeSymbol, FuncRef>,
) -> Result<()> {
    if !INLINE_COLLECTION_PRIMITIVES {
        let result = call_symbol(
            builder,
            ctx_val,
            &[set, member],
            RuntimeSymbol::BitsetContains,
            module,
            imports,
        )?;
        builder.def_var(dst, result);
        return Ok(());
    }

    let site = praxis_runtime::bitset::INLINE_BITSET_SITE;
    let (int_id, int_align, _) = inline_scalar_load_of(praxis_mir::ScalarKind::Int)
        .expect("`Int` has an inline payload form; `emit_scalar_load` reads the same row");
    let flags = MemFlags::trusted();

    let prove_int = builder.create_block();
    let probe = builder.create_block();
    let read = builder.create_block();
    let absent = builder.create_block();
    let slow = builder.create_block();
    let merge = builder.create_block();
    builder.set_cold_block(slow);

    let is_bitset = prove_descriptor(builder, ctx_val, set, site.type_id());
    builder.ins().brif(is_bitset, prove_int, &[], slow, &[]);

    // The member's payload is read **after** its descriptor is proved and never
    // before: an eight-byte load off an object that is not an `Int` is REP-56
    // exactly — a zero-width `Unit` read as a word.
    builder.switch_to_block(prove_int);
    let is_int = prove_descriptor(builder, ctx_val, member, int_id);
    builder.ins().brif(is_int, probe, &[], slow, &[]);

    builder.switch_to_block(probe);
    let i = builder.ins().load(
        GC,
        flags,
        member,
        praxis_runtime::GcHeader::payload_offset_for(int_align) as i32,
    );
    let word = builder.ins().ushr_imm_u(i, 6);
    let len = builder.ins().load(GC, flags, set, site.len_offset() as i32);
    let in_bounds = builder.ins().icmp(IntCC::UnsignedLessThan, word, len);
    builder.ins().brif(in_bounds, read, &[], absent, &[]);

    builder.switch_to_block(read);
    {
        let base = builder
            .ins()
            .load(GC, flags, set, site.elements_offset() as i32);
        let offset = builder
            .ins()
            .ishl_imm_u(word, i64::from(site.element_shift()));
        let slot = builder.ins().iadd(base, offset);
        let w = builder.ins().load(GC, flags, slot, 0);
        let shift = builder.ins().band_imm_u(i, 63);
        let bit = builder.ins().ushr(w, shift);
        let present = builder.ins().band_imm_u(bit, 1);
        builder.def_var(dst, present);
        builder.ins().jump(merge, &[]);
    }

    builder.switch_to_block(absent);
    {
        let no = builder.ins().iconst(GC, 0);
        builder.def_var(dst, no);
        builder.ins().jump(merge, &[]);
    }

    builder.switch_to_block(slow);
    {
        let result = call_symbol(
            builder,
            ctx_val,
            &[set, member],
            RuntimeSymbol::BitsetContains,
            module,
            imports,
        )?;
        builder.def_var(dst, result);
        builder.ins().jump(merge, &[]);
    }

    // `def_var` in every arm rather than a block parameter, as `emit_scalar_load`
    // and `emit_inline_intern` do — `FunctionBuilder`'s SSA construction inserts
    // the join itself — and the builder is left switched to `merge`, which
    // `lower_inst`'s caller relies on: `spill.store_debug_defs` runs immediately
    // afterwards and must land where every arm is visible.
    builder.switch_to_block(merge);
    Ok(())
}

/// The inline form of a collection *read* whose MIR is still an `Inst::Call`:
/// `praxis_vec_len` and `praxis_vec_get` (ADR-118 part 2).
///
/// Answers whether it emitted anything. When it did, `dst` is defined on every
/// path and the builder is left at the merge block — [`emit_scalar_load`]'s
/// contract, which `lower_inst`'s caller relies on because
/// `spill.store_debug_defs` runs immediately afterwards and must land where all
/// the arms are visible.
///
/// **Both keep their root spill and their safepoint**, and that is not an
/// oversight: MIR still calls them `Inst::Call`, `liveness::is_gc_safepoint`
/// matches every `Call`, and ADR-113 settled that narrowing a spill from what a
/// backend arm happens to emit is the wrong place to make the decision.
/// `praxis_bitset_contains` got out of that by acquiring its own instruction
/// (decision 6); `vec_get` and `vec_len` are registered as wanting the same
/// treatment and it is a separate change.
///
/// `praxis_deque_len` and `praxis_deque_get` have no arm here and cannot: a
/// `VecDeque` is a ring buffer with a head index, so element *i* is at
/// `(head + i) % cap` and the storage wraps (decision 5).
#[allow(clippy::too_many_arguments)] // The lowering context, as `lower_inst` carries it.
fn emit_inline_collection_read<M: Module>(
    builder: &mut FunctionBuilder,
    ctx_val: Value,
    args: &[Value],
    sym: RuntimeSymbol,
    dst: Variable,
    module: &mut M,
    imports: &mut HashMap<RuntimeSymbol, FuncRef>,
) -> Result<bool> {
    if !INLINE_COLLECTION_PRIMITIVES {
        return Ok(false);
    }
    let site = praxis_runtime::collections::INLINE_VEC_SITE;
    let flags = MemFlags::trusted();

    match (sym, args) {
        // `v.len()` — the length word, then ADR-113's intern probe to box it.
        //
        // The row is `Effect::Allocates`, and the fast path discharges that
        // obligation by **delegating** rather than by reading a `usize` and
        // pretending: `emit_inline_intern` tests `Heap::collection_is_due`
        // first and hands the wrapper the value when it holds, exactly as
        // `int_ref` inside `praxis_vec_len` would have. So the collection
        // schedule is unchanged, which is the only thing ADR-113's decision 1
        // asks of a second caller.
        (RuntimeSymbol::VecLen, &[vec]) => {
            let fast = builder.create_block();
            let slow = builder.create_block();
            let merge = builder.create_block();
            builder.set_cold_block(slow);

            let is_vec = prove_descriptor(builder, ctx_val, vec, site.type_id());
            builder.ins().brif(is_vec, fast, &[], slow, &[]);

            builder.switch_to_block(fast);
            {
                let len = builder.ins().load(GC, flags, vec, site.len_offset() as i32);
                emit_inline_intern(
                    builder,
                    ctx_val,
                    len,
                    dst,
                    praxis_runtime::small_int::INLINE_INTERN_SITE,
                    praxis_runtime::scalars::INT_CLAIM_SITE,
                    RuntimeSymbol::AllocInt,
                    module,
                    imports,
                )?;
                // `emit_inline_intern` leaves the builder at its own merge.
                builder.ins().jump(merge, &[]);
            }

            builder.switch_to_block(slow);
            {
                let result = call_symbol(builder, ctx_val, &[vec], sym, module, imports)?;
                builder.def_var(dst, result);
                builder.ins().jump(merge, &[]);
            }

            builder.switch_to_block(merge);
            Ok(true)
        }
        // `v[i]` / `v.get(i)` — one unsigned compare and one load.
        //
        // The row is `Effect::Faults` (`IndexOutOfBounds`) and **the fast arm
        // cannot fault**: an index the bounds test rejects goes to the wrapper,
        // which raises exactly as it always did. The `Inst::CheckFault` MIR
        // emits after the call is unchanged and reads a flag the fast arm never
        // writes.
        //
        // `idx < 0 || idx as usize >= len` is one *unsigned* compare, for
        // `emit_inline_intern`'s reason: a negative index reinterpreted as a
        // `u64` is above every length, so the sign test is the same branch.
        (RuntimeSymbol::VecGet, &[vec, index]) => {
            let (int_id, int_align, _) = inline_scalar_load_of(praxis_mir::ScalarKind::Int)
                .expect("`Int` has an inline payload form");

            let prove_int = builder.create_block();
            let probe = builder.create_block();
            let fast = builder.create_block();
            let slow = builder.create_block();
            let merge = builder.create_block();
            builder.set_cold_block(slow);

            let is_vec = prove_descriptor(builder, ctx_val, vec, site.type_id());
            builder.ins().brif(is_vec, prove_int, &[], slow, &[]);

            // The index's payload is read **after** its descriptor is proved
            // and never before: an eight-byte load off an object that is not an
            // `Int` is REP-56 exactly — a zero-width `Unit` read as a word.
            builder.switch_to_block(prove_int);
            let is_int = prove_descriptor(builder, ctx_val, index, int_id);
            builder.ins().brif(is_int, probe, &[], slow, &[]);

            builder.switch_to_block(probe);
            let i = builder.ins().load(
                GC,
                flags,
                index,
                praxis_runtime::GcHeader::payload_offset_for(int_align) as i32,
            );
            let len = builder.ins().load(GC, flags, vec, site.len_offset() as i32);
            let in_bounds = builder.ins().icmp(IntCC::UnsignedLessThan, i, len);
            builder.ins().brif(in_bounds, fast, &[], slow, &[]);

            builder.switch_to_block(fast);
            {
                let base = builder
                    .ins()
                    .load(GC, flags, vec, site.elements_offset() as i32);
                let offset = builder.ins().ishl_imm_u(i, i64::from(site.element_shift()));
                let slot = builder.ins().iadd(base, offset);
                let element = builder.ins().load(GC, flags, slot, 0);
                builder.def_var(dst, element);
                builder.ins().jump(merge, &[]);
            }

            builder.switch_to_block(slow);
            {
                let result = call_symbol(builder, ctx_val, &[vec, index], sym, module, imports)?;
                builder.def_var(dst, result);
                builder.ins().jump(merge, &[]);
            }

            builder.switch_to_block(merge);
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Whether an out-of-range scalar box claims its block inline.
///
/// **This pair of lines is ADR-119's whole toggle**; see the `adr119-arm-a`
/// feature's comment in this crate's `Cargo.toml`. With the feature on,
/// [`emit_inline_intern`]'s out-of-range edge goes straight to the wrapper (the
/// tree exactly as ADR-113 left it) and `Float` goes back to an unconditional
/// `call_symbol` with no pacing test at all. Nothing else in the crate reads it,
/// and `praxis-runtime` is byte-for-byte identical in both arms — the offsets
/// and [`praxis_runtime::InlineClaimSite`] exist in both, unread in arm A.
#[cfg(not(feature = "adr119-arm-a"))]
const INLINE_SCALAR_CLAIM: bool = true;
#[cfg(feature = "adr119-arm-a")]
const INLINE_SCALAR_CLAIM: bool = false;

/// Claim a block from the heap's page bitmap and lay an object out in it —
/// inline, in generated code, with no call at all (ADR-119).
///
/// Emitted into the block the builder is already in, which the caller must have
/// reached **only** on the `collection_is_due == false` edge. On success `dst`
/// holds the new reference and control is at `merge`; every bail-out branches to
/// `slow`, which is the caller's cold block and calls the allocating wrapper.
///
/// ```text
/// claim:  page  = load.i64   [heap + site.partial_head_offset()]
///                 brif page == 0, slow, scan     ; the class has no page
/// scan:   w     = uload32    [page + site.page_cursor_offset()]
///         last  = uload32    [page + site.page_last_word_offset()]
///                 brif w >= last, slow, word     ; full, *and* the tail word
/// word:   wp    = iadd page, (w << 3)
///         taken = load.i64   [wp + site.page_allocated_offset()]
///         free  = bnot taken
///                 brif free == 0, slow, store    ; this word is full
/// store:  bit   = ctz free
///         obj   = page + site.first_block() + ((w << 6) + bit) * site.stride()
///         ; (1) header — descriptor first, and see below for why the order
///         store.i64 [obj + site.header_descriptor_offset()]      = descriptor
///         istore16  [obj + site.header_payload_offset_offset()]  = payload_offset
///         istore32  [obj + site.header_heap_id_offset()]         = heap.id
///         ; (2) payload
///         store.i64 [obj + site.payload_offset()]                = value
///         ; (3) the allocated bit — the block becomes sweep-visible here
///         store.i64 [wp  + site.page_allocated_offset()] = taken | (1 << bit)
///         ; (4) both live counters and the pacing charge
///         istore32  [page + site.page_live_count_offset()] += 1
///         store.i64 [heap + site.heap_live_count_offset()] += 1
///         store.i64 [heap + site.bytes_since_collect_offset()] += site.stride()
/// slow:   (cold) r = call praxis_alloc_int / praxis_alloc_float
/// ```
///
/// # `PageHeader::cursor` is not stored, and that is not an omission
///
/// `claim_free_block` sets `cursor = w` on success. `w` is *read* from `cursor`
/// here and never advanced — the inline form scans one word, where the wrapper
/// loops — so the store would write back the value it just read.
/// `the_inline_claim_leaves_the_heap_as_the_wrapper_would` compares a claimed
/// page field-for-field against the wrapper's, `cursor` included, so this is a
/// checked equality rather than a remark.
///
/// # The store order is a severity ranking, not the safety argument
///
/// The safety argument is ADR-119 decision 1 part 2: between the pacing branch
/// and the last store there is no call and no other point at which a collection
/// can begin, so *not due on entry* implies *not due throughout* and no sweep
/// can observe any intermediate state of this sequence. **Nothing below weakens
/// if the order changes**; the order exists because if the argument were ever
/// wrong, the failures it admits should be recoverable ones. Writing the
/// descriptor first removes the single unrecoverable one — a sweep reaching an
/// allocated bit whose header holds uninitialized bytes would read them as a
/// `*const TypeDescriptor` and make an indirect call through `drop_value`.
/// Setting the bit after the payload leaves only bookkeeping errors. Claiming
/// more for the order than that is what ADR-113's Consequences forbid.
#[allow(clippy::too_many_arguments)] // The lowering context, as `lower_inst` carries it.
fn emit_inline_claim(
    builder: &mut FunctionBuilder,
    ctx_val: Value,
    heap: Value,
    value: Value,
    dst: Variable,
    site: praxis_runtime::InlineClaimSite,
    builtin: BuiltinTypeId,
    slow: Block,
    merge: Block,
) {
    // `MemFlags::trusted()` — `notrap + aligned`, and deliberately not
    // `readonly`, for `emit_inline_intern`'s reason one step further: every word
    // this sequence touches is written by the sweep as well, so a call must go
    // on clobbering all of it.
    let flags = MemFlags::trusted();

    let scan = builder.create_block();
    let word = builder.create_block();
    let store = builder.create_block();

    // (1) The class's availability list. A null head means `Heap::grow_class`,
    // which allocates a page from the system allocator — not something an inline
    // sequence attempts.
    let page = builder
        .ins()
        .load(GC, flags, heap, site.partial_head_offset() as i32);
    let no_page = builder.ins().icmp_imm_u(IntCC::Equal, page, 0);
    builder.ins().brif(no_page, slow, &[], scan, &[]);

    // (2) One bitmap word: the one the cursor names. `w >= last` is two refusals
    // in one compare — `w > last` is `claim_free_block`'s loop falling off the
    // end (the page is full), and `w == last` is the tail word, whose free bits
    // must be masked with `tail_mask` because the top of it names blocks the page
    // does not have. ADR-119 decision 3 measured reproducing the mask against
    // ceding the word, and ceding it is free: the bound was already a compare.
    builder.switch_to_block(scan);
    let w = builder
        .ins()
        .uload32(flags, page, site.page_cursor_offset() as i32);
    let last = builder
        .ins()
        .uload32(flags, page, site.page_last_word_offset() as i32);
    let exhausted = builder
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, w, last);
    builder.ins().brif(exhausted, slow, &[], word, &[]);

    builder.switch_to_block(word);
    let word_byte = builder.ins().ishl_imm_u(w, 3);
    let word_ptr = builder.ins().iadd(page, word_byte);
    let taken = builder
        .ins()
        .load(GC, flags, word_ptr, site.page_allocated_offset() as i32);
    let free = builder.ins().bnot(taken);
    let none_free = builder.ins().icmp_imm_u(IntCC::Equal, free, 0);
    builder.ins().brif(none_free, slow, &[], store, &[]);

    builder.switch_to_block(store);
    // `claim_free_block` takes the *lowest* free block, which is what makes
    // reuse deterministic (`a_reclaimed_block_is_reused_for_the_next_object_of_its_layout`).
    // `ctz` is that choice, not a convenience.
    let bit = builder.ins().ctz(free);
    let index_hi = builder.ins().ishl_imm_u(w, 6);
    let index = builder.ins().iadd(index_hi, bit);
    let block_off = builder.ins().imul_imm_u(index, site.stride() as i64);
    let block = builder.ins().iadd(page, block_off);
    let obj = builder.ins().iadd_imm_u(block, site.first_block() as i64);

    // (3) The header, descriptor first. The descriptor address is ADR-116's
    // load from the context's table, not an `iconst`: there is no heap at
    // compile time and a debugger session replaces its `Jit` while keeping its
    // `Runtime`, which is `load_gc_const`'s argument for the same shape.
    let descriptor = descriptor_address(builder, ctx_val, builtin);
    builder.ins().store(
        flags,
        descriptor,
        obj,
        site.header_descriptor_offset() as i32,
    );
    let payload_offset = builder.ins().iconst(GC, site.payload_offset() as i64);
    builder.ins().istore16(
        flags,
        payload_offset,
        obj,
        site.header_payload_offset_offset() as i32,
    );
    // The owning id comes out of the live `Heap` this block was claimed from —
    // the same word `Heap::occupy` writes. `Heap::reset` mints a fresh one and
    // re-stamps every page, so a baked constant would stamp headers with a
    // repudiated identity and the mark phase would refuse to trace them.
    let heap_id = builder
        .ins()
        .uload32(flags, heap, site.heap_id_offset() as i32);
    builder
        .ins()
        .istore32(flags, heap_id, obj, site.header_heap_id_offset() as i32);

    // (4) The payload. For `Float` this is the IEEE-754 bit pattern the scalar
    // channel already carries, which is byte-for-byte what `praxis_alloc_float`
    // writes after `f64::from_bits`.
    builder
        .ins()
        .store(flags, value, obj, site.payload_offset() as i32);

    // (5) The allocated bit. **The block becomes sweep-visible on this store**
    // and not before, which is what the order above is ranked around.
    let one = builder.ins().iconst(GC, 1);
    let mask = builder.ins().ishl(one, bit);
    let claimed = builder.ins().bor(taken, mask);
    builder.ins().store(
        flags,
        claimed,
        word_ptr,
        site.page_allocated_offset() as i32,
    );

    // (6) Both live counters and the pacing charge. Every one of these is
    // *decremented* elsewhere and never recomputed — `sweep` takes the heap's
    // by what it reclaimed and `release_blocks` takes the page's — so a skipped
    // increment does not decay, it underflows, and `relink_pages` then reads a
    // page holding live blocks as empty and hands its storage to another layout.
    let page_live = builder
        .ins()
        .uload32(flags, page, site.page_live_count_offset() as i32);
    let page_live_next = builder.ins().iadd_imm_u(page_live, 1);
    builder.ins().istore32(
        flags,
        page_live_next,
        page,
        site.page_live_count_offset() as i32,
    );
    let heap_live = builder
        .ins()
        .load(GC, flags, heap, site.heap_live_count_offset() as i32);
    let heap_live_next = builder.ins().iadd_imm_u(heap_live, 1);
    builder.ins().store(
        flags,
        heap_live_next,
        heap,
        site.heap_live_count_offset() as i32,
    );
    // `Heap::occupy` charges `stride + descriptor.owned_bytes_of(payload)`.
    // `InlineClaimSite::of` refuses every descriptor whose `owned_bytes` is
    // `Some`, so the second term is zero here by construction rather than by
    // this arm's choice — which is why the refusal is in the runtime and const.
    let since = builder
        .ins()
        .load(GC, flags, heap, site.bytes_since_collect_offset() as i32);
    let since_next = builder.ins().iadd_imm_u(since, site.stride() as i64);
    builder.ins().store(
        flags,
        since_next,
        heap,
        site.bytes_since_collect_offset() as i32,
    );

    builder.def_var(dst, obj);
    builder.ins().jump(merge, &[]);
}

/// Box a scalar by probing the runtime's intern table inline, with the
/// allocating wrapper on a cold path (ADR-113), and — since ADR-119 — an inline
/// bitmap claim on the out-of-range edge.
///
/// ```text
/// hot:   heap  = load.i64  [ctx  + site.heap_offset()]
///        since = load.i64  [heap + site.bytes_since_collect_offset()]
///        thr   = load.i64  [heap + site.collect_threshold_offset()]
///        due   = icmp uge  since, thr
///                brif due, slow, probe                 ; ADR-040's obligation
/// probe: index = iadd_imm  value, -site.min()
///        ok    = icmp_imm  ule index, site.span()      ; one compare, not two
///                brif ok, fast, claim
/// fast:  off   = ishl_imm  index, site.stride_shift()
///        base  = load.i64  [ctx + site.table_offset()]
///        r     = load.i64  [base + off]
/// claim: (ADR-119) the bitmap claim, bailing to slow
/// slow:  (cold) r = call praxis_alloc_int(ctx, value)  ; unchanged
/// ```
///
/// # What this replaces, and why it was the largest site left
///
/// `Inst::Materialize` is the hot allocating instruction in the language: every
/// loop counter, every accumulator, every fused-pipeline sink. For `Int` it was
/// a `bl` to `praxis_alloc_int`, an `abi_guard!` `catch_unwind` landing pad, a
/// `RuntimeRoots::from_context` (five raw pointers read out of the context and
/// four branches), a `Heap::pace` and a `Heap::maybe_collect` — and then, for the
/// overwhelmingly common case, `int_ref` answered from a two-load table read
/// having allocated nothing at all. Everything in front of that read is what
/// this deletes; the read itself is the same read.
///
/// # The pacing test is first, and it is the whole ADR-040 argument
///
/// ADR-040 made `Heap::alloc` take a `#[must_use] Safepoint` whose only producer
/// is `Heap::pace`, so that "allocate on the paced path without pacing" has no
/// spelling. This sequence forges no token, because **the token is permission to
/// *collect*, not permission to allocate** — and this path never collects and
/// never allocates. It reads an immortal the runtime minted before `main` ran.
///
/// What it must not do is take that branch when a collection *was* due, because
/// then a program whose pressure came from elsewhere would have its collection
/// silently deferred at every one of these sites. Hence the first branch: when
/// `Heap::collection_is_due` holds, this goes to the wrapper, which paces
/// through `Heap::pace` exactly as it always did. On the branch it keeps,
/// `maybe_collect` would have returned `false` — `collect_inner` would not have
/// run, `RuntimeRoots::from_context` has no effects, and `int_ref`'s interned
/// arm charges nothing against `bytes_since_collect`. So the two paths are
/// equal, instruction for instruction of *observable effect*, and ADR-100
/// decision 3 ("`int_ref` still paces, even when it answers from the table") is
/// preserved rather than traded: the collector is still offered its turn at
/// every site where it would have taken one.
///
/// The `site` is [`praxis_runtime::InlineInternSite`], and it carries the pacing
/// offsets as well as the table's — deliberately not as two arguments, so that a
/// caller cannot hold the permission without the obligation. Its doc, and
/// `Heap::collection_is_due`'s, are where a pacer with a third term is told that
/// this function owes the same change.
///
/// # The range test is one unsigned compare
///
/// `small_int::index_of` is `v >= MIN && v <= MAX`, two signed compares and two
/// branches. The emitted form is the two's-complement identity for the same
/// predicate — `(v - MIN) as u64 <= (MAX - MIN) as u64` — which is one compare,
/// one branch, and it reuses the subtract the index needs anyway. Both immediates
/// come off the site rather than from `SMALL_INT_MIN`/`SMALL_INT_MAX` read here,
/// and `the_unsigned_range_test_generated_code_emits_answers_index_of` in
/// `small_int.rs` proves the two agree over the boundary values and both extremes
/// of the type.
///
/// # Flags
///
/// `MemFlags::trusted()` — `notrap + aligned`, and deliberately **not**
/// `readonly`, for `emit_scalar_load`'s reason: `bytes_since_collect` is written
/// by every allocating wrapper, so Cranelift's alias analysis must go on treating
/// a call as clobbering it. Hoisting the pacing load out of a loop would turn a
/// collection into one that never happens. (Two of these sites with no call
/// between them may share the load, and that is sound in the other direction —
/// neither of them wrote it.)
#[allow(clippy::too_many_arguments)] // The lowering context, as `lower_inst` carries it.
fn emit_inline_intern<M: Module>(
    builder: &mut FunctionBuilder,
    ctx_val: Value,
    value: Value,
    dst: Variable,
    site: praxis_runtime::InlineInternSite,
    claim: praxis_runtime::InlineClaimSite,
    sym: RuntimeSymbol,
    module: &mut M,
    imports: &mut HashMap<RuntimeSymbol, FuncRef>,
) -> Result<()> {
    let flags = MemFlags::trusted();

    let probe = builder.create_block();
    let fast = builder.create_block();
    let slow = builder.create_block();
    let merge = builder.create_block();
    // Cold-block placement runs in the machine-independent lowering
    // (`BlockLoweringOrder` reads `Layout::is_cold`), so it applied at
    // `opt_level = "none"` — the level this was measured at — and applies at the
    // `"speed"` the tree runs now, the mechanism being the same either way. What
    // the level does change is the block *set*: `module.rs` records `collatz`
    // going 38 cold blocks to 34. Both edges into this one are "unlikely", and
    // neither is on the path a loop counter takes.
    builder.set_cold_block(slow);

    // (1) The pacing predicate, ahead of everything. `Heap::collection_is_due`
    // is the one statement of it; this is its second reader and the only one
    // that cannot call it.
    let heap = builder
        .ins()
        .load(GC, flags, ctx_val, site.heap_offset() as i32);
    let since = builder
        .ins()
        .load(GC, flags, heap, site.bytes_since_collect_offset() as i32);
    let threshold = builder
        .ins()
        .load(GC, flags, heap, site.collect_threshold_offset() as i32);
    let due = builder
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, since, threshold);
    builder.ins().brif(due, slow, &[], probe, &[]);

    // (2) Membership: one subtract, one unsigned compare.
    builder.switch_to_block(probe);
    // `_s`: the addend is `-min`, which is negative whenever the table's floor
    // is positive. Sign-extending it is what keeps this the same subtraction
    // `index_of` performs for a range that does not straddle zero.
    let index = builder.ins().iadd_imm_s(value, site.min().wrapping_neg());
    let in_range =
        builder
            .ins()
            .icmp_imm_u(IntCC::UnsignedLessThanOrEqual, index, site.span() as i64);
    // The out-of-range edge. Before ADR-119 it went straight to `slow`; it now
    // goes to a claim sequence that bails to `slow`, so the wrapper is reached
    // on three conditions instead of two and on none of them has anything been
    // written. `tree` and `pipeline` are the two benchmarks whose `Materialize`s
    // mostly land here, and this edge is the +2.0%/+1.4% ADR-113 recorded owing.
    let out_of_range = if INLINE_SCALAR_CLAIM {
        builder.create_block()
    } else {
        slow
    };
    builder.ins().brif(in_range, fast, &[], out_of_range, &[]);

    // (3) The table read `Inst::ConstGc` already emits for a literal, with the
    // index computed at run time instead of folded — `load_gc_const`'s
    // `GcConst::SmallInt` arm is the same two loads with a constant
    // displacement.
    builder.switch_to_block(fast);
    {
        let offset = builder
            .ins()
            .ishl_imm_u(index, i64::from(site.stride_shift()));
        let base = builder
            .ins()
            .load(GC, flags, ctx_val, site.table_offset() as i32);
        let slot = builder.ins().iadd(base, offset);
        let interned = builder.ins().load(GC, flags, slot, 0);
        builder.def_var(dst, interned);
        builder.ins().jump(merge, &[]);
    }

    // (3b) The claim, for a value the table does not hold (ADR-119). It is
    // reached only through `probe`, which is reached only on the
    // `collection_is_due == false` edge above — which is decision 1 part 1, and
    // `the_inline_claim_is_dominated_by_the_pacing_branch` asserts it against the
    // emitted CFG's dominator tree rather than against this comment.
    if INLINE_SCALAR_CLAIM {
        builder.switch_to_block(out_of_range);
        emit_inline_claim(
            builder,
            ctx_val,
            heap,
            value,
            dst,
            claim,
            BuiltinTypeId::Int,
            slow,
            merge,
        );
    }

    // (4) The wrapper, unchanged: same `#[no_mangle]`, same `abi_guard!`, same
    // manifest row, same address arm. It is what paces when a collection is due
    // and what allocates when the value is out of range, and keeping it as the
    // callee is what makes "the answer is what it always was" a property of the
    // code rather than of this comment.
    builder.switch_to_block(slow);
    {
        let result = call_symbol(builder, ctx_val, &[value], sym, module, imports)?;
        builder.def_var(dst, result);
        builder.ins().jump(merge, &[]);
    }

    // `def_var` in both arms rather than a block parameter, as `emit_scalar_load`
    // does — and the builder is left switched to `merge`, which `lower_inst`'s
    // caller relies on: `spill.store_debug_defs` runs immediately afterwards and
    // must land where both arms are visible.
    builder.switch_to_block(merge);
    Ok(())
}

/// Box a scalar that has no intern table: the pacing test, then the inline
/// claim, with the allocating wrapper cold behind both (ADR-119).
///
/// ```text
/// hot:   heap  = load.i64 [ctx  + site.heap_offset()]
///        since = load.i64 [heap + site.bytes_since_collect_offset()]
///        thr   = load.i64 [heap + site.collect_threshold_offset()]
///                brif icmp uge since, thr, slow, claim
/// claim: (the sequence in `emit_inline_claim`, bailing to slow)
/// slow:  (cold) r = call praxis_alloc_float(ctx, value)
/// ```
///
/// **`Float` had no pacing test at all before this** — the arm was an
/// unconditional `call_symbol`, and the wrapper paced. So unlike
/// [`emit_inline_intern`], where the compare was already emitted and this only
/// re-points one edge, here the compare is new; it is the whole of ADR-040's
/// obligation and it is emitted first for the same reason.
///
/// `Char` deliberately does not come here. Its manifest row is
/// `AllocatesAndFaults`: an invalid code point must reach the wrapper that
/// raises `InvalidChar` with `CheckFault`'s diversion (RT-18), so an inline arm
/// would have to reproduce a *fault*, not just a claim. ADR-113 left it out for
/// the same reason and the reason has not changed.
#[allow(clippy::too_many_arguments)] // The lowering context, as `lower_inst` carries it.
fn emit_inline_claim_box<M: Module>(
    builder: &mut FunctionBuilder,
    ctx_val: Value,
    value: Value,
    dst: Variable,
    claim: praxis_runtime::InlineClaimSite,
    builtin: BuiltinTypeId,
    sym: RuntimeSymbol,
    module: &mut M,
    imports: &mut HashMap<RuntimeSymbol, FuncRef>,
) -> Result<()> {
    let flags = MemFlags::trusted();

    let start = builder.create_block();
    let slow = builder.create_block();
    let merge = builder.create_block();
    builder.set_cold_block(slow);

    let heap = builder
        .ins()
        .load(GC, flags, ctx_val, claim.heap_offset() as i32);
    let since = builder
        .ins()
        .load(GC, flags, heap, claim.bytes_since_collect_offset() as i32);
    let threshold = builder
        .ins()
        .load(GC, flags, heap, claim.collect_threshold_offset() as i32);
    let due = builder
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, since, threshold);
    builder.ins().brif(due, slow, &[], start, &[]);

    builder.switch_to_block(start);
    emit_inline_claim(
        builder, ctx_val, heap, value, dst, claim, builtin, slow, merge,
    );

    builder.switch_to_block(slow);
    {
        let result = call_symbol(builder, ctx_val, &[value], sym, module, imports)?;
        builder.def_var(dst, result);
        builder.ins().jump(merge, &[]);
    }

    // Left switched to `merge`, for `emit_inline_intern`'s reason:
    // `spill.store_debug_defs` runs immediately after `lower_inst` and must land
    // where both arms are visible.
    builder.switch_to_block(merge);
    Ok(())
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

/// The built-in id, payload alignment and load width for a scalar kind whose
/// payload generated code may read inline — or `None` for one it may not.
///
/// A new [`praxis_mir::ScalarKind`] variant fails to compile here, which is the
/// point: this is the second statement of a mapping whose first is
/// `ScalarKind::load_symbol` (MIR-10), and the two must not drift. They cannot
/// disagree about *which* type is being read, because the cold path below calls
/// `load_symbol()` — this only adds which descriptor proves it and how wide the
/// read is.
///
/// **The first column is an id and not an address, as of ADR-116**, and that is
/// the change: the proof compares against
/// `[ctx + RuntimeContext::descriptor_offset(id)]`, so the slot the load names
/// and the descriptor it proves are one value and cannot be given separately.
/// Before, this answered `&scalars::INT` and the backend baked the address in
/// as an `iconst` — three `movz`/`movk` on aarch64, where the load is one
/// instruction from a line the prologue has already touched.
fn inline_scalar_load_of(
    scalar: praxis_mir::ScalarKind,
) -> Option<(praxis_runtime::descriptor::BuiltinTypeId, usize, ScalarLoad)> {
    use praxis_mir::ScalarKind;
    use praxis_runtime::descriptor::BuiltinTypeId;
    use praxis_runtime::scalars;
    Some(match scalar {
        ScalarKind::Int => (
            BuiltinTypeId::Int,
            core::mem::align_of::<scalars::IntPayload>(),
            ScalarLoad::Word,
        ),
        ScalarKind::Bool => (
            BuiltinTypeId::Bool,
            core::mem::align_of::<scalars::BoolPayload>(),
            ScalarLoad::BoolByte,
        ),
        ScalarKind::Char => (
            BuiltinTypeId::Char,
            core::mem::align_of::<scalars::CharPayload>(),
            ScalarLoad::HalfWord,
        ),
        ScalarKind::Float => (
            BuiltinTypeId::Float,
            core::mem::align_of::<scalars::FloatPayload>(),
            ScalarLoad::Word,
        ),
        // `Byte` is reserved and unwired, and it has no wrapper to inline: since
        // ADR-108's companion change its `load_symbol()` and `alloc_symbol()`
        // both refuse rather than answering `IntLoad`/`AllocInt`. They used to
        // answer the `Int` ones "defensively" while nothing emitted them, which
        // was an eight-byte read of a one-byte payload waiting for the day
        // `Byte` was wired. Returning `None` here is the same refusal one layer
        // up, and it is what keeps this arm from ever reaching the panic: an
        // inline form would be REP-37 by construction anyway, because the
        // descriptor check would prove `INT` of a value that is not one.
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
    let Some((builtin, payload_align, load)) = inline_scalar_load_of(scalar) else {
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
    let want = descriptor_address(builder, ctx_val, builtin);
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

/// Lower `Inst::IntBinOp` (§4.12), reporting overflow the way `report` says.
///
/// A function rather than the body of [`lower_inst`]'s arm because it has two
/// callers: that arm, and the block loop's fused [`Step`], which reaches it
/// with `RaiseExit::Folded` (ADR-117). The alternative — a parameter on
/// `lower_inst` — would be a fault target that twenty-odd other arms ignore,
/// and the one arm that reads it would be the only thing saying which of them
/// it meant.
#[allow(clippy::too_many_arguments)]
fn lower_int_binop<M: Module>(
    builder: &mut FunctionBuilder,
    op: IntBinOp,
    dst: LocalId,
    lhs: LocalId,
    rhs: LocalId,
    report: OverflowReport,
    ctx_val: Value,
    vars: &[Variable],
    module: &mut M,
    imports: &mut HashMap<RuntimeSymbol, FuncRef>,
) -> Result<()> {
    // Native scalar arithmetic (§4.12). The operands are already raw i64s in the
    // scalar channel, so the operation is one Cranelift instruction plus an
    // inline overflow predicate.
    //
    // This replaces boxing both operands with `praxis_alloc_int`, calling the
    // wrapper, and `praxis_int_load`ing the result: two allocations and three
    // calls per arithmetic op. That shape also carried a live memory bug — on
    // fault the wrapper returns the Unit sentinel, and the `int_load` ran
    // *before* the fault check, reading eight bytes past a size-0 Unit payload.
    //
    // Overflow is reported by branching to a cold block that calls a
    // non-allocating raise wrapper — see `raise_on_cold_path`, which carries the
    // argument for why a branch beats the unconditional call this used to emit.
    // The site is still not a GC safepoint and still spills no roots. What
    // *diverts* to the fault epilogue is `report`'s business: at
    // `RaiseExit::Observed` it is the `Inst::CheckFault` MIR emits next, reached
    // through the block both arms of the diamond converge on; at
    // `RaiseExit::Folded` it is the cold block itself, and that check emits
    // nothing (ADR-117).
    let l = builder.use_var(vars[lhs.0 as usize]);
    let r = builder.use_var(vars[rhs.0 as usize]);
    let exit = match report {
        // `Overflow::Bounded` sites — a `for` index bump, a `count` accumulator
        // — skip the test entirely: their operands are bounded by a collection's
        // length, so the predicate is provably false and computing it cost two
        // instructions and a call per iteration.
        OverflowReport::Bare => {
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
        OverflowReport::Checked(exit) => exit,
    };
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
                exit,
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
                exit,
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
                exit,
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
            // raises were straight-line calls. Mutual exclusion is also
            // why `RaiseExit::Folded` may be given to *both*: at most
            // one cold block runs, so the fault epilogue is entered with
            // the kind the raise that ran set, exactly as the single
            // `CheckFault` downstream would have seen it.
            raise_on_cold_path(
                builder,
                ctx_val,
                by_zero,
                RuntimeSymbol::RaiseDivByZeroIf,
                exit,
                module,
                imports,
            )?;
            raise_on_cold_path(
                builder,
                ctx_val,
                overflows,
                RuntimeSymbol::RaiseIntOverflowIf,
                exit,
                module,
                imports,
            )?;
            value
        }
    };
    // Reached only when no raise fired, at either exit: at `Folded` the raising
    // path left for the epilogue and never comes back, and at `Observed` the
    // `Inst::CheckFault` below diverts before anything reads `dst`. So the value
    // stored here is a value the program is entitled to, and on the fault path
    // `dst` is simply never defined — which is what the debugger renders as
    // `<uninit>` for the operation that faulted, as it did before ADR-117.
    builder.def_var(vars[dst.0 as usize], result);
    // No fault check here. There used to be a bare `praxis_check_fault` call at
    // this point whose result was discarded and which no branch followed — a
    // leftover from before MIR carried `Inst::CheckFault`, costing one call per
    // checked arithmetic op and diverting nothing. What observes the raise is
    // the `Inst::CheckFault` the builder emits next, which the MIR verifier
    // requires to be there (MIR-10) — either as emitted code, or, at
    // `RaiseExit::Folded`, as the branch this function already made (ADR-117).
    Ok(())
}

/// What an `Inst::IntBinOp` site does about overflow.
///
/// Two facts about one site, and pairing them in one value is what keeps the
/// third combination — a bounded site handed a fault target — from existing:
/// bounded arithmetic emits no raise at all, so a fault target given to one
/// would be silently dropped, and the `Inst::CheckFault` it was folded out of
/// would be gone with it. That combination is a program that runs past an
/// overflow, and this enum is why it cannot be written.
#[derive(Clone, Copy)]
enum OverflowReport {
    /// `Overflow::Bounded` (ADR-044 decision 6): the bare instruction and no
    /// test, because the site's operands are bounded by a collection's length.
    /// There is no fault to route, which is why this variant carries none.
    Bare,
    /// `Overflow::Checked` (§4.12): a predicate and a cold-block raise, leaving
    /// by the given exit.
    Checked(RaiseExit),
}

/// Where the cold block that reports a fault goes when the raise wrapper
/// returns.
#[derive(Clone, Copy)]
enum RaiseExit {
    /// Back to the join, which is where the `Inst::CheckFault` that ADR-088 puts
    /// after this instruction lowers. Both arms of the diamond converge before
    /// it, so the check runs on the raising path and the non-raising path alike.
    /// ADR-102's shape, and still the one a checked site takes when its check
    /// was not fused into a [`Step`].
    Observed,
    /// Straight to the function's fault epilogue, because the `Inst::CheckFault`
    /// was folded into this branch and emits nothing (ADR-117). Reachable only
    /// through [`StepKind::RaiseIntoFault`], whose construction is the proof
    /// that the check exists and observes this instruction and nothing else.
    Folded(Block),
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
    exit: RaiseExit,
    module: &mut M,
    imports: &mut HashMap<RuntimeSymbol, FuncRef>,
) -> Result<()> {
    let cond = builder
        .ins()
        .icmp_imm_s(IntCC::SignedLessThan, predicate, 0);
    raise_on_cold_path(builder, ctx, cond, sym, exit, module, imports)
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
/// # ADR-088 is untouched, and `exit` is where its rule is now discharged
///
/// The rule that a faulting instruction is observed by the next one is a
/// property of *MIR* (`verify::check_fault_observed`), and this emits no MIR.
/// What changed under ADR-117 is *which emitted branch* discharges it.
///
/// At [`RaiseExit::Observed`] both arms of the diamond converge at `cont` before
/// the `Inst::CheckFault` that MIR requires next lowers, so the check runs on
/// the raising path and the non-raising path alike — as it did when the raise
/// was straight-line. **That sentence is ADR-102's Consequences, and it is no
/// longer the whole of it.** At [`RaiseExit::Folded`] the arms do not converge:
/// the cold block jumps to the function's fault epilogue and the `CheckFault`
/// emits nothing at all. The invariant that survives both is the weaker and true
/// one — *on the raising path, control reaches the fault epilogue before any
/// instruction after the raise runs* — and at `Folded` it holds because this is
/// the only branch on the way there, where at `Observed` it holds because the
/// check re-reads what this cold block wrote.
///
/// The fold is sound because a `CheckFault` immediately after checked `Int`
/// arithmetic can observe *nothing else*: every earlier faulting instruction in
/// the block has its own check by ADR-088's converse
/// (`VerifyError::RedundantFaultCheck`), so a fault raised earlier has already
/// diverted and `pending_fault` is clear when this block runs. See ADR-117.
fn raise_on_cold_path<M: Module>(
    builder: &mut FunctionBuilder,
    ctx: Value,
    cond: Value,
    sym: RuntimeSymbol,
    exit: RaiseExit,
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
    // No block parameters at either target. The raise wrapper returns `Void`, so
    // no value crosses the join; and the fault epilogue takes none either — it
    // reads only the prologue's frame bases and `ctx`, all defined in the entry
    // block, which dominates this one. That is what makes `Folded` a jump rather
    // than an edge that has to carry the arithmetic's `dst`, and it is checked
    // by `a_folded_raise_jumps_to_the_fault_epilogue_with_no_arguments`.
    let target = match exit {
        RaiseExit::Observed => cont,
        RaiseExit::Folded(fault) => fault,
    };
    builder.ins().jump(target, &[]);

    // Everything the caller computed before the branch was defined in a block
    // that dominates `cont`, so it is readable there without a block parameter.
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
    // (HIR-01/MONO-01, hazard H10): `var m = Map()` generalizes at the `var`, so
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

/// Build this function's whole static debug metadata — its name, its source
/// span, and the `[DebugLocalMeta]` array for its `Gc` locals — in the
/// generation arena, and answer the address the prologue writes into its
/// `DebugFrameEntry` (§9.3, ADR-104).
///
/// **The locals are in the same order as `debug_slot`**, because this walk and
/// that one are the same walk — `mir.locals` filtered to `LocalKind::Gc`, in
/// order — so entry `i` describes the word `store_debug_local` writes at
/// displacement `i`. That equality is the premise the runtime reads this array
/// under, and `lower_fn` asserts the count half of it against `debug_count`
/// immediately after this returns.
///
/// It used to say "so a local's shadow-slot index doubles as its debug-local
/// index". That stopped being true at ADR-128 decision 2, where a shadow slot
/// became a colour; the debug space is the one that stayed dense, and this
/// function is unchanged precisely because it was always describing *that* one.
///
/// Each entry carries the source name (interned in the same arena, empty for
/// temps), a
/// per-local symbol-id placeholder, the static type descriptor resolved from the
/// MIR local's `Type` (§9.3, M10-WS2), the user-vs-temp classification, and the
/// source span.
///
/// The span is `mir.span`, threaded AST → HIR `TypedFn` → MIR `Function.span`
/// → here (ADR-035 decision 3). Its last hop used to be
/// `praxis_set_frame_source_span(ctx, start, end)` — a runtime call, in every
/// prologue, to record a compile-time constant. Only where it is written
/// changed; the crash debugger's `source` command reads the same two numbers out
/// of the same `SnapshotFrame` field.
///
/// Everything is deduplicated by content: a function lowered twice into one
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
fn build_function_debug_meta(
    mir: &MirFunction,
    db: &mut praxis_types::TypeDb,
    generation: &Generation,
) -> *const FunctionDebugMeta {
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
        // What this local's value slot holds (ADR-120 part 2). `Reference`
        // unless the forwarding pass elided this box, in which case the slot is
        // fed by a `Scalar` local's definition and holds that payload's raw
        // word. The map is total over `ScalarKind`, so a scalar source can never
        // come out as `Reference` — which is the one answer that would put a
        // payload into the collector's post-sweep scan.
        //
        // Note what this does *not* change: `type_id` and `descriptor` are
        // still the box's own, so `render_local_line`'s type column still says
        // `Int`. `ir.rs`'s doctrine that a `Scalar` local is always
        // `MirType::Opaque` is untouched, because the local that owns this
        // metadata is the `Gc` one — the scalar never enters this loop.
        let slot_kind = match mir.debug_scalar_source(local.id) {
            None => DebugSlotKind::Reference,
            Some((_, ScalarKind::Int)) => DebugSlotKind::Int,
            Some((_, ScalarKind::Bool)) => DebugSlotKind::Bool,
            Some((_, ScalarKind::Float)) => DebugSlotKind::Float,
            Some((_, ScalarKind::Char)) => DebugSlotKind::Char,
            Some((_, ScalarKind::Byte)) => DebugSlotKind::Byte,
        };
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
            slot_kind,
        });
        symbol_id += 1;
    }
    // Interned so the same function lowered twice into one generation costs one
    // copy of the name as well as one copy of the metadata.
    generation.function_debug_meta(generation.alloc_str(&mir.name), mir.span, metas)
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
/// - a `Known` type that is still an inference *variable* — `var xs = Vec()`
///   generalizes at the `var`, so the construction site's own element type is
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

    /// A `JITModule` configured exactly as `Jit::in_generation` configures its
    /// own — `crate::module::CRANELIFT_FLAGS`, not Cranelift's defaults.
    ///
    /// **The two no longer agree**, and that is what makes sharing the constant
    /// load-bearing rather than tidy. `CRANELIFT_FLAGS` is
    /// `opt_level = "speed"`; Cranelift's default is `"none"`. A test that built
    /// its own `JITBuilder` from the defaults would run the mid-end not at all
    /// where the real compile path runs it, so it would assert on code no
    /// `praxis run` ever emits. A test that asserts on the emitted shape, and a
    /// `PRAXIS_DUMP_CLIF` run of the same program, must be looking at the same
    /// code. (This comment said the opposite until ADR-128, from back when the
    /// two did agree.)
    fn test_module() -> JITModule {
        let builder = JITBuilder::with_flags(
            crate::module::CRANELIFT_FLAGS,
            cranelift_module::default_libcall_names(),
        )
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
    /// synthesis gave every symbol an `i64` return, so a call to a `Void`
    /// wrapper read a result register the callee never wrote. (The two debug
    /// wrappers this used to name — `praxis_pop_debug_frame` and
    /// `praxis_set_frame_source_span` — are gone with ADR-104; the property is
    /// not about them, and `every_symbol_has_a_derivable_signature` checks it
    /// over the whole manifest.)
    #[test]
    fn void_wrappers_declare_no_result() {
        let module = test_module();
        assert!(signature_for(RuntimeSymbol::SnapshotDebugChain, &module)
            .returns
            .is_empty());
        assert!(signature_for(RuntimeSymbol::RaiseStackOverflow, &module)
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
            debug_scalar_sources: Vec::new(),
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
        let meta = build_function_debug_meta(&f, &mut db, &generation);
        // SAFETY: `build_function_debug_meta` returns a record owned by
        // `generation`, which outlives this borrow, with `local_count`
        // initialized `locals` entries behind it.
        let meta = unsafe { &*meta };
        assert_eq!(meta.local_count, 2);
        let metas = unsafe { std::slice::from_raw_parts(meta.locals, 2) };
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
        let (func, entry) = emitted_function(build);
        let all = func.display().to_string();
        let entry_text = block_text(&all, entry);
        (all, entry_text)
    }

    /// [`emitted_ir`]'s scaffolding, answering the finished
    /// `codegen::ir::Function` and its entry block instead of text.
    ///
    /// Text is enough for "which block calls" and not for "which block
    /// dominates which", which is what ADR-118 part 2's proof-before-load claim
    /// actually says — so the two share one builder rather than growing a
    /// second copy that could drift in signature or sealing.
    fn emitted_function(
        build: impl FnOnce(&mut FunctionBuilder, Value, Variable, &mut JITModule) -> Result<()>,
    ) -> (codegen::ir::Function, Block) {
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

        (ctx.func, entry)
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
    /// 16 for all four scalars — the same number `Inst::EnumTag` folds — and
    /// Cranelift prints the displacement as `+16`. (It was 24 until ADR-109
    /// deleted `GcHeader::size`; this helper derives the number from
    /// `payload_offset_for` rather than spelling it, so only this sentence
    /// needed the edit, which is ADR-039 Decision 1 doing its job.)
    ///
    /// **The displacement is matched as a whole address token, not as a
    /// substring, and that is not fastidiousness.** Cranelift prints an address
    /// as `vN+DISP`, and `"v0+168".contains("+16")` is true. ADR-116 put the
    /// built-in descriptor table in the `RuntimeContext` at an offset that puts
    /// `Char`'s slot at exactly +168, so a substring match found the *context*
    /// load as well as the payload load and this helper's own
    /// "more than one instruction reads at +16" assertion fired — in a test
    /// about payload widths, for one of three scalars, with `Int` (+152) and
    /// `Float` (+176) passing beside it. Splitting on whitespace and asking
    /// which token *ends* with `+16` is exact: the `+` is the token's only one,
    /// so nothing longer can end with it.
    fn payload_load(ir: &str, align: usize) -> String {
        let displacement = format!("+{}", praxis_runtime::GcHeader::payload_offset_for(align));
        let reads_there = |l: &str| {
            l.split_whitespace()
                .any(|token| token.trim_end_matches(',').ends_with(&displacement))
        };
        let mut hits = ir
            .lines()
            .filter(|l| reads_there(l) && !l.trim_start().starts_with(';'));
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

    /// **ADR-116, as an assertion about the instruction stream.** The proof's
    /// second operand is a load from the context, and no descriptor address
    /// appears in the emitted code at all.
    ///
    /// Nothing that runs a program can see this. An `iconst` of `&scalars::INT`
    /// and a load of the slot holding `&scalars::INT` compare the same word and
    /// admit the same values; what separates them is three `movz`/`movk` per
    /// site on aarch64 versus one `ldr`, which is a property of the emitted code
    /// and of nothing else. So this is the gate, in the shape ADR-102's own
    /// tests take for the same reason.
    ///
    /// The negative half is the one that survives a rewrite: an edit that
    /// re-introduced the immediate "just for `Int`" would still load the slot
    /// for the other three and still pass a test that only looked for the load.
    ///
    /// Arm-B-only, and that is how the measurement toggle is shown to bite:
    /// under `adr116-arm-a` the emitted code holds exactly the address this
    /// asserts is absent, so a feature that did nothing would leave this test
    /// green in both arms and the A/B would be comparing a binary with itself.
    #[cfg(not(feature = "adr116-arm-a"))]
    #[test]
    fn a_scalar_proof_loads_its_descriptor_from_the_context() {
        use praxis_mir::ScalarKind;
        use praxis_runtime::descriptor::BuiltinTypeId;

        for kind in [
            ScalarKind::Int,
            ScalarKind::Bool,
            ScalarKind::Char,
            ScalarKind::Float,
        ] {
            let (all, entry) = emitted_ir(move |b, ctx, dst, m| {
                let src = b.ins().iconst(GC, 0x1000);
                emit_scalar_load(b, ctx, src, dst, kind, m, &mut HashMap::new())
            });
            let (builtin, _, _) = inline_scalar_load_of(kind).expect("a wired scalar");
            let slot = format!("v0+{}", RuntimeContext::descriptor_offset(builtin));
            assert!(
                entry.split_whitespace().any(|t| t == slot),
                "{kind:?}: the descriptor must come from `{slot}`, the context \
                 slot its `BuiltinTypeId` indexes:\n{all}"
            );

            // And no built-in's address is materialized anywhere in the
            // function — not the one being proved and not a neighbour's. This
            // is the half that catches a partial revert, and it is checked
            // against the addresses *this process* has, so it says nothing
            // about where ASLR put them.
            for descriptor in praxis_runtime::descriptor::BUILTINS {
                let immediate = format!("iconst.i64 {}", descriptor as *const _ as i64);
                assert!(
                    !all.contains(&immediate),
                    "{kind:?}: `{}`'s address is materialized as `{immediate}`; \
                     since ADR-116 the backend holds no descriptor address:\n{all}",
                    descriptor.name
                );
            }
        }

        // And the four slots are four different slots, so no two kinds can be
        // proved by the same load. `Bool`, `Int`, `Char`, `Float` are ids 1, 2,
        // 4 and 5 (`Byte` sits between and has no inline form), which is the
        // registry's order and not a list this file keeps.
        let mut offsets: Vec<usize> = [
            BuiltinTypeId::Int,
            BuiltinTypeId::Bool,
            BuiltinTypeId::Char,
            BuiltinTypeId::Float,
        ]
        .into_iter()
        .map(RuntimeContext::descriptor_offset)
        .collect();
        offsets.sort_unstable();
        offsets.dedup();
        assert_eq!(offsets.len(), 4, "each wired scalar has its own slot");
    }

    /// `ScalarKind::Byte` has no inline form, and that is not an oversight.
    #[test]
    fn a_reserved_byte_scalar_keeps_its_call() {
        assert!(
            inline_scalar_load_of(praxis_mir::ScalarKind::Byte).is_none(),
            "`Byte` has no wrapper to inline — `load_symbol()` refuses rather \
             than answering `IntLoad`, which would be an eight-byte read of a \
             one-byte payload. This arm must return `None` before that refusal \
             is reached, and inlining anything here would be REP-37 by \
             construction."
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
    ///
    /// Since ADR-116 the inline side names a [`BuiltinTypeId`] rather than an
    /// address, so this reads the descriptor back out of the registry that id
    /// indexes — which is the same registry `Runtime::context` fills the
    /// context's table from, so proving the identity here proves it of the
    /// address generated code will load.
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
            let (builtin, align, _) =
                inline_scalar_load_of(kind).expect("a wired scalar has an inline form");
            let inline_descriptor = builtin.descriptor();
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

    // -----------------------------------------------------------------------
    // ADR-113: the shape `Inst::Materialize { Int }` emits.
    //
    // Same reason as the block above, with one addition that makes these the
    // *only* gate on half of the change. The pacing test is invisible to every
    // behavioural test that could be written: a program cannot tell "the
    // collector was offered a turn and declined" from "the collector was not
    // offered a turn", because in the state the fast path runs in those two are
    // the same thing. What separates them is whether the compare is in the
    // instruction stream at all, and that is what these read.
    // -----------------------------------------------------------------------

    /// The emitted `Materialize { Int }`, as text plus its entry block.
    fn inline_intern_ir() -> (String, String) {
        emitted_ir(|b, ctx, dst, m| {
            let value = b.ins().iconst(GC, 7);
            emit_inline_intern(
                b,
                ctx,
                value,
                dst,
                praxis_runtime::small_int::INLINE_INTERN_SITE,
                praxis_runtime::scalars::INT_CLAIM_SITE,
                RuntimeSymbol::AllocInt,
                m,
                &mut HashMap::new(),
            )
        })
    }

    /// **The ADR-040 obligation, as an assertion about the instruction
    /// stream.**
    ///
    /// The inline path forges no `Safepoint` because it never collects and never
    /// allocates — but that argument holds only on the branch where
    /// `Heap::collection_is_due` is false, so the compare has to be there and it
    /// has to come first. Both pacing words are loaded in the entry block, they
    /// are compared, and the branch on the result is the entry block's
    /// terminator: nothing about the value being boxed has been looked at yet.
    ///
    /// This is the test that fails if someone "simplifies" the sequence by
    /// probing the table first and testing the counter only on the miss. That
    /// version is faster and it is the defect ADR-113 exists to forbid: a
    /// program whose pressure came from `Text` or `Vec` would have every
    /// collection deferred at every loop counter in between.
    #[test]
    fn an_inline_int_box_tests_the_pacing_counter_before_it_reads_the_table() {
        let (all, entry) = inline_intern_ir();
        let site = praxis_runtime::small_int::INLINE_INTERN_SITE;

        for (what, offset) in [
            ("the heap pointer", site.heap_offset()),
            ("bytes_since_collect", site.bytes_since_collect_offset()),
            ("collect_threshold", site.collect_threshold_offset()),
        ] {
            assert!(
                entry.contains(&format!("+{offset}")) || offset == 0,
                "{what} must be loaded in the entry block, at +{offset}:\n{all}"
            );
        }
        assert!(
            entry.contains("icmp uge"),
            "the pacing predicate is `since >= threshold`, unsigned — the same \
             compare `Heap::collection_is_due` applies:\n{all}"
        );
        let last = entry
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        assert!(
            last.starts_with("brif "),
            "and the branch on it is the entry block's terminator, so the \
             counter is tested before anything else; found `{last}`:\n{entry}"
        );
        assert!(
            !entry.contains(&format!("+{}", site.table_offset())),
            "the table base must NOT be read before the pacing branch: a load \
             there would be harmless, but it is the shape a reordering of this \
             sequence takes, and the reordering is the defect:\n{all}"
        );
    }

    /// The probe itself: one subtract, one unsigned compare against the span,
    /// and a scaled load out of the table — the immediates all off the site.
    #[test]
    fn an_inline_int_box_probes_the_intern_table_before_it_allocates() {
        let (all, entry) = inline_intern_ir();
        let site = praxis_runtime::small_int::INLINE_INTERN_SITE;

        // The `_imm` builders materialize their operand as an `iconst`, so each
        // immediate is asserted as the constant it becomes. Taking every one of
        // them off `site` is the point: not one of the three numbers below is
        // written in this file, and a range change in `small_int.rs` moves them
        // all without touching the backend.
        assert!(
            all.contains(&format!("iconst.i64 {}", site.min().wrapping_neg())),
            "the index is `value - min`, and `min` is the table's:\n{all}"
        );
        assert!(
            all.contains("icmp ule"),
            "membership is **one** unsigned compare, not two signed ones — \
             `(value - min) as u64 <= span` is `index_of` for every i64, and it \
             costs one branch where the two-compare form costs two:\n{all}"
        );
        assert!(
            all.contains(&format!("iconst.i64 {}", site.span())),
            "and the bound it compares against is the site's span ({}), the \
             table's own width rather than one this file wrote:\n{all}",
            site.span()
        );
        assert!(
            all.contains("ishl"),
            "the index is scaled by a shift, not a multiply — nothing \
             strength-reduces an `imul` at `opt_level = \"none\"`:\n{all}"
        );
        assert!(
            all.contains(&format!("iconst.i64 {}", site.stride_shift())),
            "by log2 of the table's stride:\n{all}"
        );
        assert!(
            all.contains(&format!("+{}", site.table_offset())),
            "the table base comes out of the context at the site's offset:\n{all}"
        );
        assert!(
            !entry.contains("call "),
            "and the hot path calls nothing at all — which is the whole change: \
             `praxis_alloc_int`'s `bl`, its `catch_unwind` landing pad, its \
             `RuntimeRoots::from_context` and its `maybe_collect` all stood in \
             front of a two-load table read:\n{all}"
        );
    }

    /// Exactly one block is cold, it is the one that calls `praxis_alloc_int`,
    /// and no hot block calls anything.
    ///
    /// `an_overflow_report_is_a_branch_to_a_cold_block` for the allocation path,
    /// with one difference worth pinning: this cold block has **two**
    /// predecessors — the pacing branch and the range branch — so "one cold
    /// block" also says the two bail-outs share a callee rather than each
    /// growing their own.
    #[test]
    fn the_only_block_that_calls_praxis_alloc_int_is_the_cold_one() {
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
        let value = builder.ins().iconst(GC, 7);
        emit_inline_intern(
            &mut builder,
            ctx_val,
            value,
            dst,
            praxis_runtime::small_int::INLINE_INTERN_SITE,
            praxis_runtime::scalars::INT_CLAIM_SITE,
            RuntimeSymbol::AllocInt,
            &mut module,
            &mut HashMap::new(),
        )
        .expect("emission");
        let out = builder.use_var(dst);
        builder.ins().return_(&[out]);
        builder.seal_all_blocks();
        builder.finalize(module.isa().frontend_config());

        let all = ctx.func.display().to_string();
        let cold: Vec<_> = ctx
            .func
            .layout
            .blocks()
            .filter(|&b| ctx.func.layout.is_cold(b))
            .collect();
        assert_eq!(cold.len(), 1, "exactly the wrapper block is cold:\n{all}");
        assert!(
            block_text(&all, cold[0]).contains("call "),
            "and it is the one that calls the wrapper:\n{all}"
        );
        for block in ctx.func.layout.blocks() {
            if ctx.func.layout.is_cold(block) {
                continue;
            }
            assert!(
                !block_text(&all, block).contains("call "),
                "no hot block may call: {block} does:\n{all}"
            );
        }
        // Two edges into the one cold block, which is what says the pacing
        // bail-out and the out-of-range bail-out share the wrapper.
        let edges = all.matches(&format!("{}", cold[0])).count();
        assert!(
            edges >= 3,
            "the cold block should be named by its label and by both branches \
             into it; found {edges} mentions:\n{all}"
        );
    }

    // -----------------------------------------------------------------------
    // ADR-119: the inline bitmap claim.
    //
    // These are the *whole* gate on decision 1, and for a stronger reason than
    // ADR-113's. There, a behavioural test could not see the pacing compare
    // because the two states are the same observable program. Here a
    // behavioural test cannot see any of the three parts: an object allocated
    // inline and an object allocated by the wrapper are the same object, a
    // collection that could not have run is indistinguishable from one that did
    // not, and a counter that is bumped in the wrong order is bumped. What the
    // three parts are claims about is the *emitted instruction stream* — its
    // dominator tree, its call sites and its stores — so that is what these
    // read.
    // -----------------------------------------------------------------------

    /// Two `Float` boxes in one function, so the second claim's guard is not the
    /// entry block's terminator, plus the emitted function.
    ///
    /// **The second site is the test.** ADR-113's pacing assertion asks "is the
    /// branch the entry block's terminator", which is true of a lowering with
    /// one guarded site in it and says nothing about a real function, where the
    /// hundredth `Materialize` is nowhere near the entry. Decision 1 part 1 is a
    /// dominance claim and this is the shape that can tell the two apart.
    #[cfg(not(feature = "adr119-arm-a"))]
    fn two_claim_sites() -> codegen::ir::Function {
        let (func, _entry) = emitted_function(|builder, ctx_val, dst, module| {
            let bits = builder.ins().iconst(GC, 0x3ff0_0000_0000_0000);
            for _ in 0..2 {
                emit_inline_claim_box(
                    builder,
                    ctx_val,
                    bits,
                    dst,
                    praxis_runtime::scalars::FLOAT_CLAIM_SITE,
                    BuiltinTypeId::Float,
                    RuntimeSymbol::AllocFloat,
                    module,
                    &mut HashMap::new(),
                )?;
            }
            Ok(())
        });
        func
    }

    /// The blocks of `func` that contain `needle`, in layout order.
    #[cfg(not(feature = "adr119-arm-a"))]
    fn blocks_containing(func: &codegen::ir::Function, needle: &str) -> Vec<Block> {
        let all = func.display().to_string();
        func.layout
            .blocks()
            .filter(|&b| block_text(&all, b).contains(needle))
            .collect()
    }

    /// The blocks that hold a claim site's **pacing** compare, in layout order.
    ///
    /// `icmp uge` alone is not enough and the reason is worth stating: the claim
    /// emits a second unsigned `>=`, the `cursor >= last_word` bail-out. The two
    /// are told apart by the *width of what they compare* — the pacing words are
    /// `usize` and are loaded whole, where `cursor` and `last_word` are `u32` and
    /// arrive through `uload32`. A guard block therefore has no `uload32` in it,
    /// and a test that matched the scan block instead would assert a dominance
    /// that holds for a reason other than the one decision 1 part 1 claims.
    #[cfg(not(feature = "adr119-arm-a"))]
    fn pacing_blocks(func: &codegen::ir::Function) -> Vec<Block> {
        let all = func.display().to_string();
        func.layout
            .blocks()
            .filter(|&b| {
                let text = block_text(&all, b);
                text.contains("icmp uge") && !text.contains("uload32")
            })
            .collect()
    }

    /// Every store in `block`, in order, as `(opcode, displacement)`.
    ///
    /// The displacement is what Cranelift prints after the base operand, so a
    /// bare `store … , v27` is displacement 0. Two of the eight displacements
    /// the claim writes to collide numerically — a `GcHeader`'s payload and a
    /// `Heap`'s `bytes_since_collect` are both `+16` — so nothing here reads a
    /// displacement on its own; the assertion is on the whole ordered list,
    /// which is also what makes it an assertion about the *order*.
    #[cfg(not(feature = "adr119-arm-a"))]
    fn stores_in_order(text: &str) -> Vec<(String, usize)> {
        text.lines()
            .filter_map(|line| {
                let line = line.trim_start();
                let opcode = line.split([' ', '.']).next()?;
                if !matches!(opcode, "store" | "istore16" | "istore32") {
                    return None;
                }
                // `<opcode> <flags> <value>, <base>[+<disp>]  ; <comment>`
                let operands = line.split(", ").nth(1)?;
                let base = operands.split([' ', ';']).next()?;
                let displacement = match base.split_once('+') {
                    Some((_, disp)) => disp.parse().ok()?,
                    None => 0,
                };
                Some((opcode.to_string(), displacement))
            })
            .collect()
    }

    /// **Decision 1 part 1 — entry.** Every store the claim performs is
    /// dominated, in the emitted CFG, by the branch on `collection_is_due`.
    ///
    /// The pacing block is identified by the compare itself (`icmp uge` of the
    /// two exported words) and the claim's store block by `ctz`, which is the
    /// free-bit selection and appears nowhere else in the backend. Both are
    /// found by *searching*, not by position, so a re-ordering of the emitter
    /// that moved the guard would fail here rather than silently re-point the
    /// assertion.
    #[cfg(not(feature = "adr119-arm-a"))]
    #[test]
    fn the_inline_claim_is_dominated_by_the_pacing_branch() {
        let func = two_claim_sites();
        let all = func.display().to_string();
        let paced = pacing_blocks(&func);
        let claimed = blocks_containing(&func, "ctz");
        assert_eq!(paced.len(), 2, "one pacing compare per site:\n{all}");
        assert_eq!(claimed.len(), 2, "one claim per site:\n{all}");
        // The second site's guard is not the entry block, which is what makes
        // this a dominance claim rather than ADR-113's shape claim.
        let entry = func.layout.entry_block().expect("a lowered function");
        assert_ne!(
            paced[1], entry,
            "the second site's pacing test must not be in the entry block, or \
             this test is not asking the question it means to:\n{all}"
        );
        for (guard, store) in paced.iter().zip(claimed.iter()) {
            assert_dominates(&func, *guard, *store);
        }
        // And it is genuinely the *pacing* compare that dominates: both
        // exported displacements are loaded in the guard block.
        let site = praxis_runtime::scalars::FLOAT_CLAIM_SITE;
        for guard in &paced {
            let text = block_text(&all, *guard);
            for offset in [
                site.bytes_since_collect_offset(),
                site.collect_threshold_offset(),
            ] {
                assert!(
                    text.contains(&format!("+{offset}")),
                    "{guard} compares two words it did not load at +{offset}:\n{all}"
                );
            }
        }
    }

    /// **Decision 1 part 2 — duration.** Between the pacing branch and the last
    /// store there is no call, so *not due on entry* implies *not due
    /// throughout*.
    ///
    /// This is the clause that replaces ADR-113's "the inline path allocates
    /// nothing", and it is the one that makes the claimed block unsweepable
    /// before its reference reaches a root slot. A collection can only begin
    /// inside `Heap::maybe_collect`, which generated code reaches only through a
    /// wrapper call — so "no call on the path" *is* "no collection on the path",
    /// and there is nothing weaker to check.
    #[cfg(not(feature = "adr119-arm-a"))]
    #[test]
    fn nothing_between_the_pacing_branch_and_the_last_store_can_collect() {
        let func = two_claim_sites();
        let all = func.display().to_string();
        for (block, text) in hot_blocks(&func) {
            assert!(
                !text.contains("call "),
                "no hot block may call, and {block} does — a call is a point at \
                 which a collection can begin, and this sequence has a \
                 half-written heap in front of it:\n{all}"
            );
        }
        let cold: Vec<_> = func
            .layout
            .blocks()
            .filter(|&b| func.layout.is_cold(b))
            .collect();
        assert_eq!(cold.len(), 2, "one wrapper block per site:\n{all}");
        for block in cold {
            assert!(
                block_text(&all, block).contains("call "),
                "{block} is cold because it calls the wrapper:\n{all}"
            );
        }
        // The claim's own chain is straight-line into the store block: every
        // block that `ctz`'s block is reachable from, inside the sequence,
        // branches only to the next link or to a cold bail-out. Stated as: the
        // store block has exactly one instruction that leaves it, and it is a
        // `jump`.
        for store in blocks_containing(&func, "ctz") {
            let text = block_text(&all, store);
            assert!(
                !text.contains("brif"),
                "the store block must not branch — a bail-out after the first \
                 store would leave a header written and no allocated bit:\n{all}"
            );
            assert_eq!(
                text.matches("jump ").count(),
                1,
                "…and it falls through to the merge exactly once:\n{all}"
            );
        }
    }

    /// **Decision 1 part 3 — state.** The claim writes exactly the words
    /// `alloc_raw → claim_block → occupy` writes, at the displacements the site
    /// carries, in the order header → payload → `allocated` → counters.
    ///
    /// The order is not what makes the sequence safe — part 2 is — and this test
    /// does not claim otherwise. What it pins is *completeness*: both live
    /// counters and the pacing charge. Both counters are decremented elsewhere
    /// and never recomputed (`sweep` by what it reclaimed, `release_blocks` by
    /// what a page freed), so a skipped increment does not decay into a wrong
    /// statistic — it underflows, and `relink_pages` then reads a page holding
    /// live blocks as empty and lets `reclass` hand its storage to another
    /// layout.
    ///
    /// The displacements come off the site rather than being written here, so
    /// this is checking the *shape*; that they name the right fields is
    /// `the_claim_site_displacements_name_the_fields_they_claim_to` in
    /// `praxis-runtime`, which reads a live heap through every one of them.
    #[cfg(not(feature = "adr119-arm-a"))]
    #[test]
    fn the_inline_claim_writes_every_word_the_wrapper_would() {
        let func = two_claim_sites();
        let all = func.display().to_string();
        let site = praxis_runtime::scalars::FLOAT_CLAIM_SITE;
        let store = blocks_containing(&func, "ctz")[0];
        let text = block_text(&all, store);

        // **The whole of decision 1 part 3, as one list.** Eight stores, in this
        // order, at these displacements, with these widths. Asserting the list
        // rather than eight separate `contains` is what makes it simultaneously
        // a completeness claim (a missing counter shortens the list), an order
        // claim (the severity ranking) and a displacement claim — and it is the
        // only form that survives two of the displacements colliding: a
        // `GcHeader`'s payload and a `Heap`'s `bytes_since_collect` are both
        // `+16`, so no single line identifies itself.
        let expected: Vec<(&str, usize)> = vec![
            // (1) The header. The descriptor first: this is the store whose
            // absence is unrecoverable, because a sweep reaching an allocated
            // bit over an unwritten header would read those bytes as a
            // `*const TypeDescriptor` and call through `drop_value`.
            ("store", site.header_descriptor_offset()),
            ("istore16", site.header_payload_offset_offset()),
            ("istore32", site.header_heap_id_offset()),
            // (2) The payload.
            ("store", site.payload_offset()),
            // (3) The `allocated` bit. The block becomes sweep-visible here and
            // not before.
            ("store", site.page_allocated_offset()),
            // (4) Both live counters and the pacing charge. Every one of these
            // is decremented elsewhere and never recomputed, so a skipped bump
            // underflows rather than decays — and `relink_pages` then puts a
            // page holding live blocks on the empty pool.
            ("istore32", site.page_live_count_offset()),
            ("store", site.heap_live_count_offset()),
            ("store", site.bytes_since_collect_offset()),
        ];
        let found = stores_in_order(&text);
        assert_eq!(
            found
                .iter()
                .map(|(op, disp)| (op.as_str(), *disp))
                .collect::<Vec<_>>(),
            expected,
            "the claim must write exactly what `alloc_raw` -> `claim_block` -> \
             `occupy` writes, in the order ADR-119 decision 1 ranks them:\n{all}"
        );

        // And the charge is the stride and nothing else. The `owned_bytes` term
        // `Heap::occupy` also adds is zero here by construction rather than by
        // this arm's choice — `InlineClaimSite::of` refuses every descriptor
        // that carries the callback.
        assert!(
            text.contains(&format!("iconst.i64 {}", site.stride())),
            "the pacing charge is the block stride, folded:\n{all}"
        );
    }

    /// The stride and the block geometry reach the backend as immediates, not as
    /// loads off the page header.
    ///
    /// Handover 27 §9 registered this as unverified: `BlockLayout::of` and
    /// `SizeClass::of` are const-shaped, but nobody had checked that
    /// const-evaluation reaches the emitted CLIF as a usable constant rather
    /// than needing a new accessor. It does — both are `iconst` — and the reason
    /// is that they are const-evaluated in `praxis-runtime`, in
    /// `InlineClaimSite::of`, and arrive here as numbers.
    #[cfg(not(feature = "adr119-arm-a"))]
    #[test]
    fn the_block_geometry_is_folded_rather_than_read_off_the_page() {
        let func = two_claim_sites();
        let all = func.display().to_string();
        let site = praxis_runtime::scalars::FLOAT_CLAIM_SITE;
        for (what, value) in [
            ("the stride", site.stride()),
            ("the first block's displacement", site.first_block()),
            ("the payload displacement", site.payload_offset()),
        ] {
            assert!(
                all.contains(&format!("iconst.i64 {value}")),
                "{what} must be an immediate; a load would be a word off the \
                 page header on the hottest path in the language:\n{all}"
            );
        }
    }

    /// One `raise_on_cold_path` at the given exit, as IR text, plus the entry
    /// block's own text and the one cold block's label.
    ///
    /// A closure over the exit rather than two copies: the two arms differ in
    /// exactly one instruction — the cold block's `jump` — and a second copy of
    /// the scaffolding would let the shared claims drift apart.
    fn raise_ir(exit: impl FnOnce(Block) -> RaiseExit) -> (String, String, Block) {
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
        // Stands in for the function's `Terminator::Fault` block: reachable,
        // takes no parameters, returns. Both facts are what the fold needs.
        let fault = builder.create_block();
        let cond = builder.ins().iconst(types::I8, 0);
        raise_on_cold_path(
            &mut builder,
            ctx_val,
            cond,
            RuntimeSymbol::RaiseIntOverflowIf,
            exit(fault),
            &mut module,
            &mut HashMap::new(),
        )
        .expect("emission");
        builder.ins().return_(&[]);
        builder.switch_to_block(fault);
        builder.ins().return_(&[]);
        builder.seal_all_blocks();
        builder.finalize(module.isa().frontend_config());

        let all = ctx.func.display().to_string();
        let cold: Vec<_> = ctx
            .func
            .layout
            .blocks()
            .filter(|&b| ctx.func.layout.is_cold(b))
            .collect();
        assert_eq!(cold.len(), 1, "exactly the raise block is cold:\n{all}");
        let entry_text = block_text(&all, entry);
        (all, entry_text, cold[0])
    }

    /// The overflow report is a branch to a cold block, not a call per op.
    #[test]
    fn an_overflow_report_is_a_branch_to_a_cold_block() {
        let (all, entry_text, cold) = raise_ir(|_| RaiseExit::Observed);
        assert!(entry_text.contains("brif"), "{all}");
        assert!(
            !entry_text.contains("call "),
            "the arithmetic site branches; it does not call on the hot path:\n{all}"
        );
        assert!(
            block_text(&all, cold).contains("call "),
            "and the cold block is the one that calls the wrapper:\n{all}"
        );
    }

    /// At `RaiseExit::Observed` the cold block rejoins the hot path, where the
    /// `Inst::CheckFault` that MIR emits next reads the slot the wrapper wrote.
    /// This is ADR-102's shape and it is still what an unfused site gets.
    #[test]
    fn an_observed_raise_rejoins_the_hot_path() {
        let (all, entry_text, cold) = raise_ir(|_| RaiseExit::Observed);
        // The join is the `brif`'s second target, which is also where the cold
        // block goes: three mentions of one label, exactly as ADR-102 left it.
        let join = entry_text
            .split_once("brif")
            .and_then(|(_, rest)| rest.rsplit_once(", "))
            .map(|(_, tail)| tail.trim().trim_end_matches(&['(', ')'][..]).to_string())
            .expect("the entry block branches");
        let join = join.split_whitespace().next().unwrap().to_string();
        assert!(
            block_text(&all, cold).contains(&format!("jump {join}")),
            "the cold block rejoins the not-taken arm at {join}:\n{all}"
        );
    }

    /// At `RaiseExit::Folded` the cold block goes straight to the fault
    /// epilogue, with no arguments, and never rejoins the hot path (ADR-117).
    ///
    /// The "no arguments" half is the structural precondition the fold rests
    /// on: `Terminator::Fault`'s epilogue reads only the prologue's frame bases
    /// and `ctx`, all defined in the entry block, so nothing has to be carried
    /// across this edge — and in particular not the arithmetic's `dst`, which
    /// on this path was never computed.
    #[test]
    fn a_folded_raise_jumps_to_the_fault_epilogue_with_no_arguments() {
        let mut fault_label = None;
        let (all, entry_text, cold) = raise_ir(|fault| {
            fault_label = Some(format!("{fault}"));
            RaiseExit::Folded(fault)
        });
        let fault = fault_label.expect("the closure ran");
        let cold_text = block_text(&all, cold);
        assert!(
            cold_text.contains(&format!("jump {fault}\n"))
                || cold_text.contains(&format!("jump {fault} ")),
            "the cold block leaves for the epilogue with no block arguments:\n{all}"
        );
        assert!(
            cold_text.contains("call "),
            "and it still calls the raise wrapper on the way:\n{all}"
        );
        assert!(
            entry_text.contains("brif"),
            "the hot path is still one branch:\n{all}"
        );
        assert!(
            !entry_text.contains("call "),
            "and still calls nothing:\n{all}"
        );
    }

    // -----------------------------------------------------------------------
    // ADR-118 part 2: the shapes the three inlined collection reads emit.
    //
    // Arm-B only. Under `adr118-arm-a` every one of these emits the call it
    // asserts is absent, which is the toggle working rather than the toggle
    // broken — `a_scalar_proof_loads_its_descriptor_from_the_context` carries
    // the same note for ADR-116.
    // -----------------------------------------------------------------------

    /// Every block that is not cold, as text, so "the hot path calls nothing"
    /// can be asserted once rather than per block.
    #[cfg(not(feature = "adr118-arm-a"))]
    fn hot_blocks(func: &codegen::ir::Function) -> Vec<(Block, String)> {
        let all = func.display().to_string();
        func.layout
            .blocks()
            .filter(|&b| !func.layout.is_cold(b))
            .map(|b| (b, block_text(&all, b)))
            .collect()
    }

    /// `bs.contains(x)` reaches the wrapper from exactly one block, and that
    /// block is cold.
    ///
    /// This is the package's headline stated as a shape: what the inlining
    /// removes is a `call` from the path a loop takes, and no instruction count
    /// can say that on its own — an inline sequence is *bigger* than the `bl`
    /// it replaces, because the wrapper's body was never in the count.
    #[cfg(not(feature = "adr118-arm-a"))]
    #[test]
    fn a_membership_test_calls_the_wrapper_only_from_its_cold_block() {
        let (func, _entry) = emitted_function(|builder, ctx_val, dst, module| {
            emit_bitset_contains(
                builder,
                ctx_val,
                ctx_val,
                ctx_val,
                dst,
                module,
                &mut HashMap::new(),
            )
        });
        let all = func.display().to_string();
        let cold: Vec<_> = func
            .layout
            .blocks()
            .filter(|&b| func.layout.is_cold(b))
            .collect();
        assert_eq!(cold.len(), 1, "exactly the wrapper block is cold:\n{all}");
        assert!(
            block_text(&all, cold[0]).contains("call "),
            "and it is the one that calls `praxis_bitset_contains`:\n{all}"
        );
        for (block, text) in hot_blocks(&func) {
            assert!(
                !text.contains("call "),
                "no hot block may call: {block} does:\n{all}"
            );
        }
        // Three edges reach the cold block — the two descriptor proofs — plus
        // its own label. A shape with one proof, or with a third bail-out,
        // reads differently here.
        assert!(
            all.matches(&format!("{}", cold[0])).count() >= 3,
            "both proofs share one cold block:\n{all}"
        );
    }

    /// The member's payload is read **only** where its descriptor has been
    /// proved, and the words are read only where the receiver's has.
    ///
    /// Stated as dominance over the emitted CFG rather than as "the load is in
    /// a later block", because the second is a claim about block numbering and
    /// the first is the claim REP-56 is about: an eight-byte read off an object
    /// that is not an `Int` is an out-of-bounds read of a zero-width `Unit`.
    #[cfg(not(feature = "adr118-arm-a"))]
    #[test]
    fn a_membership_test_proves_a_descriptor_before_every_payload_read() {
        let (func, entry) = emitted_function(|builder, ctx_val, dst, module| {
            emit_bitset_contains(
                builder,
                ctx_val,
                ctx_val,
                ctx_val,
                dst,
                module,
                &mut HashMap::new(),
            )
        });
        let all = func.display().to_string();
        let site = praxis_runtime::bitset::INLINE_BITSET_SITE;

        // The block that reads the `Int` payload, found by its displacement as
        // a whole address token (handover 26 §7 trap 3: `"+168"` contains
        // `"+16"`).
        let int_align = inline_scalar_load_of(praxis_mir::ScalarKind::Int)
            .expect("`Int` has an inline payload form")
            .1;
        let payload_disp = format!(
            "+{}",
            praxis_runtime::GcHeader::payload_offset_for(int_align)
        );
        let words_disp = format!("+{}", site.elements_offset());
        let len_disp = format!("+{}", site.len_offset());

        let blocks_reading = |disp: &str| {
            hot_blocks(&func)
                .into_iter()
                .filter(|(_, text)| {
                    text.lines().any(|l| {
                        l.split_whitespace()
                            .any(|t| t.trim_end_matches(',').ends_with(disp))
                    })
                })
                .map(|(b, _)| b)
                .collect::<Vec<_>>()
        };

        // The member's proof is the block the entry branches to; the entry
        // itself holds the receiver's. Every payload read must come after both,
        // and "after" is dominance over the emitted CFG rather than block
        // numbering — the second is a claim about the builder, the first is the
        // claim REP-56 is about.
        let member_proof = func
            .layout
            .blocks()
            .nth(1)
            .expect("the member's proof is the second block emitted");

        // `+16` is *both* the `Int` payload and the words pointer, because
        // `payload_offset_for(8)` is 16 and `BitSetPayload.words` is at the
        // payload's start. That collision is why this sweeps every block that
        // reads at a displacement rather than asserting there is one.
        for disp in [&payload_disp, &words_disp, &len_disp] {
            let reads = blocks_reading(disp);
            assert!(!reads.is_empty(), "something reads at {disp}:\n{all}");
            for block in reads {
                assert_ne!(
                    block, entry,
                    "a read at {disp} in the entry block would be unproved:\n{all}"
                );
                assert_dominates(&func, member_proof, block);
            }
        }
    }

    /// `v.len()` discharges its `Effect::Allocates` obligation by **delegating**
    /// to ADR-113's intern probe, not by reading a `usize` and pretending.
    ///
    /// The evidence is the pacing predicate: both `Heap` words are loaded and
    /// compared on the path that answers inline, so a collection that was due
    /// still reaches `praxis_alloc_int`, which paces exactly as the `int_ref`
    /// inside `praxis_vec_len` would have. Without that, every `v.len()` in a
    /// loop would defer the collector indefinitely — ADR-113 decision 1's
    /// rejected alternative, arrived at from a different direction.
    #[cfg(not(feature = "adr118-arm-a"))]
    #[test]
    fn an_inline_vec_length_paces_before_it_answers_from_the_intern_table() {
        let (func, _entry) = emitted_function(|builder, ctx_val, dst, module| {
            let emitted = emit_inline_collection_read(
                builder,
                ctx_val,
                &[ctx_val],
                RuntimeSymbol::VecLen,
                dst,
                module,
                &mut HashMap::new(),
            )?;
            assert!(emitted, "`praxis_vec_len` has an inline form");
            Ok(())
        });
        let all = func.display().to_string();
        let site = praxis_runtime::small_int::INLINE_INTERN_SITE;

        for (what, disp) in [
            ("bytes_since_collect", site.bytes_since_collect_offset()),
            ("collect_threshold", site.collect_threshold_offset()),
            ("the intern table's base", site.table_offset()),
        ] {
            // Cranelift prints a zero displacement as nothing at all, so a
            // field at offset 0 is unassertable this way. None of these three
            // is at 0 today; the assertion says so rather than passing vacuously
            // if one moves there.
            assert_ne!(disp, 0, "{what} is not at offset zero");
            let token = format!("+{disp}");
            assert!(
                all.lines().any(|l| l
                    .split_whitespace()
                    .any(|t| t.trim_end_matches(',').ends_with(&token))),
                "the fast path loads {what} at {token}:\n{all}"
            );
        }
        assert!(
            all.contains("icmp uge"),
            "…and compares them, which is `Heap::collection_is_due` inline:\n{all}"
        );
        for (block, text) in hot_blocks(&func) {
            assert!(
                !text.contains("call "),
                "no hot block may call: {block} does:\n{all}"
            );
        }
        // Two cold blocks, and the pair is the point: one is the wrapper for a
        // receiver that is not a `Vec`, the other is `praxis_alloc_int` for a
        // length outside the intern table. Folding them into one would mean
        // calling `praxis_vec_len` for an out-of-range length, which is a
        // second read of a payload this path already has in a register.
        let cold: Vec<_> = func
            .layout
            .blocks()
            .filter(|&b| func.layout.is_cold(b))
            .collect();
        assert_eq!(cold.len(), 2, "the two bail-outs are separate:\n{all}");
        for block in cold {
            assert!(
                block_text(&all, block).contains("call "),
                "{block} is cold because it calls:\n{all}"
            );
        }
    }

    /// `v[i]` is one unsigned compare and one load, and the fault path is the
    /// wrapper's — so the `Inst::CheckFault` MIR emits after it reads a flag
    /// the fast arm never writes.
    ///
    /// The single unsigned compare is `praxis_vec_get`'s `idx < 0 || idx as
    /// usize >= len` exactly: a negative index reinterpreted as a `u64` is
    /// above every length, so the sign test *is* the bounds test. ADR-113's
    /// `the_unsigned_range_test_generated_code_emits_answers_index_of` is the
    /// same identity in the same shape.
    #[cfg(not(feature = "adr118-arm-a"))]
    #[test]
    fn an_inline_vec_index_bounds_checks_with_one_unsigned_compare() {
        let (func, _entry) = emitted_function(|builder, ctx_val, dst, module| {
            let emitted = emit_inline_collection_read(
                builder,
                ctx_val,
                &[ctx_val, ctx_val],
                RuntimeSymbol::VecGet,
                dst,
                module,
                &mut HashMap::new(),
            )?;
            assert!(emitted, "`praxis_vec_get` has an inline form");
            Ok(())
        });
        let all = func.display().to_string();
        assert_eq!(
            all.matches("icmp ult").count(),
            1,
            "one unsigned compare covers both the sign and the bound:\n{all}"
        );
        assert!(
            !all.contains("icmp slt") && !all.contains("icmp sgt"),
            "…and no signed compare is emitted beside it:\n{all}"
        );
        for (block, text) in hot_blocks(&func) {
            assert!(
                !text.contains("call "),
                "no hot block may call: {block} does:\n{all}"
            );
        }
        let cold: Vec<_> = func
            .layout
            .blocks()
            .filter(|&b| func.layout.is_cold(b))
            .collect();
        assert_eq!(
            cold.len(),
            1,
            "the two proofs and the bounds test share one bail-out:\n{all}"
        );
        assert!(
            block_text(&all, cold[0]).contains("call "),
            "and it calls `praxis_vec_get`, which is what raises:\n{all}"
        );
    }

    /// `praxis_deque_len` has no inline form, and that is decision 5 rather
    /// than an omission: a `VecDeque` is a ring buffer with a head index, so
    /// element *i* is at `(head + i) % cap` and the storage wraps.
    ///
    /// The refusal is worth a test because the failure it prevents is silent:
    /// an arm added by copying the `VecLen` one would read a `std::VecDeque`'s
    /// second word as a length.
    #[test]
    fn a_deque_read_has_no_inline_form_and_falls_through_to_the_wrapper() {
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
        let dst = builder.declare_var(GC);

        for sym in [
            RuntimeSymbol::DequeLen,
            RuntimeSymbol::DequeGet,
            RuntimeSymbol::GridGet,
        ] {
            let emitted = emit_inline_collection_read(
                &mut builder,
                ctx_val,
                &[ctx_val, ctx_val],
                sym,
                dst,
                &mut module,
                &mut HashMap::new(),
            )
            .expect("emission");
            assert!(!emitted, "`{sym}` must keep its call");
        }
    }

    // -----------------------------------------------------------------------
    // Which instructions `steps` pairs, which is the whole of ADR-117's
    // applicability test. These need no builder: the answer is a function of
    // two adjacent `Inst`s and nothing else.
    // -----------------------------------------------------------------------

    /// An `Inst::CheckFault` diverting to block 1, the fault block every
    /// `Builder` mints second (`praxis_mir::build`).
    fn check() -> Inst {
        Inst::CheckFault {
            on_fault: BlockId(1),
            debug: praxis_mir::DebugSlots::unannotated(),
        }
    }

    /// An `Inst::IntBinOp` over locals 0..2 at the given overflow discipline.
    fn add(overflow: Overflow) -> Inst {
        Inst::IntBinOp {
            op: IntBinOp::Add,
            dst: LocalId(0),
            lhs: LocalId(1),
            rhs: LocalId(2),
            overflow,
        }
    }

    /// A call to a wrapper that faults — the shape of every *other* observed
    /// fault in the language, and the one that keeps its check.
    fn call() -> Inst {
        Inst::Call {
            dst: LocalId(0),
            callee: CallTarget::Runtime(RuntimeSymbol::ValueCmp),
            args: vec![LocalId(1), LocalId(2)],
            roots: RootSlots::unannotated(),
            debug: praxis_mir::DebugSlots::unannotated(),
        }
    }

    fn fused(insts: &[Inst]) -> Vec<bool> {
        steps(insts)
            .iter()
            .map(|s| matches!(s.kind, StepKind::RaiseIntoFault { .. }))
            .collect()
    }

    /// The pair ADR-117 folds: checked arithmetic and the check ADR-088
    /// requires after it, as one step covering both instructions.
    #[test]
    fn a_checked_int_binop_and_the_check_after_it_are_one_step() {
        let insts = vec![add(Overflow::Checked), check()];
        let steps = steps(&insts);
        assert_eq!(steps.len(), 1, "one step, not two");
        assert_eq!(steps[0].insts.len(), 2, "and it covers both instructions");
        let StepKind::RaiseIntoFault { on_fault, dst, .. } = steps[0].kind else {
            panic!("the pair is fused");
        };
        assert_eq!(on_fault, BlockId(1), "and it carries the check's target");
        assert_eq!(dst, LocalId(0), "and the arithmetic's own destination");
    }

    /// A check after anything else keeps its own step, and so emits the load,
    /// load and branch. That is the *majority* of the corpus — a wrapper that
    /// faults returns normally, and reading the slot is the only way generated
    /// code can learn it happened.
    #[test]
    fn a_check_after_a_faulting_call_is_its_own_step() {
        assert_eq!(fused(&[call(), check()]), vec![false, false]);
    }

    /// `Overflow::Bounded` emits no raise at all, so there is no branch for a
    /// check to fold into — and the verifier rejects a check after one anyway
    /// (`VerifyError::RedundantFaultCheck`). Both readings agree here, and this
    /// pins that they do: a fused bounded site would be a fault target the
    /// lowering silently drops.
    #[test]
    fn a_bounded_int_binop_is_never_fused() {
        assert_eq!(
            fused(&[add(Overflow::Bounded), check()]),
            vec![false, false]
        );
    }

    /// A checked site with no check after it lowers to ADR-102's converging
    /// diamond. The verifier makes this unreachable, and the point is that the
    /// backend does not *depend* on that: an unverified function gets slower
    /// code, not code that runs past an overflow.
    #[test]
    fn a_checked_int_binop_with_no_check_after_it_is_its_own_step() {
        assert_eq!(fused(&[add(Overflow::Checked)]), vec![false]);
        assert_eq!(
            fused(&[add(Overflow::Checked), call()]),
            vec![false, false],
            "and the instruction after it is untouched"
        );
    }

    /// Every instruction of a block belongs to exactly one step, in order.
    /// A grouping that dropped one would delete emitted code silently, and a
    /// grouping that repeated one would emit it twice.
    #[test]
    fn every_instruction_of_a_block_belongs_to_exactly_one_step_in_order() {
        let insts = vec![
            add(Overflow::Bounded),
            add(Overflow::Checked),
            check(),
            call(),
            check(),
            add(Overflow::Checked),
            check(),
        ];
        // By address, not by value: `Inst` has no `PartialEq`, and identity is
        // the stronger claim anyway — these are the block's own instructions
        // regrouped, not copies that happen to match.
        let covered: Vec<*const Inst> = steps(&insts)
            .iter()
            .flat_map(|s| s.insts.iter())
            .map(std::ptr::from_ref)
            .collect();
        let expected: Vec<*const Inst> = insts.iter().map(std::ptr::from_ref).collect();
        assert_eq!(covered, expected, "the steps concatenate back to the block");
        assert_eq!(fused(&insts), vec![false, true, false, false, true]);
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

    // -----------------------------------------------------------------------
    // From Praxis source to a whole lowered function.
    //
    // `emitted_ir` above wraps one emit closure, which is the right shape for
    // "what does `emit_scalar_load` emit" and the wrong one for "does this
    // loop's hot path contain a call" — that is a claim about a whole
    // `lower_function`, and the only way to make it today was to hand-build
    // MIR. These run the real pipeline instead: parse → HIR → MIR →
    // `lower_function`, the same passes `praxis run` runs.
    //
    // What comes back is the `Function` and not its text, because the text is
    // the weaker of the two: `assert_dominates` needs the CFG, and
    // `lowered_function_ir` is one `display()` away.
    // -----------------------------------------------------------------------

    /// The MIR of `src`, annotated and verified.
    fn mir_for(src: &str) -> (Vec<MirFunction>, praxis_types::TypeDb) {
        use praxis_ast::AstNode;

        let map = praxis_source::SourceMap::new();
        let file = map.intern("lower_test.px", src);
        let parsed = praxis_parser::parse(file, src);
        assert!(
            parsed.diagnostics.is_empty(),
            "parse diagnostics: {:?}",
            parsed.diagnostics
        );
        let mut analysis = praxis_hir::analyze_root(file, &parsed.tree);
        assert!(
            analysis.diagnostics.is_empty(),
            "analysis diagnostics: {:?}",
            analysis.diagnostics
        );
        let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).expect("a source file root");
        let module = praxis_hir::lower(file, &root, &mut analysis);
        assert!(
            module.diagnostics.is_empty(),
            "HIR lowering diagnostics: {:?}",
            module.diagnostics
        );
        let module = praxis_hir::mono::monomorphize(module, &analysis.names, &mut analysis.db);
        let mut funcs = praxis_mir::lower_module(&module, &mut analysis.db);
        for f in &mut funcs {
            praxis_mir::annotate(f);
            if let Err(errs) = praxis_mir::verify(f) {
                panic!("{}", praxis_mir::verify::report(&errs));
            }
        }
        (funcs, analysis.db)
    }

    /// Every function of `src`, lowered by the real backend, by MIR name.
    fn lowered_functions(src: &str) -> HashMap<String, codegen::ir::Function> {
        let (funcs, mut db) = mir_for(src);
        let mut module = test_module();
        // Two passes, exactly as `Jit::compile` does it: everything is declared
        // before anything is defined, so a call to a sibling — or to itself —
        // resolves.
        let mut ids = HashMap::new();
        for f in &funcs {
            let id = module
                .declare_function(&f.name, Linkage::Export, &abi_signature(f))
                .unwrap_or_else(|e| panic!("declaring `{}`: {e}", f.name));
            ids.insert(f.name.clone(), id);
        }
        let generation = Generation::new();
        let mut fn_ctx = FunctionBuilderContext::new();
        let mut out = HashMap::new();
        for f in &funcs {
            let mut captured = codegen::ir::Function::new();
            lower_function_capturing(
                &mut module,
                &mut fn_ctx,
                f,
                &ids,
                &mut db,
                &generation,
                Some(&mut captured),
            )
            .unwrap_or_else(|e| panic!("lowering `{}`: {e}", f.name));
            out.insert(f.name.clone(), captured);
        }
        out
    }

    /// One named function of `src`, lowered. Every function in the program is
    /// still lowered — a caller asking about one of them is not a reason to
    /// compile a different program from the one `praxis run` would.
    fn lowered_function_named(src: &str, name: &str) -> codegen::ir::Function {
        let mut all = lowered_functions(src);
        let mut names: Vec<String> = all.keys().cloned().collect();
        names.sort();
        all.remove(name)
            .unwrap_or_else(|| panic!("no function named `{name}`; this program has {names:?}"))
    }

    /// The program's entry point, lowered: `<entry>` when the file has
    /// top-level statements and `main` otherwise, which is the rule both hosts
    /// that execute a module ask (`praxis_hir::entry_point`).
    fn lowered_function(src: &str) -> codegen::ir::Function {
        let mut all = lowered_functions(src);
        let name = praxis_hir::entry_point(|n| all.contains_key(n))
            .expect("the program has neither top-level statements nor a `main`");
        all.remove(name).expect("`entry_point` just found it")
    }

    /// The post-`define_function` Cranelift IR of `src`'s entry point, as text
    /// — the same text `PRAXIS_DUMP_CLIF` writes, without the count headers.
    /// **Optimized**: the tree is at `opt_level = "speed"`, so the egraph mid-end
    /// has run over this. A test asserting an instruction is *absent* from it is
    /// asserting "the lowering did not emit it, or the mid-end folded it", which
    /// is usually the question one wants and is never quite the question one
    /// wrote.
    fn lowered_function_ir(src: &str) -> String {
        lowered_function(src).display().to_string()
    }

    /// Assert that every path from `func`'s entry block to `dominated` passes
    /// through `dominator`.
    ///
    /// This is what a claim like "no store into a claimed block is reachable
    /// unless the pacing branch was taken" actually says, and it is strictly
    /// stronger than ADR-113's "the branch is the entry block's terminator" —
    /// which is a statement about one shape and stops being a proof the moment
    /// there are two guarded sites.
    fn assert_dominates(func: &codegen::ir::Function, dominator: Block, dominated: Block) {
        use cranelift::codegen::dominator_tree::DominatorTree;
        use cranelift::codegen::flowgraph::ControlFlowGraph;

        let cfg = ControlFlowGraph::with_function(func);
        let domtree = DominatorTree::with_function(func, &cfg);
        // Dominance is ill-defined for an unreachable block, and
        // `DominatorTree::dominates` answers `false` there — which would read
        // as "the guard is missing" when it means "the block is dead".
        assert!(
            domtree.is_reachable(dominated),
            "{dominated} is unreachable from the entry block, so no block \
             dominates it and the question is not the one you meant to \
             ask:\n{}",
            func.display()
        );
        assert!(
            domtree.dominates(dominator, dominated, &func.layout),
            "{dominator} does not dominate {dominated}: some path from the \
             entry block reaches {dominated} without passing through \
             {dominator}:\n{}",
            func.display()
        );
    }

    /// Handover 25 §3's loop, which is the program every instruction count in
    /// the plan is quoted against.
    const SAMPLE_LOOP: &str = concat!(
        "var i = 0\n",
        "var acc = 0\n",
        "var limit = 1000\n",
        "while i < limit {\n",
        "  acc = acc + i * 3\n",
        "  i = i + 1\n",
        "}\n",
        "out(acc)\n",
    );

    /// The helper answers a whole function, and it is the entry point's.
    #[test]
    fn a_lowered_loop_is_a_whole_function_with_branches_and_calls() {
        let ir = lowered_function_ir(SAMPLE_LOOP);
        assert!(
            ir.starts_with("function "),
            "this is a function, not one emit site's block:\n{ir}"
        );
        assert!(ir.contains("icmp"), "`i < limit` is a compare:\n{ir}");
        assert!(ir.contains("brif"), "and the loop branches on it:\n{ir}");
        assert!(
            ir.contains("call "),
            "and boxing the results is still an out-of-line call — which is \
             the whole of what handover 26's plan is about:\n{ir}"
        );
        assert!(
            ir.lines()
                .filter(|l| l.trim_start().starts_with("block"))
                .count()
                > 3,
            "a loop is a header, a body and a join at least:\n{ir}"
        );
    }

    /// A caller can name a function other than the entry point, and gets that
    /// one — a program with two functions lowers both.
    #[test]
    fn a_named_function_is_lowered_beside_the_entry_point() {
        let src = concat!(
            "fn triple(n: Int) -> Int {\n  n * 3\n}\n",
            "out(triple(7))\n",
        );
        let all = lowered_functions(src);
        assert!(
            all.contains_key("triple") && all.contains_key("<entry>"),
            "both functions are lowered: {:?}",
            all.keys().collect::<Vec<_>>()
        );
        let triple = lowered_function_named(src, "triple").display().to_string();
        assert!(
            triple.contains("i64") && triple.contains("return"),
            "and `triple` is a function of its own:\n{triple}"
        );
        assert_ne!(
            triple,
            lowered_function_ir(src),
            "which is not the entry point's"
        );
    }

    // -----------------------------------------------------------------------
    // ADR-128 decision 1: the prologue's slot zeroing.
    // -----------------------------------------------------------------------

    /// The whole IR of a function that does nothing but zero `n` slots, plus its
    /// instruction count.
    ///
    /// The base is the context pointer, which is an `i64` like any other and is
    /// never dereferenced — nothing here executes, and the question is the shape
    /// of the emitted code.
    fn zeroing_ir(n: u32) -> (String, usize) {
        let (func, _entry) = emitted_function(|builder, ctx_val, dst, module| {
            let cfg = module.isa().frontend_config();
            emit_zero_slots(builder, ctx_val, n, SHADOW_SLOT_BYTES, cfg);
            builder.def_var(dst, ctx_val);
            Ok(())
        });
        let text = func.display().to_string();
        let count = text
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.is_empty() && !t.starts_with("block") && !t.starts_with("function")
            })
            .count();
        (text, count)
    }

    /// **No prologue makes a call, at any width the caps allow** (ADR-128
    /// decision 1).
    ///
    /// ADR-101 said the common prologue makes no calls; above
    /// `SLOT_ZERO_UNROLL_MAX` it made two, one per slot stack. The statement is
    /// only worth making if it cannot rot, so this asks it at every width class
    /// there is — including one past `MAX_DEBUG_VALUE_SLOTS`, which no function
    /// can reach, so that raising a cap cannot quietly reintroduce the call.
    #[test]
    fn no_width_of_slot_zeroing_emits_a_call() {
        for n in [
            0,
            1,
            4,
            SLOT_ZERO_UNROLL_MAX - 1,
            SLOT_ZERO_UNROLL_MAX,
            SLOT_ZERO_UNROLL_MAX + 1,
            1024,
            MAX_DEBUG_VALUE_SLOTS as u32,
            MAX_DEBUG_VALUE_SLOTS as u32 + 1,
        ] {
            let (ir, _) = zeroing_ir(n);
            assert!(
                !ir.contains("call"),
                "zeroing {n} slots must not call anything — not `memset`, not a \
                 libcall stub, nothing:\n{ir}"
            );
        }
    }

    /// Below the ceiling the run is exactly one store per slot and no branch.
    ///
    /// This is the case that must not change: for a four-slot frame the four
    /// stores are already optimal, and replacing them with a loop — counter
    /// setup, a branch per iteration — would regress nearly every function in
    /// the language.
    #[test]
    fn a_narrow_claim_is_one_store_per_slot_and_no_branch() {
        for n in [1u32, 4, 33, SLOT_ZERO_UNROLL_MAX] {
            let (ir, _) = zeroing_ir(n);
            let stores = ir.lines().filter(|l| l.contains("store")).count();
            assert_eq!(
                stores, n as usize,
                "{n} slots is {n} stores, one per slot:\n{ir}"
            );
            assert!(
                !ir.contains("brif") && !ir.contains("jump"),
                "and a straight run has nothing to branch on:\n{ir}"
            );
        }
    }

    /// Above the ceiling the code is a loop, so its *size* stops growing with
    /// the width — which is the whole reason there is a ceiling rather than
    /// "always unroll".
    #[test]
    fn a_wide_claim_is_a_loop_whose_code_does_not_grow_with_it() {
        let (small, small_count) = zeroing_ir(SLOT_ZERO_UNROLL_MAX + 1);
        let (large, large_count) = zeroing_ir(MAX_DEBUG_VALUE_SLOTS as u32);
        assert!(
            small.contains("brif") && large.contains("brif"),
            "both are loops:\n{small}\n{large}"
        );
        assert_eq!(
            small_count,
            large_count,
            "and a loop's code is the same size whether it runs {} times or {}: \
             the width is an immediate, not a length:\n{small}\n{large}",
            SLOT_ZERO_UNROLL_MAX + 1,
            MAX_DEBUG_VALUE_SLOTS
        );
        // And it is genuinely small — the point of not unrolling 4096 slots.
        assert!(
            large_count < 16,
            "a zeroing loop is a handful of instructions, not {large_count}:\n{large}"
        );
        let stores = large.lines().filter(|l| l.contains("store")).count();
        assert_eq!(stores, 1, "one store, executed n times:\n{large}");
    }

    /// The ceiling is the last unrolled width and the first looped one is one
    /// past it — asserted rather than assumed, because an off-by-one here is a
    /// silent kilobyte of prologue (or a needless branch in every small frame).
    #[test]
    fn the_unroll_ceiling_is_where_the_loop_begins() {
        let (at, _) = zeroing_ir(SLOT_ZERO_UNROLL_MAX);
        let (past, _) = zeroing_ir(SLOT_ZERO_UNROLL_MAX + 1);
        assert!(!at.contains("brif"), "at the ceiling, still a run:\n{at}");
        assert!(past.contains("brif"), "one past it, a loop:\n{past}");
    }

    /// A zero-slot claim emits nothing at all. `is_prime`'s sibling closures in
    /// `pipeline` have no `Gc` locals whatever, and a frame of no slots has no
    /// slot to zero.
    #[test]
    fn a_zero_slot_claim_zeroes_nothing() {
        let (ir, _) = zeroing_ir(0);
        assert!(
            !ir.contains("store"),
            "no slots claimed is no slots written:\n{ir}"
        );
    }

    // -----------------------------------------------------------------------
    // ADR-128 decision 2: the root-slot colouring.
    // -----------------------------------------------------------------------

    /// A program exercising the shapes the colouring has to get right: a hot
    /// leaf function whose locals are never co-live, a recursive one whose are,
    /// a loop that redefines a root every iteration, and a closure with no `Gc`
    /// locals at all.
    const COLOURING_SAMPLE: &str = concat!(
        "fn is_prime(n: Int) -> Bool {\n",
        "  if n < 2 { return false }\n",
        "  if n % 2 == 0 { return n == 2 }\n",
        "  var d = 3\n",
        "  while d * d <= n {\n",
        "    if n % d == 0 { return false }\n",
        "    d = d + 2\n",
        "  }\n",
        "  true\n",
        "}\n",
        "fn build(depth: Int) -> Int {\n",
        "  if depth == 0 { return 0 }\n",
        "  var left = Vec()\n",
        "  left.push(depth)\n",
        "  var right = Vec()\n",
        "  right.push(build(depth - 1))\n",
        "  left.len() + right.len() + right[0]\n",
        "}\n",
        "var acc = 0\n",
        "for i in 0..10 {\n",
        "  var row = Vec()\n",
        "  row.push(i)\n",
        "  if is_prime(i) { acc = acc + 1 }\n",
        "  acc = acc + row.len() + build(2)\n",
        "}\n",
        "out(acc)\n",
    );

    /// **The invariant the whole of decision 2 rests on**: no two locals live at
    /// one safepoint ever share a shadow slot.
    ///
    /// Its violation is not a crash and not a wrong number — it is a root the
    /// collector never sees, swept while reachable, surfacing later as a
    /// use-after-free somewhere unrelated. `RootSlotMap::color` builds the
    /// assignment so that it cannot happen and `debug_assert_disjoint` re-checks
    /// it on every real compilation; this asks the same question of a real
    /// program's MIR, out of a test that fails rather than a `debug_assert` that
    /// only fires in a debug build someone happened to run.
    #[test]
    fn two_locals_live_at_one_safepoint_never_share_a_slot() {
        let (funcs, _db) = mir_for(COLOURING_SAMPLE);
        let mut safepoints = 0usize;
        let mut co_live = 0usize;
        for f in &funcs {
            let map = RootSlotMap::color(f);
            for inst in f.blocks.iter().flat_map(|b| &b.insts) {
                let Some(roots) = praxis_mir::roots_of(inst) else {
                    continue;
                };
                safepoints += 1;
                let slots: Vec<u32> = roots
                    .live()
                    .iter()
                    .filter_map(|&l| map.get(l))
                    .collect::<Vec<_>>();
                let mut sorted = slots.clone();
                sorted.sort_unstable();
                sorted.dedup();
                assert_eq!(
                    sorted.len(),
                    slots.len(),
                    "`{}` gives two co-live roots one slot at {inst:?}: {slots:?}",
                    f.name
                );
                // And every live `Gc` root got one — the other way to lose a
                // root, and the one `spill_roots`' `debug_assert` guards.
                for &local in roots.live() {
                    let is_gc = f.locals[local.0 as usize].kind == LocalKind::Gc;
                    assert_eq!(
                        is_gc,
                        map.get(local).is_some(),
                        "`{}`: local {local:?} is {}a `Gc` root but {}colored",
                        f.name,
                        if is_gc { "" } else { "not " },
                        if map.get(local).is_some() { "" } else { "not " },
                    );
                }
                if slots.len() > 1 {
                    co_live += 1;
                }
            }
        }
        assert!(
            safepoints > 20,
            "this sample must actually have safepoints to check: {safepoints}"
        );
        assert!(
            co_live > 0,
            "and some of them must root more than one thing at once, or the \
             disjointness above is vacuous"
        );
    }

    /// Only `Gc` locals are coloured. A `Scalar` local carries a payload, not a
    /// pointer; giving it a slot would store a raw integer into the region the
    /// collector dereferences.
    ///
    /// The verifier rejects a `Scalar` in a root set (`VerifyError::RootIsNotGc`)
    /// — this says the backend does not depend on that having run.
    #[test]
    fn only_gc_locals_are_colored() {
        let (funcs, _db) = mir_for(COLOURING_SAMPLE);
        for f in &funcs {
            let map = RootSlotMap::color(f);
            for local in &f.locals {
                if map.get(local.id).is_some() {
                    assert_eq!(
                        local.kind,
                        LocalKind::Gc,
                        "`{}` colored {:?}, which is a {:?} local",
                        f.name,
                        local.id,
                        local.kind
                    );
                }
            }
        }
    }

    /// Same MIR in, same slots out. `PRAXIS_DUMP_CLIF` output and the snapshot
    /// suites both read emitted code, and a colouring that depended on hash
    /// seeding would make two runs of one compiler disagree.
    #[test]
    fn the_colouring_is_deterministic() {
        let (funcs, _db) = mir_for(COLOURING_SAMPLE);
        for f in &funcs {
            let first = RootSlotMap::color(f);
            for _ in 0..8 {
                let again = RootSlotMap::color(f);
                assert_eq!(again.width(), first.width(), "`{}` width", f.name);
                for local in &f.locals {
                    assert_eq!(
                        again.get(local.id),
                        first.get(local.id),
                        "`{}` local {:?}",
                        f.name,
                        local.id
                    );
                }
            }
        }
    }

    /// The width is the largest co-live root set, not the count of `Gc` locals
    /// — which is the whole point, and is what makes `is_prime`'s prologue one
    /// store instead of thirty-three.
    #[test]
    fn a_frames_width_is_its_largest_co_live_root_set() {
        let (funcs, _db) = mir_for(COLOURING_SAMPLE);
        let is_prime = funcs
            .iter()
            .find(|f| f.name == "is_prime")
            .expect("the sample defines `is_prime`");
        let gc_locals = is_prime
            .locals
            .iter()
            .filter(|l| l.kind == LocalKind::Gc)
            .count();
        let width = RootSlotMap::color(is_prime).width();
        let largest = is_prime
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(praxis_mir::roots_of)
            .map(|r| r.live().len())
            .max()
            .unwrap_or(0);
        assert_eq!(
            width as usize, largest,
            "greedy colouring reaches the largest co-live set exactly on this \
             shape — if it ever stops doing so, the colourer is the thing that \
             changed, not the program"
        );
        assert!(
            (width as usize) < gc_locals,
            "and that is strictly narrower than one slot per `Gc` local \
             ({width} of {gc_locals}) — the entire content of decision 2"
        );
    }

    /// How many `Inst::CheckFault`s `func` still emits.
    ///
    /// One load of `ctx.pending_fault` each, and nothing else in the lowering
    /// reads that field — `PENDING_FAULT_OFFSET` has exactly one non-test user,
    /// the `Inst::CheckFault` arm. The context value is read out of the entry
    /// block's parameters rather than spelled `v0`, so the count survives a
    /// prologue that grows a value in front of it.
    ///
    /// **The operand is matched as a whole token.** `"v0+8".contains("v0+8")`
    /// is also true of `v0+80` — the shadow-stack header's fifth slot — and a
    /// substring match here would report the frame's stores as fault checks.
    /// This is the trap handover 26 §7 records W6 walking into from the other
    /// direction, in the same file.
    fn emitted_fault_checks(func: &codegen::ir::Function) -> usize {
        let entry = func
            .layout
            .entry_block()
            .expect("a lowered function has one");
        let ctx = func.dfg.block_params(entry)[0];
        let operand = format!("{ctx}+{PENDING_FAULT_OFFSET}");
        func.display()
            .to_string()
            .lines()
            .filter(|l| {
                l.contains("load.i64")
                    && l.split_whitespace()
                        .any(|t| t.trim_end_matches(',') == operand)
            })
            .count()
    }

    /// Every fault check in handover 25 §3's loop is folded into the raise that
    /// is the only thing that could have set the flag (ADR-117).
    ///
    /// **This is the package's headline as a test.** MIR emits three
    /// `Inst::CheckFault`s per iteration — the census below is the count, not an
    /// estimate — and the lowered function reads `ctx.pending_fault` zero times.
    #[test]
    fn every_fault_check_in_the_sample_loop_is_folded_into_its_raise() {
        use praxis_mir::test_support::{lower_src_to_mir, Census, InstKind};

        let lowered = lower_src_to_mir(SAMPLE_LOOP);
        let entry = lowered.entry();
        let loop_body = lowered.innermost_loop_over(entry, "acc + i * 3");
        let per_iteration = Census::of_blocks(entry, loop_body.blocks.iter().copied());
        assert_eq!(
            per_iteration.count(InstKind::CheckFault),
            3,
            "handover 25 §3 counts three fallible operations per iteration, and \
             the census agrees: {per_iteration:?}"
        );
        assert_eq!(
            emitted_fault_checks(&lowered_function(SAMPLE_LOOP)),
            0,
            "and the backend emits none of them"
        );
    }

    /// **Five runtime type proofs per iteration of that loop — W6's
    /// denominator, and it has moved twice.** Every `Inst::ExtractScalar` is one
    /// `emit_scalar_load`, which is one descriptor proof (ADR-102), so this
    /// census is the site count.
    ///
    /// Handover 25 §3 said seven, by hand. This test said **nine** when W6
    /// wrote it, and nine was right: eight `Int` reloads and the condition's
    /// `Bool` are what `build.rs` emits. Then ADR-120's block-local forwarding
    /// landed in the same wave and deleted four of the nine — three interior
    /// nodes of the two expression trees, and the whole
    /// `Materialize{Bool}`/`ExtractScalar{Bool}` round trip of the `while`
    /// condition.
    ///
    /// **W6 is worth ten machine instructions per iteration here, not
    /// eighteen**, and the amendment at the end of ADR-116 is where that is
    /// restated with both arms re-measured on this tree. Neither number was
    /// wrong when it was written; they are two trees. `mir_shape.rs`'s
    /// `the_sample_loop_proves_a_scalars_descriptor_five_times_per_iteration`
    /// carries the same count from outside this crate, with the table of all
    /// three answers.
    #[test]
    fn the_sample_loop_proves_five_descriptors_per_iteration_where_nine_were_written() {
        use praxis_mir::test_support::{lower_src_to_mir, Census, InstKind};

        let lowered = lower_src_to_mir(SAMPLE_LOOP);
        let entry = lowered.entry();
        let loop_body = lowered.innermost_loop_over(entry, "acc + i * 3");
        let per_iteration = Census::of_blocks(entry, loop_body.blocks.iter().copied());
        let proofs = per_iteration.count(InstKind::ExtractScalar(praxis_mir::ScalarKind::Int))
            + per_iteration.count(InstKind::ExtractScalar(praxis_mir::ScalarKind::Bool));
        assert_eq!(
            proofs, 5,
            "five `Int` reads survive the forwarding and the `Bool` does not: \
             {per_iteration:?}"
        );
    }

    /// A check that follows a *call* is not foldable and is still emitted: the
    /// wrapper sets `pending_fault` and returns, so reading the slot is the only
    /// way generated code learns it happened.
    ///
    /// The control for the test above. Without it, "zero fault checks" would
    /// also pass if the lowering had stopped emitting them altogether.
    #[test]
    fn a_check_after_a_faulting_wrapper_is_still_a_load_and_a_branch() {
        let src = "var v = [3, 1, 2]\nout(v[0] < v[1])\n";
        assert!(
            emitted_fault_checks(&lowered_function(src)) > 0,
            "`praxis_value_cmp` faults, and nothing but the slot says so"
        );
    }

    /// The three raise cold blocks of the sample loop all leave for one block,
    /// and that block returns rather than rejoining the hot path (ADR-117).
    ///
    /// **The raise blocks are the argument-free ones**, and that is a fact
    /// about them rather than a trick: `praxis_raise_*_if` returns `Void`, so
    /// nothing crosses the edge. The other cold blocks in this function —
    /// ADR-102's descriptor bail-out and ADR-113's out-of-range box — hand a
    /// `GcRef` back to their join and are printed `jump blockN(vM)`.
    ///
    /// Asserting "they agree, and it returns" rather than naming the epilogue
    /// is what makes this survive the epilogue growing or shrinking. It is also
    /// what discriminates the two exits: at `RaiseExit::Observed` these blocks
    /// jump argument-free too — to a join that carries on with the loop.
    #[test]
    fn the_sample_loops_three_raises_leave_for_one_returning_block() {
        let func = lowered_function(SAMPLE_LOOP);
        let ir = func.display().to_string();
        let targets: Vec<String> = func
            .layout
            .blocks()
            .filter(|&b| func.layout.is_cold(b))
            .filter_map(|b| {
                let text = block_text(&ir, b);
                let last = text.trim_end().lines().last().unwrap_or("").trim();
                last.strip_prefix("jump block")
                    .filter(|rest| rest.chars().all(|c| c.is_ascii_digit()))
                    .map(|rest| format!("block{rest}"))
            })
            .collect();
        assert_eq!(
            targets.len(),
            3,
            "one raise per checked operation in the loop; found {targets:?}:\n{ir}"
        );
        let first = &targets[0];
        assert!(
            targets.iter().all(|t| t == first),
            "every raise leaves for the same block; found {targets:?}:\n{ir}"
        );
        let epilogue = func
            .layout
            .blocks()
            .find(|b| format!("{b}") == *first)
            .unwrap_or_else(|| panic!("`{first}` is a block of this function:\n{ir}"));
        assert!(
            block_text(&ir, epilogue).contains("return"),
            "and it is an epilogue: it returns rather than rejoining:\n{ir}"
        );
    }

    /// The entry block dominates every block of a real lowered function, which
    /// is the trivially true case the helper must not report as a failure.
    #[test]
    fn the_entry_block_dominates_every_block_of_a_lowered_loop() {
        let func = lowered_function(SAMPLE_LOOP);
        let entry = func
            .layout
            .entry_block()
            .expect("a lowered function has one");
        for block in func.layout.blocks() {
            assert_dominates(&func, entry, block);
        }
    }

    /// A diamond: `entry` dominates the join, and neither arm does.
    fn diamond() -> (codegen::ir::Function, Block, Block, Block) {
        let module = test_module();
        let mut sig = Signature::new(module.isa().default_call_conv());
        sig.params.push(AbiParam::new(GC));
        let mut func = codegen::ir::Function::with_name_signature(Default::default(), sig);
        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut fn_ctx);

        let entry = builder.create_block();
        let taken = builder.create_block();
        let skipped = builder.create_block();
        let join = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let cond = builder.block_params(entry)[0];
        builder.ins().brif(cond, taken, &[], skipped, &[]);
        builder.switch_to_block(taken);
        builder.ins().jump(join, &[]);
        builder.switch_to_block(skipped);
        builder.ins().jump(join, &[]);
        builder.switch_to_block(join);
        builder.ins().return_(&[]);
        builder.seal_all_blocks();
        builder.finalize(module.isa().frontend_config());
        (func, entry, taken, join)
    }

    /// The branch dominates the join it created.
    #[test]
    fn a_block_every_path_goes_through_dominates_the_join() {
        let (func, entry, _taken, join) = diamond();
        assert_dominates(&func, entry, join);
    }

    /// One arm of a diamond does not, and the message names both blocks — the
    /// failure this exists to catch is a store the guard was supposed to cover
    /// and does not, so "which two blocks" is the whole of the report.
    #[test]
    #[should_panic(expected = "does not dominate")]
    fn one_arm_of_a_diamond_does_not_dominate_the_join() {
        let (func, _entry, taken, join) = diamond();
        assert_dominates(&func, taken, join);
    }

    // -----------------------------------------------------------------------
    // The dump hook, end to end.
    //
    // It cannot be checked in-process: `dump::hooks` reads the environment once
    // per process on purpose, and a test binary runs its tests as threads of
    // one process, so a `set_var` here would race every other test's first
    // compilation. So the parent re-executes the test binary with the variables
    // set and reads the child's streams — which is also the only way to check
    // the half that matters most, that the dump is on **stderr**. The A/B
    // protocol voids a measurement whose stdout differs between arms (handover
    // 26 §6), so a dump on stdout would invalidate exactly the runs it exists
    // to explain.
    // -----------------------------------------------------------------------

    /// The one function the dump child asks for by name.
    const DUMP_CHILD_FN: &str = "dumped_by_the_child";

    /// A two-function program, so "by name" has something to exclude.
    const DUMP_CHILD_SRC: &str = concat!(
        "fn dumped_by_the_child(n: Int) -> Int {\n  n + 1\n}\n",
        "out(dumped_by_the_child(1))\n",
    );

    /// The child half of the two tests below: lower a program, print nothing of
    /// its own. Run ordinarily — with neither variable set — it is the arm that
    /// says an unset hook writes nothing at all.
    #[test]
    fn dump_hook_child_lowers_two_functions() {
        lowered_function_named(DUMP_CHILD_SRC, DUMP_CHILD_FN);
    }

    /// Run the child with `env` applied, and answer its (stdout, stderr).
    fn run_dump_child(env: &[(&str, Option<&str>)]) -> (String, String) {
        let exe = std::env::current_exe().expect("this test binary's own path");
        let mut cmd = std::process::Command::new(exe);
        cmd.args([
            "lower::tests::dump_hook_child_lowers_two_functions",
            "--exact",
            "--nocapture",
        ]);
        for (name, value) in env {
            match value {
                Some(v) => cmd.env(name, v),
                // Removed rather than skipped: the child inherits this
                // process's environment, and a developer who exported the
                // variable in their shell would otherwise flip the answer.
                None => cmd.env_remove(name),
            };
        }
        let out = cmd.output().expect("re-running this test binary");
        assert!(
            out.status.success(),
            "the child failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Both hooks fire through the real compile path, on stderr, for the named
    /// function and no other.
    #[test]
    fn the_dump_hooks_write_the_named_functions_ir_to_stderr() {
        let (stdout, stderr) = run_dump_child(&[
            ("PRAXIS_DUMP_CLIF", Some(DUMP_CHILD_FN)),
            ("PRAXIS_DUMP_VCODE", Some(DUMP_CHILD_FN)),
        ]);
        assert!(
            stderr.contains(&format!(";; praxis-dump clif `{DUMP_CHILD_FN}`:")),
            "the CLIF dump is on stderr:\n{stderr}"
        );
        assert!(
            stderr.contains(&format!(";; praxis-dump vcode `{DUMP_CHILD_FN}`:")),
            "and so is the machine-level one:\n{stderr}"
        );
        assert!(
            !stderr.contains("no disassembly was requested"),
            "which means `set_disasm` was called before `define_function` and \
             not after it:\n{stderr}"
        );
        assert!(
            !stderr.contains("praxis-dump clif `<entry>`"),
            "and naming one function dumps one function:\n{stderr}"
        );
        assert!(
            !stdout.contains("praxis-dump"),
            "nothing reaches stdout, or every A/B run with the hook on is \
             void:\n{stdout}"
        );
    }

    /// And with the variables unset, nothing is written at all.
    #[test]
    fn an_unset_dump_hook_writes_nothing() {
        let (stdout, stderr) =
            run_dump_child(&[("PRAXIS_DUMP_CLIF", None), ("PRAXIS_DUMP_VCODE", None)]);
        assert!(!stderr.contains("praxis-dump"), "{stderr}");
        assert!(!stdout.contains("praxis-dump"), "{stdout}");
    }
}
