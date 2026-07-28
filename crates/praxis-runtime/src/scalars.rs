//! Built-in scalar type descriptors (§4.3, §11.4).
//!
//! Each scalar type has one `static` [`TypeDescriptor`] (`UNIT`, `BOOL`, `INT`,
//! `BYTE`, `CHAR`, `FLOAT`), so its address is its identity. Every payload-aware
//! operation routes through these descriptors; there are no type switches
//! elsewhere (§11.4).
//!
//! Scalar payloads contain no `GcRef`s, so every scalar `trace` is a no-op —
//! scalars are the leaf case that proves the descriptor machinery without
//! exercising nested references. Composite tracing is covered by `Vec[T]`
//! (ADR-013).

use std::cmp::Ordering;
use std::fmt;

use crate::descriptor::{hash_value, BuiltinTypeId, DynamicHasher, Tracer, TypeDescriptor};

// ---- payload types ---------------------------------------------------------
//
// These are the concrete in-payload Rust representations of each scalar. They
// are `Copy` (no `Drop`), so every scalar `drop_value` is a no-op.

/// `Unit` payload: no data.
pub type UnitPayload = ();

/// `Bool` payload: `0` is false, `1` is true (§4.3).
pub type BoolPayload = u8;

/// `Int` payload: signed 64-bit (§4.3).
pub type IntPayload = i64;

/// `Byte` payload: unsigned 8-bit (§4.3).
pub type BytePayload = u8;

/// `Char` payload: a validated Unicode scalar value (§4.3). Stored as `u32`.
pub type CharPayload = u32;

/// `Float` payload: IEEE 754 binary64 (§4.3).
pub type FloatPayload = f64;

// ---- Unit ------------------------------------------------------------------

unsafe fn unit_trace(_: *mut u8, _: &mut dyn Tracer) {}
unsafe fn unit_drop(_: *mut u8) {}
unsafe fn unit_format(_: *const u8, out: &mut dyn fmt::Write) {
    let _ = out.write_str("Unit");
}
unsafe fn unit_equals(_: *const u8, _: *const u8) -> bool {
    true
}
unsafe fn unit_hash(_: *const u8, hasher: &mut dyn DynamicHasher) {
    // Unit is a singleton; all instances hash equally.
    hash_value(hasher, &());
}

/// Descriptor for the `Unit` scalar (§4.3).
pub static UNIT: TypeDescriptor = TypeDescriptor::builtin::<UnitPayload>(
    BuiltinTypeId::Unit,
    "Unit",
    unit_trace,
    unit_drop,
    unit_format,
    Some(unit_equals),
    Some(unit_hash),
    // Not orderable: only Int/Byte/Char/Float/Text are (ADR-045).
    None,
);

// ---- Bool ------------------------------------------------------------------

unsafe fn bool_trace(_: *mut u8, _: &mut dyn Tracer) {}
unsafe fn bool_drop(_: *mut u8) {}
unsafe fn bool_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at a `BoolPayload`.
    let v = unsafe { *(payload as *const BoolPayload) };
    let _ = out.write_str(if v != 0 { "true" } else { "false" });
}
unsafe fn bool_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at `BoolPayload`s.
    unsafe { *(a as *const BoolPayload) == *(b as *const BoolPayload) }
}
unsafe fn bool_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at a `BoolPayload`.
    let v = unsafe { *(payload as *const BoolPayload) };
    hash_value(hasher, &v);
}

/// Descriptor for the `Bool` scalar (§4.3).
pub static BOOL: TypeDescriptor = TypeDescriptor::builtin::<BoolPayload>(
    BuiltinTypeId::Bool,
    "Bool",
    bool_trace,
    bool_drop,
    bool_format,
    Some(bool_equals),
    Some(bool_hash),
    // Not orderable: only Int/Byte/Char/Float/Text are (ADR-045).
    None,
);

// ---- Int -------------------------------------------------------------------

