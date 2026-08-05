//! The compiler-managed shadow stack (§12.3, ADR-019, ADR-101).
//!
//! §12.3 offers "compiler-managed shadow-stack frames **or** explicit root
//! frames." M3 shipped explicit root frames ([`RootScope`](crate::RootScope),
//! ADR-012) for the host. M5 added the compiler-managed shadow stack that
//! JIT-generated code spills into: at every GC safepoint (allocation / call
//! that may allocate), the Cranelift backend stores the live `GcRef` locals
//! into this function's slots *before* the safepoint and reloads them after.
//!
//! A **frame is not an object.** The runtime owns one contiguous region of
//! slots for the whole program ([`SlotStack`]); a function's frame is the run
//! of slots between the `top` it found on entry and the `top` it left behind.
//! Generated code claims a run by bump-allocating inline — load `top`, zero
//! `slot_count` slots, store `top + slot_count*8` back — and reclaims it by
//! storing the saved base into `top` again. No allocation, no free, no call.
//! ADR-101 records why this replaced a per-call `Box<ShadowFrame>` of
//! `MAX_SHADOW_SLOTS` pointers, 1536 bytes of which were memset to null on
//! every Praxis call whatever the function's real slot count.
//!
//! Slots are raw `*mut GcHeader` (not [`GcRef`]) because a slot is *null until
//! the backend writes a value into it*: a local may be live across a safepoint
//! (so it must be in the root set) before it has ever been assigned at runtime.
//! `GcRef` is `NonNull` by construction, so it cannot represent that state; the
//! raw pointer can, and `push_roots` skips nulls.
//!
//! The collector reaches the whole stack through one [`RootSet`] impl on
//! [`SlotStackHeader`], which scans `[base, top)` in a single linear pass. That
//! is exactly the set the old parent-pointer chain yielded, because each frame
//! occupies exactly its own slot run and the runs partition `[base, top)`.
//!
//! The header is `#[repr(C)]` and publishes [`SlotStackHeader::TOP_OFFSET`] so
//! the backend emits a compile-time-derived displacement rather than a literal
//! (Appendix B).

use crate::gc::{GcHeader, GcRef};
use crate::roots::RootSet;
use crate::{
    FRAME_BYTES_BASE, FRAME_BYTES_PER_SLOT, MAX_RECURSION_DEPTH, REFERENCE_FRAME_SLOTS,
    STACK_BUDGET_BYTES,
};

/// The maximum `Gc` roots a single JIT'd function may spill. The backend
/// rejects (at compile time) any function exceeding this, through
/// [`SlotCount`]; real Praxis functions have small root sets.
///
/// Under ADR-019's per-call `Box<ShadowFrame>` this constant was the *width of
/// every allocation*, so raising it made every call in the language slower —
/// the handover measured 192 → 24 taking a no-op-call benchmark from 1.43 s to
/// 1.27 s on its own. Under the contiguous stack it costs no per-call time at
/// all.
///
/// **It is not what bounds the stack, and it has not been a performance dial
/// since ADR-101** (ADR-128 amends the account this comment used to give). The
/// budget guard bounds every claimed slot on its own — see [`SHADOW_STACK_SLOTS`]
/// — and this constant contributes only that reservation's headroom term for
/// Rust-side pushes. It appears in exactly two places that survive to run time:
/// the size of the reservation, and [`SlotCount::new`], which is a compile-time
/// check. **It appears in no generated code.** Raising it changes the cost of no
/// program that compiles today; it changes only which programs compile.
///
/// Since ADR-128 a frame's root width is the number of *colors* its co-live root
/// sets need, not its count of `Gc` locals, and over all 71 functions of
/// `tests/aoc-corpus` the largest co-live root set is 11 — which is
/// [`REFERENCE_FRAME_SLOTS`]. So this cap keeps its value and its meaning and
/// simply stops being reachable. What a programmer can still exhaust is
/// [`MAX_DEBUG_VALUE_SLOTS`], which bounds the thing they can see: how many `Gc`
/// locals one function may have.
///
/// This is part of the contract between the backend and the runtime; bumping it
/// is an ABI-affecting change caught by the ABI version check (§11.6) only in
/// that the two are rebuilt together.
///
/// M8 raises this from 64 to 192 to accommodate AoC-style graph programs that
/// allocate many collections (Deque/Set/Map/Vec) in a single frame.
pub const MAX_SHADOW_SLOTS: usize = 192;

