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

use crate::type_pattern::{Bound, TypePattern};

/// The catalog name of the subscript **read** `m[key]` (REP-16).
///
/// A subscript is dispatched on the receiver's shape and its arity exactly as a
/// method is — `grid[x, y]` is `Grid[T]` at arity two, `m[key]` is `Map[K, V]` at
/// arity one — so it is a catalog row rather than a second dispatch table. The
/// spelling is not an identifier, which is what keeps it out of source: the
/// parser only accepts an `Ident` after `.`, so `m.[](k)` cannot be written, and
/// nothing in the language can name these rows except the subscript grammar.
pub const INDEX_READ: &str = "[]";

/// The catalog name of the subscript **store** `m[key] = value` (REP-16). The
/// value is the last parameter, after the indices.
pub const INDEX_STORE: &str = "[]=";

/// The catalog name of `distance[key] min= candidate` (§6.2) — and
/// [`INDEX_STORE_MAX`] its `max=` dual.
///
/// Their own rows rather than a read-modify-write over [`INDEX_READ`] and
/// [`INDEX_STORE`], because §6.2 gives them a semantics no read can express: "an
/// absent entry accepts the first value", where a subscript *read* of an absent
/// key faults (§4.7).
pub const INDEX_STORE_MIN: &str = "[]min=";

