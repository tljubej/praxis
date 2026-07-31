//! Compiler-managed shadow-stack frames (§12.3, ADR-019).
//!
//! §12.3 offers "compiler-managed shadow-stack frames **or** explicit root
//! frames." M3 shipped explicit root frames ([`RootScope`], ADR-012) for the
//! host. M5 adds the compiler-managed shadow stack that JIT-generated code
//! spills into: at every GC safepoint (allocation / call that may allocate),
//! the Cranelift backend stores the live `GcRef` locals into a
//! [`ShadowFrame`]'s slots *before* the safepoint and reloads them after.
//!
//! A frame is a fixed-capacity array of raw slot pointers (one per `Gc` local
//! in the owning function, sized by the backend at compile time) plus a parent
//! pointer chaining to the caller's frame. The collector walks the chain via
//! [`RootSet`], exactly like a [`RootScope`].
//!
//! Slots are raw `*mut GcHeader` (not [`GcRef`]) because a slot is *null until
//! the backend writes a value into it*: a local may be live across a safepoint
//! (so it must be in the root set) before it has ever been assigned at runtime.
//! `GcRef` is `NonNull` by construction, so it cannot represent that state; the
//! raw pointer can, and `push_roots` skips nulls.
//!
//! Frames are allocated by the runtime (a `Box` pushed through an extern
//! helper) so `Drop` reclaims them; generated code touches only the raw slot
//! memory at fixed offsets. Each frame is `#[repr(C)]` so the slot offset
//! seen by Cranelift is stable (Appendix B).

use crate::abi::abi_guard;
use crate::gc::{GcHeader, GcRef};
use crate::roots::RootSet;

/// The maximum number of `Gc` roots a single JIT'd function may spill. The
/// backend rejects (at compile time) any function exceeding this; real Praxis
/// functions have small root sets, and a fixed cap keeps every frame a single
/// allocation with no indirection.
///
/// This is part of the contract between the backend and the runtime; bumping it
/// is an ABI-affecting change caught by the ABI version check (§11.6) only in
/// that the two are rebuilt together. The offset of `slots` within the frame is
/// stable regardless, because the struct is `#[repr(C)]`.
/// This is part of the contract between the backend and the runtime; bumping it
/// is an ABI-affecting change caught by the ABI version check (§11.6) only in
/// that the two are rebuilt together. The offset of `slots` within the frame is
/// stable regardless, because the struct is `#[repr(C)]`.
///
/// M8 raises this from 64 to 192 to accommodate AoC-style graph programs that
/// allocate many collections (Deque/Set/Map/Vec) in a single frame.
pub const MAX_SHADOW_SLOTS: usize = 192;

/// One compiler-managed shadow-stack frame (§12.3).
///
/// Generated code does *not* construct this directly; it calls
/// [`praxis_push_shadow_frame`] / [`praxis_pop_shadow_frame`] in the
/// prologue/epilogue. It then writes `GcRef`s into `slots[index]` at safepoints
/// and reads them back.
///
/// `parent` chains to the caller's frame so the collector walks the whole
/// stack. `slot_count` is the number of valid slots (the function's `Gc` local
/// count); slots beyond it are never read.
#[repr(C)]
pub struct ShadowFrame {
    /// The caller's frame, or null for the outermost (`main`) frame.
    pub parent: *mut ShadowFrame,
    /// How many of `slots` are in use (set once at prologue).
    pub slot_count: u32,
    /// The root slots, as raw nullable pointers. Generated code indexes these by
    /// `Gc`-local id. Null = "not yet written at this safepoint"; the collector
    /// skips nulls. Only the first `slot_count` are read.
    pub slots: [*mut GcHeader; MAX_SHADOW_SLOTS],
}

impl ShadowFrame {
    /// A frame with `slot_count` null slots and no parent. The backend fills
    /// `parent` by chaining off the context's current `roots` pointer.
    fn new(slot_count: u32) -> Box<Self> {
        assert!(
            slot_count as usize <= MAX_SHADOW_SLOTS,
            "function root set {slot_count} exceeds MAX_SHADOW_SLOTS ({MAX_SHADOW_SLOTS})"
        );
        Box::new(ShadowFrame {
            parent: std::ptr::null_mut(),
            slot_count,
            slots: [std::ptr::null_mut(); MAX_SHADOW_SLOTS],
        })
    }

    /// The live root refs in this frame (first `slot_count` non-null slots),
    /// converted to `GcRef`. Null slots are skipped.
    fn live_refs(&self) -> Vec<GcRef> {
        let n = self.slot_count as usize;
        self.slots[..n]
            .iter()
            .filter_map(|&p| {
                if p.is_null() {
                    None
                } else {
                    // SAFETY: non-null slots were written by the backend with a
                    // valid GcRef (a live allocation's header pointer).
                    Some(unsafe { GcRef::from_raw(p) })
                }
            })
            .collect()
    }
}

impl RootSet for ShadowFrame {
    fn push_roots(&self, out: &mut Vec<GcRef>) {
        // Walk the parent chain first (caller's roots), then this frame's.
        if !self.parent.is_null() {
            // SAFETY: `parent` is either null or a live frame pushed by a
            // caller that has not yet returned (so its frame is still alive).
            unsafe { (*self.parent).push_roots(out) };
        }
        out.extend(self.live_refs());
    }
}

