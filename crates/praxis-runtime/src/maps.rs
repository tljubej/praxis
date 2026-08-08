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
use std::fmt::{self, Write as _};

use crate::collections::nullable;
use crate::descriptor::{BuiltinTypeId, FormatSink, Tracer, TypeDescriptor};
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
    out: &mut FormatSink<'_>,
    descriptor: &TypeDescriptor,
    value: GcRef,
) {
    let payload = value.payload::<u8>() as *const u8;
    // SAFETY: the caller guarantees the payload matches the descriptor.
    unsafe { (descriptor.format)(payload, out) };
}

/// Write already-ordered `entries` between `open` and `close`, comma-separated.
///
/// It does **not** sort. It used to, on the rendered entry, because at the time
/// that was the only total order in reach — which is why `{10: a, 9: b}` printed
/// `10` first, and why a printed `Map` and a `for` over the same map could
/// disagree: one sorted the whole `"key: value"` string and the other sorted the
/// key alone, and `':'` is below `'1'` in ASCII.
///
/// Both are gone. The caller orders its keys through [`ordered_entries`] or
/// [`ordered_members`] and renders in that order, so printing and iterating are
/// one order by construction rather than by two sorts that happen to agree
/// (ADR-138 decision 4). This function's only remaining job is the punctuation.
pub(crate) fn write_ordered<I: Iterator<Item = String>>(
    out: &mut dyn fmt::Write,
    open: &str,
    entries: I,
    close: &str,
) {
    let _ = out.write_str(open);
    for (i, entry) in entries.enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        let _ = out.write_str(&entry);
    }
    let _ = out.write_str(close);
}

/// The entries of a keyed collection in a **deterministic** order: sorted by the
/// key's own order (REP-18, ADR-138).
///
/// The order matters because `keys()` and `values()` promise to be index-aligned,
/// and because a `HashMap`'s own iteration order is randomized per process — the
/// same program would answer differently on two runs, which is RT-16 again in a
/// place where the value, not just the printing, depends on it.
///
/// The sort key is [`crate::ordering::container_cmp`], which is the key's own
/// `TypeDescriptor::compare` — the same callback `sorted()` and a heap's `Ord`
/// go through. That is what makes `out(m.keys())` and `out(m.keys().sorted())`
/// agree; sorting the *rendered* key instead, which is what this did until
/// ADR-138, put `10` before `2` and made a program that walked a `Map[Int, V]`
/// answer in an order no reader would predict.
///
/// `sort_by`, not `sort_unstable_by`: `container_cmp` leaves a tie only for two
/// keys that render identically, and a stable sort at least keeps the answer
/// independent of the sort's own internal choices.
///
/// # Safety
/// Every key's payload must match the descriptor it carries.
pub(crate) unsafe fn ordered_entries(entries: &HashMap<DynamicKey, GcRef>) -> Vec<(GcRef, GcRef)> {
    let mut rows: Vec<(GcRef, GcRef)> = entries.iter().map(|(k, v)| (k.value(), *v)).collect();
    // SAFETY: every key's payload matches the descriptor in its own header,
    // which is what `DynamicKey::new` reads it from.
    rows.sort_by(|a, b| unsafe { crate::ordering::container_cmp(a.0, b.0) });
    rows
}

/// The members of a `Set` in the same **deterministic** order
/// [`ordered_entries`] gives a keyed collection: sorted by the member's own
/// order (REP-15, ADR-138).
///
/// `for x in s` iterates a snapshot of this (ADR-066), so the order is the
/// program's answer and not only its printing — the same reason
/// [`ordered_entries`] exists. `set_format` renders in this order too, so
/// `out(s)`, `for x in s` and `s.sorted()` are three readings of one sequence.
///
/// # Safety
/// Every member's payload must match the descriptor it carries.
pub(crate) unsafe fn ordered_members(entries: &HashSet<DynamicKey>) -> Vec<GcRef> {
    let mut rows: Vec<GcRef> = entries.iter().map(DynamicKey::value).collect();
    // SAFETY: as `ordered_entries`.
    rows.sort_by(|a, b| unsafe { crate::ordering::container_cmp(*a, *b) });
    rows
}

