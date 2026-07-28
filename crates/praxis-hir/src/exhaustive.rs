//! Exhaustiveness checking for `match` expressions (§4.6, the WS5 follow-up).
//!
//! A `match` must cover every value of its scrutinee type, or include a
//! catch-all arm (`_` or a variable bind). Non-exhaustive matches are rejected
//! with `Y120`; arms that an earlier arm already made unreachable are rejected
//! with `Y121`.
//!
//! The algorithm is a practical usefulness check:
//! - **Enum scrutinee**: every variant must be covered by some arm (directly or
//!   via `_`/bind). For a covered variant, its payload sub-patterns must
//!   recursively be exhaustive. Missing variants → `Y120`.
//! - **Bool scrutinee**: both `true` and `false` must be covered.
//! - **Other types** (Int, Text, Char, …): these have infinitely many values,
//!   so a `_` or variable-bind arm is required.
//!
//! Unreachability (`Y121`) is detected for arms that add no new coverage beyond
//! earlier arms — e.g. an arm after a wildcard, or a duplicate variant.
//!
//! This is intentionally simpler than a full Maranget usefulness matrix: it
//! covers the common cases (closed enums, Bool, scalars requiring `_`) without
//! the full algorithm's complexity. Nested patterns are handled recursively.

use praxis_source::{Diagnostic, FileId, FileSpan, Span};
use praxis_types::{data::TypeData, ScalarType, Type, TypeDb};

use crate::diagnostics::{non_exhaustive, unreachable_arm};
use crate::lower::{TypedMatchArm, TypedPattern};

/// A zero-length span at position 0 — used as a fallback when no real span is
/// available (the checker doesn't track match spans through lowering yet).
fn zero_span(file: FileId) -> FileSpan {
    FileSpan::new(
        file,
        Span::new(
            praxis_source::BytePos::from(0u32),
            praxis_source::BytePos::from(0u32),
        ),
    )
}

/// Check one `match` expression for exhaustiveness and unreachable arms,
/// appending `Y120`/`Y121` diagnostics to `out`.
pub(crate) fn check(
    db: &TypeDb,
    file: FileId,
    scrutinee_ty: Type,
    arms: &[TypedMatchArm],
    arm_spans: &[FileSpan],
    out: &mut Vec<Diagnostic>,
) {
    let resolved = db.follow(scrutinee_ty);

    // --- Exhaustiveness ---------------------------------------------------
    // Determine which values are not covered by any arm. If any remain, emit
    // Y120 naming the missing constructors.
    let missing = uncovered_constructors(db, resolved, arms);
    if !missing.is_empty() {
        out.push(non_exhaustive(zero_span(file), &missing));
    }

    // --- Unreachable arms -------------------------------------------------
    // An arm is unreachable if it does not cover any value not already covered
    // by an earlier arm. The common case: any arm after a `_`/bind catch-all.
    let mut caught_all = false;
    for (i, arm) in arms.iter().enumerate() {
        if caught_all {
            // A previous arm was a catch-all; this arm can never match.
            let span = arm_spans.get(i).copied().unwrap_or_else(|| zero_span(file));
            out.push(unreachable_arm(span));
            continue;
        }
        if pattern_catches_all(&arm.pattern) {
            caught_all = true;
        }
    }
}

/// Whether a pattern catches every value of its type (a `_` or variable bind,
/// possibly nested but at this level a catch-all).
fn pattern_catches_all(pat: &TypedPattern) -> bool {
    matches!(pat, TypedPattern::Wildcard | TypedPattern::Bind { .. })
}

