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
use crate::shadow_frame::ShadowFrame;
use crate::{collections::VecPayload, descriptor::TypeDescriptor};

/// What kind of runtime fault occurred (§9.2, §10.4). Set by the runtime
/// wrapper that detected it; read by the host after the generated code unwinds
/// to its fault epilogue.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    /// No fault pending. The zero state.
    None = 0,
    /// Integer arithmetic overflowed (§4.12).
    IntOverflow = 1,
    /// Division or remainder by zero (§4.12).
    DivByZero = 2,
    /// A collection index was out of bounds (§9.2). Raised by `Vec.get` /
    /// indexing and similar accessors in M5.
    IndexOutOfBounds = 3,
    /// An input parse mismatch (§7.11). Raised by the input-parser interpreter
    /// when the input does not match a parser expression. The fault carries no
    /// structured spans yet (M6 surfaces it as a plain fault; the crash debugger
    /// in M10 will render the input/parser spans from the runtime's plan).
    ParseFailed = 4,
}

impl std::fmt::Display for FaultKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FaultKind::None => write!(f, "no fault"),
            FaultKind::IntOverflow => write!(f, "integer overflow"),
            FaultKind::DivByZero => write!(f, "division by zero"),
            FaultKind::IndexOutOfBounds => write!(f, "index out of bounds"),
            FaultKind::ParseFailed => write!(f, "input parse mismatch"),
        }
    }
}

/// The fault record a [`RuntimeContext`] points at. `pending_fault` is non-null
/// and points at the owning runtime's slot; a fault is "pending" when
/// [`Fault::kind`] is not [`FaultKind::None`] (the `pending` bool mirrors that
/// for a cheap single-byte check in generated code).
#[repr(C)]
pub struct Fault {
    /// True iff a fault is pending (mirrors `kind != None`).
    pub pending: bool,
    /// The kind of fault, when pending.
    pub kind: FaultKind,
}

impl Fault {
    /// A fresh, clear fault record (no fault pending).
    pub fn clear() -> Self {
        Fault {
            pending: false,
            kind: FaultKind::None,
        }
    }

    /// Mark a fault of `kind` as pending.
    pub fn set(&mut self, kind: FaultKind) {
        self.pending = true;
        self.kind = kind;
    }

    /// True iff a fault is pending.
    pub fn is_pending(&self) -> bool {
        self.pending
    }
}

impl Default for Fault {
    fn default() -> Self {
        Self::clear()
    }
}

/// One local variable in a debug frame snapshot (§9.3, M5).
///
/// Carries the source name, the compiler-assigned `symbol_id` (which
/// disambiguates shadowed bindings — two `let a` in the same scope get distinct
/// ids, §4.2), and the current `GcRef` value. The crash debugger (M10) reads
/// these to display locals; M5 only *registers* them (the prologue/epilogue
/// push/pop frames and the spill updates the values).
#[repr(C)]
pub struct DebugLocal {
    /// The source name as written (e.g. `a`). Not owned by the frame; points at
    /// a `'static` string the compiler embedded.
    pub source_name: *const u8,
    /// The name's byte length.
    pub name_len: u32,
    /// The compiler-assigned symbol id (disambiguates shadowed bindings, §4.2).
    pub symbol_id: u32,
    /// The current value of the local (updated by the spill at safepoints).
    pub value: GcRef,
}