/// Render already-ordered `items` between `{` and `}`, each through `render`
/// into its own scratch buffer.
///
/// The shared body of `map_format`, `set_format` and `counter_format`: the
/// three differ only in what one entry renders as — `key: value` through the
/// value's own descriptor, a bare member, `key: value` through `INT` — so that
/// is the closure and everything around it is written once here.
///
/// The style is read off `out` **before** the first buffer is built, and held
/// across them, because [`write_ordered`] borrows `out` for as long as the
/// iterator it drains — so the entries cannot read the style off the sink they
/// are ultimately written to. Every scratch buffer is a place the style could
/// be dropped, and dropping it is silent: the value still renders, just in the
/// other rendering.
///
/// It does not sort. `items` is already in the one order that printing and
/// iterating share, from [`ordered_entries`] or [`ordered_members`] (ADR-138
/// decision 4).
fn write_braced<T>(
    out: &mut FormatSink<'_>,
    items: Vec<T>,
    render: impl Fn(&mut FormatSink<'_>, T),
) {
    let style = out.style();
    let entries = items.into_iter().map(|item| {
        let mut buf = String::new();
        {
            let mut s = FormatSink::styled(&mut buf, style);
            render(&mut s, item);
        }
        buf
    });
    write_ordered(out, "{", entries, "}");
}

// ---------------------------------------------------------------------------
// Hashing
// ---------------------------------------------------------------------------

/// The odd 64-bit golden-ratio multiplier that `map_hash` and `counter_hash`
/// mix a key's hash through before adding the value's, so that `{k1: v2}` and
/// `{k2: v1}` do not cancel under the commutative (XOR) accumulator.
///
/// Named because the literal was written twice, **not** because the two have to
/// agree. `MAP` and `COUNTER` are distinct descriptors that are never
/// dispatched against each other, and they already hash the same logical
/// contents differently on purpose: `map_hash` puts the value through its own
/// descriptor's `hash` callback, `counter_hash` reads the raw `i64`. Nothing
/// compares the two hashes, so changing this for one of them alone would be
/// legal — it is one constant because it is one idea.
const KEY_HASH_MIX: u64 = 0x9e3779b97f4a7c15;

// ===========================================================================
// Map[K, V]
// ===========================================================================

/// The `Map[K, V]` payload (§11.3). Both descriptors are **labels**: what the
/// construction site knew about the type, or **null** when it knew nothing
/// (REP-41). Neither is the authority for an element-wise operation — every key
/// carries its own descriptor on its [`DynamicKey`] and every value carries one
/// in its object header, and that is what `format`/`equals`/`hash` dispatch
/// through. ADR-066 decision 5 is the rule: a null descriptor slot is legal and
/// means "the value's own descriptor answers".
///
/// The label used to be non-nullable, so `praxis_map_new` had to spell an
/// unknown key type `INT`, and every `Map`'s value label was `INT`
/// unconditionally — which made a `Map[Text, Text]`'s value read as an `i64` by
/// anything that trusted it.
#[repr(C)]
pub struct MapPayload {
    /// The descriptor for every key, or null when the construction site had no
    /// static key type. Read it through [`MapPayload::key`].
    pub key_descriptor: *const TypeDescriptor,
    /// The descriptor for every value, or null when unknown. Read it through
    /// [`MapPayload::value`].
    pub value_descriptor: *const TypeDescriptor,
    /// The entries. Keys are `DynamicKey`; values are `GcRef`.
    pub entries: HashMap<DynamicKey, GcRef>,
}

impl MapPayload {
    /// The key label, or `None` when this map was never told its key type.
    #[must_use]
    pub fn key(&self) -> Option<&'static TypeDescriptor> {
        nullable(self.key_descriptor)
    }

    /// The value label, or `None` when this map was never told its value type.
    #[must_use]
    pub fn value(&self) -> Option<&'static TypeDescriptor> {
        nullable(self.value_descriptor)
    }
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

