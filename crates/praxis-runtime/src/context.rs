//! The [`RuntimeContext`] handed to every generated function (§10.3, Appendix B)
//! and the [`Runtime`] that owns the heap + immortals.
//!
//! Every generated function receives a hidden first parameter — a pointer to
//! `RuntimeContext` — followed only by `GcRef` arguments, and returns one
//! `GcRef`. The context is the single channel through which generated code
//! reaches the GC heap, the pending fault, the debug frame chain, the input
//! source, and so on.

use crate::crash_snapshot::{CrashSnapshot, SnapshotSlot};
use crate::debug::{
    DebugFrameEntry, DebugFrameStack, DebugFrameStackHeader, DebugValueStack,
    DebugValueStackHeader, DEBUG_FRAME_STACK_SLOTS, DEBUG_VALUE_STACK_SLOTS,
};
use crate::gc::GcRef;
use crate::heap::Heap;
use crate::immortal::{read_bool, Immortals};
use crate::parse_detail::ParseDetail;
#[cfg(test)]
use crate::roots::RootSet;
use crate::shadow_stack::{ShadowStack, ShadowStackHeader, SHADOW_STACK_SLOTS};
use crate::{
    collections::VecPayload,
    descriptor::{builtin_descriptor_addresses, BuiltinTypeId, TypeDescriptor},
};

/// What *any* call spends, however narrow: the floor of [`frame_cost`], in
/// bytes (ADR-105).
///
/// Measured, not assumed, and measured on **both** targets this backend
/// supports — which is the whole of why it is 160 and not the 134 ADR-105
/// landed.
///
/// - **arm64.** Bisecting the abort depth of recursive Praxis programs under
///   `ulimit -s` (release) gives a native frame of `99 + 1.06 × gc_locals`
///   bytes: 86 B for a minimal frame, 294 B for one carrying twenty-two live
///   collections. 134 was that fit at [`REFERENCE_FRAME_SLOTS`], rounded up.
/// - **x86_64.** Read straight off Cranelift's own `frame_layout` by
///   `audit_frame_cost` over every function the test suite and the book compile
///   — 2669 frames, which is a census rather than a fit. Narrow frames are
///   *wider* here than arm64's: the x64 backend has half the registers to
///   allocate out of and spills accordingly, so a function of eleven `Gc` locals
///   or fewer reaches 128 B and one of twenty-seven reaches 176 B, against a
///   134 B charge. That is the assert ADR-105 decision 5 exists to fire, and it
///   fired.
///
/// 160 covers the worst frame in that census with a 16-byte margin — one full
/// stack quantum on a target that aligns to 16 — so the model still over-charges
/// every real frame and the budget below is still a ceiling rather than an
/// estimate.
///
/// **One constant, not one per target, and that is deliberate.** ADR-105
/// decision 3 refused to derive the budget from the host's `getrlimit` because
/// it would make the same program fault at different depths on different
/// machines, "and the depth at which a Praxis program stops recursing would stop
/// being a property of Praxis". A per-target charge gives that away just as
/// surely: [`STACK_BUDGET_BYTES`] scales with this constant, so a target-varying
/// base leaves a *reference* frame at [`MAX_RECURSION_DEPTH`] everywhere but
/// moves the depth of every wider one. The charge is therefore the high-water
/// mark across targets, which costs arm64 only headroom it does not use.
///
/// **[`FRAME_BYTES_PER_SLOT`] is left alone at 2**, though raising it would have
/// bought the same margin. The x86_64 census finds no slope worth the name — a
/// 450-slot function frames in 192 B and a 105-slot one in 288 B, because `Gc`
/// locals live in the shadow and debug slot stacks rather than in the native
/// frame — so a steeper per-slot term would be an invented number that took
/// depth from wide frames to fix a floor that was too low.
///
/// **It is a floor and not merely a base, and that is load-bearing.** A frame
/// narrower than the reference is charged the same, so no call can ever cost
/// less than this — which is what makes
/// [`DEBUG_FRAME_STACK_SLOTS`](crate::debug::DEBUG_FRAME_STACK_SLOTS) sound at
/// `MAX_RECURSION_DEPTH + 1`. A cost that was proportional from zero would let
/// the budget buy more minimum-width frames than that reservation covers.
///
/// The backend checks the model rather than trusting it: after Cranelift has
/// compiled a function it knows the real frame size, and a `debug_assert`
/// there fails the build's test run if any function's actual frame outgrows
/// what [`frame_cost`] charged for it.
pub const FRAME_BYTES_BASE: u32 = 160;

/// What each `Gc` local *past* [`REFERENCE_FRAME_SLOTS`] adds to a frame's cost,
/// in bytes (ADR-105). Rounds up arm64's measured 1.06 B per local; x86_64
/// measures no slope at all, and [`FRAME_BYTES_BASE`] says why that left this
/// constant where it was.
pub const FRAME_BYTES_PER_SLOT: u32 = 2;

/// The deepest recursion a *reference-width* function reaches, and the figure
/// [`STACK_BUDGET_BYTES`] is derived from.
///
/// What runs out is bytes, not calls, and a frame's byte cost varies by a
/// factor of three with its width — so the guard spends a byte budget rather
/// than counting calls, because a count calibrated for a narrow frame lets a
/// wide one abort the host (ADR-105). This constant is the *anchor* of that
/// budget: a reference frame recurses exactly this deep.
pub const MAX_RECURSION_DEPTH: u32 = 8000;

/// The frame width [`MAX_RECURSION_DEPTH`] is calibrated against: the `Gc`
/// local count of
///
/// ```praxis
/// fn count(n: Int) -> Int { if n == 0 { 0 } else { 1 + count(n - 1) } }
/// ```
///
/// — the program that constant was chosen for, and the one
/// `adv_deep_recursion_over_limit_faults_cleanly` uses.
///
/// **Anchoring the budget here rather than at zero is deliberate.** A zero-slot
/// function is a hypothetical: every real Praxis function boxes something, and
/// the simplest recursive one there is takes eleven `Gc` locals. Deriving the
/// budget from `frame_cost(0)` would let *that* function recurse only 6686 deep
/// rather than 8000 — a 16% cut to every ordinary program, to bound a cost only
/// wide frames incur. Anchoring at the reference frame takes depth only from
/// the frames that over-reach.
///
/// `a_reference_frame_still_recurses_as_deep_as_the_call_count_allowed` is the
/// end-to-end gate; if a codegen change makes `count` wider, that test fails and
/// this constant is what to re-measure.
pub const REFERENCE_FRAME_SLOTS: u32 = 11;

/// The native stack, in bytes, that Praxis frames may occupy — and the largest
/// budget a host may install (ADR-105).
///
/// **Why this number and not the real stack limit.** The two stacks Praxis
/// actually runs on are 8 MiB (a macOS main thread, where `praxis run` calls the
/// JIT entry) and 2 MiB (std's default for a spawned thread, which is what the
/// whole `cargo test` suite runs on). `getrlimit` answers for the first and not
/// the second, so asking the OS gives a number that is wrong exactly where the
/// suite lives. Choosing one figure that fits under *both*, with room to spare,
/// removes the question instead of answering it: it is what
/// `MAX_RECURSION_DEPTH` reference frames cost — about 1.22 MiB charged — and no
/// frame shape can exceed it, because the guard charges by shape.
///
/// What is *consumed* is below that by however much the model over-charges the
/// shape doing the recursing, and the model over-charges every shape: the
/// reference program frames in 80 bytes on x86_64 against a 160-byte charge, so
/// it reaches 8000 deep on 625 KiB. The tightest ratio in the census behind
/// [`FRAME_BYTES_BASE`] — 176 actual bytes against 192 charged — puts the true
/// worst case at 1.12 MiB, which is the figure to hold against the 2 MiB thread
/// stack rather than the charged one.
///
/// A host that knows better may lower it through
/// [`Runtime::set_stack_budget`](crate::Runtime::set_stack_budget). It may not
/// raise it: [`SHADOW_STACK_SLOTS`](crate::SHADOW_STACK_SLOTS) is sized from
/// this constant, and a larger budget would make shadow-stack exhaustion
/// reachable again. [`StackBudget`] is what makes that unrepresentable rather
/// than documented.
pub const STACK_BUDGET_BYTES: u32 = MAX_RECURSION_DEPTH * FRAME_BYTES_BASE;

/// What one call spends of [`StackBudget`]: a floor, plus a per-slot term for
/// every `Gc` local past the reference width (ADR-105).
///
/// The backend knows `slots` before it emits the prologue, so this folds to one
/// immediate and the guard is four instructions.
///
/// The `saturating_sub` is the floor, and it does two jobs. It keeps an ordinary
/// function at [`MAX_RECURSION_DEPTH`] — see [`REFERENCE_FRAME_SLOTS`] — and it
/// makes [`FRAME_BYTES_BASE`] the *minimum* any call can spend, which is the
/// premise [`DEBUG_FRAME_STACK_SLOTS`](crate::debug::DEBUG_FRAME_STACK_SLOTS)
/// is sized on.
///
/// **`slots` is the count of `Gc` locals, not the count of shadow slots**
/// (ADR-128 decision 4) — a [`DebugSlotCount`](crate::DebugSlotCount) at the one
/// real call site, so at most [`MAX_DEBUG_VALUE_SLOTS`](crate::MAX_DEBUG_VALUE_SLOTS).
/// Colouring makes that a different number from
/// [`MAX_SHADOW_SLOTS`](crate::MAX_SHADOW_SLOTS), and the charge deliberately
/// rides the larger one: `FRAME_BYTES_PER_SLOT` is not rent on a shadow slot,
/// it is a calibrated proxy for the *native* frame, and under-reporting that is
/// the SIGABRT ADR-105 exists to remove.
///
/// So the bound this must not overflow is `frame_cost(4096)` = `160 + 2 × 4085`
/// = 8330, comfortably inside `u32` and inside [`STACK_BUDGET_BYTES`] — and the
/// saturating arithmetic is belt-and-braces for a caller that has not proved
/// even that.
#[must_use]
pub const fn frame_cost(slots: u32) -> u32 {
    let over = slots.saturating_sub(REFERENCE_FRAME_SLOTS);
    FRAME_BYTES_BASE.saturating_add(FRAME_BYTES_PER_SLOT.saturating_mul(over))
}

/// A native-stack budget a [`RuntimeContext`] may be minted with: a `u32` proven
/// no larger than [`STACK_BUDGET_BYTES`] at construction.
///
/// The proof is the point. `SHADOW_STACK_SLOTS` is sized from
/// `STACK_BUDGET_BYTES`, on the strength of "a frame spends at least
/// `FRAME_BYTES_PER_SLOT` per slot it claims, so the slots of every live frame
/// sum to at most `budget / FRAME_BYTES_PER_SLOT`". A host that could install a
/// larger budget would make shadow-stack overflow reachable from generated
/// code — silently, because generated code does not check the reservation. It
/// cannot: [`StackBudget::new`] is the only constructor and it refuses.
///
/// Same shape as [`SlotCount`](crate::SlotCount), and for the same reason: the
/// bound is checked once, where the value is made, and every consumer downstream
/// may assume it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StackBudget(u32);

