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
//! type-checks.
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
// Native root frames (P0-07)
// ---------------------------------------------------------------------------

/// One native (Rust) stack frame's worth of GC roots, chained to its caller's.
///
/// The runtime's own wrappers build values in Rust locals — a result `Vec` that
/// is filled by repeatedly allocating points, a record assembled field by
/// field. Those locals are invisible to the shadow stack, which only generated
/// code writes. A frame of this chain hangs off
/// [`RuntimeContext::native_roots`] and is walked by [`RuntimeRoots`].
///
/// Roots are held behind a `RefCell` so [`NativeScope::root`] can take `&self`
/// and several `Rooted` values can be live at once — the common shape, since a
/// helper usually roots its result and then roots each intermediate it builds.
#[derive(Debug)]
pub struct NativeRootFrame {
    parent: *mut NativeRootFrame,
    roots: RefCell<Vec<GcRef>>,
}

impl RootSet for NativeRootFrame {
    fn push_roots(&self, out: &mut Vec<GcRef>) {
        out.extend_from_slice(&self.roots.borrow());
        let mut cur = self.parent;
        while !cur.is_null() {
            // SAFETY: every frame in the chain is owned by a live `NativeScope`
            // further up the Rust stack; `Drop` unlinks a frame before it dies.
            let frame = unsafe { &*cur };
            out.extend_from_slice(&frame.roots.borrow());
            cur = frame.parent;
        }
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

/// A RAII native root frame: pushes onto `ctx.native_roots` on construction and
/// pops on `Drop`.
///
/// Create one in any runtime wrapper that holds a `GcRef` across something that
/// may allocate, and root every such reference through it.
pub struct NativeScope<'c> {
    ctx: *mut RuntimeContext,
    frame: Box<NativeRootFrame>,
    _ctx: PhantomData<&'c mut RuntimeContext>,
}

impl<'c> NativeScope<'c> {
    /// Push a fresh native root frame onto `ctx`'s chain.
    ///
    /// A null or unwired context is accepted: the scope still holds its roots
    /// (so `Rooted` keeps its meaning for the defensive null-context paths),
    /// it just is not reachable from a collection that never happens.
    ///
    /// # Safety
    /// `ctx` must be null, or point at a live `RuntimeContext` that outlives
    /// this scope.
    #[must_use]
    pub unsafe fn new(ctx: *mut RuntimeContext) -> NativeScope<'c> {
        let parent = if ctx.is_null() {
            std::ptr::null_mut()
        } else {
            // SAFETY: caller guarantees `ctx` is live.
            unsafe { (*ctx).native_roots }
        };
        let mut frame = Box::new(NativeRootFrame {
            parent,
            roots: RefCell::new(Vec::new()),
        });
        if !ctx.is_null() {
            // SAFETY: as above; the frame outlives the link because `Drop`
            // restores the parent before the box is freed.
            unsafe { (*ctx).native_roots = frame.as_mut() as *mut NativeRootFrame };
        }
        NativeScope {
            ctx,
            frame,
            _ctx: PhantomData,
        }
    }

    /// Root `r` for the rest of this scope and return the proof.
    #[inline]
    pub fn root(&self, r: GcRef) -> Rooted<'_> {
        self.frame.roots.borrow_mut().push(r);
        Rooted {
            r,
            _scope: PhantomData,
        }
    }

    /// The number of references this scope roots directly (its own frame only).
    #[must_use]
    pub fn root_count(&self) -> usize {
        self.frame.roots.borrow().len()
    }
}

impl Drop for NativeScope<'_> {
    fn drop(&mut self) {
        if self.ctx.is_null() {
            return;
        }
        // Unlink this frame, restoring the parent. Scopes nest with the Rust
        // stack, so the frame being popped is always the head.
        // SAFETY: `ctx` was live when the scope was created and the caller
        // guaranteed it outlives the scope.
        unsafe { (*self.ctx).native_roots = self.frame.parent };
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
/// | `native` | strong | [`NativeRootFrame`] — what Rust helpers hold (P0-07) |
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
    native: Option<&'a NativeRootFrame>,
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
            // SAFETY: a non-null `native_roots` is the head of a chain of frames
            // owned by live `NativeScope`s further up the Rust stack.
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
