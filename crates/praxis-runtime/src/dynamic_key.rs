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
/// Two keys are equal iff they carry the *same* descriptor and that descriptor's
/// `equals` callback reports them structurally equal (§5.5). The `GcRef` is the
/// rooted value; the descriptor is the `&'static TypeDescriptor` the value's own
/// header names.
///
/// Both fields are private and the descriptor is *derived* from the value, so
/// "a key whose descriptor names a different type than its payload" is
/// unrepresentable: [`DynamicKey::new`] is the only way in, and it reads the
/// descriptor out of the object's header (RT-09).
#[derive(Clone, Copy)]
pub struct DynamicKey {
    /// The rooted key value. Stable for the object's lifetime (non-moving GC,
    /// ADR-011), so its address is a valid hash-collection identity anchor.
    value: GcRef,
    /// The key type's descriptor. Selects the `hash`/`equals` callbacks.
    descriptor: &'static TypeDescriptor,
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

    /// The wrapped value.
    #[inline]
    #[must_use]
    pub fn value(&self) -> GcRef {
        self.value
    }

    /// The descriptor selecting this key's `hash`/`equals` callbacks. Always the
    /// value's own descriptor.
    #[inline]
    #[must_use]
    pub fn descriptor(&self) -> &'static TypeDescriptor {
        self.descriptor
    }
}