impl StackBudget {
    /// The budget every [`Runtime`] starts with: the whole of
    /// [`STACK_BUDGET_BYTES`].
    pub const DEFAULT: StackBudget = StackBudget(STACK_BUDGET_BYTES);

    /// `Some` iff `bytes` is a budget the shadow-stack reservation covers.
    ///
    /// `const` so a caller can prove a literal at compile time.
    #[must_use]
    pub const fn new(bytes: u32) -> Option<StackBudget> {
        if bytes <= STACK_BUDGET_BYTES {
            Some(StackBudget(bytes))
        } else {
            None
        }
    }

    /// The budget in bytes, which is `<= STACK_BUDGET_BYTES` by construction.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl Default for StackBudget {
    fn default() -> Self {
        StackBudget::DEFAULT
    }
}

/// What kind of runtime fault occurred (§9.2, §10.4). Set by the runtime
/// wrapper that detected it; read by the host after the generated code unwinds
/// to its fault epilogue.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    /// No fault pending. The zero state.
    None = 0,
    /// Integer arithmetic overflowed (§4.12).
    IntOverflow = 1,
    /// Division or remainder by zero (§4.12).
    DivByZero = 2,
    /// A collection index was out of bounds (§9.2). Raised by `Vec.get` /
    /// indexing and similar accessors.
    IndexOutOfBounds = 3,
    /// An input parse mismatch (§7.11). Raised by the input-parser interpreter
    /// when the input does not match a parser expression. The interpreter also
    /// records the deepest mismatch in the runtime's [`crate::ParseDetail`]
    /// slot, which the host reads to render the input/parser spans.
    ParseFailed = 4,
    /// An operation required a non-empty collection but found an empty one
    /// (§9.2). Raised by `Deque.pop_front`/`pop_back`, heap `pop`/`peek`, and
    /// similar accessors on an empty collection.
    EmptyCollection = 5,
    /// Recursion exhausted the native-stack budget (§9.2, §17.4). Raised by the
    /// prologue guard in every generated function when `stack_left` is less than
    /// this frame's [`frame_cost`], so the host survives deep recursion
    /// (`count(100000)` and similar) instead of overflowing the native stack and
    /// aborting (SIGABRT).
    StackOverflow = 6,
    /// A `Float` value could not be converted to `Int`: NaN, ±infinity, or a
    /// finite value outside the signed 64-bit range (§4.12). `Float` arithmetic
    /// itself never faults (per IEEE-754 it produces inf/nan); only the
    /// narrowing `to_int` conversion does.
    FloatToInt = 7,
    /// A code point was not a Unicode scalar value: negative, above
    /// `0x10FFFF`, or in the surrogate range `D800..=DFFF` (§4.3). Raised by
    /// `praxis_alloc_char`.
    InvalidChar = 8,
    /// Host input that had to be `Text` was not valid UTF-8 (§4.3). Raised by
    /// `praxis_get_input`, which is its only producer (ADR-111).
    ///
    /// The validation sits at the one caller holding bytes it did not author. A
    /// `Text` *literal*'s bytes come from a Rust `String` and cannot fail, so
    /// its `Alloc` is non-faulting, and a violated precondition in
    /// `praxis_alloc_text` aborts rather than faulting, the way
    /// `praxis_int_load`'s does.
    ///
    /// Generated code reads `FaultKind` directly since ADR-102, so renumbering
    /// a variant is an ABI change and not a tidy-up. This one is unreachable
    /// from `praxis run`, whose `lazy_stdin::read` validates stdin and exits 2
    /// — it exists for an embedder that does not.
    InvalidText = 9,
    /// A size or extent the runtime cannot honour: a negative `Grid` width or
    /// height, a `width * height` that overflows or exceeds
    /// [`GridExtent::MAX_CELLS`](crate::collections::GridExtent::MAX_CELLS), or
    /// a `BitSet` member outside [`BitIndex`](crate::bitset::BitIndex)'s range
    /// (§9.2). Each is checked *before* the `usize` cast, where a negative
    /// extent would otherwise land near `usize::MAX` and become an OOM abort or
    /// a capacity-overflow panic across `extern "C"`.
    InvalidSize = 10,
    /// A value did not have the type its destination declared: pushing a
    /// `Float` into a `Vec[Int]`, or constructing a `Grid[T]` whose cell type
    /// has no default value to fill with (§9.2).
    TypeMismatch = 11,
    /// The program called `panic(value)` (§9.1). The value it passed is
    /// rendered through its descriptor into the runtime's [`FaultMessage`]
    /// slot, so the fault says *what* the program stopped for.
    Panic = 12,
    /// An `assert(condition)` found its condition false (§9.1). Carries no
    /// message: `assert` takes a condition and nothing else, so any text would
    /// only restate the kind. `panic` is the name that carries words.
    AssertFailed = 13,
    /// A range with no members was asked for a member: `clamp(v, low, high)`
    /// with `low > high` (ADR-058), which names an empty inclusive range and so
    /// has no value to clamp to.
    ///
    /// `praxis_range_len`'s uncountable range is deliberately *not* this kind:
    /// it raises [`IntOverflow`](Self::IntOverflow), because `Int::MIN..Int::MAX`
    /// is the *fullest* range there is and calling it empty would be a fault
    /// message that lies (ADR-059, ADR-075).
    EmptyRange = 14,
    /// An argument this algorithm has no answer for: a negative edge weight in
    /// `dijkstra`/`a_star`, whose settle-once-and-never-reconsider shape makes a
    /// negative edge silently overstate the answer, and a negative heuristic,
    /// which makes `f = g + h` decrease along a path (ADR-060).
    ///
    /// The operand is well-formed and the graph is well-formed; what is absent
    /// is a *correct answer this algorithm could produce*, which is why neither
    /// [`InvalidSize`](Self::InvalidSize) nor
    /// [`TypeMismatch`](Self::TypeMismatch) fits: an answer the walk cannot
    /// compute is a fault, not a wrong number (ADR-060).
    NoAnswer = 15,
}

/// The message a [`FaultKind::Panic`] or [`FaultKind::AssertFailed`] carries.
///
/// A fault kind alone cannot say what the program stopped *for*, and `panic`'s
/// whole contract is an explicit message (§9.1). The wrapper renders the value
/// the program passed — through its descriptor, exactly as `out` would — and
/// leaves the text here; the host reads it when it renders the fault.
///
/// Rendering happens in the wrapper rather than at report time on purpose: the
/// argument is a `GcRef` into a heap that the host tears down, so keeping the
/// reference would make the message outlive what it points at. A `String` does
/// not.
///
/// Host-managed, like [`crate::ParseDetail`]: generated code never reads or
/// writes it.
#[derive(Debug, Default)]
pub struct FaultMessage {
    text: Option<String>,
}

impl FaultMessage {
    /// An empty slot.
    #[must_use]
    pub fn new() -> FaultMessage {
        FaultMessage { text: None }
    }

    /// Record `text` as the message for the fault being raised.
    pub fn set(&mut self, text: String) {
        self.text = Some(text);
    }

    /// The recorded message, or `None` when the pending fault carries none.
    #[must_use]
    pub fn get(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// Forget any recorded message.
    pub fn clear(&mut self) {
        self.text = None;
    }
}

/// A [`FaultKind`] that is actually a fault.
///
/// [`Fault::set`] takes one of these, so "raise the absence of a fault" has no
/// spelling.
///
/// The associated constants are the whole raisable set. There is no
/// `RaisedFault(FaultKind::None)` to construct — [`RaisedFault::new`] is the
/// only fallible route in, and it is for a kind that arrives as data.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RaisedFault(FaultKind);

impl RaisedFault {
    /// Integer arithmetic overflowed (§4.12).
    pub const INT_OVERFLOW: RaisedFault = RaisedFault(FaultKind::IntOverflow);
    /// Division or remainder by zero (§4.12).
    pub const DIV_BY_ZERO: RaisedFault = RaisedFault(FaultKind::DivByZero);
    /// A collection index was out of bounds (§9.2).
    pub const INDEX_OUT_OF_BOUNDS: RaisedFault = RaisedFault(FaultKind::IndexOutOfBounds);
    /// An input parse mismatch (§7.11).
    pub const PARSE_FAILED: RaisedFault = RaisedFault(FaultKind::ParseFailed);
    /// An operation required a non-empty collection (§9.2).
    pub const EMPTY_COLLECTION: RaisedFault = RaisedFault(FaultKind::EmptyCollection);
    /// Recursion exceeded the depth limit (§9.2, §17.4).
    pub const STACK_OVERFLOW: RaisedFault = RaisedFault(FaultKind::StackOverflow);
    /// A `Float` had no exact `Int` (§4.12).
    pub const FLOAT_TO_INT: RaisedFault = RaisedFault(FaultKind::FloatToInt);
    /// A code point was not a Unicode scalar value (§4.3).
    pub const INVALID_CHAR: RaisedFault = RaisedFault(FaultKind::InvalidChar);
    /// A byte buffer that had to be `Text` was not valid UTF-8 (§4.3).
    pub const INVALID_TEXT: RaisedFault = RaisedFault(FaultKind::InvalidText);
    /// A size or extent the runtime cannot honour (§9.2).
    pub const INVALID_SIZE: RaisedFault = RaisedFault(FaultKind::InvalidSize);
    /// A value did not have the type its destination declared (§9.2).
    pub const TYPE_MISMATCH: RaisedFault = RaisedFault(FaultKind::TypeMismatch);
    /// The program called `panic(value)` (§9.1).
    pub const PANIC: RaisedFault = RaisedFault(FaultKind::Panic);
    /// An `assert(condition)` found its condition false (§9.1).
    pub const ASSERT_FAILED: RaisedFault = RaisedFault(FaultKind::AssertFailed);
    /// A range with no members was asked for a member (ADR-058).
    pub const EMPTY_RANGE: RaisedFault = RaisedFault(FaultKind::EmptyRange);
    /// An argument this algorithm has no answer for (ADR-060).
    pub const NO_ANSWER: RaisedFault = RaisedFault(FaultKind::NoAnswer);

    /// The raisable fault `kind` names, or `None` for [`FaultKind::None`] —
    /// which is the *absence* of a fault and cannot be raised.
    #[must_use]
    pub const fn new(kind: FaultKind) -> Option<RaisedFault> {
        match kind {
            FaultKind::None => None,
            raisable => Some(RaisedFault(raisable)),
        }
    }

