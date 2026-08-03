//! The runtime ABI manifest: one row per `praxis_*` symbol the JIT can call.
//!
//! Everything the compiler needs to know about a runtime wrapper — its exact
//! symbol name, its parameter and return kinds, and whether calling it can
//! allocate or fault — is **one row** in [`runtime_symbols!`] below. Before
//! this manifest that knowledge was spread over five places (a `Symbol` enum,
//! an arity-derived signature synthesizer, a JIT registration list, a
//! name→pointer resolver and a MIR string literal), and they had already
//! drifted: the registration list was missing symbols that a `dlsym` fallback
//! silently found, and the arity-only signature fed an `i64` immediate into a
//! `u32` parameter.
//!
//! A call target is now a [`RuntimeSymbol`], not a string. Adding a wrapper
//! means adding a row here and one arm to `praxis_runtime::abi::address`; both
//! are exhaustive matches, so anything else that must change is a compile
//! error rather than a runtime surprise.
//!
//! This crate is the right home because it is the lowest common dependency of
//! the compiler crates that need the manifest (`praxis-mir`,
//! `praxis-codegen-cranelift`) and of `praxis-runtime`, which supplies the
//! addresses.

/// The kind of one ABI parameter — what a value in that position *is*, which
/// fixes the machine type the caller must pass.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AbiKind {
    /// `*mut RuntimeContext`. Always the first parameter of every wrapper.
    Ctx,
    /// A `GcRef` — a non-null pointer to a `GcHeader`. Pointer-width.
    Gc,
    /// A raw, unboxed `i64`. **Not** a GC reference: never rooted, never traced.
    RawI64,
    /// A raw, unboxed `u32`. Narrower than a machine word, so passing an `i64`
    /// here is exactly the mismatch this manifest exists to prevent.
    RawU32,
    /// A pointer-width raw word that is not a `GcRef`: a `*const u8`, a
    /// descriptor or schema pointer, a frame pointer, or a `usize` length.
    Ptr,
}

/// What a wrapper returns.
///
/// The `Gc`/`GcUnit` split is the one fact `AbiRet` did not carry, and its
/// absence is what let RT-14 and RT-15 exist: `praxis_map_get` and
/// `praxis_grid_find` were declared `-> Gc` and answered the Unit sentinel on a
/// miss, while their catalog rows promised a `V` and an `(Int, Int)`. Nothing in
/// the workspace could relate the two, because "a `GcRef`" said nothing about
/// whether the reference could be Unit.
///
/// **There is deliberately no third arm.** "May be Unit, may be a value" *is*
/// RT-14/RT-15, and its absence from this enum is what makes the defect
/// unrepresentable rather than merely fixed. A wrapper whose answer is
/// sometimes absent says so in its result *type* — `Option[T]` (§4.7) — or it
/// faults.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AbiRet {
    /// A `GcRef` carrying the wrapper's **answer**: a value of the result type
    /// its catalog row declares.
    ///
    /// The Unit sentinel still comes back on a fault return — that is the ABI's
    /// universal "a Praxis function returns a valid `GcRef` even when it
    /// unwinds" — and, in the handful of wrappers the *codegen* calls directly
    /// (`praxis_alloc_enum` with a null schema, `praxis_tuple_get` with an
    /// out-of-range index), on a refusal the compiler was responsible for
    /// having prevented. Neither is "the value is absent", which is the state
    /// this arm rules out.
    Gc,
    /// A `GcRef` that is **always** the Unit sentinel: the wrapper's answer is
    /// "done", not a value. `Vec.push`, `Map.insert`, `out`, `assert`.
    ///
    /// Not `Void`: the call still yields a `GcRef` the caller's uniform value
    /// channel consumes, and codegen treats it exactly as it treats `Gc`.
    GcUnit,
    /// A raw `i64`.
    RawI64,
    /// A pointer-width raw word (a frame pointer, a function pointer).
    Ptr,
    /// Nothing.
    Void,
}

/// The one answer to "does calling this need a root set, or a fault check?"
///
/// `Allocates` means the call **may trigger a collection**, so every live
/// `GcRef` the caller holds must be rooted across it — that is what makes a
/// call site a safepoint. A wrapper that only hands back an immortal singleton
/// (`true`, `false`, `unit`) allocates nothing collectable and is therefore not
/// a safepoint, however "alloc" its name reads.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Effect {
    /// Neither allocates nor faults.
    Pure,
    /// May set a pending fault; cannot allocate.
    Faults,
    /// May allocate (and therefore collect); cannot fault.
    Allocates,
    /// Both.
    AllocatesAndFaults,
}

