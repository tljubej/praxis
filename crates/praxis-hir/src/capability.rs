//! Internal capability resolution (§5.4, §5.5).
//!
//! The language has no user-visible traits (§4.8); instead the compiler owns a
//! closed table deciding which shapes support structural operations like equality
//! and hashing. This module answers those questions for the type system.
//!
//! The rules (§5.5) are:
//! - `Int`, `Bool`, `Char`, `Byte`, `Text`, `UInt`, `Float`, `Unit` are
//!   equatable and hashable.
//! - Tuples, records, and enums are equatable/hashable iff **every** contained
//!   type (tuple elements, record fields, every enum-variant payload) is.
//! - Collections (`Vec[T]`) are equatable/hashable iff their element type is.
//! - **Functions are never equatable or hashable** (function/closure values have
//!   no structural identity — §5.5).
//!
//! An unresolved type variable is optimistically treated as capable: inference
//! will constrain it, and the diagnostic for a concrete non-equatable type is
//! emitted from `infer_bin` against the resolved type. This keeps the capability
//! check out of the way of polymorphic inference.
//!
//! **Per §5.4, capability failures surfaced to the user must be translated into
//! concrete language terms and must never mention "trait" or "capability".** The
//! diagnostic wording lives in [`crate::diagnostics`].

use praxis_types::{data::TypeData, Type, TypeDb};

/// True iff values of type `t` may be compared with `==` / `!=` (§5.5).
///
/// Recursive over the type's structure. Functions are never equatable; a
/// composite is equatable iff all of its components are. An unresolved type
/// variable is optimistically equatable (see the module docs).
#[must_use]
pub fn supports_eq(db: &TypeDb, t: Type) -> bool {
    match db.data(db.follow(t)) {
        // Scalars: every implemented scalar is equatable. (`Never`, the bottom
        // type, has no values and so is vacuously equatable.)
        TypeData::Scalar(_) => true,
        TypeData::Unit => true,
        // A tuple is equatable iff every element is.
        TypeData::Tuple(els) => els.iter().all(|e| supports_eq(db, *e)),
        // Functions are never equatable (§5.5).
        TypeData::Func { .. } => false,
        // A collection is equatable iff its element type is.
        TypeData::Collection { args, .. } => args.iter().all(|a| supports_eq(db, *a)),
        // A record is equatable iff every field type is.
        TypeData::Record { def } => db
            .record_def(*def)
            .fields
            .iter()
            .all(|f| supports_eq(db, f.ty)),
        // An enum is equatable iff every variant's payload types are (a variant
        // with no payload is trivially equatable).
        TypeData::Enum { def } => db.enum_def(*def).variants.iter().all(|v| {
            v.payload
                .as_ref()
                .map_or(true, |ts| ts.iter().all(|t| supports_eq(db, *t)))
        }),
        // An unresolved var is optimistically equatable (see module docs).
        TypeData::Var(_) => true,
    }
}

/// True iff values of type `t` may be used as a map/set key (§5.5). A type is
/// hashable under the same structural rules as equatability (the runtime's hash
/// and equals callbacks are defined together per descriptor).
#[must_use]
pub fn supports_hash(db: &TypeDb, t: Type) -> bool {
    supports_eq(db, t)
}
