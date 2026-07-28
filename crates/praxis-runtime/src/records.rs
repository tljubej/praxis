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

use crate::descriptor::{BuiltinTypeId, DynamicHasher, Tracer, TypeDescriptor};
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

unsafe fn record_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at initialized RecordPayloads
    // with compatible schemas.
    let pa = unsafe { &*(a as *const RecordPayload) };
    let pb = unsafe { &*(b as *const RecordPayload) };
    // Structural equality is shape + field-wise equality (§5.5). The schemas are
    // interned by shape, so distinct shapes are distinct pointers; if the
    // schemas disagree the records are different shapes and never equal.
    if pa.schema != pb.schema {
        return false;
    }
    if pa.items.len() != pb.items.len() {
        return false;
    }
    let schema = unsafe { &*pa.schema };
    // Field-wise equality through each field's descriptor (§11.4), short-circuiting
    // on the first non-equal field. If a field type is not equatable, the record is
    // not equatable (§5.5).
    for (i, (x, y)) in pa.items.iter().zip(pb.items.iter()).enumerate() {
        let Some(eq) = unsafe { &*schema.fields[i].descriptor }.equals else {
            return false;
        };
        let xe = x.payload::<u8>() as *const u8;
        let ye = y.payload::<u8>() as *const u8;
        if !eq(xe, ye) {
            return false;
        }
    }
    true
}

unsafe fn record_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at an initialized RecordPayload.
    let p = unsafe { &*(payload as *const RecordPayload) };
    let schema = unsafe { &*p.schema };
    // Arity first to distinguish records of different field counts.
    hasher.write_bytes(&(p.items.len() as u64).to_le_bytes());
    for (i, item) in p.items.iter().enumerate() {
        // If the field type is not hashable, the record is not hashable (§5.5).
        let Some(hash_field) = unsafe { &*schema.fields[i].descriptor }.hash else {
            return;
        };
        let elem_payload = item.payload::<u8>() as *const u8;
        hash_field(elem_payload, hasher);
    }
}

/// Descriptor for the structural `Record` type (§4.5/§7.8). Structural equality
/// and hashing (§5.5) recurse field-wise through the per-shape schema's field
/// descriptors. A record is equatable/hashable iff every field is; functions
/// never are, so a record containing a function field is neither. This lets
/// records serve as map/set keys (M8 containers).
pub static RECORD: TypeDescriptor = TypeDescriptor::builtin::<RecordPayload>(
    BuiltinTypeId::Record,
    "Record",
    record_trace,
    record_drop,
    record_format,
    Some(record_equals),
    Some(record_hash),
    // Ordering: see the ordering ADR; no built-in declares `compare` yet.
    None,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_descriptor_reports_capabilities() {
        assert!(RECORD.is_equatable());
        assert!(RECORD.is_hashable());
        assert_eq!(RECORD.name, "Record");
        assert_eq!(RECORD.as_builtin(), Some(BuiltinTypeId::Record));
    }

    #[test]
    fn grid_descriptor_reports_capabilities() {
        // M8-WS5: Grid is now equatable and hashable (grid-as-map-key enabled),
        // closing the M6 "grid-as-key deferred" note.
        assert!(crate::collections::GRID.is_equatable());
        assert!(crate::collections::GRID.is_hashable());
        assert_eq!(crate::collections::GRID.name, "Grid");
        assert_eq!(
            crate::collections::GRID.as_builtin(),
            Some(BuiltinTypeId::Grid)
        );
    }

    #[test]
    fn record_equals_identical_int_fields() {
        // Build two records with the same schema and equal Int fields; their
        // structural equals must be true, and unequal fields must be false.
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        let descriptors: &'static [*const TypeDescriptor] =
            Box::leak(vec![&crate::scalars::INT as *const TypeDescriptor; 2].into_boxed_slice());
        let schema = Box::leak(Box::new(RecordSchema {
            fields: Box::leak(
                vec![
                    RecordField {
                        name: "x",
                        descriptor: descriptors[0],
                    },
                    RecordField {
                        name: "y",
                        descriptor: descriptors[1],
                    },
                ]
                .into_boxed_slice(),
            ),
        }));
        // Allocate two records and fill with Int 1, 2.
        let a = unsafe { crate::abi::praxis_alloc_record(&mut ctx, schema) };
        let b = unsafe { crate::abi::praxis_alloc_record(&mut ctx, schema) };
        let one = unsafe { crate::abi::praxis_alloc_int(&mut ctx, 1) };
        let two = unsafe { crate::abi::praxis_alloc_int(&mut ctx, 2) };
        unsafe {
            crate::abi::praxis_record_set_field(&mut ctx, a, 0, one);
            crate::abi::praxis_record_set_field(&mut ctx, a, 1, two);
            crate::abi::praxis_record_set_field(&mut ctx, b, 0, one);
            crate::abi::praxis_record_set_field(&mut ctx, b, 1, two);
        }
        assert!(unsafe {
            record_equals(
                a.payload::<u8>() as *const u8,
                b.payload::<u8>() as *const u8,
            )
        });

        // Now make b's second field differ (3) → not equal.
        let three = unsafe { crate::abi::praxis_alloc_int(&mut ctx, 3) };
        unsafe { crate::abi::praxis_record_set_field(&mut ctx, b, 1, three) };
        assert!(!unsafe {
            record_equals(
                a.payload::<u8>() as *const u8,
                b.payload::<u8>() as *const u8,
            )
        });
    }
}
