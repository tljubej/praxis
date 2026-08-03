//! Explicit root frames (§12.3, ADR-012) and the composite runtime root set.
//!
//! §12.3 offers "compiler-managed shadow-stack frames **or** explicit root
//! frames." M3 shipped explicit root frames (ADR-012): a [`RootSet`] is
//! anything that can enumerate the `GcRef`s it keeps alive, and a RAII
//! [`RootScope`] holds a `Vec<GcRef>` and chains to an optional parent.
//!
//! That left the collector's root set open: whoever called `Heap::collect`
//! chose what to root, and the automatic path chose only `ctx.shadow` — so
//! `input_source`, a parse failure's partial value, a runtime-owned crash
//! snapshot and everything native code held in a Rust local were invisible to
//! automatic GC (P0-06). [`RuntimeRoots`] closes that: it is the only thing
//! `Heap::collect` accepts, it is constructible only from a `*mut
//! RuntimeContext`, and it is exhaustive over its six arms — five **strong**,
//! enumerated by its [`RootSet`] impl, and one **weak**, cleared by its
//! [`WeakSet`] impl. "Collect against a partial root set" has no
//! representation.
//!
//! [`NativeScope`] is the fifth strong arm. Native code that builds a value
//! across an allocation — the grid helpers assembling a `Vec` of points, the
//! parser interpreter assembling a record — holds it in a `Rooted`, which is
//! the only input the `&mut Payload` accessors take (P0-07). Holding a payload
//! reference across a safepoint without rooting its owner no longer
//! type-checks. The references themselves live in **one** contiguous
//! [`NativeRootStore`] the runtime owns (ADR-114); a scope is the run of
//! entries above the watermark it found, not an object.
//!
//! The sixth arm is the crash debugger's frames, and it is weak (ADR-106): the
//! collector never traces it — tracing it would re-merge the two sets ADR-044
//! split and undo MIR-01 — but it does *scan* it, once per collection,
//! immediately after the sweep, nulling every debug slot whose object that
//! sweep reclaimed. A debug value is therefore always a live object or an
//! absence, never a dangling reference.

use std::cell::RefCell;
use std::marker::PhantomData;

use crate::context::RuntimeContext;
use crate::GcRef;

/// Anything that can enumerate the GC references it keeps alive (§12.3).
///
/// The collector treats every yielded `GcRef` (plus everything transitively
/// reachable through it) as a root.
pub trait RootSet {
    /// Push every root held by this set into `out`, in any order.
    fn push_roots(&self, out: &mut Vec<GcRef>);
}

/// A no-roots impl so the top-level scope can be rooted on `()`.
impl RootSet for () {
    fn push_roots(&self, _out: &mut Vec<GcRef>) {}
}

/// A set the collector keeps **valid** without keeping **alive** (ADR-106).
///
/// A [`RootSet`] answers "what must survive". This answers the other question,
/// which the collector had no way to ask before: "what names storage, but has
/// no say in whether that storage survives". Such a set is never traced, so it
/// retains nothing; instead it is scanned once per collection, immediately
/// after the sweep, and every entry naming reclaimed storage is turned into an
/// absence rather than left as a dangling reference.
///
/// The one implementor that matters is [`RuntimeRoots`], whose weak arm is the
/// crash debugger's per-call value slots. Those deliberately outlive the shadow
/// slots that root them — ADR-044 decision 2 nulls a shadow slot the moment its
/// local dies (MIR-01), while MIR-16 requires the debugger to keep rendering
/// the value — so between the death and the fault the debugger names something
/// the collector is free to reclaim, and after RT-01 made swept storage
/// reusable, free to *reissue as an object of another type*.
///
/// **Weak, not strong, is the whole point.** Rooting those slots strongly is a
/// two-line change and it is the set-merge ADR-044 exists to refuse: it makes
/// the GC root set the over-approximate one again and fails
/// `a_dead_local_stops_being_reachable_from_its_frame` by construction.
///
/// The timing is as load-bearing as the weakness; see
/// [`Heap::collect`](crate::Heap::collect) and ADR-106 decision 2. "Reclaimed"
/// is only observable in the window between the sweep that reclaimed a block
/// and the next allocation that reissues it, so the clear has to happen inside
/// the collection. A filter applied later — at the snapshot, at the render —
/// cannot distinguish a block that died from one that died and came back.
pub trait WeakSet {
    /// Null every entry of this set whose object the just-finished sweep
    /// reclaimed, and answer how many were nulled.
    ///
    /// The count is for tests and for the measurement ADR-106 records; nothing
    /// on the collection path reads it.
    fn clear_reclaimed(&self) -> usize;
}

/// A no-weak-set impl, so the in-crate tests that collect against a bare
/// [`RootScope`] keep their existing call shape. Mirrors `impl RootSet for ()`.
impl WeakSet for () {
    fn clear_reclaimed(&self) -> usize {
        0
    }
}

/// A RAII frame that roots a set of `GcRef`s and optionally chains to a parent
/// [`RootSet`].
///
/// Roots are added via [`RootScope::root`] and dropped automatically when the
/// scope ends. A scope keeps its own roots live; the collector also walks the
/// parent chain, so a nested scope's roots supplement (never replace) its
/// ancestors'.
pub struct RootScope<'a> {
    parent: Option<&'a dyn RootSet>,
    roots: Vec<GcRef>,
}

