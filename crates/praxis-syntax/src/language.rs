//! The rowan `Language` glue that gives Praxis a strongly-typed lossless tree.
//!
//! Praxis owns [`SyntaxKind`](crate::SyntaxKind); everything else here is the
//! thin adapter that lets `rowan`'s generic node/token/element types carry it
//! (ADR-003). Downstream crates (`praxis-parser`, `praxis-ast`, the LSP) refer
//! to the tree through the [`SyntaxNode`] / [`SyntaxToken`] / [`SyntaxElement`]
//! aliases so they stay free of generic parameters.

use rowan::{Language, SyntaxKind as RawSyntaxKind};

use crate::SyntaxKind;

/// The Praxis language tag carried by every node in the lossless tree.
///
/// It is a zero-sized marker; its only job is to bind [`SyntaxKind`] to rowan's
/// raw `u16` storage so that `SyntaxNode` is `SyntaxNode<PraxisLanguage>`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PraxisLanguage;

impl Language for PraxisLanguage {
    type Kind = SyntaxKind;

    #[inline]
    fn kind_from_raw(raw: RawSyntaxKind) -> Self::Kind {
        // This is a *safe* function, so it may be called with any `u16`
        // whatever the provenance of the value. `from_raw_u16` is total: out of
        // range yields `ERROR` rather than an invalid discriminant.
        SyntaxKind::from_raw_u16(raw.0)
    }

    #[inline]
    fn kind_to_raw(kind: Self::Kind) -> RawSyntaxKind {
        RawSyntaxKind(kind as u16)
    }
}

/// A Praxis syntax node in the lossless tree.
pub type SyntaxNode = rowan::SyntaxNode<PraxisLanguage>;
/// A Praxis syntax token (a leaf) in the lossless tree.
pub type SyntaxToken = rowan::SyntaxToken<PraxisLanguage>;
/// Either a node or a token, as walked out of the tree.
pub type SyntaxElement = rowan::SyntaxElement<PraxisLanguage>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_through_raw() {
        // Every discriminant must survive a to_raw/from_raw cycle, since rowan
        // stores kinds as raw u16 in the green tree.
        for kind in [
            SyntaxKind::Whitespace,
            SyntaxKind::Ident,
            SyntaxKind::KW_IF,
            SyntaxKind::PLUS,
            SyntaxKind::EOF,
            SyntaxKind::ERROR,
            SyntaxKind::SOURCE_FILE,
            SyntaxKind::PARSE_ERROR,
        ] {
            let raw = PraxisLanguage::kind_to_raw(kind);
            assert_eq!(PraxisLanguage::kind_from_raw(raw), kind);
        }
    }

    /// Every raw value the boundary accepts must map to the kind with that
    /// discriminant. This is what makes the range check in `from_raw_u16`
    /// sufficient: it proves the discriminants really are consecutive.
    #[test]
    fn every_raw_value_in_range_round_trips() {
        for raw in 0..=SyntaxKind::PARSER_NAMED_ARG as u16 {
            let kind = PraxisLanguage::kind_from_raw(RawSyntaxKind(raw));
            assert_eq!(
                PraxisLanguage::kind_to_raw(kind).0,
                raw,
                "raw {raw} did not round-trip"
            );
        }
    }

    #[test]
    fn repr_is_u16() {
        // rowan stores a u16; the repr contract must hold for all time.
        assert_eq!(
            std::mem::size_of::<SyntaxKind>(),
            std::mem::size_of::<u16>()
        );
    }

    /// The safe `Language` boundary is reachable with any `u16`, so it must be
    /// total. This runs in the ordinary suite, not only under Miri: a checked
    /// conversion is observable without needing UB detection.
    #[test]
    fn out_of_range_raw_kind_maps_to_a_safe_error_kind() {
        assert_eq!(
            PraxisLanguage::kind_from_raw(RawSyntaxKind(u16::MAX)),
            SyntaxKind::ERROR,
            "the safe rowan Language boundary must not construct an invalid enum discriminant"
        );
    }
}
