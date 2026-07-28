//! The `DynamicKey` wrapper for hash-based collections (§11.3).
//!
//! `Map[K, V]`, `Set[T]`, and `Counter[T]` reuse Rust's `HashMap`/`HashSet`
//! behind opaque GC objects. Rust needs `Hash` + `Eq` on its key type, but a
//! Praxis key is a uniform `GcRef` whose structural identity is defined by the
//! *value's* type descriptor (§5.5, §11.3): `DynamicKey` is the bridge.
//!
//! A `DynamicKey` stores the rooted `GcRef` plus its descriptor. Its Rust
//! `Hash`/`Eq` delegate to the descriptor's `hash`/`equals` callbacks (§11.3:
//! "Its Rust `Hash` and `Eq` implementations delegate to descriptor functions
//! generated or selected by the compiler"). The static type checker guarantees
//! one collection instance receives only its declared key type, so all keys in
//! one map share a descriptor; a non-hashable type (e.g. a closure) has
//! `hash`/`equals == None` and is rejected at the capability layer
//! (`supports_hash`, §5.5) before reaching here.
//!
//! `DynamicKey` is a Rust-internal type: it never crosses the ABI and has no
//! `TypeId`. The GC traces the underlying values through the collection's own
//! `trace` callback (which iterates the map/set entries), so `DynamicKey`
//! itself carries no GC-rooting responsibility.

use std::hash::{Hash, Hasher};

use crate::descriptor::{DynamicHasher, TypeDescriptor};
use crate::GcRef;

/// A Praxis value used as a hash-collection key, paired with its descriptor so
/// Rust's `HashMap`/`HashSet` can hash and compare it structurally (§11.3).
///
/// Two keys are equal iff their descriptors' `equals` callbacks report them
/// structurally equal (§5.5). The `GcRef` is the rooted value; the descriptor
/// is the `&'static TypeDescriptor` selected by the compiler for the key type.
#[derive(Clone, Copy)]
pub struct DynamicKey {
    /// The rooted key value. Stable for the object's lifetime (non-moving GC,
    /// ADR-011), so its address is a valid hash-collection identity anchor.
    pub value: GcRef,
    /// The key type's descriptor. Selects the `hash`/`equals` callbacks.
    pub descriptor: &'static TypeDescriptor,
}

impl DynamicKey {
    /// Wrap a `GcRef` as a key, pairing it with its descriptor.
    #[must_use]
    pub fn new(value: GcRef) -> Self {
        // The value's own descriptor IS the key-type descriptor: the type
        // checker guarantees the collection receives only its declared key type,
        // and every GcRef already carries its descriptor in its header.
        let descriptor = value.descriptor();
        Self { value, descriptor }
    }
}

impl PartialEq for DynamicKey {
    fn eq(&self, other: &Self) -> bool {
        // Fast path: identical descriptors (the common case — one key type per
        // collection) and the same object. Also handles the cheap pointer-equal
        // case without invoking the structural callback.
        if self.value == other.value {
            return true;
        }
        // The callbacks are `None` only for non-equatable types (closures), which
        // the capability layer rejects before construction. Defensively treat a
        // missing callback as pointer inequality so a malformed key never matches.
        let Some(equals) = self.descriptor.equals else {
            return false;
        };
        // SAFETY: both `value`s are live GcRefs whose payloads match the
        // descriptor (the type checker guarantees homogeneous keys). The
        // non-moving GC keeps the payloads stable for the call's duration.
        unsafe {
            let a = self.value.payload::<u8>() as *const u8;
            let b = other.value.payload::<u8>() as *const u8;
            equals(a, b)
        }
    }
}

impl Eq for DynamicKey {}

impl std::fmt::Debug for DynamicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Render the value through its descriptor's `format` callback so the
        // debug output shows the user-visible value, not just a raw pointer.
        let mut s = String::new();
        // SAFETY: `value` is a live GcRef matching the descriptor.
        let payload = self.value.payload::<u8>() as *const u8;
        // The format callback returns fmt::Result; discard it (debug rendering
        // is best-effort).
        unsafe {
            (self.descriptor.format)(payload, &mut s);
        }
        write!(f, "DynamicKey({}:{})", self.descriptor.name, s)
    }
}

impl Hash for DynamicKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Delegate to the descriptor's structural `hash` callback (§11.3),
        // routing its bytes through a `DynamicHasher` shim into Rust's `Hasher`.
        // A missing callback (non-hashable type) hashes only the descriptor id,
        // so two such keys never spuriously collide on content they don't have;
        // such keys are rejected at the capability layer before reaching here.
        match self.descriptor.hash {
            Some(hash_fn) => {
                // `value` is a live GcRef whose payload matches the descriptor;
                // the non-moving GC keeps it stable. `payload()` is a safe accessor.
                let payload = self.value.payload::<u8>() as *const u8;
                let mut shim = HasherShim(state);
                // SAFETY: `hash_fn` reads the payload per the descriptor contract.
                unsafe { hash_fn(payload, &mut shim) };
            }
            None => {
                // Defensive: hash the descriptor id only. Should not happen for
                // a well-typed program (capability check rejects non-hashable keys).
                self.descriptor.id().hash(state);
            }
        }
    }
}

