//! `Map[K, V]`, `Set[T]`, and `Counter[T]` (M8-WS3, §6.1, §11.3).
//!
//! All three reuse Rust's hash collections behind opaque GC objects:
//!
//! - `Map[K, V]`    → `HashMap<DynamicKey, GcRef>` (§11.3)
//! - `Set[T]`       → `HashSet<DynamicKey>`       (§11.3)
//! - `Counter[T]`   → `HashMap<DynamicKey, GcRef>` with Int values (§6.2)
//!
//! `DynamicKey` (dynamic_key.rs) bridges Praxis values into Rust's `Hash`/`Eq`
//! by delegating to the descriptor's `hash`/`equals` callbacks — this is the
//! mechanism that finally closes the §19.7 "tuples and records as keys"
//! criterion. A non-hashable key type (closure) is rejected at the capability
//! layer (`supports_hash`) before reaching here.
//!
//! Counter's defining behavior (§6.2): absent keys read as zero, never fault.
//! `min=`/`max=` map updates (§6.2) live in the ABI wrappers.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fmt::Write as _;

use crate::descriptor::{BuiltinTypeId, Tracer, TypeDescriptor};
use crate::dynamic_key::DynamicKey;
use crate::DynamicHasher;
use crate::GcRef;

// ---------------------------------------------------------------------------
// Deterministic rendering (RT-16)
// ---------------------------------------------------------------------------

/// Render one value through `descriptor` into `out`.
///
/// # Safety
/// `value`'s payload must match `descriptor`.
pub(crate) unsafe fn render_into(
    out: &mut dyn fmt::Write,
    descriptor: &TypeDescriptor,
    value: GcRef,
) {
    let payload = value.payload::<u8>() as *const u8;
    // SAFETY: the caller guarantees the payload matches the descriptor.
    unsafe { (descriptor.format)(payload, out) };
}

/// Write `entries` between `open` and `close`, **sorted**, comma-separated.
///
/// Rust's hash collections randomize their iteration order per process, so the
/// same `Map` printed by two runs of the same program produced two different
/// strings (RT-16). §19 promises structural, deterministic formatting, and a
/// program whose expected output cannot be written down does not have one.
///
/// The sort key is the *rendered entry*, not the value: it is the only total
/// order available today, because `TypeDescriptor::compare` is `None` on every
/// descriptor pending design decision D3. So `{10: a, 9: b}` renders with `10`
/// first — lexicographic, not numeric. That is a real limitation and it is
/// stated rather than hidden; when D3 lands and `compare` is populated, this is
/// the one place that has to change.
pub(crate) fn write_sorted<I: Iterator<Item = String>>(
    out: &mut dyn fmt::Write,
    open: &str,
    entries: I,
    close: &str,
) {
    let mut rendered: Vec<String> = entries.collect();
    rendered.sort_unstable();
    let _ = out.write_str(open);
    for (i, entry) in rendered.iter().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        let _ = out.write_str(entry);
    }
    let _ = out.write_str(close);
}

/// The entries of a keyed collection in a **deterministic** order: sorted by the
/// key's rendered form (REP-18).
///
/// The order matters because `keys()` and `values()` promise to be index-aligned,
/// and because a `HashMap`'s own iteration order is randomized per process — the
/// same program would answer differently on two runs, which is RT-16 again in a
/// place where the value, not just the printing, depends on it.
///
/// The sort key is the same one [`write_sorted`] uses, and for the same reason:
/// `TypeDescriptor::compare` is `None` on every descriptor pending D3, so the
/// rendered form is the only total order available. So `{10: a, 9: b}` orders `10`
/// first — lexicographic, not numeric. When D3 lands, `write_sorted` and this are
/// the two places that change.
///
/// # Safety
/// Every key's payload must match the descriptor it carries.
pub(crate) unsafe fn ordered_entries(entries: &HashMap<DynamicKey, GcRef>) -> Vec<(GcRef, GcRef)> {
    let mut rows: Vec<(String, GcRef, GcRef)> = entries
        .iter()
        .map(|(k, v)| {
            let mut rendered = String::new();
            // SAFETY: the key's payload matches the descriptor it carries.
            unsafe { render_into(&mut rendered, k.descriptor(), k.value()) };
            (rendered, k.value(), *v)
        })
        .collect();
    rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    rows.into_iter().map(|(_, k, v)| (k, v)).collect()
}

