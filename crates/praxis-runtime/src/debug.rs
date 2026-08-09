//! Crash-debugger frame registration (§9.3, ADR-021, ADR-104).
//!
//! What the crash debugger reads for `bt`/`locals` is, per live frame, a
//! function's *static* metadata plus that call's *current* local values. The two
//! are stored apart (ADR-104), because only one of them varies per call:
//!
//! - **Static:** [`FunctionDebugMeta`] — the function's name, source span, and
//!   the [`DebugLocalMeta`] array. One per function, interned in the JIT
//!   generation arena at compile time, shared by every call and every recursion
//!   level. Nothing records it at runtime; it is a compile-time constant.
//! - **Per call:** one machine word per `Gc` local, claimed from a contiguous
//!   [`DebugValueStack`] the runtime owns, and one [`DebugFrameEntry`] pairing
//!   the meta with the base of that run, claimed from a contiguous
//!   [`DebugFrameStack`]. The word is an `Option<GcRef>` for every local whose
//!   box survives compilation and a raw scalar payload for one whose box
//!   ADR-120's forwarding elided; which it is, is [`DebugSlotKind`]'s to say
//!   and nothing else's.
//!
//! Both stacks are [`SlotStack`]s — the mechanism ADR-101 built for the shadow
//! stack and made generic for exactly this. A prologue claims its slots by
//! bumping a `top` inline; an epilogue restores the saved base. **No malloc, no
//! free, no extern call, no `catch_unwind` landing pad.**
//!
//! The values are written **once per definition** by the backend (ADR-104),
//! rather than re-written over the whole live set at every safepoint, and they
//! are never cleared: a value that has been produced stays renderable, which is
//! MIR-16's contract and what `locals` in the crash REPL is for.
//!
//! ## The value slots are not a *strong* root set, and cannot become one by accident
//!
//! [`DebugValueStack`] is `SlotStack<Option<GcRef>>` while the shadow stack is
//! `SlotStack<*mut GcHeader>`. The two are deliberately *different types*: the
//! `RootSet` impl lives on `SlotStackHeader<*mut GcHeader>`, so the debug value
//! stack does not have one and cannot be handed to the collector as something
//! to trace. That is ADR-044's split made structural — the debug set is
//! over-approximate and never cleared, and tracing it would re-couple the two
//! sets and undo MIR-01.
//!
//! It also reads better: a debug slot holds a value or nothing, which
//! `Option<GcRef>` says exactly (its `None` is the all-zero niche, F18, so a
//! zeroed claim *is* a run of "nothing yet"). A shadow slot is a raw pointer
//! only because the collector dereferences it and `GcRef` is `NonNull`.
//!
//! ## …but the collector does *write* them (ADR-106)
//!
//! Not tracing them leaves a hazard: a value whose shadow slot `RootSlots::dead`
//! nulled, but whose debug slot still names it, is unreachable. A collection in
//! that window frees it, `poison()` nulls its descriptor, and the block is then
//! handed back out — after which the debug slot names a live object of an
//! entirely different type, and `praxis_snapshot_debug_chain` copies that into a
//! `CrashSnapshot`, which *is* a strong root set.
//!
//! So the debug frames are [`RuntimeRoots`](crate::RuntimeRoots)' one **weak**
//! arm. [`DebugFrameStackHeader::clear_reclaimed`] runs once per collection,
//! immediately after the sweep, and overwrites every slot naming reclaimed
//! storage with [`RECLAIMED_WORD`]. The slots retain nothing — a dead local's
//! object still dies on schedule — and what the debugger renders for it is
//! `<collected>` rather than freed memory.
//!
//! ## …and one slot in three holds no reference at all (ADR-120 part 2)
//!
//! ADR-120's block-local forwarding deletes the box a value is put into so the
//! next instruction can take it straight back out — and with the box goes the
//! definition that would have written the debugger's slot. Part 2 gives that
//! slot the *scalar* the box would have held, which means a value slot's word
//! is not always an `Option<GcRef>`.
//!
//! That is a memory-safety statement, not a display one, because of the
//! paragraph above: this stack is scanned after every sweep and the scan
//! dereferences what it finds. **The discrimination is
//! [`DebugLocalMeta::slot_kind`], a type, and [`DebugLocalMeta::read`] is the
//! only way to turn a word into a value.** A scalar slot decodes to a
//! [`DebugValue::Scalar`], which contains no `GcRef`, so no consumer — the
//! scan, the crash snapshot's root set, or the debugger's `p EXPR` bindings —
//! can reach a header through one. See [`DebugSlotKind`].

use crate::context::{DebugLocal, RuntimeContext};
use crate::gc::GcRef;
use crate::shadow_stack::{
    SlotStack, SlotStackHeader, MAX_DEBUG_VALUE_SLOTS, MAX_LIVE_SLOTS, MAX_SHADOW_SLOTS,
};
use crate::MAX_RECURSION_DEPTH;

/// How a local appears in the crash debugger (§9.4 `locals`). Mirrors
/// [`praxis_mir::ir::LocalDebugKind`], flattened to a `u8` for the FFI
/// boundary: `0` = a binding, `1` = a compiler temp. "Binding" is ADR-125's
/// sense — a `var`, a parameter, a `for` variable and a name a pattern
/// introduces — so that the FFI constant and the compiler agree about what the
/// byte means (ADR-139). Stored on each [`DebugLocalMeta`] so the debugger can
/// separate the two in its display and name temps with their materializing
/// expression.
pub const LOCAL_KIND_USER: u8 = 0;
pub const LOCAL_KIND_TEMP: u8 = 1;

/// [`DebugLocalMeta::type_id`] when the MIR local has no static type
/// (`MirType::Opaque`) — a pipeline accumulator, a fused-loop item.
///
/// A `Type` is an index into the compiler's arena, so every small integer is a
/// valid handle and there is no in-band "none" — `0` would render as whatever
/// type the arena interned first. `u32::MAX` is outside any arena the debugger
/// will ever pair this with (`type_str` already omits an out-of-range id), and
/// the metadata's null descriptor says the same thing in the other field.
pub const NO_STATIC_TYPE: u32 = u32::MAX;

/// What a debug value slot's word **is** — and the only thing in the process
/// that can say so (ADR-120 part 2).
///
/// The word alone cannot. A slot holding the `Int` payload `4` and a slot
/// holding a `GcRef` to address `4` are the same sixty-four bits, and there is
/// no bit left over to tag them apart: [`DebugValueStack`] is one machine word
/// per local by ADR-104's construction and the shadow stack's, which is what
/// makes a definition's debug store a single `str`.
///
/// So the discrimination lives in the *static* metadata beside the slot, where
/// it costs nothing per call and cannot be corrupted by a program: this field
/// is written once per function by `build_function_debug_meta` at compile time,
/// interned in the JIT generation arena, and never written again.
///
/// **This is the type that makes ADR-120 part 2 sound**, and it is a type
/// rather than a `bool` for a reason. The failure mode the scalar slot creates
/// is *the collector dereferencing an `f64` bit pattern as a [`GcHeader`]*
/// (`crate::GcHeader`), and ADR-106 makes the debug frames the collector's one
/// weak arm — every claimed slot is scanned after every sweep. A `bool` field
/// would be a condition each scan has to remember to test. An enum whose
/// non-[`Reference`](DebugSlotKind::Reference) variants are the input to
/// [`DebugLocalMeta::read`], which answers a
/// [`DebugValue::Scalar`] that *contains no reference*, is a condition no scan
/// can fail to test: there is no path from a scalar slot to a `GcRef`.
///
/// Every [`praxis_mir::ir::ScalarKind`](../../praxis_mir/ir/enum.ScalarKind.html)
/// has a variant here, including `Byte`, which is unwired and which ADR-120's
/// forwarding therefore cannot reach. The map is total on purpose: a partial map
/// would have to answer *something* for a kind it did not cover, and the only
/// available answer is `Reference` — which is precisely the unsound one.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum DebugSlotKind {
    /// The slot holds an `Option<GcRef>`: a reference into the heap, or the
    /// all-zero `None`. Every local whose box survives compilation has this.
    Reference,
    /// `i64` — an `Int` payload whose box ADR-120's forwarding deleted.
    Int,
    /// `u8` widened — a `Bool` payload.
    Bool,
    /// `f64::to_bits()` — a `Float` payload. The bit pattern the scalar channel
    /// carries (`ScalarKind::Float`'s doc), not an `f64` register value.
    Float,
    /// `u32` widened — a `Char` payload.
    Char,
    /// `u8` widened — a `Byte` payload.
    Byte,
}

