//! The method catalog (§16.2): one structured table of built-in methods,
//! consumed by every part of the compiler and the LSP.
//!
//! The catalog is built with [`MethodCatalogBuilder`] and finalized with
//! [`MethodCatalogBuilder::finish`], which **rejects duplicate entries** — the
//! `(receiver, name, parameter-count)` triple must be unique. That makes a
//! duplicate overload unrepresentable: the builder errors rather than silently
//! shadowing an earlier entry, which is exactly the kind of "illegal state" the
//! project rules say we should forbid.

use std::fmt;

use crate::type_pattern::TypePattern;

/// Whether a method is pure or has side effects.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Purity {
    /// No allocation, no I/O, no mutation of receiver state visible to the
    /// caller.
    Pure,
    /// May mutate the receiver, allocate, or perform I/O.
    Impure,
}

/// How a catalog entry lowers to actual code.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MethodLowering {
    /// Lowers to a call into the runtime wrapper named by the ABI manifest,
    /// e.g. [`RuntimeSymbol::VecPush`] (§11.1). Carrying the symbol rather than
    /// its name means a catalog row cannot name a wrapper that does not exist,
    /// and the row's allocation/fault behaviour comes from the manifest instead
    /// of being restated here.
    RuntimeSymbol(crate::abi::RuntimeSymbol),
    /// Lowers to a compiler intrinsic (no runtime symbol). Reserved for the
    /// sequence pipeline and a handful of primitives that the compiler folds
    /// directly.
    Intrinsic(&'static str),
}

/// Stability marker for an entry. Most built-ins are `Stable`; experimental
/// helpers carry `Experimental` so the LSP can flag them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stability {
    Stable,
    Experimental,
}

/// One row of the method catalog (§16.2 fields).
#[derive(Clone, Debug)]
pub struct MethodEntry {
    /// The receiver shape the method is defined on, e.g. `Vec[T]`.
    pub receiver: TypePattern,
    /// The method name, e.g. `push`.
    pub name: &'static str,
    /// Parameter type patterns, positional.
    pub params: Vec<TypePattern>,
    /// Result type pattern.
    pub result: TypePattern,
    /// Whether the method is pure.
    pub purity: Purity,
    /// How the method lowers.
    pub lowering: MethodLowering,
    /// One-line documentation, surfaced in hover.
    pub doc: &'static str,
    /// Stability marker.
    pub stability: Stability,
}

impl MethodEntry {
    /// The arity (number of explicit parameters, excluding the receiver).
    pub fn arity(&self) -> usize {
        self.params.len()
    }

    /// Whether calling this method may allocate, and so whether its call site
    /// is a GC safepoint.
    ///
    /// Derived from the ABI manifest, not restated per row: a catalog entry
    /// that disagreed with the wrapper it lowers to was the drift this
    /// replaces. An intrinsic has no wrapper — the MIR lowering it expands to
    /// carries its own per-instruction effects.
    pub fn allocates(&self) -> bool {
        match self.lowering {
            MethodLowering::RuntimeSymbol(sym) => sym.allocates(),
            MethodLowering::Intrinsic(_) => false,
        }
    }

    /// Whether calling this method may raise a runtime fault (§9.1), and so
    /// whether its call site needs a fault check after it.
    ///
    /// Derived, for the same reason as [`MethodEntry::allocates`]: the field
    /// this replaces was dead metadata — nothing read it, because `build.rs`
    /// emits an unconditional check after every method call — and it had drifted.
    /// `bitset_insert` declared `can_fault: false` while
    /// `praxis_bitset_insert` raises `InvalidSize` for a member outside
    /// `BitIndex`'s range. Nobody noticed, because nobody asked.
    pub fn can_fault(&self) -> bool {
        match self.lowering {
            MethodLowering::RuntimeSymbol(sym) => sym.faults(),
            MethodLowering::Intrinsic(_) => false,
        }
    }
}

/// Errors that can occur while building a [`MethodCatalog`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MethodCatalogError {
    /// Two entries share the same `(receiver, name, arity)` triple. Overloads
    /// are not permitted; this is a build-time catalog bug.
    Duplicate {
        receiver: TypePattern,
        name: &'static str,
        arity: usize,
    },
}

impl fmt::Display for MethodCatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MethodCatalogError::Duplicate {
                receiver,
                name,
                arity,
            } => write!(
                f,
                "duplicate catalog entry: {receiver}.{name}/{arity} already defined"
            ),
        }
    }
}

impl std::error::Error for MethodCatalogError {}

/// The finalized method catalog: an ordered, duplicate-free list of entries.
#[derive(Clone, Debug, Default)]
pub struct MethodCatalog {
    entries: Vec<MethodEntry>,
}

impl MethodCatalog {
    /// Begin a builder. The builder is the only way to add entries, and its
    /// `finish` step enforces uniqueness.
    pub fn build() -> MethodCatalogBuilder {
        MethodCatalogBuilder::default()
    }

