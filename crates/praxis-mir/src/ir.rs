//! The mid-level IR data structures (§13.5, ADR-015).
//!
//! MIR is deliberately **not SSA**: it is a sea of basic blocks operating over
//! named [`Local`] slots. Every language value lives in a [`Local`] of kind
//! [`LocalKind::Gc`] (holding a uniform `GcRef`); transient scalar payloads
//! (an `i64` loaded out of an `Int` object for a local computation) live in a
//! [`LocalKind::Scalar`] and **must not** survive a GC safepoint — the lowering
//! materializes a fresh `GcRef` from them before any safepoint, call, store, or
//! return (§10.3). The Cranelift backend turns this slot-based CFG into SSA.
//!
//! The fault protocol (§10.4) is woven in: a [`Inst::CheckFault`] tests the
//! context's `pending_fault` and diverts to a [`Terminator::Fault`] edge when a
//! runtime wrapper reports overflow / division-by-zero.

#![allow(dead_code)] // Some variants/fields are consumed by the Cranelift backend
                     // (praxis-codegen-cranelift) which lands later in M4.

use praxis_stdlib::abi::RuntimeSymbol;
use praxis_types::Type;

use crate::annot::{DebugSlots, RootSlots};

/// A whole lowered function: its locals, basic blocks, and debug metadata.
#[derive(Debug)]
pub struct Function {
    /// A stable, human-readable name (the source `fn` name). Used to derive the
    /// JIT symbol and for diagnostics.
    pub name: String,
    /// The parameter locals, in declaration order. Each is a `Gc` slot.
    pub params: Vec<LocalId>,
    /// The return local (a `Gc` slot holding the returned `GcRef`).
    pub return_local: LocalId,
    /// Every local declared in the function, indexed by `LocalId`.
    pub locals: Vec<Local>,
    /// The basic blocks, indexed by `BlockId`. Block 0 is the entry.
    pub blocks: Vec<Block>,
    /// Source-name metadata per local, for fault snapshots (§19 M4 acceptance:
    /// "named locals are available as `GcRef` values in fault snapshots").
    pub debug_names: Vec<Option<String>>,
    /// The debugger classification per local (user binding vs. compiler temp),
    /// threaded to the backend so the crash debugger can separate the two in
    /// its `locals` display and name temps with their materializing expression.
    /// `Scalar` locals (which the backend never shows) default to `Temp`.
    pub debug_kinds: Vec<LocalDebugKind>,
    /// Per-local source span `[start, end)` (byte offsets) for debugger
    /// provenance — the `@ "expr"` the crash debugger prints for a temp. User
    /// locals carry their binding's span; temps carry the lowered expression's
    /// span. `None` for span-less locals (the return slot, scalar scratch).
    pub debug_spans: Vec<Option<(u32, u32)>>,
    /// Per-`Gc`-local: the `Scalar` local whose word now stands in for the box
    /// [`crate::forward`] deleted (ADR-120 part 2).
    ///
    /// The fourth of the parallel debug tables, and the only one written after
    /// lowering rather than during it. A box the forwarding elides defines
    /// nothing, so the backend's store-at-definition (ADR-104) never writes its
    /// debug slot and the temp renders `<uninit>` where it rendered `= 30`.
    /// This is the link that repairs it: the entry says *this `Gc` local's
    /// debug slot is fed by that `Scalar` local's definition*, and it is
    /// written at the only point in the compiler that still knows the two are
    /// one value.
    ///
    /// Read through [`Function::debug_scalar_source`], never directly, because
    /// the accessor is what makes the pairing's invariant observable: it
    /// answers `None` unless the recorded source really is a `Scalar` local,
    /// so a backend handed one of these cannot be handed a reference kind.
    pub debug_scalar_sources: Vec<Option<LocalId>>,
    /// The function's source span `[start, end)` as byte offsets into the
    /// program source (§9.3, M10-WS1). Threaded AST → HIR → MIR → backend so
    /// the crash debugger's `source` command can render the faulting function.
    /// `(0, 0)` for synthetic functions with no source (closures get the
    /// literal's span; the `__p_expr` debugger function is span-less).
    pub span: (u32, u32),
}

/// How a local appears in the crash debugger (§9.4 `locals`).
///
/// `User` locals are bindings the programmer wrote (`let x`, params, captures);
/// they render as `name: Type = value`. `Temp` locals are compiler-generated
/// intermediates (the hidden slot holding `a+b` in `a+b+c`); they render as
/// `<tmp#N: Type> @ "expr" = value`. This replaces the old `"<tmp>"` string
/// placeholder: the split is now structural, not string-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalDebugKind {
    /// A user-written binding, parameter, or capture: has a source name.
    User,
    /// A compiler-generated temporary: anonymous, named by the debugger with a
    /// per-frame index and the expression it materialized.
    Temp,
}

/// The static language type of a MIR slot — or the explicit statement that
/// lowering does not have one (P0-02).
///
/// `praxis_types::Type` is an index into the [`TypeDb`](praxis_types::TypeDb)
/// arena, so *every* integer is a valid handle: the old `Type(0)` "unknown"
/// sentinel silently denoted whichever type happened to be interned first, and
/// fed that type into descriptor resolution, schema construction and debug
/// metadata. Making the absence its own variant means "no type here" can no
/// longer be mistaken for a type, and a consumer that needs a real one has to
/// say so.
///
/// `Opaque` is not a shortcut — it is the honest answer at the sites where the
/// lowering genuinely has no type (pipeline accumulators, fused-loop items),
/// which stays true until HIR-01 carries inferred per-use types into lowering.
/// The MIR verifier's "no `Opaque` in a descriptor-producing position" rule
/// lands with that work, not here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MirType {
    /// A real, inference-produced type handle.
    Known(Type),
    /// Lowering has no static type for this slot.
    Opaque,
}

impl MirType {
    /// The type handle, or `None` when the slot is opaque. The only way to read
    /// a `Type` out of a `MirType`, so a consumer must handle the absence.
    #[inline]
    #[must_use]
    pub fn known(self) -> Option<Type> {
        match self {
            MirType::Known(t) => Some(t),
            MirType::Opaque => None,
        }
    }

    /// Whether lowering left this slot without a static type.
    #[inline]
    #[must_use]
    pub fn is_opaque(self) -> bool {
        matches!(self, MirType::Opaque)
    }
}

/// A local slot.
#[derive(Debug)]
pub struct Local {
    pub id: LocalId,
    /// What the slot holds. Governs how the backend lays it out and whether the
    /// GC must see it at a safepoint.
    pub kind: LocalKind,
    /// The static language type, when lowering knows one. `Scalar` slots are
    /// always [`MirType::Opaque`]: their [`ScalarKind`] is authoritative and the
    /// backend never shows them to the debugger.
    pub ty: MirType,
}