impl DebugSlotKind {
    /// What generated code adds to a payload before storing it into a debug
    /// slot, and what [`DebugLocalMeta::read`] subtracts on the way out
    /// (ADR-121 decision 2).
    ///
    /// # The problem this solves
    ///
    /// A slot is one word and a claim zeroes its run, so the all-zero word means
    /// "nothing written here yet". For a [`Reference`](Self::Reference) slot
    /// that is *exact* — a `GcRef` is `NonNull`. For a scalar slot it cannot be:
    /// there are 2^64 payloads and 2^64 words, so **some** payload must collide
    /// with "unwritten", and no encoding avoids it. The only question is which.
    ///
    /// Storing the payload raw would make the collision `0` — and therefore
    /// `false`, and `0.0`. ADR-121 promotes *bindings* into scalar slots, so
    /// that collision reaches `var i = 0`, which is close to the most common
    /// line a Praxis program has.
    ///
    /// # What each kind gives up instead
    ///
    /// The bias is chosen per kind so the collision lands on a value the
    /// language does not hold, or barely does:
    ///
    /// | kind | bias | the one payload that reads `<uninit>` |
    /// |---|---:|---|
    /// | [`Reference`](Self::Reference) | 0 | none — `NonNull` has no zero |
    /// | [`Bool`](Self::Bool) | 1 | **none**: two payloads, biased to 1 and 2 |
    /// | [`Char`](Self::Char) | 1 | **none**: `0..=0x10FFFF` biased clear of zero |
    /// | [`Byte`](Self::Byte) | 1 | **none**: `0..=255` biased clear of zero |
    /// | [`Float`](Self::Float) | 1 | one quiet NaN (`0xFFFF_FFFF_FFFF_FFFF`) |
    /// | [`Int`](Self::Int) | `i64::MIN` | `i64::MIN` |
    ///
    /// Three of the six lose nothing at all, because their payloads do not fill
    /// the word. `Float` loses one NaN bit pattern out of the 2^52 that are NaN,
    /// and every one of them prints `NaN` anyway. `Int` genuinely loses a value
    /// a program could compute — and `i64::MIN` against `0` is the whole trade,
    /// made in the direction where the losing case is a number nothing reaches
    /// by accident. `small_int`'s range starts at `-256`, and the sentinel
    /// idiom that module names is `-1`; neither is anywhere near this.
    ///
    /// # Why not a written-marker, which would lose nothing
    ///
    /// A parallel byte per slot, zeroed by the claim and set by each store, is
    /// exact. It is also **a second store per definition** in generated code, on
    /// the path ADR-120 part 2 already measures at 2.4% of the suite. This is one
    /// `iadd_imm` against a register that is about to be stored anyway — an ALU
    /// operation with no memory traffic, which the same measurement cannot see.
    /// Exactness here is worth an instruction, not a store.
    #[inline]
    #[must_use]
    pub const fn store_bias(self) -> i64 {
        match self {
            // Must stay exact: `crash_snapshot` hands this word back as a real
            // reference, and a biased pointer is not one.
            DebugSlotKind::Reference => 0,
            DebugSlotKind::Int => i64::MIN,
            DebugSlotKind::Bool
            | DebugSlotKind::Float
            | DebugSlotKind::Char
            | DebugSlotKind::Byte => 1,
        }
    }
}

/// One scalar payload read out of a debug slot, decoded under the slot's
/// [`DebugSlotKind`].
///
/// The point of the type is what it does **not** have: no pointer, no `GcRef`,
/// no descriptor. A consumer holding one of these cannot reach the heap through
/// it, which is why [`DebugValue`] is safe to hand to the crash-snapshot
/// walker, the renderer and the collector's post-sweep scan alike.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ScalarValue {
    Int(i64),
    Bool(bool),
    Float(f64),
    Char(char),
    Byte(u8),
}

/// Render a payload the way the object it came out of would have rendered.
///
/// **The text must match**, and that is the requirement rather than a nicety:
/// ADR-120 part 2 exists so a user cannot tell which temps the optimizer kept a
/// box for, and a `Float` that printed `3` here and `3.0` through
/// `crate::scalars::FLOAT`'s `format` would give the answer away. So this lives
/// beside those callbacks — `write_float` is literally the one `FLOAT.format`
/// calls (ADR-083's `.0` rule) — rather than in the debugger's renderer, which
/// is the crate that would have had to guess.
impl std::fmt::Display for ScalarValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            ScalarValue::Int(v) => write!(f, "{v}"),
            ScalarValue::Bool(v) => f.write_str(if v { "true" } else { "false" }),
            ScalarValue::Float(v) => {
                crate::scalars::write_float(f, v);
                Ok(())
            }
            ScalarValue::Char(c) => write!(f, "{c}"),
            ScalarValue::Byte(b) => write!(f, "{b}"),
        }
    }
}

/// What a debug slot holds: a reference the collector and the debugger may
/// follow, or a raw scalar neither may.
///
/// [`DebugLocalMeta::read`] is the only constructor, and it is the only place
/// in the runtime where a slot word becomes something typed.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum DebugValue {
    /// A live reference into the heap.
    Reference(GcRef),
    /// A payload whose box the compiler elided (ADR-120).
    Scalar(ScalarValue),
    /// A reference that **was** here and whose object the collector has since
    /// reclaimed (ADR-106's weak arm, [`RECLAIMED_WORD`]).
    ///
    /// Carries nothing, deliberately: the object is gone, and this variant
    /// exists to say *that* rather than to say anything about it. It is the
    /// third thing a slot can be, and it is distinct from "never written": one
    /// is a line that never ran, the other a line that ran and whose result is
    /// no longer around to show.
    ///
    /// Reaching one is ordinary, not exceptional. ADR-044 decision 2 nulls a
    /// shadow slot the moment its local dies, so a binding stops being a root at
    /// its last use rather than at the end of its scope; any collection after
    /// that point is free to take it, while the debug slot goes on naming it.
    /// `var b = "asdf"` followed by an allocation big enough to collect is the
    /// whole recipe.
    Reclaimed,
}

impl DebugValue {
    /// The reference this value names, or `None` if it is a scalar.
    ///
    /// The **one** door from a debug value back to the heap, so every consumer
    /// that roots, traces, poisons-checks or type-recovers a debug value goes
    /// through a single line that a scalar cannot pass. `crash_snapshot`'s
    /// `push_roots`, the collector's post-sweep scan and the debugger's
    /// `p EXPR` bindings are its three callers.
    #[must_use]
    pub fn reference(self) -> Option<GcRef> {
        match self {
            DebugValue::Reference(r) => Some(r),
            // A scalar has no reference and a reclaimed slot no longer has one.
            // The second is what keeps the post-sweep scan idempotent — a slot
            // it already cleared cannot be poison-checked again, because there
            // is no pointer left to check — and what keeps `push_roots` from
            // rooting storage the sweep has handed back.
            DebugValue::Scalar(_) | DebugValue::Reclaimed => None,
        }
    }

    /// Whether this slot still holds something the debugger can render.
    ///
    /// [`DebugLocalMeta::read`] answers `Some` for a reclaimed slot — a value
    /// *was* written — but there is nothing left to show, so a consumer
    /// branching on that `Option` alone has to say which of the two it means.
    /// This is the "renderable" half; `fault_span`'s question is the other one,
    /// and it wants `is_some()`: a temp whose expression finished is not where
    /// the frame faulted, whatever became of the result.
    #[must_use]
    pub fn is_live(self) -> bool {
        !matches!(self, DebugValue::Reclaimed)
    }

    /// Read this value as an `Int`, whether it is still boxed or was elided
    /// into a scalar slot.
    ///
    /// # Panics
    /// If it is neither — the same contract, and the same assertion, as
    /// [`GcRef::as_int`](crate::GcRef::as_int), which this delegates to for a
    /// reference. A caller asking a `Vec` for its integer has a bug either way,
    /// and ADR-120 part 2 does not make that bug quieter.
    #[must_use]
    pub fn as_int(&self) -> i64 {
        match self {
            DebugValue::Reference(r) => r.as_int(),
            DebugValue::Scalar(ScalarValue::Int(v)) => *v,
            DebugValue::Scalar(other) => panic!("not an Int: {other:?}"),
            DebugValue::Reclaimed => panic!("the object this slot named was reclaimed"),
        }
    }

    /// Read this value as a `Vec`'s elements.
    ///
    /// # Panics
    /// If it is a scalar. A `Vec` is never a scalar payload, so this arm is a
    /// caller bug and not a slot the compiler could have produced: ADR-120
    /// forwards `Int`, `Bool` and `Float` boxes only.
    #[must_use]
    pub fn as_vec(&self) -> &[GcRef] {
        match self {
            DebugValue::Reference(r) => r.as_vec(),
            DebugValue::Scalar(other) => panic!("a scalar is not a Vec: {other:?}"),
            DebugValue::Reclaimed => panic!("the object this slot named was reclaimed"),
        }
    }
}

/// One local's metadata at frame construction: the source name (ptr + len),
/// the compiler-assigned symbol id, the local's static type descriptor, the
/// full static `Type` id, the user-vs-temp classification, the source span, and
/// what its value slot's word means.
/// Flattened for FFI.
#[repr(C)]
pub struct DebugLocalMeta {
    pub source_name: *const u8,
    pub name_len: u32,
    pub symbol_id: u32,
    /// The local's static type descriptor (§9.3). The backend embeds the
    /// `'static TypeDescriptor` resolved from the MIR local's `Type`.
    pub descriptor: *const crate::TypeDescriptor,
    /// The full static `Type` id (`praxis_types::Type(u32)` handle). Lets the
    /// debugger reconstruct the exact local type (incl. collection element
    /// types / record shapes) the runtime `descriptor` alone loses.
    pub type_id: u32,
    /// The debugger classification: `LOCAL_KIND_USER` (a binding the programmer
    /// wrote) or `LOCAL_KIND_TEMP` (a compiler intermediate). The split is
    /// structural, not a name convention.
    pub kind: u8,
    /// The local's source span `[start, end)` (byte offsets into program
    /// source) for debugger provenance. User locals carry their binding's span;
    /// temps carry the expression they materialize (rendered as `@ "expr"`).
    /// `(0, 0)` means "no span" (the return slot, span-less captures).
    pub span_start: u32,
    pub span_end: u32,
    /// The function a **direct** call defines this local from (a `'static`
    /// embedded string), or null with a zero `callee_name_len` for every local
    /// that is not one.
    ///
    /// It is what places a *caller* frame's line: a frame below the innermost
    /// is stopped in a call, and the call is the one whose callee is the frame
    /// above it. Static, because it is a fact about the program — the live
    /// slots cannot answer it, since a loop leaves an earlier pass's value in
    /// the call's temp.
    ///
    /// Null for a `CallIndirect`: a closure's target is a value, and the name
    /// this would hold does not exist until the call runs.
    pub callee_name: *const u8,
    pub callee_name_len: u32,
    /// What this local's value slot holds (ADR-120 part 2). [`DebugSlotKind::Reference`]
    /// for every local whose box the compiler kept; a scalar kind for a temp
    /// whose box the block-local forwarding deleted and whose payload the
    /// definition stores raw.
    ///
    /// A real `enum`, not the `u8` its neighbours `kind` and `type_id` are,
    /// because nothing outside Rust ever writes this struct: `#[repr(C)]` is
    /// here so `crate::crash_snapshot` reads a stable layout, and generated code
    /// only ever stores the address of the enclosing [`FunctionDebugMeta`]. So
    /// there is no bit pattern to validate and no "unknown tag" case to decide
    /// what to do with — which is one fewer place the answer could be
    /// `Reference` by accident.
    pub slot_kind: DebugSlotKind,
}