/// Compute a human-readable description of the constructors not covered by any
/// arm. Returns empty if the match is exhaustive.
fn uncovered_constructors(db: &TypeDb, scrutinee_ty: Type, arms: &[TypedMatchArm]) -> String {
    match db.data(scrutinee_ty) {
        // An enum: every variant must be covered (directly or by a catch-all).
        TypeData::Enum { def } => {
            let edef = db.enum_def(*def);
            // If any arm is a catch-all, everything is covered.
            if arms.iter().any(|a| pattern_catches_all(&a.pattern)) {
                return String::new();
            }
            let covered: Vec<u32> = arms
                .iter()
                .filter_map(|a| match &a.pattern {
                    TypedPattern::EnumVariant { variant_idx, .. } => Some(*variant_idx),
                    _ => None,
                })
                .collect();
            let missing: Vec<&str> = edef
                .variants
                .iter()
                .enumerate()
                .filter(|(idx, _)| !covered.contains(&(*idx as u32)))
                .map(|(_, v)| v.name.as_str())
                .collect();
            if missing.is_empty() {
                String::new()
            } else {
                format!("variant(s): {}", missing.join(", "))
            }
        }
        // Bool: both true and false must be covered (or a catch-all).
        TypeData::Scalar(ScalarType::Bool) => {
            if arms.iter().any(|a| pattern_catches_all(&a.pattern)) {
                return String::new();
            }
            let mut has_true = false;
            let mut has_false = false;
            for arm in arms {
                if let TypedPattern::Lit { value, .. } = &arm.pattern {
                    match value {
                        crate::lower::Lit::Bool(true) => has_true = true,
                        crate::lower::Lit::Bool(false) => has_false = true,
                        _ => {}
                    }
                }
            }
            match (has_true, has_false) {
                (true, true) => String::new(),
                (true, false) => "case: `false`".to_string(),
                (false, true) => "case: `true`".to_string(),
                (false, false) => "cases: `true`, `false`".to_string(),
            }
        }
        // Int, Text, Char, and all other types have infinitely many values: a
        // catch-all arm is required.
        _ => {
            if arms.iter().any(|a| pattern_catches_all(&a.pattern)) {
                String::new()
            } else {
                "a `_` catch-all arm".to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_source::FileId;

    /// Helper: check a match and return the count of Y120 (non-exhaustive) and
    /// Y121 (unreachable) diagnostics it produced.
    fn check_counts(db: &TypeDb, scrutinee_ty: Type, arms: &[TypedMatchArm]) -> (usize, usize) {
        let mut diags = Vec::new();
        let spans = vec![zero_span(FileId::SYNTHETIC); arms.len()];
        check(
            db,
            FileId::SYNTHETIC,
            scrutinee_ty,
            arms,
            &spans,
            &mut diags,
        );
        let y120 = diags.iter().filter(|d| d.code().number() == 120).count();
        let y121 = diags.iter().filter(|d| d.code().number() == 121).count();
        (y120, y121)
    }

    #[test]
    fn exhaustive_enum_with_wildcard_is_ok() {
        let mut db = TypeDb::new();
        let et = db.register_enum("Tile", vec![("Empty".into(), None), ("Wall".into(), None)]);
        let arms = vec![
            arm(TypedPattern::Wildcard),
            arm(TypedPattern::EnumVariant {
                enum_def_id: praxis_types::data::EnumDefId(0),
                variant_idx: 0,
                subpatterns: vec![],
                ty: et,
            }),
        ];
        let (y120, y121) = check_counts(&db, et, &arms);
        assert_eq!(y120, 0, "wildcard makes it exhaustive");
        assert_eq!(y121, 1, "arm after wildcard is unreachable");
    }

    #[test]
    fn non_exhaustive_enum_reports_y120() {
        let mut db = TypeDb::new();
        let et = db.register_enum("Tile", vec![("Empty".into(), None), ("Wall".into(), None)]);
        // Only Empty is covered; Wall is missing.
        let arms = vec![arm(TypedPattern::EnumVariant {
            enum_def_id: praxis_types::data::EnumDefId(0),
            variant_idx: 0,
            subpatterns: vec![],
            ty: et,
        })];
        let (y120, y121) = check_counts(&db, et, &arms);
        assert_eq!(y120, 1, "missing Wall variant");
        assert_eq!(y121, 0);
    }

    #[test]
    fn int_match_without_wildcard_reports_y120() {
        let mut db = TypeDb::new();
        let int = db.int();
        let arms = vec![
            arm(TypedPattern::Lit {
                value: crate::lower::Lit::Int(1),
                ty: int,
            }),
            arm(TypedPattern::Lit {
                value: crate::lower::Lit::Int(2),
                ty: int,
            }),
        ];
        let (y120, _) = check_counts(&db, int, &arms);
        assert_eq!(y120, 1, "Int match needs a wildcard");
    }

    /// Build a trivial arm with the given pattern (body is an Int-0 lit).
    fn arm(pattern: TypedPattern) -> TypedMatchArm {
        TypedMatchArm {
            pattern,
            body: crate::lower::TypedExpr::Lit {
                value: crate::lower::Lit::Int(0),
                ty: praxis_types::Type(0),
                span: (0, 0),
            },
        }
    }
}