// ===========================================================================
// Map[K, V]
// ===========================================================================

/// The `Map[K, V]` payload (§11.3). The key descriptor selects structural
/// `hash`/`equals` (via `DynamicKey`); the value descriptor is recorded for
/// `format`/`trace` dispatch (values are uniform `GcRef`s, so the descriptor
/// only matters for element-wise formatting and tracing).
#[repr(C)]
pub struct MapPayload {
    /// The descriptor for every key (selects `DynamicKey`'s hash/eq).
    pub key_descriptor: &'static TypeDescriptor,
    /// The descriptor for every value (for format/trace dispatch).
    pub value_descriptor: &'static TypeDescriptor,
    /// The entries. Keys are `DynamicKey`; values are `GcRef`.
    pub entries: HashMap<DynamicKey, GcRef>,
}

unsafe fn map_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized MapPayload.
    let p = unsafe { &*(payload as *const MapPayload) };
    for (k, v) in p.entries.iter() {
        tracer.trace(k.value());
        tracer.trace(*v);
    }
}

unsafe fn map_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized MapPayload.
    unsafe { std::ptr::drop_in_place(payload as *mut MapPayload) };
}

unsafe fn map_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an initialized MapPayload.
    let p = unsafe { &*(payload as *const MapPayload) };
    let val_desc = p.value_descriptor;
    let entries = p.entries.iter().map(|(k, v)| {
        let mut s = String::new();
        // SAFETY: the key's payload matches the descriptor it carries, and
        // every value matches the map's value descriptor (homogeneous per the
        // type checker).
        unsafe {
            render_into(&mut s, k.descriptor(), k.value());
            let _ = s.write_str(": ");
            render_into(&mut s, val_desc, *v);
        }
        s
    });
    write_sorted(out, "{", entries, "}");
}

unsafe fn map_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at initialized MapPayloads.
    let pa = unsafe { &*(a as *const MapPayload) };
    let pb = unsafe { &*(b as *const MapPayload) };
    if pa.entries.len() != pb.entries.len() {
        return false;
    }
    let Some(eq) = pa.value_descriptor.equals else {
        return false;
    };
    // Two maps are equal iff they have the same keys with equal values. Compare
    // value-wise through the value descriptor.
    for (k, va) in pa.entries.iter() {
        // `get` uses DynamicKey's PartialEq (structural, via the descriptor).
        let Some(vb) = pb.entries.get(k) else {
            return false;
        };
        let va_p = va.payload::<u8>() as *const u8;
        let vb_p = vb.payload::<u8>() as *const u8;
        // SAFETY: both values match the value descriptor (homogeneous per the
        // type checker).
        if !unsafe { eq(va_p, vb_p) } {
            return false;
        }
    }
    true
}

unsafe fn map_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at an initialized MapPayload.
    let p = unsafe { &*(payload as *const MapPayload) };
    // A map's hash is order-independent: hash the count, then each (key, value)
    // pair and combine with a commutative accumulator (XOR). This matches the
    // set/map hashing convention where insertion order must not affect the hash.
    hasher.write_bytes(&(p.entries.len() as u64).to_le_bytes());
    let Some(hash_val) = p.value_descriptor.hash else {
        return;
    };
    let Some(hash_key) = p.key_descriptor.hash else {
        return;
    };
    let mut acc: u64 = 0;
    for (k, v) in p.entries.iter() {
        let mut kh = crate::descriptor::StructHasher::new();
        let k_payload = k.value().payload::<u8>() as *const u8;
        // SAFETY: key payload matches the key descriptor.
        unsafe { hash_key(k_payload, &mut kh) };
        let mut vh = crate::descriptor::StructHasher::new();
        let v_payload = v.payload::<u8>() as *const u8;
        // SAFETY: value payload matches the value descriptor.
        unsafe { hash_val(v_payload, &mut vh) };
        // Pair the key and value hashes together before XOR, so (k1:v2) ≠ (k2:v1).
        let pair = kh
            .finish()
            .wrapping_mul(0x9e3779b97f4a7c15)
            .wrapping_add(vh.finish());
        acc ^= pair;
    }
    hasher.write_bytes(&acc.to_le_bytes());
}