/// The maximum `Gc` locals a single JIT'd function may have — the bound on the
/// *dense* index space the crash debugger reads (ADR-128 decision 3).
///
/// Until ADR-128 this was [`MAX_SHADOW_SLOTS`], because one index space served
/// both: a local's shadow slot index doubled as its debug-local index. Colouring
/// the root slots by live range ends that. The two spaces now answer different
/// questions and are sized differently — root slots are as many as a function's
/// co-live root sets need, debug value slots are one per `Gc` local, in MIR local
/// order, so that the debugger can render a local the program has finished with.
///
/// So the dense space needs its own bound, and it is sized for the thing it now
/// limits: how many `Gc` locals a function may have, which is a property of the
/// source text a programmer can see and can act on. 192 was closer to biting than
/// anyone noticed — `bfs`'s and `vm`'s entry points are already at 178 and 185,
/// and a 40-line function of twenty `var v = [1, 2, 3]` / `out(v.len())` pairs
/// did not compile at all, while its largest co-live root set was **2**.
///
/// The cost of the headroom is address space and nothing else, and it is **one**
/// reservation's, not two: [`MAX_SHADOW_SLOTS`] keeps its value, so
/// [`SHADOW_STACK_SLOTS`] is unchanged at 624,192 slots and only
/// [`DEBUG_VALUE_STACK_SLOTS`](crate::debug::DEBUG_VALUE_STACK_SLOTS) grows —
/// 624,192 → 628,096, which is `(4096 − 192) × 8` = **30.5 KiB**. (ADR-128's own
/// text prices it at ~64 KB by charging both reservations; that is the one
/// number in the record that is wrong.) [`SlotStack::new`] allocates zeroed,
/// which for the shadow stack's raw pointers is an `mmap` of untouched zero
/// pages, so resident memory tracks how wide programs actually are.
pub const MAX_DEBUG_VALUE_SLOTS: usize = 4096;

/// A frame width the shadow stack can actually hold: a `u32` proven `<=`
/// [`MAX_SHADOW_SLOTS`] at construction.
///
/// This replaces the runtime `assert!` in the deleted `ShadowFrame::new`. An
/// over-wide frame used to be a panic inside the prologue helper — a state the
/// program could reach and the runtime then had to reject. It is now
/// *unconstructible*: [`SlotCount::new`] is the only way to make one, the
/// backend turns the `None` into a compile diagnostic naming the function, and
/// every consumer of a `SlotCount` may assume the bound without re-checking it.
/// That assumption is load-bearing — it is one of the two premises of
/// [`SHADOW_STACK_SLOTS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotCount(u32);

impl SlotCount {
    /// `Some` iff `n` slots fit in one frame. `const` so a caller can prove a
    /// literal width at compile time.
    #[must_use]
    pub const fn new(n: u32) -> Option<SlotCount> {
        if n as usize <= MAX_SHADOW_SLOTS {
            Some(SlotCount(n))
        } else {
            None
        }
    }

    /// The width, which is `<= MAX_SHADOW_SLOTS` by construction.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A count of debug value slots the debug value stack can actually hold: a `u32`
/// proven `<=` [`MAX_DEBUG_VALUE_SLOTS`] at construction (ADR-128 decision 3).
///
/// **A distinct type from [`SlotCount`], and that is the point.** Before ADR-128
/// one count was both — the claim on the shadow stack and the claim on the debug
/// value stack were the same number, so one type served. They are now different
/// numbers with different bounds over different index spaces, and the failure
/// mode of confusing them is silent: claiming the *colored* width on the debug
/// stack would give the debugger a frame too short for its own metadata to index,
/// and every local past the end would render another frame's value. Making them
/// two types means that mix-up does not compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DebugSlotCount(u32);

impl DebugSlotCount {
    /// `Some` iff `n` `Gc` locals fit in one function's debug frame.
    #[must_use]
    pub const fn new(n: u32) -> Option<DebugSlotCount> {
        if n as usize <= MAX_DEBUG_VALUE_SLOTS {
            Some(DebugSlotCount(n))
        } else {
            None
        }
    }

