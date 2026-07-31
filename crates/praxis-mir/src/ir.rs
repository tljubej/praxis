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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
}
