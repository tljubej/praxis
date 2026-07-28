//! Crash snapshots: a stable, deep-copied view of the debug-frame chain taken
//! at the moment a fault begins to unwind (§9.3, M10-WS3).
//!
//! When a fault fires, each generated function's fault epilogue pops its shadow
//! and debug frames as it returns to its caller. By the time control reaches the
//! host, **every** language frame has unwound and `ctx.debug_top` is null again.
//! To give the host (the noninteractive fallback, the crash REPL) something to
//! inspect, the **first** fault epilogue — the innermost frame's, which runs
//! while the full chain is still intact — deep-copies the chain into a
//! [`CrashSnapshot`] owned by the [`Runtime`]. The copy is stable because the
//! collector is precise and non-moving (ADR-011): a `GcRef` copied into a
//! snapshot keeps pointing at the same object.
//!
//! GC rooting (the §19.10 acceptance criterion "GC retains all objects
//! reachable from snapshots"): [`CrashSnapshot`] implements [`RootSet`],
//! yielding every copied `DebugLocal.value`. Transitive reachability is the
//! collector's job; the snapshot just pins the entry points. The host registers
//! the snapshot as a root when collecting during the REPL/noninteractive render.
//!
//! Idempotency: a snapshot is taken at most once per fault. The
//! [`SnapshotSlot`] guards with a `taken` flag; subsequent fault epilogues (the
//! outer frames unwinding after the innermost already snapshotted) are no-ops.
//! The slot is cleared at the start of each program run.

use std::ptr::NonNull;

use crate::context::{DebugFrame, DebugLocal};
use crate::gc::GcRef;
use crate::roots::RootSet;

/// A deep copy of one debug frame + its locals, stable across the fault unwind.
/// The `value` GcRefs point at the same non-moving objects the live frame did.
#[derive(Debug)]
pub struct SnapshotFrame {
    /// The caller's snapshot frame, chained to mirror the live `parent` chain.
    pub parent: usize,
    /// The function name (copied from the live frame's `'static` name pointer;
    /// safe to keep as a raw pointer since the compiler embedded it `'static`).
    pub func_name: *const u8,
    pub func_name_len: u32,
    /// The copied locals. `value` fields are the GC roots.
    pub locals: Vec<DebugLocal>,
    /// The function's source span `[start, end)` byte offsets (§9.3). Copied
    /// from the live frame; `(0, 0)` means "unknown" (M10b-WS1 fills it from
    /// the AST; the `source` REPL command renders it).
    pub source_span: (u32, u32),
}

/// A crash snapshot: the deep-copied frame chain + the fault kind that triggered
/// it. Owned by the [`Runtime`] via [`SnapshotSlot`]; implements [`RootSet`] so
/// the collector retains every snapshot-reachable object.
#[derive(Debug)]
pub struct CrashSnapshot {
    /// The copied frames, in innermost-first order (frame 0 is the faulting
    /// function; the last is `main`).
    pub frames: Vec<SnapshotFrame>,
    /// The fault kind that triggered the snapshot (§9.1). Set when taken.
    pub fault_kind: crate::FaultKind,
}

impl Default for CrashSnapshot {
    fn default() -> Self {
        CrashSnapshot {
            frames: Vec::new(),
            fault_kind: crate::FaultKind::None,
        }
    }
}

impl CrashSnapshot {
    /// A fresh, empty snapshot.
    pub fn new() -> Self {
        CrashSnapshot::default()
    }

    /// True iff no frames were captured.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// The number of frames in the chain (0 if not taken).
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// The function name of frame `i` as a `&str`, or `<unknown>`.
    ///
    /// # Safety
    /// The frame's `func_name` must be valid UTF-8 for `func_name_len` bytes
    /// (the compiler guarantees this for embedded names).
    pub unsafe fn frame_name(&self, i: usize) -> &str {
        let f = &self.frames[i];
        if f.func_name.is_null() || f.func_name_len == 0 {
            return "<unknown>";
        }
        // SAFETY: caller upholds the UTF-8/len contract (compiler-embedded names).
        unsafe {
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                f.func_name,
                f.func_name_len as usize,
            ))
        }
    }
}

impl RootSet for CrashSnapshot {
    fn push_roots(&self, out: &mut Vec<GcRef>) {
        // Walk every copied local's value; null sentinels are skipped (they are
        // the dangling NonNull used for not-yet-written slots, never real refs).
        for frame in &self.frames {
            for local in &frame.locals {
                if is_real_ref(local.value) {
                    out.push(local.value);
                }
            }
        }
    }
}

/// A runtime-owned slot holding at most one [`CrashSnapshot`], with a `taken`
/// guard for idempotent snapshotting across the multi-frame fault unwind.
///
/// Lives on [`crate::Runtime`] (address-stable); `clear` resets it before each
/// program run so a stale snapshot does not leak into the next.
#[derive(Debug, Default)]
pub struct SnapshotSlot {
    snapshot: Option<CrashSnapshot>,
}

impl SnapshotSlot {
    /// A fresh, empty slot.
    pub fn new() -> Self {
        SnapshotSlot::default()
    }

    /// Clear any held snapshot (call before each program run).
    pub fn clear(&mut self) {
        self.snapshot = None;
    }

    /// Borrow the held snapshot, if any.
    #[must_use]
    pub fn get(&self) -> Option<&CrashSnapshot> {
        self.snapshot.as_ref()
    }

    /// Take the held snapshot out of the slot (the host owns it after).
    pub fn take(&mut self) -> Option<CrashSnapshot> {
        self.snapshot.take()
    }

    /// True iff a snapshot is currently held.
    pub fn is_set(&self) -> bool {
        self.snapshot.is_some()
    }
}