/// Descriptor for `Map[K, V]` (§11.3). Per-instance key/value types live in the
/// payload, so a single descriptor serves all `Map[K, V]`.
pub static MAP: TypeDescriptor = TypeDescriptor::builtin::<MapPayload>(
    BuiltinTypeId::Map,
    "Map",
    map_trace,
    map_drop,
    map_format,
    Some(map_equals),
    Some(map_hash),
    // Not orderable: only Int/Byte/Char/Float/Text are (ADR-045).
    None,
)
.with_owned_bytes(map_owned_bytes);

/// The heap bytes `Map[K,V]` owns beyond its payload, for GC pacing (RT-04).
/// `capacity`, not `len`: the buffer's real footprint is what the collector is
/// paced against.
///
/// # Safety
/// `payload` must point at an initialized `MapPayload`.
unsafe fn map_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized MapPayload.
    let p = unsafe { &*(payload as *const MapPayload) };
    p.entries.capacity() * (std::mem::size_of::<DynamicKey>() + std::mem::size_of::<GcRef>())
}

// ===========================================================================
// Set[T]
// ===========================================================================

/// The `Set[T]` payload (§11.3). The element descriptor selects structural
/// `hash`/`equals` (via `DynamicKey`).
#[repr(C)]
pub struct SetPayload {
    /// The descriptor for every element (selects `DynamicKey`'s hash/eq).
    pub element_descriptor: &'static TypeDescriptor,
    /// The elements, as `DynamicKey`s.
    pub entries: HashSet<DynamicKey>,
}

unsafe fn set_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized SetPayload.
    let p = unsafe { &*(payload as *const SetPayload) };
    for k in p.entries.iter() {
        tracer.trace(k.value());
    }
}

unsafe fn set_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized SetPayload.
    unsafe { std::ptr::drop_in_place(payload as *mut SetPayload) };
}

unsafe fn set_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an initialized SetPayload.
    let p = unsafe { &*(payload as *const SetPayload) };
    let entries = p.entries.iter().map(|k| {
        let mut s = String::new();
        // SAFETY: the key's payload matches the descriptor it carries.
        unsafe { render_into(&mut s, k.descriptor(), k.value()) };
        s
    });
    write_sorted(out, "{", entries, "}");
}

unsafe fn set_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at initialized SetPayloads.
    let pa = unsafe { &*(a as *const SetPayload) };
    let pb = unsafe { &*(b as *const SetPayload) };
    pa.entries.len() == pb.entries.len() && pa.entries.is_subset(&pb.entries)
}

unsafe fn set_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at an initialized SetPayload.
    let p = unsafe { &*(payload as *const SetPayload) };
    // Order-independent: XOR all element hashes.
    hasher.write_bytes(&(p.entries.len() as u64).to_le_bytes());
    let Some(hash_el) = p.element_descriptor.hash else {
        return;
    };
    let mut acc: u64 = 0;
    for k in p.entries.iter() {
        let mut h = crate::descriptor::StructHasher::new();
        let k_payload = k.value().payload::<u8>() as *const u8;
        // SAFETY: element payload matches the element descriptor.
        unsafe { hash_el(k_payload, &mut h) };
        acc ^= h.finish();
    }
    hasher.write_bytes(&acc.to_le_bytes());
}

/// Descriptor for `Set[T]` (§11.3).
pub static SET: TypeDescriptor = TypeDescriptor::builtin::<SetPayload>(
    BuiltinTypeId::Set,
    "Set",
    set_trace,
    set_drop,
    set_format,
    Some(set_equals),
    Some(set_hash),
    // Not orderable: only Int/Byte/Char/Float/Text are (ADR-045).
    None,
)
.with_owned_bytes(set_owned_bytes);

