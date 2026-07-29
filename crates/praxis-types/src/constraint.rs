//! Constraints: the capabilities a type variable must have, carried alongside
//! the scheme that quantified it (F10, TY-29).
//!
//! # The channel this is
//!
//! Inference discovers capability requirements while it is looking at a *use*:
//! `a == b` needs `Eq`, `for x in xs` needs `Iterable`, `m.insert(k, v)` needs
//! `HashStable` of `k`. When the type in question is already concrete, the
//! requirement is decided on the spot and forgotten. When it is a variable, it
//! cannot be — the variable may be *generalized*, and then instantiated at a
//! type that does not have the capability at all:
//!
//! ```text
//! fn equal(a, b) { a == b }          // needs Eq(?a); ?a is quantified
//! fn main() -> Bool { equal(f, g) }  // instantiated at a function type
//! ```
//!
//! Before this channel, the requirement was discarded at generalization and the
//! program compiled. A constraint survives generalization by riding on the
//! [`Scheme`](crate::Scheme), and every instantiation **re-emits** it against
//! the fresh variables the use site chose — so the check happens once per use,
//! against that use's types, which is the only place it can be right.
//!
//! # Discharge
//!
//! A constraint on a variable that is still unbound cannot be answered yet;
//! answering it optimistically is what the free predicates in
//! `praxis_hir::capability` do, and it is right for them because they are asked
//! about a specific type. Here the rule is *defer*: a pending constraint whose
//! variable is now resolved is **dischargeable**, and inference drains the
//! dischargeable ones after each unification and reports the ones that fail.
//! One that never resolves is one nothing pinned, and an unpinned variable has
//! already been reported as itself.

use praxis_source::FileSpan;
use praxis_stdlib::CapKind;

use crate::type_id::{Type, VarId};

/// A capability requirement, including the ones that carry types.
///
/// [`Capability::Kind`] is the payload-free vocabulary (`praxis_stdlib::CapKind`);
/// the other two exist because their requirement is not "this type is X" but
/// "this type is X **at** these types", and the inner types have to travel with
/// the constraint or the check cannot be made.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Capability {
    /// One of the five structural properties (§5.4).
    Kind(CapKind),
    /// Iterable, yielding `item`. `for x in xs` emits this against `xs`'s type
    /// with `item` bound to `x`'s, so pinning either pins the other.
    Iterable { item: Type },
    /// Has a method `name` taking `params` and returning `result`. Emitted by a
    /// method call on a receiver whose type is not yet known (TY-30): it is
    /// what lets `fn total(values) { values.sum() }` constrain `values` rather
    /// than give up.
    HasMethod {
        name: String,
        params: Vec<Type>,
        result: Type,
    },
}

impl Capability {
    /// The [`CapKind`] this is, or `None` for the two type-carrying arms.
    #[must_use]
    pub fn kind(&self) -> Option<CapKind> {
        match self {
            Capability::Kind(k) => Some(*k),
            _ => None,
        }
    }
}

/// One capability requirement on one type variable, and where the program asked
/// for it.
///
/// `at` is not decoration. A constraint that fails must be reported *where the
/// requirement was written* — at `a == b`, at the `for`, at the `insert` — and a
/// constraint carried through generalization has left that site far behind by
/// the time it is discharged. Without the span the report has nowhere to point.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Constraint {
    /// The variable required to have the capability. A constraint is always
    /// *about a variable*: a requirement on a concrete type is decided
    /// immediately and never becomes one of these.
    pub var: VarId,
    /// What is required.
    pub cap: Capability,
    /// Where the program asked for it.
    pub at: FileSpan,
}

impl Constraint {
    /// A constraint on `var` requiring `cap`, written at `at`.
    #[must_use]
    pub fn new(var: VarId, cap: Capability, at: FileSpan) -> Constraint {
        Constraint { var, cap, at }
    }

    /// A constraint requiring the payload-free capability `kind`.
    #[must_use]
    pub fn of_kind(var: VarId, kind: CapKind, at: FileSpan) -> Constraint {
        Constraint::new(var, Capability::Kind(kind), at)
    }

    /// The type this constraint is about, as a handle.
    #[must_use]
    pub fn var_type(&self) -> Type {
        self.var.as_type()
    }
}
