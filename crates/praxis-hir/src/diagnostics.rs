//! Diagnostic construction for name resolution and (later) type inference.
//!
//! Name-resolution diagnostics use the `N0xx` prefix; type diagnostics use
//! `Y0xx`. Both reuse the existing [`Diagnostic`] type from `praxis-source` —
//! there is no separate error channel (§8.2, rule 20.6).
//!
//! The `Y0xx` constructors are used by type inference (Slice 5); they are kept
//! here alongside the `N0xx` ones so all diagnostic wording lives in one place.

#![allow(dead_code)] // Y0xx constructors are exercised in Slice 5.

use praxis_source::{DiagCode, Diagnostic, FileSpan, Severity};

/// `N001` — a name was used that is not in scope.
pub(crate) fn unresolved_name(at: FileSpan, name: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::UnknownName,
        format!("`{name}` is not defined"),
        at,
    )
}

/// `N002` — a type annotation names a type that is not a known built-in. This
/// covers reserved-but-unimplemented scalars (`Float`, `UInt`, …, per §4.3) and
/// plain typos.
pub(crate) fn unknown_type(at: FileSpan, name: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::UnknownType,
        format!("unknown type `{name}`"),
        at,
    )
}

/// `Y001` — two types that could not be unified (expected vs found).
pub(crate) fn type_mismatch(at: FileSpan, expected: &str, found: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::TypeMismatch,
        format!("expected {expected}, found {found}"),
        at,
    )
}

/// `Y001` with an advisory `help:` hint attached (§8.2: "a concrete suggestion
/// when available"). Use when the fix is explanatory rather than mechanical
/// (e.g. "this value is `Unit`; make the last expression produce a value").
pub(crate) fn type_mismatch_with_help(
    at: FileSpan,
    expected: &str,
    found: &str,
    help_label: &str,
) -> Diagnostic {
    Diagnostic::build(
        Severity::Error,
        DiagCode::TypeMismatch,
        format!("expected {expected}, found {found}"),
        at,
    )
    .help(at, help_label)
    .finish()
}

/// `Y002` — an occurs-check failure (infinite type), e.g. unifying `a` with
/// `(a) -> a`.
pub(crate) fn infinite_type(at: FileSpan) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::InfiniteType,
        "an infinite type would be required here",
        at,
    )
}

/// `Y003` — an explicit annotation conflicts with what inference derived.
pub(crate) fn annotation_conflict(at: FileSpan, annotated: &str, derived: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::AnnotationConflict,
        format!("annotation says {annotated}, but use implies {derived}"),
        at,
    )
}

/// `Y004` — `==` / `!=` applied to a type whose values cannot be compared (e.g.
/// a function value). The wording is concrete (§5.4: never mention trait or
/// capability names): it says the type "cannot be compared" and names the type.
pub(crate) fn not_equatable(at: FileSpan, ty: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NotEquatable,
        format!("values of type `{ty}` cannot be compared with `==`"),
        at,
    )
}

/// `Y005` — a value is iterated (`for x in …`) whose type is not iterable (§4.11,
/// §5.4 `Iterable`). Wording is concrete and never names the capability (§5.4).
pub(crate) fn not_iterable(at: FileSpan, ty: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NotIterable,
        format!("values of type `{ty}` cannot be iterated"),
        at,
    )
}

/// `Y006` — a value is used where an orderable type is required (heap element,
/// sort, comparison) but its type is not orderable (§5.4 `SupportsOrd`). Wording
/// is concrete and never names the capability (§5.4).
pub(crate) fn not_orderable(at: FileSpan, ty: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NotOrderable,
        format!("values of type `{ty}` cannot be ordered"),
        at,
    )
}

/// `Y007` — a type constructor in an annotation was given the wrong number of
/// type arguments, e.g. `Map[Int]` or `Vec[Int, Text]`.
///
/// The arity was declared by [`CollectionCtor::arity`](praxis_types::CollectionCtor::arity)
/// all along and nothing consulted it (TY-07): a wrong-arity annotation
/// interned a type that could never unify with anything, so the user saw a
/// downstream `Y001` naming a type they did not write. This names the mistake
/// where it was made.
pub(crate) fn wrong_type_argument_count(
    at: FileSpan,
    ctor: &str,
    got: usize,
    want: usize,
) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::WrongTypeArgumentCount,
        format!("`{ctor}` takes {want} type argument(s), but {got} were given"),
        at,
    )
}

/// `Y008` — a `struct` or `enum` declaration names the same field or variant
/// twice.
///
/// Also TY-07. The def used to register with both members and every lookup
/// answered the first, so `struct P { x: Int, x: Text }` silently declared a
/// one-field record whose second `x` was unreachable.
pub(crate) fn duplicate_member(at: FileSpan, what: &str, name: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::DuplicateMember,
        format!("duplicate {what} `{name}`"),
        at,
    )
}

/// `Y120` — a `match` is not exhaustive: some values of the scrutinee type are
/// not covered by any arm. The missing constructors are named to guide the fix.
pub(crate) fn non_exhaustive(at: FileSpan, missing: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::NonExhaustiveMatch,
        format!("non-exhaustive match: missing {missing}"),
        at,
    )
}

/// `Y121` — a `match` arm is unreachable: an earlier arm already matched all
/// the values this arm could match.
pub(crate) fn unreachable_arm(at: FileSpan) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagCode::UnreachableArm,
        "unreachable match arm",
        at,
    )
}