    /// The width, which is `<= MAX_DEBUG_VALUE_SLOTS` by construction.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// The size of the one shadow-stack reservation, in slots.
///
/// **Exhaustion is unrepresentable, not handled.** One fact bounds the stack,
/// and since ADR-105 it bounds it *exactly* rather than through a product of two
/// independent worst cases:
///
/// - every generated prologue rejects `stack_left < frame_cost(slots)` *before*
///   it pushes anything. A context starts with at most [`STACK_BUDGET_BYTES`] —
///   [`StackBudget`] refuses to make a larger one — and a frame spends at least
///   [`FRAME_BYTES_BASE`], so there are at most [`MAX_RECURSION_DEPTH`] live
///   frames; and it spends [`FRAME_BYTES_PER_SLOT`] on every slot past
///   [`REFERENCE_FRAME_SLOTS`], so those slots number at most
///   `STACK_BUDGET_BYTES / FRAME_BYTES_PER_SLOT`. Adding the two: live slots are
///   bounded by `budget / FRAME_BYTES_PER_SLOT + MAX_RECURSION_DEPTH ×
///   REFERENCE_FRAME_SLOTS`.
///
/// Reserving that — plus one frame of headroom for the Rust-side [`push_frame`]
/// callers, which spend no budget and so are not covered by the argument —
/// means there is no inline bounds check in the prologue, because there is
/// nothing left to check. That is one branch removed from the hottest path in
/// the language.
///
/// The old bound was `MAX_RECURSION_DEPTH * MAX_SHADOW_SLOTS`, which multiplied
/// "the deepest recursion" by "the widest frame" as if a program could have both
/// at once. It cannot, and now the guard is what says so: a maximum-width frame
/// spends `FRAME_BYTES_BASE + 2 × (MAX_SHADOW_SLOTS − REFERENCE_FRAME_SLOTS)`
/// bytes, so a stack of them runs out of budget at 2161 frames, not 8000. The
/// reservation falls from 12.29 MiB to 4.77 MiB of *virtual address space* per
/// [`Runtime`](crate::Runtime). [`SlotStack::new`] allocates it zeroed, which is
/// an `mmap` of fresh zero pages: resident memory tracks how deep the program
/// actually recurses, not how deep it is allowed to.
pub const SHADOW_STACK_SLOTS: usize = (STACK_BUDGET_BYTES / FRAME_BYTES_PER_SLOT) as usize
    + (MAX_RECURSION_DEPTH * REFERENCE_FRAME_SLOTS) as usize
    + MAX_SHADOW_SLOTS;

/// The bound the reservation above covers, spelled once so the `const` block and
/// the test can both read it rather than restating the arithmetic.
///
/// `pub(crate)` since ADR-128: it bounds the *debug value* stack too, and for the
/// same reason rather than by coincidence. [`crate::frame_cost`] charges the
/// dense count of `Gc` locals (decision 4), which is exactly that stack's width,
/// so the budget argument that bounds the claimed shadow slots bounds the claimed
/// debug value slots by the identical arithmetic. The shadow stack's own claim is
/// no wider — a colored width is at most the dense one — so this covers both.
pub(crate) const MAX_LIVE_SLOTS: usize = (STACK_BUDGET_BYTES / FRAME_BYTES_PER_SLOT) as usize
    + (MAX_RECURSION_DEPTH * REFERENCE_FRAME_SLOTS) as usize;

// The capacity identity, restated so a build cannot disagree with it. It is
// deliberately spelled without reference to how `SHADOW_STACK_SLOTS` is
// computed: the hazard is not someone raising the budget (the reservation
// follows it, and `StackBudget` refuses anyway), it is someone deciding the
// address space is too much and writing a smaller number. That edit makes
// shadow-stack overflow reachable from generated code — silently, because
// generated code does not check the limit — and this fails the *build* instead.
// Same discipline as ADR-040's P0-08c: a sizing error is a compile error, not a
// test run.
const _: () = assert!(
    SHADOW_STACK_SLOTS > MAX_LIVE_SLOTS,
    "the shadow stack must cover every slot the budget can buy, plus one frame \
     of headroom for Rust-side pushes"
);

// The two premises that argument rests on, stated where the reservation is
// sized rather than left to the reader of `frame_cost`: a frame must spend
// something (or there is no bound on how many are live), and a slot past the
// reference width must spend something (or there is no bound on how many are
// claimed).
const _: () = assert!(
    FRAME_BYTES_BASE > 0 && FRAME_BYTES_PER_SLOT > 0,
    "a frame and a slot must each spend budget, or the reservation argument in \
     SHADOW_STACK_SLOTS does not close"
);

/// The three-word header generated code bump-allocates against.
///
/// `#[repr(C)]` and pointer-shaped rather than index-shaped: the prologue's
/// whole job is `top` → zero → `top + n*8`, and holding raw addresses makes
/// that three instructions with no base-plus-scaled-index arithmetic.
///
/// The fields stay private and the backend reaches `top` through
/// [`Self::TOP_OFFSET`]. That is strictly better than making the field `pub`
/// (which is what `ShadowFrame.slots` had to be, only so the backend could
/// `offset_of!` it): the displacement is still derived from the `#[repr(C)]`
/// layout at compile time, but no other crate can write the field.
///
/// Generic in the slot type because the mechanism is not specific to GC roots;
/// see [`SlotStack`].
#[repr(C)]
pub struct SlotStackHeader<T: Copy> {
    /// One past the last claimed slot — where the next frame starts.
    top: *mut T,
    /// The first slot of the reservation. Never moves.
    base: *mut T,
    /// One past the last slot of the reservation. Never moves. Read only by
    /// [`push_frame`]; generated code does not check it (see
    /// [`SHADOW_STACK_SLOTS`]).
    limit: *mut T,
}

impl<T: Copy> SlotStackHeader<T> {
    /// The byte offset of `top` within the header, for the backend to emit as a
    /// load/store displacement. Computed from the `#[repr(C)]` layout so it
    /// stays correct if the struct evolves.
    pub const TOP_OFFSET: i32 = core::mem::offset_of!(Self, top) as i32;

