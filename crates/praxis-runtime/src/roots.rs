//! Explicit root frames (§12.3, ADR-012).
//!
//! §12.3 offers "compiler-managed shadow-stack frames **or** explicit root
//! frames." M3 ships explicit root frames (ADR-012): a [`RootSet`] is anything
//! that can enumerate the `GcRef`s it keeps alive, and a RAII [`RootScope`]
//! holds a `Vec<GcRef>` and chains to an optional parent. `Heap::collect` walks
//! a `&dyn RootSet`.
//!
//! In M4, a generated shadow-stack frame will *also* implement [`RootSet`], so
//! M3's choice does not constrain the JIT.

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
