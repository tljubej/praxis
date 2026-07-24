//! The `Enum` value descriptor (§4.6, M7).
//!
//! An enum value carries a discriminant (`tag`) selecting its variant plus the
//! variant's payload values (one `GcRef` per payload type, in declaration
//! order). A single `ENUM` descriptor serves every enum value because the
//! per-variant shape is recovered from the tag + the compile-time `EnumDef`
//! (the codegen embeds the enum-def context; the runtime payload is uniform).

use std::fmt;

use crate::descriptor::{Tracer, TypeDescriptor, TypeId};
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

/// Descriptor for the `Enum` value type (M7, §4.6). Non-equatable / non-hashable
/// in M7; WS6 adds structural equality/hashing.
pub const ENUM: &TypeDescriptor = &TypeDescriptor {
    id: TypeId(9),
    name: "Enum",
    size: std::mem::size_of::<EnumPayload>(),
    align: std::mem::align_of::<EnumPayload>(),
    trace: enum_trace,
    drop_value: enum_drop,
    format: enum_format,
    equals: None,
    hash: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_descriptor_reports_capabilities() {
        assert!(!ENUM.is_equatable());
        assert!(!ENUM.is_hashable());
        assert_eq!(ENUM.name, "Enum");
        assert_eq!(ENUM.id, TypeId(9));
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