/// What a [`Local`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalKind {
    /// A uniform `GcRef` to a GC object. These are the only locals the GC roots
    /// at a safepoint (§12.3).
    Gc,
    /// A transient scalar payload extracted from a `GcRef` for a local
    /// computation. Must be rematerialized into a `GcRef` before any safepoint,
    /// call, store, or return (§10.3).
    Scalar(ScalarKind),
}

/// The representation of a transient scalar payload.
///
/// `Ord` and `Hash` are the same courtesy [`RuntimeSymbol`] extends, and mean
/// the same thing: this is a tag, so it can key a map. The order is declaration
/// order and says nothing about the payloads — a `Float` is not "greater than"
/// an `Int`, and how the language orders *values* is ADR-045's `compare`
/// callback, not this.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScalarKind {
    /// `i64` — the payload of an `Int` object.
    Int,
    /// `u8` — the payload of a `Byte` object (reserved; not yet wired).
    Byte,
    /// `u32` — the payload of a `Char` object (M6 wires it end-to-end).
    Char,
    /// `bool` — the payload of a `Bool` object (represented as `i8`/`u8`).
    Bool,
    /// `f64` — the payload of a `Float` object (§4.12). Carried through the
    /// uniform `i64` scalar channel as `f64::to_bits()`; the backend bit-casts
    /// to/from `f64` at each arithmetic/comparison point. Unlike `Int`, Float
    /// arithmetic never faults (IEEE-754 produces inf/nan), so a `FloatBinOp`
    /// is never followed by `CheckFault`.
    Float,
}

impl ScalarKind {
    /// The wrapper an [`Inst::Materialize`] of this payload calls to re-box it.
    ///
    /// **This is the one statement of the mapping** (MIR-10). It used to live
    /// inline in the Cranelift backend's `Materialize` arm and nowhere else,
    /// which meant [`Inst::can_fault`] would have had to restate it — and the
    /// verifier's answer and the call the backend emits would then be two
    /// statements of one fact, drifting the first time someone changed a symbol
    /// in `lower_inst`. The backend reads this function now.
    ///
    /// # Panics
    /// On [`ScalarKind::Byte`], which has no boxing wrapper. See
    /// [`ScalarKind::BYTE_HAS_NO_WRAPPER`] for why refusing is the answer.
    #[inline]
    #[must_use]
    pub const fn alloc_symbol(self) -> RuntimeSymbol {
        match self {
            ScalarKind::Int => RuntimeSymbol::AllocInt,
            ScalarKind::Bool => RuntimeSymbol::AllocBool,
            ScalarKind::Char => RuntimeSymbol::AllocChar,
            // Float's bit pattern is boxed by `praxis_alloc_float`.
            ScalarKind::Float => RuntimeSymbol::AllocFloat,
            ScalarKind::Byte => panic!("{}", ScalarKind::BYTE_HAS_NO_WRAPPER),
        }
    }

    /// The wrapper an [`Inst::ExtractScalar`] of this payload calls to read it.
    /// The sibling of [`ScalarKind::alloc_symbol`], and here for its reason.
    ///
    /// # Panics
    /// On [`ScalarKind::Byte`], for [`ScalarKind::alloc_symbol`]'s reason.
    #[inline]
    #[must_use]
    pub const fn load_symbol(self) -> RuntimeSymbol {
        match self {
            ScalarKind::Int => RuntimeSymbol::IntLoad,
            ScalarKind::Bool => RuntimeSymbol::BoolLoad,
            ScalarKind::Char => RuntimeSymbol::CharLoad,
            // Float's payload is read as its f64 bit pattern (i64 channel).
            ScalarKind::Float => RuntimeSymbol::FloatLoad,
            ScalarKind::Byte => panic!("{}", ScalarKind::BYTE_HAS_NO_WRAPPER),
        }
    }

    /// Why [`alloc_symbol`](ScalarKind::alloc_symbol) and
    /// [`load_symbol`](ScalarKind::load_symbol) refuse [`ScalarKind::Byte`]
    /// instead of answering for it.
    ///
    /// Both arms used to name `Int`'s wrapper "defensively", and neither
    /// defended anything. `load_symbol`'s `IntLoad` is an eight-byte read of a
    /// one-byte `BytePayload`, so it returns the byte plus seven bytes of
    /// whatever the allocator put next to it; `alloc_symbol`'s `AllocInt` mints
    /// an object carrying the `INT` descriptor over a byte's worth of value, so
    /// every later `descriptor()` check — the inline scalar-load guard, `==`,
    /// `Map` keying, `out` — reads it as an `Int`. Neither is a memory-safety
    /// bug today, because nothing constructs a `ScalarKind::Byte`: `build` maps
    /// `ScalarType::Byte` to the descriptor channel in `compare_kind` and never
    /// mentions `ScalarKind::Byte` at all. That is precisely the problem — the
    /// wrong answer is invisible until the day somebody wires `Byte`, and on
    /// that day it is a silently wrong program rather than a failed build.
    ///
    /// A refusal is the smallest thing that cannot go quiet. There is no
    /// correct symbol to point at: the ABI manifest
    /// (`crates/praxis-stdlib/src/abi.rs`) has no `praxis_alloc_byte` and no
    /// `praxis_byte_load` row, and inventing one is a runtime + ABI-version
    /// change, not a MIR one. So the mapping says what is true — a `Byte` has no
    /// wrapper — and the compiler stops at the site rather than emitting a call
    /// that reads the wrong width. Wiring `Byte` means adding those two rows
    /// (`AllocByte` must be `AllocatesAndFaults`: it has to reject a value
    /// outside `0..=255`, exactly as `praxis_alloc_char` rejects a non-scalar
    /// code point) and then replacing these two arms with them.
    pub const BYTE_HAS_NO_WRAPPER: &'static str =
        "ScalarKind::Byte has no boxing wrapper: the ABI manifest has no \
         `praxis_alloc_byte`/`praxis_byte_load` row, and `Int`'s wrappers are \
         the wrong width and the wrong descriptor. Wire the two rows before \
         emitting a `Byte` scalar.";
}

/// A basic block: a straight-line list of instructions and one terminator.
#[derive(Debug)]
pub struct Block {
    pub id: BlockId,
    pub insts: Vec<Inst>,
    pub term: Terminator,
}