/// Deep-copy the live debug-frame chain at `ctx.debug_top` into a fresh
/// [`CrashSnapshot`], recording `fault_kind`, and store it in the runtime's
/// [`SnapshotSlot`] — **but only if no snapshot has been taken yet this run**
/// (idempotency: the innermost fault epilogue runs first, while the chain is
/// intact; outer frames unwinding later are no-ops).
///
/// Called from generated fault epilogues (and the stack-overflow epilogue)
/// *before* the debug-frame pop. If `ctx.debug_top` is null (no debug frames
/// were pushed, e.g. a host-side fault path), this is a no-op.
///
/// # Safety
/// `ctx` must be live and wired; `debug_top`, if non-null, must point at a
/// valid `DebugFrame` whose parent chain is intact.
#[no_mangle]
pub unsafe extern "C" fn praxis_snapshot_debug_chain(ctx: *mut crate::RuntimeContext) {
    if ctx.is_null() {
        return;
    }
    let slot_ptr = unsafe { (*ctx).crash_snapshot };
    if slot_ptr.is_null() {
        return;
    }
    // Idempotency: if a snapshot already exists this run, do nothing. The first
    // (innermost) fault epilogue captures the intact chain; later frames skip.
    // SAFETY: slot_ptr points at a live SnapshotSlot owned by the Runtime.
    if unsafe { (*slot_ptr).is_set() } {
        return;
    }
    let top = unsafe { (*ctx).debug_top };
    if top.is_null() {
        return;
    }
    // SAFETY: debug_top is null or a valid DebugFrame with an intact parent chain.
    let snapshot = unsafe { copy_chain(top) };
    let kind = unsafe { crate::context::current_fault_kind(ctx) };
    let mut s = CrashSnapshot::new();
    s.fault_kind = kind;
    s.frames = snapshot;
    unsafe { (*slot_ptr).snapshot = Some(s) };
}

/// Deep-copy the frame chain starting at `top` into a `Vec<SnapshotFrame>`,
/// innermost-first. The `parent` index of each frame points at the next entry
/// in the vec (so frame 0's parent is frame 1, etc.); the outermost frame's
/// parent is `usize::MAX` (sentinel for "no parent").
///
/// # Safety
/// `top` must be a valid `DebugFrame`; its parent chain must be intact until
/// the outermost (null-parent) frame.
unsafe fn copy_chain(top: *mut DebugFrame) -> Vec<SnapshotFrame> {
    let mut out = Vec::new();
    let mut cur = top;
    while !cur.is_null() {
        // SAFETY: cur is null or a valid DebugFrame in the live chain.
        let frame = unsafe { &*cur };
        // Copy the locals slice. The DebugLocal fields are plain data except
        // `value` (a GcRef) and `descriptor`/`source_name` (raw pointers to
        // 'static data); a shallow Vec copy is correct and keeps the GcRefs live.
        let locals: Vec<DebugLocal> = if frame.locals.is_null() || frame.local_count == 0 {
            Vec::new()
        } else {
            // SAFETY: locals was allocated with local_count entries.
            let slice =
                unsafe { std::slice::from_raw_parts(frame.locals, frame.local_count as usize) };
            slice.to_vec()
        };
        out.push(SnapshotFrame {
            // parent index is filled in the second pass below.
            parent: usize::MAX,
            func_name: frame.func_name,
            func_name_len: frame.func_name_len,
            source_span: frame.source_span,
            locals,
        });
        cur = frame.parent;
    }
    // Fill in parent indices: frame i's parent is i+1 (the caller), except the
    // outermost frame whose parent stays usize::MAX.
    for i in 0..out.len().saturating_sub(1) {
        out[i].parent = i + 1;
    }
    out
}

/// True iff `r` is a real GC reference (not the null sentinel used for
/// not-yet-written debug-local slots). The sentinel is `NonNull::dangling()`
/// (alignment of `GcHeader`); any real allocation has a distinct address. We
/// compare against the dangling marker the spill/debug-frame helpers use.
fn is_real_ref(r: GcRef) -> bool {
    let dangling = NonNull::<crate::gc::GcHeader>::dangling();
    !std::ptr::eq(r.as_ptr(), dangling.as_ptr())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scalars::INT;
    use crate::{Runtime, LOCAL_KIND_USER};

    #[test]
    fn empty_snapshot_roots_nothing() {
        let s = CrashSnapshot::new();
        let mut out = Vec::new();
        s.push_roots(&mut out);
        assert!(out.is_empty());
        assert!(s.is_empty());
    }

    #[test]
    fn snapshot_slot_clear_resets() {
        let mut slot = SnapshotSlot::new();
        assert!(!slot.is_set());
        slot.snapshot = Some(CrashSnapshot::new());
        assert!(slot.is_set());
        slot.clear();
        assert!(!slot.is_set());
    }

    #[test]
    fn explicit_collection_preserves_values_held_by_a_crash_snapshot() {
        let runtime = Runtime::new();
        let value = runtime.heap().alloc(INT, 42_i64);
        let snapshot = CrashSnapshot {
            frames: vec![SnapshotFrame {
                parent: usize::MAX,
                func_name: std::ptr::null(),
                func_name_len: 0,
                locals: vec![DebugLocal {
                    source_name: std::ptr::null(),
                    name_len: 0,
                    symbol_id: 0,
                    descriptor: INT as *const _,
                    value,
                    type_id: 0,
                    kind: LOCAL_KIND_USER,
                    span_start: 0,
                    span_end: 0,
                }],
                source_span: (0, 0),
            }],
            fault_kind: crate::FaultKind::None,
        };

        runtime.collect(&snapshot);

        assert_eq!(runtime.heap().stats().live_count, 1);
        assert_eq!(value.as_int(), 42);
    }
}