    /// The kind this raises.
    #[inline]
    #[must_use]
    pub const fn kind(self) -> FaultKind {
        self.0
    }
}

impl std::fmt::Display for FaultKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FaultKind::None => write!(f, "no fault"),
            FaultKind::IntOverflow => write!(f, "integer overflow"),
            FaultKind::DivByZero => write!(f, "division by zero"),
            FaultKind::IndexOutOfBounds => write!(f, "index out of bounds"),
            FaultKind::ParseFailed => write!(f, "input parse mismatch"),
            FaultKind::EmptyCollection => write!(f, "empty collection"),
            FaultKind::StackOverflow => write!(f, "stack overflow (recursion limit)"),
            FaultKind::FloatToInt => write!(f, "float-to-int conversion out of range"),
            FaultKind::InvalidChar => write!(f, "not a Unicode scalar value"),
            FaultKind::InvalidText => write!(f, "invalid UTF-8 in Text"),
            FaultKind::InvalidSize => write!(f, "size or extent out of range"),
            FaultKind::TypeMismatch => write!(f, "value does not have the declared type"),
            FaultKind::Panic => write!(f, "panic"),
            FaultKind::AssertFailed => write!(f, "assertion failed"),
            FaultKind::EmptyRange => write!(f, "empty range"),
            FaultKind::NoAnswer => write!(f, "an argument this algorithm has no answer for"),
        }
    }
}

/// The fault record a [`RuntimeContext`] points at. `pending_fault` is non-null
/// and points at the owning runtime's slot.
///
/// **The kind is the whole state.** There is no `pending: bool` mirroring
/// `kind != None` beside it: one field cannot contradict itself, and generated
/// code branches on the kind word directly (ADR-102).
#[repr(C)]
pub struct Fault {
    /// The pending fault, or [`FaultKind::None`] for no fault. Private: the
    /// only way to raise one is [`Fault::set`], which takes a [`RaisedFault`].
    kind: FaultKind,
}

impl Fault {
    /// Where the kind sits within the record, and how wide it is.
    ///
    /// **Generated code reads the kind directly (ADR-102).** An
    /// `Inst::CheckFault` is a load of `ctx.pending_fault`, a load of the kind
    /// at this offset, and a `brif` — which works only because
    /// [`FaultKind::None`] is 0 and every raisable kind is not, so the loaded
    /// word *is* [`Fault::is_pending`].
    ///
    /// So a repr change to `Fault` or to [`FaultKind`] is a generated-code
    /// change and owes a
    /// [`RUNTIME_ABI_VERSION`](crate::abi::RUNTIME_ABI_VERSION) bump. The
    /// backend asserts `KIND_SIZE` at compile time against the width it loads,
    /// so a `#[repr(u8)]` or a `#[repr(C)]` that grows the enum fails the build
    /// rather than reading three bytes of something else.
    ///
    /// Both are minted here rather than reached for with `offset_of!` from the
    /// backend because `kind` is private — the field is private so that
    /// [`Fault::set`] is the only way to raise, which is what makes
    /// "raise no fault" unspellable.
    pub const KIND_OFFSET: usize = core::mem::offset_of!(Fault, kind);

    /// The width of the kind, in bytes. See [`Fault::KIND_OFFSET`].
    pub const KIND_SIZE: usize = core::mem::size_of::<FaultKind>();

    /// A fresh, clear fault record (no fault pending).
    pub fn clear() -> Self {
        Fault {
            kind: FaultKind::None,
        }
    }

    /// Raise `fault`.
    ///
    /// Takes a [`RaisedFault`] rather than a `FaultKind` so that "raise no
    /// fault" has no spelling.
    pub fn set(&mut self, fault: RaisedFault) {
        self.kind = fault.kind();
    }

    /// The pending fault kind, or [`FaultKind::None`].
    #[inline]
    #[must_use]
    pub fn kind(&self) -> FaultKind {
        self.kind
    }

    /// True iff a fault is pending.
    pub fn is_pending(&self) -> bool {
        self.kind != FaultKind::None
    }
}

impl Default for Fault {
    fn default() -> Self {
        Self::clear()
    }
}

/// One local variable in a debug frame snapshot (§9.3).
///
/// Carries the source name, the compiler-assigned `symbol_id` (which
/// disambiguates shadowed bindings — two `var a` in the same scope get distinct
/// ids, §4.2), the local's type descriptor, and the current `GcRef` value. The
/// prologue and epilogue push and pop the frames, the spill updates the values,
/// and the crash debugger reads them to display locals.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DebugLocal {
    /// The source name as written (e.g. `a`). Not owned by the frame; points at
    /// a `'static` string the compiler embedded.
    pub source_name: *const u8,
    /// The name's byte length.
    pub name_len: u32,
    /// The compiler-assigned symbol id (disambiguates shadowed bindings, §4.2).
    pub symbol_id: u32,
    /// The local's static type descriptor (§9.3 "local type descriptors"), so
    /// the debugger can render a local without re-deriving its type. Embedded
    /// by the backend at push time from the MIR local's `Type`. Null when the
    /// local has no static type (`MirType::Opaque` — a pipeline accumulator, a
    /// fused-loop item), alongside
    /// [`NO_STATIC_TYPE`](crate::debug::NO_STATIC_TYPE) in `type_id`; and null
    /// on its own when the type has no runtime descriptor (`Never`, an
    /// unresolved inference variable), where `type_id` is still a real handle.
    /// Either way the debugger omits the type column.
    pub descriptor: *const crate::TypeDescriptor,
    /// The current value of the local, or `None` for a slot no value has been
    /// written into yet.
    ///
    /// Decoded from the slot's one machine word by
    /// [`DebugLocalMeta::read`](crate::debug::DebugLocalMeta::read), under the
    /// [`DebugSlotKind`](crate::debug::DebugSlotKind) the compiler recorded for
    /// this local — so a temp whose box ADR-120 elided is a
    /// [`DebugValue::Scalar`](crate::debug::DebugValue::Scalar) carrying its
    /// payload and *no* reference, and everything else is a
    /// [`DebugValue::Reference`](crate::debug::DebugValue::Reference). A
    /// consumer that means to follow the value into the heap says so with
    /// [`DebugValue::reference`](crate::debug::DebugValue::reference), which is
    /// the one door a scalar cannot pass.
    ///
    /// The word itself is `Option<GcRef>`-shaped in the slot, and the zeroed
    /// slot a fresh frame starts with is the `None` niche (F18).
    pub value: Option<crate::debug::DebugValue>,
    /// The full static `Type` id (a `praxis_types::Type(u32)` handle), so the
    /// crash debugger can reconstruct the local's *exact* type — including
    /// collection element types (`Vec[Int]`, `Map[Text, Int]`) and record field
    /// shapes — which the runtime `descriptor` alone loses. The debugger pairs
    /// this id with the live `TypeDb` to type-check `p EXPR` against the
    /// selected frame (§9.5). [`NO_STATIC_TYPE`](crate::debug::NO_STATIC_TYPE)
    /// when the local has none; every other `u32` is a valid arena index, so
    /// there is no in-band zero sentinel.
    pub type_id: u32,
    /// The debugger classification: `LOCAL_KIND_USER` (a binding the programmer
    /// wrote) or `LOCAL_KIND_TEMP` (a compiler intermediate). See
    /// [`crate::debug::LOCAL_KIND_USER`].
    pub kind: u8,
    /// The local's source span start (byte offset), paired with `span_end`.
    pub span_start: u32,
    /// The local's source span end (byte offset). `(span_start, span_end) ==
    /// (0, 0)` means "no span" (the return slot, span-less captures).
    pub span_end: u32,
    /// The function a direct call defines this local from, copied from
    /// [`DebugLocalMeta::callee_name`](crate::debug::DebugLocalMeta::callee_name).
    /// Null for every local that is not a direct call's result.
    pub callee_name: *const u8,
    /// The callee name's byte length; `0` where there is no name.
    pub callee_name_len: u32,
}

