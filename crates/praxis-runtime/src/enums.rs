//! The `Enum` value descriptor (§4.6, M7).
//!
//! An enum value carries a discriminant (`tag`) selecting its variant plus the
//! variant's payload values (one `GcRef` per payload type, in declaration
//! order). A single `ENUM` descriptor serves every enum value because the
//! per-variant shape is recovered from the tag + the compile-time `EnumDef`
//! (the codegen embeds the enum-def context; the runtime payload is uniform).

use std::fmt;

use crate::descriptor::{BuiltinTypeId, DynamicHasher, Tracer, TypeDescriptor};
use crate::GcRef;

/// The runtime payload of an enum value: the variant discriminant plus its
/// payload values (one `GcRef` per payload field, in declaration order).
#[repr(C)]
pub struct EnumPayload {
    /// Which variant this value is (index into the enum's variant list).
    pub tag: u32,
    /// The variant's payload values (empty for a payload-less variant).
    pub items: Vec<GcRef>,
}

unsafe fn enum_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized EnumPayload.
    let p = unsafe { &*(payload as *const EnumPayload) };
    for item in p.items.iter() {
        tracer.trace(*item);
    }
}

unsafe fn enum_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized EnumPayload.
    unsafe { std::ptr::drop_in_place(payload as *mut EnumPayload) };
}

unsafe fn enum_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an initialized EnumPayload.
    let p = unsafe { &*(payload as *const EnumPayload) };
    // Without the compile-time variant names, render as `<variant N: …>`. The
    // codegen could embed names for richer formatting in a follow-up.
    let _ = write!(out, "<variant {}: ", p.tag);
    for (i, item) in p.items.iter().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        // Format each item through its descriptor.
        let desc = item.descriptor();
        (desc.format)(item.payload::<u8>() as *const u8, out);
    }
    let _ = out.write_str(">");
}

unsafe fn enum_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at initialized EnumPayloads.
    let pa = unsafe { &*(a as *const EnumPayload) };
    let pb = unsafe { &*(b as *const EnumPayload) };
    // Two enum values are equal only if they carry the same variant (tag), then
    // if their payloads are element-wise equal (§5.5). The variant's payload
    // types are fixed by the tag, so each item's own descriptor is the right one.
    if pa.tag != pb.tag {
        return false;
    }
    if pa.items.len() != pb.items.len() {
        return false;
    }
    for (x, y) in pa.items.iter().zip(pb.items.iter()) {
        let eq_x = match x.descriptor().equals {
            Some(eq) => eq,
            // If a payload type is not equatable, the enum is not equatable (§5.5).
            None => return false,
        };
        let xe = x.payload::<u8>() as *const u8;
        let ye = y.payload::<u8>() as *const u8;
        if !eq_x(xe, ye) {
            return false;
        }
    }
    true
}

unsafe fn enum_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at an initialized EnumPayload.
    let p = unsafe { &*(payload as *const EnumPayload) };
    // The tag first — two values of different variants must hash distinctly even
    // before the payload is considered.
    hasher.write_bytes(&(p.tag as u64).to_le_bytes());
    for item in p.items.iter() {
        // If a payload type is not hashable, the enum is not hashable (§5.5).
        let Some(hash_item) = item.descriptor().hash else {
            return;
        };
        let elem_payload = item.payload::<u8>() as *const u8;
        hash_item(elem_payload, hasher);
    }
}

/// Descriptor for the `Enum` value type (M7, §4.6). Structural equality and
/// hashing (§5.5): two enum values are equal iff same variant tag and equal
/// payloads; hashing mixes the tag then each payload. An enum is
/// equatable/hashable iff every payload type is; functions never are. This lets
/// enums serve as map/set keys (M8 containers).
pub static ENUM: TypeDescriptor = TypeDescriptor::builtin::<EnumPayload>(
    BuiltinTypeId::Enum,
    "Enum",
    enum_trace,
    enum_drop,
    enum_format,
    Some(enum_equals),
    Some(enum_hash),
    // Ordering: see the ordering ADR; no built-in declares `compare` yet.
    None,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_descriptor_reports_capabilities() {
        assert!(ENUM.is_equatable());
        assert!(ENUM.is_hashable());
        assert_eq!(ENUM.name, "Enum");
        assert_eq!(ENUM.as_builtin(), Some(BuiltinTypeId::Enum));
    }
}

#[cfg(test)]
mod alloc_tests {
    use super::*;
    use crate::abi::praxis_alloc_enum;

    #[test]
    fn alloc_enum_tag_zero_round_trips() {
        // Allocate an enum with tag 0 via the ABI wrapper and verify the tag.
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        let eref = unsafe { praxis_alloc_enum(&mut ctx, 0, 0) };
        let payload = eref.payload::<u8>() as *const EnumPayload;
        let tag = unsafe { (*payload).tag };
        assert_eq!(tag, 0, "tag should be 0 after alloc_enum(0, 0)");
    }

    #[test]
    fn alloc_enum_tag_one_round_trips() {
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        let eref = unsafe { praxis_alloc_enum(&mut ctx, 1, 0) };
        let payload = eref.payload::<u8>() as *const EnumPayload;
        let tag = unsafe { (*payload).tag };
        assert_eq!(tag, 1, "tag should be 1 after alloc_enum(1, 0)");
    }
}