impl PartialEq for DynamicKey {
    fn eq(&self, other: &Self) -> bool {
        // Runtime type identity comes first (RT-09). Without it a key of one
        // type dispatches the *left* descriptor's `equals` against the *right*
        // payload — a read through the wrong layout — and the result can also
        // disagree with `Hash`, which is keyed on the descriptor below.
        // Descriptors are `static`, so pointer identity is the authoritative
        // test (ADR-038); the id is not, while two built-ins may share one.
        if !std::ptr::eq(self.descriptor, other.descriptor) {
            return false;
        }
        // Fast path: the same object. Cheaper than the structural callback, and
        // reflexive for a type whose `equals` is not (a future NaN payload).
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
        // descriptor — checked pointer-equal just above, and each descriptor is
        // read from its own object's header. The non-moving GC keeps the
        // payloads stable for the call's duration.
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
        // The descriptor id leads, mirroring `eq`'s descriptor check: keys that
        // can never be equal because they are of different types are then also
        // unlikely to share a bucket. Ids are globally unique (P0-01) and
        // deterministic, unlike a descriptor's address.
        self.descriptor.id().hash(state);
        // Delegate to the descriptor's structural `hash` callback (§11.3),
        // routing its bytes through a `DynamicHasher` shim into Rust's `Hasher`.
        // A missing callback (non-hashable type) hashes the descriptor id alone,
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
                // Defensive: the id hashed above is the whole hash. Should not
                // happen for a well-typed program (the capability check rejects
                // non-hashable keys).
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
    unsafe fn test_format_u8(payload: *const u8, out: &mut dyn std::fmt::Write) {
        let value = unsafe { *payload };
        let _ = write!(out, "{value}");
    }
    unsafe fn test_equals_u8(a: *const u8, b: *const u8) -> bool {
        unsafe { *a == *b }
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
    /// A one-byte payload, so dispatching `LOGICAL_A`'s eight-byte `equals`
    /// against it would read past the object.
    static LOGICAL_C: TypeDescriptor = TypeDescriptor::for_test::<u8>(
        12,
        "LogicalC",
        test_trace,
        test_drop,
        test_format_u8,
        Some(test_equals_u8),
        None,
        None,
    );

    // The payload handles for the three fixtures (REP-02). Declared as
    // `static`s, which is what makes `Payload::new`'s layout check a
    // compile-time one — a fixture whose type argument disagreed with its
    // `for_test::<P>` payload would not build.
    static A_PAYLOAD: crate::descriptor::Payload<i64> = crate::descriptor::Payload::new(&LOGICAL_A);
    static B_PAYLOAD: crate::descriptor::Payload<i64> = crate::descriptor::Payload::new(&LOGICAL_B);
    static C_PAYLOAD: crate::descriptor::Payload<u8> = crate::descriptor::Payload::new(&LOGICAL_C);

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
    fn dynamic_keys_with_different_descriptors_are_never_equal() {
        let heap = Heap::new();
        let a = heap.alloc_unpaced(A_PAYLOAD, 7_i64);
        let b = heap.alloc_unpaced(B_PAYLOAD, 7_i64);

        assert_ne!(
            DynamicKey::new(a),
            DynamicKey::new(b),
            "runtime type identity is part of structural key equality"
        );
    }

    /// The `equals` callback must not run at all against a foreign payload —
    /// the descriptor check has to short-circuit before dispatch, not merely
    /// discard the answer. `LOGICAL_C` reads eight bytes; its payload is one.
    #[test]
    fn a_mismatched_key_never_dispatches_the_equality_callback() {
        let heap = Heap::new();
        let wide = heap.alloc_unpaced(A_PAYLOAD, 7_i64);
        let narrow = heap.alloc_unpaced(C_PAYLOAD, 7_u8);

        // `LOGICAL_A::equals` would read eight bytes out of a one-byte payload.
        // Equality must answer `false` from the descriptors alone.
        assert_ne!(DynamicKey::new(wide), DynamicKey::new(narrow));
        assert_ne!(DynamicKey::new(narrow), DynamicKey::new(wide));
    }

    /// `Hash`'s contract is one-directional — equal keys hash equal — and the
    /// descriptor is now part of both. Distinct types are free to collide, but
    /// they must never be *equal*, which is what would corrupt a bucket.
    #[test]
    fn keys_of_different_types_are_unequal_in_a_real_hash_set() {
        use std::collections::HashSet;

        let heap = Heap::new();
        let a = heap.alloc_unpaced(A_PAYLOAD, 7_i64);
        let b = heap.alloc_unpaced(B_PAYLOAD, 7_i64);

        let mut set = HashSet::new();
        assert!(set.insert(DynamicKey::new(a)));
        assert!(
            set.insert(DynamicKey::new(b)),
            "a same-valued key of another type is a distinct entry"
        );
        assert_eq!(set.len(), 2);
    }

    /// **Rewritten**, not un-ignored (plan §8.2). It asserted that a `Vec` key
    /// stays findable after it is mutated, which no structural hash can
    /// deliver — and named the alternative the language actually took: reject
    /// the state. **D4 chose rejection**, so no Praxis program can build this
    /// `HashSet` any more.
    ///
    /// What it pins now is the fact that makes the rejection necessary rather
    /// than the impossible property it asked for: a `DynamicKey` hashes by the
    /// value's *contents*, so mutating a stored key really does move its
    /// bucket. `a_mutable_collection_is_not_a_key` (`infer_tests.rs`) is the
    /// compile-time half; this is why that half has to exist.
    /// **The observation is rewritten** (REP-48). It used to insert the key into
    /// a `HashSet`, mutate the `Vec` in place, and assert `!set.contains(&
    /// wrapped)` — and it failed about once in five hundred runs. The reason is
    /// that `wrapped` is the *same* `GcRef`, so `DynamicKey`'s equality is
    /// trivially true and the whole assertion rested on the mutated key's new
    /// hash not probing the stored entry's slot. A one-element hashbrown table
    /// is a single 16-byte control group, so a new hash whose top seven bits
    /// happen to match the stored tag lands on that slot, equality says yes, and
    /// `contains` answers `true` — one time in about 128 across the two nested
    /// chances. The rule being pinned is real; the way it was being *watched*
    /// was a probability.
    ///
    /// So the hashes are compared directly, with the same `RandomState` a
    /// `HashMap` builds its hasher from. Two 64-bit hashes colliding is not a
    /// number this suite has to care about, and the assertion now measures the
    /// property in the sentence rather than a consequence of it.
    #[test]
    fn a_structural_key_hashes_by_contents_so_mutating_it_moves_its_bucket() {
        use std::collections::hash_map::RandomState;
        use std::collections::HashSet;
        use std::hash::BuildHasher;

        let rt = Runtime::new();
        let state = RandomState::new();
        let hash_of = |k: &DynamicKey| state.hash_one(*k);

        let key = rt.alloc_vec(&crate::scalars::INT, Vec::new());
        let wrapped = DynamicKey::new(key);
        let before = hash_of(&wrapped);

        // Contents, not identity: a *different* empty `Vec` hashes the same.
        let twin = DynamicKey::new(rt.alloc_vec(&crate::scalars::INT, Vec::new()));
        assert_eq!(hash_of(&twin), before, "the hash is over the contents");

        let mut set = HashSet::new();
        assert!(set.insert(wrapped));

        // One push is enough: the hash is over the contents, and the contents
        // are different.
        let item = rt.alloc_int(1);
        unsafe {
            (*key.payload::<crate::collections::VecPayload>())
                .items
                .push(item);
        }
        assert_ne!(
            hash_of(&wrapped),
            before,
            "a mutated key hashes elsewhere — which is exactly why the type \
             checker refuses one (D4, Y014)"
        );
        // …and the entry is still in the table, filed under the hash it no
        // longer has. That is the shape of the corruption: not a lost value, an
        // unfindable one.
        assert_eq!(set.len(), 1);
    }
}
