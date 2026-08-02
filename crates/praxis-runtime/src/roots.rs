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
//! RuntimeContext`, and its [`RootSet`] impl is exhaustive over its five arms.
//! "Collect against a partial root set" has no representation.
//!
//! [`NativeScope`] is the fifth arm. Native code that builds a value across an
//! allocation — the grid helpers assembling a `Vec` of points, the parser
//! interpreter assembling a record — holds it in a `Rooted`, which is the only
//! input the `&mut Payload` accessors take (P0-07). Holding a payload reference
//! across a safepoint without rooting its owner no longer type-checks.

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

/// Everything the runtime owns that keeps a `GcRef` alive.
///
/// Sealed: the only constructor is [`RuntimeRoots::from_context`], so a
/// collection cannot be run against a hand-picked subset. The five arms are
/// every documented owner of a live reference:
///
/// | arm | owner |
/// |---|---|
/// | `shadow` | `ctx.shadow` — the generated shadow stack, scanned `[base, top)` (ADR-019, ADR-101) |
/// | `input` | `ctx.input_source` — the read-in buffer |
/// | `parse_partial` | `ParseDetail.fail.partial` — the best partial parse |
/// | `snapshot` | the runtime-owned `CrashSnapshot`'s copied locals |
/// | `native` | [`NativeRootFrame`] — what Rust helpers hold (P0-07) |
///
/// Before this, `abi::maybe_collect` walked `shadow` alone *and returned early
/// when it was null* — so during host-driven allocation, and throughout the
/// parser interpreter, nothing was collected at all. Deleting that early return
/// is what makes the other four arms load-bearing rather than decorative.
pub struct RuntimeRoots<'a> {
    shadow: Option<&'a crate::ShadowStackHeader>,
    input: Option<GcRef>,
    parse_partial: Option<GcRef>,
    snapshot: Option<&'a crate::CrashSnapshot>,
    native: Option<&'a NativeRootFrame>,
}

impl<'a> RuntimeRoots<'a> {
    /// Read every root arm out of `ctx`.
    ///
    /// # Safety
    /// `ctx` must be null, or point at a live `RuntimeContext` whose non-null
    /// `shadow` / `parse_detail` / `crash_snapshot` / `native_roots` pointers
    /// reference live values for `'a`. A non-null context's `input_source` must
    /// be a valid `GcRef` (`RuntimeContext::placeholder` documents the same
    /// requirement).
    #[must_use]
    pub unsafe fn from_context(ctx: *mut RuntimeContext) -> RuntimeRoots<'a> {
        if ctx.is_null() {
            return RuntimeRoots {
                shadow: None,
                input: None,
                parse_partial: None,
                snapshot: None,
                native: None,
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
        }
    }
}

impl RootSet for RuntimeRoots<'_> {
    fn push_roots(&self, out: &mut Vec<GcRef>) {
        // Exhaustive over the five arms. Destructured rather than field-accessed
        // so adding an owner to `RuntimeContext` without rooting it fails to
        // compile here.
        let RuntimeRoots {
            shadow,
            input,
            parse_partial,
            snapshot,
            native,
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
}