/// The hidden first argument to every generated function.
///
/// Matches the sketch in Appendix B. Fields are raw pointers because generated
/// Cranelift code reads them at a fixed offset with a fixed calling convention;
/// Rust borrows would not survive across the ABI boundary.
#[repr(C)]
pub struct RuntimeContext {
    pub heap: *mut Heap,
    /// The runtime's one fault slot. **Non-null in every context generated code
    /// is ever handed** — [`Runtime::context`] is its only producer and wires
    /// it to the runtime's own `Fault`; the sole null-wiring constructor,
    /// [`RuntimeContext::placeholder`], is `unsafe` and test-only.
    ///
    /// That invariant is load-bearing (ADR-102): an `Inst::CheckFault` is a
    /// load of this pointer and a load of the [`Fault::KIND_OFFSET`] word
    /// behind it, with no null test. A host that hand-built a context with a
    /// null here and called generated code would fault the process rather than
    /// silently never observing a Praxis fault. `a_wired_context_has_a_fault_slot`
    /// is the gate.
    pub pending_fault: *mut Fault,
    /// The header of the runtime's one crash-debugger frame stack (§9.3,
    /// ADR-021, ADR-104). Generated code claims one [`DebugFrameEntry`] in the
    /// prologue and restores the `top` in the epilogue;
    /// [`crate::crash_snapshot::praxis_snapshot_debug_chain`] reads `[base, top)`
    /// innermost-first to build the frames the crash REPL renders.
    ///
    /// §11.6's discipline in this struct is *append at the end, never reorder*:
    /// a field generated code reads that comes to point at something else keeps
    /// its position and bumps
    /// [`RUNTIME_ABI_VERSION`](crate::abi::RUNTIME_ABI_VERSION), because
    /// deleting it and appending a replacement would shift every field below.
    pub debug_frames: *mut DebugFrameStackHeader,
    /// The header of the runtime's one compiler-managed shadow stack (§12.3,
    /// ADR-019, ADR-101). Generated code claims a run of slots in the prologue
    /// by bumping the header's `top`, spills live `GcRef`s into that run at
    /// safepoints, and restores `top` in the epilogue. The collector scans
    /// `[base, top)` via [`RootSet`].
    pub shadow: *mut ShadowStackHeader,
    pub input_source: GcRef,
    /// The cached immortal `Unit` — the "defined dummy" returned on fault paths
    /// (§10.4). Separate from `input_source` (which holds the read-in buffer
    /// when present), so fault sentinels are stable regardless of input.
    pub unit_ref: GcRef,
    pub current_generation: u64,
    /// How much of the native-stack budget the live Praxis frames have *not*
    /// yet spent, in bytes (§9.2, §17.4, ADR-105). A prologue subtracts its own
    /// [`frame_cost`]; its epilogue stores back the value it found. A prologue
    /// guard faults with [`FaultKind::StackOverflow`] when what is left will not
    /// cover this frame, so deep recursion faults cleanly instead of overflowing
    /// the native stack and aborting the host (SIGABRT). Read by generated
    /// Cranelift code at a fixed offset, like the other `#[repr(C)]` fields
    /// above.
    ///
    /// **It counts down, and the direction is the design.** Counting up needs
    /// the limit in generated code, which fixes it at compile time for every
    /// host. Counting down puts the limit in this field, so [`Runtime::context`]
    /// — the one producer every caller of generated code goes through — is the
    /// single place a stack size enters the system, and the backend never learns
    /// it. It also makes zero mean *exhausted*, which is the right thing for
    /// [`RuntimeContext::placeholder`] to say.
    pub stack_left: u32,
    /// Host-managed pointer to the runtime's [`crate::ParseDetail`] slot
    /// (§7.11). The parser interpreter writes the richest parse mismatch into
    /// it on `ParseFailed`; the host (CLI / crash debugger) reads it after the
    /// fault. Generated code never touches this field — it is appended at the
    /// end of `RuntimeContext` so the offsets of all generated-code-read fields
    /// above are unchanged (§11.6 ABI stability).
    pub parse_detail: *mut crate::ParseDetail,
    /// Host-managed pointer to the runtime's [`crate::SnapshotSlot`] (§9.3).
    /// The first fault epilogue deep-copies the debug-frame chain into it
    /// before unwinding; the host reads the snapshot after the fault. Like
    /// `parse_detail`, generated code only passes it to
    /// `praxis_snapshot_debug_chain` — it is appended at the end of
    /// `RuntimeContext` for ABI stability.
    pub crash_snapshot: *mut crate::SnapshotSlot,
    /// The runtime's one native root store (ADR-114): what the runtime's own
    /// Rust code holds live across an allocation, in one contiguous array.
    ///
    /// Claimed and released by [`crate::roots::NativeScope`], never by generated
    /// code — which is why it, like `parse_detail` and `crash_snapshot`, is
    /// appended at the end of the struct. It is the fifth arm of
    /// [`crate::roots::RuntimeRoots`], which scans `[0, len)`.
    ///
    /// It has no reader outside `praxis-runtime` at all, so changing what it
    /// points at is not the ABI-version event it would be for `shadow` or
    /// `debug_frames`, which every generated prologue bump-allocates from. See
    /// ADR-114.
    pub native_roots: *mut crate::roots::NativeRootStore,
    /// The cached immortal `true`, alongside [`Self::unit_ref`] (§4.3). There
    /// are exactly two `Bool` values; the runtime allocates them once, so no
    /// comparison in a loop consumes arena storage.
    pub true_ref: GcRef,
    /// The cached immortal `false`. See [`Self::true_ref`].
    pub false_ref: GcRef,
    /// Host-managed pointer to the runtime's [`FaultMessage`] slot (§9.1).
    /// `praxis_panic` and `praxis_assert` write the message the program gave;
    /// the host reads it when it renders the fault. Like `parse_detail` and
    /// `crash_snapshot`, generated code never touches it — it is appended at
    /// the end of the struct so every generated-code-read offset above is
    /// unchanged (§11.6 ABI stability).
    pub fault_message: *mut FaultMessage,
    /// The base of the interned small-`Int` table (`Immortals::small_ints`),
    /// alongside [`Self::true_ref`] and [`Self::unit_ref`] (§4.3,
    /// [`crate::small_int`]).
    ///
    /// Unlike those, this is a *pointer to* the objects rather than one of
    /// them: there are [`crate::SMALL_INT_COUNT`] of them, so generated code
    /// takes two loads — the base from here, then the element at a byte offset
    /// it computed at compile time from the literal's value. That is what
    /// `Inst::ConstGc` emits, and it is why an in-range `Int` literal in a loop
    /// body is not a call, an allocation and a shadow-frame spill per iteration
    /// (docs/handovers/21-where-the-time-goes.md §3.5).
    ///
    /// Generated code *does* read this one, so it would be a compatibility
    /// break if it moved — but it is appended like `fault_message` and its
    /// neighbours, so every offset above is unchanged.
    pub small_ints: *const GcRef,
    /// The header of the runtime's one crash-debugger value stack (§9.3,
    /// ADR-104). Generated code claims one slot per `Gc` local in the prologue,
    /// stores each local's value there at the instruction that defines it, and
    /// restores the `top` in the epilogue. Each [`DebugFrameEntry`] in
    /// `debug_frames` names the base of its own call's run.
    ///
    /// **The collector never *traces* this** — it is the weak arm of
    /// [`crate::roots::RuntimeRoots`] (ADR-106), not a strong one. The slot type
    /// is `Option<GcRef>` rather than the shadow stack's `*mut GcHeader`
    /// precisely so that `impl RootSet for SlotStackHeader<*mut GcHeader>`
    /// cannot reach it: the debug set is over-approximate and never cleared
    /// (MIR-16), and rooting it would undo MIR-01's clears.
    ///
    /// It *is* scanned, once per collection, immediately after the sweep: every
    /// slot naming storage that sweep just reclaimed becomes `None`. That is
    /// what makes a debug value always a live object or an absence, and never a
    /// reference to a block the allocator has since reissued as something else
    /// — which would render as a well-formed value of another type under the
    /// dead local's own name, sharper than a dangling read.
    ///
    /// Appended after `small_ints`, so every offset above is unchanged.
    pub debug_values: *mut DebugValueStackHeader,
    /// The base of the interned ASCII-`Char` table (`Immortals::small_chars`),
    /// alongside `small_ints` (§4.3, [`crate::small_char`], ADR-107).
    ///
    /// **Generated code never reads this one**, which is what separates it from
    /// `small_ints`: the language has no character literal, so there is no
    /// `GcConst::Char` and nothing lowers to a load of this base. Its readers are
    /// the runtime's own — `abi.rs`'s `char_ref` and the parser interpreter's
    /// `Rt::alloc_char`, both of which reach the runtime only through a
    /// `*mut RuntimeContext`. It is therefore the `native_roots`/`fault_message`
    /// class of field, and it is appended at the end of the struct for their
    /// reason: every generated-code-read offset above stays where it was
    /// (§11.6 ABI stability).
    pub small_chars: *const GcRef,
    /// Every built-in descriptor's address, indexed by [`BuiltinTypeId`]
    /// (ADR-116). ADR-102's inline type proof loads one slot of this and
    /// compares the object header's descriptor word against it.
    ///
    /// **By value, and that is the decision.** A pointer to
    /// [`crate::descriptor::BUILTINS`] would make the proof two *dependent*
    /// loads; the array makes it one load at a displacement the backend folds
    /// from [`RuntimeContext::descriptor_offset`]. Baking the address in as an
    /// `iconst` instead would be no load at all, but on aarch64 it costs
    /// `movz`+`movk`+`movk` (a `static` in this binary lives above 2³²) and it
    /// would make the compiler name a descriptor *address*, which it otherwise
    /// never does (docs/handovers/25-two-mallocs-per-runtime-call.md §3 F-4).
    ///
    /// Filled by [`crate::descriptor::builtin_descriptor_addresses`], which
    /// derives it from `BUILTINS` — so "slot `i` holds the descriptor whose id
    /// is `i`" has no second place it could be written wrong, and
    /// `builtins_are_indexed_by_their_id` stays the one gate on it.
    ///
    /// Appended after `small_chars`, so every offset above is unchanged
    /// (§11.6 ABI stability). Generated code *does* read this one.
    pub descriptors: [*const TypeDescriptor; BuiltinTypeId::COUNT],
}

/// The table is appended, so every field generated code reads sits where it
/// did. Pinned rather than described: the proof site's displacement is only a
/// compile-time immediate because the array starts right after `small_chars`.
const _: () = assert!(
    RuntimeContext::descriptor_offset(BuiltinTypeId::Unit)
        == core::mem::offset_of!(RuntimeContext, small_chars)
            + core::mem::size_of::<*const GcRef>(),
    "the descriptor table is appended after `small_chars`, not spliced in"
);

/// A slot is one pointer wide and the table is exactly the built-ins, so the
/// last slot is in bounds and there is no room for a twenty-third.
const _: () = assert!(
    RuntimeContext::descriptor_offset(BuiltinTypeId::Range)
        + core::mem::size_of::<*const TypeDescriptor>()
        == core::mem::size_of::<RuntimeContext>(),
    "`Range` is the last built-in and its slot is the last word of the context"
);

impl RuntimeContext {
    /// The byte displacement of `id`'s slot in [`Self::descriptors`], from the
    /// base of a `RuntimeContext`.
    ///
    /// **The one authority for the address ADR-102's proof compares against**,
    /// and the reason the backend holds no descriptor address at all: it folds
    /// this displacement, loads whatever the runtime put there, and compares.
    /// *Which* descriptor that is is the runtime's answer rather than a pointer
    /// the compiler carried across the ABI — so the two cannot disagree about
    /// the address `Int`'s descriptor has, only about which slot it is in, and
    /// that is the enum discriminant.
    ///
    /// Minted here rather than reached for with `offset_of!` from the backend
    /// for [`Fault::KIND_OFFSET`]'s reason one step further on: the element
    /// stride is part of the answer, and a backend that multiplied by its own
    /// `size_of::<*const _>()` would be a second statement of this layout.
    #[must_use]
    pub const fn descriptor_offset(id: BuiltinTypeId) -> usize {
        core::mem::offset_of!(RuntimeContext, descriptors)
            + (id as usize) * core::mem::size_of::<*const TypeDescriptor>()
    }