/// One frame in the crash-debugger's snapshot chain (§9.3, M5).
///
/// M5 gives `DebugFrame` a real layout: a parent pointer (the call chain), the
/// function name, and a slice of [`DebugLocal`] entries. The prologue helper
/// ([`crate::debug::push_debug_frame`]) allocates and links a frame; the
/// epilogue pops it. The shadow-stack spill (ADR-019) keeps the `value` fields
/// fresh across GC safepoints so a crash snapshot reflects live state.
///
/// The crash-debugger REPL that *reads* these frames lands in M10; M5 only
/// ensures the metadata is correct and registered.
#[repr(C)]
pub struct DebugFrame {
    /// The caller's frame, or null for the outermost (`main`) frame.
    pub parent: *mut DebugFrame,
    /// The function's source name (a `'static` embedded string).
    pub func_name: *const u8,
    /// The function name's byte length.
    pub func_name_len: u32,
    /// The local-variable entries, as a pointer + count (FFI-safe slice).
    pub locals: *mut DebugLocal,
    /// How many locals are in the `locals` array.
    pub local_count: u32,
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
    /// The current top of the compiler-managed shadow-stack root chain (§12.3,
    /// ADR-019). Generated code pushes a frame in the prologue, spills live
    /// `GcRef`s into it at safepoints, and pops it in the epilogue. The
    /// collector walks this chain via [`RootSet`].
    pub roots: *mut ShadowFrame,
    pub input_source: GcRef,
    /// The cached immortal `Unit` — the "defined dummy" returned on fault paths
    /// (§10.4). M6 split this from `input_source` (which now holds the read-in
    /// buffer when present), so fault sentinels are stable regardless of input.
    pub unit_ref: GcRef,
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
            roots: std::ptr::null_mut(),
            input_source,
            // Placeholder: reuse the input_source ref as the Unit sentinel too,
            // since this constructor is only for not-yet-wired test scaffolding.
            unit_ref: input_source,
            current_generation: 0,
        }
    }

    /// True iff a fault is currently pending on this context. Generated code
    /// checks this at safepoints after potentially-faulting operations (§10.4).
    ///
    /// `pending_fault` is non-null once the context is wired to a runtime; a
    /// fault is pending when the pointed-at [`Fault`] slot says so.
    #[inline]
    pub fn has_pending_fault(&self) -> bool {
        if self.pending_fault.is_null() {
            return false;
        }
        // SAFETY: a non-null `pending_fault` points at a live `Fault` owned by
        // the runtime for as long as the context is in use.
        unsafe { (*self.pending_fault).is_pending() }
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
    /// The fault slot generated code signals through (§10.4). Owned here so its
    /// address is stable for the lifetime of the runtime.
    fault: Fault,
}