    /// Claim `n` slots, all set to `zero`, and answer the base of the run: the
    /// Rust-side form of the bump a generated prologue emits inline.
    ///
    /// Crate-private, and deliberately so — the public doors are
    /// [`push_frame`] and [`crate::debug::push_frame`], which hand back a guard
    /// that restores `top` on drop. A caller holding a bare base could forget.
    ///
    /// # Safety
    /// The [`SlotStack`] this header belongs to must be live.
    ///
    /// # Panics
    /// If the run does not fit. This is the one place the reservation's limit is
    /// checked at runtime: Rust callers do not pass the prologue's depth guard,
    /// so the argument in [`SHADOW_STACK_SLOTS`] does not cover them.
    pub(crate) unsafe fn claim(&mut self, n: usize, zero: T) -> *mut T {
        let base = self.top;
        // SAFETY: `top` is inside the reservation and `n` slots past it is at
        // worst one-past-the-end once the assertion below passes.
        let new_top = unsafe { base.add(n) };
        assert!(
            new_top <= self.limit,
            "slot stack exhausted: {n} more slots do not fit"
        );
        // SAFETY: `[base, new_top)` is inside the live reservation.
        unsafe { std::slice::from_raw_parts_mut(base, n) }.fill(zero);
        self.top = new_top;
        base
    }

    /// Restore `top` to `base`, releasing everything claimed since.
    ///
    /// An absolute, not a subtraction: it cannot underflow, and an imbalance
    /// introduced below this frame is corrected here rather than propagated.
    pub(crate) fn restore(&mut self, base: *mut T) {
        self.top = base;
    }

    /// Every frame currently on the stack, concatenated — the collector's door
    /// for the shadow instantiation, and the crash snapshot's for the debug
    /// ones.
    #[must_use]
    pub fn claimed(&self) -> &[T] {
        self.live_slots()
    }

    /// The slots between `base` and `top`: every frame currently on the stack,
    /// concatenated.
    fn live_slots(&self) -> &[T] {
        debug_assert!(self.base <= self.top && self.top <= self.limit);
        // SAFETY: `base` heads a `Box<[T]>` owned by the `SlotStack` that also
        // owns this header, and `top` never leaves `[base, limit]` — generated
        // code only ever stores back a value it loaded from `top`, or that
        // value plus a `SlotCount`-bounded bump, and `SHADOW_STACK_SLOTS` is
        // sized so the bump cannot pass `limit`.
        unsafe {
            let len = self.top.offset_from(self.base) as usize;
            std::slice::from_raw_parts(self.base, len)
        }
    }

    /// How many slots are currently claimed. Zero between runs, if every
    /// prologue was balanced by an epilogue.
    #[must_use]
    pub fn len(&self) -> usize {
        self.live_slots().len()
    }