    /// Construct a context with all pointers null and the input source set to
    /// the canonical placeholder. Real runtime setup (rooting the heap,
    /// installing a fault sink) is done via [`Runtime::context`].
    ///
    /// **Generated code must never be run against a placeholder.** The prologue
    /// is inline (ADR-101): it dereferences `shadow` unconditionally and without
    /// a null check, because the check cost every call in the language and
    /// `Runtime::context` is the only producer of a context generated code is
    /// ever handed.
    ///
    /// # Safety
    /// `input_source` must be a valid `GcRef` (or the caller must ensure no
    /// generated code dereferences it before the runtime is fully initialized).
    pub unsafe fn placeholder(input_source: GcRef) -> RuntimeContext {
        RuntimeContext {
            heap: std::ptr::null_mut(),
            pending_fault: std::ptr::null_mut(),
            debug_frames: std::ptr::null_mut(),
            shadow: std::ptr::null_mut(),
            input_source,
            // Placeholder: reuse the input_source ref as the Unit sentinel too,
            // since this constructor is only for not-yet-wired test scaffolding.
            unit_ref: input_source,
            current_generation: 0,
            // Zero is *exhausted*, not "fresh" — so if generated code ever did
            // reach a placeholder in spite of the paragraph above, its first
            // prologue faults `StackOverflow` rather than running with a full
            // budget over a stack nobody sized. Counting down is what makes the
            // safe default and the zero value the same number (ADR-105).
            stack_left: 0,
            parse_detail: std::ptr::null_mut(),
            crash_snapshot: std::ptr::null_mut(),
            native_roots: std::ptr::null_mut(),
            // As for `unit_ref`: this constructor is not-yet-wired scaffolding.
            true_ref: input_source,
            false_ref: input_source,
            fault_message: std::ptr::null_mut(),
            // Null, not a dangling table: a placeholder context is scaffolding
            // no generated code runs against, and a null here faults loudly at
            // the first `Inst::ConstGc` rather than reading whatever the
            // `input_source` trick would have aliased.
            small_ints: std::ptr::null(),
            debug_values: std::ptr::null_mut(),
            // Null for `small_ints`' reason: a null faults loudly at the first
            // read rather than aliasing whatever the `input_source` trick above
            // would have pointed at. The exposure is the parser interpreter's
            // `Rt::alloc_char`, which is the standing one `Rt::alloc_int`
            // already has — a parse is never run against a placeholder.
            small_chars: std::ptr::null(),
            // Real addresses, where every neighbour above is null — and the
            // difference is that there is nothing here for a runtime to wire.
            // These are `static`s of this binary; they are valid before `main`
            // and depend on no `Runtime`. A null table would be a trap for a
            // state that cannot exist, and it would make the *only* difference
            // between a placeholder and a wired context at ADR-102's proof site
            // a null dereference rather than an honest comparison (ADR-116).
            descriptors: builtin_descriptor_addresses(),
        }
    }

    /// True iff a fault is currently pending on this context. Generated code
    /// checks this at safepoints after potentially-faulting operations (§10.4).
    ///
    /// `pending_fault` is non-null once the context is wired to a runtime; a
    /// fault is pending when the pointed-at [`Fault`] slot says so.
    #[inline]
    pub fn has_pending_fault(&self) -> bool {
        if self.pending_fault.is_null() {
            return false;
        }
        // SAFETY: a non-null `pending_fault` points at a live `Fault` owned by
        // the runtime for as long as the context is in use.
        unsafe { (*self.pending_fault).is_pending() }
    }
}

/// Read the current fault kind from a context's `pending_fault` slot (§9.2).
/// Returns [`FaultKind::None`] if no fault is pending or the slot is null. Used
/// by [`crate::crash_snapshot::praxis_snapshot_debug_chain`] to record which
/// fault kind triggered the snapshot.
///
/// # Safety
/// `ctx` must be live and wired (a null `pending_fault` yields `None`).
pub unsafe fn current_fault_kind(ctx: *mut RuntimeContext) -> FaultKind {
    if ctx.is_null() || unsafe { (*ctx).pending_fault.is_null() } {
        return FaultKind::None;
    }
    // SAFETY: caller guarantees the context is live; a non-null pending_fault
    // points at a live Fault owned by the runtime.
    unsafe { (*(*ctx).pending_fault).kind }
}

/// The owner of the heap and the immortal singletons.
///
/// The entry point for runtime code: construct a `Runtime`, allocate values
/// through it, root them in a [`crate::RootScope`], and collect when needed.
/// [`Runtime::context`] produces the `RuntimeContext` handed to generated code.
pub struct Runtime {
    heap: Heap,
    immortals: Immortals,
    /// The fault slot generated code signals through (§10.4). Owned here so its
    /// address is stable for the lifetime of the runtime.
    fault: Fault,
    /// The rich parse-failure detail slot (§7.11). Owned here so its address is
    /// stable; `Runtime::context` installs it on every context. The parser
    /// interpreter writes the deepest mismatch into it; the host reads it after
    /// a `FaultKind::ParseFailed`.
    parse_detail: ParseDetail,
    /// The crash-snapshot slot (§9.3). Owned here so its address is stable; the
    /// first fault epilogue deep-copies the debug-frame chain into it before
    /// unwinding. The host reads it (and roots it for GC) after a fault.
    crash_snapshot: SnapshotSlot,
    /// The message slot a `panic`/`assert` fault carries (§9.1). Owned here so
    /// its address is stable; `Runtime::context` installs it on every context.
    fault_message: FaultMessage,
    /// The one shadow stack every generated frame bump-allocates from
    /// (ADR-101). Owned here, sized once, never resized: generated code holds
    /// the header's address for the whole program and a frame's base pointer
    /// for the duration of a call, so a reallocation would be a use-after-free.
    shadow_stack: ShadowStack,
    /// The one native root store every [`crate::roots::NativeScope`] claims from
    /// (ADR-114). Owned here so its address is stable, like `fault` and
    /// `parse_detail`, and for the same reason: `Runtime::context` hands out a
    /// raw pointer to it.
    ///
    /// Unlike `shadow_stack` this one **grows**, and it can, because the only
    /// address anything holds is the store's own — never the array's. A scope
    /// saves a `usize` watermark; the collector re-reads the slice at every
    /// collection. ADR-114 prices the asymmetry: how deep the scopes nest is
    /// bounded, how many roots one of them holds is the program's input.
    native_roots: crate::roots::NativeRootStore,
    /// The crash debugger's two stacks (§9.3, ADR-104), owned and sized here
    /// for the same reason and under the same never-resize rule as
    /// `shadow_stack`. `debug_frames` holds one entry per live call — which
    /// function, and where its values are — and `debug_values` one slot per `Gc`
    /// local per live call.
    debug_frames: DebugFrameStack,
    debug_values: DebugValueStack,
    /// The native-stack budget every context this runtime mints starts with
    /// (ADR-105).
    ///
    /// Owned here rather than baked into generated code because the budget is a
    /// property of the *stack the program runs on*, which the backend cannot
    /// know and the host sometimes can. [`Runtime::context`] is the only reader,
    /// which makes it the one door a stack size enters through.
    stack_budget: StackBudget,
}

impl Runtime {
    /// Create a runtime with a fresh heap and the immortal singletons allocated.
    pub fn new() -> Self {
        let heap = Heap::new();
        // Immortals must be allocated before any collection can run.
        let immortals = Immortals::new(&heap);
        Runtime {
            heap,
            immortals,
            fault: Fault::clear(),
            parse_detail: ParseDetail::new(),
            crash_snapshot: SnapshotSlot::new(),
            fault_message: FaultMessage::new(),
            // 3.42 MiB of address space, allocated zeroed — one `mmap` of
            // untouched pages, faulted in only as deep as the program actually
            // recurses. See `SHADOW_STACK_SLOTS` for why it can be sized once
            // and never checked.
            shadow_stack: ShadowStack::new(SHADOW_STACK_SLOTS, std::ptr::null_mut()),
            // 8 KiB of reservation, one `malloc`, and a growable one — the
            // asymmetry ADR-114 records: how deep the native scopes nest is
            // bounded, how many roots one of them holds is not.
            native_roots: crate::roots::NativeRootStore::new(),
            debug_frames: DebugFrameStack::new(DEBUG_FRAME_STACK_SLOTS, DebugFrameEntry::empty()),
            debug_values: DebugValueStack::new(DEBUG_VALUE_STACK_SLOTS, None),
            stack_budget: StackBudget::DEFAULT,
        }
    }

    /// Lower the native-stack budget every context this runtime mints will start
    /// with (ADR-105).
    ///
    /// For a host that knows its stack is smaller than the one
    /// [`STACK_BUDGET_BYTES`] assumes, and for tests that want to reach the
    /// guard without recursing eight thousand times. It cannot be *raised* past
    /// the default — [`StackBudget::new`] refuses, because the shadow-stack
    /// reservation is sized from that figure.
    pub fn set_stack_budget(&mut self, budget: StackBudget) {
        self.stack_budget = budget;
    }

    /// The native-stack budget this runtime hands to a new context.
    #[must_use]
    pub fn stack_budget(&self) -> StackBudget {
        self.stack_budget
    }

    /// Borrow the heap.
    #[inline]
    pub fn heap(&self) -> &Heap {
        &self.heap
    }

    /// The immortal singletons (§4.3).
    #[inline]
    pub fn immortals(&self) -> &Immortals {
        &self.immortals
    }

    /// Force a mark-and-sweep collection (§12.1) rooted from everything this
    /// runtime owns — the shadow stack, the ambient input buffer, a parse
    /// failure's partial value, the crash snapshot, and the native root store.
    ///
    /// This is the host's collection entry point. It takes no root set: a host
    /// that could name its own would be choosing which of the runtime's owners
    /// to honour, and choosing wrong frees a live object.
    pub fn collect_now(&mut self) {
        let mut ctx = self.context();
        // SAFETY: `ctx` is a fresh view of this live runtime, and the arms it
        // points at (parse detail, snapshot slot) are owned by `self`, which
        // outlives the borrow.
        let roots = unsafe { crate::roots::RuntimeRoots::from_context(&mut ctx) };
        self.heap.collect(&roots);
    }

    /// Run a mark-and-sweep collection (§12.1) against an arbitrary root set.
    ///
    /// Test-only. Production collection goes through
    /// [`Heap::collect`](crate::Heap::collect), which accepts only a
    /// [`RuntimeRoots`](crate::roots::RuntimeRoots) read out of a live context
    /// — a host that could pass its own `&dyn RootSet` could collect against a
    /// set that omits the runtime's own owners, and so free a live object.
    #[cfg(test)]
    pub fn collect_with(&self, roots: &dyn RootSet) {
        self.heap.collect_with(roots);
    }

    /// A `RuntimeContext` view of this runtime, suitable for generated code.
    /// `pending_fault` points at this runtime's fault slot; `shadow` points at
    /// this runtime's shadow-stack header, which every generated prologue
    /// bump-allocates from and which the collector scans; `debug_frames` and
    /// `debug_values` point at the crash debugger's two stacks, which the
    /// prologue claims from and which `praxis_snapshot_debug_chain` reads.
    /// `parse_detail` points at this runtime's [`ParseDetail`] slot so the
    /// parser interpreter can record the richest `ParseFailed` detail.
    ///
    /// Every context this mints shares the three stacks **and the native root
    /// store**, so a context taken while generated code or a runtime wrapper is
    /// running (as [`Runtime::collect_now`] does) sees the frames and scopes
    /// already on them.
    ///
    /// **Two contexts must never execute over these stacks concurrently.** That
    /// holds because a Praxis program is single-threaded and every host that
    /// mints a second context ([`crate::Runtime::collect_now`], the debugger's
    /// `p EXPR` and `restart`) does so only when the previous run has fully
    /// unwound. A second context therefore starts with the *full* stack
    /// budget rather than the running one's remainder, which is correct for the
    /// two callers that mint one while frames are live: both do so from the host,
    /// on the host's own stack, not from underneath the frames.
    pub fn context(&mut self) -> RuntimeContext {
        RuntimeContext {
            heap: &mut self.heap as *mut Heap,
            pending_fault: &mut self.fault as *mut Fault,
            debug_frames: self.debug_frames.header_ptr(),
            shadow: self.shadow_stack.header_ptr(),
            input_source: self.immortals.unit(),
            unit_ref: self.immortals.unit(),
            current_generation: 0,
            // The one door a native-stack size enters the system through
            // (ADR-105). Generated code never learns the budget; it only ever
            // subtracts from what it finds here.
            stack_left: self.stack_budget.get(),
            parse_detail: &mut self.parse_detail as *mut ParseDetail,
            crash_snapshot: &mut self.crash_snapshot as *mut SnapshotSlot,
            // The one store, shared by every context this runtime mints — so a
            // context taken while native code is running (as `collect_now` does)
            // sees the scopes already open on it.
            native_roots: &mut self.native_roots as *mut crate::roots::NativeRootStore,
            true_ref: self.immortals.true_(),
            false_ref: self.immortals.false_(),
            fault_message: &mut self.fault_message as *mut FaultMessage,
            small_ints: self.immortals.small_ints_ptr(),
            debug_values: self.debug_values.header_ptr(),
            small_chars: self.immortals.small_chars_ptr(),
            // Not `self`'s: the built-in descriptors are `static`s shared by
            // every runtime in the process, so this is the same table in every
            // context and copying it costs one 176-byte block move per program
            // run (ADR-116).
            descriptors: builtin_descriptor_addresses(),
        }
    }

