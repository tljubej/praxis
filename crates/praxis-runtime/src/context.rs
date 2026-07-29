//! The [`RuntimeContext`] handed to every generated function (§10.3, Appendix B)
//! and the [`Runtime`] that owns the heap + immortals.
//!
//! Every generated function receives a hidden first parameter — a pointer to
//! `RuntimeContext` — followed only by `GcRef` arguments, and returns one
//! `GcRef`. The context is the single channel through which generated code
//! reaches the GC heap, the pending fault, the debug frame chain, the input
//! source, and so on.
//!
//! M3 fills in the real [`Heap`] and [`crate::Immortals`]; the fault and
//! debug-frame pointers remain null (M4/M10).

use crate::crash_snapshot::{CrashSnapshot, SnapshotSlot};
use crate::gc::GcRef;
use crate::heap::Heap;
use crate::immortal::{read_bool, Immortals};
use crate::parse_detail::ParseDetail;
#[cfg(test)]
use crate::roots::RootSet;
use crate::shadow_frame::ShadowFrame;
use crate::{collections::VecPayload, descriptor::TypeDescriptor};

/// The maximum Praxis call depth before the prologue guard raises
/// [`FaultKind::StackOverflow`]. Chosen with headroom under the native stack's
/// abort threshold: high enough that legitimate recursion passes, low enough
/// that runaway recursion faults cleanly instead of killing the host with
/// SIGABRT. The guard is emitted in every generated function's prologue (§9.2,
/// §17.4).
pub const MAX_RECURSION_DEPTH: u32 = 8000;

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
    /// indexing and similar accessors in M5.
    IndexOutOfBounds = 3,
    /// An input parse mismatch (§7.11). Raised by the input-parser interpreter
    /// when the input does not match a parser expression. The fault carries no
    /// structured spans yet (M6 surfaces it as a plain fault; the crash debugger
    /// in M10 will render the input/parser spans from the runtime's plan).
    ParseFailed = 4,
    /// An operation required a non-empty collection but found an empty one
    /// (§9.2). Raised by `Deque.pop_front`/`pop_back`, heap `pop`/`peek`, and
    /// similar accessors on an empty collection.
    EmptyCollection = 5,
    /// Recursion exceeded the depth limit (§9.2, §17.4). Raised by the
    /// prologue guard in every generated function when `recursion_depth` would
    /// exceed `MAX_RECURSION_DEPTH`, so the host survives deep recursion
    /// (`count(100000)` and similar) instead of overflowing the native stack
    /// and aborting (SIGABRT).
    StackOverflow = 6,
    /// A `Float` value could not be converted to `Int`: NaN, ±infinity, or a
    /// finite value outside the signed 64-bit range (§4.12). `Float` arithmetic
    /// itself never faults (per IEEE-754 it produces inf/nan); only the
    /// narrowing `to_int` conversion does.
    FloatToInt = 7,
    /// A code point was not a Unicode scalar value: negative, above
    /// `0x10FFFF`, or in the surrogate range `D800..=DFFF` (§4.3). Raised by
    /// `praxis_alloc_char`, which previously had no kind of its own to report
    /// and raised `None` (RT-17/RT-18).
    InvalidChar = 8,
    /// A byte buffer that had to be `Text` was not valid UTF-8 (§4.3). Raised
    /// by `praxis_alloc_text`, which recovers with a lossy conversion rather
    /// than panicking across the ABI — but the recovery is a fault, not a
    /// silent success, and now says so.
    InvalidText = 9,
    /// A size or extent the runtime cannot honour: a negative `Grid` width or
    /// height, a `width * height` that overflows or exceeds
    /// [`GridExtent::MAX_CELLS`](crate::collections::GridExtent::MAX_CELLS), or
    /// a `BitSet` member outside [`BitIndex`](crate::bitset::BitIndex)'s range
    /// (§9.2). These reached Rust as a `usize` cast and became an OOM abort or
    /// a capacity-overflow panic *across* `extern "C"`; they are now faults
    /// (RT-07).
    InvalidSize = 10,
    /// A value did not have the type its destination declared: pushing a
    /// `Float` into a `Vec[Int]`, or constructing a `Grid[T]` whose cell type
    /// has no default value to fill with (§9.2). The collection used to *retag*
    /// itself to the intruder's type, so every element already stored was then
    /// read through the wrong layout (P0-11).
    TypeMismatch = 11,
    /// The program called `panic(value)` (§9.1). The value it passed is
    /// rendered through its descriptor into the runtime's [`FaultMessage`]
    /// slot, so the fault says *what* the program stopped for.
    Panic = 12,
    /// An `assert(condition)` found its condition false (§9.1). Carries no
    /// message: `assert` takes a condition and nothing else, so any text would
    /// only restate the kind. `panic` is the name that carries words.
    AssertFailed = 13,
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
/// spelling. It used to take a bare `FaultKind`, and two callers passed `None`
/// for want of a kind that described them: the result was `{pending: true, kind:
/// None}`, on which generated code branched to its fault path while the host
/// reported "no fault" and exited zero (RT-17).
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
        }
    }
}