unsafe fn int_trace(_: *mut u8, _: &mut dyn Tracer) {}
unsafe fn int_drop(_: *mut u8) {}
unsafe fn int_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an `IntPayload`.
    let v = unsafe { *(payload as *const IntPayload) };
    let _ = write!(out, "{v}");
}
unsafe fn int_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at `IntPayload`s.
    unsafe { *(a as *const IntPayload) == *(b as *const IntPayload) }
}
unsafe fn int_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at an `IntPayload`.
    let v = unsafe { *(payload as *const IntPayload) };
    hash_value(hasher, &v);
}
unsafe fn int_compare(a: *const u8, b: *const u8) -> Ordering {
    // SAFETY: caller guarantees both pointers point at `IntPayload`s.
    unsafe { (*(a as *const IntPayload)).cmp(&*(b as *const IntPayload)) }
}

/// Descriptor for the `Int` scalar (§4.3).
pub static INT: TypeDescriptor = TypeDescriptor::builtin::<IntPayload>(
    BuiltinTypeId::Int,
    "Int",
    int_trace,
    int_drop,
    int_format,
    Some(int_equals),
    Some(int_hash),
    // Signed numeric order (ADR-045).
    Some(int_compare),
);

// ---- Byte ------------------------------------------------------------------

unsafe fn byte_trace(_: *mut u8, _: &mut dyn Tracer) {}
unsafe fn byte_drop(_: *mut u8) {}
unsafe fn byte_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at a `BytePayload`.
    let v = unsafe { *(payload as *const BytePayload) };
    let _ = write!(out, "{v}");
}
unsafe fn byte_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at `BytePayload`s.
    unsafe { *(a as *const BytePayload) == *(b as *const BytePayload) }
}
unsafe fn byte_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at a `BytePayload`.
    let v = unsafe { *(payload as *const BytePayload) };
    hash_value(hasher, &v);
}
unsafe fn byte_compare(a: *const u8, b: *const u8) -> Ordering {
    // SAFETY: caller guarantees both pointers point at `BytePayload`s.
    unsafe { (*(a as *const BytePayload)).cmp(&*(b as *const BytePayload)) }
}

/// Descriptor for the `Byte` scalar (§4.3).
pub static BYTE: TypeDescriptor = TypeDescriptor::builtin::<BytePayload>(
    BuiltinTypeId::Byte,
    "Byte",
    byte_trace,
    byte_drop,
    byte_format,
    Some(byte_equals),
    Some(byte_hash),
    // Unsigned numeric order (ADR-045).
    Some(byte_compare),
);

// ---- Char ------------------------------------------------------------------

unsafe fn char_trace(_: *mut u8, _: &mut dyn Tracer) {}
unsafe fn char_drop(_: *mut u8) {}
unsafe fn char_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at a validated `CharPayload`.
    let raw = unsafe { *(payload as *const CharPayload) };
    match char::from_u32(raw) {
        Some(c) => {
            let _ = write!(out, "{c}");
        }
        // Should not happen for a constructed Char, but never panic across a
        // descriptor callback (§10.4 spirit): render a replacement.
        None => {
            let _ = out.write_str("\u{FFFD}");
        }
    }
}
unsafe fn char_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at `CharPayload`s.
    unsafe { *(a as *const CharPayload) == *(b as *const CharPayload) }
}
unsafe fn char_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at a `CharPayload`.
    let v = unsafe { *(payload as *const CharPayload) };
    hash_value(hasher, &v);
}
unsafe fn char_compare(a: *const u8, b: *const u8) -> Ordering {
    // SAFETY: caller guarantees both pointers point at `CharPayload`s. The
    // payload is the Unicode scalar value, so `u32` order *is* code-point
    // order — and it is four bytes, which is why reading it as an `i64` was
    // both wrong and out of bounds (P0-12).
    unsafe { (*(a as *const CharPayload)).cmp(&*(b as *const CharPayload)) }
}

/// Descriptor for the `Char` scalar (§4.3).
pub static CHAR: TypeDescriptor = TypeDescriptor::builtin::<CharPayload>(
    BuiltinTypeId::Char,
    "Char",
    char_trace,
    char_drop,
    char_format,
    Some(char_equals),
    Some(char_hash),
    // Unicode scalar value order (ADR-045).
    Some(char_compare),
);

// ---- Float ------------------------------------------------------------------

