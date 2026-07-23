//! Type descriptors: the vtable-equivalent for runtime objects (§11.4).
//!
//! Every GC object carries a pointer to a [`TypeDescriptor`] that centralizes
//! all payload-aware operations — tracing, dropping, formatting, equality, and
//! hashing. The compiler generates one descriptor per type and emits code that
//! reaches these function pointers through the object header. The point of the
//! design (§11.4) is that there are no scattered type switches in generated or
//! runtime code: every operation routes through a descriptor.

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};

/// An opaque, interned identifier for a type. Equality on `TypeId` *is* type
/// identity for descriptor-table lookups.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TypeId(pub u32);

impl TypeId {
    #[inline]
    pub const fn to_u32(self) -> u32 {
        self.0
    }
}

/// The tracer a descriptor's `trace` function receives during GC. The collector
/// supplies a concrete implementation whose own `trace` method enqueues child
/// references onto the mark worklist (ADR-011).
pub trait Tracer {
    /// Mark a `GcRef` as reachable and arrange for it to be traced.
    fn trace(&mut self, reference: crate::GcRef);
}

/// A hashing sink used by structural hash descriptors (§5.5). Concrete
/// implementations feed bytes into a hash state; [`StructHasher`] is the
/// built-in implementation used by the scalar and collection descriptors.
pub trait DynamicHasher {
    fn write_bytes(&mut self, bytes: &[u8]);
    fn finish(&self) -> u64;
}

/// The built-in [`DynamicHasher`] backed by [`DefaultHasher`]. Used by every
/// descriptor's `hash` callback in M3.
pub struct StructHasher(DefaultHasher);

impl StructHasher {
    pub fn new() -> Self {
        StructHasher(DefaultHasher::new())
    }
}

impl Default for StructHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl DynamicHasher for StructHasher {
    fn write_bytes(&mut self, bytes: &[u8]) {
        // `Hasher::write` consumes the bytes into the hash state.
        self.0.write(bytes);
    }

    fn finish(&self) -> u64 {
        self.0.finish()
    }
}

/// Convenience: feed any `Hash` value into a [`DynamicHasher`] byte-wise.
pub(crate) fn hash_value<H: DynamicHasher + ?Sized, T: Hash + ?Sized>(hasher: &mut H, value: &T) {
    // Route through a shim Hasher so we don't re-implement Hash for each scalar.
    struct HasherShim<'a, H: ?Sized>(&'a mut H);
    impl<H: DynamicHasher + ?Sized> Hasher for HasherShim<'_, H> {
        #[inline]
        fn write(&mut self, bytes: &[u8]) {
            self.0.write_bytes(bytes);
        }
        #[inline]
        fn finish(&self) -> u64 {
            self.0.finish()
        }
    }
    value.hash(&mut HasherShim(hasher));
}

/// `trace` callback shape: receive a pointer to the object payload (the bytes
/// after the header) plus a tracer, and report any `GcRef`s stored inside.
///
/// # Safety
/// The `payload` pointer must point at a value of the descriptor's type for the
/// duration of the call.
pub type TraceFn = unsafe fn(payload: *mut u8, tracer: &mut dyn Tracer);

/// `drop_value` callback shape: release Rust-owned resources held in the
/// payload (e.g. the backing `Vec<GcRef>` of a `Vec[T]`). Invoked during sweep
/// (§12.5).
///
/// # Safety
/// `payload` must point at a value of the descriptor's type, and afterwards the
/// memory is no longer valid.
pub type DropFn = unsafe fn(payload: *mut u8);

/// `format` callback shape: append the user-visible representation of the value
/// to the given writer.
///
/// # Safety
/// `payload` must point at a value of the descriptor's type.
pub type FormatFn = unsafe fn(payload: *const u8, out: &mut dyn fmt::Write);

/// `equals` callback shape: structural equality between two values of the same
/// descriptor. `None` on the descriptor means the type is not equatable.
///
/// # Safety
/// Both pointers must point at values of the descriptor's type.
pub type EqualsFn = unsafe fn(a: *const u8, b: *const u8) -> bool;

/// `hash` callback shape: feed the value's structural identity into a hasher.
/// `None` on the descriptor means the type is not hashable.
///
/// # Safety
/// `payload` must point at a value of the descriptor's type.
pub type HashFn = unsafe fn(payload: *const u8, hasher: &mut dyn DynamicHasher);

/// Centralized table of operations on a value's payload (§11.4).
///
/// Exact Rust types may evolve, but all payload-aware operations must live here
/// rather than in scattered type switches. For Milestone 0 this is a fully
/// defined, constructible data type; the entries themselves are populated in
/// later milestones.
#[derive(Clone, Copy)]
pub struct TypeDescriptor {
    pub id: TypeId,
    pub name: &'static str,
    pub size: usize,
    pub align: usize,
    pub trace: TraceFn,
    pub drop_value: DropFn,
    pub format: FormatFn,
    pub equals: Option<EqualsFn>,
    pub hash: Option<HashFn>,
}

impl TypeDescriptor {
    /// True iff values of this type participate in structural equality (§5.5).
    #[inline]
    pub fn is_equatable(&self) -> bool {
        self.equals.is_some()
    }

    /// True iff values of this type may be used as a map/set key (§5.5).
    /// Equatable + hashable together.
    #[inline]
    pub fn is_hashable(&self) -> bool {
        self.hash.is_some()
    }
}

impl fmt::Debug for TypeDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TypeDescriptor")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("size", &self.size)
            .field("align", &self.align)
            .field("equatable", &self.is_equatable())
            .field("hashable", &self.is_hashable())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: a descriptor can be constructed and copied, and the
    /// `is_equatable` / `is_hashable` flags reflect the optional callbacks.
    /// The function pointers here are dummies that must never be called — the
    /// point is that the *type* is well-formed at Milestone 0.
    unsafe fn dummy_trace(_: *mut u8, _: &mut dyn Tracer) {}
    unsafe fn dummy_drop(_: *mut u8) {}
    unsafe fn dummy_format(_: *const u8, _: &mut dyn fmt::Write) {}
    unsafe fn dummy_eq(a: *const u8, b: *const u8) -> bool {
        a == b
    }
    unsafe fn dummy_hash(_: *const u8, _: &mut dyn DynamicHasher) {}

    #[test]
    fn descriptor_constructs_and_reports_capabilities() {
        let equatable_only = TypeDescriptor {
            id: TypeId(1),
            name: "EquatableOnly",
            size: 8,
            align: 8,
            trace: dummy_trace,
            drop_value: dummy_drop,
            format: dummy_format,
            equals: Some(dummy_eq),
            hash: None,
        };
        assert!(equatable_only.is_equatable());
        assert!(!equatable_only.is_hashable());

        let hashable = TypeDescriptor {
            id: TypeId(2),
            name: "Key",
            size: 16,
            align: 8,
            trace: dummy_trace,
            drop_value: dummy_drop,
            format: dummy_format,
            equals: Some(dummy_eq),
            hash: Some(dummy_hash),
        };
        assert!(hashable.is_equatable());
        assert!(hashable.is_hashable());
    }
}
