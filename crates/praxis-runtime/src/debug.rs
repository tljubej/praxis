//! Crash-debugger frame registration (§9.3, M5).
//!
//! M5 gives [`DebugFrame`] a real layout and provides extern helpers the JIT
//! prologue/epilogue call to push/pop frames on the context's `debug_top`
//! chain. The crash-debugger REPL that *reads* these frames lands in M10; M5
//! only ensures the metadata (source name, symbol id, function name) is correct
//! and registered, so shadowed locals are distinguishable by `(source_name,
//! symbol_id)` per the §4.2 acceptance criterion.
//!
//! Frames are allocated by the runtime (a `Box`) so `Drop` reclaims them; the
//! spill (ADR-019) updates each local's `value` field at safepoints so a
//! snapshot reflects live state.

use std::ptr::NonNull;

use crate::context::{DebugFrame, DebugLocal, RuntimeContext};
use crate::gc::GcRef;

/// One local's metadata at frame construction: the source name (ptr + len),
/// the compiler-assigned symbol id, the local's static type descriptor, and the
/// full static `Type` id. Flattened for FFI.
#[repr(C)]
pub struct DebugLocalMeta {
    pub source_name: *const u8,
    pub name_len: u32,
    pub symbol_id: u32,
    /// The local's static type descriptor (§9.3). The backend embeds the
    /// `'static TypeDescriptor` resolved from the MIR local's `Type`.
    pub descriptor: *const crate::TypeDescriptor,
    /// The full static `Type` id (`praxis_types::Type(u32)` handle, M10-WS1b).
    /// Lets the debugger reconstruct the exact local type (incl. collection
    /// element types / record shapes) the runtime `descriptor` alone loses.
    pub type_id: u32,
}

/// Allocate a debug frame for `func_name` with `local_count` local slots, chain
/// it onto `ctx.debug_top`, and return its address. Called in every generated
/// function's prologue.
///
/// `local_metas` is an array of [`DebugLocalMeta`] — one per local — so the
/// frame is populated with source names and symbol ids at construction. The
/// `value` fields start as the null sentinel and are updated by the spill.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`. `func_name` must point
/// at `func_name_len` valid UTF-8 bytes valid for the program's lifetime.
/// `local_metas` must point at `local_count` valid entries or be null if
/// `local_count == 0`.
#[no_mangle]
pub unsafe extern "C" fn praxis_push_debug_frame(
    ctx: *mut RuntimeContext,
    func_name: *const u8,
    func_name_len: u32,
    local_count: u32,
    local_metas: *const DebugLocalMeta,
) -> *mut DebugFrame {
    if ctx.is_null() {
        return std::ptr::null_mut();
    }
    let count = local_count as usize;
    let locals: Vec<DebugLocal> = if count == 0 || local_metas.is_null() {
        Vec::new()
    } else {
        // SAFETY: caller guarantees `local_metas` points at `count` valid entries.
        let metas = unsafe { std::slice::from_raw_parts(local_metas, count) };
        metas
            .iter()
            .map(|m| DebugLocal {
                source_name: m.source_name,
                name_len: m.name_len,
                symbol_id: m.symbol_id,
                descriptor: m.descriptor,
                value: GcRef::null_sentinel_ref(),
                type_id: m.type_id,
            })
            .collect()
    };
    let mut locals_box = locals.into_boxed_slice();
    let locals_ptr = locals_box.as_mut_ptr();
    std::mem::forget(locals_box);
    let frame = Box::new(DebugFrame {
        parent: std::ptr::null_mut(),
        func_name,
        func_name_len,
        locals: locals_ptr,
        local_count,
        // Reserved for M10b: source span and active-parser path are zeroed/null
        // until the backend threads them from the AST/plan.
        source_span: (0, 0),
        parser_path: std::ptr::null(),
        parser_path_len: 0,
    });
    let raw = Box::into_raw(frame);
    // SAFETY: ctx is live; chain onto debug_top.
    unsafe {
        (*raw).parent = (*ctx).debug_top;
        (*ctx).debug_top = raw;
    }
    raw
}

