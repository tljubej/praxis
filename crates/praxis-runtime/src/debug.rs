//! Crash-debugger frame registration (§9.3, M5, ADR-021, ADR-104).
//!
//! What the crash debugger reads for `bt`/`locals` is, per live frame, a
//! function's *static* metadata plus that call's *current* local values. ADR-021
//! carried both in one heap-allocated `DebugFrame` that every generated prologue
//! `Box`ed and chained onto `ctx.debug_top`; ADR-104 splits them, because only
//! one of the two halves varies per call:
//!
//! - **Static:** [`FunctionDebugMeta`] — the function's name, source span, and
//!   the [`DebugLocalMeta`] array. One per function, interned in the JIT
//!   generation arena at compile time, shared by every call and every recursion
//!   level. This is what made `praxis_set_frame_source_span` a *runtime* call to
//!   record a *compile-time* constant, which is why that wrapper is gone.
//! - **Per call:** one `Option<GcRef>` per `Gc` local, claimed from a contiguous
//!   [`DebugValueStack`] the runtime owns, and one [`DebugFrameEntry`] pairing
//!   the meta with the base of that run, claimed from a contiguous
//!   [`DebugFrameStack`].
//!
//! Both stacks are [`SlotStack`]s — the mechanism ADR-101 built for the shadow
//! stack and made generic for exactly this. A prologue claims its slots by
//! bumping a `top` inline; an epilogue restores the saved base. **No malloc, no
//! free, no extern call, no `catch_unwind` landing pad**, where ADR-021's frame
//! cost two to three allocations and three calls per Praxis call.
//!
//! The values are written **once per definition** by the backend (ADR-104), not
//! re-written over the whole `DebugSlots` set at every safepoint, and they are
//! never cleared: a value that has been produced stays renderable, which is
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
//! Not tracing them left a hole, and ADR-104's Consequences registered it: a
//! value whose shadow slot `RootSlots::dead` nulled, but whose debug slot still
//! names it, is unreachable. A collection in that window frees it, `poison()`
//! nulls its descriptor, and the block is then handed back out — after which the
//! debug slot names a live object of an entirely different type, and
//! `praxis_snapshot_debug_chain` copies that into a `CrashSnapshot`, which *is*
//! a strong root set.
//!
//! So the debug frames are [`RuntimeRoots`](crate::RuntimeRoots)' one **weak**
//! arm. [`DebugFrameStackHeader::clear_reclaimed`] runs once per collection,
//! immediately after the sweep, and turns every slot naming reclaimed storage
//! into `None`. The slots retain nothing — a dead local's object still dies on
//! schedule — and what the debugger renders for it changes from freed memory to
//! `<uninit>`, which is the honest answer and the one the `None` niche already
//! spells.

use crate::context::{DebugLocal, RuntimeContext};
use crate::gc::GcRef;
use crate::shadow_stack::{SlotStack, SlotStackHeader, SHADOW_STACK_SLOTS};
use crate::MAX_RECURSION_DEPTH;

/// How a local appears in the crash debugger (§9.4 `locals`). Mirrors
/// [`praxis_mir::ir::LocalDebugKind`], flattened to a `u8` for the FFI
/// boundary: `0` = a user-written binding, `1` = a compiler temp. Stored on
/// each [`DebugLocalMeta`] so the debugger can separate the two in its display
/// and name temps with their materializing expression instead of the old
/// `"<tmp>"` placeholder.
pub const LOCAL_KIND_USER: u8 = 0;
pub const LOCAL_KIND_TEMP: u8 = 1;

/// [`DebugLocalMeta::type_id`] when the MIR local has no static type
/// (`MirType::Opaque`) — a pipeline accumulator, a fused-loop item.
///
/// A `Type` is an index into the compiler's arena, so every small integer is a
/// valid handle and there is no in-band "none": the old lowering wrote `0`,
/// which the debugger faithfully rendered as whatever type the arena interned
/// first. `u32::MAX` is outside any arena the debugger will ever pair this with
/// (`type_str` already omits an out-of-range id), and the metadata's null
/// descriptor says the same thing in the other field.
pub const NO_STATIC_TYPE: u32 = u32::MAX;

