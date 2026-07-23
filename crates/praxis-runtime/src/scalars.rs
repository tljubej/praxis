//! Built-in scalar type descriptors (§4.3, §11.4).
//!
//! Each scalar type has a `const` [`TypeDescriptor`] exposed as a static
//! reference (`UNIT`, `BOOL`, `INT`, `BYTE`, `CHAR`). Every payload-aware
//! operation routes through these descriptors; there are no type switches
//! elsewhere (§11.4).
//!
//! Scalar payloads contain no `GcRef`s, so every scalar `trace` is a no-op —
//! scalars are the leaf case that proves the descriptor machinery without
//! exercising nested references. Composite tracing is covered by `Vec[T]`
//! (ADR-013).

use std::fmt;

use crate::descriptor::{hash_value, DynamicHasher, Tracer, TypeDescriptor, TypeId};

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
pub const UNIT: &TypeDescriptor = &TypeDescriptor {
    id: TypeId(0),
    name: "Unit",
    size: std::mem::size_of::<UnitPayload>(),
    align: std::mem::align_of::<UnitPayload>(),
    trace: unit_trace,
    drop_value: unit_drop,
    format: unit_format,
    equals: Some(unit_equals),
    hash: Some(unit_hash),
};

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
pub const BOOL: &TypeDescriptor = &TypeDescriptor {
    id: TypeId(1),
    name: "Bool",
    size: std::mem::size_of::<BoolPayload>(),
    align: std::mem::align_of::<BoolPayload>(),
    trace: bool_trace,
    drop_value: bool_drop,
    format: bool_format,
    equals: Some(bool_equals),
    hash: Some(bool_hash),
};

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

/// Descriptor for the `Int` scalar (§4.3).
pub const INT: &TypeDescriptor = &TypeDescriptor {
    id: TypeId(2),
    name: "Int",
    size: std::mem::size_of::<IntPayload>(),
    align: std::mem::align_of::<IntPayload>(),
    trace: int_trace,
    drop_value: int_drop,
    format: int_format,
    equals: Some(int_equals),
    hash: Some(int_hash),
};

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

/// Descriptor for the `Byte` scalar (§4.3).
pub const BYTE: &TypeDescriptor = &TypeDescriptor {
    id: TypeId(3),
    name: "Byte",
    size: std::mem::size_of::<BytePayload>(),
    align: std::mem::align_of::<BytePayload>(),
    trace: byte_trace,
    drop_value: byte_drop,
    format: byte_format,
    equals: Some(byte_equals),
    hash: Some(byte_hash),
};

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

/// Descriptor for the `Char` scalar (§4.3).
pub const CHAR: &TypeDescriptor = &TypeDescriptor {
    id: TypeId(4),
    name: "Char",
    size: std::mem::size_of::<CharPayload>(),
    align: std::mem::align_of::<CharPayload>(),
    trace: char_trace,
    drop_value: char_drop,
    format: char_format,
    equals: Some(char_equals),
    hash: Some(char_hash),
};

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
    }

    #[test]
    fn char_validation_matches_std() {
        assert!(is_valid_char('A' as u32));
        assert!(is_valid_char(0x10FFFF));
        assert!(!is_valid_char(0x110000));
        assert!(!is_valid_char(0xD800)); // surrogate
    }
}