impl<'a> RootScope<'a> {
    /// A fresh top-level scope with no parent.
    pub fn new() -> Self {
        RootScope {
            parent: None,
            roots: Vec::new(),
        }
    }

    /// A scope that chains onto `parent`; its roots are added to `parent`'s.
    pub fn child(parent: &'a dyn RootSet) -> Self {
        RootScope {
            parent: Some(parent),
            roots: Vec::new(),
        }
    }

    /// Register `gcref` as a root for the lifetime of this scope and return a
    /// copy of it. The returned `GcRef` is kept alive until the scope drops.
    ///
    /// Called for its rooting side-effect; the returned copy is a convenience
    /// for chaining.
    pub fn root(&mut self, gcref: GcRef) -> GcRef {
        self.roots.push(gcref);
        gcref
    }

    /// Number of roots held directly by this scope (excluding the parent chain).
    pub fn root_count(&self) -> usize {
        self.roots.len()
    }
}

impl Default for RootScope<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl RootSet for RootScope<'_> {
    fn push_roots(&self, out: &mut Vec<GcRef>) {
        if let Some(parent) = self.parent {
            parent.push_roots(out);
        }
        out.extend_from_slice(&self.roots);
    }
}

// ---------------------------------------------------------------------------
// The native root store (P0-07, ADR-114)
// ---------------------------------------------------------------------------

/// How many roots the store reserves at [`Runtime::new`](crate::Runtime::new).
///
/// **This is a reservation and not a bound**, which is the whole difference
/// between this and [`SHADOW_STACK_SLOTS`], where exhaustion is unrepresentable
/// rather than handled. ADR-101 could close that arithmetic because both of its
/// factors are constants: depth is capped by ADR-105's byte budget and width by
/// [`SlotCount`]. **Here only the depth half holds.** Every nested scope sits
/// under a Praxis call or a parser-plan node, so the budget bounds it — measured
/// depth is 1 on every benchmark, 2 for `read lines(int)`, 3 for a template
/// inside `lines`. A single scope's *width* is bounded by nothing at all:
/// `walk_lines` (`parser.rs`) opens one scope for a whole `lines(…)` walk and
/// roots one reference per input line, and `praxis_bfs` opens one for an entire
/// search. Measured: **200,001 roots** for a 200,000-line `read lines(int)`, and
/// 119,997 for a 40,000-node `bfs`. No constant covers that, and an `assert!`
/// that tried would turn a large puzzle input into a process abort — the exact
/// failure ADR-105's guard exists to prevent.
///
/// So the store reallocs, and the two populations it serves are 1 and *the
/// input*, with nothing in between: **the reservation is not sized to demand,
/// because there is no demand curve to size to.** 1024 roots is 8 KiB, three
/// orders of magnitude above every bounded program measured and about what a
/// *single* old-style scope's two allocations cost — of which the benchmark
/// suite made tens of millions. It buys the bounded population "one `malloc`
/// per `Runtime`, ever", which is the property this whole change is about.
///
/// **Nothing turns on the number**, and that is worth saying rather than
/// implying: for the unbounded population the growth is `Vec`'s doubling, so the
/// total bytes copied is within a factor of two of the final size whatever the
/// starting capacity, and even `Vec::new()` would cost the bounded population
/// exactly one allocation, at the first `root()` of the program's life. Raising
/// this is a memory decision, not a speed one.
///
/// [`SHADOW_STACK_SLOTS`]: crate::SHADOW_STACK_SLOTS
/// [`SlotCount`]: crate::SlotCount
pub const NATIVE_ROOT_RESERVATION: usize = 1024;

/// Every `GcRef` the runtime's own Rust code is holding across an allocation,
/// in one contiguous array (ADR-114).
///
/// The runtime's own wrappers build values in Rust locals — a result `Vec` that
/// is filled by repeatedly allocating points, a record assembled field by
/// field. Those locals are invisible to the shadow stack, which only generated
/// code writes. There is exactly one store per [`Runtime`](crate::Runtime), it
/// is reachable through [`RuntimeContext::native_roots`], and it is the fifth
/// strong arm of [`RuntimeRoots`].
///
/// **A frame is not an object.** Through ABI v19 each [`NativeScope`] boxed a
/// `NativeRootFrame` carrying a `parent` pointer and its own `Vec`, so every
/// runtime wrapper that rooted anything paid two `malloc`s and two `free`s to do
/// it — on the path of `praxis_vec_push`, `praxis_map_insert` and every other
/// mutating collection primitive in the language. A scope is now the run of
/// entries above the watermark it found on entry, exactly as ADR-101 made a
/// shadow frame the run of slots above the `top` a prologue found.
///
/// Roots are held behind a `RefCell` so [`NativeScope::root`] can take `&self`
/// and several `Rooted` values can be live at once — the common shape, since a
/// helper usually roots its result and then roots each intermediate it builds.
/// Nothing can collect inside the `borrow_mut`: the only thing a push can call
/// is the *system* allocator, on a growth, and that is not a safepoint.
#[derive(Debug)]
pub struct NativeRootStore {
    roots: RefCell<Vec<GcRef>>,
}

impl NativeRootStore {
    /// A store with [`NATIVE_ROOT_RESERVATION`] roots' worth of capacity.
    #[must_use]
    pub fn new() -> NativeRootStore {
        NativeRootStore {
            roots: RefCell::new(Vec::with_capacity(NATIVE_ROOT_RESERVATION)),
        }
    }

