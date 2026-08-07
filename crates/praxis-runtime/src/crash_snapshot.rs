//! Crash snapshots: a stable, deep-copied view of the debug frames taken at the
//! moment a fault begins to unwind (§9.3, M10-WS3).
//!
//! When a fault fires, each generated function's fault epilogue restores the
//! shadow and debug stack tops it saved as it returns to its caller. By the time
//! control reaches the host, **every** language frame has unwound and both debug
//! stacks are empty again. To give the host (the noninteractive fallback, the
//! crash REPL) something to inspect, the **first** fault epilogue — the
//! innermost frame's, which runs while the whole stack is still claimed —
//! deep-copies it into a [`CrashSnapshot`] owned by the [`Runtime`]. The copy is
//! stable because the collector is precise and non-moving (ADR-011): a `GcRef`
//! copied into a snapshot keeps pointing at the same object.
//!
//! Copying eagerly at the first fault epilogue is ADR-033 decision 1, and it
//! survives ADR-104 unchanged even though a stack — unlike the `Box`ed chain it
//! replaced — does not *destroy* the words a pop releases. Reading them lazily
//! from the host would be possible and is rejected: values above `top` are in no
//! arm of [`crate::roots::RuntimeRoots`], so a collection between the unwind and
//! the read could free what they name.
//!
//! ADR-106 sharpens that into the reason the eager copy is *sound* rather than
//! merely conventional. Values **below** `top` are the weak arm: every
//! collection clears the debug slots whose objects it reclaimed, so at the
//! moment [`copy_stack`] reads a slot, that slot holds a live object or `None`.
//! Values **above** `top` are still in no arm at all — nothing scans them,
//! nothing clears them, and a popped frame's words are exactly as stale as they
//! always were. The eager copy is what keeps the snapshot on the first side of
//! that line, and the weak arm is what makes the first side mean something.
//!
//! GC rooting (the §19.10 acceptance criterion "GC retains all objects
//! reachable from snapshots"): [`CrashSnapshot`] implements [`RootSet`],
//! yielding every copied `DebugLocal.value` that is a
//! [`DebugValue::Reference`](crate::debug::DebugValue) — a slot holding an
//! elided box's scalar payload (ADR-120 part 2) names no object and roots
//! nothing, and `DebugValue::reference` is where it drops out. Transitive
//! reachability is the
//! collector's job; the snapshot just pins the entry points. The host registers
//! the snapshot as a root when collecting during the REPL/noninteractive render.
//!
//! Idempotency: a snapshot is taken at most once per fault. The
//! [`SnapshotSlot`] guards with a `taken` flag; subsequent fault epilogues (the
//! outer frames unwinding after the innermost already snapshotted) are no-ops.
//! The slot is cleared at the start of each program run.