    /// The current fault state (§10.4). `FaultKind::None` when no fault is set.
    pub fn fault(&self) -> FaultKind {
        self.fault.kind()
    }

    /// True iff a fault is pending.
    pub fn has_pending_fault(&self) -> bool {
        self.fault.is_pending()
    }

    /// Clear any pending fault, returning the kind that was pending (if any).
    pub fn take_fault(&mut self) -> Option<FaultKind> {
        let kind = self.fault.kind();
        if self.fault.is_pending() {
            self.fault = Fault::clear();
            self.fault_message.clear();
            Some(kind)
        } else {
            None
        }
    }

    /// The message a `panic`/`assert` fault carried (§9.1), or `None` for a
    /// fault kind that carries none. The host renders it beside the fault line.
    #[must_use]
    pub fn fault_message(&self) -> Option<&str> {
        self.fault_message.get()
    }

    /// Borrow the rich parse-failure detail slot (§7.11). The host reads this
    /// after a `FaultKind::ParseFailed` to render the input/parser span, the
    /// expected description, and the actual preview. Returns `None` when no
    /// detail was recorded (e.g. a non-parser `ParseFailed` path).
    #[must_use]
    pub fn parse_detail(&self) -> &ParseDetail {
        &self.parse_detail
    }

    /// Mutably borrow the parse-detail slot (so the host can clear it before a
    /// rerun, or the debugger can read the partial root value).
    pub fn parse_detail_mut(&mut self) -> &mut ParseDetail {
        &mut self.parse_detail
    }

    /// Borrow the crash-snapshot slot (§9.3). `None` when no fault snapshotted
    /// this run (the program completed cleanly, or faulted before any debug
    /// frame was pushed). The host reads this after a fault for the
    /// noninteractive render / crash REPL.
    #[must_use]
    pub fn crash_snapshot(&self) -> Option<&CrashSnapshot> {
        self.crash_snapshot.get()
    }

    /// Take the crash snapshot out of the runtime (the host owns it after).
    /// Returns `None` when no snapshot was taken.
    pub fn take_crash_snapshot(&mut self) -> Option<CrashSnapshot> {
        self.crash_snapshot.take()
    }

    /// Reset the fault, crash-snapshot, and parse-detail slots so the next
    /// `main` call starts from a clean slate (§9.7 `restart`/`reload`). The
    /// heap is *not* collected — old allocations (and the snapshot the host
    /// may still hold as a root set) survive until an explicit `collect`.
    /// Call this before re-executing `main`.
    pub fn clear_for_rerun(&mut self) {
        self.fault = Fault::clear();
        self.crash_snapshot.clear();
        self.parse_detail.clear();
        self.fault_message.clear();
        // Every epilogue — including every fault epilogue — restores the `top`
        // its prologue saved, so a completed run leaves the stacks exactly as it
        // found them. A non-empty stack here is an unbalanced prologue, which is
        // a codegen bug and not something a rerun should paper over silently.
        debug_assert!(
            self.shadow_stack.is_empty(),
            "the shadow stack is {} slots deep between runs; some prologue was \
             not balanced by an epilogue",
            self.shadow_stack.len()
        );
        debug_assert!(
            self.debug_frames.is_empty() && self.debug_values.is_empty(),
            "the debug stacks are {} frames / {} values deep between runs; some \
             prologue was not balanced by an epilogue",
            self.debug_frames.len(),
            self.debug_values.len()
        );
        // The same statement for the fourth region, and a sharper one: a
        // `NativeScope` is RAII on the *Rust* stack, so between runs there is no
        // frame that could still be holding a claim. A non-empty store is a
        // scope that was leaked or `mem::forget`ten, and every root in it is one
        // the next run's collections would keep alive forever.
        debug_assert!(
            self.native_roots.is_empty(),
            "the native root store holds {} roots between runs; some \
             `NativeScope` was not dropped",
            self.native_roots.len()
        );
        self.shadow_stack.reset();
        // Length only. The capacity is deliberately kept: a `restart` re-parses
        // the same input, so a store that grew to hold one root per line wants
        // to be exactly that big again, and shrinking here would put the whole
        // doubling schedule back on the next run's parse.
        self.native_roots.reset();
        self.debug_frames.reset();
        self.debug_values.reset();
    }

    /// The shadow stack every generated frame bump-allocates from (ADR-101).
    ///
    /// Read-only, and the reason it is exposed at all is that "the stack is
    /// empty again" is the observable form of "every prologue was balanced by
    /// an epilogue" — an unbalanced prologue must be a test failure, not a slow
    /// leak that only shows up as a wrong root set thousands of calls later.
    #[must_use]
    pub fn shadow_stack(&self) -> &ShadowStack {
        &self.shadow_stack
    }

    /// The native root store every [`crate::roots::NativeScope`] claims from
    /// (ADR-114). Read-only, and exposed for [`Runtime::shadow_stack`]'s reason
    /// — "the store is empty again" is the observable form of "every scope was
    /// dropped" — plus one this region has and the others do not: its
    /// [`capacity`](crate::roots::NativeRootStore::capacity) is the observable
    /// form of "this program made the store grow", which is the state a
    /// pointer-shaped watermark would not have survived.
    #[must_use]
    pub fn native_root_store(&self) -> &crate::roots::NativeRootStore {
        &self.native_roots
    }

    /// The crash debugger's frame stack (§9.3, ADR-104). Read-only, and exposed
    /// for the same reason as [`Runtime::shadow_stack`]: "the stack is empty
    /// again" is the observable form of "every prologue was balanced".
    #[must_use]
    pub fn debug_frame_stack(&self) -> &DebugFrameStack {
        &self.debug_frames
    }

    /// The crash debugger's value stack (§9.3, ADR-104). See
    /// [`Runtime::debug_frame_stack`].
    #[must_use]
    pub fn debug_value_stack(&self) -> &DebugValueStack {
        &self.debug_values
    }