/// The catalog name of `best[key] max= score` (§6.2). See [`INDEX_STORE_MIN`].
pub const INDEX_STORE_MAX: &str = "[]max=";

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
    /// Lowers to a **dedicated MIR instruction whose result is a scalar**, with
    /// this symbol as the out-of-line form the backend's cold arm calls
    /// (ADR-118 decision 6).
    ///
    /// The distinction from [`RuntimeSymbol`](Self::RuntimeSymbol) is not
    /// cosmetic and it is not "this one is inlined". Two facts follow from it
    /// that the plain arm cannot express:
    ///
    /// * **The answer is not a `GcRef`.** The row's manifest return is
    ///   `AbiRet::RawI64`, so the wrapper hands back the scalar channel and the
    ///   builder decides whether the value is ever boxed at all. Every value
    ///   answer in the plain arm is an `AbiRet::Gc`, which is what
    ///   `a_non_faulting_row_with_a_value_result_cannot_answer_the_unit_sentinel`
    ///   checks — and the check is *right* about that arm, which is why this one
    ///   is a separate variant rather than a loosening of it.
    /// * **The call site's safepoint status is the instruction's, not
    ///   `Inst::Call`'s.** `liveness::is_gc_safepoint` matches every
    ///   `Inst::Call` regardless of the symbol's [`Effect`](crate::abi::Effect),
    ///   so a `Pure` primitive lowered as a call spills the whole root set at a
    ///   point no collection can happen. A row lowered this way gets an
    ///   instruction MIR can classify honestly.
    ///
    /// `BitSet.contains` is the only row here today. `Vec.get`/`Vec[]` want the
    /// same treatment and cannot have it yet: their answer is a `GcRef` element,
    /// so they need a `Gc`-dst instruction rather than a scalar one. See
    /// ADR-118's open questions.
    ScalarPrimitive(crate::abi::RuntimeSymbol),
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
            MethodLowering::RuntimeSymbol(sym) | MethodLowering::ScalarPrimitive(sym) => {
                sym.allocates()
            }
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
            MethodLowering::RuntimeSymbol(sym) | MethodLowering::ScalarPrimitive(sym) => {
                sym.faults()
            }
            MethodLowering::Intrinsic(_) => false,
        }
    }

    /// What each of this entry's type variables must be (TY-31), by name.
    ///
    /// A bound is a fact about the *variable*, not about the position it is
    /// written in, so this sweeps the receiver, the parameters and the result and
    /// reports each name once. That is why `Vec[T].sum()` can declare its `Int`
    /// requirement on the receiver's element and have it apply — there is nowhere
    /// else in the row for it to live.
    ///
    /// A name that declares the *same* bound twice is one requirement.
    /// [`MethodCatalogBuilder::finish`] refuses two *different* ones, so the
    /// dedup here cannot hide a contradiction.
    #[must_use]
    pub fn bounds(&self) -> Vec<(&'static str, Bound)> {
        let mut all = Vec::new();
        self.receiver.collect_bounds(&mut all);
        for p in &self.params {
            p.collect_bounds(&mut all);
        }
        self.result.collect_bounds(&mut all);
        let mut seen: Vec<(&'static str, Bound)> = Vec::new();
        for entry in all {
            if !seen.contains(&entry) {
                seen.push(entry);
            }
        }
        seen
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
    /// One entry declares two *different* bounds for the same type variable
    /// (TY-31). A bound is a fact about the variable, so the row is asking for
    /// two incompatible things and whichever the checker happened to read first
    /// would win silently.
    ConflictingBound {
        method: &'static str,
        var: &'static str,
        first: Bound,
        second: Bound,
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
            MethodCatalogError::ConflictingBound {
                method,
                var,
                first,
                second,
            } => write!(
                f,
                "catalog entry `{method}` bounds `{var}` as both {first:?} and {second:?}"
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

    /// Does **any** receiver in the catalog have a method `name` taking `arity`
    /// arguments?
    ///
    /// The predicate lives here rather than at the call site because its
    /// justification is a fact about this table: the catalog is the *complete*
    /// method universe of the language. A record carries no rows (`p.len()` on
    /// `struct P { len: Int }` is a missing method, not a field read), an enum
    /// carries none, and there is no user `impl` — so a name this table does not
    /// hold at that arity can never resolve against **any** receiver, known or
    /// not yet known.
    ///
    /// That is what lets inference refuse `fn f(x) { x.nope() }` before anything
    /// says what `x` is (ADR-093). The complementary half matters just as much:
    /// a name the table *does* hold — `sum`, at arity 0 — is left deferred even
    /// though no receiver is known, because §5.2's `fn total(values) {
    /// values.sum() }` must still infer. Spelling the predicate as "no row
    /// matches this receiver" instead would reject that program.
    ///
    /// If this language ever grows user-defined methods, this predicate loses
    /// its justification and ADR-093's Rule B has to go with it.
    pub fn has_name_at_arity(&self, name: &str, arity: usize) -> bool {
        self.entries
            .iter()
            .any(|e| e.name == name && e.arity() == arity)
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
    /// `(receiver, name, arity)` triple, or if any single entry bounds one type
    /// variable two different ways (TY-31).
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
            // `bounds()` dedups equal declarations, so anything left twice under
            // one name is a contradiction the checker would resolve by accident.
            let bounds = a.bounds();
            for (j, (var, first)) in bounds.iter().enumerate() {
                if let Some((_, second)) = bounds.iter().skip(j + 1).find(|(v, _)| v == var) {
                    return Err(MethodCatalogError::ConflictingBound {
                        method: a.name,
                        var,
                        first: *first,
                        second: *second,
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
            args: vec![TypePattern::var("T")],
        }
    }

    fn vec_push() -> MethodEntry {
        MethodEntry {
            receiver: vec_of_t(),
            name: "push",
            params: vec![TypePattern::var("T")],
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
            other => panic!("expected a duplicate, got {other}"),
        }
    }

    /// A bound is a fact about the *variable* (TY-31), so one entry cannot
    /// declare two of them for one name: whichever the checker read first would
    /// win, silently, and the row's other claim would simply not happen.
    ///
    /// The same bound written twice is *not* a conflict — an entry that names `T`
    /// in three positions may restate it — which is the half a "reject
    /// duplicates" rule would get wrong.
    #[test]
    fn finish_rejects_two_bounds_on_one_variable() {
        let conflicted = MethodEntry {
            receiver: TypePattern::Collection {
                ctor: CollectionCtor::Vec,
                args: vec![TypePattern::is_scalar("T", ScalarType::Int)],
            },
            params: vec![TypePattern::is_scalar("T", ScalarType::Text)],
            ..vec_push()
        };
        let err = MethodCatalog::build()
            .entry(conflicted)
            .finish()
            .unwrap_err();
        match err {
            MethodCatalogError::ConflictingBound { method, var, .. } => {
                assert_eq!(method, "push");
                assert_eq!(var, "T");
            }
            other => panic!("expected a conflicting bound, got {other}"),
        }

        // Restating the *same* bound is one requirement, not a conflict.
        let restated = MethodEntry {
            receiver: TypePattern::Collection {
                ctor: CollectionCtor::Vec,
                args: vec![TypePattern::is_scalar("T", ScalarType::Int)],
            },
            params: vec![TypePattern::is_scalar("T", ScalarType::Int)],
            ..vec_push()
        };
        let bounds = restated.bounds();
        assert_eq!(bounds, vec![("T", Bound::Is(ScalarType::Int))]);
        assert!(MethodCatalog::build().entry(restated).finish().is_ok());
    }

    /// `bounds()` finds a declaration wherever it is written — the whole point of
    /// keying on the variable rather than the position. `sum` declares its `Int`
    /// requirement on the receiver's element, and `min_by`-shaped rows would
    /// declare one inside a closure parameter.
    #[test]
    fn bounds_are_found_in_every_position() {
        let on_receiver = MethodEntry {
            receiver: TypePattern::Collection {
                ctor: CollectionCtor::Vec,
                args: vec![TypePattern::is_scalar("T", ScalarType::Int)],
            },
            ..vec_len()
        };
        assert_eq!(
            on_receiver.bounds(),
            vec![("T", Bound::Is(ScalarType::Int))]
        );

        // Inside a closure parameter, two levels down.
        let in_a_closure = MethodEntry {
            params: vec![TypePattern::Function {
                params: vec![TypePattern::is_scalar("U", ScalarType::Char)],
                result: Box::new(TypePattern::Unit),
            }],
            ..vec_len()
        };
        assert_eq!(
            in_a_closure.bounds(),
            vec![("U", Bound::Is(ScalarType::Char))]
        );

        // In the result, and inside a tuple in it.
        let in_the_result = MethodEntry {
            result: TypePattern::Tuple(vec![
                TypePattern::Scalar(ScalarType::Int),
                TypePattern::is_scalar("V", ScalarType::Byte),
            ]),
            ..vec_len()
        };
        assert_eq!(
            in_the_result.bounds(),
            vec![("V", Bound::Is(ScalarType::Byte))]
        );

        // An unbounded variable declares nothing, which is the common case.
        assert!(vec_push().bounds().is_empty());
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
        // **This assertion used to be `!e.can_fault()`** (REP-45, §8.2), with
        // the reason "praxis_vec_push is Allocates, not AllocatesAndFaults" —
        // which restated the manifest row rather than checking it against the
        // wrapper. `praxis_vec_push` calls `adopt_or_reject`, which ends in
        // `set_fault(ctx, TYPE_MISMATCH)`, so the row was wrong and the test
        // was pinning it. `praxis_vec_len` is the contrast that keeps the
        // assertion meaningful: it really cannot fault.
        assert!(e.can_fault(), "praxis_vec_push raises TypeMismatch");
        assert!(!vec_len().can_fault());
        assert_eq!(e.purity, Purity::Impure);
    }
}