/// An instruction. Operands and results are [`Local`]s (slot-based, not SSA).
#[derive(Debug)]
pub enum Inst {
    /// Load an immediate integer constant into a `Scalar(Int)` local.
    ConstInt { dst: LocalId, value: i64 },
    /// Load an immediate `f64` constant into a `Scalar(Float)` local, carried
    /// as its IEEE-754 bit pattern (`f64::to_bits()` as `i64`) so it rides the
    /// uniform scalar channel. The backend bit-casts back to `f64` on use (§4.12).
    ConstFloat { dst: LocalId, bits: i64 },
    /// Put a `GcRef` the runtime has **already minted** into a `Gc` local
    /// ([`GcConst`]).
    ///
    /// The `Gc` counterpart of [`ConstInt`](Self::ConstInt), and it carries no
    /// [`RootSlots`] or [`DebugSlots`] for a reason worth stating: it is not a
    /// GC safepoint. It calls no wrapper, allocates nothing, and cannot fault,
    /// so there is nothing here that could trigger a collection and therefore
    /// nothing the collector must be shown. The backend lowers it to two loads
    /// out of the [`RuntimeContext`](praxis_runtime::RuntimeContext) — no call,
    /// no `catch_unwind`, no pacing check, no shadow-frame spill.
    ///
    /// That is the whole of the win. An `Int` literal in a loop body used to be
    /// `ConstInt` + `Alloc { Int }`, and the `Alloc` was a safepoint: a call to
    /// `praxis_alloc_int` *and* a store of every live root into the shadow frame
    /// before it, on every iteration
    /// (docs/handovers/21-where-the-time-goes.md §3.5). Interning the value in
    /// the runtime (`praxis_runtime::small_int`) removed the allocation; this
    /// removes everything that surrounded it.
    ///
    /// **This is not the shape a fresh allocation may take.** A value the
    /// runtime has not already minted still needs [`Alloc`](Self::Alloc): the
    /// allocation can collect, and a collection that cannot see the frame is
    /// ADR-040's whole subject. [`crate::build`] chooses between the two forms
    /// by asking `small_int::index_of`, the same function the runtime asks.
    ConstGc { dst: LocalId, konst: GcConst },
    /// Allocate a fresh `GcRef` and store it in `dst`. This is a GC safepoint
    /// (allocation may trigger collection). `kind` selects the runtime wrapper.
    Alloc {
        dst: LocalId,
        alloc: AllocKind,
        /// The GC root set at this safepoint — filled by the liveness pass. The
        /// backend spills exactly [`RootSlots::live`] into the shadow stack and
        /// nulls exactly [`RootSlots::dead`]. Separate from the debugger's view
        /// of the same point (MIR-16); see [`crate::annot`].
        roots: RootSlots,
        /// What the crash debugger must see here. Over-approximate on purpose,
        /// and never shrunk by making `roots` exact.
        debug: DebugSlots,
    },
    /// Read a scalar payload out of a `Gc` local into a `Scalar` local.
    ExtractScalar {
        dst: LocalId,
        src: LocalId,
        scalar: ScalarKind,
    },
    /// Write a scalar payload from a `Scalar` local into a `Gc` local's object.
    /// (Re-materializes the object reference first if needed.)
    StoreScalar {
        dst_gc: LocalId,
        src: LocalId,
        scalar: ScalarKind,
    },
    /// Materialize a `GcRef` from a `Scalar` payload (`Scalar` → `Gc`). This is
    /// a safepoint (the allocation may trigger collection).
    Materialize {
        dst: LocalId,
        src: LocalId,
        scalar: ScalarKind,
        roots: RootSlots,
        debug: DebugSlots,
    },
    /// A binary arithmetic op on `Int` scalars. When `overflow` is
    /// [`Overflow::Checked`] the runtime sets `pending_fault` on overflow and
    /// the following [`Inst::CheckFault`] diverts; when it is
    /// [`Overflow::Bounded`] the site cannot overflow and the backend emits
    /// the bare arithmetic.
    IntBinOp {
        op: IntBinOp,
        dst: LocalId,
        lhs: LocalId,
        rhs: LocalId,
        overflow: Overflow,
    },
    /// A comparison yielding a `Bool` scalar (`i8`).
    IntCmp {
        op: CmpOp,
        dst: LocalId,
        lhs: LocalId,
        rhs: LocalId,
    },
    /// An unchecked binary arithmetic op on `Float` scalars (§4.12). Never
    /// faults — IEEE-754 produces inf/NaN for overflow and division by zero —
    /// so unlike [`IntBinOp`](Self::IntBinOp) this is not followed by
    /// [`CheckFault`](Self::CheckFault). Operands/results are bit-pattern `i64`s.
    FloatBinOp {
        op: FloatBinOp,
        dst: LocalId,
        lhs: LocalId,
        rhs: LocalId,
    },
    /// Negate a `Float` scalar: IEEE-754 `negate`, which flips the sign bit and
    /// changes nothing else (§4.12). Never faults, like every other float
    /// operation.
    ///
    /// This exists because a negation is **not** a subtraction from zero, and
    /// spelling it that way is what REP-50 was: `0.0 - x` answers `+0.0` at
    /// `x = +0.0`, so the literal `-0.0` evaluated to `+0.0` and printed
    /// `0.0` — a rendering that does not read back as the value it came from,
    /// which is the one rule ADR-083 states. ADR-045 had already decided the
    /// two zeros are distinct values (a container orders them apart), so
    /// losing the sign is losing a value the language admits.
    /// Operand/result are bit-pattern `i64`s; the backend bit-casts and emits
    /// an `fneg`.
    FloatNeg { dst: LocalId, src: LocalId },
    /// A comparison of two `Float` scalars yielding a `Bool` scalar (§4.12).
    /// Uses IEEE-754 ordering: NaN compares unordered against everything (so
    /// `NaN == NaN` and `NaN < x` are both false). Operands are bit-pattern
    /// `i64`s; the backend bit-casts to `f64` and emits an `fcmp`.
    FloatCmp {
        op: CmpOp,
        dst: LocalId,
        lhs: LocalId,
        rhs: LocalId,
    },
    /// Structural equality of two composite GC values (records/tuples/enums/
    /// collections), yielding a `Bool` scalar (§5.5). Lowers to the
    /// `praxis_struct_eq` runtime call, which dispatches to the descriptor's
    /// `equals` callback and recurses element/field wise. A safepoint + fault
    /// check follow (the call may trigger GC). Both operands are `Gc` locals;
    /// `roots` are the GC locals that must survive the call.
    StructEq {
        dst: LocalId,
        lhs: LocalId,
        rhs: LocalId,
        roots: RootSlots,
        debug: DebugSlots,
    },
    /// Order two GC values through their descriptor's `compare` callback,
    /// yielding `-1`/`0`/`1` in a `Scalar(Int)` local (ADR-045). Lowers to the
    /// `praxis_value_cmp` runtime call; the ordering the source asked for is an
    /// [`IntCmp`](Self::IntCmp) of that result against zero.
    ///
    /// Used where the operand type has no ordering the scalar channel can
    /// express — `Text` today, whose payload is a pointer-and-length structure
    /// that an `i64` load turned into an address comparison (P0-12).
    ///
    /// **Not a GC safepoint**: `praxis_value_cmp` is `Effect::Faults`, so it
    /// allocates nothing and carries no [`RootSlots`]. It *can* fault (a
    /// mismatch between the operands' runtime types), so a
    /// [`CheckFault`](Self::CheckFault) follows.
    ValueCmp {
        dst: LocalId,
        lhs: LocalId,
        rhs: LocalId,
    },
    /// `bs.contains(x)` — membership in a `BitSet`, yielding a `Bool` scalar
    /// (`0`/`1`) in `dst` (ADR-118 decision 6). `set` and `member` are `Gc`
    /// locals; the out-of-line form is `praxis_bitset_contains`, whose manifest
    /// row answers `RawI64` for exactly this reason.
    ///
    /// **Its own instruction rather than an [`Inst::Call`], on
    /// [`StructEq`](Self::StructEq)'s and [`ValueCmp`](Self::ValueCmp)'s
    /// precedent, and it buys two separate things.**
    ///
    /// The first is the box. `praxis_bitset_contains` used to answer a boxed
    /// `Bool`, and every use in the language immediately unboxed it: `if
    /// bs.contains(x)` is a [`ExtractScalar`](Self::ExtractScalar) of the
    /// call's result feeding the block's terminator. A `Scalar(Bool)` dst puts
    /// the value where the consumer wanted it; the builder re-boxes through
    /// [`Materialize`](Self::Materialize) only where a `Gc` is genuinely
    /// wanted, which is what leaves `lower_expr_gc`'s contract unchanged.
    ///
    /// The second is the safepoint, and it is the larger one. **Not a GC
    /// safepoint**: `praxis_bitset_contains` is `Effect::Pure` — it allocates
    /// nothing, faults for nothing, and a `BitSet` query is total — so
    /// [`crate::liveness::is_gc_safepoint`] does not match this variant, and
    /// the shadow-frame spill in front of it is gone. That spill existed only
    /// because `is_gc_safepoint` matches *every* `Inst::Call` regardless of the
    /// symbol's effect, which is a property of the instruction shape and not of
    /// the wrapper. Stating the narrowing here rather than in a backend arm is
    /// what makes it ADR-113's rule satisfied rather than bent: the decision
    /// about which instructions the collector may run at is MIR's, and this is
    /// MIR making it.
    ///
    /// No [`CheckFault`](Self::CheckFault) follows, for the same `Effect::Pure`
    /// reason, and [`Inst::fault_reason`] answers that from the manifest rather
    /// than from this doc.
    BitsetContains {
        dst: LocalId,
        set: LocalId,
        member: LocalId,
    },
    /// Call a function. Arguments and result are `Gc` locals. A safepoint +
    /// fault check follow (calls may allocate and may fault).
    Call {
        dst: LocalId,
        callee: CallTarget,
        args: Vec<LocalId>,
        roots: RootSlots,
        debug: DebugSlots,
    },
    /// Call a closure value indirectly (M7, §4.10). `callee` is the `Gc` local
    /// holding the closure `GcRef`. The codegen reads the closure's `fn_ptr`
    /// via `praxis_closure_fn_ptr`, then emits a Cranelift `call_indirect` with
    /// the signature `fn(ctx, closure, args...) -> GcRef` (Approach B: the
    /// closure is passed as a hidden first arg; the synthetic function loads its
    /// captures at entry). A safepoint + fault check follow.
    CallIndirect {
        dst: LocalId,
        callee: LocalId,
        args: Vec<LocalId>,
        roots: RootSlots,
        debug: DebugSlots,
    },
    /// Test `pending_fault`; if set, jump to `on_fault`. Inserted after any
    /// faultable operation (checked arith, div/rem, calls).
    ///
    /// **Not a GC safepoint** — it allocates nothing, so it carries no
    /// [`RootSlots`] at all. It *is* a debugger safepoint: the backend spills
    /// `debug` into the debug frame before the fault test, so a snapshot taken
    /// on the fault path (e.g. a div-by-zero) sees the operands' current values
    /// rather than stale `<uninit>` slots.
    CheckFault {
        on_fault: BlockId,
        debug: DebugSlots,
    },
    /// Copy one `Gc` local into another (a move; no allocation). **`Gc` → `Gc`
    /// only**: a raw scalar word may not enter a rootable slot this way (P0-03).
    /// [`Materialize`](Self::Materialize) is the one legal `Scalar` → `Gc`
    /// transition.
    MoveGc { dst: LocalId, src: LocalId },
    /// Read capture slot `index` out of a closure's environment into a `Gc`
    /// local (M7, §4.10). Emitted once per capture in a synthetic closure
    /// function's prologue.
    ///
    /// `index` is an **immediate**, following the [`LoadField`](Self::LoadField)
    /// precedent, because it is a raw ABI word and not a value: the previous
    /// lowering boxed it as `ConstInt` + `MoveGc` into a `Gc` local so it could
    /// ride the call's argument list, which put the integer `1` in a slot the
    /// collector may dereference (P0-03). Not a safepoint —
    /// `praxis_closure_capture` is `Effect::Pure`, so no fault check follows.
    LoadCapture {
        dst: LocalId,
        closure: LocalId,
        index: u32,
    },
    /// Read a field out of a record `GcRef` into a `Gc` local (M7, §4.5).
    /// `field_idx` is the field's index in the record's `RecordSchema`. Not a
    /// safepoint (no allocation).
    LoadField {
        dst: LocalId,
        src: LocalId,
        field_idx: u32,
    },
    /// Read an element out of a tuple `GcRef` into a `Gc` local (REP-08, §4.4).
    /// `index` is the element's position. Not a safepoint (no allocation).
    ///
    /// Its own instruction rather than a [`Inst::LoadField`] with a flag: the two
    /// call different runtime symbols (`praxis_tuple_get` and
    /// `praxis_record_field`), and choosing between them from the receiver's type
    /// at codegen time would be a second answer to a question the typed tree has
    /// already given.
    LoadTupleElem {
        dst: LocalId,
        src: LocalId,
        index: u32,
    },
    /// Read the variant tag of an enum `GcRef` into a `Scalar(Int)` local (M7,
    /// §4.6). The codegen reads the tag directly from the payload without
    /// allocating. Not a safepoint.
    EnumTag { dst: LocalId, src: LocalId },
    /// Read payload slot `idx` of an enum `GcRef` into a `Gc` local (M7, §4.6).
    /// Not a safepoint (no allocation).
    EnumPayloadGet {
        dst: LocalId,
        src: LocalId,
        idx: u32,
    },
}

