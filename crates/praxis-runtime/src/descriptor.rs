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

/// The closed set of built-in runtime types (§11.4).
///
/// This enum *is* the type-id registry: a descriptor's [`TypeId`] is derived
/// from its variant, so two built-ins can no longer be labelled with the same
/// id (which is how `Float` and `Text` both became `TypeId(5)`). Uniqueness
/// reduces to enum-discriminant uniqueness, which rustc already enforces.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u32)]
pub enum BuiltinTypeId {
    Unit = 0,
    Bool,
    Int,
    Byte,
    Char,
    Float,
    Text,
    Vec,
    Deque,
    Grid,
    Map,
    Set,
    Counter,
    MinHeap,
    MaxHeap,
    BitSet,
    Tuple,
    Record,
    Enum,
    Closure,
    VarCell,
    /// `Range` (§4.11, ADR-059). Appended, so every existing id is unchanged.
    Range,
}

impl BuiltinTypeId {
    /// Number of built-in types. Kept honest by [`BUILTINS`]'s array length and
    /// by `builtins_are_indexed_by_their_id`.
    pub const COUNT: usize = 22;

    /// Total inverse of the discriminant. A `match` rather than a `transmute`,
    /// so an out-of-range word yields `None` instead of an invalid enum value.
    pub const fn from_u32(v: u32) -> Option<BuiltinTypeId> {
        use BuiltinTypeId::*;
        Some(match v {
            0 => Unit,
            1 => Bool,
            2 => Int,
            3 => Byte,
            4 => Char,
            5 => Float,
            6 => Text,
            7 => Vec,
            8 => Deque,
            9 => Grid,
            10 => Map,
            11 => Set,
            12 => Counter,
            13 => MinHeap,
            14 => MaxHeap,
            15 => BitSet,
            16 => Tuple,
            17 => Record,
            18 => Enum,
            19 => Closure,
            20 => VarCell,
            21 => Range,
            _ => return None,
        })
    }

    /// This built-in's descriptor. The inverse of
    /// [`TypeDescriptor::as_builtin`].
    pub fn descriptor(self) -> &'static TypeDescriptor {
        BUILTINS[self as usize]
    }
}

/// An opaque, interned identifier for a type. Equality on `TypeId` *is* type
/// identity for descriptor-table lookups.
///
/// The inner word is **private**: the only producers are
/// [`TypeDescriptor::builtin`] (which derives it from a [`BuiltinTypeId`]) and
/// the test-only escape hatch, so a hand-written integer literal can no longer
/// impersonate a built-in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TypeId(u32);

impl TypeId {
    #[inline]
    pub const fn to_u32(self) -> u32 {
        self.0
    }

