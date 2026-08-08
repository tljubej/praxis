//! The deterministic half of ADR-114's evidence: **a native root scope makes no
//! call to the system allocator.**
//!
//! The rooting runtime wrappers sit on the path of `praxis_vec_push`,
//! `praxis_deque_push_back`, `praxis_bitset_insert`, `praxis_set_insert` and
//! `praxis_map_insert` — every mutating collection primitive in the language —
//! so a per-scope heap allocation there is paid on every one of those calls.
//! With one, `bfs` spent ~24% of its time inside `libsystem_malloc`.
//!
//! A wall-clock A/B can say the change is faster on this laptop this afternoon.
//! **This says the allocations are gone**, which is the claim, and it does not
//! drift: the count is exact, it is the same on every machine, and a future
//! change that puts a per-scope allocation back fails here rather than showing
//! up as two percent on a benchmark somebody re-runs next quarter.
//!
//! It is its own integration binary because a `#[global_allocator]` is
//! process-wide. The counter is armed only around the measured region so the
//! harness's own allocations — thread spawn, the test name, the panic machinery
//! — are not counted, and this file holds exactly one test so nothing else in
//! the binary can run concurrently with the armed window.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use praxis_runtime::{NativeScope, Runtime, NATIVE_ROOT_RESERVATION};

static ARMED: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static FREES: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every method forwards to `System`, which is a correct allocator; the
// counters are `Relaxed` atomics and have no bearing on the returned pointers.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ARMED.load(Ordering::Relaxed) {
            FREES.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Open a scope, root a reference, drop the scope — ten thousand times, which is
/// what a wrapper on the collection path does per call.
///
/// Under ADR-114 that is **zero allocations and zero frees**: the store was
/// reserved once at `Runtime::new`, the scope is a `usize` read, `root` is a
/// bounds-checked store into capacity that is already there, and `Drop` is a
/// `truncate` on a `Copy` element type.
#[test]
fn a_rooting_scope_does_not_call_the_allocator() {
    const ROUNDS: usize = 10_000;

    // Everything that allocates happens before the window: the runtime, its
    // heap, its reservations, and the one object being rooted.
    let mut rt = Box::new(Runtime::new());
    let mut ctx = Box::new(rt.context());
    let ctx_ptr: *mut praxis_runtime::RuntimeContext = &mut *ctx;
    // Out of the intern range on purpose, so this is a real block rather than a
    // load out of `small_ints` — the rooted reference should be an ordinary
    // heap object, not an immortal.
    // SAFETY: `ctx_ptr` is wired to `rt`, which outlives the call.
    let value = unsafe { praxis_runtime::abi::praxis_alloc_int(ctx_ptr, 1_000_000) };
    assert!(rt.native_root_store().is_empty());

    ARMED.store(true, Ordering::Relaxed);
    for _ in 0..ROUNDS {
        // SAFETY: `ctx` is wired to `rt`, which outlives every scope here.
        let scope = unsafe { NativeScope::new(ctx_ptr) };
        scope.root(value);
        scope.root(value);
        drop(scope);
    }
    ARMED.store(false, Ordering::Relaxed);

    let allocs = ALLOCS.load(Ordering::Relaxed);
    let frees = FREES.load(Ordering::Relaxed);
    assert_eq!(
        (allocs, frees),
        (0, 0),
        "{ROUNDS} scopes rooting two references each made {allocs} allocations \
         and {frees} frees; ADR-114's whole claim is that this is (0, 0), and \
         ADR-012's boxed frame made it ({}, {})",
        ROUNDS * 2,
        ROUNDS * 2
    );
    assert!(
        rt.native_root_store().is_empty(),
        "every scope was balanced"
    );
    assert_eq!(
        rt.native_root_store().capacity(),
        NATIVE_ROOT_RESERVATION,
        "two live roots at a time never approaches the reservation, so nothing \
         grew — the zero above is not a growth that happened to be free"
    );
}