/// The heap bytes `Set[T]` owns beyond its payload, for GC pacing (RT-04).
/// `capacity`, not `len`: the buffer's real footprint is what the collector is
/// paced against.
///
/// # Safety
/// `payload` must point at an initialized `SetPayload`.
unsafe fn set_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized SetPayload.
    let p = unsafe { &*(payload as *const SetPayload) };
    p.entries.capacity() * std::mem::size_of::<DynamicKey>()
}

// ===========================================================================
// Counter[T]
// ===========================================================================

/// The `Counter[T]` payload (§6.2, §11.3). A map whose values are always `Int`
/// and whose absent keys read as zero. Backed by `HashMap<DynamicKey, GcRef>`
/// where each value is a boxed `Int`; the key descriptor selects hash/eq.
#[repr(C)]
pub struct CounterPayload {
    /// The descriptor for every key (selects `DynamicKey`'s hash/eq).
    pub key_descriptor: &'static TypeDescriptor,
    /// The entries: key → boxed Int value.
    pub entries: HashMap<DynamicKey, GcRef>,
}

unsafe fn counter_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized CounterPayload.
    let p = unsafe { &*(payload as *const CounterPayload) };
    for (k, v) in p.entries.iter() {
        tracer.trace(k.value());
        tracer.trace(*v);
    }
}

unsafe fn counter_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized CounterPayload.
    unsafe { std::ptr::drop_in_place(payload as *mut CounterPayload) };
}

unsafe fn counter_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an initialized CounterPayload.
    let p = unsafe { &*(payload as *const CounterPayload) };
    let entries = p.entries.iter().map(|(k, v)| {
        let mut s = String::new();
        // SAFETY: the key's payload matches its descriptor; a Counter's values
        // are always `Int` (§6.2).
        unsafe {
            render_into(&mut s, k.descriptor(), k.value());
            let _ = s.write_str(": ");
            render_into(&mut s, &crate::scalars::INT, *v);
        }
        s
    });
    write_sorted(out, "{", entries, "}");
}

unsafe fn counter_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at initialized CounterPayloads.
    let pa = unsafe { &*(a as *const CounterPayload) };
    let pb = unsafe { &*(b as *const CounterPayload) };
    if pa.entries.len() != pb.entries.len() {
        return false;
    }
    for (k, va) in pa.entries.iter() {
        let Some(vb) = pb.entries.get(k) else {
            return false;
        };
        // Values are Ints; compare payloads directly.
        let va_i = unsafe { *(va.payload::<i64>()) };
        let vb_i = unsafe { *(vb.payload::<i64>()) };
        if va_i != vb_i {
            return false;
        }
    }
    true
}

unsafe fn counter_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at an initialized CounterPayload.
    let p = unsafe { &*(payload as *const CounterPayload) };
    hasher.write_bytes(&(p.entries.len() as u64).to_le_bytes());
    let Some(hash_key) = p.key_descriptor.hash else {
        return;
    };
    let mut acc: u64 = 0;
    for (k, v) in p.entries.iter() {
        let mut kh = crate::descriptor::StructHasher::new();
        let k_payload = k.value().payload::<u8>() as *const u8;
        // SAFETY: key payload matches the key descriptor.
        unsafe { hash_key(k_payload, &mut kh) };
        let v_i = unsafe { *(v.payload::<i64>()) };
        let pair = kh
            .finish()
            .wrapping_mul(0x9e3779b97f4a7c15)
            .wrapping_add(v_i as u64);
        acc ^= pair;
    }
    hasher.write_bytes(&acc.to_le_bytes());
}

/// Descriptor for `Counter[T]` (§6.2).
pub static COUNTER: TypeDescriptor = TypeDescriptor::builtin::<CounterPayload>(
    BuiltinTypeId::Counter,
    "Counter",
    counter_trace,
    counter_drop,
    counter_format,
    Some(counter_equals),
    Some(counter_hash),
    // Not orderable: only Int/Byte/Char/Float/Text are (ADR-045).
    None,
)
.with_owned_bytes(counter_owned_bytes);

