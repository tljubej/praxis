//! The `VarCell` value descriptor (M7-WS7b, §4.10).
//!
//! A `VarCell` is a single-slot GC heap cell holding one `GcRef`. It is the
//! shared mutable storage for a `var` binding captured by a closure (§4.10:
//! "mutable captures use GC-managed environment cells"). The binding site and
//! every closure that captures the `var` refer to the *same* cell, so a write
//! in one is visible in the other.
//!
//! A `var` is *boxed* into a `VarCell` at its binding site iff it is captured
//! by some closure in the module (escape analysis, run during HIR lowering).
//! Uncaptured `var`s stay as ordinary mutable locals — no cell overhead. The
//! cell is transparent to the source program: reads (`Path`) deref it via
//! `praxis_var_cell_get`, writes (`Assign`) store via `praxis_var_cell_set`.
//!
//! Per §5.5, `VarCell`s are never equatable or hashable — they are an internal
//! implementation detail, not a first-class value the program can name.
//!
//! Its `TypeId` is derived from `BuiltinTypeId::VarCell`.

use std::fmt;

use crate::descriptor::{BuiltinTypeId, Tracer, TypeDescriptor};
use crate::GcRef;

/// The runtime payload of a `VarCell`: a single `GcRef` slot. `#[repr(C)]` so
/// the ABI wrappers can read/write it at a known offset.
#[repr(C)]
pub struct VarCellPayload {
    /// The current value held by the captured `var`.
    pub value: GcRef,
}

unsafe fn var_cell_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized VarCellPayload.
    let p = unsafe { &*(payload as *const VarCellPayload) };
    tracer.trace(p.value);
}

unsafe fn var_cell_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized VarCellPayload.
    // No heap allocation beyond the GcRef field (a plain pointer-sized Copy), so
    // drop_in_place is a no-op; we still call it for uniformity with other payloads.
    unsafe { std::ptr::drop_in_place(payload as *mut VarCellPayload) };
}

unsafe fn var_cell_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an initialized VarCellPayload.
    let _ = payload;
    // VarCells are internal; render opaquely for debugging.
    let _ = out.write_str("<var-cell>");
}

/// Descriptor for the `VarCell` internal value type (M7-WS7b, §4.10). Never
/// equatable or hashable (it is not a first-class value).
pub static VAR_CELL: TypeDescriptor = TypeDescriptor::builtin::<VarCellPayload>(
    BuiltinTypeId::VarCell,
    "VarCell",
    var_cell_trace,
    var_cell_drop,
    var_cell_format,
    None,
    None,
    // Not orderable: only Int/Byte/Char/Float/Text are (ADR-045).
    None,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn var_cell_descriptor_reports_capabilities() {
        // VarCells are never equatable/hashable (internal type).
        assert!(!VAR_CELL.is_equatable());
        assert!(!VAR_CELL.is_hashable());
        assert_eq!(VAR_CELL.name, "VarCell");
        assert_eq!(VAR_CELL.as_builtin(), Some(BuiltinTypeId::VarCell));
    }
}