/// The word [`DebugFrameStackHeader::clear_reclaimed`] writes over a reference
/// slot whose object the sweep took, and the one word
/// [`DebugLocalMeta::read`] decodes as [`DebugValue::Reclaimed`].
///
/// ### Why a reserved word costs nothing, where the zero word cost a payload
///
/// [`DebugSlotKind::store_bias`] documents the trade the *unwritten* state
/// forces: a scalar slot has 2^64 payloads and 2^64 words, so reserving one word
/// costs one payload, and the bias only chooses which. Reserving a second word
/// here looks like the same trade and is not, because this word is only ever
/// **interpreted** in a [`DebugSlotKind::Reference`] slot — and a reference slot
/// has far fewer than 2^64 inhabitants. `align_of::<GcHeader>()` is 8 (asserted
/// in `crate::gc`, and `BLOCK_GRANULE` is that same alignment), so no `GcRef`
/// can ever be 1. Scalar slots keep every payload they had; `Int` loses only
/// `i64::MIN`, to the bias, and nothing to this.
///
/// `None` — the *other* reserved word — would say "nothing was ever written
/// here" about a slot whose value the collector had just taken. Those are
/// different facts about the program: one is a line that never ran, the other a
/// line that ran and whose result is simply no longer around to show.
///
/// ### Not part of the ABI
///
/// Generated code only ever **stores** into a debug value slot
/// (`store_debug_local` is a `str` at a fixed displacement, with no matching
/// load), so no compiled program can observe this word — a store past the clear
/// overwrites it, which is exactly the right behaviour if a local is written
/// again. It is therefore not a `RUNTIME_ABI_VERSION` concern: this word is
/// written and read by the runtime alone.
pub const RECLAIMED_WORD: usize = 1;

/// No `GcRef` can collide with [`RECLAIMED_WORD`]: a `GcRef` points at a
/// `GcHeader`, whose alignment `crate::gc` pins at 8, so its low three bits are
/// always clear. This is that argument as a compile error.
const _: () = {
    assert!(
        RECLAIMED_WORD != 0,
        "zero is already `None` — the other state"
    );
    assert!(RECLAIMED_WORD % std::mem::align_of::<crate::GcHeader>() != 0);
};

impl DebugLocalMeta {
    /// Decode one value slot's word under this local's [`slot_kind`](Self::slot_kind).
    ///
    /// **The only place a slot word becomes something typed**, and therefore the
    /// only place the reference/scalar question is asked. `None` is "nothing has
    /// been written here yet"; [`DebugValue::Reclaimed`] is the third state, a
    /// reference slot whose object the collector took after it was written.
    ///
    /// ### The zero word, and the one thing a scalar slot cannot say
    ///
    /// A claim zeroes its run, so an all-zero word is "no value yet" — which for
    /// a [`DebugSlotKind::Reference`] slot is *exact*, because a `GcRef` is
    /// `NonNull` and can never be zero (F18, the niche this module's header
    /// describes).
    ///
    /// For a scalar slot it cannot be exact — 2^64 payloads do not fit in 2^64
    /// words beside an "unwritten" state — so exactly one payload per kind reads
    /// back as `<uninit>`. **Which one is [`DebugSlotKind::store_bias`]'s
    /// choice**: three kinds lose nothing, `Float` loses one NaN, and `Int`
    /// loses `i64::MIN`. The direction of the error is the safe one — a slot
    /// under-reports a value it holds and never reports a value it does not.
    /// `an_int_slot_holding_i64_min_reads_as_uninit_and_zero_does_not` pins both
    /// halves.
    ///
    /// # Safety
    /// `word` must be the current content of a live value slot belonging to a
    /// frame whose metadata is this one — that is, `values[i]` paired with
    /// `locals[i]` of the same [`FunctionDebugMeta`]. Pairing a word with
    /// another local's metadata is exactly the mistake this function exists to
    /// make impossible to write by hand, and the two callers
    /// ([`DebugFrameStackHeader::clear_reclaimed`] and
    /// `crash_snapshot::copy_stack`) both zip the two arrays.
    #[must_use]
    pub unsafe fn read(&self, word: Option<GcRef>) -> Option<DebugValue> {
        let word = word?;
        if self.slot_kind == DebugSlotKind::Reference {
            // The reserved word first, because the whole point of reserving it
            // is that the reference arm never sees it: a `GcRef` built from
            // `RECLAIMED_WORD` would be handed to `crash_snapshot`, rooted, and
            // dereferenced through a descriptor it does not have. The test is
            // confined to this arm — in a scalar slot the same bits are the
            // payload `1`, and that slot's object, if it ever had one, is not
            // this stack's business.
            if word.as_ptr() as usize == RECLAIMED_WORD {
                return Some(DebugValue::Reclaimed);
            }
            return Some(DebugValue::Reference(word));
        }
        // Not a reference: recover the raw bits without ever forming something
        // dereferenceable from them. `GcRef` is `#[repr(transparent)]` over a
        // `NonNull`, so this is the address-as-integer read `strict_provenance`
        // sanctions and not a load through the pointer — then undo the bias
        // generated code applied on the way in (ADR-121 decision 2). Wrapping,
        // because the bias is chosen precisely so that one payload wraps to the
        // all-zero word — and that payload is the one the `word?` above has
        // already answered `None` for.
        let bits = (word.as_ptr() as usize as u64).wrapping_sub(self.slot_kind.store_bias() as u64);
        let scalar = match self.slot_kind {
            // Unreachable: the branch above returned. Spelled out rather than
            // `unreachable!()` so this match stays total over the enum and a
            // new variant is a compile error here.
            DebugSlotKind::Reference => return Some(DebugValue::Reference(word)),
            DebugSlotKind::Int => ScalarValue::Int(bits as i64),
            DebugSlotKind::Bool => ScalarValue::Bool(bits & 1 != 0),
            DebugSlotKind::Float => ScalarValue::Float(f64::from_bits(bits)),
            // A `Char` payload is a validated Unicode scalar everywhere the
            // language can produce one, so `None` here is a compiler bug rather
            // than a program one — and rendering the slot as `<uninit>` is how
            // it stays a missing value instead of becoming a wrong character.
            DebugSlotKind::Char => ScalarValue::Char(char::from_u32(bits as u32)?),
            DebugSlotKind::Byte => ScalarValue::Byte(bits as u8),
        };
        Some(DebugValue::Scalar(scalar))
    }
}

/// Everything the crash debugger needs about a function that does **not** vary
/// per call: its name, its source extent, and the metadata for its `Gc` locals.
///
/// One of these exists per lowered function, interned by content in the JIT
/// generation arena (ADR-043), so a debugger session that recompiles the same
/// function on every `p EXPR` (DBG-05) pays for it once. A generated prologue
/// stores its address into a [`DebugFrameEntry`] — one immediate, one store.
///
/// `#[repr(C)]` because generated code writes its address and
/// [`crate::crash_snapshot`] reads its fields across the ABI boundary.
#[repr(C)]
pub struct FunctionDebugMeta {
    /// The function's source name (a `'static` embedded string).
    pub func_name: *const u8,
    /// The function name's byte length.
    pub func_name_len: u32,
    /// How many `Gc` locals this function has — the length of both `locals` and
    /// the run of value slots a call of it claims.
    pub local_count: u32,
    /// `local_count` entries, in **debug-slot order**: the local's position among
    /// this function's `Gc` locals, in MIR local order. Entry `i` describes the
    /// word at displacement `i` of the run a call claims, which is what
    /// [`crate::crash_snapshot`] and [`DebugFrameStackHeader::clear_reclaimed`]
    /// rely on when they zip the two.
    ///
    /// Debug-slot order is **not** shadow-slot order (ADR-128 decision 3): a
    /// shadow slot index is a *colour* — `is_prime`'s shadow indices are `{0}`
    /// while its debug indices are `0..33` — so the two stacks are not
    /// index-parallel with each other.
    pub locals: *const DebugLocalMeta,
    /// The function's source span `[start, end)` as byte offsets into the
    /// program source (§9.3 "current source span", ADR-035 decision 3). `(0, 0)`
    /// means "no span recorded" — the `__fnvalue_*` adapter and `__p_expr`,
    /// whose bodies nobody wrote. A closure's span is its literal's.
    pub span_start: u32,
    pub span_end: u32,
}

/// One live call's debug frame: which function, and where its value slots are.
///
/// This is the whole of what a frame is — everything else about the call is
/// static and lives in [`FunctionDebugMeta`], and the parent frame is the entry
/// below this one on the stack.
///
/// Claimed by bumping the [`DebugFrameStack`]'s `top` in the prologue and
/// released by restoring the saved base in the epilogue.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DebugFrameEntry {
    /// The static metadata for the function this call is executing.
    pub meta: *const FunctionDebugMeta,
    /// The base of this call's run of `meta.local_count` value slots inside the
    /// [`DebugValueStack`].
    pub values: *mut Option<GcRef>,
}

impl DebugFrameEntry {
    /// The byte offsets generated code writes at. Derived from the `#[repr(C)]`
    /// layout, like every other offset the backend emits (Appendix B), so a
    /// reorder here is a recompiled constant rather than a silent miscompile.
    pub const META_OFFSET: i32 = core::mem::offset_of!(Self, meta) as i32;
    pub const VALUES_OFFSET: i32 = core::mem::offset_of!(Self, values) as i32;
    /// The stride the frame stack's `top` moves by, per call.
    pub const SIZE: i64 = core::mem::size_of::<Self>() as i64;

