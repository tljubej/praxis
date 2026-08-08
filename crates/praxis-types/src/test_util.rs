//! Record and enum fixtures shared by this crate's test modules.
//!
//! One home for `FieldSet::from_pairs(..).expect(..)` and its `VariantSet`
//! twin, so no test module spells them again.
//!
//! The nominal and anonymous forms stay as separate functions rather than
//! collapsing into one `Option<&str>` parameter: that buys no invariant and
//! makes every call site noisier.
//!
//! These stay here rather than in `praxis-test-support`: that crate does not
//! depend on `praxis-types`, and nothing outside this crate needs them.

#![cfg(test)]

use crate::{FieldSet, Type, TypeDb, VariantSet};

/// A nominal record from `(name, type)` pairs.
pub(crate) fn record(db: &mut TypeDb, name: &str, fields: Vec<(String, Type)>) -> Type {
    let fields = FieldSet::from_pairs(fields).expect("distinct field names");
    db.record(Some(name.to_string()), fields)
}

/// An anonymous structural record (§5.6).
pub(crate) fn anon_record(db: &mut TypeDb, fields: Vec<(String, Type)>) -> Type {
    let fields = FieldSet::from_pairs(fields).expect("distinct field names");
    db.record(None, fields)
}

/// A nominal enum from `(name, payload)` pairs. An empty payload is a
/// payload-less variant.
pub(crate) fn enum_ty(db: &mut TypeDb, name: &str, variants: Vec<(String, Vec<Type>)>) -> Type {
    let variants = VariantSet::from_pairs(variants).expect("distinct variant names");
    db.enum_(Some(name.to_string()), variants)
}

/// An anonymous enum (`choice(...)`, §7.5).
pub(crate) fn anon_enum(db: &mut TypeDb, variants: Vec<(String, Vec<Type>)>) -> Type {
    let variants = VariantSet::from_pairs(variants).expect("distinct variant names");
    db.enum_(None, variants)
}