    /// How many roots are currently held, across every live scope. Zero between
    /// runs, if every scope was balanced by its `Drop`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.roots.borrow().len()
    }

    /// True iff no scope holds anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The reservation's current capacity, in roots.
    ///
    /// Exposed because it is the observable form of "this program forced the
    /// store to grow": the capacity only ever rises, so a value above
    /// [`NATIVE_ROOT_RESERVATION`] is a realloc that happened, and the growth
    /// path is the one a pointer-shaped watermark would have died on.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.roots.borrow().capacity()
    }

    /// Append one root. The whole of [`NativeScope::root`]'s work.
    #[inline]
    fn push(&self, r: GcRef) {
        self.roots.borrow_mut().push(r);
    }

    /// Drop everything above `watermark`.
    ///
    /// An absolute, not a subtraction, for ADR-101's reason — and `truncate`'s
    /// own no-op-when-already-shorter rule is what makes it *self-healing*: a
    /// scope dropped out of order restores a watermark that is already below the
    /// current length, and the store simply stays where it is. A pop that could
    /// *raise* the length would resurrect entries a live scope had released,
    /// handing the collector references to storage a sweep may already have
    /// reclaimed. There is no spelling for that here.
    #[inline]
    fn truncate(&self, watermark: usize) {
        self.roots.borrow_mut().truncate(watermark);
    }

    /// Drop every root. Only correct between runs, when no scope is live.
    pub(crate) fn reset(&mut self) {
        self.roots.get_mut().clear();
    }
}

impl Default for NativeRootStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RootSet for NativeRootStore {
    /// One `extend_from_slice` over every live scope's roots at once.
    ///
    /// This yields *exactly* the set the parent-pointer walk yielded, for
    /// ADR-101's reason applied to this chain: scopes nest with the Rust stack,
    /// each occupies exactly the run between its own watermark and the next
    /// one's, and the runs partition `[0, len)`. What it does not do is walk a
    /// linked list and `extend_from_slice` once per frame — the parser
    /// interpreter's own recursion put dozens of those in front of every
    /// collection taken inside a parse.
    fn push_roots(&self, out: &mut Vec<GcRef>) {
        out.extend_from_slice(&self.roots.borrow());
    }
}

/// A `GcRef` proven rooted for `'s` — the only input to a `&mut Payload`
/// accessor.
///
/// The accessors used to take a bare `GcRef` and hand back a `&'static mut
/// Payload`, which says the payload outlives the program: a helper could hold
/// one across an allocation that reclaimed its owner and keep writing through
/// it. A `Rooted<'s>` cannot outlive the [`NativeScope`] that produced it, and
/// the accessors' results cannot outlive the `Rooted`, so the whole chain is
/// bounded by a scope that is itself in the collector's root set.
///
/// **It carries the reference by value, and that is what makes it survive the
/// store's growth.** A `Rooted` that held a `*mut GcRef` into
/// [`NativeRootStore`] would be the natural shape — `Drop` could then clear its
/// own slot — and it would be wrong: the store reallocs, so a later `root()` in
/// *any* live scope can move the array out from under every `Rooted` handed out
/// before it. Holding the value instead means growth is invisible here. What the
/// store owes a `Rooted` is not an address but a promise — that the reference is
/// somewhere in `[0, len)` for as long as the scope lives — and moving the array
/// does not break that promise.
#[derive(Clone, Copy, Debug)]
pub struct Rooted<'s> {
    r: GcRef,
    _scope: PhantomData<&'s ()>,
}

impl Rooted<'_> {
    /// The underlying reference. Copying it out drops the proof, so this is for
    /// passing the value on (as a call argument, as a return value), not for
    /// re-deriving a payload reference.
    #[inline]
    #[must_use]
    pub fn get(self) -> GcRef {
        self.r
    }
}

/// A RAII claim on the tail of [`NativeRootStore`]: records the store's length
/// on construction and truncates back to it on `Drop`.
///
/// Create one in any runtime wrapper that holds a `GcRef` across something that
/// may allocate, and root every such reference through it. Sixty sites do
/// (forty-two in `abi.rs`, seventeen in the parser interpreter, one in the
/// debugger's `p EXPR`), and none of them had to change when ADR-114 replaced
/// the boxed frame underneath: the type is a pointer, a `usize` and a
/// `PhantomData`, it is still constructed by an `unsafe fn new(ctx)` returning
/// by value, `root` still takes `&self` so several `Rooted` values can be live
/// at once, and `Rooted<'s>` still borrows from the scope.
///
/// **The watermark is a `usize` index and it must stay one.** The store grows —
/// see [`NATIVE_ROOT_RESERVATION`] for why it has to — so a `*mut GcRef`
/// watermark saved by an outer scope dangles the moment an inner scope's
/// `root()` reallocs, and `Drop` then publishes that dangling pointer as the
/// store's new end. Every small test passes; the failure needs a scope that
/// roots past the reservation while another is live, which is
/// `a_scope_survives_the_growth_its_own_roots_force` and its sibling.
pub struct NativeScope<'c> {
    /// The store this scope claims from, or null when the context was null or
    /// a [`placeholder`](RuntimeContext::placeholder). Shared, never `&mut`:
    /// every mutation goes through the `RefCell`, which is what lets `root`
    /// take `&self`.
    store: *const NativeRootStore,
    /// The store's length when this scope opened — everything above it belongs
    /// to this scope and to the scopes it nests.
    watermark: usize,
    _ctx: PhantomData<&'c mut RuntimeContext>,
}

