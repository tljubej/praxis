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

use crate::descriptor::{BuiltinTypeId, Tracer, TypeDescriptor};
use crate::dynamic_key::DynamicKey;
use crate::DynamicHasher;
use crate::GcRef;

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
        tracer.trace(k.value);
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
    let _ = out.write_str("{");
    for (i, (k, v)) in p.entries.iter().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        let k_payload = k.value.payload::<u8>() as *const u8;
        (k.descriptor.format)(k_payload, out);
        let _ = out.write_str(": ");
        let v_payload = v.payload::<u8>() as *const u8;
        (val_desc.format)(v_payload, out);
    }
    let _ = out.write_str("}");
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
        let k_payload = k.value.payload::<u8>() as *const u8;
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
    // Ordering: see the ordering ADR; no built-in declares `compare` yet.
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
        tracer.trace(k.value);
    }
}

unsafe fn set_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized SetPayload.
    unsafe { std::ptr::drop_in_place(payload as *mut SetPayload) };
}

unsafe fn set_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an initialized SetPayload.
    let p = unsafe { &*(payload as *const SetPayload) };
    let _ = out.write_str("{");
    for (i, k) in p.entries.iter().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        let k_payload = k.value.payload::<u8>() as *const u8;
        (k.descriptor.format)(k_payload, out);
    }
    let _ = out.write_str("}");
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
        let k_payload = k.value.payload::<u8>() as *const u8;
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
    // Ordering: see the ordering ADR; no built-in declares `compare` yet.
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
        tracer.trace(k.value);
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
    let _ = out.write_str("{");
    for (i, (k, v)) in p.entries.iter().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        let k_payload = k.value.payload::<u8>() as *const u8;
        (k.descriptor.format)(k_payload, out);
        let _ = out.write_str(": ");
        let v_payload = v.payload::<u8>() as *const u8;
        // Counter values are always Int; format via INT.
        (crate::scalars::INT.format)(v_payload, out);
    }
    let _ = out.write_str("}");
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
        let k_payload = k.value.payload::<u8>() as *const u8;
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
    // Ordering: see the ordering ADR; no built-in declares `compare` yet.
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
}