impl Effect {
    /// Whether a call to this symbol is a safepoint.
    #[inline]
    pub const fn allocates(self) -> bool {
        matches!(self, Effect::Allocates | Effect::AllocatesAndFaults)
    }

    /// Whether a call to this symbol needs a fault check afterwards.
    #[inline]
    pub const fn faults(self) -> bool {
        matches!(self, Effect::Faults | Effect::AllocatesAndFaults)
    }
}

/// One wrapper's full ABI: what it takes, what it gives back, what it may do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AbiSig {
    /// Parameter kinds, including the leading [`AbiKind::Ctx`].
    pub params: &'static [AbiKind],
    /// Return kind.
    pub ret: AbiRet,
    /// Allocation and fault behaviour.
    pub effect: Effect,
}

impl AbiSig {
    /// Parameter count excluding the leading context pointer.
    #[inline]
    pub const fn arity(&self) -> usize {
        self.params.len() - 1
    }
}

/// Declare the manifest. One row per symbol:
/// `Variant = "praxis_name": (ParamKinds…) -> Ret, Effect;`
macro_rules! runtime_symbols {
    ($( $variant:ident = $name:literal : ( $($kind:ident),* ) -> $ret:ident , $effect:ident ; )*) => {
        /// Every `praxis_*` runtime wrapper generated code may call.
        ///
        /// A call target in MIR is one of these, so "the compiler emitted a call
        /// to a symbol that does not exist" is not a representable state.
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
        pub enum RuntimeSymbol {
            $(
                #[doc = concat!("`", $name, "`")]
                $variant,
            )*
        }

        impl RuntimeSymbol {
            /// Every symbol, in declaration order.
            pub const ALL: &'static [RuntimeSymbol] = &[$(RuntimeSymbol::$variant),*];

            /// The exact linker symbol name. This is the only place the string
            /// is written.
            #[inline]
            pub const fn name(self) -> &'static str {
                match self { $(RuntimeSymbol::$variant => $name,)* }
            }

            /// This symbol's parameter kinds, return kind and effect.
            #[inline]
            pub const fn sig(self) -> AbiSig {
                match self {
                    $(RuntimeSymbol::$variant => AbiSig {
                        params: &[$(AbiKind::$kind),*],
                        ret: AbiRet::$ret,
                        effect: Effect::$effect,
                    },)*
                }
            }

            /// Recover a symbol from its linker name. The inverse of
            /// [`RuntimeSymbol::name`]; used where a name crosses a boundary
            /// that is not yet typed.
            pub fn from_name(name: &str) -> Option<RuntimeSymbol> {
                match name {
                    $($name => Some(RuntimeSymbol::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

impl RuntimeSymbol {
    /// Whether calling this symbol may trigger a collection (a safepoint).
    #[inline]
    pub const fn allocates(self) -> bool {
        self.sig().effect.allocates()
    }

    /// Whether calling this symbol may set a pending fault.
    #[inline]
    pub const fn faults(self) -> bool {
        self.sig().effect.faults()
    }

    /// Parameter count excluding the leading context pointer.
    #[inline]
    pub const fn arity(self) -> usize {
        self.sig().arity()
    }
}

impl std::fmt::Display for RuntimeSymbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

runtime_symbols! {
    AllocBool = "praxis_alloc_bool": (Ctx, RawI64) -> Gc, Pure;
    AllocChar = "praxis_alloc_char": (Ctx, RawI64) -> Gc, AllocatesAndFaults;
    AllocClosure = "praxis_alloc_closure": (Ctx, Ptr, RawI64) -> Gc, Allocates;
    AllocEnum = "praxis_alloc_enum": (Ctx, Ptr, RawI64) -> Gc, Allocates;
    AllocFloat = "praxis_alloc_float": (Ctx, RawI64) -> Gc, Allocates;
    AllocInt = "praxis_alloc_int": (Ctx, RawI64) -> Gc, Allocates;
    AllocRecord = "praxis_alloc_record": (Ctx, Ptr) -> Gc, Allocates;
    AllocText = "praxis_alloc_text": (Ctx, Ptr, Ptr) -> Gc, Allocates;
    AllocTuple = "praxis_alloc_tuple": (Ctx, Ptr) -> Gc, Allocates;
    AllocUnit = "praxis_alloc_unit": (Ctx) -> GcUnit, Pure;
    AllocVarCell = "praxis_alloc_var_cell": (Ctx, Gc) -> Gc, Allocates;
    Assert = "praxis_assert": (Ctx, Gc) -> GcUnit, Faults;
    AStar = "praxis_a_star": (Ctx, Gc, Gc, Gc, Gc, Gc) -> Gc, AllocatesAndFaults;
    Bfs = "praxis_bfs": (Ctx, Gc, Gc) -> Gc, AllocatesAndFaults;
    BfsDistance = "praxis_bfs_distance": (Ctx, Gc, Gc, Gc) -> Gc, AllocatesAndFaults;
    // `-> RawI64` and not `-> Gc`, which is what makes `bs.contains(x)` a
    // scalar-producing MIR instruction rather than a call whose answer has to
    // be unboxed again (ADR-118 decision 6). `StructEq` and `ValueCmp` are the
    // two rows this copies, and the shape is the same on all three: a boxed
    // `Bool` the caller immediately unboxes is a box nobody looks at.
    BitsetContains = "praxis_bitset_contains": (Ctx, Gc, Gc) -> RawI64, Pure;
    BitsetInsert = "praxis_bitset_insert": (Ctx, Gc, Gc) -> GcUnit, AllocatesAndFaults;
    BitsetIsEmpty = "praxis_bitset_is_empty": (Ctx, Gc) -> Gc, Pure;
    BitsetItems = "praxis_bitset_items": (Ctx, Gc) -> Gc, Allocates;
    BitsetLen = "praxis_bitset_len": (Ctx, Gc) -> Gc, Allocates;
    BitsetNew = "praxis_bitset_new": (Ctx) -> Gc, Allocates;
    BitsetRemove = "praxis_bitset_remove": (Ctx, Gc, Gc) -> GcUnit, Pure;
    BoolLoad = "praxis_bool_load": (Ctx, Gc) -> RawI64, Pure;
    CharLoad = "praxis_char_load": (Ctx, Gc) -> RawI64, Pure;
    CharToInt = "praxis_char_to_int": (Ctx, Gc) -> Gc, Allocates;
    CheckFault = "praxis_check_fault": (Ctx) -> RawI64, Pure;
    ClosureCapture = "praxis_closure_capture": (Ctx, Gc, RawI64) -> Gc, Pure;
    ClosureFnPtr = "praxis_closure_fn_ptr": (Ctx, Gc) -> Ptr, Pure;
    ClosureSetCapture = "praxis_closure_set_capture": (Ctx, Gc, RawI64, Gc) -> Gc, Pure;
    CounterGet = "praxis_counter_get": (Ctx, Gc, Gc) -> Gc, Allocates;
    CounterInc = "praxis_counter_inc": (Ctx, Gc, Gc) -> GcUnit, AllocatesAndFaults;
    CounterKeys = "praxis_counter_keys": (Ctx, Gc) -> Gc, Allocates;
    CounterSet = "praxis_counter_set": (Ctx, Gc, Gc, Gc) -> GcUnit, Allocates;
    CounterValues = "praxis_counter_values": (Ctx, Gc) -> Gc, Allocates;
    CounterIsEmpty = "praxis_counter_is_empty": (Ctx, Gc) -> Gc, Pure;
    CounterLen = "praxis_counter_len": (Ctx, Gc) -> Gc, Allocates;
    CounterNew = "praxis_counter_new": (Ctx, Ptr) -> Gc, Allocates;
    DequeGet = "praxis_deque_get": (Ctx, Gc, Gc) -> Gc, Faults;
    DequeIsEmpty = "praxis_deque_is_empty": (Ctx, Gc) -> Gc, Pure;
    DequeLen = "praxis_deque_len": (Ctx, Gc) -> Gc, Allocates;
    DequeNew = "praxis_deque_new": (Ctx, Ptr) -> Gc, Allocates;
    DequePopBack = "praxis_deque_pop_back": (Ctx, Gc) -> Gc, Faults;
    DequePopFront = "praxis_deque_pop_front": (Ctx, Gc) -> Gc, Faults;
    DequePushBack = "praxis_deque_push_back": (Ctx, Gc, Gc) -> GcUnit, AllocatesAndFaults;
    DequePushFront = "praxis_deque_push_front": (Ctx, Gc, Gc) -> GcUnit, AllocatesAndFaults;
    Dbg = "praxis_dbg": (Ctx, Gc) -> Gc, Pure;
    Dfs = "praxis_dfs": (Ctx, Gc, Gc) -> Gc, AllocatesAndFaults;
    Dijkstra = "praxis_dijkstra": (Ctx, Gc, Gc, Gc) -> Gc, AllocatesAndFaults;
    EnumPayload = "praxis_enum_payload": (Ctx, Gc, RawI64) -> Gc, Pure;
    EnumSetPayload = "praxis_enum_set_payload": (Ctx, Gc, RawI64, Gc) -> Gc, Pure;
    EnumTag = "praxis_enum_tag": (Ctx, Gc) -> Gc, Allocates;
    FloatAbs = "praxis_float_abs": (Ctx, Gc) -> Gc, Allocates;
    FloatCeil = "praxis_float_ceil": (Ctx, Gc) -> Gc, Allocates;
    FloatE = "praxis_float_e": (Ctx) -> Gc, Allocates;
    FloatFloor = "praxis_float_floor": (Ctx, Gc) -> Gc, Allocates;
    FloatIsInfinite = "praxis_float_is_infinite": (Ctx, Gc) -> Gc, Pure;
    FloatIsNan = "praxis_float_is_nan": (Ctx, Gc) -> Gc, Pure;
    FloatLoad = "praxis_float_load": (Ctx, Gc) -> RawI64, Pure;
    FloatMax = "praxis_float_max": (Ctx, Gc, Gc) -> Gc, Allocates;
    FloatMin = "praxis_float_min": (Ctx, Gc, Gc) -> Gc, Allocates;
    FloatPi = "praxis_float_pi": (Ctx) -> Gc, Allocates;
    FloatRound = "praxis_float_round": (Ctx, Gc) -> Gc, Allocates;
    FloatSign = "praxis_float_sign": (Ctx, Gc) -> Gc, Allocates;
    FloatSqrt = "praxis_float_sqrt": (Ctx, Gc) -> Gc, Allocates;
    FloatToInt = "praxis_float_to_int": (Ctx, Gc) -> Gc, AllocatesAndFaults;
    FloatToText = "praxis_float_to_text": (Ctx, Gc) -> Gc, Allocates;
    FloodFill = "praxis_flood_fill": (Ctx, Gc, Gc) -> Gc, AllocatesAndFaults;
    GetInput = "praxis_get_input": (Ctx) -> Gc, AllocatesAndFaults;
    GridCells = "praxis_grid_cells": (Ctx, Gc) -> Gc, Allocates;
    GridColumn = "praxis_grid_column": (Ctx, Gc, Gc) -> Gc, AllocatesAndFaults;
    GridContains = "praxis_grid_contains": (Ctx, Gc, Gc, Gc) -> Gc, Pure;
    GridFind = "praxis_grid_find": (Ctx, Gc, Gc) -> Gc, Allocates;
    GridFindAll = "praxis_grid_find_all": (Ctx, Gc, Gc) -> Gc, Allocates;
    GridGet = "praxis_grid_get": (Ctx, Gc, Gc, Gc) -> Gc, Faults;
    GridHeight = "praxis_grid_height": (Ctx, Gc) -> Gc, Allocates;
    GridNeighbors4 = "praxis_grid_neighbors4": (Ctx, Gc, Gc) -> Gc, Allocates;
    GridNeighbors8 = "praxis_grid_neighbors8": (Ctx, Gc, Gc) -> Gc, Allocates;
    GridNew = "praxis_grid_new": (Ctx, Ptr, RawI64, RawI64) -> Gc, AllocatesAndFaults;
    GridPositions = "praxis_grid_positions": (Ctx, Gc) -> Gc, Allocates;
    GridRotateLeft = "praxis_grid_rotate_left": (Ctx, Gc) -> Gc, Allocates;
    GridRotateRight = "praxis_grid_rotate_right": (Ctx, Gc) -> Gc, Allocates;
    GridRow = "praxis_grid_row": (Ctx, Gc, Gc) -> Gc, AllocatesAndFaults;
    GridSet = "praxis_grid_set": (Ctx, Gc, Gc, Gc, Gc) -> GcUnit, Faults;
    GridTranspose = "praxis_grid_transpose": (Ctx, Gc) -> Gc, Allocates;
    GridWidth = "praxis_grid_width": (Ctx, Gc) -> Gc, Allocates;
    IntAbs = "praxis_int_abs": (Ctx, Gc) -> Gc, AllocatesAndFaults;
    IntAdd = "praxis_int_add": (Ctx, Gc, Gc) -> Gc, AllocatesAndFaults;
    IntCheckedAdd = "praxis_int_checked_add": (Ctx, Gc, Gc) -> Gc, Allocates;
    IntCheckedMul = "praxis_int_checked_mul": (Ctx, Gc, Gc) -> Gc, Allocates;
    IntCheckedSub = "praxis_int_checked_sub": (Ctx, Gc, Gc) -> Gc, Allocates;
    IntClamp = "praxis_int_clamp": (Ctx, Gc, Gc, Gc) -> Gc, Faults;
    IntDiv = "praxis_int_div": (Ctx, Gc, Gc) -> Gc, AllocatesAndFaults;
    IntEq = "praxis_int_eq": (Ctx, Gc, Gc) -> Gc, Pure;
    IntGcd = "praxis_int_gcd": (Ctx, Gc, Gc) -> Gc, AllocatesAndFaults;
    IntGe = "praxis_int_ge": (Ctx, Gc, Gc) -> Gc, Pure;
    IntGt = "praxis_int_gt": (Ctx, Gc, Gc) -> Gc, Pure;
    IntLcm = "praxis_int_lcm": (Ctx, Gc, Gc) -> Gc, AllocatesAndFaults;
    IntLe = "praxis_int_le": (Ctx, Gc, Gc) -> Gc, Pure;
    IntLoad = "praxis_int_load": (Ctx, Gc) -> RawI64, Pure;
    IntLt = "praxis_int_lt": (Ctx, Gc, Gc) -> Gc, Pure;
    IntMax = "praxis_int_max": (Ctx, Gc, Gc) -> Gc, Pure;
    IntMin = "praxis_int_min": (Ctx, Gc, Gc) -> Gc, Pure;
    IntMul = "praxis_int_mul": (Ctx, Gc, Gc) -> Gc, AllocatesAndFaults;
    IntNe = "praxis_int_ne": (Ctx, Gc, Gc) -> Gc, Pure;
    IntNeg = "praxis_int_neg": (Ctx, Gc) -> Gc, AllocatesAndFaults;
    IntRem = "praxis_int_rem": (Ctx, Gc, Gc) -> Gc, AllocatesAndFaults;
    IntSaturatingAdd = "praxis_int_saturating_add": (Ctx, Gc, Gc) -> Gc, Allocates;
    IntSaturatingMul = "praxis_int_saturating_mul": (Ctx, Gc, Gc) -> Gc, Allocates;
    IntSaturatingSub = "praxis_int_saturating_sub": (Ctx, Gc, Gc) -> Gc, Allocates;
    IntSign = "praxis_int_sign": (Ctx, Gc) -> Gc, Allocates;
    IntSub = "praxis_int_sub": (Ctx, Gc, Gc) -> Gc, AllocatesAndFaults;
    IntToChar = "praxis_int_to_char": (Ctx, Gc) -> Gc, AllocatesAndFaults;
    IntToFloat = "praxis_int_to_float": (Ctx, Gc) -> Gc, Allocates;
    IntWrappingAdd = "praxis_int_wrapping_add": (Ctx, Gc, Gc) -> Gc, Allocates;
    IntWrappingMul = "praxis_int_wrapping_mul": (Ctx, Gc, Gc) -> Gc, Allocates;
    IntWrappingSub = "praxis_int_wrapping_sub": (Ctx, Gc, Gc) -> Gc, Allocates;
    MapContains = "praxis_map_contains": (Ctx, Gc, Gc) -> Gc, Pure;
    RangeGet = "praxis_range_get": (Ctx, Gc, Gc) -> Gc, AllocatesAndFaults;
    RangeLen = "praxis_range_len": (Ctx, Gc) -> Gc, AllocatesAndFaults;
    RangeNew = "praxis_range_new": (Ctx, Gc, Gc) -> Gc, Allocates;
    RangeNewInclusive = "praxis_range_new_inclusive": (Ctx, Gc, Gc) -> Gc, Allocates;
    MapGet = "praxis_map_get": (Ctx, Gc, Gc) -> Gc, Allocates;
    MapIndex = "praxis_map_index": (Ctx, Gc, Gc) -> Gc, Faults;
    MapInsert = "praxis_map_insert": (Ctx, Gc, Gc, Gc) -> GcUnit, Allocates;
    MapIsEmpty = "praxis_map_is_empty": (Ctx, Gc) -> Gc, Pure;
    MapKeys = "praxis_map_keys": (Ctx, Gc) -> Gc, Allocates;
    MapLen = "praxis_map_len": (Ctx, Gc) -> Gc, Allocates;
    MapNew = "praxis_map_new": (Ctx, Ptr) -> Gc, Allocates;
    MapRemove = "praxis_map_remove": (Ctx, Gc, Gc) -> GcUnit, Pure;
    MapUpdateMax = "praxis_map_update_max": (Ctx, Gc, Gc, Gc) -> GcUnit, Allocates;
    MapValues = "praxis_map_values": (Ctx, Gc) -> Gc, Allocates;
    MapUpdateMin = "praxis_map_update_min": (Ctx, Gc, Gc, Gc) -> GcUnit, Allocates;
    MaxHeapIsEmpty = "praxis_max_heap_is_empty": (Ctx, Gc) -> Gc, Pure;
    MaxHeapItems = "praxis_max_heap_items": (Ctx, Gc) -> Gc, Allocates;
    MaxHeapLen = "praxis_max_heap_len": (Ctx, Gc) -> Gc, Allocates;
    MaxHeapNew = "praxis_max_heap_new": (Ctx, Ptr) -> Gc, Allocates;
    MaxHeapPeek = "praxis_max_heap_peek": (Ctx, Gc) -> Gc, Faults;
    MaxHeapPop = "praxis_max_heap_pop": (Ctx, Gc) -> Gc, Faults;
    MaxHeapPush = "praxis_max_heap_push": (Ctx, Gc, Gc) -> GcUnit, Allocates;
    MinHeapIsEmpty = "praxis_min_heap_is_empty": (Ctx, Gc) -> Gc, Pure;
    MinHeapItems = "praxis_min_heap_items": (Ctx, Gc) -> Gc, Allocates;
    MinHeapLen = "praxis_min_heap_len": (Ctx, Gc) -> Gc, Allocates;
    MinHeapNew = "praxis_min_heap_new": (Ctx, Ptr) -> Gc, Allocates;
    MinHeapPeek = "praxis_min_heap_peek": (Ctx, Gc) -> Gc, Faults;
    MinHeapPop = "praxis_min_heap_pop": (Ctx, Gc) -> Gc, Faults;
    MinHeapPush = "praxis_min_heap_push": (Ctx, Gc, Gc) -> GcUnit, Allocates;
    Panic = "praxis_panic": (Ctx, Gc) -> GcUnit, Faults;
    RaiseDivByZeroIf = "praxis_raise_div_by_zero_if": (Ctx, RawI64) -> Void, Faults;
    RaiseEmptyCollection = "praxis_raise_empty_collection": (Ctx) -> GcUnit, Faults;
    RaiseIntOverflowIf = "praxis_raise_int_overflow_if": (Ctx, RawI64) -> Void, Faults;
    RaiseStackOverflow = "praxis_raise_stack_overflow": (Ctx) -> Void, Faults;
    RecordField = "praxis_record_field": (Ctx, Gc, RawU32) -> Gc, Pure;
    RecordSetField = "praxis_record_set_field": (Ctx, Gc, RawU32, Gc) -> Gc, Pure;
    RunParser = "praxis_run_parser": (Ctx, Gc, Gc) -> Gc, AllocatesAndFaults;
    SetContains = "praxis_set_contains": (Ctx, Gc, Gc) -> Gc, Pure;
    SetInsert = "praxis_set_insert": (Ctx, Gc, Gc) -> GcUnit, Allocates;
    SetIsEmpty = "praxis_set_is_empty": (Ctx, Gc) -> Gc, Pure;
    SetItems = "praxis_set_items": (Ctx, Gc) -> Gc, Allocates;
    SetLen = "praxis_set_len": (Ctx, Gc) -> Gc, Allocates;
    SetNew = "praxis_set_new": (Ctx, Ptr) -> Gc, Allocates;
    SetRemove = "praxis_set_remove": (Ctx, Gc, Gc) -> GcUnit, Pure;
    SnapshotDebugChain = "praxis_snapshot_debug_chain": (Ctx) -> Void, Pure;
    StructEq = "praxis_struct_eq": (Ctx, Gc, Gc) -> RawI64, Pure;
    TextConcat = "praxis_text_concat": (Ctx, Gc, Gc) -> Gc, Allocates;
    TextGet = "praxis_text_get": (Ctx, Gc, Gc) -> Gc, AllocatesAndFaults;
    TextIsEmpty = "praxis_text_is_empty": (Ctx, Gc) -> Gc, Pure;
    TextLen = "praxis_text_len": (Ctx, Gc) -> Gc, Allocates;
    TupleGet = "praxis_tuple_get": (Ctx, Gc, RawI64) -> Gc, Pure;
    TupleSet = "praxis_tuple_set": (Ctx, Gc, RawI64, Gc) -> Gc, Pure;
    ValueCmp = "praxis_value_cmp": (Ctx, Gc, Gc) -> RawI64, Faults;
    VarCellGet = "praxis_var_cell_get": (Ctx, Gc) -> Gc, Pure;
    VarCellSet = "praxis_var_cell_set": (Ctx, Gc, Gc) -> Gc, Pure;
    VecFrequencies = "praxis_vec_frequencies": (Ctx, Gc) -> Gc, Allocates;
    VecGet = "praxis_vec_get": (Ctx, Gc, Gc) -> Gc, Faults;
    VecIsEmpty = "praxis_vec_is_empty": (Ctx, Gc) -> Gc, Pure;
    VecLen = "praxis_vec_len": (Ctx, Gc) -> Gc, Allocates;
    VecNew = "praxis_vec_new": (Ctx, Ptr) -> Gc, Allocates;
    VecPush = "praxis_vec_push": (Ctx, Gc, Gc) -> GcUnit, AllocatesAndFaults;
    // `sorted` faults and `unique` does not, and the difference is derived from
    // the wrappers rather than guessed: `praxis_vec_sorted` raises
    // `TypeMismatch` when the element type has no `compare`, while
    // `praxis_vec_unique` and `praxis_vec_frequencies` go through `DynamicKey`,
    // which answers "not equal" for a type with no `equals` instead of raising.
    VecSorted = "praxis_vec_sorted": (Ctx, Gc) -> Gc, AllocatesAndFaults;
    VecUnique = "praxis_vec_unique": (Ctx, Gc) -> Gc, Allocates;
    WriteStdout = "praxis_write_stdout": (Ctx, Gc) -> GcUnit, Pure;
}

/// Build-time coverage of the effect table (P0-08c).
///
/// The allocation effect used to live on `MethodEntry.allocates`, a hand-written
/// `bool` per catalog row that had already drifted from the wrapper it described
/// (`Vec.len` said `false`; `praxis_vec_len` boxes its result and can collect).
/// The field is deleted; the manifest is the one answer, and this walks every
/// row of it *at compile time* so a symbol can neither be added without an
/// effect nor left out of [`RuntimeSymbol::ALL`], which is what the rest of the
/// workspace iterates.
///
/// Anything checkable statically is checked here rather than in a test: a
/// classification error should fail the build, not a test run.
const _: () = {
    // `ALL` is generated from the same rows as the enum, so a non-empty `ALL`
    // that ends at the last variant means every variant is present.
    assert!(!RuntimeSymbol::ALL.is_empty());

    let mut i = 0;
    while i < RuntimeSymbol::ALL.len() {
        let sym = RuntimeSymbol::ALL[i];
        let sig = sym.sig();

        // Every wrapper leads with the context pointer. Without it there is no
        // route to the heap, the fault slot or the root set — so a wrapper
        // lacking one could be neither a safepoint nor a faulting call, and any
        // effect other than `Pure` would be a lie.
        assert!(matches!(sig.params[0], AbiKind::Ctx));

        // A wrapper that returns nothing produced no object, so `Allocates`
        // would misclassify it — and `Allocates` is exactly what makes a call
        // site a safepoint that the caller must spill its live roots across.
        assert!(!(matches!(sig.ret, AbiRet::Void) && sig.effect.allocates()));

        // The two queries partition the four variants; `allocates`/`faults`
        // must agree with the row rather than being independently answerable.
        assert!(sig.effect.allocates() == sym.allocates());
        assert!(sig.effect.faults() == sym.faults());

        // `GcUnit` gets no check here on purpose. The invariant it exists for
        // relates a manifest row to a *catalog* row — a non-faulting wrapper
        // with a non-`Unit` result type must not be able to answer the sentinel
        // (RT-14/RT-15) — and the catalog is built at run time, so the check
        // lives in `builtins::tests::a_non_faulting_row_with_a_value_result_\
        // cannot_answer_the_unit_sentinel`. An assertion here that restated
        // something already true would say nothing.

        i += 1;
    }
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The manifest is a bijection between variants and linker names. A typo
    /// that duplicated a name would otherwise make two symbols resolve to one
    /// address.
    #[test]
    fn names_are_unique_and_well_formed() {
        let mut seen = HashSet::new();
        for &sym in RuntimeSymbol::ALL {
            assert!(
                sym.name().starts_with("praxis_"),
                "{sym} is not a praxis_* symbol"
            );
            assert!(seen.insert(sym.name()), "duplicate symbol name {sym}");
        }
        assert_eq!(seen.len(), RuntimeSymbol::ALL.len());
    }

    /// `ALL` must list every variant. It is generated from the same rows as the
    /// enum, so this is really a check that the macro was not edited apart.
    #[test]
    fn from_name_round_trips_every_symbol() {
        for &sym in RuntimeSymbol::ALL {
            assert_eq!(RuntimeSymbol::from_name(sym.name()), Some(sym));
        }
        assert_eq!(RuntimeSymbol::from_name("praxis_not_a_symbol"), None);
    }

    /// Every wrapper takes the context pointer first: the fault slot, the heap
    /// and the root set all hang off it, so a wrapper without it could not
    /// allocate, fault or be a safepoint.
    #[test]
    fn every_symbol_leads_with_the_context_pointer() {
        for &sym in RuntimeSymbol::ALL {
            let sig = sym.sig();
            assert_eq!(
                sig.params.first(),
                Some(&AbiKind::Ctx),
                "{sym} does not take ctx first"
            );
            assert!(
                !sig.params[1..].contains(&AbiKind::Ctx),
                "{sym} takes ctx more than once"
            );
        }
    }

    #[test]
    fn effect_queries_agree_with_the_variants() {
        assert!(!Effect::Pure.allocates() && !Effect::Pure.faults());
        assert!(!Effect::Faults.allocates() && Effect::Faults.faults());
        assert!(Effect::Allocates.allocates() && !Effect::Allocates.faults());
        assert!(Effect::AllocatesAndFaults.allocates() && Effect::AllocatesAndFaults.faults());
    }

    /// **A standing invariant, not a gate** (REP-46): none of the nine overflow
    /// alternatives may be declared faulting.
    ///
    /// It is green by construction the moment the rows exist, so it was never
    /// red for this change and is not counted among its gates. What it catches
    /// is a *later* edit marking one `AllocatesAndFaults` — which would make MIR
    /// emit a `CheckFault` after a call that never faults, i.e. REP-53's failure
    /// mode arriving from the other end, and would quietly undo the one property
    /// that makes these methods alternatives to a faulting operator at all.
    #[test]
    fn no_overflow_alternative_declares_that_it_faults() {
        use RuntimeSymbol::*;
        for sym in [
            IntWrappingAdd,
            IntSaturatingAdd,
            IntCheckedAdd,
            IntWrappingSub,
            IntSaturatingSub,
            IntCheckedSub,
            IntWrappingMul,
            IntSaturatingMul,
            IntCheckedMul,
        ] {
            assert_eq!(
                sym.sig().effect,
                Effect::Allocates,
                "`{}` answers a fresh number and cannot fault (§4.12)",
                sym.name()
            );
        }
    }

    /// **ADR-111.** `praxis_alloc_text` trusts its bytes, and the row is where
    /// that is said.
    ///
    /// The UTF-8 requirement is the caller's precondition, not a runtime
    /// judgement: the compiler's bytes come from a Rust `String` unbroken from
    /// `Lit::Text` through `Generation::alloc_str`, and the one runtime caller
    /// that holds raw host bytes (`praxis_get_input`) validates them itself. A
    /// violation panics into `abi_guard!` and aborts; it never sets a fault.
    ///
    /// Written as an assertion rather than left to the manifest because the row
    /// is read by three things at once and only this one is visible: it decides
    /// whether `Inst::Alloc { AllocKind::Text }` is followed by a `CheckFault`
    /// (ADR-088), whether a `Text` literal in a loop is hoisted into the
    /// preheader (ADR-108 §3), and whether `panic_fault_is_observable` lets the
    /// wrapper's panic path abort. A later edit marking it faulting again would
    /// silently restore 41 corpus checks, un-hoist every `Text` literal, and
    /// make the abort a fault — this makes it a failing test instead.
    #[test]
    fn alloc_text_trusts_its_bytes_and_the_row_says_so() {
        assert_eq!(
            RuntimeSymbol::AllocText.sig().effect,
            Effect::Allocates,
            "`praxis_alloc_text`'s UTF-8 requirement is its caller's precondition \
             (ADR-111); declaring it faulting puts a check back after every text \
             literal and takes `Text` back out of the ADR-108 hoist"
        );
        // And the wrapper the fault moved *to* still declares it, or the
        // relocation would read as a deletion.
        assert!(
            RuntimeSymbol::GetInput.faults(),
            "`praxis_get_input` holds raw host bytes and raises `InvalidText` \
             itself, so the fault still lands at the `read`"
        );
    }

    /// Spot-check the rows the compiler is most sensitive to: the two that take
    /// a narrow `u32` (the arity-only signature synthesis this manifest
    /// replaces passed an `i64` here), and the arithmetic wrappers whose
    /// fault-and-allocate pair drives both the safepoint and the fault check.
    #[test]
    fn narrow_and_faulting_rows_are_recorded_exactly() {
        assert_eq!(
            RuntimeSymbol::RecordField.sig().params,
            &[AbiKind::Ctx, AbiKind::Gc, AbiKind::RawU32]
        );
        assert_eq!(
            RuntimeSymbol::RecordSetField.sig().params,
            &[AbiKind::Ctx, AbiKind::Gc, AbiKind::RawU32, AbiKind::Gc]
        );

        for sym in [
            RuntimeSymbol::IntAdd,
            RuntimeSymbol::IntSub,
            RuntimeSymbol::IntMul,
            RuntimeSymbol::IntDiv,
            RuntimeSymbol::IntRem,
            RuntimeSymbol::IntNeg,
        ] {
            assert_eq!(sym.sig().effect, Effect::AllocatesAndFaults, "{sym}");
        }
        // Comparisons hand back an immortal Bool: no collection can happen
        // inside them, so they are not safepoints.
        for sym in [
            RuntimeSymbol::IntEq,
            RuntimeSymbol::IntLt,
            RuntimeSymbol::IntGe,
        ] {
            assert_eq!(sym.sig().effect, Effect::Pure, "{sym}");
        }
    }
}
