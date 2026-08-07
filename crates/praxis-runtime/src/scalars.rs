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
//!
//! Each descriptor is followed by its [`Payload`] handle — `INT_PAYLOAD` beside
//! `INT` — which is what the allocators take (REP-02). The descriptor derives
//! its width from the payload type and then erases it, so an allocator handed a
//! bare `&TypeDescriptor` could only compare widths at runtime; the handle
//! carries the type, and the pairing is checked when the `static` is evaluated.

use std::cmp::Ordering;
use std::fmt::{self, Write as _};

use crate::descriptor::{
    hash_value, BuiltinTypeId, DynamicHasher, FormatSink, Payload, Tracer, TypeDescriptor,
};
use crate::heap::InlineClaimSite;

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
unsafe fn unit_format(_: *const u8, out: &mut FormatSink<'_>) {
    let _ = out.write_str("Unit");
}
unsafe fn unit_equals(_: *const u8, _: *const u8) -> bool {
    true
}
unsafe fn unit_hash(_: *const u8, hasher: &mut dyn DynamicHasher) {
    // Unit is a singleton; all instances hash equally.
    hash_value(hasher, &());
}
unsafe fn unit_compare(_: *const u8, _: *const u8) -> Ordering {
    // A singleton has one value, so `Equal` is the only answer that agrees with
    // `unit_equals` — and agreeing with equality is what makes it a total order
    // rather than a shrug (ADR-138).
    Ordering::Equal
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
    // A `Unit` can be a `Map` key, so a container has to be able to order one
    // (ADR-138). `<` on a `Unit` is still Y006 — that is `supports_ord`'s
    // question, and it is deliberately a different one.
    Some(unit_compare),
);

/// `Unit`'s payload handle (REP-02). Its one value is an immortal, minted at
/// startup — nothing gc-allocates a `Unit`.
pub static UNIT_PAYLOAD: Payload<UnitPayload> = Payload::new(&UNIT);

// ---- Bool ------------------------------------------------------------------