/// The fault record a [`RuntimeContext`] points at. `pending_fault` is non-null
/// and points at the owning runtime's slot.
///
/// **The kind is the whole state.** There used to be a `pending: bool` beside
/// it, documented as a mirror of `kind != None` and justified as "a cheap
/// single-byte check in generated code" — but generated code never read it (it
/// calls `praxis_check_fault`), and the two could disagree: `set(FaultKind::None)`
/// produced `{pending: true, kind: None}`, on which generated code branched to
/// its fault path while the host reported "no fault" (RT-17). One field cannot
/// contradict itself.
#[repr(C)]
pub struct Fault {
    /// The pending fault, or [`FaultKind::None`] for no fault. Private: the
    /// only way to raise one is [`Fault::set`], which takes a [`RaisedFault`].
    kind: FaultKind,
}

impl Fault {
    /// A fresh, clear fault record (no fault pending).
    pub fn clear() -> Self {
        Fault {
            kind: FaultKind::None,
        }
    }

    /// Raise `fault`.
    ///
    /// Takes a [`RaisedFault`] rather than a `FaultKind` so that "raise no
    /// fault" — the state generated code and the host disagreed about — has no
    /// spelling.
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

/// One local variable in a debug frame snapshot (§9.3, M5).
///
/// Carries the source name, the compiler-assigned `symbol_id` (which
/// disambiguates shadowed bindings — two `let a` in the same scope get distinct
/// ids, §4.2), the local's type descriptor, and the current `GcRef` value. The
/// crash debugger (M10) reads these to display locals; M5 only *registers* them
/// (the prologue/epilogue push/pop frames and the spill updates the values).
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
    /// by the backend at push time from the MIR local's `Type`. Null only on
    /// frames constructed before M10-WS2 (the M5 unit tests).
    pub descriptor: *const crate::TypeDescriptor,
    /// The current value of the local, or `None` for a slot no value has been
    /// written into yet (updated by the debug spill at safepoints).
    ///
    /// `GcRef` is `#[repr(transparent)]` over a `NonNull`, so `None` is the
    /// all-zero niche and this field is still one machine word: generated code
    /// writes a raw pointer and gets `Some`, and the zeroed slot a fresh frame
    /// starts with *is* `None`. The predecessor was a `GcRef` holding
    /// `NonNull::dangling()`, compared by pointer identity to decide whether a
    /// slot held anything — which is to say, an invalid `GcRef` constructed in
    /// Rust (UB) and a sentinel a real allocation could in principle collide
    /// with (F18).
    pub value: Option<GcRef>,
    /// The full static `Type` id (`praxis_types::Type(u32)` handle, M10-WS1b),
    /// so the crash debugger can reconstruct the local's *exact* type —
    /// including collection element types (`Vec[Int]`, `Map[Text, Int]`) and
    /// record field shapes — which the runtime `descriptor` alone loses. The
    /// debugger pairs this id with the live `TypeDb` to type-check `p EXPR`
    /// against the selected frame (§9.5). `0` until the backend threads it
    /// (M10b); sound as a fallback since the debugger treats `0` as "unknown".
    pub type_id: u32,
    /// The debugger classification: `LOCAL_KIND_USER` (a binding the programmer
    /// wrote) or `LOCAL_KIND_TEMP` (a compiler intermediate). See
    /// [`crate::debug::LOCAL_KIND_USER`]. Replaces the old `"<tmp>"` string
    /// placeholder.
    pub kind: u8,
    /// The local's source span start (byte offset), paired with `span_end`.
    pub span_start: u32,
    /// The local's source span end (byte offset). `(span_start, span_end) ==
    /// (0, 0)` means "no span" (the return slot, span-less captures).
    pub span_end: u32,
}