impl Inst {
    /// Whether this instruction can set `pending_fault`, and therefore must be
    /// followed immediately by an [`Inst::CheckFault`] (§10.4, ADR-088).
    ///
    /// [`crate::verify`] enforces both directions of that. This is the *one*
    /// answer both it and the sites in [`crate::build`] read, and it derives
    /// from the ABI manifest — [`RuntimeSymbol::faults`] — through the same
    /// instruction→symbol mapping the Cranelift backend uses to emit the call.
    /// Before it, "can this instruction fault" was ~34 local judgements in the
    /// builder, and about half of them were wrong in one direction or the other
    /// (REP-52 forgot a check the fused `collect` needed; REP-53 emitted one
    /// after every method call, including the `Effect::Allocates` ones).
    #[inline]
    #[must_use]
    pub fn can_fault(&self) -> bool {
        self.fault_reason().is_some()
    }

    /// *Why* this instruction can fault — the wrapper it calls, or the
    /// operation whose overflow the runtime reports — or `None` when it cannot.
    ///
    /// One function rather than a `can_fault` predicate plus a separate
    /// diagnostic string: the verifier names the reason in its error, and a
    /// second answer to "which symbol is this" is exactly the drift this
    /// mapping exists to prevent.
    #[must_use]
    pub fn fault_reason(&self) -> Option<&'static str> {
        let faulting = |sym: RuntimeSymbol| sym.faults().then(|| sym.name());
        match self {
            // An allocation is one constructor call plus, for a composite, one
            // filler call per slot. `praxis_alloc_char` validates its payload
            // (`INVALID_CHAR`) and `praxis_grid_new` its dimensions; the rest
            // only allocate. `praxis_alloc_text` was in the first group until
            // ADR-111 made UTF-8 its caller's precondition, and the code here
            // did not change when it left — the answer comes from the manifest,
            // which is ADR-088 §2's entire point.
            Inst::Alloc { alloc, .. } => alloc.symbols().find_map(faulting),
            // Re-boxing a payload: `Char`'s wrapper validates the Unicode
            // scalar, the others do not.
            Inst::Materialize { scalar, .. } => faulting(scalar.alloc_symbol()),
            // Every `praxis_*_load` is `Effect::Pure`; the arm is here so a
            // future width that validates is not silently unobserved.
            Inst::ExtractScalar { scalar, .. } => faulting(scalar.load_symbol()),
            // Checked arithmetic reports overflow (and, for `Div`/`Rem`, a zero
            // divisor) through `praxis_raise_*_if`. `Overflow::Bounded` is a
            // claim about the *site* (ADR-044 decision 6): the backend emits the
            // bare instruction, so there is nothing to observe and a check after
            // one is rejected as redundant.
            Inst::IntBinOp { overflow, .. } => {
                matches!(overflow, Overflow::Checked).then_some("checked Int arithmetic")
            }
            // `praxis_value_cmp` faults when the operands' runtime types
            // disagree, or the type has no ordering (ADR-045).
            Inst::ValueCmp { .. } => faulting(RuntimeSymbol::ValueCmp),
            // `praxis_struct_eq` dispatches to the descriptor's `equals`
            // callback, which answers a `bool` for every pair: `Effect::Pure`.
            Inst::StructEq { .. } => faulting(RuntimeSymbol::StructEq),
            // `praxis_bitset_contains` is `Effect::Pure`: a `BitSet` query is
            // total, so a member the set cannot hold is absent rather than a
            // fault (RT-07). The arm goes through `faulting` anyway, so a row
            // that ever grows a fault is observed without editing this.
            Inst::BitsetContains { .. } => faulting(RuntimeSymbol::BitsetContains),
            Inst::LoadCapture { .. } => faulting(RuntimeSymbol::ClosureCapture),
            Inst::LoadField { .. } => faulting(RuntimeSymbol::RecordField),
            Inst::LoadTupleElem { .. } => faulting(RuntimeSymbol::TupleGet),
            Inst::EnumPayloadGet { .. } => faulting(RuntimeSymbol::EnumPayload),
            // The backend reads the tag inline out of the payload — no call, so
            // no manifest row applies and nothing can fault.
            Inst::EnumTag { .. } => None,
            Inst::Call {
                callee: CallTarget::Runtime(sym),
                ..
            } => faulting(*sym),
            // A callee's *body* may raise any fault, and there is no manifest
            // row for a Praxis function. The fault reaches this frame as the
            // Unit sentinel in `dst`, and only the check turns that back into a
            // diversion — which is what makes a deep `StackOverflow` observable
            // before the caller feeds the sentinel to an arithmetic wrapper.
            Inst::Call {
                callee: CallTarget::User(_),
                ..
            } => Some("a called function's body may raise any fault"),
            Inst::CallIndirect { .. } => Some("a called closure's body may raise any fault"),
            // The backend loads the reference out of the context — no call, so
            // no manifest row applies and nothing can fault. Same standing as
            // `EnumTag` above, and the exhaustive match is what forces the
            // decision to be made here rather than assumed at the emit site.
            Inst::ConstGc { .. } => None,
            Inst::ConstInt { .. }
            | Inst::ConstFloat { .. }
            | Inst::StoreScalar { .. }
            | Inst::IntCmp { .. }
            | Inst::FloatBinOp { .. }
            | Inst::FloatNeg { .. }
            | Inst::FloatCmp { .. }
            | Inst::MoveGc { .. }
            | Inst::CheckFault { .. } => None,
        }
    }
}

