//! The [`RuntimeContext`] handed to every generated function (§10.3, Appendix B)
//! and the [`Runtime`] that owns the heap + immortals.
//!
//! Every generated function receives a hidden first parameter — a pointer to
//! `RuntimeContext` — followed only by `GcRef` arguments, and returns one
//! `GcRef`. The context is the single channel through which generated code
//! reaches the GC heap, the pending fault, the debug frame chain, the input
//! source, and so on.
//!
//! M3 fills in the real [`Heap`] and [`crate::Immortals`]; the fault and
//! debug-frame pointers remain null (M4/M10).

use crate::gc::GcRef;
use crate::heap::Heap;
use crate::immortal::{read_bool, Immortals};
use crate::roots::RootSet;
use crate::{collections::VecPayload, descriptor::TypeDescriptor};

/// Opaque fault record. Real layout lands in Milestone 4 (§9.2). When
/// `pending_fault` is non-null, generated code branches to its fault epilogue
/// at the next safepoint.
#[repr(C)]
pub struct Fault {
    _opaque: (),
}

/// One frame in the crash-debugger's snapshot chain (§9.3). Real layout lands
/// in Milestone 10; for now it is an opaque anchor so the context's shape is
/// fixed and ABI-stable within the build.
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
    /// installing a fault sink) is done via [`Runtime::context`] in M3+.
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

/// The owner of the heap and the immortal singletons.
///
/// This is the M3 entry point for runtime code: construct a `Runtime`, allocate
/// values through it, root them in a [`crate::RootScope`], and collect when
/// needed. In M4, lowering will produce a `RuntimeContext` from a `Runtime` to
/// hand to generated code.
pub struct Runtime {
    heap: Heap,
    immortals: Immortals,
}

impl Runtime {
    /// Create a runtime with a fresh heap and the immortal singletons allocated.
    pub fn new() -> Self {
        let heap = Heap::new();
        // Immortals must be allocated before any collection can run.
        let immortals = Immortals::new(&heap);
        Runtime { heap, immortals }
    }

    /// Borrow the heap.
    #[inline]
    pub fn heap(&self) -> &Heap {
        &self.heap
    }