unsafe fn bool_trace(_: *mut u8, _: &mut dyn Tracer) {}
unsafe fn bool_drop(_: *mut u8) {}
unsafe fn bool_format(payload: *const u8, out: &mut FormatSink<'_>) {
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
unsafe fn bool_compare(a: *const u8, b: *const u8) -> Ordering {
    // `false` before `true`, which is both the conventional order and the one
    // the rendered forms already had — so no `Set[Bool]` prints differently for
    // this (ADR-138).
    // SAFETY: caller guarantees both pointers point at `BoolPayload`s.
    unsafe { (*(a as *const BoolPayload)).cmp(&*(b as *const BoolPayload)) }
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
    // A `Bool` can be a `Map` key, so a container has to be able to order one
    // (ADR-138). `true < false` is still Y006 — see `unit_compare`.
    Some(bool_compare),
);

/// `Bool`'s payload handle (REP-02). Both values are immortals too (RT-03), so
/// this mints the pair at startup rather than one per comparison.
pub static BOOL_PAYLOAD: Payload<BoolPayload> = Payload::new(&BOOL);

// ---- Int -------------------------------------------------------------------

unsafe fn int_trace(_: *mut u8, _: &mut dyn Tracer) {}
unsafe fn int_drop(_: *mut u8) {}

/// Render an `Int`: the decimal digits, with a leading `-` when negative.
///
/// Factored out of [`int_format`] so `Int.to_text()` calls *this* rather than
/// writing a second `write!` of its own (ADR-143). `out(n)` and `n.to_text()`
/// disagreeing would be a defect in itself, and one writer with two callers is
/// what makes it unrepresentable instead of merely tested — the shape
/// [`write_float`] already has.
pub(crate) fn write_int(out: &mut dyn fmt::Write, v: IntPayload) {
    let _ = write!(out, "{v}");
}

unsafe fn int_format(payload: *const u8, out: &mut FormatSink<'_>) {
    // SAFETY: caller guarantees `payload` points at an `IntPayload`.
    let v = unsafe { *(payload as *const IntPayload) };
    write_int(out, v);
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

/// `Int`'s payload handle (REP-02). `IntPayload` is `i64` while a Rust integer
/// literal defaults to `i32` — the mismatch that used to abort the process, and
/// that this handle makes the compiler resolve.
pub static INT_PAYLOAD: Payload<IntPayload> = Payload::new(&INT);

/// The inline bitmap claim for an `Int` that [`crate::small_int`]'s table does
/// not hold (ADR-119).
///
/// **Minted here, beside the descriptor**, for [`crate::small_int`]'s reason one
/// level up: the site's whole content is a function of `INT`, and a site minted
/// anywhere else would be a second place that has to agree about which
/// descriptor generated code is about to write into a header. `unwrap` in a
/// `const` initializer means "fails the build": `INT` carries no `owned_bytes`
/// callback and its 24-byte block is on the ladder, and if either stops being
/// true this stops compiling rather than starting to under-charge the pacer.
pub const INT_CLAIM_SITE: InlineClaimSite = match InlineClaimSite::of(&INT) {
    Some(site) => site,
    None => panic!("Int has no owned_bytes charge and its block is on the ladder"),
};

// ---- Byte ------------------------------------------------------------------

unsafe fn byte_trace(_: *mut u8, _: &mut dyn Tracer) {}
unsafe fn byte_drop(_: *mut u8) {}
unsafe fn byte_format(payload: *const u8, out: &mut FormatSink<'_>) {
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

/// `Byte`'s payload handle (REP-02). Its payload is the same Rust type as
/// `Bool`'s, which is why a handle names its descriptor explicitly instead of
/// the pairing being derived from the payload type.
pub static BYTE_PAYLOAD: Payload<BytePayload> = Payload::new(&BYTE);

// ---- Char ------------------------------------------------------------------

unsafe fn char_trace(_: *mut u8, _: &mut dyn Tracer) {}
unsafe fn char_drop(_: *mut u8) {}

/// Render a `Char`: the character itself, with no quotes and no escaping.
///
/// Factored out of [`char_format`] for [`write_int`]'s reason (ADR-143): the
/// `U+FFFD` fallback below is a decision about what an impossible payload looks
/// like, and `Char.to_text()` answering something else would make one of the two
/// wrong without saying which.
pub(crate) fn write_char(out: &mut dyn fmt::Write, v: CharPayload) {
    match char::from_u32(v) {
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

unsafe fn char_format(payload: *const u8, out: &mut FormatSink<'_>) {
    // SAFETY: caller guarantees `payload` points at a validated `CharPayload`.
    let raw = unsafe { *(payload as *const CharPayload) };
    write_char(out, raw);
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

/// `Char`'s payload handle (REP-02). `CharPayload` is `u32`, not `char`: the two
/// share a layout, so the runtime width check could never tell them apart — and
/// one call site was passing a `char`. The type argument is what says which.
pub static CHAR_PAYLOAD: Payload<CharPayload> = Payload::new(&CHAR);

// ---- Float ------------------------------------------------------------------

unsafe fn float_trace(_: *mut u8, _: &mut dyn Tracer) {}
unsafe fn float_drop(_: *mut u8) {}
/// Render a `Float` the way §4.12 asks: in the shortest form that reads back as
/// **the same Praxis `Float`** (ADR-083, REP-44).
///
/// Rust's `{}` is shortest-round-trippable for Rust, where a bare `1` re-reads
/// as an `f64`. Praxis is not Rust here: §4.12's typing rule is that `42` is
/// strictly an `Int` literal and that `Float` and `Int` never mix, so a `Float`
/// rendered `1` does not read back as a `Float` at all — and, printed inside a
/// collection, a `Vec[Float]` of `[3.0, 5.0]` was indistinguishable from a
/// `Vec[Int]`. So a finite value with no `.` and no exponent in its digits gets
/// a `.0`, and everything else — including `inf`/`-inf`/`NaN`, which §4.12 names
/// as those literals — is Rust's rendering unchanged.
pub(crate) fn write_float(out: &mut dyn fmt::Write, v: FloatPayload) {
    let rendered = format!("{v}");
    let is_a_float_literal = rendered
        .bytes()
        .any(|b| b == b'.' || b == b'e' || b == b'E');
    if v.is_finite() && !is_a_float_literal {
        let _ = write!(out, "{rendered}.0");
    } else {
        let _ = out.write_str(&rendered);
    }
}

unsafe fn float_format(payload: *const u8, out: &mut FormatSink<'_>) {
    // SAFETY: caller guarantees `payload` points at a `FloatPayload`.
    let v = unsafe { *(payload as *const FloatPayload) };
    write_float(out, v);
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

/// `Float`'s payload handle (REP-02).
pub static FLOAT_PAYLOAD: Payload<FloatPayload> = Payload::new(&FLOAT);

/// The inline bitmap claim for a `Float` (ADR-119). See [`INT_CLAIM_SITE`].
///
/// A `Float` has no intern table, so unlike `Int` this is the *whole* inline
/// form of the box: there is no probe in front of it and the wrapper behind it
/// is reached only when the claim itself bails.
pub const FLOAT_CLAIM_SITE: InlineClaimSite = match InlineClaimSite::of(&FLOAT) {
    Some(site) => site,
    None => panic!("Float has no owned_bytes charge and its block is on the ladder"),
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
        unsafe { (UNIT.format)(ptr::null(), &mut crate::FormatSink::display(&mut buf)) };
        assert_eq!(buf, "Unit");

        // Bool
        let t: BoolPayload = 1;
        let f: BoolPayload = 0;
        buf.clear();
        unsafe { (BOOL.format)(ptr::addr_of!(t), &mut crate::FormatSink::display(&mut buf)) };
        assert_eq!(buf, "true");
        buf.clear();
        unsafe { (BOOL.format)(ptr::addr_of!(f), &mut crate::FormatSink::display(&mut buf)) };
        assert_eq!(buf, "false");
        assert!(unsafe { (BOOL.equals.unwrap())(ptr::addr_of!(t), ptr::addr_of!(t)) });
        assert!(!unsafe { (BOOL.equals.unwrap())(ptr::addr_of!(t), ptr::addr_of!(f)) });

        // Int
        let a: IntPayload = 42;
        let b: IntPayload = -7;
        buf.clear();
        unsafe {
            (INT.format)(
                ptr::addr_of!(a) as *const u8,
                &mut crate::FormatSink::display(&mut buf),
            )
        };
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
        unsafe { (BYTE.format)(ptr::addr_of!(by), &mut crate::FormatSink::display(&mut buf)) };
        assert_eq!(buf, "255");

        // Char
        let ch: CharPayload = 'A' as u32;
        buf.clear();
        unsafe {
            (CHAR.format)(
                ptr::addr_of!(ch) as *const u8,
                &mut crate::FormatSink::display(&mut buf),
            )
        };
        assert_eq!(buf, "A");

        // Float — finite value formats via Rust's shortest round-trip form.
        let f: FloatPayload = 2.5;
        buf.clear();
        unsafe {
            (FLOAT.format)(
                ptr::addr_of!(f) as *const u8,
                &mut crate::FormatSink::display(&mut buf),
            )
        };
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
        unsafe {
            (FLOAT.format)(
                ptr::addr_of!(inf) as *const u8,
                &mut crate::FormatSink::display(&mut buf),
            )
        };
        assert_eq!(buf, "inf");
        buf.clear();
        unsafe {
            (FLOAT.format)(
                ptr::addr_of!(neg_inf) as *const u8,
                &mut crate::FormatSink::display(&mut buf),
            )
        };
        assert_eq!(buf, "-inf");
        buf.clear();
        unsafe {
            (FLOAT.format)(
                ptr::addr_of!(nan) as *const u8,
                &mut crate::FormatSink::display(&mut buf),
            )
        };
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

    /// `Bool` and `Unit` have a **container** order and no **source** order
    /// (ADR-138). Both can be a `Map` key, so `out(m)` and `for k in m` have to
    /// put them in some sequence and it has to be the same sequence twice;
    /// `true < false` is still refused at check time, which is
    /// `praxis_hir::capability::supports_ord`'s question and not this one.
    #[test]
    fn bool_and_unit_have_a_container_order_and_no_source_order() {
        use std::ptr;
        let (f, t): (BoolPayload, BoolPayload) = (0, 1);
        assert_eq!(
            unsafe { bool_compare(ptr::addr_of!(f).cast(), ptr::addr_of!(t).cast()) },
            Ordering::Less,
            "false sorts before true"
        );
        let unit: UnitPayload = ();
        assert_eq!(
            unsafe { unit_compare(ptr::addr_of!(unit).cast(), ptr::addr_of!(unit).cast()) },
            Ordering::Equal,
            "a singleton equals itself and nothing else exists to order it against"
        );
        assert!(BOOL.is_orderable());
        assert!(UNIT.is_orderable());
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

    /// REP-02: every scalar's payload handle names the type its descriptor
    /// describes, and the allocator writes exactly that.
    ///
    /// The *pairing* is already a compile-time property — each `Payload::new`
    /// above runs during const evaluation of a `static`, so a handle whose type
    /// argument disagreed with its descriptor would not build, and the value's
    /// type is checked at every `gc_alloc` call site. What a test can still add
    /// is that the list is **complete** (a scalar with no handle is one whose
    /// callers must fall back to something unchecked) and that the round trip
    /// through a real allocation reads back the value that went in — the
    /// "declared payload type matches what its allocator writes" half.
    #[test]
    fn every_scalar_has_a_payload_handle_and_it_round_trips() {
        use crate::descriptor::{BuiltinTypeId, Payload};

        // One entry per scalar `BuiltinTypeId`, so a new scalar without a handle
        // fails the exhaustive match below rather than being forgotten.
        fn declared(id: BuiltinTypeId) -> Option<(&'static TypeDescriptor, usize, usize)> {
            /// The layout `Payload<T>` promises, read back off the handle.
            fn of<T: Copy>(p: Payload<T>) -> (&'static TypeDescriptor, usize, usize) {
                (
                    p.descriptor(),
                    std::mem::size_of::<T>(),
                    std::mem::align_of::<T>(),
                )
            }
            Some(match id {
                BuiltinTypeId::Unit => of(UNIT_PAYLOAD),
                BuiltinTypeId::Bool => of(BOOL_PAYLOAD),
                BuiltinTypeId::Int => of(INT_PAYLOAD),
                BuiltinTypeId::Byte => of(BYTE_PAYLOAD),
                BuiltinTypeId::Char => of(CHAR_PAYLOAD),
                BuiltinTypeId::Float => of(FLOAT_PAYLOAD),
                // Not a scalar: its payload owns Rust resources or is composite,
                // so it is allocated through `alloc_with` and has no `Copy`
                // handle. `Range` is the one non-scalar that does — see
                // `range.rs`.
                _ => return None,
            })
        }

        for (id, expected) in [
            (BuiltinTypeId::Unit, &UNIT),
            (BuiltinTypeId::Bool, &BOOL),
            (BuiltinTypeId::Int, &INT),
            (BuiltinTypeId::Byte, &BYTE),
            (BuiltinTypeId::Char, &CHAR),
            (BuiltinTypeId::Float, &FLOAT),
        ] {
            let (descriptor, size, align) =
                declared(id).unwrap_or_else(|| panic!("{id:?} has no payload handle"));
            // The handle carries the one `static`, so descriptor identity — which
            // is pointer identity (ADR-038) — survives being wrapped.
            assert!(
                std::ptr::eq(descriptor, expected),
                "{id:?}'s handle names another descriptor"
            );
            assert_eq!(size, descriptor.size(), "{id:?} payload width");
            assert_eq!(align, descriptor.align(), "{id:?} payload alignment");
        }

        // The round trip: what each allocator writes is readable as the declared
        // payload type. `praxis_alloc_int` used to take an `i64` and hand it to a
        // generic `gc_alloc` that checked the width at runtime; the width is now
        // the handle's, and this is the value arriving intact through it.
        let rt = crate::Runtime::new();
        assert_eq!(rt.alloc_int(-42).as_int(), -42);
        assert_eq!(rt.alloc_float(2.5).as_float(), 2.5);
        // SAFETY: each ref was just allocated with the matching descriptor, so
        // its payload is a value of that type.
        unsafe {
            assert_eq!(*rt.alloc_byte(255).payload::<BytePayload>(), 255);
            assert_eq!(
                *rt.alloc_char('A' as u32).payload::<CharPayload>(),
                'A' as u32
            );
        }
    }

    /// **REP-44, ADR-083.** A `Float` renders as a Praxis `Float` literal.
    ///
    /// The descriptor test above could not catch this: its one value is `2.5`,
    /// which already carries a `.`, so it passes whichever rule is in force.
    /// Every case here is a whole-numbered value, an exponent, or a non-finite
    /// one — the three places the two rules differ.
    #[test]
    fn a_whole_numbered_float_renders_as_a_float() {
        let rendered = |v: FloatPayload| {
            let mut buf = String::new();
            // SAFETY: `v` is a `FloatPayload` and `FLOAT` is its descriptor.
            unsafe {
                (FLOAT.format)(
                    std::ptr::addr_of!(v) as *const u8,
                    &mut crate::FormatSink::display(&mut buf),
                )
            };
            buf
        };
        // The defect: identical to an `Int`'s rendering, so `[3.0, 5.0]` and
        // `[3, 5]` printed the same and neither read back as the other's type.
        assert_eq!(rendered(1.0), "1.0");
        assert_eq!(rendered(0.0), "0.0");
        assert_eq!(rendered(-7.0), "-7.0");
        assert_eq!(rendered(1e10), "10000000000.0");
        // Already a literal: untouched, and no second `.0`.
        assert_eq!(rendered(2.5), "2.5");
        assert_eq!(rendered(0.1 + 0.2), "0.30000000000000004");
        // §4.12 names these three, and none of them takes a `.0`.
        assert_eq!(rendered(f64::INFINITY), "inf");
        assert_eq!(rendered(f64::NEG_INFINITY), "-inf");
        assert_eq!(rendered(f64::NAN), "NaN");
    }

    /// **REP-50, and ADR-083's rule stated as a round trip.** The rendered form
    /// of a `Float` is the text that reads back as *the same* `Float`, so the
    /// check is a re-read and not a string comparison.
    ///
    /// `-0.0` is the case that made this worth writing: the formatter has
    /// always been right about it, and the evaluator was not — a Float
    /// negation was lowered as `0.0 - x`, so the literal `-0.0` produced
    /// `+0.0` and the rendering of the *wrong value* round-tripped perfectly.
    /// `to_bits` is what tells the two zeros apart; `==` cannot, because
    /// IEEE-754 says they are equal.
    ///
    /// It is **not REP-50's gate** — that is `run_pass_float_negative_zero`,
    /// which asks the *evaluator*. It **is** REP-44's: before the rendering fix
    /// a whole-numbered `Float` printed like an `Int`, so `-0.0` rendered as
    /// `-0` and the last assertion below was red. Both halves are worth having,
    /// because a later edit to `FLOAT.format` would otherwise stop satisfying
    /// the round trip without any evaluator test noticing.
    #[test]
    fn a_rendered_float_reads_back_as_the_same_float() {
        let rendered = |v: FloatPayload| {
            let mut buf = String::new();
            // SAFETY: `v` is a `FloatPayload` and `FLOAT` is its descriptor.
            unsafe {
                (FLOAT.format)(
                    std::ptr::addr_of!(v) as *const u8,
                    &mut crate::FormatSink::display(&mut buf),
                )
            };
            buf
        };
        for v in [
            0.0_f64,
            -0.0,
            1.0,
            -7.0,
            2.5,
            1e10,
            0.1 + 0.2,
            f64::MAX,
            f64::MIN_POSITIVE,
        ] {
            let text = rendered(v);
            let reread: f64 = text
                .parse()
                .unwrap_or_else(|e| panic!("`{text}` does not read back as a Float: {e}"));
            assert_eq!(
                reread.to_bits(),
                v.to_bits(),
                "`{text}` read back as a different Float"
            );
        }
        // The signed zeros are distinct *values* — different bit patterns — and
        // so must render distinctly for the round trip above to mean anything.
        // They are NOT distinct to a container: ADR-045's `compare` treats them
        // as equal, and rejected `f64::total_cmp` precisely for splitting them,
        // so a `Map` keyed on them holds one entry. Rendering and ordering
        // disagree here on purpose; see §4.12.
        assert_eq!(rendered(-0.0), "-0.0");
        assert_ne!(rendered(-0.0), rendered(0.0));
    }
}