unsafe fn map_format(payload: *const u8, out: &mut FormatSink<'_>) {
    // SAFETY: caller guarantees `payload` points at an initialized MapPayload.
    let p = unsafe { &*(payload as *const MapPayload) };
    // Order first, render second: a printed `Map` is the same sequence a `for`
    // over it walks, because both read `ordered_entries` (ADR-138 decision 4).
    // Rendering first and sorting the strings is what made `{a1: 2, a: 1}` print
    // in an order `m.keys()` disagreed with.
    // SAFETY: every key's payload matches the descriptor its `DynamicKey` carries.
    let rows = unsafe { ordered_entries(&p.entries) };
    write_braced(out, rows, |s, (k, v)| {
        // SAFETY: the key's payload matches its own header's descriptor, and
        // so does the value's. Rendering through the *map's* value label is
        // what printed a `Map[Text, Text]` as integers, because that label
        // was `INT` unconditionally (REP-42).
        unsafe {
            render_into(s, k.descriptor(), k);
            let _ = s.write_str(": ");
            render_into(s, v.descriptor(), v);
        }
    });
}

unsafe fn map_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at initialized MapPayloads.
    let pa = unsafe { &*(a as *const MapPayload) };
    let pb = unsafe { &*(b as *const MapPayload) };
    if pa.entries.len() != pb.entries.len() {
        return false;
    }
    // Two maps are equal iff they have the same keys with equal values. Each
    // pair compares through the *left* value's own descriptor, after checking
    // the right value carries the same one: values of two different types are
    // never equal, and dispatching one type's `equals` against the other's
    // payload is precisely the wrong-type read a shared label licensed.
    for (k, va) in pa.entries.iter() {
        // `get` uses DynamicKey's PartialEq (structural, via the descriptor).
        let Some(vb) = pb.entries.get(k) else {
            return false;
        };
        if !std::ptr::eq(va.descriptor(), vb.descriptor()) {
            return false;
        }
        let Some(eq) = va.descriptor().equals else {
            return false;
        };
        let va_p = va.payload::<u8>() as *const u8;
        let vb_p = vb.payload::<u8>() as *const u8;
        // SAFETY: both values carry the descriptor `eq` came from.
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
    let mut acc: u64 = 0;
    for (k, v) in p.entries.iter() {
        // Each half hashes through its *own* descriptor rather than the map's
        // label: equal values must hash equally, and only the value knows what
        // it is.
        let (Some(hash_key), Some(hash_val)) = (k.descriptor().hash, v.descriptor().hash) else {
            return;
        };
        let mut kh = crate::descriptor::StructHasher::new();
        let k_payload = k.value().payload::<u8>() as *const u8;
        // SAFETY: key payload matches the descriptor its `DynamicKey` carries.
        unsafe { hash_key(k_payload, &mut kh) };
        let mut vh = crate::descriptor::StructHasher::new();
        let v_payload = v.payload::<u8>() as *const u8;
        // SAFETY: value payload matches the descriptor in its own header.
        unsafe { hash_val(v_payload, &mut vh) };
        // Pair the key and value hashes together before XOR, so (k1:v2) ≠ (k2:v1).
        let pair = kh
            .finish()
            .wrapping_mul(KEY_HASH_MIX)
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
    // No container order: a mutable collection can never be a `Map` key or a
    // `Set` member (ADR-057 D4), so nothing ever has to put one in a
    // deterministic sequence (ADR-138).
    None,
)
.with_owned_bytes(map_owned_bytes);

impl MapPayload {
    /// The hash table this payload owns beyond its GC block, for GC pacing
    /// (RT-04) — `capacity` slots of key *and* value, not `len`.
    ///
    /// One statement of the size, with two readers (ADR-121):
    /// [`VecPayload::owned_bytes`](crate::collections::VecPayload::owned_bytes)
    /// is that statement.
    #[must_use]
    pub(crate) fn owned_bytes(&self) -> usize {
        self.entries.capacity() * (std::mem::size_of::<DynamicKey>() + std::mem::size_of::<GcRef>())
    }
}

unsafe fn map_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized MapPayload.
    let p = unsafe { &*(payload as *const MapPayload) };
    p.owned_bytes()
}

// ===========================================================================
// Set[T]
// ===========================================================================