/// A `GcRef` the runtime minted before the program started, for
/// [`Inst::ConstGc`].
///
/// Every variant names an object that already exists in
/// [`Immortals`](praxis_runtime::Immortals) and is reachable from the
/// `RuntimeContext`, so lowering one is a *load*, never a construction. That is
/// the membership rule: a value belongs here only if the runtime can name it
/// without allocating, for the whole life of the run, on the context the code is
/// executed against.
///
/// The rule rules out the obvious extension. A constant cannot be a compile-time
/// *address*, because there is no heap at compile time (the CLI builds the `Jit`
/// before the `Runtime`) and because a `praxis_debugger` session replaces its
/// `Jit` while keeping its `Runtime` — an address baked into code would belong to
/// whichever runtime happened to mint it, and nothing types that relationship.
/// Reading it out of the live context at run time costs one extra load and is
/// correct by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcConst {
    /// An `Int` inside `praxis_runtime::small_int`'s interned range. The backend
    /// reads `ctx.small_ints` and indexes it by
    /// `small_int::index_of(value)` — an offset it computes at compile time,
    /// from the same function the runtime uses, so the compiler's notion of
    /// "in range" cannot drift from the table's actual bounds.
    SmallInt(i64),
    /// The `Unit` singleton (`ctx.unit_ref`).
    ///
    /// `Unit` has been an immortal since M3, so `AllocKind::Unit` never
    /// allocated anything — `praxis_alloc_unit` returns the cached reference and
    /// the manifest declares it `Effect::Pure`. What it *did* cost was an extern
    /// call and, because `liveness::is_gc_safepoint` matches `Inst::Alloc`
    /// unconditionally, a full shadow-frame spill at a point where no collection
    /// can happen. Both are gone here: the two answers to "is this a safepoint"
    /// — the manifest's and the instruction shape's — become one.
    Unit,
    /// One of the two `Bool` singletons (`ctx.true_ref` / `ctx.false_ref`), for
    /// [`Unit`](Self::Unit)'s reason: `praxis_alloc_bool` has answered from the
    /// context since ABI v10 and its row is `Effect::Pure`.
    ///
    /// Only a *literal* takes this form. A computed `Bool` — a comparison's
    /// result, `contains`, `is_empty` — is a `Scalar(Bool)` the backend
    /// re-boxes through [`Inst::Materialize`], which does not know the value.
    Bool(bool),
}