use crate::abi::abi_guard;
use crate::context::DebugLocal;
use crate::debug::DebugFrameEntry;
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
        // Walk every copied local's value. A slot no value was ever spilled
        // into is `None` and roots nothing — an absence the type carries, not a
        // sentinel pointer to be compared against (F18). A slot holding an
        // elided box's scalar payload (ADR-120 part 2) roots nothing either,
        // and `reference()` is where it drops out: a snapshot *is* a strong
        // root set, so a scalar reaching this line would be a payload traced as
        // an object.
        for frame in &self.frames {
            out.extend(
                frame
                    .locals
                    .iter()
                    .filter_map(|l| l.value.and_then(crate::debug::DebugValue::reference)),
            );
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

/// Deep-copy the claimed debug frames into a fresh [`CrashSnapshot`], recording
/// the pending fault kind, and store it in the runtime's [`SnapshotSlot`] —
/// **but only if no snapshot has been taken yet this run** (idempotency: the
/// innermost fault epilogue runs first, while every frame is still claimed;
/// outer frames unwinding later are no-ops).
///
/// Called from generated fault epilogues (and the stack-overflow epilogue)
/// *before* the debug-stack pops. If the stack is empty (no debug frames were
/// pushed, e.g. a host-side fault path), this is a no-op.
///
/// # Safety
/// `ctx` must be live and wired.
#[no_mangle]
pub unsafe extern "C" fn praxis_snapshot_debug_chain(ctx: *mut crate::RuntimeContext) {
    abi_guard!("praxis_snapshot_debug_chain", ctx, {
        if ctx.is_null() {
            return;
        }
        let slot_ptr = unsafe { (*ctx).crash_snapshot };
        if slot_ptr.is_null() {
            return;
        }
        // Idempotency: if a snapshot already exists this run, do nothing. The first
        // (innermost) fault epilogue captures the whole stack; later frames skip.
        // SAFETY: slot_ptr points at a live SnapshotSlot owned by the Runtime.
        if unsafe { (*slot_ptr).is_set() } {
            return;
        }
        let frames = unsafe { (*ctx).debug_frames };
        if frames.is_null() {
            return;
        }
        // SAFETY: a non-null `debug_frames` is the header of a live
        // `DebugFrameStack` owned by the runtime that wired this context.
        let entries = unsafe { (*frames).claimed() };
        if entries.is_empty() {
            return;
        }
        // SAFETY: every claimed entry was written by a prologue with a
        // `'static` meta and the base of its own run of value slots, and no
        // epilogue has run yet (this is called before the pops).
        let snapshot = unsafe { copy_stack(entries) };
        let kind = unsafe { crate::context::current_fault_kind(ctx) };
        let mut s = CrashSnapshot::new();
        s.fault_kind = kind;
        s.frames = snapshot;
        unsafe { (*slot_ptr).snapshot = Some(s) };
    })
}

/// Deep-copy the claimed frame entries into a `Vec<SnapshotFrame>`,
/// innermost-first. The `parent` index of each frame points at the next entry
/// in the vec (so frame 0's parent is frame 1, etc.); the outermost frame's
/// parent is `usize::MAX` (sentinel for "no parent").
///
/// `entries` is in push order — outermost first — so this walks it in reverse.
/// That reversal is what ADR-021's `parent` pointer used to buy, and the stack's
/// order is a stronger statement of the same thing: the frames *are* the run, so
/// a chain cannot be truncated or looped by a bad pointer.
///
/// Reassembling a [`DebugLocal`] here is what keeps `SnapshotFrame`,
/// `CrashSnapshot` and every consumer in `praxis-debugger` unchanged across
/// ADR-104: the static half of each local comes from the function's
/// [`crate::debug::FunctionDebugMeta`] and the value from the call's own slot,
/// where they used to be pre-joined in a heap frame the prologue built.
///
/// # Safety
/// Every entry's `meta` must point at a live `FunctionDebugMeta` whose `locals`
/// array has `local_count` entries, and its `values` at that many value slots.
unsafe fn copy_stack(entries: &[DebugFrameEntry]) -> Vec<SnapshotFrame> {
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries.iter().rev() {
        // SAFETY: the caller guarantees `meta` is live; a prologue writes it
        // before anything that could fault, so null is unreachable here.
        let Some(meta) = (unsafe { entry.meta.as_ref() }) else {
            continue;
        };
        let count = meta.local_count as usize;
        let locals: Vec<DebugLocal> = if count == 0 {
            Vec::new()
        } else {
            // SAFETY: the caller guarantees both arrays hold `count` entries.
            let metas = unsafe { std::slice::from_raw_parts(meta.locals, count) };
            let values = unsafe { std::slice::from_raw_parts(entry.values, count) };
            metas
                .iter()
                .zip(values)
                .map(|(m, &word)| DebugLocal {
                    source_name: m.source_name,
                    name_len: m.name_len,
                    symbol_id: m.symbol_id,
                    descriptor: m.descriptor,
                    // The zip *is* `read`'s precondition: slot `i` is decoded
                    // under local `i`'s own `slot_kind` and no other's. A temp
                    // whose box ADR-120 elided becomes a `DebugValue::Scalar`
                    // here and is therefore not in `push_roots`' root set below
                    // — correctly, since there is nothing to keep alive.
                    // SAFETY: as above; the two arrays are index-parallel.
                    value: unsafe { m.read(word) },
                    type_id: m.type_id,
                    kind: m.kind,
                    span_start: m.span_start,
                    span_end: m.span_end,
                })
                .collect()
        };
        out.push(SnapshotFrame {
            // parent index is filled in the second pass below.
            parent: usize::MAX,
            func_name: meta.func_name,
            func_name_len: meta.func_name_len,
            source_span: (meta.span_start, meta.span_end),
            locals,
        });
    }
    // Fill in parent indices: frame i's parent is i+1 (the caller), except the
    // outermost frame whose parent stays usize::MAX.
    for i in 0..out.len().saturating_sub(1) {
        out[i].parent = i + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scalars::{INT, INT_PAYLOAD};
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
        let value = runtime.heap().alloc_unpaced(INT_PAYLOAD, 42_i64);
        let snapshot = CrashSnapshot {
            frames: vec![SnapshotFrame {
                parent: usize::MAX,
                func_name: std::ptr::null(),
                func_name_len: 0,
                locals: vec![DebugLocal {
                    source_name: std::ptr::null(),
                    name_len: 0,
                    symbol_id: 0,
                    descriptor: &INT as *const _,
                    value: Some(crate::debug::DebugValue::Reference(value)),
                    type_id: 0,
                    kind: LOCAL_KIND_USER,
                    span_start: 0,
                    span_end: 0,
                }],
                source_span: (0, 0),
            }],
            fault_kind: crate::FaultKind::None,
        };

        runtime.collect_with(&snapshot);

        assert_eq!(runtime.heap().stats().live_count, 1);
        assert_eq!(value.as_int(), 42);
    }

    /// A snapshot taken *out* of the runtime outlives it in both hosts: the CLI
    /// moves the runtime into the `DebugSession` the `Repl` owns alongside the
    /// snapshot, and the debugger REPL replaces its snapshot after a restart.
    /// Now that `Heap` finalizes live payloads on `Drop` (RT-02), that ordering
    /// is load-bearing — so the property this pins is that dropping the
    /// snapshot after the runtime is *itself* harmless: a `CrashSnapshot` holds
    /// `GcRef`s but has no `Drop` that dereferences one (hazard H8).
    #[test]
    fn a_snapshot_may_be_dropped_after_the_runtime_it_names() {
        let snapshot = {
            let runtime = Runtime::new();
            let value = runtime.heap().alloc_unpaced(INT_PAYLOAD, 42_i64);
            CrashSnapshot {
                frames: vec![SnapshotFrame {
                    parent: usize::MAX,
                    func_name: std::ptr::null(),
                    func_name_len: 0,
                    locals: vec![DebugLocal {
                        source_name: std::ptr::null(),
                        name_len: 0,
                        symbol_id: 0,
                        descriptor: &INT as *const _,
                        value: Some(crate::debug::DebugValue::Reference(value)),
                        type_id: 0,
                        kind: LOCAL_KIND_USER,
                        span_start: 0,
                        span_end: 0,
                    }],
                    source_span: (0, 0),
                }],
                fault_kind: crate::FaultKind::None,
            }
            // `runtime` — and its heap, which finalizes `value` — dies here.
        };
        // The frames are still readable as plain data; only the objects they
        // *name* are gone. Reading `value.as_int()` here would be the
        // use-after-free the audit warns about, and is deliberately not done.
        assert_eq!(snapshot.len(), 1);
        drop(snapshot);
    }

    /// The defect ADR-106 closes, end to end at the level the snapshot is taken.
    ///
    /// The setup is what every Praxis function produces at a local's last use:
    /// `RootSlots::dead` nulls the shadow slot (MIR-01) and the debug slot keeps
    /// the value (MIR-16), so between the two the debugger names an object no
    /// arm of the root set reaches. A collection in that window reclaims it and
    /// the *next* allocation of the same layout takes the block back — here as a
    /// `Float`, since `Float`'s payload has `Int`'s size and alignment, so it
    /// lands on the same rung of the ladder.
    ///
    /// **The reissue is the whole point.** Without the weak arm the snapshot
    /// copies a `GcRef` that is now a live `Float` under a local whose static
    /// descriptor says `Int`, and `impl RootSet for CrashSnapshot` then roots
    /// it — a strong root, of the wrong type, into a `CrashSnapshot`. And no
    /// filter applied *here* could have caught it: at this point the block is a
    /// perfectly ordinary live object, indistinguishable from one the local
    /// legitimately named. That is why the clear happens inside the collection.
    #[test]
    fn a_reissued_block_is_not_rendered_under_the_dead_locals_name() {
        use crate::scalars::FLOAT_PAYLOAD;

        let mut rt = Runtime::new();
        // Past the interned small-`Int` range: an interned `Int` is an immortal
        // no sweep touches, and this test needs a real allocation to die.
        let dead = rt.heap().alloc_unpaced(INT_PAYLOAD, 9_999_i64);
        let address = dead.as_ptr();
        let mut ctx = Box::new(rt.context());

        let name = b"xs";
        let locals = [crate::DebugLocalMeta {
            source_name: name.as_ptr(),
            name_len: 2,
            symbol_id: 1,
            descriptor: &INT,
            type_id: 1,
            kind: LOCAL_KIND_USER,
            span_start: 0,
            span_end: 0,
            slot_kind: crate::debug::DebugSlotKind::Reference,
        }];
        let meta = crate::FunctionDebugMeta {
            func_name: b"main".as_ptr(),
            func_name_len: 4,
            local_count: 1,
            locals: locals.as_ptr(),
            span_start: 0,
            span_end: 0,
        };
        // SAFETY: `ctx` is wired to `rt`, and `meta`/`locals` outlive the guard.
        let mut guard = unsafe { crate::debug::push_frame(&mut *ctx, &meta) };
        guard.set(0, dead);

        rt.collect_now();

        let reissued = rt.heap().alloc_unpaced(FLOAT_PAYLOAD, 2.5_f64);
        assert_eq!(
            reissued.as_ptr(),
            address,
            "this test only says anything if the dead local's block came back"
        );

        // SAFETY: `ctx` is live and wired; the frame is still claimed, which is
        // the state a fault epilogue snapshots in.
        unsafe { praxis_snapshot_debug_chain(&mut *ctx) };
        drop(guard);

        let snapshot = rt.take_crash_snapshot().expect("a frame was claimed");
        assert_eq!(snapshot.len(), 1);
        let local = &snapshot.frames[0].locals[0];
        assert_eq!(
            local.value,
            Some(crate::debug::DebugValue::Reclaimed),
            "the snapshot copied the reissued block under `xs`, whose static \
             descriptor is Int — a `Float` rendered as an `Int`, and a strong \
             root to it out of `CrashSnapshot::push_roots`"
        );
        // And it is the *collected* absence, not the unwritten one. `guard.set`
        // above wrote this slot; a snapshot that reported `None` here would be
        // saying the store never happened.
        assert_ne!(local.value, None, "a written slot never reads as unwritten");

        let mut out = Vec::new();
        snapshot.push_roots(&mut out);
        assert!(
            out.is_empty(),
            "an absence must root nothing; a dangling entry would have made \
             the snapshot a strong root set for storage it does not own"
        );
        assert_eq!(reissued.descriptor().name, "Float");
    }
}
