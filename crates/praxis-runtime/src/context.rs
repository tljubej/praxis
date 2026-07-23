//! The [`RuntimeContext`] handed to every generated function (§10.3, Appendix B).
//!
//! Every generated function receives a hidden first parameter — a pointer to
//! `RuntimeContext` — followed only by `GcRef` arguments, and returns one
//! `GcRef`. The context is the single channel through which generated code
//! reaches the GC heap, the pending fault, the debug frame chain, the input
//! source, and so on.

use crate::gc::GcRef;

/// Opaque GC heap. Real layout lands in Milestone 3 (§12).
#[repr(C)]
pub struct Heap {
    _opaque: (),
}

/// Opaque fault record. Real layout lands in Milestone 4 (§9.2). When
/// `pending_fault` is non-null, generated code branches to its fault epilogue
/// at the next safepoint.
#[repr(C)]
pub struct Fault {
    _opaque: (),
}

/// One frame in the crash-debugger's snapshot chain (§9.3). Real layout lands
/// in Milestone 10; for Milestone 0 it is an opaque anchor so the context's
/// shape is fixed and ABI-stable within the build.
#[repr(C)]
pub struct DebugFrame {
    _opaque: (),
}

/// The hidden first argument to every generated function.
///
/// Matches the sketch in Appendix B. Fields are raw pointers because generated
/// Cranelift code reads them at a fixed offset with a fixed calling convention;
/// Rust borrows would not survive across the ABI boundary.
#[repr(C)]
pub struct RuntimeContext {
    pub heap: *mut Heap,
    pub pending_fault: *mut Fault,
    pub debug_top: *mut DebugFrame,
    pub input_source: GcRef,
    pub current_generation: u64,
}

impl RuntimeContext {
    /// Construct a context with all pointers null and the input source set to
    /// the canonical placeholder. Real runtime setup (rooting the heap,
    /// installing a fault sink) happens in Milestone 3+.
    ///
    /// # Safety
    /// `input_source` must be a valid `GcRef` (or the caller must ensure no
    /// generated code dereferences it before the runtime is fully initialized).
    pub unsafe fn placeholder(input_source: GcRef) -> RuntimeContext {
        RuntimeContext {
            heap: std::ptr::null_mut(),
            pending_fault: std::ptr::null_mut(),
            debug_top: std::ptr::null_mut(),
            input_source,
            current_generation: 0,
        }
    }

    /// True iff a fault is currently pending on this context. Generated code
    /// checks this at safepoints after potentially-faulting operations (§9.2).
    #[inline]
    pub fn has_pending_fault(&self) -> bool {
        !self.pending_fault.is_null()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::GcHeader;
    use std::ptr::NonNull;

    #[test]
    fn placeholder_reports_no_fault() {
        let mut header = GcHeader {
            descriptor: std::ptr::null(),
            flags: 0,
        };
        let nn = NonNull::from(&mut header);
        // SAFETY: local live header for the duration of this test.
        let gcref = unsafe { GcRef::from_non_null(nn) };
        let ctx = unsafe { RuntimeContext::placeholder(gcref) };
        assert!(!ctx.has_pending_fault());
        assert_eq!(ctx.current_generation, 0);
    }

    #[test]
    fn has_pending_fault_flips_with_non_null_pointer() {
        let mut header = GcHeader {
            descriptor: std::ptr::null(),
            flags: 0,
        };
        let nn = NonNull::from(&mut header);
        let gcref = unsafe { GcRef::from_non_null(nn) };
        let mut ctx = unsafe { RuntimeContext::placeholder(gcref) };
        assert!(!ctx.has_pending_fault());
        // Synthesize a non-null fault pointer on the stack; we never
        // dereference it, so this is sound for the flag check.
        let fault = Fault { _opaque: () };
        ctx.pending_fault = &fault as *const Fault as *mut Fault;
        assert!(ctx.has_pending_fault());
    }
}