/// Set the source span `[start, end)` (byte offsets into program source) on the
/// frame at `ctx.debug_top` (§9.3 "current source span"). The backend calls
/// this in the prologue, right after [`praxis_push_debug_frame`], so each
/// generated function records its source extent for the `source` REPL command.
/// `M10b-WS1`: spans flow AST → HIR `TypedFn` → MIR `Function` → backend here.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext` whose `debug_top` is a
/// valid frame (the frame just pushed by the caller).
#[no_mangle]
pub unsafe extern "C" fn praxis_set_frame_source_span(
    ctx: *mut RuntimeContext,
    start: u32,
    end: u32,
) {
    if ctx.is_null() {
        return;
    }
    let top = unsafe { (*ctx).debug_top };
    if top.is_null() {
        return;
    }
    // SAFETY: caller guarantees debug_top is the frame just pushed.
    unsafe {
        (*top).source_span = (start, end);
    }
}

/// Pop the frame at `ctx.debug_top` (must be `frame`), restoring the parent,
/// and free it. Called in every generated function's epilogue.
///
/// # Safety
/// `ctx` must be live and wired; `frame` must be the current top.
#[no_mangle]
pub unsafe extern "C" fn praxis_pop_debug_frame(ctx: *mut RuntimeContext, frame: *mut DebugFrame) {
    if ctx.is_null() || frame.is_null() {
        return;
    }
    // SAFETY: caller guarantees frame is the current top and is valid.
    unsafe {
        let parent = (*frame).parent;
        // Free the locals array.
        if !(*frame).locals.is_null() && (*frame).local_count > 0 {
            let count = (*frame).local_count as usize;
            // SAFETY: locals was allocated via into_boxed_slice; reconstruct it.
            let local_slice = std::ptr::slice_from_raw_parts_mut((*frame).locals, count);
            let _ = Box::from_raw(local_slice);
        }
        (*ctx).debug_top = parent;
        let _ = Box::from_raw(frame);
    }
}

impl DebugFrame {
    /// The function name as a `&str`, or `"<unknown>"`. For M10/M5 testing.
    ///
    /// # Safety
    /// `func_name` must be valid UTF-8 for `func_name_len` bytes.
    pub fn name(&self) -> &str {
        if self.func_name.is_null() || self.func_name_len == 0 {
            return "<unknown>";
        }
        // SAFETY: caller (the compiler) guarantees valid UTF-8 for the lifetime
        // of the program.
        unsafe {
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                self.func_name,
                self.func_name_len as usize,
            ))
        }
    }

    /// The locals as a slice. For M10/M5 testing.
    pub fn locals(&self) -> &[DebugLocal] {
        if self.locals.is_null() || self.local_count == 0 {
            &[]
        } else {
            // SAFETY: locals was allocated with local_count entries.
            unsafe { std::slice::from_raw_parts(self.locals, self.local_count as usize) }
        }
    }

    /// Walk the parent chain from this frame, collecting `(func_name, locals)`.
    /// For M10/M5 structural testing.
    pub fn chain(&self) -> Vec<(&str, &[DebugLocal])> {
        let mut out = vec![(self.name(), self.locals())];
        let mut cur = self.parent;
        while !cur.is_null() {
            // SAFETY: parent chain is live until the frame is popped.
            let f = unsafe { &*cur };
            out.push((f.name(), f.locals()));
            cur = f.parent;
        }
        out
    }
}

impl DebugLocal {
    /// The source name as a `String`. Allocates; for testing/debugger only.
    pub fn name(&self) -> String {
        if self.source_name.is_null() || self.name_len == 0 {
            return String::new();
        }
        // SAFETY: caller (compiler) guarantees valid UTF-8.
        unsafe {
            String::from_utf8_lossy(std::slice::from_raw_parts(
                self.source_name,
                self.name_len as usize,
            ))
            .into_owned()
        }
    }
}