impl<'c> NativeScope<'c> {
    /// Open a scope on `ctx`'s native root store.
    ///
    /// A null or unwired context is accepted: the scope has no store to claim
    /// from, so `root` records nothing, but it still hands back a `Rooted` — the
    /// proof keeps its meaning on the defensive null-context paths, exactly as
    /// it did when those roots went into a frame that was linked to nothing.
    /// Either way the references are unreachable from a collection that cannot
    /// happen, because a context with no store has no heap either.
    ///
    /// # Safety
    /// `ctx` must be null, or point at a live `RuntimeContext` that outlives
    /// this scope.
    #[must_use]
    pub unsafe fn new(ctx: *mut RuntimeContext) -> NativeScope<'c> {
        let store: *const NativeRootStore = if ctx.is_null() {
            std::ptr::null()
        } else {
            // SAFETY: caller guarantees `ctx` is live.
            unsafe { (*ctx).native_roots }
        };
        // SAFETY: a non-null `native_roots` is the store owned by the `Runtime`
        // this context views, which the caller guarantees outlives the scope.
        let watermark = match unsafe { store.as_ref() } {
            Some(store) => store.len(),
            None => 0,
        };
        NativeScope {
            store,
            watermark,
            _ctx: PhantomData,
        }
    }

    /// Root `r` for the rest of this scope and return the proof.
    ///
    /// One bounds-checked store and one increment past the null test — where
    /// through ABI v19 the first call on a scope also grew a zero-capacity
    /// `Vec` (and the scope's construction had already boxed a frame).
    #[inline]
    pub fn root(&self, r: GcRef) -> Rooted<'_> {
        // SAFETY: as `new`'s — `store` is either null or the live store of the
        // context this scope borrows.
        if let Some(store) = unsafe { self.store.as_ref() } {
            store.push(r);
        }
        Rooted {
            r,
            _scope: PhantomData,
        }
    }

    /// The number of references this scope and everything nested inside it
    /// currently root.
    ///
    /// The old shape answered "this frame only", which was the same number for
    /// every caller that asked before opening an inner scope — and every caller
    /// does, because a scope is a leaf for as long as it is being filled.
    #[must_use]
    pub fn root_count(&self) -> usize {
        // SAFETY: as `new`'s.
        match unsafe { self.store.as_ref() } {
            Some(store) => store.len() - self.watermark,
            None => 0,
        }
    }
}

impl Drop for NativeScope<'_> {
    fn drop(&mut self) {
        // SAFETY: `store` was the live store of the context when the scope was
        // created, and the caller of `new` guaranteed that context outlives the
        // scope.
        if let Some(store) = unsafe { self.store.as_ref() } {
            store.truncate(self.watermark);
        }
    }
}

// ---------------------------------------------------------------------------
// The composite runtime root set (P0-06)
// ---------------------------------------------------------------------------

/// Everything the runtime owns that names a `GcRef` — five arms that keep one
/// alive, and one that only has to keep one *valid*.
///
/// Sealed: the only constructor is [`RuntimeRoots::from_context`], so a
/// collection cannot be run against a hand-picked subset. The six arms are
/// every documented owner of a reference:
///
/// | arm | strength | owner |
/// |---|---|---|
/// | `shadow` | strong | `ctx.shadow` — the generated shadow stack, scanned `[base, top)` (ADR-019, ADR-101) |
/// | `input` | strong | `ctx.input_source` — the read-in buffer |
/// | `parse_partial` | strong | `ParseDetail.fail.partial` — the best partial parse |
/// | `snapshot` | strong | the runtime-owned `CrashSnapshot`'s copied locals |
/// | `native` | strong | [`NativeRootStore`] — what Rust helpers hold, scanned `[0, len)` (P0-07, ADR-114) |
/// | `debug` | **weak** | `ctx.debug_frames` + `ctx.debug_values` — the crash debugger's live frames and the value slots they name (ADR-104, ADR-106) |
///
/// Before this, `abi::maybe_collect` walked `shadow` alone *and returned early
/// when it was null* — so during host-driven allocation, and throughout the
/// parser interpreter, nothing was collected at all. Deleting that early return
/// is what makes the other four strong arms load-bearing rather than
/// decorative.
///
/// ## Why the sixth arm is weak
///
/// The debug slots are the *over-approximate* set: ADR-044 split them from the
/// root set precisely so that making the root set exact would not make the
/// debugger render `<uninit>` for a local the user can still see in their
/// source. `RootSlots::dead` nulls a shadow slot at its local's last use; the
/// debug slot keeps the value, because MIR-16 says a value that has been
/// produced stays renderable.
///
/// Pushing `debug` in [`RootSet::push_roots`] would undo exactly that split. It
/// is one line, it makes every dead local reachable again, and
/// `a_dead_local_stops_being_reachable_from_its_frame` is the end-to-end gate
/// that fails when someone writes it — deliberately, and it must keep failing.
/// The arm's whole content is therefore in [`WeakSet::clear_reclaimed`]: the
/// collector decides what dies without consulting the debugger, and then tells
/// the debugger what died.
pub struct RuntimeRoots<'a> {
    shadow: Option<&'a crate::ShadowStackHeader>,
    input: Option<GcRef>,
    parse_partial: Option<GcRef>,
    snapshot: Option<&'a crate::CrashSnapshot>,
    native: Option<&'a NativeRootStore>,
    /// The weak arm (ADR-106). `None` on a placeholder context, exactly as the
    /// strong arms are.
    debug: Option<DebugArm<'a>>,
}