    /// Consume the runtime, drop the heap, and return the proof that no live
    /// object can still name a JIT generation's arena (F13, hazard H15).
    ///
    /// This is the *only* constructor of [`HeapDrained`], and reclaiming a
    /// generation requires one. Dropping the heap runs every finalizer
    /// (`Heap::drop`), so after this call no `RecordPayload` or `TuplePayload`
    /// survives to dereference a schema pointer.
    ///
    /// A host that never calls this loses nothing but memory: an un-retired
    /// generation leaks its arena.
    #[must_use]
    pub fn teardown(self) -> crate::teardown::HeapDrained {
        drop(self);
        crate::teardown::HeapDrained::new()
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

// ---- typed allocation helpers --------------------------------------------

impl Runtime {
    /// Allocate an `Int` (§4.3), or answer the interned immortal when `value` is
    /// small ([`crate::small_int`]).
    ///
    /// The interning is here and not only in `praxis_alloc_int` so that the host
    /// helper and the ABI wrapper answer the *same object* for the same small
    /// value, exactly as [`Runtime::alloc_bool`] already does. Two allocators
    /// disagreeing about whether `5` is shared would be a wart with no upside:
    /// nothing can observe the sharing (that is `small_int`'s argument), so the
    /// only thing a split would buy is two behaviours to remember.
    pub fn alloc_int(&self, value: i64) -> GcRef {
        match self.immortals.small_int(value) {
            Some(interned) => interned,
            None => self.heap.alloc_unpaced(crate::scalars::INT_PAYLOAD, value),
        }
    }

    /// Allocate a `Bool` as the corresponding immortal singleton (§4.3). Booleans
    /// are always the immortals — there is never a fresh `Bool` allocation.
    pub fn alloc_bool(&self, value: bool) -> GcRef {
        self.immortals.bool_(value)
    }

    /// Allocate a `Byte` (§4.3).
    pub fn alloc_byte(&self, value: u8) -> GcRef {
        self.heap.alloc_unpaced(crate::scalars::BYTE_PAYLOAD, value)
    }

    /// Allocate a `Char` (§4.3), or answer the interned immortal when `value` is
    /// ASCII ([`crate::small_char`]). Panics if `value` is not a valid scalar
    /// value.
    ///
    /// The validity assert stays in front of the table lookup rather than being
    /// absorbed into it: `index_of` answers "is it interned", which for a value
    /// above the range is `None` and therefore says nothing at all about
    /// validity. An out-of-range invalid code point must still panic here.
    ///
    /// The interning is here and not only in `praxis_alloc_char` for
    /// [`Runtime::alloc_int`]'s reason — the host helper and the ABI wrapper must
    /// answer the *same object* for the same small value. Nothing can observe the
    /// sharing (that is `small_char`'s argument), so a split would buy nothing
    /// but two behaviours to remember.
    pub fn alloc_char(&self, value: u32) -> GcRef {
        assert!(
            crate::scalars::is_valid_char(value),
            "{value:#x} is not a valid Unicode scalar"
        );
        match self.immortals.small_char(value) {
            Some(interned) => interned,
            None => self.heap.alloc_unpaced(crate::scalars::CHAR_PAYLOAD, value),
        }
    }

    /// Allocate a `Float` (§4.3, §4.12). All finite values, ±infinity, and NaN
    /// are valid payloads — `Float` arithmetic never faults (IEEE-754).
    pub fn alloc_float(&self, value: f64) -> GcRef {
        self.heap
            .alloc_unpaced(crate::scalars::FLOAT_PAYLOAD, value)
    }

    /// The immortal `Unit` (§4.3).
    pub fn alloc_unit(&self) -> GcRef {
        self.immortals.unit()
    }

    /// Allocate an owned `Text` (§4.3, ADR-013).
    pub fn alloc_text(&self, value: &str) -> GcRef {
        // SAFETY: TextPayload is TEXT's payload type.
        unsafe {
            self.heap
                .alloc_payload_unpaced(&crate::text::TEXT, crate::text::TextPayload::owned(value))
        }
    }

    /// Allocate a source-slice `Text` — a zero-copy view into `owner`'s bytes
    /// spanning `[start, start+len)` (§7.10, ADR-013). The slice's descriptor
    /// traces `owner`, keeping the backing alive.
    ///
    /// Returns `None` if the range is not a `Text`: past the owner's end, an
    /// overflowing length, or ends that split a multi-byte scalar. The check is
    /// unconditional, not a `debug_assert` — a release build must not slice out
    /// of range.
    ///
    /// # Safety
    /// `owner` must be a live `Text` `GcRef`.
    #[must_use]
    pub unsafe fn alloc_text_slice(&self, owner: GcRef, start: usize, len: usize) -> Option<GcRef> {
        // SAFETY: caller guarantees `owner` is a live Text.
        let slice = unsafe { crate::text::SourceSlice::new(owner, start, len) }?;
        let payload = crate::text::TextPayload::Slice(slice);
        // SAFETY: TextPayload is TEXT's payload type.
        Some(unsafe { self.heap.alloc_payload_unpaced(&crate::text::TEXT, payload) })
    }

    /// Allocate a `Vec[T]` from a slice of already-allocated element refs and the
    /// element descriptor (§11.2, ADR-013).
    pub fn alloc_vec(
        &self,
        element_descriptor: &'static TypeDescriptor,
        items: Vec<GcRef>,
    ) -> GcRef {
        // SAFETY: VecPayload is VEC's payload type.
        unsafe {
            self.heap.alloc_payload_unpaced(
                &crate::collections::VEC,
                VecPayload {
                    element_descriptor,
                    items: items.into(),
                },
            )
        }
    }

    /// Allocate a `Grid[T]` from a flat row-major list of cells, the element
    /// descriptor, and the column count (§7.5). `items.len()` must be a
    /// multiple of `width`.
    pub fn alloc_grid(
        &self,
        element_descriptor: &'static TypeDescriptor,
        items: Vec<GcRef>,
        width: usize,
    ) -> GcRef {
        debug_assert!(
            width == 0 || items.len() % width == 0,
            "grid items ({}) not a multiple of width ({})",
            items.len(),
            width
        );
        // SAFETY: GridPayload is GRID's payload type.
        unsafe {
            self.heap.alloc_payload_unpaced(
                &crate::collections::GRID,
                crate::collections::GridPayload {
                    element_descriptor,
                    items,
                    width,
                },
            )
        }
    }

    /// Allocate a provisional structural `Record` from field values and a static
    /// schema (§7.8). `items.len()` must equal `schema.arity()`.
    pub fn alloc_record(
        &self,
        schema: &'static crate::records::RecordSchema,
        items: Vec<GcRef>,
    ) -> GcRef {
        debug_assert_eq!(
            items.len(),
            schema.arity(),
            "record field count ({}) != schema arity ({})",
            items.len(),
            schema.arity()
        );
        // SAFETY: RecordPayload is RECORD's payload type.
        unsafe {
            self.heap.alloc_payload_unpaced(
                &crate::records::RECORD,
                crate::records::RecordPayload { schema, items },
            )
        }
    }
}

// ---- typed payload access helpers ----------------------------------------

impl GcRef {
    /// Read an `Int` payload (§4.3).
    ///
    /// Panics if this reference's descriptor is not `Int`.
    pub fn as_int(&self) -> i64 {
        assert_eq!(
            self.descriptor().id(),
            crate::scalars::INT.id(),
            "not an Int"
        );
        // SAFETY: descriptor check confirms payload is i64.
        unsafe { *self.payload::<i64>() }
    }

    /// Read a `Bool` payload as a Rust `bool` (§4.3).
    ///
    /// Panics if this reference's descriptor is not `Bool`.
    pub fn as_bool(&self) -> bool {
        assert_eq!(
            self.descriptor().id(),
            crate::scalars::BOOL.id(),
            "not a Bool"
        );
        // SAFETY: descriptor check confirms payload is BoolPayload.
        unsafe { read_bool(*self) }
    }

    /// Read a `Byte` payload (§4.3).
    pub fn as_byte(&self) -> u8 {
        assert_eq!(
            self.descriptor().id(),
            crate::scalars::BYTE.id(),
            "not a Byte"
        );
        // SAFETY: descriptor check confirms payload is u8.
        unsafe { *self.payload::<u8>() }
    }

    /// Read a `Char` payload as a Rust `char` (§4.3).
    pub fn as_char(&self) -> char {
        assert_eq!(
            self.descriptor().id(),
            crate::scalars::CHAR.id(),
            "not a Char"
        );
        let raw = unsafe { *self.payload::<u32>() };
        char::from_u32(raw).expect("Char payload was not a valid scalar; memory corrupted")
    }

    /// Read a `Float` payload as an `f64` (§4.3).
    pub fn as_float(&self) -> f64 {
        assert_eq!(
            self.descriptor().id(),
            crate::scalars::FLOAT.id(),
            "not a Float"
        );
        // SAFETY: descriptor check confirms payload is FloatPayload (f64).
        unsafe { *self.payload::<f64>() }
    }

    /// Read a `Text` payload as a `&str` (§4.3).
    ///
    /// The lifetime is tied to the `GcRef`'s borrow; the text stays valid as long
    /// as the object is reachable. Handles both owned and source-slice payloads
    /// (ADR-013): a slice reads through its owner.
    pub fn as_text(&self) -> &str {
        assert_eq!(self.descriptor().id(), crate::text::TEXT.id(), "not Text");
        // SAFETY: descriptor check confirms payload is a TextPayload; the
        // reference is valid while the object lives (non-moving GC, ADR-011).
        let payload = self.payload::<crate::text::TextPayload>() as *const crate::text::TextPayload;
        unsafe { crate::text::text_str(payload) }
    }

    /// Read a `Vec[T]` payload as a slice of element refs (§11.2).
    pub fn as_vec(&self) -> &[GcRef] {
        assert_eq!(
            self.descriptor().id(),
            crate::collections::VEC.id(),
            "not a Vec"
        );
        // SAFETY: descriptor check confirms payload is VecPayload.
        let p: &VecPayload = unsafe { &*self.payload::<VecPayload>() };
        &p.items
    }

    /// Format this value through its descriptor into `out` (§11.4), in the
    /// program's own rendering — what `out(v)` writes and what `"{v}"` splices.
    pub fn format(&self, out: &mut dyn std::fmt::Write) {
        self.format_styled(&mut crate::FormatSink::display(out));
    }

    /// Format this value into `out` in the **debugger's** rendering
    /// ([`FormatStyle::Debug`](crate::FormatStyle::Debug)): a `Text` is a quoted
    /// literal, at every depth.
    ///
    /// The pair exists because the two callers want opposite things from the
    /// same value. A program printing a string means its characters; a debugger
    /// showing a local means "this is a string, and here is exactly which one" —
    /// and on a locals row the difference between `""` and no output at all is
    /// the difference between a value and a bug report.
    pub fn format_debug(&self, out: &mut dyn std::fmt::Write) {
        self.format_styled(&mut crate::FormatSink::debug(out));
    }

    /// Format this value into an existing sink, keeping its style.
    ///
    /// The shared body of the two above, and the entry point for a caller that
    /// already has a sink — a descriptor callback rendering a part of itself.
    pub fn format_styled(&self, out: &mut crate::FormatSink<'_>) {
        let desc = self.descriptor();
        // SAFETY: `self`'s payload matches its descriptor.
        unsafe { (desc.format)(self.payload::<u8>() as *const u8, out) };
    }

    /// Structural equality through the descriptors (§5.5). Returns `false` if
    /// either side's type is not equatable, or if the descriptors differ.
    pub fn equals(&self, other: &GcRef) -> bool {
        let a = self.descriptor();
        let b = other.descriptor();
        if a.id() != b.id() {
            return false;
        }
        let Some(eq) = a.equals else {
            return false;
        };
        // SAFETY: both payloads match the shared descriptor.
        unsafe {
            eq(
                self.payload::<u8>() as *const u8,
                other.payload::<u8>() as *const u8,
            )
        }
    }
}

#[cfg(test)]
mod tests {

    /// Nothing can reset the heap a runtime's immortals live in: `Runtime`
    /// exposes only `&Heap`, never `&mut Heap`, so no safe call can tear down
    /// the arena and mint a fresh `HeapId` underneath `Runtime.immortals`. This
    /// pins the invariant that would make such an accessor dangerous — every
    /// context's cached `unit_ref` / `true_ref` / `false_ref` is live storage
    /// in this runtime's own heap.
    #[test]
    fn a_runtimes_immortals_belong_to_its_own_live_heap() {
        let mut rt = Runtime::new();
        let ctx = rt.context();
        for cached in [ctx.unit_ref, ctx.true_ref, ctx.false_ref] {
            assert!(
                rt.heap().owns(cached),
                "a cached immortal must be live storage in this runtime's heap"
            );
        }
        assert_eq!(ctx.unit_ref.as_ptr(), rt.immortals().unit().as_ptr());
        assert_eq!(ctx.true_ref.as_ptr(), rt.immortals().true_().as_ptr());
        assert_eq!(ctx.false_ref.as_ptr(), rt.immortals().false_().as_ptr());
    }
    use super::*;
    use crate::gc::GcHeader;
    use crate::roots::RootScope;
    use std::ptr::NonNull;

    /// ADR-102: generated code loads the fault kind rather than calling
    /// `praxis_check_fault`, so what `is_pending()` encapsulates is baked into
    /// emitted instructions and must be pinned here.
    ///
    /// The `brif` the backend emits treats the loaded word as the predicate, so
    /// "a fault is pending" and "the word is non-zero" have to be the same
    /// statement. That holds because `None` is 0 and no other kind is — and the
    /// second half needs no loop here: [`FaultKind`] gives every variant an
    /// explicit discriminant, and Rust rejects an enum that assigns one twice.
    /// So pinning `None == 0` is the whole of what is left to check.
    #[test]
    fn the_fault_record_is_one_kind_at_offset_zero() {
        assert_eq!(Fault::KIND_OFFSET, 0);
        assert_eq!(
            Fault::KIND_SIZE,
            4,
            "a `#[repr(C)]` fieldless enum is a C `int`, and the backend loads \
             this width"
        );
        assert_eq!(
            std::mem::size_of::<Fault>(),
            Fault::KIND_SIZE,
            "the kind is the whole record; a second field would make the \
             inline load read half of it"
        );
        assert_eq!(FaultKind::None as u32, 0, "the zero word means no fault");

        // And the load really is the predicate: raise, then read the record's
        // first four bytes the way generated code does.
        let mut fault = Fault::clear();
        let word = |f: &Fault| {
            let base = f as *const Fault as *const u8;
            // SAFETY: `KIND_OFFSET`/`KIND_SIZE` bound a `FaultKind` inside a
            // live `Fault`, and `u32` is that width with no alignment demand
            // the record does not already meet.
            unsafe { base.add(Fault::KIND_OFFSET).cast::<u32>().read() }
        };
        assert_eq!(word(&fault), 0, "a clear record loads as zero");
        fault.set(RaisedFault::INT_OVERFLOW);
        assert_ne!(word(&fault), 0, "a raised record loads as non-zero");
        assert!(fault.is_pending());
    }

    /// The invariant the inline fault check depends on, stated as a test rather
    /// than as prose in ADR-017's Consequences: a context generated code can be
    /// handed has a fault slot to read.
    ///
    /// The two loads generated code emits do not test for null, so a null here
    /// is a segfault rather than a program that never observes a fault.
    #[test]
    fn a_wired_context_has_a_fault_slot() {
        let mut rt = Runtime::new();
        let ctx = rt.context();
        assert!(
            !ctx.pending_fault.is_null(),
            "`Runtime::context` is the only producer of a context generated code \
             sees, and generated code dereferences this without testing it"
        );
        // SAFETY: non-null as just asserted, and it points at `rt`'s own slot,
        // which outlives this borrow.
        assert!(!unsafe { (*ctx.pending_fault).is_pending() });
    }

    /// **ADR-116's whole correctness argument, as one assertion.** Generated
    /// code proves a value's type by loading
    /// `[ctx + RuntimeContext::descriptor_offset(id)]` and comparing it against
    /// the header's descriptor word (ADR-102). If a slot held a neighbour's
    /// descriptor, that proof would accept an object of the wrong type and the
    /// payload read behind it would be a wrong-type read at whatever width the
    /// backend folded — so the correspondence between the slot index and the
    /// descriptor is the one thing this table has to get right.
    ///
    /// It is checked here at the offset generated code reads, in bytes, rather
    /// than by indexing the Rust array: indexing would re-derive the stride
    /// from `size_of` and prove that `descriptor_offset` and the compiler agree
    /// only if they were both wrong in the same way.
    #[test]
    fn every_descriptor_slot_holds_the_builtin_whose_id_indexes_it() {
        let mut rt = Runtime::new();
        let ctx = rt.context();
        let base = &ctx as *const RuntimeContext as *const u8;
        for index in 0..BuiltinTypeId::COUNT {
            let id = BuiltinTypeId::from_u32(index as u32).expect("index is in range");
            // SAFETY: the two `const _` blocks beside the field bound the table
            // inside the context, and `ctx` is live for this borrow.
            let read = unsafe {
                base.add(RuntimeContext::descriptor_offset(id))
                    .cast::<*const TypeDescriptor>()
                    .read()
            };
            assert!(
                std::ptr::eq(read, id.descriptor()),
                "the slot generated code reads for {id:?} holds `{}`",
                // SAFETY: every slot holds a `&'static TypeDescriptor`'s address.
                unsafe { (*read).name }
            );
        }
    }

    /// The table is the same in every context a process mints, which is what
    /// lets the backend fold a displacement and nothing else.
    ///
    /// Two runtimes are two heaps, two fault slots and two shadow stacks; they
    /// are not two sets of built-in descriptors, because those are `static`s of
    /// this binary. A future runtime that minted per-`Runtime` descriptors
    /// would make code compiled for one unusable against another, and this is
    /// where that would be noticed.
    #[test]
    fn two_runtimes_agree_on_every_descriptor_address() {
        let mut first = Runtime::new();
        let mut second = Runtime::new();
        assert_eq!(first.context().descriptors, second.context().descriptors);
    }

    /// A placeholder carries the real table, alone among its fields.
    ///
    /// Every pointer `placeholder` nulls is one a `Runtime` has to wire. These
    /// are not: they are addresses of `static`s, valid before `main`. Nulling
    /// them would be a trap for a state that cannot arise, and it would make a
    /// placeholder fail ADR-102's proof by segfaulting on the load rather than
    /// by comparing unequal.
    #[test]
    fn a_placeholder_context_still_knows_every_builtin_descriptor() {
        let mut header = GcHeader::detached();
        let nn = NonNull::from(&mut header);
        // SAFETY: local live header for the duration of this test.
        let gcref = unsafe { GcRef::from_non_null(nn) };
        let ctx = unsafe { RuntimeContext::placeholder(gcref) };
        assert!(std::ptr::eq(
            ctx.descriptors[BuiltinTypeId::Int as usize],
            &crate::scalars::INT
        ));
        assert!(ctx.descriptors.iter().all(|d| !d.is_null()));
    }

    #[test]
    fn placeholder_reports_no_fault() {
        let mut header = GcHeader::detached();
        let nn = NonNull::from(&mut header);
        // SAFETY: local live header for the duration of this test.
        let gcref = unsafe { GcRef::from_non_null(nn) };
        let ctx = unsafe { RuntimeContext::placeholder(gcref) };
        assert!(!ctx.has_pending_fault());
        assert_eq!(ctx.current_generation, 0);
    }

    #[test]
    fn has_pending_fault_flips_with_non_null_pointer() {
        let mut header = GcHeader::detached();
        let nn = NonNull::from(&mut header);
        let gcref = unsafe { GcRef::from_non_null(nn) };
        let mut ctx = unsafe { RuntimeContext::placeholder(gcref) };
        assert!(!ctx.has_pending_fault());
        let mut fault = Fault::clear();
        fault.set(RaisedFault::INT_OVERFLOW);
        ctx.pending_fault = &mut fault;
        assert!(ctx.has_pending_fault());
    }

    /// `set` takes a [`RaisedFault`] and there is no `RaisedFault` for `None`,
    /// so `fault.set(FaultKind::None)` does not compile: the property is
    /// structural. What is left to test is the one place a `FaultKind` arriving
    /// as data becomes a raisable one — and that it rejects the absence of a
    /// fault.
    #[test]
    fn setting_none_cannot_create_a_pending_fault() {
        assert!(
            RaisedFault::new(FaultKind::None).is_none(),
            "FaultKind::None represents the absence of a fault and cannot be raised"
        );

        let mut fault = Fault::clear();
        assert!(!fault.is_pending());
        assert_eq!(fault.kind(), FaultKind::None);

        // Every other kind round-trips, and raising one is what makes a fault
        // pending — there is no second field to disagree with the kind.
        for kind in [
            FaultKind::IntOverflow,
            FaultKind::DivByZero,
            FaultKind::IndexOutOfBounds,
            FaultKind::ParseFailed,
            FaultKind::EmptyCollection,
            FaultKind::StackOverflow,
            FaultKind::FloatToInt,
            FaultKind::InvalidChar,
            FaultKind::InvalidText,
        ] {
            let raised = RaisedFault::new(kind).expect("every non-None kind is raisable");
            assert_eq!(raised.kind(), kind);
            fault.set(raised);
            assert!(fault.is_pending(), "{kind} must be pending once raised");
            assert_eq!(fault.kind(), kind);
        }
    }

    #[test]
    fn runtime_allocates_and_reads_scalars() {
        let rt = Runtime::new();
        let i = rt.alloc_int(-123);
        assert_eq!(i.as_int(), -123);
        let b = rt.alloc_bool(true);
        assert!(b.as_bool());
        let by = rt.alloc_byte(200);
        assert_eq!(by.as_byte(), 200);
        let c = rt.alloc_char('€' as u32);
        assert_eq!(c.as_char(), '€');
        let t = rt.alloc_text("héllo");
        assert_eq!(t.as_text(), "héllo");
        assert_eq!(rt.alloc_unit().as_ptr(), rt.immortals().unit().as_ptr());
    }

    #[test]
    fn runtime_formats_and_compares() {
        let rt = Runtime::new();
        let a = rt.alloc_int(42);
        let b = rt.alloc_int(42);
        let c = rt.alloc_int(43);
        assert!(a.equals(&b));
        assert!(!a.equals(&c));

        let mut out = String::new();
        a.format(&mut out);
        assert_eq!(out, "42");
    }

    #[test]
    fn runtime_vec_allocates_and_reads() {
        let rt = Runtime::new();
        let e0 = rt.alloc_int(1);
        let e1 = rt.alloc_int(2);
        let v = rt.alloc_vec(&crate::scalars::INT, vec![e0, e1]);
        assert_eq!(v.descriptor().name, "Vec");
        assert_eq!(v.as_vec().len(), 2);

        let mut out = String::new();
        v.format(&mut out);
        assert_eq!(out, "[1, 2]");
    }

    #[test]
    fn runtime_collect_keeps_immortals_alive_unrooted() {
        // Immortals are out-of-band; a collection with no roots must not touch
        // them. Capture each singleton's address before the collection and assert
        // the same address afterwards (a self-comparison would assert nothing).
        let rt = Runtime::new();
        let unit_before = rt.immortals().unit().as_ptr();
        let true_before = rt.immortals().true_().as_ptr();
        let false_before = rt.immortals().false_().as_ptr();
        let roots = RootScope::new();
        rt.collect_with(&roots);
        assert_eq!(rt.immortals().unit().as_ptr(), unit_before);
        assert_eq!(rt.immortals().true_().as_ptr(), true_before);
        assert_eq!(rt.immortals().false_().as_ptr(), false_before);
    }

    #[test]
    #[should_panic(expected = "not an Int")]
    fn as_int_rejects_wrong_descriptor() {
        let rt = Runtime::new();
        let b = rt.alloc_bool(false);
        let _ = b.as_int();
    }

    #[test]
    fn a_rerun_starts_from_an_empty_shadow_stack() {
        // The `restart`/`reload` path (§9.7): the debugger reruns `main` against
        // the same `Runtime`. A run that faulted still restored every frame on
        // the way out — the fault epilogue is an epilogue — so the stack is
        // already empty, and `clear_for_rerun` says so with a `debug_assert`
        // before resetting it. That reset is the backstop, not the mechanism.
        let mut rt = Runtime::new();
        let mut ctx = rt.context();
        // SAFETY: `ctx` is wired to `rt`, which outlives the guard.
        let guard = unsafe {
            crate::shadow_stack::push_frame(
                &mut ctx as *mut RuntimeContext,
                crate::shadow_stack::SlotCount::new(5).unwrap(),
            )
        };
        assert_eq!(rt.shadow_stack().len(), 5);
        drop(guard);
        assert!(rt.shadow_stack().is_empty());
        rt.clear_for_rerun();
        assert!(rt.shadow_stack().is_empty());
    }
}