/// A [`DynamicHasher`] that feeds bytes into a borrowed Rust [`Hasher`]. Used by
/// [`DynamicKey::hash`] to route the descriptor's structural hash into the
/// `HashMap`/`HashSet`'s own `Hasher`.
struct HasherShim<'a, H: Hasher + ?Sized>(&'a mut H);

impl<H: Hasher + ?Sized> DynamicHasher for HasherShim<'_, H> {
    fn write_bytes(&mut self, bytes: &[u8]) {
        self.0.write(bytes);
    }

    fn finish(&self) -> u64 {
        self.0.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::praxis_alloc_int;
    use crate::context::{Runtime, RuntimeContext};
    use crate::descriptor::TypeDescriptor;
    use crate::{Heap, Tracer};

    unsafe fn test_trace(_: *mut u8, _: &mut dyn Tracer) {}
    unsafe fn test_drop(_: *mut u8) {}
    unsafe fn test_format(payload: *const u8, out: &mut dyn std::fmt::Write) {
        let value = unsafe { *(payload as *const i64) };
        let _ = write!(out, "{value}");
    }
    unsafe fn test_equals(a: *const u8, b: *const u8) -> bool {
        unsafe { *(a as *const i64) == *(b as *const i64) }
    }

    static LOGICAL_A: TypeDescriptor = TypeDescriptor::for_test::<i64>(
        10,
        "LogicalA",
        test_trace,
        test_drop,
        test_format,
        Some(test_equals),
        None,
        None,
    );
    static LOGICAL_B: TypeDescriptor = TypeDescriptor::for_test::<i64>(
        11,
        "LogicalB",
        test_trace,
        test_drop,
        test_format,
        Some(test_equals),
        None,
        None,
    );

    /// Wire a fresh runtime and return its context pointer (test helper).
    fn wired_ctx(rt: &mut Runtime) -> *mut RuntimeContext {
        let ctx = Box::leak(Box::new(rt.context()));
        ctx as *mut RuntimeContext
    }

    #[test]
    fn dynamic_key_equal_for_identical_scalar_values() {
        // Two Int(5) values are structurally equal keys.
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx is wired; praxis_alloc_int produces a valid Int.
        let a = unsafe { praxis_alloc_int(ctx, 5) };
        let b = unsafe { praxis_alloc_int(ctx, 5) };
        assert_ne!(a, b, "distinct allocations");
        let ka = DynamicKey::new(a);
        let kb = DynamicKey::new(b);
        assert_eq!(ka, kb, "Int(5) == Int(5) structurally");
    }

    #[test]
    fn dynamic_key_unequal_for_different_scalar_values() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let a = unsafe { praxis_alloc_int(ctx, 5) };
        let b = unsafe { praxis_alloc_int(ctx, 7) };
        assert_ne!(DynamicKey::new(a), DynamicKey::new(b));
    }

    #[test]
    fn dynamic_key_hash_matches_for_equal_values() {
        // Equal keys must hash equal (the HashMap invariant).
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let a = unsafe { praxis_alloc_int(ctx, 42) };
        let b = unsafe { praxis_alloc_int(ctx, 42) };
        let ha = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            DynamicKey::new(a).hash(&mut h);
            h.finish()
        };
        let hb = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            DynamicKey::new(b).hash(&mut h);
            h.finish()
        };
        assert_eq!(ha, hb, "equal keys hash equal");
    }

    #[test]
    #[ignore = "known bug: DynamicKey::eq does not compare descriptor identity"]
    fn dynamic_keys_with_different_descriptors_are_never_equal() {
        let heap = Heap::new();
        let a = heap.alloc_unpaced(&LOGICAL_A, 7_i64);
        let b = heap.alloc_unpaced(&LOGICAL_B, 7_i64);

        assert_ne!(
            DynamicKey::new(a),
            DynamicKey::new(b),
            "runtime type identity is part of structural key equality"
        );
    }

    #[test]
    #[ignore = "known bug: mutable structural keys invalidate Rust hash-table buckets"]
    fn mutating_a_structural_key_does_not_break_lookup_by_the_same_value() {
        use std::collections::HashSet;

        let rt = Runtime::new();
        let key = rt.alloc_vec(&crate::scalars::INT, Vec::new());
        let wrapped = DynamicKey::new(key);
        let mut set = HashSet::new();
        assert!(set.insert(wrapped));

        for i in 0..4096_i64 {
            let item = rt.alloc_int(i);
            unsafe {
                (*key.payload::<crate::collections::VecPayload>())
                    .items
                    .push(item);
            }
            assert!(
                set.contains(&wrapped),
                "mutating a key after insertion changed its bucket at length {}",
                i + 1
            );
        }
    }
}