impl GcConst {
    /// [`GcConst::SmallInt`] for a value the runtime's table actually holds,
    /// and `None` for one it does not.
    ///
    /// **This is the only way to obtain a `SmallInt` that is worth writing**,
    /// and it exists because the alternative failure is invisible until it is
    /// catastrophic. The backend's `load_gc_const` computes the element offset
    /// with `small_int::index_of(n).expect(..)`; nothing in [`crate::verify`]
    /// checks the range, so a lowering site that decides for itself that its
    /// value "is obviously small" produces MIR that verifies, passes every test
    /// that runs early in a fresh process, and panics the JIT later.
    ///
    /// The concrete case that motivated this is `run_parser_plan`: plan ids come
    /// from a process-wide arena bounded by `MAX_PLANS = 1 << 20`, not by
    /// `SMALL_INT_MAX = 1024`, so an unconditional `SmallInt(plan_id)` would
    /// have failed on the 1025th plan a long-lived process registered — the LSP
    /// or a big test binary, sporadically, never on `praxis run`. Asking
    /// `index_of` here means the question and the answer are one step: a caller
    /// holding a `GcConst::SmallInt` is holding proof the slot exists.
    #[inline]
    #[must_use]
    pub const fn small_int(value: i64) -> Option<GcConst> {
        // `is_some()` rather than `map`: `Option::map` is not a `const fn`, and
        // this stays `const` so the range question can be asked at compile time.
        if praxis_runtime::small_int::index_of(value).is_some() {
            Some(GcConst::SmallInt(value))
        } else {
            None
        }
    }
}

/// What to allocate, for [`Inst::Alloc`].
#[derive(Debug)]
pub enum AllocKind {
    /// A boxed `Int` initialized from `value` (a `Scalar` local).
    Int { value: LocalId },
    /// A boxed `Bool` initialized from `value` (a `Scalar` local).
    Bool { value: LocalId },
    /// The `Unit` singleton (an immortal; no allocation, but still a safepoint
    /// shape for uniformity).
    Unit,
    /// A boxed `Text` from a literal (the string is embedded in the MIR).
    Text { value: String },
    /// A boxed `Char` initialized from a `u32` Unicode scalar (a `Scalar` local;
    /// M6 wires it for the input parser's `char`/`grid(char)`).
    Char { value: LocalId },
    /// A boxed `Float` initialized from an `f64` bit pattern (a `Scalar(Float)`
    /// local; §4.12). The runtime wrapper `praxis_alloc_float` reassembles the
    /// `f64` from the `i64` bits.
    Float { value: LocalId },
    /// A boxed nominal record (M7, §4.5). `record_def_id` identifies the struct
    /// type (index into `TypeDb::record_defs`); `fields` are the field-value
    /// locals in declaration order. The builder builds a `RecordSchema` from the
    /// def and leaks it to `&'static`.
    Record {
        record_def_id: u32,
        fields: Vec<LocalId>,
    },
    /// A boxed enum value (M7, §4.6). `enum_def_id` identifies the enum,
    /// `variant_idx` is the discriminant, and `args` are the payload values.
    ///
    /// `ty` is the value's static type — `Option[Int]`, not `Option` — because
    /// the backend resolves an `EnumSchema` from it and the def alone cannot
    /// say what `Some`'s payload descriptor is for a *generic* def. `Option` is
    /// the one the language has (F12), and it is exactly the one every
    /// `Map.get`/`Grid.find`/graph-walk result is. [`MirType::Opaque`] means
    /// the lowering had no type; the payload slots then go unknown (null) and
    /// the values' own descriptors answer, as they do for a tuple.
    Enum {
        enum_def_id: u32,
        variant_idx: u32,
        ty: MirType,
        args: Vec<LocalId>,
    },
    /// A boxed tuple (M7, §4.5 structural tuples). `ty` is the tuple's static
    /// type (the codegen resolves it to a `TupleSchema` keyed on the type's
    /// element-type sequence); `elements` are the element-value locals in
    /// positional order. Unlike records, tuples have no def-id — their shape is
    /// the element-type sequence alone, so the schema is keyed by the `Type`.
    ///
    /// [`MirType::Opaque`] means the lowering has no tuple type at all. Every
    /// site that builds a tuple has one now — a fused `enumerate`/`zip` pair
    /// reads it off the call node (MIR-05), which was the last exception — so
    /// the case that remains is a *half* of a known pair whose element type is
    /// still an inference variable. The backend answers either with a schema of
    /// `elements.len()` slots, filling in the descriptors it can resolve and
    /// leaving the rest **null** for the runtime to read off each value's own
    /// header (REP-23, ADR-066 decision 5).
    Tuple { ty: MirType, elements: Vec<LocalId> },
    /// A boxed closure value (M7, §4.10). `fn_name` is the synthetic MIR
    /// function's name (the codegen takes its address via `func_addr`); `captures`
    /// are the captured-value locals in env-slot order. The codegen allocates via
    /// `praxis_alloc_closure(ctx, fn_ptr, n)` then fills each slot with
    /// `praxis_closure_set_capture`.
    Closure {
        fn_name: String,
        captures: Vec<LocalId>,
    },
    /// An empty collection constructed via `Vec[T]()`, `Deque[T]()`, etc. (M8,
    /// §11.1/§11.2). The codegen resolves the element/key descriptor(s) from
    /// `ctor` + `args` (the static type args) via [`descriptor_for_type`] and
    /// calls `praxis_<kind>_new`. Carrying the type args (not a pre-resolved
    /// pointer) mirrors `AllocKind::Tuple { ty, .. }`: the descriptor is resolved
    /// in the backend, where the process-static descriptor consts live.
    /// `args` are the collection's type arguments in order (`Vec`/`Deque`/`Set`/
    /// `Heap`/`Grid` → one element type; `Map` → `[K, V]`; `Counter` → `[K]`;
    /// `BitSet`/`Range` → empty). An [`MirType::Opaque`] argument means the
    /// element type is unknown here (a pipeline's result Vec), and the backend
    /// passes a null descriptor — which is exactly what the wrapper's own
    /// "unknown element" contract expects.
    Collection {
        ctor: praxis_types::CollectionCtor,
        args: Vec<MirType>,
    },
}

impl AllocKind {
    /// The wrapper that creates the object.
    ///
    /// `None` only for a [`AllocKind::Collection`] whose constructor has no
    /// `praxis_*_new` wrapper — `Range` and `Seq`, which the backend refuses
    /// (they are unreachable from source: `collection_from_name` resolves the
    /// *type*, but no construction lowering exists).
    #[inline]
    #[must_use]
    pub const fn constructor(&self) -> Option<RuntimeSymbol> {
        match self {
            AllocKind::Int { .. } => Some(RuntimeSymbol::AllocInt),
            AllocKind::Bool { .. } => Some(RuntimeSymbol::AllocBool),
            AllocKind::Unit => Some(RuntimeSymbol::AllocUnit),
            AllocKind::Text { .. } => Some(RuntimeSymbol::AllocText),
            AllocKind::Char { .. } => Some(RuntimeSymbol::AllocChar),
            AllocKind::Float { .. } => Some(RuntimeSymbol::AllocFloat),
            AllocKind::Record { .. } => Some(RuntimeSymbol::AllocRecord),
            AllocKind::Enum { .. } => Some(RuntimeSymbol::AllocEnum),
            AllocKind::Tuple { .. } => Some(RuntimeSymbol::AllocTuple),
            AllocKind::Closure { .. } => Some(RuntimeSymbol::AllocClosure),
            AllocKind::Collection { ctor, .. } => collection_new_symbol(*ctor),
        }
    }