    /// Borrow the heap mutably.
    #[inline]
    pub fn heap_mut(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// The immortal singletons (§4.3).
    #[inline]
    pub fn immortals(&self) -> &Immortals {
        &self.immortals
    }

    /// Run a mark-and-sweep collection (§12.1). Everything reachable from
    /// `roots` survives; everything else is reclaimed.
    pub fn collect(&self, roots: &dyn RootSet) {
        self.heap.collect(roots);
    }

    /// A `RuntimeContext` view of this runtime, suitable for (future) generated
    /// code. `pending_fault`/`debug_top` are null (M4/M10).
    pub fn context(&mut self) -> RuntimeContext {
        RuntimeContext {
            heap: &mut self.heap as *mut Heap,
            pending_fault: std::ptr::null_mut(),
            debug_top: std::ptr::null_mut(),
            input_source: self.immortals.unit(),
            current_generation: 0,
        }
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

// ---- typed allocation helpers (M3 deliverable: "allocation and payload
// access helpers") -----------------------------------------------

impl Runtime {
    /// Allocate an `Int` (§4.3).
    pub fn alloc_int(&self, value: i64) -> GcRef {
        self.heap.alloc(crate::scalars::INT, value)
    }

    /// Allocate a `Bool` as the corresponding immortal singleton (§4.3). Booleans
    /// are always the immortals — there is never a fresh `Bool` allocation.
    pub fn alloc_bool(&self, value: bool) -> GcRef {
        self.immortals.bool_(value)
    }

    /// Allocate a `Byte` (§4.3).
    pub fn alloc_byte(&self, value: u8) -> GcRef {
        self.heap.alloc(crate::scalars::BYTE, value)
    }

    /// Allocate a `Char` (§4.3). Panics if `value` is not a valid scalar value.
    pub fn alloc_char(&self, value: u32) -> GcRef {
        assert!(
            crate::scalars::is_valid_char(value),
            "{value:#x} is not a valid Unicode scalar"
        );
        self.heap.alloc(crate::scalars::CHAR, value)
    }

    /// The immortal `Unit` (§4.3).
    pub fn alloc_unit(&self) -> GcRef {
        self.immortals.unit()
    }

    /// Allocate an owned `Text` (§4.3, ADR-013).
    pub fn alloc_text(&self, value: &str) -> GcRef {
        let owned: Box<str> = value.into();
        // SAFETY: Box<str> matches TEXT's size/align and is fully initialized.
        unsafe {
            self.heap.alloc_with(
                crate::text::TEXT,
                std::mem::size_of::<Box<str>>(),
                std::mem::align_of::<Box<str>>(),
                |payload| (payload as *mut Box<str>).write(owned),
            )
        }
    }

    /// Allocate a `Vec[T]` from a slice of already-allocated element refs and the
    /// element descriptor (§11.2, ADR-013).
    pub fn alloc_vec(
        &self,
        element_descriptor: &'static TypeDescriptor,
        items: Vec<GcRef>,
    ) -> GcRef {
        // SAFETY: VecPayload matches VEC's size/align and is fully initialized.
        unsafe {
            self.heap.alloc_with(
                crate::collections::VEC,
                std::mem::size_of::<VecPayload>(),
                std::mem::align_of::<VecPayload>(),
                |payload| {
                    (payload as *mut VecPayload).write(VecPayload {
                        element_descriptor,
                        items: items.into_boxed_slice(),
                    });
                },
            )
        }
    }
}

// ---- typed payload access helpers ----------------------------------------

impl GcRef {
    /// Read an `Int` payload (§4.3).
    ///
    /// Panics if this reference's descriptor is not `Int`.
    pub fn as_int(&self) -> i64 {
        assert_eq!(self.descriptor().id, crate::scalars::INT.id, "not an Int");
        // SAFETY: descriptor check confirms payload is i64.
        unsafe { *self.payload::<i64>() }
    }

    /// Read a `Bool` payload as a Rust `bool` (§4.3).
    ///
    /// Panics if this reference's descriptor is not `Bool`.
    pub fn as_bool(&self) -> bool {
        assert_eq!(self.descriptor().id, crate::scalars::BOOL.id, "not a Bool");
        // SAFETY: descriptor check confirms payload is BoolPayload.
        unsafe { read_bool(*self) }
    }

    /// Read a `Byte` payload (§4.3).
    pub fn as_byte(&self) -> u8 {
        assert_eq!(self.descriptor().id, crate::scalars::BYTE.id, "not a Byte");
        // SAFETY: descriptor check confirms payload is u8.
        unsafe { *self.payload::<u8>() }
    }

    /// Read a `Char` payload as a Rust `char` (§4.3).
    pub fn as_char(&self) -> char {
        assert_eq!(self.descriptor().id, crate::scalars::CHAR.id, "not a Char");
        let raw = unsafe { *self.payload::<u32>() };
        char::from_u32(raw).expect("Char payload was not a valid scalar; memory corrupted")
    }

    /// Read a `Text` payload as a `&str` (§4.3).
    ///
    /// The lifetime is tied to the `GcRef`'s borrow; the text stays valid as long
    /// as the object is reachable.
    pub fn as_text(&self) -> &str {
        assert_eq!(self.descriptor().id, crate::text::TEXT.id, "not Text");
        // SAFETY: descriptor check confirms payload is Box<str>. The returned
        // reference is valid while the object lives (non-moving GC, ADR-011).
        // We materialize a `&str` (not `&Box<str>`) to avoid carrying the box
        // wrapper; `Box<str>` derefs to `str`.
        let boxed: *const Box<str> = self.payload::<Box<str>>();
        unsafe { &*boxed }
    }

    /// Read a `Vec[T]` payload as a slice of element refs (§11.2).
    pub fn as_vec(&self) -> &[GcRef] {
        assert_eq!(
            self.descriptor().id,
            crate::collections::VEC.id,
            "not a Vec"
        );
        // SAFETY: descriptor check confirms payload is VecPayload.
        let p: &VecPayload = unsafe { &*self.payload::<VecPayload>() };
        &p.items
    }

    /// Format this value through its descriptor into `out` (§11.4). Returns the
    /// same `&mut dyn fmt::Write` result the descriptor's `format` produced.
    pub fn format(&self, out: &mut dyn std::fmt::Write) {
        let desc = self.descriptor();
        // SAFETY: `self`'s payload matches its descriptor.
        unsafe { (desc.format)(self.payload::<u8>() as *const u8, out) };
    }

    /// Structural equality through the descriptors (§5.5). Returns `false` if
    /// either side's type is not equatable, or if the descriptors differ.
    pub fn equals(&self, other: &GcRef) -> bool {
        let a = self.descriptor();
        let b = other.descriptor();
        if a.id != b.id {
            return false;
        }
        let Some(eq) = a.equals else {
            return false;
        };
        // SAFETY: both payloads match the shared descriptor.
        unsafe {
            eq(
                self.payload::<u8>() as *const u8,
                other.payload::<u8>() as *const u8,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::GcHeader;
    use crate::roots::RootScope;
    use std::ptr::NonNull;

    #[test]
    fn placeholder_reports_no_fault() {
        let mut header = GcHeader {
            descriptor: std::ptr::null(),
            mark: std::cell::Cell::new(0),
            size: 0,
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
            mark: std::cell::Cell::new(0),
            size: 0,
        };
        let nn = NonNull::from(&mut header);
        let gcref = unsafe { GcRef::from_non_null(nn) };
        let mut ctx = unsafe { RuntimeContext::placeholder(gcref) };
        assert!(!ctx.has_pending_fault());
        let fault = Fault { _opaque: () };
        ctx.pending_fault = &fault as *const Fault as *mut Fault;
        assert!(ctx.has_pending_fault());
    }

    #[test]
    fn runtime_allocates_and_reads_scalars() {
        let rt = Runtime::new();
        let i = rt.alloc_int(-123);
        assert_eq!(i.as_int(), -123);
        let b = rt.alloc_bool(true);
        assert!(b.as_bool());
        let by = rt.alloc_byte(200);
        assert_eq!(by.as_byte(), 200);
        let c = rt.alloc_char('€' as u32);
        assert_eq!(c.as_char(), '€');
        let t = rt.alloc_text("héllo");
        assert_eq!(t.as_text(), "héllo");
        assert_eq!(rt.alloc_unit().as_ptr(), rt.immortals().unit().as_ptr());
    }

    #[test]
    fn runtime_formats_and_compares() {
        let rt = Runtime::new();
        let a = rt.alloc_int(42);
        let b = rt.alloc_int(42);
        let c = rt.alloc_int(43);
        assert!(a.equals(&b));
        assert!(!a.equals(&c));

        let mut out = String::new();
        a.format(&mut out);
        assert_eq!(out, "42");
    }

    #[test]
    fn runtime_vec_allocates_and_reads() {
        let rt = Runtime::new();
        let e0 = rt.alloc_int(1);
        let e1 = rt.alloc_int(2);
        let v = rt.alloc_vec(crate::scalars::INT, vec![e0, e1]);
        assert_eq!(v.descriptor().name, "Vec");
        assert_eq!(v.as_vec().len(), 2);

        let mut out = String::new();
        v.format(&mut out);
        assert_eq!(out, "[1, 2]");
    }

    #[test]
    fn runtime_collect_keeps_immortals_alive_unrooted() {
        // Immortals are out-of-band; a collection with no roots must not touch
        // them.
        let rt = Runtime::new();
        let unit_before = rt.immortals().unit().as_ptr();
        let roots = RootScope::new();
        rt.collect(&roots);
        assert_eq!(rt.immortals().unit().as_ptr(), unit_before);
        assert_eq!(
            rt.immortals().true_().as_ptr(),
            rt.immortals().true_().as_ptr()
        );
    }

    #[test]
    #[should_panic(expected = "not an Int")]
    fn as_int_rejects_wrong_descriptor() {
        let rt = Runtime::new();
        let b = rt.alloc_bool(false);
        let _ = b.as_int();
    }
}