/// The heap bytes `Counter[K]` owns beyond its payload, for GC pacing (RT-04).
/// `capacity`, not `len`: the buffer's real footprint is what the collector is
/// paced against.
///
/// # Safety
/// `payload` must point at an initialized `CounterPayload`.
unsafe fn counter_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized CounterPayload.
    let p = unsafe { &*(payload as *const CounterPayload) };
    p.entries.capacity() * (std::mem::size_of::<DynamicKey>() + std::mem::size_of::<GcRef>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_set_counter_descriptors_report_capabilities() {
        // All three hash collections are eq-able and hashable themselves (so a
        // Map/Set/Counter can be a value in another collection).
        assert!(MAP.is_equatable() && MAP.is_hashable());
        assert!(SET.is_equatable() && SET.is_hashable());
        assert!(COUNTER.is_equatable() && COUNTER.is_hashable());
        assert_eq!(MAP.name, "Map");
        assert_eq!(SET.name, "Set");
        assert_eq!(COUNTER.name, "Counter");
    }

    /// Render a payload through its own `format` callback, without allocating a
    /// GC object to hold it.
    fn rendered<P>(format: unsafe fn(*const u8, &mut dyn fmt::Write), payload: &P) -> String {
        let mut s = String::new();
        // SAFETY: `payload` is an initialized value of the type `format` reads.
        unsafe { format((payload as *const P).cast::<u8>(), &mut s) };
        s
    }

    fn int_key(rt: &crate::Runtime, n: i64) -> DynamicKey {
        DynamicKey::new(rt.alloc_int(n))
    }

    /// RT-16. Rust randomizes hash-table iteration order **per process**, so
    /// the same program printing the same `Map` gave a different string on
    /// every run. §19 promises deterministic formatting, and a program whose
    /// expected output cannot be written down does not have one.
    ///
    /// Two maps built by inserting the same pairs in opposite orders is the
    /// cheap in-process proxy: it does not reproduce the cross-run seed, but it
    /// does catch "the output follows the table's internal layout".
    #[test]
    fn map_formatting_does_not_follow_hash_table_order() {
        let rt = crate::Runtime::new();
        let build = |order: [i64; 6]| MapPayload {
            key_descriptor: &crate::scalars::INT,
            value_descriptor: &crate::scalars::INT,
            entries: order
                .iter()
                .map(|&n| (int_key(&rt, n), rt.alloc_int(n * 10)))
                .collect(),
        };

        let forward = rendered(map_format, &build([1, 2, 3, 4, 5, 6]));
        let backward = rendered(map_format, &build([6, 5, 4, 3, 2, 1]));
        assert_eq!(forward, backward, "insertion order must not show through");
        // Sorted by the *rendered* entry, which is the only total order
        // available until D3 populates `TypeDescriptor::compare` — so `10:`
        // would precede `2:`. Every key here is one digit, so this is also
        // numeric order, and the test says which property it is relying on.
        assert_eq!(forward, "{1: 10, 2: 20, 3: 30, 4: 40, 5: 50, 6: 60}");
    }

    #[test]
    fn set_formatting_does_not_follow_hash_table_order() {
        let rt = crate::Runtime::new();
        let build = |order: [i64; 5]| SetPayload {
            element_descriptor: &crate::scalars::INT,
            entries: order.iter().map(|&n| int_key(&rt, n)).collect(),
        };

        let forward = rendered(set_format, &build([3, 1, 4, 5, 9]));
        let backward = rendered(set_format, &build([9, 5, 4, 1, 3]));
        assert_eq!(forward, backward);
        assert_eq!(forward, "{1, 3, 4, 5, 9}");
    }

    #[test]
    fn counter_formatting_does_not_follow_hash_table_order() {
        let rt = crate::Runtime::new();
        let build = |order: [i64; 4]| CounterPayload {
            key_descriptor: &crate::scalars::INT,
            entries: order
                .iter()
                .map(|&n| (int_key(&rt, n), rt.alloc_int(n)))
                .collect(),
        };

        let forward = rendered(counter_format, &build([2, 7, 1, 8]));
        let backward = rendered(counter_format, &build([8, 1, 7, 2]));
        assert_eq!(forward, backward);
        assert_eq!(forward, "{1: 1, 2: 2, 7: 7, 8: 8}");
    }
}