    /// The wrapper that fills one slot of a composite, for the four allocations
    /// the backend builds in two phases (allocate, then set each slot).
    /// `None` for a scalar box or a collection, which have no slots to fill.
    #[inline]
    #[must_use]
    pub const fn filler(&self) -> Option<RuntimeSymbol> {
        match self {
            AllocKind::Record { .. } => Some(RuntimeSymbol::RecordSetField),
            AllocKind::Enum { .. } => Some(RuntimeSymbol::EnumSetPayload),
            AllocKind::Tuple { .. } => Some(RuntimeSymbol::TupleSet),
            AllocKind::Closure { .. } => Some(RuntimeSymbol::ClosureSetCapture),
            _ => None,
        }
    }

    /// Every runtime wrapper this one [`Inst::Alloc`] calls — the constructor
    /// and, for a composite, the filler it calls once per slot.
    ///
    /// One `Alloc` is more than one call, which is why the fault question is
    /// asked of the whole set rather than of the constructor: an allocation
    /// faults if *any* wrapper it reaches can.
    pub fn symbols(&self) -> impl Iterator<Item = RuntimeSymbol> {
        self.constructor().into_iter().chain(self.filler())
    }
}

/// The `praxis_*_new` wrapper for a collection constructor, or `None` when
/// there is none.
///
/// `Range` and `Seq` have no construction wrapper: a range is built by
/// `praxis_range_new` from its endpoints (an [`Inst::Call`], not an `Alloc`),
/// and `Seq` is the compiler-internal lazy sequence a fused pipeline never
/// materializes. The backend errors on either, and the verifier's fault rule
/// answers "cannot fault" for them, which is consistent: an allocation the
/// backend refuses to lower emits no call at all.
#[inline]
#[must_use]
pub const fn collection_new_symbol(ctor: praxis_types::CollectionCtor) -> Option<RuntimeSymbol> {
    use praxis_types::CollectionCtor as C;
    match ctor {
        C::Vec => Some(RuntimeSymbol::VecNew),
        C::Deque => Some(RuntimeSymbol::DequeNew),
        C::Map => Some(RuntimeSymbol::MapNew),
        C::Set => Some(RuntimeSymbol::SetNew),
        C::Counter => Some(RuntimeSymbol::CounterNew),
        C::MinHeap => Some(RuntimeSymbol::MinHeapNew),
        C::MaxHeap => Some(RuntimeSymbol::MaxHeapNew),
        C::BitSet => Some(RuntimeSymbol::BitsetNew),
        C::Grid => Some(RuntimeSymbol::GridNew),
        C::Range | C::Seq => None,
    }
}

/// A call target. M4 resolves user functions by name; the backend mints a symbol.
#[derive(Clone, Debug)]
pub enum CallTarget {
    /// A user-defined function, by its MIR-local index-resolved name. The
    /// backend resolves this to the JIT'd function pointer.
    User(String),
    /// A built-in runtime wrapper, named by the ABI manifest
    /// (`praxis_stdlib::abi`, M5, §11.1). The backend derives the call
    /// signature from the symbol's row rather than from the argument count.
    /// Method calls (`receiver.push(x)`) lower to this variant.
    Runtime(RuntimeSymbol),
}

/// Whether an [`Inst::IntBinOp`] site can overflow, and therefore whether the
/// backend emits an overflow test at all.
///
/// Source-level arithmetic is always [`Overflow::Checked`]. The distinction
/// exists for the arithmetic the *compiler* writes: a `for` loop's index bump
/// and a `count()` accumulator are bounded above by a collection's length, so
/// reaching `i64::MAX` is not a state the program can be in. Emitting the
/// overflow predicate and the raise call for those was two wasted instructions
/// and a call per iteration — and it left the fault protocol looking violated,
/// since none of them is followed by a [`Inst::CheckFault`] (the verifier's
/// "a faulting instruction is observed" rule, MIR-10).
///
/// [`Overflow::Bounded`] is a claim about the *site*, not about the operator.
/// It is only legal on `Add`/`Sub`/`Mul`: `Div`/`Rem` also trap on a zero
/// divisor, which no bound rules out, and the verifier rejects a `Bounded`
/// division.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overflow {
    /// Report overflow (and division by zero) as a runtime fault.
    Checked,
    /// The operands are bounded such that this site cannot overflow. Emitted
    /// bare.
    Bounded,
}

/// Integer binary operators (§4.12). Under [`Overflow::Checked`] all fault on
/// overflow and `Div`/`Rem` additionally fault on division-by-zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntBinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

/// Comparison operators (yield `Bool`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Neq,
    Lt,
    Gt,
    Le,
    Ge,
}

/// Float binary operators (§4.12). Unlike [`IntBinOp`], these never fault —
/// IEEE-754 division by zero yields ±inf or NaN rather than a runtime fault, so
/// a `FloatBinOp` is never followed by [`Inst::CheckFault`]. There is no `Rem`
/// (the `%` operator is not defined for floats and is a type error in inference).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloatBinOp {
    Add,
    Sub,
    Mul,
    Div,
}

/// A block terminator.
#[derive(Debug)]
pub enum Terminator {
    /// Conditional branch on a `Bool` scalar local.
    Branch {
        cond: LocalId,
        then_block: BlockId,
        else_block: BlockId,
    },
    /// Unconditional jump.
    Jump { target: BlockId },
    /// Return the `GcRef` in `value`.
    Return { value: LocalId },
    /// A fault edge: control reached here because `pending_fault` was set. The
    /// backend unwinds to the host (no Rust panic — §10.4).
    Fault,
}

// ---------------------------------------------------------------------------
// Opaque ids (index-based, `Copy`).
// ---------------------------------------------------------------------------

/// A local-slot id (index into [`Function::locals`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LocalId(pub u32);

/// A basic-block id (index into [`Function::blocks`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct BlockId(pub u32);

impl Function {
    /// Append a new local, returning its id. `debug_name`/`debug_kind`/`debug_span`
    /// are the per-local debugger metadata (name, user-vs-temp classification,
    /// source span); only meaningful for `Gc` locals (the backend skips others).
    pub fn new_local(
        &mut self,
        kind: LocalKind,
        ty: MirType,
        debug_name: Option<String>,
        debug_kind: LocalDebugKind,
        debug_span: Option<(u32, u32)>,
    ) -> LocalId {
        let id = LocalId(self.locals.len() as u32);
        self.locals.push(Local { id, kind, ty });
        self.debug_names.push(debug_name);
        self.debug_kinds.push(debug_kind);
        self.debug_spans.push(debug_span);
        self.debug_scalar_sources.push(None);
        id
    }