    /// The built-in this id names, or `None` for a non-built-in (today: only
    /// the test descriptors, which live at the top of the `u32` range).
    #[inline]
    pub const fn as_builtin(self) -> Option<BuiltinTypeId> {
        BuiltinTypeId::from_u32(self.0)
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

/// `owned_bytes` callback shape: how many bytes *outside* the object's
/// `[header|payload]` block this value owns — the `Box<str>` behind a `Text`,
/// the `Vec`'s buffer behind a `Vec[T]`, the `HashMap`'s table behind a
/// `Map[K,V]`.
///
/// `None` on the descriptor means "nothing beyond the payload", which is the
/// truth for every scalar and the reason this is opt-in rather than a required
/// constructor argument.
///
/// The collector's pacing counter reads it at allocation. Without it a 1 MiB
/// `Text` charged the same 40 bytes as an `Int`, so a text-heavy program
/// under-reported its own pressure by essentially its whole footprint (RT-04).
///
/// # Safety
/// `payload` must point at a value of the descriptor's type.
pub type OwnedBytesFn = unsafe fn(payload: *const u8) -> usize;

/// `compare` callback shape: total ordering between two values of the same
/// descriptor. `None` on the descriptor means the type is not orderable.
///
/// This is the ordering a **container** imposes — a heap's `Ord`, a sort, a
/// deterministic rendering — and it is total, including over `Float` NaN
/// (which sorts last and equals itself). The source-level `<` on a `Float`
/// keeps IEEE semantics and is a different operation; see ADR-045.
///
/// Populated on `Int`, `Byte`, `Char`, `Float` and `Text`; `None` on everything
/// else, including every composite (ADR-045 decision 1).
///
/// # Safety
/// Both pointers must point at values of the descriptor's type.
pub type CompareFn = unsafe fn(a: *const u8, b: *const u8) -> std::cmp::Ordering;

/// Centralized table of operations on a value's payload (§11.4).
///
/// Exact Rust types may evolve, but all payload-aware operations must live here
/// rather than in scattered type switches.
///
/// `id`, `size` and `align` are private and *derived*: a built-in descriptor is
/// constructible only through [`TypeDescriptor::builtin`], which takes the
/// [`BuiltinTypeId`] the id comes from and the payload type the layout comes
/// from. "A descriptor whose id names a different type" and "a descriptor whose
/// size disagrees with its payload" are therefore unrepresentable.
#[derive(Clone, Copy)]
pub struct TypeDescriptor {
    id: TypeId,
    pub name: &'static str,
    size: usize,
    align: usize,
    pub trace: TraceFn,
    pub drop_value: DropFn,
    pub format: FormatFn,
    pub equals: Option<EqualsFn>,
    pub hash: Option<HashFn>,
    pub compare: Option<CompareFn>,
    /// Bytes this value owns outside its allocation block, for GC pacing.
    /// `None` means none — the scalar case, and the default. Set with
    /// [`TypeDescriptor::with_owned_bytes`].
    pub owned_bytes: Option<OwnedBytesFn>,
}

impl TypeDescriptor {
    /// The only constructor for a built-in descriptor. `id` is derived from
    /// `builtin`; `size`/`align` are derived from the payload type `P`.
    ///
    /// Built-in descriptors must be declared as `static`, never `const`: a
    /// `const` reference is a promoted rvalue with no guaranteed unique
    /// address, and descriptor *pointer* identity is what the runtime compares.
    #[allow(clippy::too_many_arguments)]
    pub const fn builtin<P>(
        builtin: BuiltinTypeId,
        name: &'static str,
        trace: TraceFn,
        drop_value: DropFn,
        format: FormatFn,
        equals: Option<EqualsFn>,
        hash: Option<HashFn>,
        compare: Option<CompareFn>,
    ) -> TypeDescriptor {
        TypeDescriptor {
            id: TypeId(builtin as u32),
            name,
            size: std::mem::size_of::<P>(),
            align: std::mem::align_of::<P>(),
            trace,
            drop_value,
            format,
            equals,
            hash,
            compare,
            owned_bytes: None,
        }
    }

    /// Test-only descriptor whose id is outside the built-in range by
    /// construction, so a fixture can never collide with a real type.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub const fn for_test<P>(
        n: u32,
        name: &'static str,
        trace: TraceFn,
        drop_value: DropFn,
        format: FormatFn,
        equals: Option<EqualsFn>,
        hash: Option<HashFn>,
        compare: Option<CompareFn>,
    ) -> TypeDescriptor {
        TypeDescriptor {
            id: TypeId(u32::MAX - n),
            name,
            size: std::mem::size_of::<P>(),
            align: std::mem::align_of::<P>(),
            trace,
            drop_value,
            format,
            equals,
            hash,
            compare,
            owned_bytes: None,
        }
    }

    /// Declare that this type owns memory outside its allocation block, and how
    /// to measure it (RT-04).
    ///
    /// A builder rather than a constructor argument because the default —
    /// "nothing beyond the payload" — is right for every scalar and for
    /// `VarCell`, and a required argument would make twenty-one declarations
    /// spell out the same `None`.
    #[must_use]
    pub const fn with_owned_bytes(self, owned_bytes: OwnedBytesFn) -> TypeDescriptor {
        TypeDescriptor {
            id: self.id,
            name: self.name,
            size: self.size,
            align: self.align,
            trace: self.trace,
            drop_value: self.drop_value,
            format: self.format,
            equals: self.equals,
            hash: self.hash,
            compare: self.compare,
            owned_bytes: Some(owned_bytes),
        }
    }

    /// Bytes `payload` owns outside its allocation block, or 0 if this type
    /// owns nothing beyond its payload.
    ///
    /// # Safety
    /// `payload` must point at a value of this descriptor's type.
    #[inline]
    pub unsafe fn owned_bytes_of(&self, payload: *const u8) -> usize {
        match self.owned_bytes {
            // SAFETY: forwarded from this function's contract.
            Some(f) => unsafe { f(payload) },
            None => 0,
        }
    }

    /// This descriptor's type identity.
    #[inline]
    pub const fn id(&self) -> TypeId {
        self.id
    }

    /// Which built-in this descriptor is, if any.
    #[inline]
    pub const fn as_builtin(&self) -> Option<BuiltinTypeId> {
        self.id.as_builtin()
    }

    /// Size in bytes of this type's payload.
    #[inline]
    pub const fn size(&self) -> usize {
        self.size
    }

    /// Alignment in bytes of this type's payload.
    #[inline]
    pub const fn align(&self) -> usize {
        self.align
    }

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

    /// True iff values of this type have a defined ordering (§5.5).
    #[inline]
    pub fn is_orderable(&self) -> bool {
        self.compare.is_some()
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

/// Every built-in descriptor, indexed by its [`BuiltinTypeId`] discriminant.
///
/// This is the registry `BuiltinTypeId::descriptor` reads and the array
/// `builtins_are_indexed_by_their_id` walks; adding a variant without adding an
/// entry here is a compile error on the array length.
pub static BUILTINS: [&TypeDescriptor; BuiltinTypeId::COUNT] = [
    &crate::scalars::UNIT,
    &crate::scalars::BOOL,
    &crate::scalars::INT,
    &crate::scalars::BYTE,
    &crate::scalars::CHAR,
    &crate::scalars::FLOAT,
    &crate::text::TEXT,
    &crate::collections::VEC,
    &crate::collections::DEQUE,
    &crate::collections::GRID,
    &crate::maps::MAP,
    &crate::maps::SET,
    &crate::maps::COUNTER,
    &crate::heaps::MIN_HEAP,
    &crate::heaps::MAX_HEAP,
    &crate::bitset::BITSET,
    &crate::tuples::TUPLE,
    &crate::records::RECORD,
    &crate::enums::ENUM,
    &crate::closures::CLOSURE,
    &crate::var_cell::VAR_CELL,
    &crate::range::RANGE,
];

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
        static EQUATABLE_ONLY: TypeDescriptor = TypeDescriptor::for_test::<i64>(
            0,
            "EquatableOnly",
            dummy_trace,
            dummy_drop,
            dummy_format,
            Some(dummy_eq),
            None,
            None,
        );
        assert!(EQUATABLE_ONLY.is_equatable());
        assert!(!EQUATABLE_ONLY.is_hashable());
        assert!(!EQUATABLE_ONLY.is_orderable());

        static HASHABLE: TypeDescriptor = TypeDescriptor::for_test::<[u64; 2]>(
            1,
            "Key",
            dummy_trace,
            dummy_drop,
            dummy_format,
            Some(dummy_eq),
            Some(dummy_hash),
            None,
        );
        assert!(HASHABLE.is_equatable());
        assert!(HASHABLE.is_hashable());
        assert_eq!(HASHABLE.size(), 16);
        assert_eq!(HASHABLE.align(), 8);
    }

    /// A test descriptor's id is outside the built-in range by construction, so
    /// a fixture can never be mistaken for a real type.
    #[test]
    fn test_descriptor_ids_are_not_builtins() {
        static PROBE: TypeDescriptor = TypeDescriptor::for_test::<u8>(
            0,
            "Probe",
            dummy_trace,
            dummy_drop,
            dummy_format,
            None,
            None,
            None,
        );
        assert_eq!(PROBE.as_builtin(), None);
    }

    #[test]
    fn builtin_type_ids_are_globally_unique() {
        let mut by_id = std::collections::BTreeMap::new();

        for descriptor in BUILTINS {
            if let Some(previous) = by_id.insert(descriptor.id(), descriptor.name) {
                panic!(
                    "built-in descriptors {previous} and {} share {:?}; descriptor IDs are runtime type identity",
                    descriptor.name, descriptor.id()
                );
            }
        }
        assert_eq!(by_id.len(), BuiltinTypeId::COUNT);
    }

    /// The registry is a lookup table: `BUILTINS[b as usize]` must be the
    /// descriptor whose id *is* `b`. Without this, `BuiltinTypeId::descriptor`
    /// would silently return a neighbour.
    #[test]
    fn builtins_are_indexed_by_their_id() {
        for (index, descriptor) in BUILTINS.iter().enumerate() {
            assert_eq!(
                descriptor.id().to_u32(),
                index as u32,
                "BUILTINS[{index}] is {} whose id is {:?}",
                descriptor.name,
                descriptor.id()
            );
            let builtin = BuiltinTypeId::from_u32(index as u32).expect("index is in range");
            assert!(std::ptr::eq(builtin.descriptor(), *descriptor));
        }
        assert!(BuiltinTypeId::from_u32(BuiltinTypeId::COUNT as u32).is_none());
    }

    /// Built-in descriptors are `static`, so their address is their identity.
    /// Two reads of the same descriptor must produce the same pointer — this is
    /// what lets the runtime compare descriptors by pointer rather than by id.
    #[test]
    fn builtin_descriptors_have_a_stable_address() {
        assert!(std::ptr::eq(&crate::scalars::INT, &crate::scalars::INT));
        assert!(std::ptr::eq(
            BuiltinTypeId::Int.descriptor(),
            &crate::scalars::INT
        ));
        assert!(!std::ptr::eq(
            &crate::scalars::FLOAT,
            &crate::text::TEXT as &TypeDescriptor
        ));
    }
}