    /// True iff no frame is on the stack.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The owner of one slot reservation and its header.
///
/// Two allocations, both made once and never resized. **Not a `Vec`**: a `Vec`
/// that reallocated would invalidate the base pointer generated code holds in a
/// Cranelift `Variable` for the duration of a call — a use-after-free reachable
/// from any Praxis program deep enough to trigger the growth. A `Box<[T]>`
/// allocated at its final size cannot move.
///
/// The header is separately boxed so [`Self::header_ptr`] survives a `Runtime`
/// move: `Runtime::new` returns by value, and generated code holds the address
/// this hands out for the whole program.
///
/// Generic in `T` because the mechanism is not specific to GC roots. The same
/// shape serves any per-frame array of `Copy` slots whose zero value means
/// "nothing here yet" — the crash debugger's per-frame locals
/// (`SlotStack<Option<GcRef>>`, whose zero *is* `None` by the `NonNull` niche,
/// F18) being the one this was built with in view. Two such stacks are
/// index-parallel for free, because a local's shadow slot index already doubles
/// as its debug-local index.
pub struct SlotStack<T: Copy> {
    header: Box<SlotStackHeader<T>>,
    slots: Box<[T]>,
}

impl<T: Copy> SlotStack<T> {
    /// Reserve `capacity` slots, all set to `zero`, and point a header at them.
    ///
    /// `zero` is a parameter rather than a `Default` bound so the caller names
    /// the all-zero value — and because that is what lets this lower to a
    /// single `alloc_zeroed`. `vec![zero; n]` hits std's `IsZero`
    /// specialization for raw pointers, so the 12.29 MiB shadow reservation is
    /// an `mmap` of untouched zero pages rather than a 12 MiB memset at every
    /// `Runtime::new()`. An instantiation whose `zero` std does not recognise
    /// as all-zero-bytes still works; it pays that memset.
    #[must_use]
    pub fn new(capacity: usize, zero: T) -> Self {
        // Build the storage first: `base`/`limit` must be addresses inside the
        // final allocation, so nothing may move after they are taken.
        let mut slots: Box<[T]> = vec![zero; capacity].into_boxed_slice();
        let base = slots.as_mut_ptr();
        // SAFETY: `capacity` elements were just allocated at `base`, so
        // one-past-the-end is a valid pointer to form.
        let limit = unsafe { base.add(capacity) };
        SlotStack {
            header: Box::new(SlotStackHeader {
                top: base,
                base,
                limit,
            }),
            slots,
        }
    }

    /// The address generated code bump-allocates against. Stable for the life
    /// of this `SlotStack`, including across moves of whatever owns it.
    pub fn header_ptr(&mut self) -> *mut SlotStackHeader<T> {
        &mut *self.header
    }

    /// Borrow the header — the collector's door, and the tests'.
    #[must_use]
    pub fn header(&self) -> &SlotStackHeader<T> {
        &self.header
    }

    /// Drop every frame. Only correct between runs: a generated epilogue that
    /// later restored its saved base would undo this.
    pub fn reset(&mut self) {
        self.header.top = self.header.base;
    }

    /// How many slots are currently claimed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.header.len()
    }

    /// True iff no frame is on the stack.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.header.is_empty()
    }