    /// Append a new empty block that jumps nowhere yet.
    pub fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(Block {
            id,
            insts: Vec::new(),
            term: Terminator::Jump { target: id }, // placeholder; overwritten.
        });
        id
    }

    /// The debug name for a local, if any.
    pub fn debug_name(&self, local: LocalId) -> Option<&str> {
        self.debug_names
            .get(local.0 as usize)
            .and_then(Option::as_deref)
    }

    /// The debugger classification (user vs. temp) for a local. Defaults to
    /// `Temp` for locals allocated before the kinds table existed (defensive).
    pub fn debug_kind(&self, local: LocalId) -> LocalDebugKind {
        self.debug_kinds
            .get(local.0 as usize)
            .copied()
            .unwrap_or(LocalDebugKind::Temp)
    }

    /// The source span for a local, if threaded. `None` for span-less locals
    /// (scalar scratch, the return slot).
    pub fn debug_span(&self, local: LocalId) -> Option<(u32, u32)> {
        self.debug_spans.get(local.0 as usize).copied().flatten()
    }

    /// The `Scalar` local feeding `local`'s debug slot, and what kind of
    /// payload it holds — `None` unless [`crate::forward`] elided `local`'s box
    /// (ADR-120 part 2).
    ///
    /// The [`ScalarKind`] is read out of [`Function::locals`] rather than
    /// stored beside the id, so the answer cannot disagree with the local it
    /// names. That is also what makes the pair *unrepresentable* where it would
    /// be unsound: the backend turns this kind into the runtime's
    /// `DebugSlotKind`, and there is no way to reach that conversion holding a
    /// source that is not a scalar. A recorded id that somehow named a `Gc`
    /// local reads back as "no scalar source", which is the pre-ADR-120
    /// behaviour — a `<uninit>` temp, not a payload the collector walks.
    #[must_use]
    pub fn debug_scalar_source(&self, local: LocalId) -> Option<(LocalId, ScalarKind)> {
        let src = (*self.debug_scalar_sources.get(local.0 as usize)?)?;
        match self.locals.get(src.0 as usize)?.kind {
            LocalKind::Scalar(kind) => Some((src, kind)),
            LocalKind::Gc => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_local_and_block_increment_ids() {
        let mut f = Function {
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
        let a = f.new_local(
            LocalKind::Gc,
            MirType::Opaque,
            Some("x".into()),
            LocalDebugKind::User,
            Some((1, 5)),
        );
        let b = f.new_local(
            LocalKind::Scalar(ScalarKind::Int),
            MirType::Opaque,
            None,
            LocalDebugKind::Temp,
            None,
        );
        let blk = f.new_block();
        assert_eq!(a, LocalId(0));
        assert_eq!(b, LocalId(1));
        assert_eq!(blk, BlockId(0));
        assert_eq!(f.debug_name(a), Some("x"));
        assert_eq!(f.debug_name(b), None);
        assert_eq!(f.debug_kind(a), LocalDebugKind::User);
        assert_eq!(f.debug_kind(b), LocalDebugKind::Temp);
        assert_eq!(f.debug_span(a), Some((1, 5)));
        assert_eq!(f.debug_span(b), None);
    }

    /// A [`GcConst::SmallInt`] cannot be built for a value the interned table
    /// does not hold, at either end of the range.
    ///
    /// This is what turns the backend's `small_int::index_of(n).expect(..)` in
    /// `load_gc_const` into a documented fact rather than a hope. Nothing in
    /// [`crate::verify`] checks the range, so before the constructor existed the
    /// only thing standing between a lowering site's private opinion about what
    /// counts as "small" and a JIT panic was that no site had one — and
    /// `run_parser_plan`, whose ids come from an arena bounded by `1 << 20`, was
    /// exactly the site about to form one.
    #[test]
    fn a_gc_const_small_int_cannot_name_a_slot_the_table_does_not_have() {
        use praxis_runtime::small_int::{SMALL_INT_MAX, SMALL_INT_MIN};

        assert_eq!(
            GcConst::small_int(SMALL_INT_MIN),
            Some(GcConst::SmallInt(SMALL_INT_MIN)),
            "the floor is in the table"
        );
        assert_eq!(
            GcConst::small_int(SMALL_INT_MAX),
            Some(GcConst::SmallInt(SMALL_INT_MAX)),
            "the ceiling is in the table"
        );
        assert_eq!(GcConst::small_int(SMALL_INT_MIN - 1), None);
        assert_eq!(GcConst::small_int(SMALL_INT_MAX + 1), None);
        // The case the constructor exists for: a plan id past the table's end.
        // `MAX_PLANS` is `1 << 20`, so this is reachable in a long-lived process
        // rather than hypothetical.
        assert_eq!(GcConst::small_int(1 << 20), None);
    }

    /// A `Byte` payload has no boxing wrapper, and asking for one is refused
    /// rather than answered with `Int`'s.
    ///
    /// The arm used to answer `RuntimeSymbol::AllocInt`, which mints an object
    /// carrying the `INT` descriptor over a byte's worth of value. Nothing
    /// constructs a [`ScalarKind::Byte`] today, so the wrong answer was
    /// invisible — which is the whole reason it had to stop being an answer.
    #[test]
    #[should_panic(expected = "ScalarKind::Byte has no boxing wrapper")]
    fn boxing_a_byte_payload_is_refused_because_there_is_no_byte_wrapper() {
        let _ = ScalarKind::Byte.alloc_symbol();
    }

    /// The mirror of
    /// [`boxing_a_byte_payload_is_refused_because_there_is_no_byte_wrapper`],
    /// and the arm handover 23 §4 named: `IntLoad` is an eight-byte read of a
    /// one-byte `BytePayload`, so it answers the byte plus seven bytes of
    /// whatever the allocator happened to put beside it.
    #[test]
    #[should_panic(expected = "ScalarKind::Byte has no boxing wrapper")]
    fn reading_a_byte_payload_is_refused_because_there_is_no_byte_wrapper() {
        let _ = ScalarKind::Byte.load_symbol();
    }

    /// Every *other* `ScalarKind` answers both questions, so the refusal above
    /// is a statement about `Byte` and not a hole in the mapping.
    #[test]
    fn every_wired_scalar_kind_names_both_of_its_wrappers() {
        for sk in [
            ScalarKind::Int,
            ScalarKind::Bool,
            ScalarKind::Char,
            ScalarKind::Float,
        ] {
            // Both are total for these four; the assertion is that neither
            // panics and that they do not collapse onto one symbol.
            assert_ne!(sk.alloc_symbol(), sk.load_symbol());
        }
    }
}