    /// The zero value a fresh reservation holds: no function, no values.
    ///
    /// A claimed-but-unwritten entry is not a state generated code can be
    /// observed in — the prologue's claim and its two stores are straight-line
    /// with nothing between them — so this exists for [`SlotStack::new`], not as
    /// a case the snapshot walker handles.
    #[must_use]
    pub const fn empty() -> DebugFrameEntry {
        DebugFrameEntry {
            meta: std::ptr::null(),
            values: std::ptr::null_mut(),
        }
    }
}

/// The runtime's one reservation of per-call debug value slots.
///
/// `Option<GcRef>` rather than the shadow stack's `*mut GcHeader`, for two
/// reasons stated in this module's header: the `None` niche means a zeroed claim
/// *is* a run of "no value yet" (F18), and the distinct type is what keeps
/// `impl RootSet for SlotStackHeader<*mut GcHeader>` from applying here. The
/// collector must not **trace** these slots; ADR-044 decision 2 nulls a shadow
/// slot the moment its local dies, and this stack deliberately does not.
///
/// It does scan them, after every sweep, and mark the ones whose object that
/// sweep reclaimed — see [`DebugFrameStackHeader::clear_reclaimed`] and
/// ADR-106. That is the difference between keeping a value *alive* and keeping
/// a slot *valid*, and only the second is this stack's business.
///
/// So a slot's word is one of three things, not two: zero (nothing written),
/// [`RECLAIMED_WORD`] (written, then collected), or a value. `Option<GcRef>`
/// spells the first and the third; the second is a reserved word inside the
/// `Some` half, which costs nothing because a reference slot's inhabitants are
/// aligned pointers.
pub type DebugValueStack = SlotStack<Option<GcRef>>;
/// The header generated code bump-allocates value slots against.
pub type DebugValueStackHeader = SlotStackHeader<Option<GcRef>>;
/// The runtime's one reservation of per-call frame entries.
pub type DebugFrameStack = SlotStack<DebugFrameEntry>;
/// The header generated code bump-allocates frame entries against.
pub type DebugFrameStackHeader = SlotStackHeader<DebugFrameEntry>;

/// The size of the debug value reservation, in slots.
///
/// **Sized by its own headroom term** (ADR-128 decision 3), not written as
/// `SHADOW_STACK_SLOTS`: root slots are colored by live range and debug value
/// slots stay dense, one per `Gc` local, so the two index spaces answer
/// different questions and are bounded by different caps
/// ([`MAX_SHADOW_SLOTS`](crate::MAX_SHADOW_SLOTS) and
/// [`MAX_DEBUG_VALUE_SLOTS`]).
///
/// Exhaustion is unrepresentable here exactly as it is on the shadow stack, and
/// for the same reason: every generated prologue rejects
/// `stack_left < frame_cost(slots)` before it claims anything, and
/// [`frame_cost`](crate::frame_cost) charges the **dense** count of `Gc` locals
/// (ADR-128 decision 4) — which is precisely this stack's width. So the claimed
/// debug value slots of all live frames are bounded by `MAX_LIVE_SLOTS`, the
/// same `budget / FRAME_BYTES_PER_SLOT + MAX_RECURSION_DEPTH ×
/// REFERENCE_FRAME_SLOTS` that bounds the shadow stack, and generated code emits
/// no bounds check because there is nothing left to check.
///
/// The headroom term is [`MAX_DEBUG_VALUE_SLOTS`] rather than `MAX_SHADOW_SLOTS`,
/// for the reason `SHADOW_STACK_SLOTS` keeps one at all: the Rust-side
/// [`push_frame`] callers spend no budget, so the argument above does not cover
/// them, and one widest frame of headroom is what covers them instead.
///
/// This is a *reservation*, and the weak scan (ADR-106) does not walk it. Its
/// cost is bounded by `top - base` — the slots live calls have actually claimed
/// — so raising this number costs address space and not collection time.
pub const DEBUG_VALUE_STACK_SLOTS: usize = MAX_LIVE_SLOTS + MAX_DEBUG_VALUE_SLOTS;

// The capacity identity for this stack, spelled the same way and for the same
// reason `SHADOW_STACK_SLOTS`'s is (ADR-128 decision 3: "the assert is not
// optional"). The hazard is not someone raising the budget — the reservation
// follows it — it is someone deciding ~5 MiB of address space is too much and
// writing a smaller number. That edit makes debug-value-stack overflow reachable
// from generated code, silently, because generated code does not check the limit.
// This fails the *build* instead.
const _: () = assert!(
    DEBUG_VALUE_STACK_SLOTS > MAX_LIVE_SLOTS,
    "the debug value stack must cover every slot the budget can buy, plus one \
     frame of headroom for Rust-side pushes"
);

// And the premise that keeps the two stacks' bounds the same arithmetic: a
// colored root width can never exceed the dense debug width, so the budget
// charge on the dense count over-covers the shadow stack. If a later change ever
// made the shadow claim the wider of the two, `SHADOW_STACK_SLOTS` would need its
// own re-derivation rather than this one.
const _: () = assert!(
    MAX_SHADOW_SLOTS <= MAX_DEBUG_VALUE_SLOTS,
    "a function's root slots are colored from its `Gc` locals, so there cannot \
     be more of them than there are locals"
);

// The two reservations differ only in their headroom terms — same budget-derived
// terms, different headroom (ADR-128 decision 3). A `const` block rather than a
// test for the reason the capacity identity above is one: this is arithmetic over
// constants, so a build that disagrees with it should not link.
const _: () = assert!(
    DEBUG_VALUE_STACK_SLOTS - crate::SHADOW_STACK_SLOTS == MAX_DEBUG_VALUE_SLOTS - MAX_SHADOW_SLOTS,
    "the two slot reservations differ by exactly their headroom terms, because \
     the budget-derived terms are the same arithmetic over the same charge"
);

/// The size of the debug frame-entry reservation, in slots — one per live call,
/// bounded by the same depth guard, plus one entry of headroom for the
/// Rust-side [`push_frame`] callers, who spend no budget and so are not covered
/// by that guard. This is `SHADOW_STACK_SLOTS`' headroom term counted in
/// frames, because a frame stack claims one entry per call rather than one per
/// slot.
pub const DEBUG_FRAME_STACK_SLOTS: usize = MAX_RECURSION_DEPTH as usize + 1;

// ---------------------------------------------------------------------------
// The weak arm (ADR-106)
// ---------------------------------------------------------------------------

impl DebugFrameStackHeader {
    /// Mark every claimed debug value slot whose object the sweep that just
    /// finished reclaimed, and answer how many were marked.
    ///
    /// This is the entire content of [`RuntimeRoots`](crate::RuntimeRoots)' one
    /// weak arm. It retains nothing: it runs *after* the mark and the sweep have
    /// already decided what dies, and its only effect is to replace a reference
    /// to storage that no longer holds an object with [`RECLAIMED_WORD`], the
    /// absence that says how it became one.
    ///
    /// ### Why `is_poisoned`, and why here
    ///
    /// Sweep calls `GcHeader::poison` on each reclaimed block *before* it clears
    /// that block's `allocated` bit (ADR-039 decision 3), and nothing else in the
    /// runtime ever nulls a descriptor. So at this instant "poisoned" is exactly
    /// "reclaimed by this collection or an earlier one", and it is a one-word
    /// load and a compare against zero.
    ///
    /// It is only exactly that *at this instant*. `claim_free_block` hands a
    /// reclaimed block back to the next allocation, which writes a fresh header
    /// over the poison — so a slot naming that block stops being distinguishable
    /// from a slot naming a live object, and the two have different types. That
    /// is why this cannot be deferred to `praxis_snapshot_debug_chain` or to the
    /// debugger's render: the window between the sweep and the next allocation is
    /// the only place the question has an answer. `Heap::collect_inner` calls this
    /// inside that window.
    ///
    /// ### Why the frame entries rather than the value stack's `[base, top)`
    ///
    /// A frame entry is what pairs a run of value slots with the `local_count`
    /// that bounds it, and `crash_snapshot::copy_stack` walks exactly these pairs
    /// to build a snapshot. Driving the clear from the same walk makes "every
    /// value a snapshot could copy has been checked" true by construction rather
    /// than by an argument about the runs partitioning the reservation. The
    /// `debug_assert` below is that argument, kept as a check: if a prologue ever
    /// claims value slots without a frame entry to name them, this fires in every
    /// debug build instead of silently skipping the slots it cannot see.
    ///
    /// # Safety
    /// Every claimed entry's `meta` must point at a live [`FunctionDebugMeta`]
    /// whose `locals` array has `local_count` entries, and its `values` at that
    /// many value slots — the same contract `copy_stack` runs under, and the one
    /// every prologue establishes. `values` must be live for the duration of the
    /// call: the collector writes through it.
    ///
    /// A slot whose [`DebugLocalMeta::slot_kind`] is not
    /// [`DebugSlotKind::Reference`] is not a reference and is not scanned. The
    /// exclusion is structural rather than a test this loop performs — see the
    /// comment at the `continue` — and it is what keeps ADR-120 part 2's scalar
    /// slots out of the collector's one weak arm entirely.
    ///
    /// Reading `r.header()` for a reference into a *reclaimed* block is a read of
    /// mapped memory, not a use-after-free: a page is unmapped only at teardown,
    /// after `finalize_all` (`Heap::release_pages`), which is the same premise
    /// that makes the provenance check in `Heap::mark` a rejection rather than a
    /// wild read (ADR-103 decision 3).
    pub(crate) unsafe fn clear_reclaimed(&self, values: &DebugValueStackHeader) -> usize {
        let mut cleared = 0usize;
        let mut scanned = 0usize;
        for entry in self.claimed() {
            // SAFETY: the caller guarantees a live `meta` on every claimed
            // entry. `copy_stack` treats null the same way and for the same
            // reason: a prologue writes both words in straight-line code, so
            // this is unreachable rather than handled.
            let Some(meta) = (unsafe { entry.meta.as_ref() }) else {
                continue;
            };
            let count = meta.local_count as usize;
            scanned += count;
            for i in 0..count {
                // SAFETY: the caller guarantees `values` names `local_count`
                // live slots. This is the same pointer a generated debug store
                // and `DebugFrameGuard::set` write through, carrying the
                // reservation's own provenance — not a pointer re-derived from
                // a shared slice.
                let slot = unsafe { entry.values.add(i) };
                // SAFETY: the caller guarantees `locals` names `local_count`
                // entries, and slot `i` is local `i`'s — the two arrays are
                // index-parallel by ADR-104's construction.
                let local = unsafe { &*meta.locals.add(i) };
                // SAFETY: `local` is slot `i`'s own metadata, which is `read`'s
                // whole precondition; the slot holds an initialized word (a
                // claim zeroes its run).
                let value = unsafe { local.read(*slot) };
                // **A scalar slot never reaches `header()`**, and it is the type
                // that says so rather than a test above this line: `read`
                // answers `DebugValue::Scalar`, which holds no `GcRef`, so
                // `reference()` is `None` and the poison check is not merely
                // skipped — it is unreachable. That is the whole of ADR-120
                // part 2's soundness argument against the failure mode it
                // creates, which is this scan dereferencing an `f64` bit
                // pattern as a `GcHeader`.
                let Some(r) = value.and_then(DebugValue::reference) else {
                    continue;
                };
                if r.header().is_poisoned() {
                    // SAFETY: as above. Writing this word is correct for a
                    // reference slot and would be a *lie* in a scalar one — `1`
                    // is a payload there — which is the second reason the arms
                    // are separate.
                    //
                    // Written as a machine word rather than as an
                    // `Option<GcRef>`, for `DebugFrameGuard::set_scalar`'s
                    // reason: no `GcRef` should exist at this address even
                    // momentarily, and a slot word is dereferenceable only after
                    // `DebugLocalMeta::read` has said what it is.
                    unsafe { *slot.cast::<usize>() = RECLAIMED_WORD };
                    cleared += 1;
                }
            }
        }
        debug_assert_eq!(
            scanned,
            values.len(),
            "the frame entries' value runs must partition the value stack's \
             [base, top) — a run of slots no frame entry names is a run this \
             scan cannot reach, and a stale reference in it would survive the \
             collection that freed what it points at"
        );
        cleared
    }
}