/// The `Set[T]` payload (§11.3). The element descriptor is a **label** — what
/// the construction site knew, or null when it knew nothing (REP-41). Each
/// member's `DynamicKey` carries its own descriptor, which is what `hash` and
/// `format` dispatch through.
#[repr(C)]
pub struct SetPayload {
    /// The descriptor for every element, or null when the construction site had
    /// no static element type. Read it through [`SetPayload::element`].
    pub element_descriptor: *const TypeDescriptor,
    /// The elements, as `DynamicKey`s.
    pub entries: HashSet<DynamicKey>,
}

impl SetPayload {
    /// The element label, or `None` when this set was never told its element
    /// type.
    #[must_use]
    pub fn element(&self) -> Option<&'static TypeDescriptor> {
        nullable(self.element_descriptor)
    }
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

unsafe fn set_format(payload: *const u8, out: &mut FormatSink<'_>) {
    // SAFETY: caller guarantees `payload` points at an initialized SetPayload.
    let p = unsafe { &*(payload as *const SetPayload) };
    // The order a `for` over this set walks (ADR-138 decision 4).
    // SAFETY: every member's payload matches the descriptor it carries.
    let members = unsafe { ordered_members(&p.entries) };
    write_braced(out, members, |s, m| {
        // SAFETY: the member's payload matches its own header's descriptor.
        unsafe { render_into(s, m.descriptor(), m) };
    });
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
    let mut acc: u64 = 0;
    for k in p.entries.iter() {
        // Through the member's own descriptor, not the set's label — `format`
        // has always read `k.descriptor()` here and this is the same rule.
        let Some(hash_el) = k.descriptor().hash else {
            return;
        };
        let mut h = crate::descriptor::StructHasher::new();
        let k_payload = k.value().payload::<u8>() as *const u8;
        // SAFETY: element payload matches the descriptor its key carries.
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
    // No container order: a mutable collection can never be a `Map` key or a
    // `Set` member (ADR-057 D4), so nothing ever has to put one in a
    // deterministic sequence (ADR-138).
    None,
)
.with_owned_bytes(set_owned_bytes);

impl SetPayload {
    /// The hash table this payload owns beyond its GC block, for GC pacing
    /// (RT-04) — `capacity`, not `len`.
    ///
    /// One statement of the size, with two readers (ADR-121):
    /// [`VecPayload::owned_bytes`](crate::collections::VecPayload::owned_bytes)
    /// is that statement.
    #[must_use]
    pub(crate) fn owned_bytes(&self) -> usize {
        self.entries.capacity() * std::mem::size_of::<DynamicKey>()
    }
}

unsafe fn set_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized SetPayload.
    let p = unsafe { &*(payload as *const SetPayload) };
    p.owned_bytes()
}

// ===========================================================================
// Counter[T]
// ===========================================================================

/// The `Counter[T]` payload (§6.2, §11.3). A map whose values are always `Int`
/// and whose absent keys read as zero. Backed by `HashMap<DynamicKey, GcRef>`
/// where each value is a boxed `Int`; the key descriptor selects hash/eq.
#[repr(C)]
pub struct CounterPayload {
    /// The descriptor for every key, or null when the construction site had no
    /// static key type (REP-41). A label, not the authority: each key's
    /// `DynamicKey` carries its own. Read it through [`CounterPayload::key`].
    pub key_descriptor: *const TypeDescriptor,
    /// The entries: key → boxed Int value.
    pub entries: HashMap<DynamicKey, GcRef>,
}

impl CounterPayload {
    /// The key label, or `None` when this counter was never told its key type.
    #[must_use]
    pub fn key(&self) -> Option<&'static TypeDescriptor> {
        nullable(self.key_descriptor)
    }
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

