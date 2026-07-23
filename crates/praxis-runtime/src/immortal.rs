//! Immortal singleton objects (§4.3, M3 deliverable).
//!
//! §4.3 notes that `Unit` and `Bool` "may be immortal singleton objects." M3
//! materializes this: [`Immortals`] pre-allocates `Unit`, `true`, and `false`
//! once at runtime startup. They are kept out of the sweepable `live` set, so
//! the collector never reclaims them — they live for the entire run.
//!
//! Immortals are the natural return value for runtime wrappers that must return
//! *a* `GcRef` after a fault (Appendix B: "return a valid sentinel object such
//! as `Unit`").

use crate::heap::Heap;
use crate::scalars::{BoolPayload, BOOL, UNIT};
use crate::GcRef;

/// The immortal singletons, pre-allocated at runtime start (§4.3).
#[repr(C)]
pub struct Immortals {
    unit: GcRef,
    true_: GcRef,
    false_: GcRef,
}

impl Immortals {
    /// Allocate the immortal singletons on `heap`. They are intentionally *not*
    /// registered in the heap's live set; the collector never reclaims them.
    pub fn new(heap: &Heap) -> Self {
        // Bypass `Heap::alloc`'s live-set registration: immortals are managed
        // out-of-band. We use the same low-level layout so the descriptors and
        // accessors still work on them.
        let unit = heap.alloc_immortal(UNIT, ());
        let true_ = heap.alloc_immortal(BOOL, 1_u8);
        let false_ = heap.alloc_immortal(BOOL, 0_u8);
        // Immortals start black so a mark phase that happens to visit them (e.g.
        // via a root that aliases them) does not transiently un-protect them.
        unit.header().set_mark_color(crate::gc::BLACK);
        true_.header().set_mark_color(crate::gc::BLACK);
        false_.header().set_mark_color(crate::gc::BLACK);
        Immortals {
            unit,
            true_,
            false_,
        }
    }

    /// The immortal `Unit` value.
    #[inline]
    pub fn unit(&self) -> GcRef {
        self.unit
    }

    /// The immortal `true` value.
    #[inline]
    pub fn true_(&self) -> GcRef {
        self.true_
    }

    /// The immortal `false` value.
    #[inline]
    pub fn false_(&self) -> GcRef {
        self.false_
    }

    /// The immortal `Bool` for a Rust `bool`.
    #[inline]
    pub fn bool_(&self, b: bool) -> GcRef {
        if b {
            self.true_
        } else {
            self.false_
        }
    }
}

/// Read a `Bool` payload, recognizing the immortal singletons.
///
/// # Safety
/// `r` must be a `GcRef` whose descriptor is `BOOL`.
pub(crate) unsafe fn read_bool(r: GcRef) -> bool {
    // SAFETY: caller guarantees `r` is a Bool.
    let v: BoolPayload = unsafe { *r.payload::<BoolPayload>() };
    v != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immortals_are_distinct_and_stable() {
        let heap = Heap::new();
        let im = Immortals::new(&heap);
        // Each accessor returns the same singleton address on every call (stability)
        // and the three singletons are mutually distinct.
        let unit = im.unit().as_ptr();
        let true_ = im.true_().as_ptr();
        let false_ = im.false_().as_ptr();
        assert_eq!(im.unit().as_ptr(), unit);
        assert_eq!(im.true_().as_ptr(), true_);
        assert_eq!(im.false_().as_ptr(), false_);
        assert_ne!(true_, false_);
        assert_ne!(unit, true_);
        assert_ne!(unit, false_);
        // `bool_` dispatches to the cached singletons rather than allocating.
        assert_eq!(im.bool_(true).as_ptr(), true_);
        assert_eq!(im.bool_(false).as_ptr(), false_);

        // Nothing is in the live set — immortals are out-of-band.
        assert_eq!(heap.stats().live_count, 0);
    }
}