/// The weak arm's two halves: a frame is two claims on two stacks (ADR-104
/// decision 3), and the clear needs both.
///
/// The scan is driven from the *frames*, because a frame entry is what pairs a
/// run of value slots with the `local_count` that bounds it — the same pair
/// `crash_snapshot::copy_stack` walks, so the set the collector clears and the
/// set a snapshot copies are the same set by construction rather than by
/// argument. The value stack comes along so
/// [`DebugFrameStackHeader::clear_reclaimed`] can `debug_assert` that those runs
/// really do partition `[base, top)`, which is the premise that makes "driven
/// from the frames" and "every claimed slot" the same statement.
///
/// Shared references like every other arm. The collector *reads* both headers
/// and writes through the `*mut Option<GcRef>` each frame entry carries — the
/// same pointer `DebugFrameGuard::set` and every generated prologue write
/// through, and one that carries the reservation's own provenance rather than
/// being re-derived from a shared slice.
#[derive(Clone, Copy)]
struct DebugArm<'a> {
    frames: &'a crate::DebugFrameStackHeader,
    values: &'a crate::DebugValueStackHeader,
}

impl<'a> RuntimeRoots<'a> {
    /// Read every root arm out of `ctx`.
    ///
    /// # Safety
    /// `ctx` must be null, or point at a live `RuntimeContext` whose non-null
    /// `shadow` / `parse_detail` / `crash_snapshot` / `native_roots` /
    /// `debug_frames` / `debug_values` pointers reference live values for `'a`.
    /// A non-null context's `input_source` must be a valid `GcRef`
    /// (`RuntimeContext::placeholder` documents the same requirement).
    #[must_use]
    pub unsafe fn from_context(ctx: *mut RuntimeContext) -> RuntimeRoots<'a> {
        if ctx.is_null() {
            return RuntimeRoots {
                shadow: None,
                input: None,
                parse_partial: None,
                snapshot: None,
                native: None,
                debug: None,
            };
        }
        // SAFETY: caller guarantees `ctx` is live for `'a`.
        let c = unsafe { &*ctx };
        RuntimeRoots {
            // SAFETY: a non-null `shadow` is the header of the runtime-owned
            // shadow stack, which is live for as long as the context is.
            shadow: unsafe { c.shadow.as_ref() },
            input: Some(c.input_source),
            // SAFETY: a non-null `parse_detail` points at the runtime's slot.
            parse_partial: unsafe { c.parse_detail.as_ref() }
                .and_then(|d| d.fail.as_ref())
                .and_then(|f| f.partial),
            // SAFETY: a non-null `crash_snapshot` points at the runtime's slot.
            snapshot: unsafe { c.crash_snapshot.as_ref() }.and_then(|s| s.get()),
            // SAFETY: a non-null `native_roots` is the one store owned by the
            // `Runtime` this context views, live for as long as the context is.
            native: unsafe { c.native_roots.as_ref() },
            // SAFETY: a non-null `debug_frames` / `debug_values` are the headers
            // of the runtime-owned debug stacks, live for as long as the
            // context. `Runtime::context` wires the two together or not at all,
            // and `zip` is what says the arm needs both to mean anything.
            debug: unsafe { c.debug_frames.as_ref() }
                .zip(unsafe { c.debug_values.as_ref() })
                .map(|(frames, values)| DebugArm { frames, values }),
        }
    }
}

impl RootSet for RuntimeRoots<'_> {
    fn push_roots(&self, out: &mut Vec<GcRef>) {
        // Exhaustive over all six arms. Destructured rather than field-accessed
        // so adding an owner to `RuntimeContext` without deciding its strength
        // fails to compile here.
        let RuntimeRoots {
            shadow,
            input,
            parse_partial,
            snapshot,
            native,
            debug,
        } = self;
        if let Some(shadow) = shadow {
            shadow.push_roots(out);
        }
        out.extend(input.iter().copied());
        out.extend(parse_partial.iter().copied());
        if let Some(snapshot) = snapshot {
            snapshot.push_roots(out);
        }
        if let Some(native) = native {
            native.push_roots(out);
        }
        // `debug` is bound and deliberately not pushed (ADR-106). This is the
        // one arm that is named here only so the destructure stays exhaustive:
        // the debug slots are the over-approximate set, so rooting them makes
        // the collector's set over-approximate too, which is the merge ADR-044
        // exists to refuse and which
        // `a_dead_local_stops_being_reachable_from_its_frame` fails on. What the
        // collector does with this arm instead is `WeakSet::clear_reclaimed`
        // below; `the_debug_arm_contributes_no_strong_roots` pins the absence.
        let _ = debug;
    }
}