/// One local's metadata at frame construction: the source name (ptr + len),
/// the compiler-assigned symbol id, the local's static type descriptor, the
/// full static `Type` id, the user-vs-temp classification, and the source span.
/// Flattened for FFI.
#[repr(C)]
pub struct DebugLocalMeta {
    pub source_name: *const u8,
    pub name_len: u32,
    pub symbol_id: u32,
    /// The local's static type descriptor (§9.3). The backend embeds the
    /// `'static TypeDescriptor` resolved from the MIR local's `Type`.
    pub descriptor: *const crate::TypeDescriptor,
    /// The full static `Type` id (`praxis_types::Type(u32)` handle, M10-WS1b).
    /// Lets the debugger reconstruct the exact local type (incl. collection
    /// element types / record shapes) the runtime `descriptor` alone loses.
    pub type_id: u32,
    /// The debugger classification: `LOCAL_KIND_USER` (a binding the programmer
    /// wrote) or `LOCAL_KIND_TEMP` (a compiler intermediate). Replaces the old
    /// `"<tmp>"` string placeholder — the split is now structural.
    pub kind: u8,
    /// The local's source span `[start, end)` (byte offsets into program
    /// source) for debugger provenance. User locals carry their binding's span;
    /// temps carry the expression they materialize (rendered as `@ "expr"`).
    /// `(0, 0)` means "no span" (the return slot, span-less captures).
    pub span_start: u32,
    pub span_end: u32,
}

/// Everything the crash debugger needs about a function that does **not** vary
/// per call: its name, its source extent, and the metadata for its `Gc` locals.
///
/// One of these exists per lowered function, interned by content in the JIT
/// generation arena (ADR-043), so a debugger session that recompiles the same
/// function on every `p EXPR` (DBG-05) pays for it once. A generated prologue
/// stores its address into a [`DebugFrameEntry`] — one immediate, one store —
/// where ADR-021 passed the same four words as *arguments* to
/// `praxis_push_debug_frame` and a fifth call, `praxis_set_frame_source_span`,
/// wrote a compile-time constant at runtime.
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
    /// `local_count` entries, in shadow-slot order: a local's shadow slot index
    /// doubles as its debug-local index, which is what makes the two stacks
    /// index-parallel for free.
    pub locals: *const DebugLocalMeta,
    /// The function's source span `[start, end)` as byte offsets into the
    /// program source (§9.3 "current source span", ADR-035 decision 3). `(0, 0)`
    /// means "no span recorded" (synthetic/closure functions).
    pub span_start: u32,
    pub span_end: u32,
}

/// One live call's debug frame: which function, and where its value slots are.
///
/// This is the whole of what a frame *is* now. ADR-021's `DebugFrame` was a
/// `Box` with a `parent` pointer, a name, a length, a locals pointer, a count, a
/// span and two reserved parser-path words; six of those nine are static and
/// live in [`FunctionDebugMeta`], the `parent` is the entry below this one on
/// the stack, and the two parser-path fields were null from M10a onward and no
/// `SnapshotFrame` ever carried them.
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
/// It does scan them, after every sweep, and null the ones whose object that
/// sweep reclaimed — see [`DebugFrameStackHeader::clear_reclaimed`] and
/// ADR-106. That is the difference between keeping a value *alive* and keeping
/// a slot *valid*, and only the second is this stack's business.
pub type DebugValueStack = SlotStack<Option<GcRef>>;
/// The header generated code bump-allocates value slots against.
pub type DebugValueStackHeader = SlotStackHeader<Option<GcRef>>;
/// The runtime's one reservation of per-call frame entries.
pub type DebugFrameStack = SlotStack<DebugFrameEntry>;
/// The header generated code bump-allocates frame entries against.
pub type DebugFrameStackHeader = SlotStackHeader<DebugFrameEntry>;

/// The size of the debug value reservation, in slots.
///
/// Exactly the shadow stack's, and for exactly its two reasons: a call claims
/// one slot per `Gc` local, a frame has at most `MAX_SHADOW_SLOTS` of them, and
/// every prologue rejects `recursion_depth >= MAX_RECURSION_DEPTH` *before* it
/// claims anything. So exhaustion is unrepresentable here too, and generated
/// code emits no bounds check. Written as the shadow constant rather than
/// re-derived, because the two must move together: they are indexed by the same
/// slot number for the same local.
///
/// This is a *reservation*, and the weak scan (ADR-106) does not walk it. Its
/// cost is bounded by `top - base` — the slots live calls have actually claimed
/// — so raising this number costs address space and not collection time.
pub const DEBUG_VALUE_STACK_SLOTS: usize = SHADOW_STACK_SLOTS;

