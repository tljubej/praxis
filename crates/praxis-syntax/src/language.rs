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
        // SAFETY: `SyntaxKind` is `#[repr(u16)]`, so every `u16` is a valid
        // discriminant. The only producer of raw kinds is `kind_to_raw` below
        // (plus rowan's own storage, which round-trips our values). Unknown
        // values cannot occur from Praxis-owned input.
        debug_assert!(raw.0 <= SyntaxKind::PARSE_ERROR as u16);
        // The cast is sound because of `repr(u16)` and the provenance of `raw`.
        unsafe { std::mem::transmute(raw.0) }
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

    #[test]
    fn repr_is_u16() {
        // rowan stores a u16; the repr contract must hold for all time.
        assert_eq!(
            std::mem::size_of::<SyntaxKind>(),
            std::mem::size_of::<u16>()
        );
    }
}