unsafe fn float_trace(_: *mut u8, _: &mut dyn Tracer) {}
unsafe fn float_drop(_: *mut u8) {}
unsafe fn float_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at a `FloatPayload`.
    let v = unsafe { *(payload as *const FloatPayload) };
    // Rust's default `{}` formatting renders finite values in the shortest
    // round-trippable form, and `inf`/`-inf`/`NaN` as those literals (§4.12).
    let _ = write!(out, "{v}");
}
unsafe fn float_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at `FloatPayload`s.
    //
    // IEEE-754 comparison: NaN compares unequal to everything, including
    // itself. This matches the language's float comparison semantics (§4.12)
    // and the Cranelift `fcmp` lowering used in generated code.
    unsafe { *(a as *const FloatPayload) == *(b as *const FloatPayload) }
}
unsafe fn float_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at a `FloatPayload`.
    //
    // Hash the bit pattern so that equal floats hash equally (and unequal NaN
    // bit patterns can differ, which is acceptable — NaN equality is already
    // false for every NaN pair). Canonicalize -0.0 and +0.0 to the same bits
    // so they hash identically, matching `==` (which treats them as equal).
    let v = unsafe { *(payload as *const FloatPayload) };
    let bits = if v == 0.0 {
        v.to_bits() & 0x7fff_ffff_ffff_ffff
    } else {
        v.to_bits()
    };
    hash_value(hasher, &bits);
}

/// The **container** ordering of two `Float`s (ADR-045 decision 2): numeric for
/// everything `partial_cmp` can answer — which makes `-0.0` equal to `+0.0`,
/// agreeing with [`float_equals`] — and NaN last, equal to itself.
///
/// Not the source-level `<`: that is `Inst::FloatCmp`, stays IEEE-754, and
/// answers `false` whenever either operand is NaN (§4.12). This callback exists
/// because a `BinaryHeap` needs a *total* `Ord` or it corrupts its own sift
/// invariants, and `f64::total_cmp` was rejected for splitting the two zeros.
///
/// # Safety
/// Both pointers must point at `FloatPayload`s.
unsafe fn float_compare(a: *const u8, b: *const u8) -> Ordering {
    // SAFETY: caller guarantees both pointers point at `FloatPayload`s.
    let (x, y) = unsafe { (*(a as *const FloatPayload), *(b as *const FloatPayload)) };
    match x.partial_cmp(&y) {
        Some(o) => o,
        // Unordered: at least one is NaN. NaN sorts after every number and
        // ties with another NaN.
        None => match (x.is_nan(), y.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            // Unreachable: `partial_cmp` on two non-NaN f64s always answers.
            (false, false) => Ordering::Equal,
        },
    }
}

/// Descriptor for the `Float` scalar (§4.3).
pub static FLOAT: TypeDescriptor = TypeDescriptor::builtin::<FloatPayload>(
    BuiltinTypeId::Float,
    "Float",
    float_trace,
    float_drop,
    float_format,
    Some(float_equals),
    Some(float_hash),
    // Numeric order with NaN last (ADR-045).
    Some(float_compare),
);

// ---- validation helper -----------------------------------------------------