    /// The reservation's capacity in slots.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }
}

/// The whole shadow stack, as the runtime owns it.
pub type ShadowStack = SlotStack<*mut GcHeader>;
/// The header generated code bump-allocates against.
pub type ShadowStackHeader = SlotStackHeader<*mut GcHeader>;

// Nothing *depends* on `top` being first — the backend emits `TOP_OFFSET`,
// whatever it is. But a header whose hottest field is not at displacement zero
// is a layout mistake worth noticing at build time rather than in a profile.
const _: () = assert!(ShadowStackHeader::TOP_OFFSET == 0);

impl RootSet for ShadowStackHeader {
    /// One linear pass over every claimed slot.
    ///
    /// This yields *exactly* the set ADR-019's chain walk yielded: each frame
    /// occupies exactly its own `slot_count` slots and the frames partition
    /// `[base, top)`, so the concatenation is the union of every live frame's
    /// `slots[..slot_count]`. What it does not do is what the chain walk did on
    /// the way — `ShadowFrame::push_roots` recursed the parent pointers and
    /// called a `live_refs()` that **allocated a fresh `Vec<GcRef>` per frame**,
    /// so a collection 8000 frames deep was 8000 mallocs and 8000 levels of
    /// native recursion *inside* `Heap::mark`.
    ///
    /// Slots *above* `top` may still hold pointers a popped frame wrote. That is
    /// harmless, and needs no invariant to make it so: they are never scanned,
    /// and the next push zeroes exactly the run it claims before any safepoint
    /// can read it.
    fn push_roots(&self, out: &mut Vec<GcRef>) {
        out.extend(self.live_slots().iter().copied().filter_map(|p| {
            // SAFETY: a non-null slot was written by generated code (or by
            // `ShadowFrameGuard::set`) with a live allocation's header pointer.
            std::ptr::NonNull::new(p).map(|nn| unsafe { GcRef::from_non_null(nn) })
        }));
    }
}

// ---------------------------------------------------------------------------
// The Rust-side push. Generated code does this inline; this is for the
// runtime's own tests and for any host that wants to root as a prologue does.
// ---------------------------------------------------------------------------

/// A frame claimed from Rust, released when dropped.
///
/// The RAII shape is what makes "write past your frame" unrepresentable from
/// Rust: [`Self::set`] and [`Self::clear`] are bounds-checked against the width
/// the frame was pushed with, and the only way to restore `top` is to drop the
/// guard — so a frame cannot outlive its slots or be popped twice. The tests
/// this replaces reached into `(*frame).slots[i]` by hand.
pub struct ShadowFrameGuard {
    header: *mut ShadowStackHeader,
    /// This frame's first slot, and the `top` the drop restores.
    base: *mut *mut GcHeader,
    count: u32,
}

impl ShadowFrameGuard {
    /// Root `r` in slot `index`.
    ///
    /// # Panics
    /// If `index` is outside the frame. Writing another frame's slot would make
    /// the collector's view of *that* frame wrong, which is not a condition the
    /// caller could detect afterwards.
    pub fn set(&mut self, index: usize, r: GcRef) {
        assert!(
            index < self.count as usize,
            "shadow slot {index} is outside a {}-slot frame",
            self.count
        );
        // SAFETY: `index` is inside the run claimed by `push_frame`, which is
        // live until this guard drops.
        unsafe { *self.base.add(index) = r.as_ptr() };
    }

    /// Un-root slot `index` (MIR-01: a dead slot must not keep its object
    /// reachable, and once swept storage became reusable a stale slot could
    /// name a live object of an entirely different type).
    ///
    /// # Panics
    /// If `index` is outside the frame.
    pub fn clear(&mut self, index: usize) {
        assert!(
            index < self.count as usize,
            "shadow slot {index} is outside a {}-slot frame",
            self.count
        );
        // SAFETY: as `set`.
        unsafe { *self.base.add(index) = std::ptr::null_mut() };
    }

