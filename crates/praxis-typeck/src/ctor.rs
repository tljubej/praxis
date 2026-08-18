//! Validated constructor payloads (F5).
//!
//! A [`TypeDb`](crate::TypeDb) constructor taking a bare `Vec` would put every
//! illegal shape one call away: a one-element tuple, a `Map[T]`, a record with
//! two fields called `x`. Checking at the *syntax* callers is not enough — the
//! parser-template synthesizer and the prelude seeding do not go through one.
//!
//! Each type here is a shape that has already been checked, and it is the only
//! way to reach the corresponding constructor. Validation happens once, at
//! `new`, and everything downstream reads a payload it cannot doubt.

use crate::CollectionCtor;
use crate::data::{EnumVariantDef, RecordFieldDef};
use crate::error::TypeCtorError;
use crate::type_id::Type;

/// Two or more element types, in tuple order.
///
/// A one-element "tuple" is not a tuple — the parser keeps a single
/// parenthesized type as the inner type — and a zero-element one is `Unit`.
/// [`TypeData::Tuple`](crate::TypeData::Tuple) documents that invariant; this
/// is what enforces it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TupleElems(Box<[Type]>);

impl TupleElems {
    /// Validate `elements` as a tuple's elements.
    ///
    /// # Errors
    /// [`TypeCtorError::TupleArity`] if fewer than two were given.
    pub fn new(elements: Vec<Type>) -> Result<TupleElems, TypeCtorError> {
        if elements.len() < 2 {
            return Err(TypeCtorError::TupleArity(elements.len()));
        }
        Ok(TupleElems(elements.into_boxed_slice()))
    }

    /// A pair — the common case, and infallible.
    #[must_use]
    pub fn pair(a: Type, b: Type) -> TupleElems {
        TupleElems(vec![a, b].into_boxed_slice())
    }

    pub(crate) fn into_vec(self) -> Vec<Type> {
        self.0.into_vec()
    }
}

/// A collection's type arguments, shaped so the arity *is* the variant.
///
/// Matching the shape against the ctor is still
/// [`TypeDb::collection`](crate::TypeDb::collection)'s job — a `Unary` handed
/// to `Map` is caught there — but a *wrong-length* argument list cannot even be
/// spelled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CollectionArgs {
    /// `BitSet`, `Range`.
    Nullary,
    /// `Vec[T]`, `Set[T]`, `Grid[T]`, …
    Unary(Type),
    /// `Map[K, V]`.
    Binary(Type, Type),
}

impl CollectionArgs {
    /// Shape an argument list for `ctor`.
    ///
    /// # Errors
    /// [`TypeCtorError::CollectionArity`] if `args` does not have the arity
    /// `ctor` declares.
    pub fn new(ctor: CollectionCtor, args: Vec<Type>) -> Result<CollectionArgs, TypeCtorError> {
        match args.as_slice() {
            [] if ctor.arity() == 0 => Ok(CollectionArgs::Nullary),
            [t] if ctor.arity() == 1 => Ok(CollectionArgs::Unary(*t)),
            [k, v] if ctor.arity() == 2 => Ok(CollectionArgs::Binary(*k, *v)),
            other => Err(TypeCtorError::CollectionArity {
                ctor,
                got: other.len(),
                want: ctor.arity(),
            }),
        }
    }

    /// How many type arguments this shape carries.
    #[must_use]
    pub fn arity(self) -> usize {
        match self {
            CollectionArgs::Nullary => 0,
            CollectionArgs::Unary(_) => 1,
            CollectionArgs::Binary(_, _) => 2,
        }
    }

    /// The arguments, in order.
    #[must_use]
    pub fn to_vec(self) -> Vec<Type> {
        match self {
            CollectionArgs::Nullary => Vec::new(),
            CollectionArgs::Unary(t) => vec![t],
            CollectionArgs::Binary(k, v) => vec![k, v],
        }
    }
}

/// A record definition's fields: name-unique, in declaration order.
///
/// Order is preserved because it is the construction and display order (§5.6);
/// uniqueness is checked because `RecordDef::field` resolves by name and would
/// silently answer the first of a duplicate pair.
#[derive(Clone, Debug)]
pub struct FieldSet(Box<[RecordFieldDef]>);

impl FieldSet {
    /// Validate `fields` as one record definition's fields.
    ///
    /// # Errors
    /// [`TypeCtorError::DuplicateField`] on the first repeated name.
    pub fn new(fields: Vec<RecordFieldDef>) -> Result<FieldSet, TypeCtorError> {
        for (i, f) in fields.iter().enumerate() {
            if fields[..i].iter().any(|prev| prev.name == f.name) {
                return Err(TypeCtorError::DuplicateField(f.name.clone()));
            }
        }
        Ok(FieldSet(fields.into_boxed_slice()))
    }

    /// Validate `(name, type)` pairs — the shape every caller actually has.
    ///
    /// # Errors
    /// As [`new`](Self::new).
    pub fn from_pairs(fields: Vec<(String, Type)>) -> Result<FieldSet, TypeCtorError> {
        FieldSet::new(
            fields
                .into_iter()
                .map(|(name, ty)| RecordFieldDef { name, ty })
                .collect(),
        )
    }

    /// Re-wrap fields taken from a def that was validated when it was
    /// registered. Used by [`fold`](crate::fold) to build a *specialized* copy
    /// of an existing def: substitution rewrites field types and never field
    /// names, so re-checking uniqueness would be quadratic work for an answer
    /// that cannot have changed.
    pub(crate) fn preserving(fields: Vec<RecordFieldDef>) -> FieldSet {
        FieldSet(fields.into_boxed_slice())
    }

    pub(crate) fn into_vec(self) -> Vec<RecordFieldDef> {
        self.0.into_vec()
    }
}

/// An enum definition's variants: name-unique, in declaration order.
#[derive(Clone, Debug)]
pub struct VariantSet(Box<[EnumVariantDef]>);

impl VariantSet {
    /// Validate `variants` as one enum definition's variants.
    ///
    /// # Errors
    /// [`TypeCtorError::DuplicateVariant`] on the first repeated name.
    pub fn new(variants: Vec<EnumVariantDef>) -> Result<VariantSet, TypeCtorError> {
        for (i, v) in variants.iter().enumerate() {
            if variants[..i].iter().any(|prev| prev.name == v.name) {
                return Err(TypeCtorError::DuplicateVariant(v.name.clone()));
            }
        }
        Ok(VariantSet(variants.into_boxed_slice()))
    }

    /// Validate `(name, payload)` pairs. An empty payload is a payload-less
    /// variant.
    ///
    /// # Errors
    /// As [`new`](Self::new).
    pub fn from_pairs(variants: Vec<(String, Vec<Type>)>) -> Result<VariantSet, TypeCtorError> {
        VariantSet::new(
            variants
                .into_iter()
                .map(|(name, payload)| EnumVariantDef { name, payload })
                .collect(),
        )
    }

    /// Re-wrap variants taken from an already-registered def. See
    /// [`FieldSet::preserving`].
    pub(crate) fn preserving(variants: Vec<EnumVariantDef>) -> VariantSet {
        VariantSet(variants.into_boxed_slice())
    }

    pub(crate) fn into_vec(self) -> Vec<EnumVariantDef> {
        self.0.into_vec()
    }
}