unsafe fn counter_format(payload: *const u8, out: &mut FormatSink<'_>) {
    // SAFETY: caller guarantees `payload` points at an initialized CounterPayload.
    let p = unsafe { &*(payload as *const CounterPayload) };
    // As `map_format`: the order is decided over the keys, then rendered.
    // SAFETY: every key's payload matches the descriptor it carries.
    let rows = unsafe { ordered_entries(&p.entries) };
    write_braced(out, rows, |s, (k, v)| {
        // SAFETY: the key's payload matches its descriptor; a Counter's
        // values are always `Int` (§6.2).
        unsafe {
            render_into(s, k.descriptor(), k);
            let _ = s.write_str(": ");
            render_into(s, &crate::scalars::INT, v);
        }
    });
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
    let mut acc: u64 = 0;
    for (k, v) in p.entries.iter() {
        // Through the key's own descriptor, not the counter's label (REP-41).
        let Some(hash_key) = k.descriptor().hash else {
            return;
        };
        let mut kh = crate::descriptor::StructHasher::new();
        let k_payload = k.value().payload::<u8>() as *const u8;
        // SAFETY: key payload matches the key descriptor.
        unsafe { hash_key(k_payload, &mut kh) };
        let v_i = unsafe { *(v.payload::<i64>()) };
        let pair = kh
            .finish()
            .wrapping_mul(KEY_HASH_MIX)
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
    // No container order: a mutable collection can never be a `Map` key or a
    // `Set` member (ADR-057 D4), so nothing ever has to put one in a
    // deterministic sequence (ADR-138).
    None,
)
.with_owned_bytes(counter_owned_bytes);

impl CounterPayload {
    /// The hash table this payload owns beyond its GC block, for GC pacing
    /// (RT-04) — `capacity` slots of key *and* boxed-`Int` value, not `len`.
    ///
    /// One statement of the size, with two readers (ADR-121):
    /// [`VecPayload::owned_bytes`](crate::collections::VecPayload::owned_bytes)
    /// is that statement.
    #[must_use]
    pub(crate) fn owned_bytes(&self) -> usize {
        self.entries.capacity() * (std::mem::size_of::<DynamicKey>() + std::mem::size_of::<GcRef>())
    }
}

unsafe fn counter_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized CounterPayload.
    let p = unsafe { &*(payload as *const CounterPayload) };
    p.owned_bytes()
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
    fn rendered<P>(format: crate::FormatFn, payload: &P) -> String {
        rendered_styled(format, payload, crate::FormatStyle::Display)
    }

    /// [`rendered`], in a style the caller picks — for the tests that are about
    /// the two renderings differing.
    fn rendered_styled<P>(
        format: crate::FormatFn,
        payload: &P,
        style: crate::FormatStyle,
    ) -> String {
        let mut s = String::new();
        let mut sink = FormatSink::styled(&mut s, style);
        // SAFETY: `payload` is an initialized value of the type `format` reads.
        unsafe { format((payload as *const P).cast::<u8>(), &mut sink) };
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
        // Ordered by the key's own `compare` (ADR-138), which for `Int` is
        // numeric. Every key here is one digit, so the lexicographic order this
        // used to use would agree — `a_set_of_ints_orders_numerically_and_not_
        // lexicographically` is the test that tells the two apart.
        assert_eq!(forward, "{1: 10, 2: 20, 3: 30, 4: 40, 5: 50, 6: 60}");
    }

    /// A container's [`FormatStyle`] reaches its **elements**, including across
    /// the scratch buffer `map_format` renders each entry into.
    ///
    /// This is the property that decided the design: a `format_debug` field
    /// beside `format` would have quoted a `Text` local and left a `Text` inside
    /// a `Map` bare, because the container's callback would have had no way to
    /// know which of the two it was running as. The buffer is the place the
    /// style is easiest to drop, since the entries are rendered before the sink
    /// they end up in is written to at all.
    #[test]
    fn a_containers_style_reaches_the_values_inside_it() {
        let rt = crate::Runtime::new();
        let payload = MapPayload {
            key_descriptor: &crate::text::TEXT,
            value_descriptor: &crate::text::TEXT,
            entries: [(
                DynamicKey::new(rt.alloc_text("k")),
                // Empty on purpose: in the program's rendering this entry's
                // value is zero characters wide.
                rt.alloc_text(""),
            )]
            .into_iter()
            .collect(),
        };
        assert_eq!(
            rendered_styled(map_format, &payload, crate::FormatStyle::Display),
            "{k: }",
            "the program's rendering is unchanged, empty value and all"
        );
        assert_eq!(
            rendered_styled(map_format, &payload, crate::FormatStyle::Debug),
            r#"{"k": ""}"#,
            "the debugger's reaches both the key and the value"
        );
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

    /// **REP-15.** A `Set`'s snapshot order is the order it prints in, and
    /// neither follows the hash table's own.
    ///
    /// This matters more than the formatting rule it shares: `for x in s`
    /// iterates the snapshot, so the order is the *answer* a program computes
    /// and not only the string it prints. Rust randomizes the table's order per
    /// process, so a program that concatenates its members would answer
    /// differently on two runs.
    #[test]
    fn a_sets_members_come_out_in_the_order_it_prints_them() {
        let rt = crate::Runtime::new();
        let build = |order: [i64; 5]| SetPayload {
            element_descriptor: &crate::scalars::INT,
            entries: order.iter().map(|&n| int_key(&rt, n)).collect(),
        };

        let read_back = |p: &SetPayload| -> Vec<i64> {
            // SAFETY: every member is an `Int` matching the element descriptor.
            unsafe { ordered_members(&p.entries) }
                .into_iter()
                .map(|m| unsafe { *m.payload::<i64>() })
                .collect()
        };
        let forward = read_back(&build([3, 1, 4, 5, 9]));
        let backward = read_back(&build([9, 5, 4, 1, 3]));
        assert_eq!(forward, backward, "insertion order must not show through");
        assert_eq!(forward, vec![1, 3, 4, 5, 9]);
        // …and it is the same order `set_format` writes, which is the property
        // that keeps `out(s)` and `for x in s` from disagreeing.
        assert_eq!(
            rendered(set_format, &build([3, 1, 4, 5, 9])),
            "{1, 3, 4, 5, 9}"
        );
    }

    /// `keys()` and `values()` are index-aligned because they share one order,
    /// and a `for` over the same map is the third caller of it (REP-15/REP-18).
    #[test]
    fn a_keyed_collections_entries_come_out_paired() {
        let rt = crate::Runtime::new();
        let p = MapPayload {
            key_descriptor: &crate::scalars::INT,
            value_descriptor: &crate::scalars::INT,
            entries: [3, 1, 2]
                .iter()
                .map(|&n| (int_key(&rt, n), rt.alloc_int(n * 10)))
                .collect(),
        };
        // SAFETY: every key and value is an `Int` matching its descriptor.
        let rows = unsafe { ordered_entries(&p.entries) };
        let pairs: Vec<(i64, i64)> = rows
            .into_iter()
            .map(|(k, v)| unsafe { (*k.payload::<i64>(), *v.payload::<i64>()) })
            .collect();
        assert_eq!(pairs, vec![(1, 10), (2, 20), (3, 30)]);
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

    /// **The regression gate for ADR-138.** A `Set[Int]` orders by the number,
    /// not by how the number prints.
    ///
    /// The handover's own case: an Advent-of-Code solve walked a `Set[Int]` and
    /// got `10, 100, 2, 9`, because the sort key was the rendered member and
    /// `"10" < "2"`. Nothing reported it — a wrong order out of a `for` is an
    /// *answer*, not a formatting wart — so this is the shape of defect a test
    /// has to hold down rather than a reader.
    #[test]
    fn a_set_of_ints_orders_numerically_and_not_lexicographically() {
        let rt = crate::Runtime::new();
        let build = |order: [i64; 4]| SetPayload {
            element_descriptor: &crate::scalars::INT,
            entries: order.iter().map(|&n| int_key(&rt, n)).collect(),
        };
        let read_back = |p: &SetPayload| -> Vec<i64> {
            // SAFETY: every member is an `Int` matching the element descriptor.
            unsafe { ordered_members(&p.entries) }
                .into_iter()
                .map(|m| unsafe { *m.payload::<i64>() })
                .collect()
        };
        assert_eq!(read_back(&build([9, 10, 100, 2])), vec![2, 9, 10, 100]);
        assert_eq!(read_back(&build([2, 100, 10, 9])), vec![2, 9, 10, 100]);
        // …and the printing is that same sequence, which is what makes `out(s)`
        // and `out(s.sorted())` agree.
        assert_eq!(
            rendered(set_format, &build([9, 10, 100, 2])),
            "{2, 9, 10, 100}"
        );
    }

    /// A keyed collection prints in the order it iterates (ADR-138 decision 4).
    ///
    /// `"a"` and `"a1"` are the pair that used to tell the two apart: printing
    /// sorted the whole rendered *entry*, so `"a1: 2"` came before `"a: 1"`
    /// because `'1'` (0x31) is below `':'` (0x3A), while `keys()` and a `for`
    /// sorted the rendered *key* and answered `a, a1`. One `Map`, two orders,
    /// and a program that printed it and walked it disagreed with itself.
    #[test]
    fn a_keyed_collection_prints_in_the_order_it_iterates() {
        let rt = crate::Runtime::new();
        let p = MapPayload {
            key_descriptor: &crate::text::TEXT,
            value_descriptor: &crate::scalars::INT,
            entries: [("a1", 2), ("a", 1)]
                .iter()
                .map(|&(k, v)| (DynamicKey::new(rt.alloc_text(k)), rt.alloc_int(v)))
                .collect(),
        };
        // SAFETY: every key is a `Text` and every value an `Int`.
        let iterated: Vec<String> = unsafe { ordered_entries(&p.entries) }
            .into_iter()
            .map(|(k, _)| {
                let mut s = String::new();
                unsafe { render_into(&mut crate::FormatSink::display(&mut s), k.descriptor(), k) };
                s
            })
            .collect();
        assert_eq!(iterated, vec!["a".to_string(), "a1".to_string()]);
        assert_eq!(rendered(map_format, &p), "{a: 1, a1: 2}");
    }

    /// A tuple key orders element-wise — day 11's memo key shape,
    /// `Map[(Text, Int), V]`, and the reason `TUPLE.compare` had to be
    /// populated rather than left to the rendered-form fallback: `"(a, 10)"`
    /// sorts before `"(a, 9)"` and `(a, 9)` does not.
    #[test]
    fn a_tuple_keyed_map_orders_element_wise() {
        let mut rt = crate::Runtime::new();
        let schema: &'static crate::tuples::TupleSchema =
            Box::leak(Box::new(crate::tuples::TupleSchema {
                descriptors: Box::leak(
                    vec![
                        &crate::text::TEXT as *const TypeDescriptor,
                        &crate::scalars::INT as *const TypeDescriptor,
                    ]
                    .into_boxed_slice(),
                ),
            }));
        let pairs = [("a", 10), ("a", 9), ("b", 1)];
        let values: Vec<(GcRef, GcRef)> = pairs
            .iter()
            .map(|&(t, n)| (rt.alloc_text(t), rt.alloc_int(n)))
            .collect();
        let mut ctx = rt.context();
        let keys: Vec<GcRef> = values
            .into_iter()
            .map(|(t, n)| {
                // SAFETY: a live context, and the schema names exactly these
                // two element types.
                unsafe {
                    let tup = crate::abi::praxis_alloc_tuple(&mut ctx, schema);
                    crate::abi::praxis_tuple_set(&mut ctx, tup, 0, t);
                    crate::abi::praxis_tuple_set(&mut ctx, tup, 1, n);
                    tup
                }
            })
            .collect();
        let p = MapPayload {
            key_descriptor: &crate::tuples::TUPLE,
            value_descriptor: &crate::scalars::INT,
            entries: keys
                .into_iter()
                .map(|k| (DynamicKey::new(k), rt.alloc_int(0)))
                .collect(),
        };
        assert_eq!(
            rendered(map_format, &p),
            "{(a, 9): 0, (a, 10): 0, (b, 1): 0}"
        );
    }

    /// A `Float` key orders numerically, with NaN last (ADR-045 decision 2).
    ///
    /// The NaN rule was written when nothing consumed it at these three call
    /// sites; this is the first test that ties it to the order a `Set` prints
    /// and iterates in. Rendered-form order put `10.25` between `1.5` and `2.0`.
    #[test]
    fn a_float_keyed_set_orders_numerically_and_puts_nan_last() {
        let rt = crate::Runtime::new();
        let p = SetPayload {
            element_descriptor: &crate::scalars::FLOAT,
            entries: [2.0, f64::NAN, 10.25, 1.5]
                .iter()
                .map(|&f| DynamicKey::new(rt.alloc_float(f)))
                .collect(),
        };
        assert_eq!(rendered(set_format, &p), "{1.5, 2.0, 10.25, NaN}");
    }
}