    /// All entries, in insertion order.
    pub fn entries(&self) -> &[MethodEntry] {
        &self.entries
    }

    /// Entries whose `(receiver, name)` match, in insertion order. The caller
    /// disambiguates by arity at the call site.
    pub fn by_receiver_and_name<'a>(
        &'a self,
        receiver: &'a TypePattern,
        name: &'a str,
    ) -> impl Iterator<Item = &'a MethodEntry> + 'a {
        self.entries
            .iter()
            .filter(move |e| &e.receiver == receiver && e.name == name)
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if there are no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Builder for [`MethodCatalog`]. Enforces the duplicate-entry invariant at
/// `finish`.
#[derive(Default)]
pub struct MethodCatalogBuilder {
    entries: Vec<MethodEntry>,
}

impl MethodCatalogBuilder {
    /// Add an entry. Duplicates are detected at [`finish`](Self::finish).
    pub fn entry(mut self, entry: MethodEntry) -> Self {
        self.entries.push(entry);
        self
    }

    /// Finalize the catalog, returning an error if any two entries share a
    /// `(receiver, name, arity)` triple.
    pub fn finish(self) -> Result<MethodCatalog, MethodCatalogError> {
        for (i, a) in self.entries.iter().enumerate() {
            for b in self.entries.iter().skip(i + 1) {
                if a.receiver == b.receiver && a.name == b.name && a.arity() == b.arity() {
                    return Err(MethodCatalogError::Duplicate {
                        receiver: a.receiver.clone(),
                        name: a.name,
                        arity: a.arity(),
                    });
                }
            }
        }
        Ok(MethodCatalog {
            entries: self.entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::type_pattern::{CollectionCtor, ScalarType};

    fn vec_of_t() -> TypePattern {
        TypePattern::Collection {
            ctor: CollectionCtor::Vec,
            args: vec![TypePattern::Var("T")],
        }
    }

    fn vec_push() -> MethodEntry {
        MethodEntry {
            receiver: vec_of_t(),
            name: "push",
            params: vec![TypePattern::Var("T")],
            result: TypePattern::Unit,
            purity: Purity::Impure,
            lowering: MethodLowering::RuntimeSymbol(crate::abi::RuntimeSymbol::VecPush),
            doc: "Append a value to the end of the vector.",
            stability: Stability::Stable,
        }
    }

    fn vec_len() -> MethodEntry {
        MethodEntry {
            receiver: vec_of_t(),
            name: "len",
            params: vec![],
            result: TypePattern::Scalar(ScalarType::Int),
            purity: Purity::Pure,
            lowering: MethodLowering::RuntimeSymbol(crate::abi::RuntimeSymbol::VecLen),
            doc: "Number of elements in the vector.",
            stability: Stability::Stable,
        }
    }

    #[test]
    fn finish_accepts_distinct_entries() {
        let catalog = MethodCatalog::build()
            .entry(vec_push())
            .entry(vec_len())
            .finish()
            .expect("distinct entries");
        assert_eq!(catalog.len(), 2);
        let names: Vec<_> = catalog
            .by_receiver_and_name(&vec_of_t(), "push")
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["push"]);
    }

    #[test]
    fn finish_rejects_duplicate_triple() {
        // Same receiver, name, and arity as `vec_push` → duplicate.
        let dup = MethodEntry {
            doc: "alternate overload that the language does not allow",
            ..vec_push()
        };
        let err = MethodCatalog::build()
            .entry(vec_push())
            .entry(dup)
            .finish()
            .unwrap_err();
        match err {
            MethodCatalogError::Duplicate { name, arity, .. } => {
                assert_eq!(name, "push");
                assert_eq!(arity, 1);
            }
        }
    }

    #[test]
    fn same_name_different_arity_is_allowed() {
        // `len` (0 args) and a hypothetical `len` taking a sentinel are two
        // different triples. The catalog allows them; the *language* may not,
        // but that is a separate concern from table integrity.
        let other = MethodEntry {
            params: vec![TypePattern::Scalar(ScalarType::Int)],
            ..vec_len()
        };
        let catalog = MethodCatalog::build()
            .entry(vec_len())
            .entry(other)
            .finish()
            .expect("different arity is not a duplicate");
        assert_eq!(catalog.len(), 2);
    }

    #[test]
    fn entry_reports_capabilities() {
        let e = vec_push();
        assert_eq!(e.arity(), 1);
        // `allocates` is the manifest's answer, not a field the row restates.
        // Both of these are safepoints, and `len` is the interesting one: the
        // row used to declare `allocates: false` because "reading a length
        // allocates nothing" — but `praxis_vec_len` boxes the count into a
        // fresh `Int`, so a collection can run inside it. That disagreement is
        // exactly what deriving the answer from the wrapper removes.
        assert!(e.allocates());
        assert!(vec_len().allocates());
        assert!(
            !e.can_fault(),
            "praxis_vec_push is Allocates, not AllocatesAndFaults"
        );
        assert_eq!(e.purity, Purity::Impure);
    }
}