    /// This frame's first slot, for a caller that wants to observe the slot
    /// memory the way generated code addresses it.
    #[must_use]
    pub fn base_ptr(&self) -> *mut *mut GcHeader {
        self.base
    }
}

impl Drop for ShadowFrameGuard {
    fn drop(&mut self) {
        // Restore the absolute base rather than subtracting `count`, for the
        // same reason the generated epilogue does: an imbalance introduced by
        // anything that ran inside this frame cannot leak past it, and there is
        // no subtraction to underflow.
        // SAFETY: `header` was non-null when the guard was made, and belongs to
        // a runtime the caller guaranteed outlives it.
        unsafe { (*self.header).restore(self.base) };
    }
}

/// Claim `count` zeroed slots on `ctx`'s shadow stack, the way a generated
/// prologue does.
///
/// This is the one place the reservation's limit is checked at runtime: Rust
/// callers do not go through the prologue's depth guard, so the argument in
/// [`SHADOW_STACK_SLOTS`] does not cover them.
///
/// # Safety
/// `ctx` must point at a live context wired by
/// [`Runtime::context`](crate::Runtime::context), and the runtime that owns the
/// stack must outlive the returned guard.
///
/// # Panics
/// If `ctx` is null, its `shadow` header is null, or the frame would not fit.
#[must_use]
pub unsafe fn push_frame(ctx: *mut crate::RuntimeContext, count: SlotCount) -> ShadowFrameGuard {
    assert!(!ctx.is_null(), "push_frame needs a wired context");
    // SAFETY: the caller guarantees `ctx` is live.
    let header = unsafe { (*ctx).shadow };
    assert!(
        !header.is_null(),
        "push_frame needs a context from `Runtime::context`, not a placeholder"
    );
    let n = count.get() as usize;
    // SAFETY: `header` is non-null and owned by a live `SlotStack`, so `claim`
    // may bump it; it checks the reservation's limit itself.
    let base = unsafe { (*header).claim(n, std::ptr::null_mut()) };
    ShadowFrameGuard {
        header,
        base,
        count: count.get(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::NonNull;

    fn dummy_ref() -> GcRef {
        // A leaked header; only its non-null address matters for root-walking
        // tests. The collector is never run against these.
        let header = Box::leak(Box::new(GcHeader::detached()));
        // SAFETY: `header` is leaked, aligned, and live for the process.
        unsafe { GcRef::from_non_null(NonNull::from(header)) }
    }

    /// A stack plus a context wired to it, the way `Runtime` wires one.
    struct Fixture {
        stack: ShadowStack,
        ctx: Box<crate::RuntimeContext>,
    }

    impl Fixture {
        fn new() -> Fixture {
            let mut stack = ShadowStack::new(SHADOW_STACK_SLOTS, std::ptr::null_mut());
            // SAFETY: no generated code runs against this context and nothing
            // dereferences `input_source`; only `shadow` is read.
            let mut ctx = Box::new(unsafe { crate::RuntimeContext::placeholder(dummy_ref()) });
            ctx.shadow = stack.header_ptr();
            Fixture { stack, ctx }
        }

        fn ctx_ptr(&mut self) -> *mut crate::RuntimeContext {
            &mut *self.ctx
        }

        fn roots(&self) -> Vec<GcRef> {
            let mut out = Vec::new();
            self.stack.header().push_roots(&mut out);
            out
        }
    }

    #[test]
    fn an_empty_stack_roots_nothing() {
        let f = Fixture::new();
        assert!(f.roots().is_empty());
        assert!(f.stack.is_empty());
    }

    #[test]
    fn a_frame_yields_written_slots_only() {
        // The test that pins "null means not-yet-written" — the reason a slot
        // is a raw `*mut GcHeader` and not a `GcRef`.
        let mut f = Fixture::new();
        let a = dummy_ref();
        let b = dummy_ref();
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.stack`, which outlives the guard.
        let mut guard = unsafe { push_frame(ctx, SlotCount::new(3).unwrap()) };
        guard.set(0, a);
        guard.set(2, b); // slot 1 left null
        let out = f.roots();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].as_ptr(), a.as_ptr());
        assert_eq!(out[1].as_ptr(), b.as_ptr());
        drop(guard);
    }

    #[test]
    fn nested_frames_are_one_contiguous_scan() {
        // ADR-019's parent pointer is gone: the collector no longer walks a
        // chain, it reads `[base, top)` once and gets the same set.
        let mut f = Fixture::new();
        let a = dummy_ref();
        let b = dummy_ref();
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.stack`, which outlives both guards.
        let (outer, inner) = unsafe {
            let mut outer = push_frame(ctx, SlotCount::new(1).unwrap());
            outer.set(0, a);
            let mut inner = push_frame(ctx, SlotCount::new(1).unwrap());
            inner.set(0, b);
            (outer, inner)
        };
        let out = f.roots();
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|r| r.as_ptr() == a.as_ptr()));
        assert!(out.iter().any(|r| r.as_ptr() == b.as_ptr()));
        drop(inner);
        drop(outer);
    }

    #[test]
    fn a_popped_frames_slots_are_not_scanned() {
        // Slots above `top` still hold what the popped frame wrote. Nothing
        // clears them, and nothing needs to: they are outside `[base, top)`,
        // and the next push zeroes what it claims.
        let mut f = Fixture::new();
        let a = dummy_ref();
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.stack`, which outlives the guard.
        let base = unsafe {
            let mut guard = push_frame(ctx, SlotCount::new(1).unwrap());
            guard.set(0, a);
            let base = guard.base_ptr();
            drop(guard);
            base
        };
        // SAFETY: `base` is inside the live reservation; that reading a stale
        // slot is harmless is exactly what this test asserts.
        assert_eq!(unsafe { *base }, a.as_ptr(), "the slot memory is untouched");
        assert!(f.roots().is_empty(), "but it is outside [base, top)");
    }

    #[test]
    fn pushing_and_popping_restores_the_top() {
        let mut f = Fixture::new();
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.stack`, which outlives the guards.
        unsafe {
            let outer = push_frame(ctx, SlotCount::new(4).unwrap());
            assert_eq!(f.stack.len(), 4);
            {
                let inner = push_frame(ctx, SlotCount::new(7).unwrap());
                assert_eq!(f.stack.len(), 11);
                drop(inner);
            }
            assert_eq!(f.stack.len(), 4, "an inner pop restores the outer extent");
            drop(outer);
        }
        assert!(f.stack.is_empty(), "every push is balanced by a pop");
    }

    #[test]
    fn a_zero_slot_frame_moves_nothing() {
        // The counterexample that keeps the prologue guard alive: a function
        // with no `Gc` locals consumes no slots, so it can recurse without limit
        // while `top` never moves. A full shadow stack is therefore *not* a
        // cleaner encoding of stack overflow — the guard's budget bounds the
        // native stack, which the shadow stack knows nothing about. It is also
        // why `frame_cost` has a base at all: a frame charged only for its slots
        // would charge this one nothing.
        let mut f = Fixture::new();
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.stack`, which outlives the guards.
        unsafe {
            let guards: Vec<ShadowFrameGuard> = (0..1000)
                .map(|_| push_frame(ctx, SlotCount::new(0).unwrap()))
                .collect();
            assert!(f.stack.is_empty(), "1000 frames claimed nothing");
            drop(guards);
        }
        assert!(f.stack.is_empty());
    }

    #[test]
    fn rejects_an_oversized_frame() {
        // This used to be a `#[should_panic]` against `ShadowFrame::new`. The
        // panic disappearing is the invariant getting stronger: an over-wide
        // frame moved from "rejected at runtime" to "unconstructible".
        assert!(SlotCount::new(MAX_SHADOW_SLOTS as u32).is_some());
        assert!(SlotCount::new((MAX_SHADOW_SLOTS + 1) as u32).is_none());
    }

    #[test]
    fn the_reservation_covers_every_slot_the_budget_can_buy() {
        // The readable form of the `const` block above, and the arithmetic that
        // makes it exact rather than a product of two worst cases. Whatever mix
        // of widths a program recurses through, every slot it claims was paid
        // for out of one budget, so the slots alone can never outrun it.
        for width in [0u32, 1, 7, 64, MAX_SHADOW_SLOTS as u32] {
            let per_frame = crate::frame_cost(width);
            let frames = STACK_BUDGET_BYTES / per_frame;
            let slots = frames as usize * width as usize;
            assert!(
                slots <= MAX_LIVE_SLOTS,
                "a stack of {frames} frames {width} slots wide claims {slots} \
                 slots, past the {MAX_LIVE_SLOTS}-slot bound"
            );
        }
        let stack = ShadowStack::new(SHADOW_STACK_SLOTS, std::ptr::null_mut());
        assert_eq!(stack.capacity(), SHADOW_STACK_SLOTS);
    }

    #[test]
    fn a_wide_frame_spends_more_budget_than_a_narrow_one() {
        // The whole content of ADR-105, as arithmetic: the guard's addend now
        // varies with the frame, so the deepest recursion a program reaches
        // falls as its frames widen. A call count could not express this — and
        // the factor it could not express is the factor by which the measured
        // native frames actually differ.
        let reference = STACK_BUDGET_BYTES / crate::frame_cost(REFERENCE_FRAME_SLOTS);
        let widest = STACK_BUDGET_BYTES / crate::frame_cost(MAX_SHADOW_SLOTS as u32);
        assert_eq!(
            STACK_BUDGET_BYTES / crate::frame_cost(0),
            MAX_RECURSION_DEPTH,
            "the cheapest frame there is must not buy more calls than the debug \
             frame stack has entries — that stack is sized MAX_RECURSION_DEPTH + 1"
        );
        assert_eq!(
            reference, MAX_RECURSION_DEPTH,
            "a reference-width frame reaches exactly the depth the old call \
             count allowed, so an ordinary recursive program is unaffected"
        );
        assert!(
            widest * 3 < reference,
            "the widest legal frame must be several times dearer than the \
             reference one: {widest} vs {reference}"
        );
    }

    #[test]
    fn a_budget_larger_than_the_reservation_cannot_be_built() {
        // The seal. `SHADOW_STACK_SLOTS` is sized from `STACK_BUDGET_BYTES`, so
        // a host that could install a bigger budget would make shadow-stack
        // overflow reachable from generated code, which does not check.
        assert!(crate::StackBudget::new(STACK_BUDGET_BYTES).is_some());
        assert!(crate::StackBudget::new(STACK_BUDGET_BYTES + 1).is_none());
        assert_eq!(crate::StackBudget::DEFAULT.get(), STACK_BUDGET_BYTES);
    }
}
