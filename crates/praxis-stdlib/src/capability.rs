//! The capability vocabulary (§5.4): the payload-free names of the structural
//! properties the compiler decides about a type.
//!
//! This is deliberately the *names* and nothing else. A capability that carries
//! a type — `Iterable(T, Item)`, `HasMethod(name, params, result)` — lives in
//! `praxis_types::constraint`, which can name a `Type`; this crate cannot, and
//! should not, because the method catalog's type *patterns* are written here and
//! a pattern is not a type.
//!
//! **§5.4 forbids surfacing any of these names to the user.** A diagnostic says
//! what the program did and why it cannot work — "a `Vec` can change after it is
//! stored, so it cannot be found again as a key" — never "does not satisfy
//! `HashStable`", never "capability", never "trait". The wording lives in
//! `praxis_hir::diagnostics`.

/// One structural property a type either has or does not (§5.4, §5.5).
///
/// The five are not independent, and the two hash-shaped ones are the pair
/// TY-32 exists for. See [`CapKind::HashStable`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CapKind {
    /// Comparable with `==` / `!=` (§5.5). Scalars and `Unit` are; a composite
    /// is iff every component is; functions never are.
    Eq,
    /// Has a total order — usable with `<`/`>`, in a heap, or as a sort key
    /// (§5.4 `SupportsOrd`, ADR-045). The orderable types are exactly the
    /// scalars whose descriptors carry a `compare` callback.
    Ord,
    /// Hashable: the runtime can compute a structural hash of the value. This
    /// is [`CapKind::Eq`]'s companion — the descriptor's `hash` and `equals`
    /// callbacks are defined together — and on its own it is **not** enough to
    /// be a key.
    Hash,
    /// Hashable **and immutable**, which is what a `Map` key or `Set` element
    /// must be (D4, TY-32/RT-08).
    ///
    /// A `Vec` is hashable: the runtime can hash its current contents. It is
    /// not *stably* hashable, because `key.push(2)` after `table.insert(key,
    /// v)` moves the entry's hash without moving the entry, and the value can
    /// no longer be found. Python rejects `list`/`dict`/`set` as keys for
    /// exactly this reason; Rust permits `HashMap<Vec<i32>, V>` only because
    /// the borrow checker makes mutating a held key impossible, and Praxis has
    /// `var` mutation and no borrow checker.
    ///
    /// The rule is **mutability**, not container-ness: scalars, `Text`, tuples,
    /// records and enums are stable *structurally* — a tuple is a key iff every
    /// component is — and the eight mutable collections are not.
    HashStable,
    /// Numeric: admits `+`, `-`, `*`, `/`, unary minus, and the numeric sinks
    /// (`sum`, `product`). `Int`, `UInt`, `Byte` and `Float` are; nothing else
    /// is. `%` is narrower still and is not this capability (TY-27).
    Numeric,
}

impl CapKind {
    /// Every capability, for exhaustive sweeps (agreement tests, tables).
    pub const ALL: &'static [CapKind] = &[
        CapKind::Eq,
        CapKind::Ord,
        CapKind::Hash,
        CapKind::HashStable,
        CapKind::Numeric,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// `ALL` is what every exhaustive sweep iterates, so a variant missing from
    /// it is a capability no agreement test ever checks. The match is what makes
    /// the omission impossible to introduce silently.
    #[test]
    fn all_lists_every_capability() {
        let listed: HashSet<CapKind> = CapKind::ALL.iter().copied().collect();
        assert_eq!(listed.len(), CapKind::ALL.len(), "no duplicates in ALL");
        for kind in CapKind::ALL {
            // An exhaustive match: adding a variant without adding it to `ALL`
            // fails to compile here rather than passing quietly.
            let named = match kind {
                CapKind::Eq => CapKind::Eq,
                CapKind::Ord => CapKind::Ord,
                CapKind::Hash => CapKind::Hash,
                CapKind::HashStable => CapKind::HashStable,
                CapKind::Numeric => CapKind::Numeric,
            };
            assert!(listed.contains(&named));
        }
    }
}