/// One frame in the crash-debugger's snapshot chain (§9.3, M5/M10).
///
/// M5 gave `DebugFrame` a real layout (parent, func name, locals). M10-WS2
/// completes the §9.3 field set: per-local type descriptors (on
/// [`DebugLocal`]), the function's source span, and the active input-parser
/// path. The prologue helper ([`crate::debug::praxis_push_debug_frame`])
/// allocates and links a frame; the epilogue pops it. The shadow-stack spill
/// (ADR-019) keeps the `value` fields fresh across GC safepoints so a crash
/// snapshot reflects live state.
///
/// `source_span` and `parser_path` are **reserved** in M10a: the backend
/// zeroes them at push time (the MIR does not yet carry per-function spans or
/// the active-parser path). M10b fills them so the `source`/`input`/`parser`
/// REPL commands can render them.
#[repr(C)]
pub struct DebugFrame {
    /// The caller's frame, or null for the outermost (`main`) frame.
    pub parent: *mut DebugFrame,
    /// The function's source name (a `'static` embedded string).
    pub func_name: *const u8,
    /// The function name's byte length.
    pub func_name_len: u32,
    /// The local-variable entries, as a pointer + count (FFI-safe slice).
    pub locals: *mut DebugLocal,
    /// How many locals are in the `locals` array.
    pub local_count: u32,
    /// The function's source span `[start, end)` as byte offsets into the
    /// program source (§9.3 "current source span"). `(0, 0)` until M10b threads
    /// the span from the AST through HIR/MIR into the backend.
    pub source_span: (u32, u32),
    /// The active input-parser path at the fault (§9.3), as a `'static`
    /// embedded string. Null/zero-length until M10b populates it from the
    /// parser plan; the `parser` REPL command renders it.
    pub parser_path: *const u8,
    /// The byte length of `parser_path`.
    pub parser_path_len: u32,
}

/// The hidden first argument to every generated function.
///
/// Matches the sketch in Appendix B. Fields are raw pointers because generated
/// Cranelift code reads them at a fixed offset with a fixed calling convention;
/// Rust borrows would not survive across the ABI boundary.
#[repr(C)]
pub struct RuntimeContext {
    pub heap: *mut Heap,
    pub pending_fault: *mut Fault,
    pub debug_top: *mut DebugFrame,
    /// The current top of the compiler-managed shadow-stack root chain (§12.3,
    /// ADR-019). Generated code pushes a frame in the prologue, spills live
    /// `GcRef`s into it at safepoints, and pops it in the epilogue. The
    /// collector walks this chain via [`RootSet`].
    pub roots: *mut ShadowFrame,
    pub input_source: GcRef,
    /// The cached immortal `Unit` — the "defined dummy" returned on fault paths
    /// (§10.4). M6 split this from `input_source` (which now holds the read-in
    /// buffer when present), so fault sentinels are stable regardless of input.
    pub unit_ref: GcRef,
    pub current_generation: u64,
    /// The current Praxis call depth — incremented at every generated function's
    /// prologue and decremented in its epilogue (§9.2, §17.4). A prologue guard
    /// faults with [`FaultKind::StackOverflow`] when this would exceed
    /// [`MAX_RECURSION_DEPTH`], so deep recursion faults cleanly instead of
    /// overflowing the native stack and aborting the host (SIGABRT). Read by
    /// generated Cranelift code at a fixed offset, like the other `#[repr(C)]`
    /// fields above.
    pub recursion_depth: u32,
    /// Host-managed pointer to the runtime's [`crate::ParseDetail`] slot
    /// (§7.11, M10-WS1). The parser interpreter writes the richest parse
    /// mismatch into it on `ParseFailed`; the host (CLI / crash debugger) reads
    /// it after the fault. Generated code never touches this field — it is
    /// appended at the end of `RuntimeContext` so the offsets of all
    /// generated-code-read fields above are unchanged (§11.6 ABI stability).
    pub parse_detail: *mut crate::ParseDetail,
    /// Host-managed pointer to the runtime's [`crate::SnapshotSlot`] (§9.3,
    /// M10-WS3). The first fault epilogue deep-copies the debug-frame chain
    /// into it before unwinding; the host reads the snapshot after the fault.
    /// Like `parse_detail`, generated code only passes it to
    /// `praxis_snapshot_debug_chain` — it is appended at the end of
    /// `RuntimeContext` for ABI stability.
    pub crash_snapshot: *mut crate::SnapshotSlot,
    /// The head of the native root-frame chain (P0-07): what the runtime's own
    /// Rust code holds live across an allocation.
    ///
    /// Pushed and popped by [`crate::roots::NativeScope`], never by generated
    /// code — which is why it, like `parse_detail` and `crash_snapshot`, is
    /// appended at the end of the struct. It is the fifth arm of
    /// [`crate::roots::RuntimeRoots`].
    pub native_roots: *mut crate::roots::NativeRootFrame,
    /// The cached immortal `true`, alongside [`Self::unit_ref`] (§4.3).
    ///
    /// `praxis_alloc_bool` used to mint a *fresh* immortal on every call, so a
    /// program that evaluated a comparison in a loop consumed unregistered
    /// arena storage that no collection could ever reclaim (RT-03). There are
    /// exactly two `Bool` values; the runtime allocates them once.
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
}

