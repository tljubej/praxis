//! The anonymous structural `Record` descriptor (§7.8, provisional in M6).
//!
//! The input parser's named-capture templates produce anonymous records, e.g.
//! `lines(`{x:int},{y:int}`)` → `Vec[{x:Int,y:Int}]`. Nominal records and enums
//! formally land in M7; M6 ships a **provisional structural record** that holds
//! exactly what parser results need: a fixed set of named fields, each a `GcRef`.
//!
//! Each distinct record *shape* (field names + element descriptors) gets a
//! [`RecordSchema`]. The schema is leaked to `&'static` (one per parser plan)
//! because a record's descriptor callbacks need a type-stable home for the
//! field descriptors; this matches how the JIT leaks function-name strings.
//!
//! The descriptor dispatches element-wise through the schema (§11.4) — there are
//! no scattered type switches in formatting/tracing. A single `RECORD`-shaped
//! descriptor serves every record because the per-shape knowledge lives in the
//! schema referenced from the payload.

use std::fmt;

use crate::descriptor::{Tracer, TypeDescriptor, TypeId};
use crate::GcRef;

/// One field of a record shape: its source name plus the descriptor for the
/// values stored at that field. The descriptor pointer is `const` data shared
/// across all records of this shape.
#[repr(C)]
pub struct RecordField {
    pub name: &'static str,
    pub descriptor: *const TypeDescriptor,
}

/// The static shape of an anonymous record: an ordered list of named fields,
/// each with its value descriptor. Leaked to `&'static` per parser plan.
#[repr(C)]
pub struct RecordSchema {
    pub fields: &'static [RecordField],
}

impl RecordSchema {
    /// The number of fields in this record shape.
    pub fn arity(&self) -> usize {
        self.fields.len()
    }
}

/// The `Record` payload: a pointer to the static schema plus the field values
/// (one `GcRef` per field, in schema order).
#[repr(C)]
pub struct RecordPayload {
    /// The static field shape. `items.len()` must equal `schema.arity()`.
    pub schema: *const RecordSchema,
    /// Field values in schema order.
    pub items: Vec<GcRef>,
}

unsafe fn record_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized RecordPayload.
    let p = unsafe { &*(payload as *const RecordPayload) };
    for item in p.items.iter() {
        tracer.trace(*item);
    }
}

unsafe fn record_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized RecordPayload.
    // `drop_in_place` frees the items Vec; the schema is static and not owned.
    unsafe { std::ptr::drop_in_place(payload as *mut RecordPayload) };
}

unsafe fn record_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an initialized RecordPayload.
    let p = unsafe { &*(payload as *const RecordPayload) };
    let schema = unsafe { &*p.schema };
    let _ = out.write_str("{ ");
    for (i, item) in p.items.iter().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        let field = &schema.fields[i];
        let _ = out.write_str(field.name);
        let _ = out.write_str(": ");
        let elem_desc = unsafe { &*field.descriptor };
        (elem_desc.format)(item.payload::<u8>() as *const u8, out);
    }
    let _ = out.write_str(" }");
}

/// Descriptor for the provisional structural `Record` type (M6, §7.8). Marked
/// non-equatable / non-hashable for M6; M7 adds structural equality + hashing
/// as part of the full record story (so records can be map/set keys).
pub const RECORD: &TypeDescriptor = &TypeDescriptor {
    id: TypeId(8),
    name: "Record",
    size: std::mem::size_of::<RecordPayload>(),
    align: std::mem::align_of::<RecordPayload>(),
    trace: record_trace,
    drop_value: record_drop,
    format: record_format,
    equals: None,
    hash: None,
};

// Suppress the unused-import warning for DynamicHasher: it is part of the
// descriptor vocabulary and will be referenced when equality/hash land in M7.
#[allow(unused_imports)]
use crate::DynamicHasher as _DynamicHasher;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_descriptor_reports_capabilities() {
        assert!(!RECORD.is_equatable());
        assert!(!RECORD.is_hashable());
        assert_eq!(RECORD.name, "Record");
        assert_eq!(RECORD.id, TypeId(8));
    }

    #[test]
    fn grid_descriptor_reports_capabilities() {
        assert!(!crate::collections::GRID.is_equatable());
        assert!(!crate::collections::GRID.is_hashable());
        assert_eq!(crate::collections::GRID.name, "Grid");
        assert_eq!(crate::collections::GRID.id, TypeId(7));
    }
}
