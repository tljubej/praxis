//! Diagnostic construction for name resolution and (later) type inference.
//!
//! Name-resolution diagnostics use the `N0xx` prefix; type diagnostics use
//! `Y0xx`. Both reuse the existing [`Diagnostic`] type from `praxis-source` —
//! there is no separate error channel (§8.2, rule 20.6).
//!
//! The `Y0xx` constructors are used by type inference (Slice 5); they are kept
//! here alongside the `N0xx` ones so all diagnostic wording lives in one place.

#![allow(dead_code)] // Y0xx constructors are exercised in Slice 5.

use praxis_source::{Diagnostic, DiagnosticCategory, DiagnosticCode, FileSpan, Severity};

/// `N001` — a name was used that is not in scope.
pub(crate) fn unresolved_name(at: FileSpan, name: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagnosticCode::new(DiagnosticCategory::Name, 1),
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
        DiagnosticCode::new(DiagnosticCategory::Name, 2),
        format!("unknown type `{name}`"),
        at,
    )
}

/// `Y001` — two types that could not be unified (expected vs found).
pub(crate) fn type_mismatch(at: FileSpan, expected: &str, found: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagnosticCode::new(DiagnosticCategory::Type, 1),
        format!("expected {expected}, found {found}"),
        at,
    )
}

/// `Y002` — an occurs-check failure (infinite type), e.g. unifying `a` with
/// `(a) -> a`.
pub(crate) fn infinite_type(at: FileSpan) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagnosticCode::new(DiagnosticCategory::Type, 2),
        "an infinite type would be required here",
        at,
    )
}

/// `Y003` — an explicit annotation conflicts with what inference derived.
pub(crate) fn annotation_conflict(at: FileSpan, annotated: &str, derived: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagnosticCode::new(DiagnosticCategory::Type, 3),
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
        DiagnosticCode::new(DiagnosticCategory::Type, 4),
        format!("values of type `{ty}` cannot be compared with `==`"),
        at,
    )
}

/// `Y005` — a value is iterated (`for x in …`) whose type is not iterable (§4.11,
/// §5.4 `Iterable`). Wording is concrete and never names the capability (§5.4).
pub(crate) fn not_iterable(at: FileSpan, ty: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagnosticCode::new(DiagnosticCategory::Type, 5),
        format!("values of type `{ty}` cannot be iterated"),
        at,
    )
}

/// `Y120` — a `match` is not exhaustive: some values of the scrutinee type are
/// not covered by any arm. The missing constructors are named to guide the fix.
pub(crate) fn non_exhaustive(at: FileSpan, missing: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagnosticCode::new(DiagnosticCategory::Type, 120),
        format!("non-exhaustive match: missing {missing}"),
        at,
    )
}

/// `Y121` — a `match` arm is unreachable: an earlier arm already matched all
/// the values this arm could match.
pub(crate) fn unreachable_arm(at: FileSpan) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        DiagnosticCode::new(DiagnosticCategory::Type, 121),
        "unreachable match arm",
        at,
    )
}