/// True iff `v` is a valid Unicode scalar value (a `Char` payload invariant).
/// Used by the allocation helpers to uphold §4.3's "validated scalar value".
pub(crate) fn is_valid_char(v: u32) -> bool {
    char::from_u32(v).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::StructHasher;

    /// Exercise every scalar descriptor's format/equals/hash against a stack
    /// payload, proving the vtable is wired correctly without going through the
    /// allocator. This is the unit-level check; allocation/reclamation are
    /// tested through the `Heap` in `heap.rs`.
    #[test]
    fn scalar_descriptors_format_equality_hash() {
        use std::ptr;
        let mut buf = String::new();

        // Unit
        buf.clear();
        // SAFETY: Unit's format ignores its payload pointer.
        unsafe { (UNIT.format)(ptr::null(), &mut buf) };
        assert_eq!(buf, "Unit");

        // Bool
        let t: BoolPayload = 1;
        let f: BoolPayload = 0;
        buf.clear();
        unsafe { (BOOL.format)(ptr::addr_of!(t), &mut buf) };
        assert_eq!(buf, "true");
        buf.clear();
        unsafe { (BOOL.format)(ptr::addr_of!(f), &mut buf) };
        assert_eq!(buf, "false");
        assert!(unsafe { (BOOL.equals.unwrap())(ptr::addr_of!(t), ptr::addr_of!(t)) });
        assert!(!unsafe { (BOOL.equals.unwrap())(ptr::addr_of!(t), ptr::addr_of!(f)) });

        // Int
        let a: IntPayload = 42;
        let b: IntPayload = -7;
        buf.clear();
        unsafe { (INT.format)(ptr::addr_of!(a) as *const u8, &mut buf) };
        assert_eq!(buf, "42");
        assert!(unsafe {
            (INT.equals.unwrap())(ptr::addr_of!(a) as *const u8, ptr::addr_of!(a) as *const u8)
        });
        assert!(!unsafe {
            (INT.equals.unwrap())(ptr::addr_of!(a) as *const u8, ptr::addr_of!(b) as *const u8)
        });

        // Byte
        let by: BytePayload = 255;
        buf.clear();
        unsafe { (BYTE.format)(ptr::addr_of!(by), &mut buf) };
        assert_eq!(buf, "255");

        // Char
        let ch: CharPayload = 'A' as u32;
        buf.clear();
        unsafe { (CHAR.format)(ptr::addr_of!(ch) as *const u8, &mut buf) };
        assert_eq!(buf, "A");

        // Float — finite value formats via Rust's shortest round-trip form.
        let f: FloatPayload = 2.5;
        buf.clear();
        unsafe { (FLOAT.format)(ptr::addr_of!(f) as *const u8, &mut buf) };
        assert_eq!(buf, "2.5");
        assert!(unsafe {
            (FLOAT.equals.unwrap())(ptr::addr_of!(f) as *const u8, ptr::addr_of!(f) as *const u8)
        });
        // NaN compares unequal to everything, including itself.
        let nan: FloatPayload = f64::NAN;
        assert!(!unsafe {
            (FLOAT.equals.unwrap())(
                ptr::addr_of!(nan) as *const u8,
                ptr::addr_of!(nan) as *const u8,
            )
        });
        // ±0.0 are equal.
        let pos_zero: FloatPayload = 0.0;
        let neg_zero: FloatPayload = -0.0;
        assert!(unsafe {
            (FLOAT.equals.unwrap())(
                ptr::addr_of!(pos_zero) as *const u8,
                ptr::addr_of!(neg_zero) as *const u8,
            )
        });
        // Special values format as literals.
        let inf: FloatPayload = f64::INFINITY;
        let neg_inf: FloatPayload = f64::NEG_INFINITY;
        buf.clear();
        unsafe { (FLOAT.format)(ptr::addr_of!(inf) as *const u8, &mut buf) };
        assert_eq!(buf, "inf");
        buf.clear();
        unsafe { (FLOAT.format)(ptr::addr_of!(neg_inf) as *const u8, &mut buf) };
        assert_eq!(buf, "-inf");
        buf.clear();
        unsafe { (FLOAT.format)(ptr::addr_of!(nan) as *const u8, &mut buf) };
        assert_eq!(buf, "NaN");
    }

    #[test]
    fn scalar_hash_is_stable() {
        use std::ptr;
        let a: IntPayload = 1234;
        let mut h1 = StructHasher::new();
        unsafe { (INT.hash.unwrap())(ptr::addr_of!(a) as *const u8, &mut h1) };
        let mut h2 = StructHasher::new();
        unsafe { (INT.hash.unwrap())(ptr::addr_of!(a) as *const u8, &mut h2) };
        assert_eq!(h1.finish(), h2.finish());

        // Different value → (almost certainly) different hash.
        let b: IntPayload = 1235;
        let mut h3 = StructHasher::new();
        unsafe { (INT.hash.unwrap())(ptr::addr_of!(b) as *const u8, &mut h3) };
        assert_ne!(h1.finish(), h3.finish());

        // Float hashing is stable, and +0.0 / -0.0 collide (they compare equal).
        let fp: FloatPayload = 2.5;
        let mut fh1 = StructHasher::new();
        unsafe { (FLOAT.hash.unwrap())(ptr::addr_of!(fp) as *const u8, &mut fh1) };
        let mut fh2 = StructHasher::new();
        unsafe { (FLOAT.hash.unwrap())(ptr::addr_of!(fp) as *const u8, &mut fh2) };
        assert_eq!(fh1.finish(), fh2.finish());
        let pos_zero: FloatPayload = 0.0;
        let neg_zero: FloatPayload = -0.0;
        let mut hp = StructHasher::new();
        let mut hn = StructHasher::new();
        unsafe { (FLOAT.hash.unwrap())(ptr::addr_of!(pos_zero) as *const u8, &mut hp) };
        unsafe { (FLOAT.hash.unwrap())(ptr::addr_of!(neg_zero) as *const u8, &mut hn) };
        assert_eq!(hp.finish(), hn.finish());
    }

    /// ADR-045 decision 2. The container order is total, so it has to answer
    /// for NaN — and it has to agree with `equals` everywhere else, which is
    /// why `f64::total_cmp` (which splits the two zeros) was not used.
    #[test]
    fn float_compare_is_numeric_with_nan_last() {
        use std::ptr;
        let cmp = FLOAT.compare.expect("Float is orderable");
        let at = |v: &FloatPayload| ptr::addr_of!(*v) as *const u8;

        let minus_two: FloatPayload = -2.0;
        let minus_one: FloatPayload = -1.0;
        let one: FloatPayload = 1.0;
        // Numeric, not the signed bit pattern: -2.0 has the *larger* magnitude
        // and so the larger unsigned payload.
        assert_eq!(
            unsafe { cmp(at(&minus_two), at(&minus_one)) },
            Ordering::Less
        );
        assert_eq!(unsafe { cmp(at(&one), at(&minus_one)) }, Ordering::Greater);

        // The two zeros are one value, as they are for `equals`.
        let pos_zero: FloatPayload = 0.0;
        let neg_zero: FloatPayload = -0.0;
        assert_eq!(
            unsafe { cmp(at(&pos_zero), at(&neg_zero)) },
            Ordering::Equal
        );

        // NaN sorts after every number, including infinity, and ties with NaN.
        let nan: FloatPayload = f64::NAN;
        let inf: FloatPayload = f64::INFINITY;
        assert_eq!(unsafe { cmp(at(&nan), at(&inf)) }, Ordering::Greater);
        assert_eq!(unsafe { cmp(at(&inf), at(&nan)) }, Ordering::Less);
        assert_eq!(unsafe { cmp(at(&nan), at(&nan)) }, Ordering::Equal);
    }

    /// The `compare` callback reads the payload it was declared for — the whole
    /// of P0-12's scalar half. A `Char` payload is four bytes; the old ordering
    /// read eight.
    #[test]
    fn scalar_compare_reads_its_own_payload_width() {
        use std::ptr;
        let int_cmp = INT.compare.expect("Int is orderable");
        let a: IntPayload = -5;
        let b: IntPayload = 3;
        assert_eq!(
            unsafe { int_cmp(ptr::addr_of!(a) as *const u8, ptr::addr_of!(b) as *const u8,) },
            Ordering::Less
        );

        let char_cmp = CHAR.compare.expect("Char is orderable");
        let lower_a: CharPayload = 'a' as u32;
        let beta: CharPayload = 'β' as u32;
        assert_eq!(
            unsafe {
                char_cmp(
                    ptr::addr_of!(lower_a) as *const u8,
                    ptr::addr_of!(beta) as *const u8,
                )
            },
            Ordering::Less,
            "'a' (U+0061) precedes 'β' (U+03B2) by scalar value"
        );

        let byte_cmp = BYTE.compare.expect("Byte is orderable");
        let low: BytePayload = 1;
        let high: BytePayload = 200;
        assert_eq!(
            unsafe { byte_cmp(ptr::addr_of!(low), ptr::addr_of!(high)) },
            Ordering::Less,
            "Byte is unsigned: 200 is not negative"
        );
    }

    /// ADR-045 decision 1: `Bool` and `Unit` have no ordering, and the absence
    /// is what `is_orderable` reports.
    #[test]
    fn bool_and_unit_declare_no_ordering() {
        assert!(!BOOL.is_orderable());
        assert!(!UNIT.is_orderable());
        assert!(INT.is_orderable());
        assert!(CHAR.is_orderable());
        assert!(FLOAT.is_orderable());
        assert!(BYTE.is_orderable());
    }

    #[test]
    fn char_validation_matches_std() {
        assert!(is_valid_char('A' as u32));
        assert!(is_valid_char(0x10FFFF));
        assert!(!is_valid_char(0x110000));
        assert!(!is_valid_char(0xD800)); // surrogate
    }
}