impl ShadowFrame {
    /// Read the slot at `index` as a `GcRef`, or `None` if null/out of range.
    /// Used only by tests and the debugger; generated code reads the slot
    /// directly at its offset.
    pub fn slot(&self, index: usize) -> Option<GcRef> {
        let p = *self.slots.get(index)?;
        if p.is_null() {
            None
        } else {
            // SAFETY: non-null slot holds a valid header pointer.
            Some(unsafe { GcRef::from_raw(p) })
        }
    }
}

// ---------------------------------------------------------------------------
// Extern helpers the Cranelift prologue/epilogue call.
// ---------------------------------------------------------------------------

/// Allocate a shadow frame with `slot_count` root slots, chain it onto the
/// context's current `roots` top, store its address into the context's `roots`
/// field, and return its address. Called in every generated function's
/// prologue.
///
/// Also bumps the context's `recursion_depth` (§9.2, §17.4); the generated
/// prologue reads it back and branches to the fault epilogue when it exceeds
/// [`MAX_RECURSION_DEPTH`], so deep recursion faults cleanly instead of
/// overflowing the native stack. The matching [`praxis_pop_shadow_frame`]
/// decrements it.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`. `slot_count` must be ≤
/// [`MAX_SHADOW_SLOTS`]. The returned pointer is valid until the matching
/// [`praxis_pop_shadow_frame`] call.
#[no_mangle]
pub unsafe extern "C" fn praxis_push_shadow_frame(
    ctx: *mut crate::RuntimeContext,
    slot_count: u32,
) -> *mut ShadowFrame {
    abi_guard!("praxis_push_shadow_frame", ctx, {
        if ctx.is_null() {
            return std::ptr::null_mut();
        }
        let frame = ShadowFrame::new(slot_count);
        let raw = Box::into_raw(frame);
        // SAFETY: `ctx` is live and wired; chaining reads/writes its `roots` field.
        unsafe {
            (*raw).parent = (*ctx).roots;
            (*ctx).roots = raw;
            // Bump the Praxis call depth; the prologue guard faults if it exceeds
            // MAX_RECURSION_DEPTH. Saturating add so a runaway counter can't wrap.
            let d = (*ctx).recursion_depth.saturating_add(1);
            (*ctx).recursion_depth = d;
        }
        raw
    })
}

/// Pop the frame at `ctx.roots` (must be `frame`), restoring the parent as the
/// current top and freeing `frame`. Called in every generated function's
/// epilogue (including fault epilogues).
///
/// Also decrements the context's `recursion_depth`, balancing the bump in
/// [`praxis_push_shadow_frame`]. Saturating so the depth never underflows on a
/// fault path.
///
/// # Safety
/// `ctx` must point at a live, wired `RuntimeContext`. `frame` must be the
/// exact pointer returned by the matching [`praxis_push_shadow_frame`] call and
/// must currently be the top of the context's root chain.
#[no_mangle]
pub unsafe extern "C" fn praxis_pop_shadow_frame(
    ctx: *mut crate::RuntimeContext,
    frame: *mut ShadowFrame,
) {
    abi_guard!("praxis_pop_shadow_frame", ctx, {
        if ctx.is_null() || frame.is_null() {
            return;
        }
        // SAFETY: caller guarantees `frame` is the current top and is valid.
        unsafe {
            let parent = (*frame).parent;
            (*ctx).roots = parent;
            // Reclaim the Box. After this the slots are invalid.
            let _ = Box::from_raw(frame);
            // Balance the prologue's depth bump (saturating so a fault path can't
            // underflow).
            (*ctx).recursion_depth = (*ctx).recursion_depth.saturating_sub(1);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::NonNull;

    fn dummy_ref() -> *mut GcHeader {
        // A leaked header; only its non-null address matters for root-walking
        // tests. The collector is never run against these.
        let header = Box::leak(Box::new(GcHeader::detached()));
        NonNull::from(header).as_ptr()
    }

    #[test]
    fn empty_frame_roots_nothing() {
        let frame = ShadowFrame::new(0);
        let mut out = Vec::new();
        frame.push_roots(&mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn frame_yields_written_slots_only() {
        let mut frame = ShadowFrame::new(3);
        let a = dummy_ref();
        let b = dummy_ref();
        frame.slots[0] = a;
        frame.slots[2] = b; // slot 1 left null
        let mut out = Vec::new();
        frame.push_roots(&mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].as_ptr(), a);
        assert_eq!(out[1].as_ptr(), b);
    }

    #[test]
    fn parent_chain_walks_all_frames() {
        let mut parent = ShadowFrame::new(1);
        let a = dummy_ref();
        parent.slots[0] = a;
        let parent_ptr: *mut ShadowFrame = &mut *parent;

        let mut child = ShadowFrame::new(1);
        let b = dummy_ref();
        child.slots[0] = b;
        child.parent = parent_ptr;

        let mut out = Vec::new();
        child.push_roots(&mut out);
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|r| r.as_ptr() == a));
        assert!(out.iter().any(|r| r.as_ptr() == b));
    }

    #[test]
    #[should_panic(expected = "exceeds MAX_SHADOW_SLOTS")]
    fn rejects_oversized_frame() {
        let _ = ShadowFrame::new((MAX_SHADOW_SLOTS + 1) as u32);
    }
}
