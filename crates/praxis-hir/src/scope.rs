//! Lexical scope tree.
//!
//! Scopes form a parent-indexed tree (each scope knows its parent). Name lookup
//! walks from the innermost scope outward, so an inner binding shadows an outer
//! one with the same name — exactly the §4.2/§5.3 shadowing rule.
//!
//! The tree is a `Vec<ScopeData>` indexed by [`ScopeId`]; child scopes carry
//! their parent's id. This avoids `Rc` cycles and keeps scope creation cheap.

use std::collections::HashMap;

use crate::symbol::SymbolId;

/// An opaque index into the scope tree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScopeId(pub u32);

/// One scope: a name→symbol map plus a parent pointer. The root scope has no
/// parent and is seeded with the prelude.
#[derive(Clone, Debug)]
pub struct ScopeData {
    /// `None` for the root scope.
    pub parent: Option<ScopeId>,
    /// Each name maps to the innermost symbol visible in this scope. Shadowing
    /// inserts a new entry for an existing name, displacing (but not deleting)
    /// the outer one — the outer is still reachable in the parent's own map.
    bindings: HashMap<String, SymbolId>,
}

impl ScopeData {
    fn new(parent: Option<ScopeId>) -> Self {
        ScopeData {
            parent,
            bindings: HashMap::new(),
        }
    }
}

/// The lexical scope tree. Create with [`ScopeTree::new`] (which mints the root
/// scope), then push child scopes and bind/look up names.
#[derive(Clone, Debug)]
pub struct ScopeTree {
    scopes: Vec<ScopeData>,
}

impl ScopeTree {
    /// A fresh tree containing only the root scope (no parent).
    #[must_use]
    pub fn new() -> Self {
        ScopeTree {
            scopes: vec![ScopeData::new(None)],
        }
    }

    /// The root scope id (always `ScopeId(0)`).
    #[must_use]
    pub fn root(&self) -> ScopeId {
        ScopeId(0)
    }

    /// Open a child scope of `parent` and return its id.
    #[must_use]
    pub fn push_child(&mut self, parent: ScopeId) -> ScopeId {
        let id = ScopeId(self.scopes.len() as u32);
        self.scopes.push(ScopeData::new(Some(parent)));
        id
    }

    /// Introduce `name → symbol` into `scope`. If `name` already exists in this
    /// exact scope, the new binding shadows it (the map entry is overwritten);
    /// the previous binding remains reachable through its parent chain if it was
    /// in an outer scope. Returns the displaced symbol, if any (rare; only when
    /// rebinding within the same scope).
    pub fn bind(&mut self, scope: ScopeId, name: impl Into<String>, symbol: SymbolId) {
        self.scopes[scope.0 as usize]
            .bindings
            .insert(name.into(), symbol);
    }

    /// Whether `name` is bound in **this exact scope**, ignoring the parent
    /// chain.
    ///
    /// [`ScopeTree::lookup`] cannot answer this: a top-level `fn` declared twice
    /// and a `var` shadowing a prelude name both find a symbol, and only the
    /// first is a mistake (TY-24).
    #[must_use]
    pub fn is_bound_here(&self, scope: ScopeId, name: &str) -> bool {
        self.scopes[scope.0 as usize].bindings.contains_key(name)
    }

    /// Look up `name` starting in `scope` and walking out to the root. Returns
    /// the innermost visible symbol, or `None` if unbound. This is the core of
    /// name resolution.
    #[must_use]
    pub fn lookup(&self, scope: ScopeId, name: &str) -> Option<SymbolId> {
        self.lookup_binding(scope, name).map(|(sym, _)| sym)
    }

    /// Like [`lookup`](Self::lookup), and also **where** the binding was found.
    ///
    /// The scope is what answers "did this name cross a boundary": a `fn` body
    /// is a child of the scope around it, so the lookup walks straight out to the
    /// file's top level and a `fn` reading a top-level `var` resolved silently
    /// (REP-22). Only the caller knows which boundaries matter, so this reports
    /// the fact and decides nothing.
    #[must_use]
    pub fn lookup_binding(&self, scope: ScopeId, name: &str) -> Option<(SymbolId, ScopeId)> {
        let mut current = Some(scope);
        while let Some(s) = current {
            if let Some(&sym) = self.scopes[s.0 as usize].bindings.get(name) {
                return Some((sym, s));
            }
            current = self.scopes[s.0 as usize].parent;
        }
        None
    }

    /// Whether `scope` is `ancestor` or is nested inside it.
    ///
    /// Reflexive on purpose: a binding in the very scope being asked about has
    /// not crossed it.
    #[must_use]
    pub fn is_within(&self, scope: ScopeId, ancestor: ScopeId) -> bool {
        let mut current = Some(scope);
        while let Some(s) = current {
            if s == ancestor {
                return true;
            }
            current = self.scopes[s.0 as usize].parent;
        }
        false
    }
}

impl Default for ScopeTree {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_walks_outward() {
        let mut tree = ScopeTree::new();
        let root = tree.root();
        tree.bind(root, "outer", SymbolId(0));
        let child = tree.push_child(root);
        tree.bind(child, "inner", SymbolId(1));
        // Inner shadows outer.
        assert_eq!(tree.lookup(child, "outer"), Some(SymbolId(0)));
        assert_eq!(tree.lookup(child, "inner"), Some(SymbolId(1)));
        // Root cannot see the child's binding.
        assert_eq!(tree.lookup(root, "inner"), None);
    }

    #[test]
    fn shadowing_in_same_scope_displaces() {
        let mut tree = ScopeTree::new();
        let root = tree.root();
        tree.bind(root, "a", SymbolId(1));
        tree.bind(root, "a", SymbolId(2)); // shadows
        assert_eq!(tree.lookup(root, "a"), Some(SymbolId(2)));
    }

    #[test]
    fn unresolved_returns_none() {
        let tree = ScopeTree::new();
        assert!(tree.lookup(tree.root(), "missing").is_none());
    }
}