/// The size of the debug frame-entry reservation, in slots — one per live call,
/// bounded by the same depth guard, plus the headroom `SHADOW_STACK_SLOTS`
/// keeps for Rust-side pushes.
pub const DEBUG_FRAME_STACK_SLOTS: usize = MAX_RECURSION_DEPTH as usize + 1;

// ---------------------------------------------------------------------------
// The weak arm (ADR-106)
// ---------------------------------------------------------------------------

impl DebugFrameStackHeader {
    /// Null every claimed debug value slot whose object the sweep that just
    /// finished reclaimed, and answer how many were nulled.
    ///
    /// This is the entire content of [`RuntimeRoots`](crate::RuntimeRoots)' one
    /// weak arm. It retains nothing: it runs *after* the mark and the sweep have
    /// already decided what dies, and its only effect is to replace a reference
    /// to storage that no longer holds an object with the absence
    /// `Option<GcRef>` already spells.
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
                // SAFETY: as above; the slot holds an initialized
                // `Option<GcRef>` (a claim zeroes its run, and zero is `None`).
                let Some(r) = (unsafe { *slot }) else {
                    continue;
                };
                if r.header().is_poisoned() {
                    // SAFETY: as above.
                    unsafe { *slot = None };
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

    /// True iff this local is a user-written binding (a `let`/`var`/param/
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
}

/// `DebugLocal.value` and a debug value slot must stay one machine word:
/// generated code stores a raw `GcRef` into each at a fixed offset, and reads
/// the zeroed slot a fresh claim starts with as "no value yet". `Option<GcRef>`
/// is niche-optimized to exactly that (F18); this is the compile-time proof.
const _: () = {
    assert!(std::mem::size_of::<Option<GcRef>>() == std::mem::size_of::<GcRef>());
    assert!(std::mem::size_of::<Option<GcRef>>() == std::mem::size_of::<usize>());
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
/// unrepresentable from Rust. The tests this replaces reached into a
/// `Box<DebugFrame>`'s `locals` array by hand.
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

    /// Two shadowed `a` bindings, as `let a = ...; let a = ...` produces.
    fn shadowed_a_metas() -> [DebugLocalMeta; 2] {
        let name_a = b"a";
        [
            DebugLocalMeta {
                source_name: name_a.as_ptr(),
                name_len: 1,
                symbol_id: 10,
                descriptor: &crate::scalars::INT,
                type_id: 1,
                kind: LOCAL_KIND_USER,
                span_start: 5,
                span_end: 6,
            },
            DebugLocalMeta {
                source_name: name_a.as_ptr(),
                name_len: 1,
                symbol_id: 20,
                descriptor: &crate::scalars::INT,
                type_id: 1,
                kind: LOCAL_KIND_USER,
                span_start: 20,
                span_end: 21,
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

    /// ADR-021's §4.2 guarantee, rebuilt on the metadata's new home. The whole
    /// reason ADR-021 exists is that "shadowed locals are distinguishable in
    /// debugger frames by source name and symbol ID" is testable without a REPL
    /// — so moving the metadata out of the frame must not cost that test, only
    /// change where it looks.
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
        // The span is the function's, and it is static: nothing wrote it at
        // runtime, which is `praxis_set_frame_source_span` not existing.
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
        // The balance property ADR-021's `praxis_pop_debug_frame` had and this
        // must keep: `m10ws2_debug_frame_pushpop_balanced_across_recursion` is
        // its end-to-end form, and `Runtime::clear_for_rerun` asserts on it.
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

    /// ADR-106, the defect it closes. A value the shadow stack has stopped
    /// naming — which is every `Gc` local after its last use, by ADR-044
    /// decision 2 — is unreachable while the debugger still names it. The
    /// collection that reclaims it must leave the debug slot as an absence, not
    /// as a reference into storage the allocator is now free to hand out.
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
        assert_eq!(
            guard.values()[1],
            None,
            "the debug slot still names swept storage; the next allocation \
             reissues that block and the slot then names an object of another \
             type"
        );
        assert_eq!(guard.values()[0], None, "slot 0 was never written");
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
        assert_eq!(outer_guard.values()[0], None);
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
}