impl GcRef {
    /// A null-valued GcRef used as the initial value for debug-local slots that
    /// haven't been written yet. This is the same pointer value the shadow frame
    /// uses for null slots (ADR-019), but expressed as a GcRef for the debug
    /// frame's `value` field. It is never dereferenced.
    fn null_sentinel_ref() -> GcRef {
        // Reuse the heap's null header pattern: a dangling NonNull that is never
        // dereferenced. We use the alignment-marker trick: NonNull::dangling()
        // gives a non-null pointer that is never a real allocation.
        let nn = NonNull::dangling();
        // SAFETY: dangling NonNull is non-null and aligned; never dereferenced.
        unsafe { GcRef::from_non_null(nn) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Runtime;

    fn wired_ctx(rt: &mut Runtime) -> *mut RuntimeContext {
        let ctx = Box::leak(Box::new(rt.context()));
        ctx as *mut RuntimeContext
    }

    unsafe fn drop_ctx(ctx: *mut RuntimeContext) {
        let _ = unsafe { Box::from_raw(ctx) };
    }

    #[test]
    fn push_and_pop_frame() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let name = b"main";
        // SAFETY: ctx is wired; name is valid for the program's lifetime.
        unsafe {
            let frame =
                praxis_push_debug_frame(ctx, name.as_ptr(), name.len() as u32, 0, std::ptr::null());
            assert!(!frame.is_null());
            assert_eq!((*frame).name(), "main");
            assert!((*ctx).debug_top == frame);
            praxis_pop_debug_frame(ctx, frame);
            assert!((*ctx).debug_top.is_null());
        }
        unsafe { drop_ctx(ctx) };
    }

    #[test]
    fn frame_carries_local_metadata() {
        // Two shadowed `a` bindings must have distinct symbol ids.
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        let name_a = b"a";
        let metas = [
            DebugLocalMeta {
                source_name: name_a.as_ptr(),
                name_len: 1,
                symbol_id: 10,
                descriptor: crate::scalars::INT,
                type_id: 1,
            },
            DebugLocalMeta {
                source_name: name_a.as_ptr(),
                name_len: 1,
                symbol_id: 20,
                descriptor: crate::scalars::INT,
                type_id: 1,
            },
        ];
        // SAFETY: ctx wired; metas is valid.
        unsafe {
            let frame = praxis_push_debug_frame(ctx, b"f".as_ptr(), 1, 2, metas.as_ptr());
            let locals = (*frame).locals();
            assert_eq!(locals.len(), 2);
            // Both named "a" but distinct symbol ids — the §4.2 guarantee.
            assert_eq!(locals[0].name(), "a");
            assert_eq!(locals[1].name(), "a");
            assert_ne!(locals[0].symbol_id, locals[1].symbol_id);
            // M10-WS2: the descriptor is carried through to the frame.
            assert_eq!(
                locals[0].descriptor as *const _,
                crate::scalars::INT as *const _
            );
            // M10-WS1b: the full static Type id is carried through.
            assert_eq!(locals[0].type_id, 1);
            assert_eq!(locals[1].type_id, 1);
            praxis_pop_debug_frame(ctx, frame);
        }
        unsafe { drop_ctx(ctx) };
    }

    /// M10-WS1: `praxis_set_frame_source_span` records the span on the
    /// just-pushed frame, so the `source` REPL command can render it.
    #[test]
    fn set_frame_source_span_records_on_top() {
        let mut rt = Runtime::new();
        let ctx = wired_ctx(&mut rt);
        // SAFETY: ctx is wired; name is valid for the program's lifetime.
        unsafe {
            let frame = praxis_push_debug_frame(ctx, b"f".as_ptr(), 1, 0, std::ptr::null());
            // Freshly pushed frames default to (0, 0).
            assert_eq!((*frame).source_span, (0, 0));
            praxis_set_frame_source_span(ctx, 40, 90);
            assert_eq!((*frame).source_span, (40, 90));
            // A second push+set records a *different* span on the new top, not
            // the old frame — the setter always targets `debug_top`.
            let outer = praxis_push_debug_frame(ctx, b"g".as_ptr(), 1, 0, std::ptr::null());
            praxis_set_frame_source_span(ctx, 7, 9);
            assert_eq!((*outer).source_span, (7, 9));
            // The inner frame keeps its span.
            assert_eq!((*frame).source_span, (40, 90));
            praxis_pop_debug_frame(ctx, outer);
            praxis_pop_debug_frame(ctx, frame);
        }
        unsafe { drop_ctx(ctx) };
    }

    /// M10-WS1: the setter is null-safe (no crash on a null ctx / no top).
    #[test]
    fn set_frame_source_span_is_null_safe() {
        // SAFETY: the contract is null-safety; we exercise it directly.
        unsafe {
            praxis_set_frame_source_span(std::ptr::null_mut(), 1, 2);
        }
    }
}