impl Runtime {
    /// Create a runtime with a fresh heap and the immortal singletons allocated.
    pub fn new() -> Self {
        let heap = Heap::new();
        // Immortals must be allocated before any collection can run.
        let immortals = Immortals::new(&heap);
        Runtime {
            heap,
            immortals,
            fault: Fault::clear(),
        }
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

    /// A `RuntimeContext` view of this runtime, suitable for generated code.
    /// `pending_fault` points at this runtime's fault slot; `debug_top` stays
    /// null until the debugger lands (M10); `roots` starts null — the first
    /// generated function's prologue pushes the initial shadow frame.
    pub fn context(&mut self) -> RuntimeContext {
        RuntimeContext {
            heap: &mut self.heap as *mut Heap,
            pending_fault: &mut self.fault as *mut Fault,
            debug_top: std::ptr::null_mut(),
            roots: std::ptr::null_mut(),
            input_source: self.immortals.unit(),
            unit_ref: self.immortals.unit(),
            current_generation: 0,
        }
    }

    /// The current fault state (§10.4). `FaultKind::None` when no fault is set.
    pub fn fault(&self) -> FaultKind {
        self.fault.kind
    }

    /// True iff a fault is pending.
    pub fn has_pending_fault(&self) -> bool {
        self.fault.is_pending()
    }

    /// Clear any pending fault, returning the kind that was pending (if any).
    pub fn take_fault(&mut self) -> Option<FaultKind> {
        let kind = self.fault.kind;
        if self.fault.is_pending() {
            self.fault = Fault::clear();
            Some(kind)
        } else {
            None
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
        // SAFETY: TextPayload matches TEXT's size/align and is fully initialized.
        unsafe {
            self.heap.alloc_with(
                crate::text::TEXT,
                std::mem::size_of::<crate::text::TextPayload>(),
                std::mem::align_of::<crate::text::TextPayload>(),
                |payload| {
                    (payload as *mut crate::text::TextPayload)
                        .write(crate::text::TextPayload::Owned(owned));
                },
            )
        }
    }

    /// Allocate a source-slice `Text` — a zero-copy view into `owner`'s bytes
    /// spanning `[start, start+len)` (§7.10, ADR-013). The slice's descriptor
    /// traces `owner`, keeping the backing alive.
    ///
    /// # Panics
    /// Debug-build assertion that the byte range lands within the owner; the
    /// parser guarantees this by construction.
    pub fn alloc_text_slice(&self, owner: GcRef, start: usize, len: usize) -> GcRef {
        debug_assert!(
            start.saturating_add(len) <= owner.as_text().len(),
            "source-slice Text range [{start}, {start}+{len}) exceeds owner length"
        );
        let payload = crate::text::TextPayload::Slice { owner, start, len };
        // SAFETY: TextPayload matches TEXT's size/align and is fully initialized.
        unsafe {
            self.heap.alloc_with(
                crate::text::TEXT,
                std::mem::size_of::<crate::text::TextPayload>(),
                std::mem::align_of::<crate::text::TextPayload>(),
                |ptr| (ptr as *mut crate::text::TextPayload).write(payload),
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
                        items,
                    });
                },
            )
        }
    }

    /// Allocate a `Grid[T]` from a flat row-major list of cells, the element
    /// descriptor, and the column count (§7.5, M6). `items.len()` must be a
    /// multiple of `width`.
    pub fn alloc_grid(
        &self,
        element_descriptor: &'static TypeDescriptor,
        items: Vec<GcRef>,
        width: usize,
    ) -> GcRef {
        debug_assert!(
            width == 0 || items.len() % width == 0,
            "grid items ({}) not a multiple of width ({})",
            items.len(),
            width
        );
        // SAFETY: GridPayload matches GRID's size/align and is fully initialized.
        unsafe {
            self.heap.alloc_with(
                crate::collections::GRID,
                std::mem::size_of::<crate::collections::GridPayload>(),
                std::mem::align_of::<crate::collections::GridPayload>(),
                |payload| {
                    (payload as *mut crate::collections::GridPayload).write(
                        crate::collections::GridPayload {
                            element_descriptor,
                            items,
                            width,
                        },
                    );
                },
            )
        }
    }

    /// Allocate a provisional structural `Record` from field values and a static
    /// schema (§7.8, M6). `items.len()` must equal `schema.arity()`.
    pub fn alloc_record(
        &self,
        schema: &'static crate::records::RecordSchema,
        items: Vec<GcRef>,
    ) -> GcRef {
        debug_assert_eq!(
            items.len(),
            schema.arity(),
            "record field count ({}) != schema arity ({})",
            items.len(),
            schema.arity()
        );
        // SAFETY: RecordPayload matches RECORD's size/align and is fully initialized.
        unsafe {
            self.heap.alloc_with(
                crate::records::RECORD,
                std::mem::size_of::<crate::records::RecordPayload>(),
                std::mem::align_of::<crate::records::RecordPayload>(),
                |payload| {
                    (payload as *mut crate::records::RecordPayload)
                        .write(crate::records::RecordPayload { schema, items });
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
    /// as the object is reachable. Handles both owned and source-slice payloads
    /// (ADR-013): a slice reads through its owner.
    pub fn as_text(&self) -> &str {
        assert_eq!(self.descriptor().id, crate::text::TEXT.id, "not Text");
        // SAFETY: descriptor check confirms payload is a TextPayload; the
        // reference is valid while the object lives (non-moving GC, ADR-011).
        let payload = self.payload::<crate::text::TextPayload>() as *const crate::text::TextPayload;
        unsafe { crate::text::text_str(payload) }
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
        let mut fault = Fault::clear();
        fault.set(FaultKind::IntOverflow);
        ctx.pending_fault = &mut fault;
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
        // them. Capture each singleton's address before the collection and assert
        // the same address afterwards (a self-comparison would assert nothing).
        let rt = Runtime::new();
        let unit_before = rt.immortals().unit().as_ptr();
        let true_before = rt.immortals().true_().as_ptr();
        let false_before = rt.immortals().false_().as_ptr();
        let roots = RootScope::new();
        rt.collect(&roots);
        assert_eq!(rt.immortals().unit().as_ptr(), unit_before);
        assert_eq!(rt.immortals().true_().as_ptr(), true_before);
        assert_eq!(rt.immortals().false_().as_ptr(), false_before);
    }

    #[test]
    #[should_panic(expected = "not an Int")]
    fn as_int_rejects_wrong_descriptor() {
        let rt = Runtime::new();
        let b = rt.alloc_bool(false);
        let _ = b.as_int();
    }
}
