//! Bridge between Praxis spans and rowan's text offsets.
//!
//! Praxis `Span`/`BytePos` are the source of truth for diagnostics and never
//! leak rowan types outward. Internally, rowan indexes the tree with its own
//! `TextSize`/`TextRange` (also byte offsets into UTF-8). Neither type is local
//! to this crate, so the orphan rule forbids `From` impls between them; these
//! free functions are the only place the two meet (ADR-003).
//!
//! Prefer the explicit `span_to_range` / `range_to_span` calls at the tree
//! boundary over implicit conversions, so every crossing is visible.

use praxis_source::{BytePos, Span};
use rowan::{TextRange, TextSize};

/// Convert a Praxis [`Span`] into a rowan [`TextRange`].
///
/// `Span` is never inverted (start ≤ end by construction), so this cannot panic
/// on `TextRange::new`'s internal assertion.
#[inline]
#[must_use]
pub fn span_to_range(span: Span) -> TextRange {
    TextRange::new(
        TextSize::from(span.start().to_u32()),
        TextSize::from(span.end().to_u32()),
    )
}

/// Convert a rowan [`TextRange`] back into a Praxis [`Span`].
#[inline]
#[must_use]
pub fn range_to_span(range: TextRange) -> Span {
    Span::new(
        BytePos::from(u32::from(range.start())),
        BytePos::from(u32::from(range.end())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_round_trips_through_textrange() {
        for (start, end) in [(0, 0), (0, 3), (5, 5), (10, 42)] {
            let span = Span::new(BytePos::from(start), BytePos::from(end));
            let range = span_to_range(span);
            let back = range_to_span(range);
            assert_eq!(back, span);
        }
    }

    #[test]
    fn empty_span_maps_to_empty_range() {
        let span = Span::new(BytePos::from(7), BytePos::from(7));
        let range = span_to_range(span);
        assert!(range.is_empty());
    }
}