impl WeakSet for RuntimeRoots<'_> {
    /// Null every debug value slot whose object the sweep just reclaimed.
    ///
    /// The whole of the weak arm. Called by `Heap::collect_inner` between the
    /// sweep and the return to the allocator — see
    /// [`WeakSet`] for why nowhere else will do.
    fn clear_reclaimed(&self) -> usize {
        let Some(arm) = self.debug else {
            return 0;
        };
        // SAFETY: `from_context`'s contract puts the two stacks' liveness on its
        // caller, and every claimed entry was written by a prologue (or by
        // `debug::push_frame`) with a `'static` meta and the base of its own run
        // of value slots. A collection can only be entered from a safepoint, and
        // a prologue's claim and its two stores are straight-line with no
        // safepoint between them, so a half-written entry is not a state this
        // can observe.
        unsafe { arm.frames.clear_reclaimed(arm.values) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::NonNull;

    fn dummy_ref(n: usize) -> GcRef {
        // A `GcRef` whose header is a stack `GcHeader` — never dereferenced by
        // these root-set tests; only the pointer identity is observed.
        let header = Box::leak(Box::new(crate::GcHeader::detached()));
        let nn = NonNull::from(header);
        // SAFETY: `nn` points at a leaked, aligned, live header.
        let r = unsafe { GcRef::from_non_null(nn) };
        // Tag the address so distinct refs are distinguishable; the low bits are
        // unused under the allocation alignment.
        let _ = n;
        r
    }

    #[test]
    fn empty_scope_has_no_roots() {
        let scope = RootScope::new();
        let mut out = Vec::new();
        scope.push_roots(&mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn scope_yields_its_roots() {
        let mut scope = RootScope::new();
        let a = dummy_ref(1);
        let b = dummy_ref(2);
        scope.root(a);
        scope.root(b);
        let mut out = Vec::new();
        scope.push_roots(&mut out);
        assert_eq!(out.len(), 2);
        assert!(out.contains(&a));
        assert!(out.contains(&b));
    }

    #[test]
    fn child_scope_chains_to_parent() {
        let mut parent = RootScope::new();
        let a = dummy_ref(1);
        parent.root(a);
        let mut child = RootScope::child(&parent);
        let b = dummy_ref(2);
        child.root(b);

        let mut out = Vec::new();
        child.push_roots(&mut out);
        assert_eq!(out.len(), 2);
        assert!(out.contains(&a));
        assert!(out.contains(&b));
    }

    // -----------------------------------------------------------------------
    // The native root store (ADR-114)
    // -----------------------------------------------------------------------

    /// A runtime plus a context wired to it, which is the only shape a
    /// `NativeScope` can be opened against.
    ///
    /// The runtime is boxed because the fixture moves it into this struct after
    /// minting the context, and `native_roots` — like `heap`, `pending_fault`
    /// and `fault_message` — is a pointer *into* the `Runtime` rather than into
    /// a separately boxed header the way `shadow` and the two debug stacks are.
    /// That distinction is deliberate (only generated code needs a header that
    /// survives a move; nothing outside this crate ever learns this address),
    /// and it is the kind of thing a fixture discovers as a SIGBUS.
    struct Native {
        rt: Box<crate::Runtime>,
        ctx: Box<RuntimeContext>,
    }

    impl Native {
        fn new() -> Native {
            let mut rt = Box::new(crate::Runtime::new());
            let ctx = Box::new(rt.context());
            Native { rt, ctx }
        }

        fn ctx_ptr(&mut self) -> *mut RuntimeContext {
            &mut *self.ctx
        }

        fn store(&self) -> &NativeRootStore {
            self.rt.native_root_store()
        }

        /// What the collector would see through the fifth arm.
        fn native_roots(&mut self) -> Vec<GcRef> {
            let ctx = self.ctx_ptr();
            // SAFETY: `ctx` is a live view of `self.rt`, which outlives the
            // borrow.
            let roots = unsafe { RuntimeRoots::from_context(ctx) };
            let mut out = Vec::new();
            roots.push_roots(&mut out);
            out
        }
    }

    /// A `GcRef` from the real heap, so the collector can be run against it.
    fn heap_ref(rt: &crate::Runtime, value: i64) -> GcRef {
        rt.heap().alloc_unpaced(crate::scalars::INT_PAYLOAD, value)
    }

    #[test]
    fn a_scope_claims_the_tail_and_drops_exactly_what_it_claimed() {
        let mut f = Native::new();
        let a = heap_ref(&f.rt, 1);
        let b = heap_ref(&f.rt, 2);
        assert!(
            f.store().is_empty(),
            "a fresh runtime holds no native roots"
        );
        {
            let ctx = f.ctx_ptr();
            // SAFETY: `ctx` is wired to `f.rt`, which outlives the scope.
            let scope = unsafe { NativeScope::new(ctx) };
            scope.root(a);
            scope.root(b);
            assert_eq!(scope.root_count(), 2);
            assert_eq!(f.store().len(), 2);
            let found = f.native_roots();
            assert!(found.contains(&a) && found.contains(&b));
        }
        assert!(f.store().is_empty(), "the scope released its whole run");
        assert!(f.native_roots().iter().all(|r| *r != a && *r != b));
    }

    #[test]
    fn nested_scopes_partition_one_contiguous_run() {
        // ADR-114's form of `nested_frames_are_one_contiguous_scan`: the parent
        // pointer is gone, so the collector reads `[0, len)` once instead of
        // walking a chain and allocating a `Vec` per link.
        let mut f = Native::new();
        let a = heap_ref(&f.rt, 1);
        let b = heap_ref(&f.rt, 2);
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.rt`, which outlives both scopes.
        unsafe {
            let outer = NativeScope::new(ctx);
            outer.root(a);
            {
                let inner = NativeScope::new(ctx);
                inner.root(b);
                assert_eq!(inner.root_count(), 1);
                assert_eq!(f.store().len(), 2, "one run holds both scopes");
                let found = f.native_roots();
                assert!(found.contains(&a) && found.contains(&b));
            }
            assert_eq!(
                f.store().len(),
                1,
                "the inner pop restores the outer scope's extent"
            );
            assert!(!f.native_roots().contains(&b));
            drop(outer);
        }
        assert!(f.store().is_empty());
    }

    #[test]
    fn a_scope_survives_the_growth_its_own_roots_force() {
        // The test the package exists for. The store reallocs — it must, because
        // one scope's root count is the program's input (`praxis_bfs` roots
        // ~2 per edge) — and a `*mut GcRef` watermark would have been left
        // pointing into the freed array. A `usize` index cannot be.
        let mut f = Native::new();
        let refs: Vec<GcRef> = (0..(NATIVE_ROOT_RESERVATION as i64 + 64))
            .map(|n| heap_ref(&f.rt, n))
            .collect();
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.rt`, which outlives the scope.
        let scope = unsafe { NativeScope::new(ctx) };
        assert_eq!(f.store().capacity(), NATIVE_ROOT_RESERVATION);
        for r in &refs {
            scope.root(*r);
        }
        assert!(
            f.store().capacity() > NATIVE_ROOT_RESERVATION,
            "the reservation was not actually exceeded, so this test proves \
             nothing: capacity is still {}",
            f.store().capacity()
        );
        assert_eq!(scope.root_count(), refs.len());

        // Every root is still found, still in order, and still the object it
        // was — a moved array that was re-read correctly, rather than a stale
        // pointer that happened not to crash.
        let found = f.native_roots();
        let native: Vec<GcRef> = found[found.len() - refs.len()..].to_vec();
        assert_eq!(native, refs);
        drop(scope);
        assert!(f.store().is_empty());
    }

    #[test]
    fn an_inner_scopes_growth_leaves_the_outer_scopes_watermark_valid() {
        // The sharper half: the growth is forced by an *inner* scope, so the
        // outer scope's saved watermark was taken before the array moved. Under
        // a pointer watermark the outer `Drop` publishes an address inside the
        // freed allocation as the store's new end, and the next collection reads
        // it. Under an index it is arithmetic on a number.
        let mut f = Native::new();
        let outer_refs: Vec<GcRef> = (0..3).map(|n| heap_ref(&f.rt, n)).collect();
        let inner_refs: Vec<GcRef> = (0..(NATIVE_ROOT_RESERVATION as i64 + 8))
            .map(|n| heap_ref(&f.rt, 1_000 + n))
            .collect();
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.rt`, which outlives both scopes.
        unsafe {
            let outer = NativeScope::new(ctx);
            for r in &outer_refs {
                outer.root(*r);
            }
            {
                let inner = NativeScope::new(ctx);
                for r in &inner_refs {
                    inner.root(*r);
                }
                assert!(f.store().capacity() > NATIVE_ROOT_RESERVATION);
            }
            assert_eq!(
                f.store().len(),
                outer_refs.len(),
                "the inner scope released exactly its own run across a growth"
            );
            let found = f.native_roots();
            for r in &outer_refs {
                assert!(found.contains(r), "the outer scope lost a root");
            }
            for r in &inner_refs {
                assert!(!found.contains(r), "a released root is still scanned");
            }
            drop(outer);
        }
        assert!(f.store().is_empty());
    }

    #[test]
    fn a_rooted_handed_out_before_a_growth_still_names_its_object() {
        // Why `Rooted` carries the reference by value. If it held a slot address
        // instead — the shape that would let `Drop` clear its own entry — this
        // is where it would dangle, and it would dangle silently: the read would
        // land in freed-then-reused storage and answer *a* `GcRef`.
        let mut f = Native::new();
        let first = heap_ref(&f.rt, 7);
        let filler: Vec<GcRef> = (0..(NATIVE_ROOT_RESERVATION as i64))
            .map(|n| heap_ref(&f.rt, n))
            .collect();
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.rt`, which outlives the scope.
        let scope = unsafe { NativeScope::new(ctx) };
        let rooted = scope.root(first);
        for r in &filler {
            scope.root(*r);
        }
        assert!(f.store().capacity() > NATIVE_ROOT_RESERVATION);
        assert_eq!(rooted.get(), first, "the proof still names its object");
        assert!(f.native_roots().contains(&first));
    }

    #[test]
    fn a_scope_dropped_out_of_order_cannot_raise_the_watermark() {
        // Scopes nest with the Rust stack, so this is not a state the runtime
        // reaches — but the release is `truncate`, whose no-op-when-shorter rule
        // makes the bad order *unrepresentable* rather than merely unreached. A
        // pop that could raise the length would republish entries a live scope
        // had already released, and the collector would trace storage a sweep
        // may have reclaimed.
        let mut f = Native::new();
        let a = heap_ref(&f.rt, 1);
        let b = heap_ref(&f.rt, 2);
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.rt`, which outlives both scopes.
        let (outer, inner) = unsafe {
            let outer = NativeScope::new(ctx);
            outer.root(a);
            let inner = NativeScope::new(ctx);
            inner.root(b);
            (outer, inner)
        };
        drop(outer);
        assert_eq!(f.store().len(), 0, "the outer release took both runs");
        drop(inner);
        assert_eq!(
            f.store().len(),
            0,
            "the late inner release restored a watermark above the length and \
             the store stayed where it was"
        );
        assert!(f.native_roots().iter().all(|r| *r != a && *r != b));
    }

    #[test]
    fn a_scope_on_a_null_context_roots_nothing_and_drops_cleanly() {
        // The defensive path `NativeScope::new`'s contract has always allowed.
        // There is no store to claim from, so the proof is all the caller gets —
        // which is the same thing it got when the roots went into a frame that
        // was linked to nothing.
        let mut rt = crate::Runtime::new();
        let a = heap_ref(&rt, 1);
        // SAFETY: a null context is explicitly accepted.
        let scope = unsafe { NativeScope::new(std::ptr::null_mut()) };
        assert_eq!(scope.root(a).get(), a);
        assert_eq!(scope.root_count(), 0);
        drop(scope);
        assert!(rt.native_root_store().is_empty());
        assert!(
            !rt.context().native_roots.is_null(),
            "a wired context is the case that does have a store"
        );
    }

    #[test]
    fn every_context_this_runtime_mints_sees_the_same_store() {
        // The store is the runtime's, not the context's, so a context taken
        // *while* a scope is open — which is what `Runtime::collect_now` and the
        // debugger's `p EXPR` both do — sees it. The chain this replaced started
        // null on every fresh context, so a collection driven from the host was
        // blind to everything native code was holding.
        let mut f = Native::new();
        let a = heap_ref(&f.rt, 42);
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.rt`, which outlives the scope.
        let scope = unsafe { NativeScope::new(ctx) };
        scope.root(a);

        let mut fresh = f.rt.context();
        // SAFETY: `fresh` is a second live view of the same runtime.
        let roots = unsafe { RuntimeRoots::from_context(&mut fresh) };
        let mut out = Vec::new();
        roots.push_roots(&mut out);
        assert!(
            out.contains(&a),
            "a freshly minted context could not see the open scope"
        );
    }

    #[test]
    fn a_native_root_survives_the_collection_that_reclaims_its_neighbour() {
        // The end-to-end statement of the fifth arm, against the real sweep: two
        // objects, one rooted in a scope and one held only in a Rust local, and
        // the collection has to tell them apart.
        let mut f = Native::new();
        let kept = heap_ref(&f.rt, 111);
        let dropped = heap_ref(&f.rt, 222);
        let before = f.rt.heap().stats().live_count;
        assert!(before >= 2);
        let ctx = f.ctx_ptr();
        // SAFETY: `ctx` is wired to `f.rt`, which outlives the scope.
        let scope = unsafe { NativeScope::new(ctx) };
        let rooted = scope.root(kept);
        f.rt.collect_now();
        assert!(
            f.rt.heap().stats().live_count < before,
            "nothing was reclaimed, so this test cannot distinguish the arms"
        );
        assert!(
            f.native_roots().contains(&kept),
            "the scope's root did not survive its own collection"
        );
        assert_eq!(rooted.get(), kept);
        let _ = dropped;
        drop(scope);
    }

    #[test]
    fn the_reservation_is_a_reservation_and_not_a_bound() {
        // ADR-114's whole decision, as an assertion: the store starts at
        // `NATIVE_ROOT_RESERVATION` and goes past it rather than refusing. A
        // hard cap here is a process abort on a graph one edge too large, which
        // is the failure ADR-105's budget exists to prevent.
        let store = NativeRootStore::new();
        assert_eq!(store.capacity(), NATIVE_ROOT_RESERVATION);
        assert!(store.is_empty());
    }

    /// The structural statement that the sixth arm is weak (ADR-106), one layer
    /// below any heap behaviour: a value that *only* a debug slot names is not
    /// in the set the collector traces.
    ///
    /// `a_dead_local_stops_being_reachable_from_its_frame` is the end-to-end
    /// form of the same property and is the gate that a future change did not
    /// quietly promote this arm to a strong one. This is the local form, and it
    /// fails on the line that would do it rather than on a heap size three
    /// layers away.
    #[test]
    fn the_debug_arm_contributes_no_strong_roots() {
        let mut rt = crate::Runtime::new();
        let value = rt.heap().alloc_unpaced(crate::scalars::INT_PAYLOAD, 9_999);
        let mut ctx = Box::new(rt.context());

        let name = b"x";
        let locals = [crate::DebugLocalMeta {
            source_name: name.as_ptr(),
            name_len: 1,
            symbol_id: 1,
            descriptor: &crate::scalars::INT,
            type_id: 1,
            kind: crate::LOCAL_KIND_USER,
            span_start: 0,
            span_end: 0,
        }];
        let meta = crate::FunctionDebugMeta {
            func_name: b"f".as_ptr(),
            func_name_len: 1,
            local_count: 1,
            locals: locals.as_ptr(),
            span_start: 0,
            span_end: 0,
        };
        // SAFETY: `ctx` is wired to `rt`, and `meta`/`locals` outlive the guard.
        let mut guard = unsafe { crate::debug::push_frame(&mut *ctx, &meta) };
        guard.set(0, value);
        assert_eq!(guard.values()[0], Some(value), "the debugger names it");

        // SAFETY: `ctx` is a live view of `rt`, which outlives `roots`.
        let roots = unsafe { RuntimeRoots::from_context(&mut *ctx) };
        let mut out = Vec::new();
        roots.push_roots(&mut out);
        assert!(
            !out.contains(&value),
            "the debug slot put a value in the collector's strong set — that is \
             the ADR-044 set-merge, arriving as one line in `push_roots`"
        );
        drop(guard);
    }
}