impl RuntimeContext {
    /// Construct a context with all pointers null and the input source set to
    /// the canonical placeholder. Real runtime setup (rooting the heap,
    /// installing a fault sink) is done via [`Runtime::context`] in M3+.
    ///
    /// # Safety
    /// `input_source` must be a valid `GcRef` (or the caller must ensure no
    /// generated code dereferences it before the runtime is fully initialized).
    pub unsafe fn placeholder(input_source: GcRef) -> RuntimeContext {
        RuntimeContext {
            heap: std::ptr::null_mut(),
            pending_fault: std::ptr::null_mut(),
            debug_top: std::ptr::null_mut(),
            roots: std::ptr::null_mut(),
            input_source,
            // Placeholder: reuse the input_source ref as the Unit sentinel too,
            // since this constructor is only for not-yet-wired test scaffolding.
            unit_ref: input_source,
            current_generation: 0,
            recursion_depth: 0,
            parse_detail: std::ptr::null_mut(),
            crash_snapshot: std::ptr::null_mut(),
            native_roots: std::ptr::null_mut(),
            // As for `unit_ref`: this constructor is not-yet-wired scaffolding.
            true_ref: input_source,
            false_ref: input_source,
            fault_message: std::ptr::null_mut(),
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
/// This is the M3 entry point for runtime code: construct a `Runtime`, allocate
/// values through it, root them in a [`crate::RootScope`], and collect when
/// needed. In M4, lowering will produce a `RuntimeContext` from a `Runtime` to
/// hand to generated code.
pub struct Runtime {
    heap: Heap,
    immortals: Immortals,
    /// The fault slot generated code signals through (§10.4). Owned here so its
    /// address is stable for the lifetime of the runtime.
    fault: Fault,
    /// The rich parse-failure detail slot (§7.11, M10-WS1). Owned here so its
    /// address is stable; `Runtime::context` installs it on every context. The
    /// parser interpreter writes the deepest mismatch into it; the host reads it
    /// after a `FaultKind::ParseFailed`.
    parse_detail: ParseDetail,
    /// The crash-snapshot slot (§9.3, M10-WS3). Owned here so its address is
    /// stable; the first fault epilogue deep-copies the debug-frame chain into
    /// it before unwinding. The host reads it (and roots it for GC) after a
    /// fault.
    crash_snapshot: SnapshotSlot,
    /// The message slot a `panic`/`assert` fault carries (§9.1). Owned here so
    /// its address is stable; `Runtime::context` installs it on every context.
    fault_message: FaultMessage,
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
        }
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
    /// runtime owns — the shadow chain, the ambient input buffer, a parse
    /// failure's partial value, the crash snapshot, and any native root frame.
    ///
    /// This is the host's collection entry point. It takes no root set: a host
    /// that could name its own would be choosing which of the runtime's owners
    /// to honour, and choosing wrong is P0-06.
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
    /// set that omits the runtime's own owners, which is P0-06 by another name.
    #[cfg(test)]
    pub fn collect_with(&self, roots: &dyn RootSet) {
        self.heap.collect_with(roots);
    }

    /// A `RuntimeContext` view of this runtime, suitable for generated code.
    /// `pending_fault` points at this runtime's fault slot; `debug_top` stays
    /// null until the debugger lands (M10); `roots` starts null — the first
    /// generated function's prologue pushes the initial shadow frame.
    /// `parse_detail` points at this runtime's [`ParseDetail`] slot so the
    /// parser interpreter can record the richest `ParseFailed` detail.
    pub fn context(&mut self) -> RuntimeContext {
        RuntimeContext {
            heap: &mut self.heap as *mut Heap,
            pending_fault: &mut self.fault as *mut Fault,
            debug_top: std::ptr::null_mut(),
            roots: std::ptr::null_mut(),
            input_source: self.immortals.unit(),
            unit_ref: self.immortals.unit(),
            current_generation: 0,
            recursion_depth: 0,
            parse_detail: &mut self.parse_detail as *mut ParseDetail,
            crash_snapshot: &mut self.crash_snapshot as *mut SnapshotSlot,
            // No native frame is on the Rust stack when the context is minted.
            native_roots: std::ptr::null_mut(),
            true_ref: self.immortals.true_(),
            false_ref: self.immortals.false_(),
            fault_message: &mut self.fault_message as *mut FaultMessage,
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

    /// Borrow the rich parse-failure detail slot (§7.11, M10-WS1). The host
    /// reads this after a `FaultKind::ParseFailed` to render the input/parser
    /// span, the expected description, and the actual preview. Returns `None`
    /// when no detail was recorded (e.g. a non-parser `ParseFailed` path).
    #[must_use]
    pub fn parse_detail(&self) -> &ParseDetail {
        &self.parse_detail
    }

    /// Mutably borrow the parse-detail slot (so the host can clear it before a
    /// rerun, or the debugger can read the partial root value).
    pub fn parse_detail_mut(&mut self) -> &mut ParseDetail {
        &mut self.parse_detail
    }

    /// Borrow the crash-snapshot slot (§9.3, M10-WS3). `None` when no fault
    /// snapshotted this run (the program completed cleanly, or faulted before
    /// any debug frame was pushed). The host reads this after a fault for the
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
    /// generation leaks its arena, which is exactly what the pre-S8
    /// `Box::leak` did.
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

// ---- typed allocation helpers (M3 deliverable: "allocation and payload
// access helpers") -----------------------------------------------

impl Runtime {
    /// Allocate an `Int` (§4.3).
    pub fn alloc_int(&self, value: i64) -> GcRef {
        self.heap.alloc_unpaced(&crate::scalars::INT, value)
    }

    /// Allocate a `Bool` as the corresponding immortal singleton (§4.3). Booleans
    /// are always the immortals — there is never a fresh `Bool` allocation.
    pub fn alloc_bool(&self, value: bool) -> GcRef {
        self.immortals.bool_(value)
    }

    /// Allocate a `Byte` (§4.3).
    pub fn alloc_byte(&self, value: u8) -> GcRef {
        self.heap.alloc_unpaced(&crate::scalars::BYTE, value)
    }

    /// Allocate a `Char` (§4.3). Panics if `value` is not a valid scalar value.
    pub fn alloc_char(&self, value: u32) -> GcRef {
        assert!(
            crate::scalars::is_valid_char(value),
            "{value:#x} is not a valid Unicode scalar"
        );
        self.heap.alloc_unpaced(&crate::scalars::CHAR, value)
    }

    /// Allocate a `Float` (§4.3, §4.12). All finite values, ±infinity, and NaN
    /// are valid payloads — `Float` arithmetic never faults (IEEE-754).
    pub fn alloc_float(&self, value: f64) -> GcRef {
        self.heap.alloc_unpaced(&crate::scalars::FLOAT, value)
    }

    /// The immortal `Unit` (§4.3).
    pub fn alloc_unit(&self) -> GcRef {
        self.immortals.unit()
    }

    /// Allocate an owned `Text` (§4.3, ADR-013).
    pub fn alloc_text(&self, value: &str) -> GcRef {
        let owned: Box<str> = value.into();
        // SAFETY: TextPayload matches TEXT's size/align and is fully initialized.
        unsafe {
            self.heap.alloc_with_unpaced(
                &crate::text::TEXT,
                std::mem::size_of::<crate::text::TextPayload>(),
                std::mem::align_of::<crate::text::TextPayload>(),
                |payload| {
                    (payload as *mut crate::text::TextPayload)
                        .write(crate::text::TextPayload::Owned(owned));
                },
            )
        }
    }

    /// Allocate a source-slice `Text` — a zero-copy view into `owner`'s bytes
    /// spanning `[start, start+len)` (§7.10, ADR-013). The slice's descriptor
    /// traces `owner`, keeping the backing alive.
    ///
    /// Returns `None` if the range is not a `Text`: past the owner's end, an
    /// overflowing length, or ends that split a multi-byte scalar. This used to
    /// be a `debug_assert` on the range only, so a release build sliced out of
    /// range or produced a `Text` that read as empty (RT-06).
    ///
    /// # Safety
    /// `owner` must be a live `Text` `GcRef`.
    #[must_use]
    pub unsafe fn alloc_text_slice(&self, owner: GcRef, start: usize, len: usize) -> Option<GcRef> {
        // SAFETY: caller guarantees `owner` is a live Text.
        let slice = unsafe { crate::text::SourceSlice::new(owner, start, len) }?;
        let payload = crate::text::TextPayload::Slice(slice);
        // SAFETY: TextPayload matches TEXT's size/align and is fully initialized.
        Some(unsafe {
            self.heap.alloc_with_unpaced(
                &crate::text::TEXT,
                std::mem::size_of::<crate::text::TextPayload>(),
                std::mem::align_of::<crate::text::TextPayload>(),
                |ptr| (ptr as *mut crate::text::TextPayload).write(payload),
            )
        })
    }

    /// Allocate a `Vec[T]` from a slice of already-allocated element refs and the
    /// element descriptor (§11.2, ADR-013).
    pub fn alloc_vec(
        &self,
        element_descriptor: &'static TypeDescriptor,
        items: Vec<GcRef>,
    ) -> GcRef {
        // SAFETY: VecPayload matches VEC's size/align and is fully initialized.
        unsafe {
            self.heap.alloc_with_unpaced(
                &crate::collections::VEC,
                std::mem::size_of::<VecPayload>(),
                std::mem::align_of::<VecPayload>(),
                |payload| {
                    (payload as *mut VecPayload).write(VecPayload {
                        element_descriptor,
                        items,
                    });
                },
            )
        }
    }

    /// Allocate a `Grid[T]` from a flat row-major list of cells, the element
    /// descriptor, and the column count (§7.5, M6). `items.len()` must be a
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
        // SAFETY: GridPayload matches GRID's size/align and is fully initialized.
        unsafe {
            self.heap.alloc_with_unpaced(
                &crate::collections::GRID,
                std::mem::size_of::<crate::collections::GridPayload>(),
                std::mem::align_of::<crate::collections::GridPayload>(),
                |payload| {
                    (payload as *mut crate::collections::GridPayload).write(
                        crate::collections::GridPayload {
                            element_descriptor,
                            items,
                            width,
                        },
                    );
                },
            )
        }
    }

    /// Allocate a provisional structural `Record` from field values and a static
    /// schema (§7.8, M6). `items.len()` must equal `schema.arity()`.
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
        // SAFETY: RecordPayload matches RECORD's size/align and is fully initialized.
        unsafe {
            self.heap.alloc_with_unpaced(
                &crate::records::RECORD,
                std::mem::size_of::<crate::records::RecordPayload>(),
                std::mem::align_of::<crate::records::RecordPayload>(),
                |payload| {
                    (payload as *mut crate::records::RecordPayload)
                        .write(crate::records::RecordPayload { schema, items });
                },
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

    /// Format this value through its descriptor into `out` (§11.4). Returns the
    /// same `&mut dyn fmt::Write` result the descriptor's `format` produced.
    pub fn format(&self, out: &mut dyn std::fmt::Write) {
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

    /// RT-05: nothing can reset the heap a runtime's immortals live in.
    ///
    /// `Runtime::heap_mut()` handed out `&mut Heap`, and `Heap::reset` tears
    /// down the arena and mints a fresh `HeapId` — so one safe call left
    /// `Runtime.immortals` and every context's cached `unit_ref` / `true_ref` /
    /// `false_ref` naming storage the arena was free to hand out again. The
    /// accessor is deleted; `Runtime` exposes only `&Heap`. This pins the
    /// invariant that made it dangerous.
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

    /// The audit wrote this as `fault.set(FaultKind::None)` followed by
    /// `assert!(!fault.is_pending())`. That line no longer compiles: `set`
    /// takes a [`RaisedFault`], and there is no `RaisedFault` for `None`. The
    /// property is now structural, so what is left to test is the one place a
    /// `FaultKind` arriving as data becomes a raisable one — and that it
    /// rejects the absence of a fault (RT-17).
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
}