impl DebugLocal {
    /// The source name as a `String`. Allocates; for testing/debugger only.
    pub fn name(&self) -> String {
        if self.source_name.is_null() || self.name_len == 0 {
            return String::new();
        }
        // SAFETY: caller (compiler) guarantees valid UTF-8.
        unsafe {
            String::from_utf8_lossy(std::slice::from_raw_parts(
                self.source_name,
                self.name_len as usize,
            ))
            .into_owned()
        }
    }

    /// True iff this local is a user-written binding (a `var`/param/
    /// capture), as opposed to a compiler-generated temporary.
    pub fn is_user(&self) -> bool {
        self.kind == LOCAL_KIND_USER
    }

    /// The local's source span `[start, end)` (byte offsets into program
    /// source), or `None` if none was threaded. `None` is signalled by the
    /// `(0, 0)` sentinel (the zero-width span at offset 0 is not a meaningful
    /// program location for a local that exists).
    pub fn span(&self) -> Option<(u32, u32)> {
        let s = (self.span_start, self.span_end);
        (s != (0, 0)).then_some(s)
    }

    /// The function a direct call defines this local from, or `None` where the
    /// local is not a direct call's result.
    ///
    /// # Safety
    /// The pointer is a compiler-embedded `'static` UTF-8 string — the same
    /// contract [`Self::name`] reads `source_name` under — so the borrow this
    /// returns outlives any frame it came from.
    pub unsafe fn callee(&self) -> Option<&'static str> {
        if self.callee_name.is_null() || self.callee_name_len == 0 {
            return None;
        }
        // SAFETY: the caller guarantees compiler-embedded 'static UTF-8.
        unsafe {
            Some(std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                self.callee_name,
                self.callee_name_len as usize,
            )))
        }
    }
}

/// A debug value slot must stay one machine word: generated code stores into
/// it at a fixed offset with a single `str` — a `GcRef` for a reference slot,
/// a raw payload for a scalar one (ADR-120 part 2) — and reads the zeroed slot
/// a fresh claim starts with as "no value yet". `Option<GcRef>` is
/// niche-optimized to exactly that (F18); this is the compile-time proof, and
/// the second assertion is also what makes `DebugFrameGuard::set_scalar`'s
/// `*mut u64` write in-bounds and correctly aligned.
const _: () = {
    assert!(std::mem::size_of::<Option<GcRef>>() == std::mem::size_of::<GcRef>());
    assert!(std::mem::size_of::<Option<GcRef>>() == std::mem::size_of::<u64>());
    assert!(std::mem::align_of::<Option<GcRef>>() == std::mem::align_of::<u64>());
};

// ---------------------------------------------------------------------------
// The Rust-side push. Generated code does this inline; this is for the
// runtime's own tests and for any host that wants a debug frame the way a
// prologue makes one.
// ---------------------------------------------------------------------------

/// A debug frame claimed from Rust, released when dropped.
///
/// Mirrors [`crate::shadow_stack::ShadowFrameGuard`], and for the same reason:
/// the two stacks must be popped together and in the reverse of the order they
/// were pushed, and an RAII guard is what makes "pop one and not the other"
/// unrepresentable from Rust.
pub struct DebugFrameGuard {
    frames: *mut DebugFrameStackHeader,
    values: *mut DebugValueStackHeader,
    /// This frame's entry, and the frame-stack `top` the drop restores.
    frame_base: *mut DebugFrameEntry,
    /// This frame's first value slot, and the value-stack `top` the drop
    /// restores.
    value_base: *mut Option<GcRef>,
    count: u32,
}

impl DebugFrameGuard {
    /// Record `r` as the current value of local `index`.
    ///
    /// # Panics
    /// If `index` is outside the frame. Writing another frame's slot would make
    /// the *other* frame render a value it never held, which is not a condition
    /// the caller could detect afterwards.
    pub fn set(&mut self, index: usize, r: GcRef) {
        assert!(
            index < self.count as usize,
            "debug slot {index} is outside a {}-local frame",
            self.count
        );
        // SAFETY: `index` is inside the run claimed by `push_frame`, which is
        // live until this guard drops.
        unsafe { *self.value_base.add(index) = Some(r) };
    }

    /// Record the raw word `bits` as the current value of local `index` — the
    /// Rust-side equivalent of the store a definition of an elided box's scalar
    /// emits (ADR-120 part 2).
    ///
    /// Deliberately *not* typed as a [`ScalarValue`]: generated code writes one
    /// machine word and knows nothing about what it means, and a test that
    /// could only write a well-formed payload could not reproduce the state the
    /// collector has to survive — a slot whose word is an `f64` bit pattern
    /// that happens to be a plausible heap address.
    ///
    /// # Panics
    /// If `index` is outside the frame, for [`DebugFrameGuard::set`]'s reason.
    pub fn set_scalar(&mut self, index: usize, bits: u64) {
        assert!(
            index < self.count as usize,
            "debug slot {index} is outside a {}-local frame",
            self.count
        );
        // SAFETY: `index` is inside the run claimed by `push_frame`, which is
        // live until this guard drops. Written as a machine word through a
        // `*mut u64` rather than as an `Option<GcRef>`, because that is what a
        // generated `str` does and because no `GcRef` should exist here even
        // momentarily: a slot word is dereferenceable only after
        // `DebugLocalMeta::read` says its `slot_kind` is `Reference`. The two
        // types have the same size and alignment (the `const _` below), and
        // every bit pattern is a valid `Option<GcRef>` — `NonNull`'s only
        // validity invariant is non-nullness — so the write is in-bounds and
        // leaves the slot initialized either way.
        unsafe {
            *self.value_base.add(index).cast::<u64>() = bits;
        }
    }

    /// Record `payload` as local `index`'s value **the way generated code does**
    /// — biased by its slot kind (ADR-121 decision 2).
    ///
    /// [`set_scalar`](Self::set_scalar)'s counterpart, and the two exist for the
    /// two different questions a test can ask. That one writes a machine word
    /// and is what an adversarial state needs (a payload that *is* a plausible
    /// heap address). This one writes a *payload* and is what a round-trip needs:
    /// with the bias in place, a test that stores a raw word and expects
    /// [`DebugLocalMeta::read`] to answer it back is asserting that the encoding
    /// does not exist.
    pub fn set_scalar_payload(&mut self, index: usize, kind: DebugSlotKind, payload: u64) {
        self.set_scalar(index, payload.wrapping_add(kind.store_bias() as u64));
    }

    /// This frame's value slots, as the crash snapshot reads them.
    #[must_use]
    pub fn values(&self) -> &[Option<GcRef>] {
        // SAFETY: the run is live until this guard drops.
        unsafe { std::slice::from_raw_parts(self.value_base, self.count as usize) }
    }
}

impl Drop for DebugFrameGuard {
    fn drop(&mut self) {
        // SAFETY: both headers were non-null when the guard was made, and
        // belong to a runtime the caller guaranteed outlives it.
        unsafe {
            (*self.frames).restore(self.frame_base);
            (*self.values).restore(self.value_base);
        }
    }
}

/// Claim a debug frame for `meta` on `ctx`'s debug stacks, the way a generated
/// prologue does: one frame entry, and `meta.local_count` value slots that start
/// as `None`.
///
/// # Safety
/// `ctx` must point at a live context wired by
/// [`Runtime::context`](crate::Runtime::context); `meta` must point at a
/// `FunctionDebugMeta` (with a `locals` array of `local_count` entries) valid
/// for at least as long as the returned guard; and the runtime that owns the
/// stacks must outlive the guard.
///
/// # Panics
/// If `ctx` is null, either header is null, or `meta` is null.
#[must_use]
pub unsafe fn push_frame(
    ctx: *mut RuntimeContext,
    meta: *const FunctionDebugMeta,
) -> DebugFrameGuard {
    assert!(!ctx.is_null(), "push_frame needs a wired context");
    assert!(!meta.is_null(), "a debug frame is a function's metadata");
    // SAFETY: the caller guarantees `ctx` and `meta` are live.
    let (frames, values, count) = unsafe {
        (
            (*ctx).debug_frames,
            (*ctx).debug_values,
            (*meta).local_count,
        )
    };
    assert!(
        !frames.is_null() && !values.is_null(),
        "push_frame needs a context from `Runtime::context`, not a placeholder"
    );
    // SAFETY: both headers are non-null and owned by live `SlotStack`s, and
    // `claim` checks each reservation's limit itself.
    unsafe {
        let value_base = (*values).claim(count as usize, None);
        let frame_base = (*frames).claim(1, DebugFrameEntry::empty());
        *frame_base = DebugFrameEntry {
            meta,
            values: value_base,
        };
        DebugFrameGuard {
            frames,
            values,
            frame_base,
            value_base,
            count,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Runtime;

    /// A context wired to a runtime, kept alive alongside it.
    struct Fixture {
        rt: Runtime,
        ctx: Box<RuntimeContext>,
    }

    impl Fixture {
        fn new() -> Fixture {
            let mut rt = Runtime::new();
            let ctx = Box::new(rt.context());
            Fixture { rt, ctx }
        }

        fn ctx_ptr(&mut self) -> *mut RuntimeContext {
            &mut *self.ctx
        }
    }

    /// Two shadowed `a` bindings, as `var a = ...; var a = ...` produces.
    fn shadowed_a_metas() -> [DebugLocalMeta; 2] {
        let name_a = b"a";
        [
            DebugLocalMeta {
                callee_name: std::ptr::null(),
                callee_name_len: 0,
                source_name: name_a.as_ptr(),
                name_len: 1,
                symbol_id: 10,
                descriptor: &crate::scalars::INT,
                type_id: 1,
                kind: LOCAL_KIND_USER,
                span_start: 5,
                span_end: 6,
                slot_kind: crate::debug::DebugSlotKind::Reference,
            },
            DebugLocalMeta {
                callee_name: std::ptr::null(),
                callee_name_len: 0,
                source_name: name_a.as_ptr(),
                name_len: 1,
                symbol_id: 20,
                descriptor: &crate::scalars::INT,
                type_id: 1,
                kind: LOCAL_KIND_USER,
                span_start: 20,
                span_end: 21,
                slot_kind: crate::debug::DebugSlotKind::Reference,
            },
        ]
    }

    fn meta_for(
        name: &'static [u8],
        locals: &[DebugLocalMeta],
        span: (u32, u32),
    ) -> FunctionDebugMeta {
        FunctionDebugMeta {
            func_name: name.as_ptr(),
            func_name_len: name.len() as u32,
            local_count: locals.len() as u32,
            locals: locals.as_ptr(),
            span_start: span.0,
            span_end: span.1,
        }
    }

    /// §4.2's guarantee (ADR-021): shadowed locals are distinguishable in
    /// debugger frames by source name and symbol ID.
    #[test]
    fn a_functions_metadata_distinguishes_shadowed_bindings() {
        let metas = shadowed_a_metas();
        let meta = meta_for(b"f", &metas, (0, 12));
        let mut f = Fixture::new();
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.rt`, and `meta`/`metas` outlive the guard.
        let guard = unsafe { push_frame(ctx, &meta) };

        // SAFETY: the entry the guard just wrote is the one claimed slot.
        let entries = unsafe { (*(*ctx).debug_frames).claimed() };
        assert_eq!(entries.len(), 1, "one frame is on the stack");
        // SAFETY: the entry names the `meta` this test owns.
        let seen = unsafe { &*entries[0].meta };
        assert_eq!(seen.local_count, 2);
        // SAFETY: `locals` is the `metas` array above.
        let locals = unsafe { std::slice::from_raw_parts(seen.locals, 2) };
        // Both named "a" but distinct symbol ids — the §4.2 guarantee.
        assert_eq!(locals[0].symbol_id, 10);
        assert_eq!(locals[1].symbol_id, 20);
        assert_ne!(locals[0].symbol_id, locals[1].symbol_id);
        assert!(std::ptr::eq(locals[0].descriptor, &crate::scalars::INT));
        assert_eq!(locals[0].type_id, 1);
        assert_eq!(locals[0].kind, LOCAL_KIND_USER);
        assert_eq!((locals[1].span_start, locals[1].span_end), (20, 21));
        // The span is the function's, and it is static: nothing writes it at
        // runtime.
        assert_eq!((seen.span_start, seen.span_end), (0, 12));
        drop(guard);
    }

    #[test]
    fn a_frames_values_start_empty_and_hold_what_is_written() {
        // The claim is zeroed, and a zeroed `Option<GcRef>` *is* `None` (F18) —
        // which is what lets a local that has not been assigned yet render as
        // `<uninit>` without a sentinel to compare against.
        let metas = shadowed_a_metas();
        let meta = meta_for(b"f", &metas, (0, 0));
        let mut f = Fixture::new();
        let value =
            f.rt.heap()
                .alloc_unpaced(crate::scalars::INT_PAYLOAD, 42_i64);
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.rt`, and `meta`/`metas` outlive the guard.
        let mut guard = unsafe { push_frame(ctx, &meta) };
        assert_eq!(guard.values(), &[None, None]);
        guard.set(1, value);
        assert_eq!(guard.values()[0], None);
        assert_eq!(guard.values()[1].map(|r| r.as_int()), Some(42));
        drop(guard);
    }

    #[test]
    fn pushing_and_popping_restores_both_tops() {
        // The balance property: pushes and pops match on both stacks.
        // `m10ws2_debug_frame_pushpop_balanced_across_recursion` is its
        // end-to-end form, and `Runtime::clear_for_rerun` asserts on it.
        let metas = shadowed_a_metas();
        let outer_meta = meta_for(b"outer", &metas, (0, 0));
        let inner_meta = meta_for(b"inner", &metas[..1], (0, 0));
        let mut f = Fixture::new();
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.rt`; both metas outlive both guards.
        unsafe {
            let outer = push_frame(ctx, &outer_meta);
            assert_eq!(f.rt.debug_frame_stack().len(), 1);
            assert_eq!(f.rt.debug_value_stack().len(), 2);
            {
                let inner = push_frame(ctx, &inner_meta);
                assert_eq!(f.rt.debug_frame_stack().len(), 2);
                assert_eq!(
                    f.rt.debug_value_stack().len(),
                    3,
                    "the inner frame's one local sits above the outer frame's two"
                );
                drop(inner);
            }
            assert_eq!(f.rt.debug_frame_stack().len(), 1);
            assert_eq!(f.rt.debug_value_stack().len(), 2);
            drop(outer);
        }
        assert!(f.rt.debug_frame_stack().is_empty());
        assert!(f.rt.debug_value_stack().is_empty());
    }

    /// ADR-106's invariant. A value the shadow stack has stopped naming — which
    /// is every `Gc` local after its last use, by ADR-044 decision 2 — is
    /// unreachable while the debugger still names it. The collection that
    /// reclaims it must leave the debug slot as an absence, not as a reference
    /// into storage the allocator is now free to hand out.
    ///
    /// The `9_999` is past the interned small-`Int` range on purpose: an
    /// interned `Int` is an immortal that no sweep touches, so a value inside
    /// that range would pass this test by never dying.
    #[test]
    fn a_weak_slot_whose_object_died_becomes_an_absence() {
        let metas = shadowed_a_metas();
        let meta = meta_for(b"f", &metas, (0, 0));
        let mut f = Fixture::new();
        let value =
            f.rt.heap()
                .alloc_unpaced(crate::scalars::INT_PAYLOAD, 9_999_i64);
        let before = f.rt.heap().stats().live_count;
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.rt`, and `meta`/`metas` outlive the guard.
        let mut guard = unsafe { push_frame(ctx, &meta) };
        // Slot 1 names the value; nothing roots it. This is the state
        // `RootSlots::dead` produces at a local's last use.
        guard.set(1, value);
        assert_eq!(guard.values()[1].map(|r| r.as_int()), Some(9_999));

        f.rt.collect_now();

        assert_eq!(
            f.rt.heap().stats().live_count,
            before - 1,
            "the weak arm must not have retained it — that is the merge ADR-044 \
             refuses, and it would make this test pass for the wrong reason"
        );
        // SAFETY: slot `i` decoded under local `i`'s own metadata, which is
        // `read`'s whole precondition.
        let (written, unwritten) = unsafe {
            (
                metas[1].read(guard.values()[1]),
                metas[0].read(guard.values()[0]),
            )
        };
        assert_eq!(
            written,
            Some(DebugValue::Reclaimed),
            "the debug slot still names swept storage; the next allocation \
             reissues that block and the slot then names an object of another \
             type"
        );
        assert_eq!(
            written.and_then(DebugValue::reference),
            None,
            "and nothing can follow it back into the heap"
        );
        // The contrast this test exists to draw: slot 1 was written and
        // collected, slot 0 was never written. A debugger reading them apart is
        // the difference between `<collected>` and `<uninit>` on a locals line.
        assert_eq!(unwritten, None, "slot 0 was never written");
        assert_ne!(written, unwritten, "the two absences are not one absence");
        drop(guard);
    }

    /// The other half: the scan is a clear, not a sweep of its own. A value some
    /// *strong* arm still roots survives the collection, so its debug slot is
    /// untouched and reads back.
    ///
    /// Without this, nulling every claimed slot unconditionally would pass the
    /// test above and destroy the debugger.
    #[test]
    fn a_weak_slot_whose_object_is_still_rooted_is_untouched() {
        let metas = shadowed_a_metas();
        let meta = meta_for(b"f", &metas, (0, 0));
        let mut f = Fixture::new();
        let value =
            f.rt.heap()
                .alloc_unpaced(crate::scalars::INT_PAYLOAD, 9_999_i64);
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.rt`; the shadow frame and the debug frame
        // are both released before the runtime drops.
        let (mut shadow, mut guard) = unsafe {
            (
                crate::shadow_stack::push_frame(ctx, crate::SlotCount::new(1).unwrap()),
                push_frame(ctx, &meta),
            )
        };
        shadow.set(0, value);
        guard.set(1, value);

        f.rt.collect_now();

        assert_eq!(
            guard.values()[1].map(|r| r.as_int()),
            Some(9_999),
            "the weak scan nulled a slot whose object the shadow stack roots"
        );
        drop(guard);
        drop(shadow);
    }

    /// A pair of metadata whose **second** slot is a scalar (ADR-120 part 2),
    /// so a test can put one of each side by side in one frame and check that
    /// the collector treats them differently.
    fn one_reference_and_one_scalar(kind: DebugSlotKind) -> [DebugLocalMeta; 2] {
        let mut metas = shadowed_a_metas();
        metas[1].slot_kind = kind;
        metas
    }

    /// **The whole of ADR-120 part 2's soundness argument, as a test.** A scalar
    /// slot must not enter the collector's post-sweep scan.
    ///
    /// The word written is the *address of an object this collection reclaims*,
    /// which is the adversarial case rather than a plausible one: had the slot
    /// been marked `Reference` the scan would have found its header poisoned
    /// and nulled it, so "the word is unchanged" is a deterministic statement
    /// about the discrimination working and not about a bit pattern happening
    /// not to look like a pointer. A payload that *is* a heap address is
    /// exactly what an `f64` or a large `Int` can be.
    ///
    /// `a_weak_slot_whose_object_died_becomes_an_absence` is the same program
    /// with the same word in a `Reference` slot, and it asserts the opposite —
    /// so the two together say the `slot_kind` is what decides, and nothing
    /// else is.
    #[test]
    fn a_scalar_slot_is_not_scanned_even_when_its_word_names_reclaimed_storage() {
        let metas = one_reference_and_one_scalar(DebugSlotKind::Int);
        let meta = meta_for(b"f", &metas, (0, 0));
        let mut f = Fixture::new();
        // Past ADR-100's intern range, so the sweep really does reclaim it.
        let doomed =
            f.rt.heap()
                .alloc_unpaced(crate::scalars::INT_PAYLOAD, 9_999_i64);
        let bits = doomed.as_ptr() as usize as u64;
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.rt`, and `meta`/`metas` outlive the guard.
        let mut guard = unsafe { push_frame(ctx, &meta) };
        guard.set_scalar(1, bits);

        f.rt.collect_now();

        // **The claim is about the word**, asserted on the word itself rather
        // than through `read`: the encoding is biased (ADR-121 decision 2), so a
        // decode here would compare `bits` against `bits - i64::MIN` and the
        // test would be about the bias rather than about the scan.
        assert_eq!(
            guard.values()[1].map(|v| v.as_ptr() as usize as u64),
            Some(bits),
            "the scan nulled a scalar slot, which means it read the word as a \
             reference and dereferenced its header"
        );
        // SAFETY: slot 1 is local 1's, which is the pairing `read` requires.
        let seen = unsafe { metas[1].read(guard.values()[1]) };
        assert_eq!(
            seen.and_then(DebugValue::reference),
            None,
            "and a scalar can never be handed back as something to follow"
        );
        drop(guard);
    }

    /// [`RECLAIMED_WORD`] is reserved in a `Reference` slot and **only** there:
    /// the same word in a scalar slot is that slot's payload, and reads back as
    /// one.
    ///
    /// This is the property that makes the reserved word free where the
    /// `store_bias` trade is not. The adversarial case is the cheapest one to
    /// reach: `Byte`'s bias is 1, so the payload `0` — the most ordinary byte
    /// there is — stores *exactly* this word. A decode that tested for the
    /// sentinel before consulting the `slot_kind` would answer `Reclaimed` for
    /// `0u8`, and would do it for `false` and for `'\0'` too, all three biased
    /// the same way.
    #[test]
    fn the_reclaimed_word_is_a_payload_in_a_scalar_slot() {
        let metas = one_reference_and_one_scalar(DebugSlotKind::Byte);
        let meta = meta_for(b"f", &metas, (0, 0));
        let mut f = Fixture::new();
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.rt`, and `meta`/`metas` outlive the guard.
        let mut guard = unsafe { push_frame(ctx, &meta) };
        guard.set_scalar_payload(1, DebugSlotKind::Byte, 0);
        assert_eq!(
            guard.values()[1].map(|v| v.as_ptr() as usize),
            Some(RECLAIMED_WORD),
            "the premise: this payload really does store the reserved word"
        );

        // SAFETY: slot 1 is local 1's, which is the pairing `read` requires.
        let seen = unsafe { metas[1].read(guard.values()[1]) };
        assert_eq!(
            seen,
            Some(DebugValue::Scalar(ScalarValue::Byte(0))),
            "a scalar slot has no reserved words — its `slot_kind` decides \
             before the word does"
        );

        // And a collection does not change that: the scan never reaches a
        // scalar slot, so nothing here can turn into the other reading.
        f.rt.collect_now();
        // SAFETY: as above.
        assert_eq!(
            unsafe { metas[1].read(guard.values()[1]) },
            Some(DebugValue::Scalar(ScalarValue::Byte(0))),
        );
        drop(guard);
    }

    /// The control: the *same* word in a `Reference` slot is cleared. Without
    /// it, a scan that had quietly stopped clearing anything at all would pass
    /// the test above.
    #[test]
    fn the_same_word_in_a_reference_slot_is_still_cleared_by_the_scan() {
        let metas = shadowed_a_metas();
        let meta = meta_for(b"f", &metas, (0, 0));
        let mut f = Fixture::new();
        let doomed =
            f.rt.heap()
                .alloc_unpaced(crate::scalars::INT_PAYLOAD, 9_999_i64);
        let bits = doomed.as_ptr() as usize as u64;
        let ctx = f.ctx_ptr();
        // SAFETY: as above.
        let mut guard = unsafe { push_frame(ctx, &meta) };
        guard.set_scalar(1, bits);

        f.rt.collect_now();

        // SAFETY: slot 1 decoded under local 1's own metadata.
        assert_eq!(
            unsafe { metas[1].read(guard.values()[1]) },
            Some(DebugValue::Reclaimed),
            "a reference slot is scanned"
        );
        drop(guard);
    }

    /// Every scalar kind round-trips through the slot's one machine word, and
    /// `Float` is the one that matters: the scalar channel carries
    /// `f64::to_bits()` (`ScalarKind::Float`'s own doc), so a decode that
    /// forgot `from_bits` would render `4614256656552045848` for `3.14`.
    #[test]
    fn each_scalar_kind_decodes_the_word_its_channel_carries() {
        let cases: [(DebugSlotKind, u64, ScalarValue); 5] = [
            (DebugSlotKind::Int, -7_i64 as u64, ScalarValue::Int(-7)),
            (DebugSlotKind::Bool, 1, ScalarValue::Bool(true)),
            (
                DebugSlotKind::Float,
                std::f64::consts::PI.to_bits(),
                ScalarValue::Float(std::f64::consts::PI),
            ),
            (
                DebugSlotKind::Char,
                u32::from('q') as u64,
                ScalarValue::Char('q'),
            ),
            (DebugSlotKind::Byte, 200, ScalarValue::Byte(200)),
        ];
        for (kind, bits, expected) in cases {
            let metas = one_reference_and_one_scalar(kind);
            let meta = meta_for(b"f", &metas, (0, 0));
            let mut f = Fixture::new();
            let ctx = f.ctx_ptr();
            // SAFETY: `ctx` is wired to `f.rt`, and `meta`/`metas` outlive the guard.
            let mut guard = unsafe { push_frame(ctx, &meta) };
            guard.set_scalar_payload(1, kind, bits);
            // SAFETY: slot 1 is local 1's.
            let seen = unsafe { metas[1].read(guard.values()[1]) };
            assert_eq!(seen, Some(DebugValue::Scalar(expected)), "{kind:?}");
            drop(guard);
        }
    }

    /// **The one thing an `Int` slot cannot say**, pinned so a later change
    /// moves it deliberately rather than discovers it.
    ///
    /// A claim zeroes its run and zero means "nothing written here yet", which
    /// is exact for a `Reference` slot (a `GcRef` is `NonNull`) and cannot be
    /// exact for a scalar one: 2^64 payloads do not fit in 2^64 words beside an
    /// "unwritten" state. Exactly one payload per kind is therefore lost, and
    /// the bias chooses which — for `Int`, `i64::MIN` (ADR-121 decision 2).
    ///
    /// Both halves are asserted here, because the test is worth nothing without
    /// the second: a bias that lost *both* would pass the first line.
    #[test]
    fn an_int_slot_holding_i64_min_reads_as_uninit_and_zero_does_not() {
        let metas = one_reference_and_one_scalar(DebugSlotKind::Int);
        let meta = meta_for(b"f", &metas, (0, 0));
        let mut f = Fixture::new();
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.rt`, and `meta`/`metas` outlive the guard.
        let mut guard = unsafe { push_frame(ctx, &meta) };

        guard.set_scalar_payload(1, DebugSlotKind::Int, i64::MIN as u64);
        // SAFETY: slot 1 is local 1's.
        assert_eq!(
            unsafe { metas[1].read(guard.values()[1]) },
            None,
            "`i64::MIN` is the payload the bias spends: it and an unwritten \
             slot are the same word, and the honest answer for the pair is the \
             absence"
        );

        guard.set_scalar_payload(1, DebugSlotKind::Int, 0);
        // SAFETY: slot 1 is local 1's.
        assert_eq!(
            unsafe { metas[1].read(guard.values()[1]) },
            Some(DebugValue::Scalar(ScalarValue::Int(0))),
            "and `0` — the payload `var i = 0` holds — round-trips, which is \
             the whole of what decision 2 bought"
        );
        drop(guard);
    }

    /// The three kinds whose payloads do not fill the word lose **nothing**, and
    /// that is a stronger claim than "the collision moved".
    ///
    /// `Bool` has two payloads, `Byte` 256 and `Char` rather more; biased by one
    /// none of them can reach the all-zero word, so every value of all three
    /// round-trips. Without this the table in [`DebugSlotKind::store_bias`] is a
    /// claim nothing checks — and the case that matters is `false`, which is as
    /// common in a crash snapshot as `0` is.
    #[test]
    fn the_bounded_scalar_kinds_lose_no_payload_at_all() {
        let cases: [(DebugSlotKind, u64, ScalarValue); 3] = [
            (DebugSlotKind::Bool, 0, ScalarValue::Bool(false)),
            (DebugSlotKind::Byte, 0, ScalarValue::Byte(0)),
            (DebugSlotKind::Char, 0, ScalarValue::Char('\0')),
        ];
        for (kind, payload, expected) in cases {
            let metas = one_reference_and_one_scalar(kind);
            let meta = meta_for(b"f", &metas, (0, 0));
            let mut f = Fixture::new();
            let ctx = f.ctx_ptr();
            // SAFETY: `ctx` is wired to `f.rt`, and `meta`/`metas` outlive the guard.
            let mut guard = unsafe { push_frame(ctx, &meta) };
            guard.set_scalar_payload(1, kind, payload);
            // SAFETY: slot 1 is local 1's.
            assert_eq!(
                unsafe { metas[1].read(guard.values()[1]) },
                Some(DebugValue::Scalar(expected)),
                "{kind:?}'s zero payload is not the unwritten word"
            );
            drop(guard);
        }
    }

    /// `0.0` round-trips and the NaN the bias spends does not.
    ///
    /// `Float` is the kind that *does* fill the word, so it loses one pattern
    /// like `Int` — but the pattern it loses is a quiet NaN, and every NaN
    /// renders `NaN` anyway. The pair is asserted for
    /// `an_int_slot_holding_i64_min_reads_as_uninit_and_zero_does_not`'s reason.
    #[test]
    fn a_float_slot_keeps_zero_and_spends_one_nan() {
        let metas = one_reference_and_one_scalar(DebugSlotKind::Float);
        let meta = meta_for(b"f", &metas, (0, 0));
        let mut f = Fixture::new();
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.rt`, and `meta`/`metas` outlive the guard.
        let mut guard = unsafe { push_frame(ctx, &meta) };

        guard.set_scalar_payload(1, DebugSlotKind::Float, 0.0_f64.to_bits());
        // SAFETY: slot 1 is local 1's.
        assert_eq!(
            unsafe { metas[1].read(guard.values()[1]) },
            Some(DebugValue::Scalar(ScalarValue::Float(0.0))),
            "`0.0` round-trips"
        );

        let spent = u64::MAX;
        assert!(f64::from_bits(spent).is_nan(), "and what it costs is a NaN");
        guard.set_scalar_payload(1, DebugSlotKind::Float, spent);
        // SAFETY: slot 1 is local 1's.
        assert_eq!(
            unsafe { metas[1].read(guard.values()[1]) },
            None,
            "which is the one pattern the bias spends"
        );
        drop(guard);
    }

    /// A scalar renders as the *object would have*, which is the requirement
    /// that keeps ADR-120's elision invisible: a user must not be able to tell
    /// which temps kept a box by looking at the value column.
    #[test]
    fn a_scalar_renders_the_text_its_descriptor_would_have_written() {
        let mut out = String::new();
        // SAFETY: the payload is a real `FloatPayload` on this stack frame.
        let f = 3.0_f64;
        unsafe {
            (crate::scalars::FLOAT.format)(
                std::ptr::addr_of!(f) as *const u8,
                &mut crate::FormatSink::display(&mut out),
            );
        }
        assert_eq!(out, ScalarValue::Float(3.0).to_string(), "ADR-083's `.0`");
        let mut out = String::new();
        let b: crate::scalars::BoolPayload = 1;
        // SAFETY: as above, for a `BoolPayload`.
        unsafe {
            (crate::scalars::BOOL.format)(
                std::ptr::addr_of!(b).cast::<u8>(),
                &mut crate::FormatSink::display(&mut out),
            );
        }
        assert_eq!(out, ScalarValue::Bool(true).to_string());
    }

    /// The scan is driven from the frame entries, so a frame that claims no
    /// value slots must contribute nothing to it — and, more to the point, must
    /// not make the partition check in `clear_reclaimed` disagree with the value
    /// stack's own extent.
    #[test]
    fn a_zero_local_frame_neither_holds_nor_clears_anything() {
        let metas = shadowed_a_metas();
        let outer = meta_for(b"outer", &metas, (0, 0));
        let empty = meta_for(b"nothing", &[], (0, 0));
        let mut f = Fixture::new();
        let value =
            f.rt.heap()
                .alloc_unpaced(crate::scalars::INT_PAYLOAD, 9_999_i64);
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.rt`, and both metas outlive both guards.
        let (mut outer_guard, inner_guard) =
            unsafe { (push_frame(ctx, &outer), push_frame(ctx, &empty)) };
        outer_guard.set(0, value);

        f.rt.collect_now();

        assert!(inner_guard.values().is_empty());
        // SAFETY: slot 0 decoded under local 0's own metadata.
        assert_eq!(
            unsafe { metas[0].read(outer_guard.values()[0]) },
            Some(DebugValue::Reclaimed),
            "the outer frame's slot was still reached by a scan the empty \
             frame took part in"
        );
        drop(inner_guard);
        drop(outer_guard);
    }

    #[test]
    fn a_zero_local_function_claims_no_value_slots() {
        // The counterexample that keeps `MAX_RECURSION_DEPTH` in charge of the
        // depth bound rather than the value stack's capacity, restated for this
        // stack: a function with no `Gc` locals still needs a frame entry (its
        // name must appear in a `bt`) but claims no value slots at all.
        let meta = meta_for(b"nothing", &[], (0, 0));
        let mut f = Fixture::new();
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.rt`, and `meta` outlives the guard.
        let guard = unsafe { push_frame(ctx, &meta) };
        assert_eq!(f.rt.debug_frame_stack().len(), 1);
        assert!(f.rt.debug_value_stack().is_empty());
        assert!(guard.values().is_empty());
        drop(guard);
    }

    // -----------------------------------------------------------------------
    // ADR-128 decision 3: this stack is bounded on its own terms.
    //
    // The shadow stack's three sizing tests, restated here because the two
    // stacks are sized independently and `shadow_stack.rs`'s tests cover only
    // the other one.
    // -----------------------------------------------------------------------

    /// The sibling of `rejects_an_oversized_frame`: a function with more `Gc`
    /// locals than the debug value stack can index is unconstructible, not
    /// rejected at run time.
    #[test]
    fn rejects_a_function_with_more_gc_locals_than_the_stack_can_index() {
        use crate::DebugSlotCount;
        assert!(DebugSlotCount::new(MAX_DEBUG_VALUE_SLOTS as u32).is_some());
        assert!(DebugSlotCount::new(MAX_DEBUG_VALUE_SLOTS as u32 + 1).is_none());
    }

    /// The sibling of `the_reservation_covers_every_slot_the_budget_can_buy`.
    ///
    /// The arithmetic is the shadow stack's, and it closes here for the reason
    /// ADR-128 decision 4 gives: `frame_cost` charges the **dense** count of `Gc`
    /// locals, which is exactly this stack's width. Charging the colored root
    /// count instead would leave this reservation unbounded — which is one of the
    /// two reasons decision 4 refuses the obvious temptation.
    #[test]
    fn the_value_reservation_covers_every_slot_the_budget_can_buy() {
        for width in [0u32, 1, 11, 192, 1024, MAX_DEBUG_VALUE_SLOTS as u32] {
            let per_frame = crate::frame_cost(width);
            let frames = crate::STACK_BUDGET_BYTES / per_frame;
            let slots = frames as usize * width as usize;
            assert!(
                slots <= MAX_LIVE_SLOTS,
                "a stack of {frames} frames {width} `Gc` locals wide claims \
                 {slots} value slots, past the {MAX_LIVE_SLOTS}-slot bound"
            );
        }
        let stack = DebugValueStack::new(DEBUG_VALUE_STACK_SLOTS, None);
        assert_eq!(stack.capacity(), DEBUG_VALUE_STACK_SLOTS);
    }

    /// The sibling of `a_wide_frame_spends_more_budget_than_a_narrow_one`, and
    /// what pins ADR-128 decision 4.
    ///
    /// A function's *debug* width is what the guard charges for, so a function
    /// with many `Gc` locals recurses less deeply than one with few — whatever
    /// its co-live root set colors down to. If some later change moved the charge
    /// onto the colored count, `MAX_DEBUG_VALUE_SLOTS` locals would cost the same
    /// as `REFERENCE_FRAME_SLOTS` and this fails.
    #[test]
    fn a_function_with_more_gc_locals_spends_more_budget() {
        let reference = crate::STACK_BUDGET_BYTES / crate::frame_cost(crate::REFERENCE_FRAME_SLOTS);
        let widest = crate::STACK_BUDGET_BYTES / crate::frame_cost(MAX_DEBUG_VALUE_SLOTS as u32);
        assert_eq!(
            reference, MAX_RECURSION_DEPTH,
            "a reference-width frame still reaches exactly the depth the old \
             call count allowed"
        );
        assert!(
            widest * 50 < reference,
            "and the widest function there is must be far dearer than the \
             reference one — 129 against 8000 as this is written: {widest} vs \
             {reference}"
        );
    }
}
