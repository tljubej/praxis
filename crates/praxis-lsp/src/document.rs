//! Open-document overlays and source revisions (§15.1).
//!
//! The server's view of a file is the client's buffer, not the disk: an editor
//! asks about text it has not saved. Each open URI carries its text, the
//! client's `version`, and an internal [`Revision`] that the query cache keys
//! on.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use lsp_types::{TextDocumentContentChangeEvent, Uri};
use praxis_source::LineMap;

use crate::position::{Encoding, PositionMap};

/// A monotonically increasing edit counter.
///
/// **Not the client's `version`.** A client is free to reuse or omit a version;
/// the query cache's correctness must not depend on it, because a stale cache
/// entry answers hover with the previous keystroke's types.
///
/// **Process-wide, not per-document.** A per-document counter starting at zero
/// makes a *re-open* look like the state the cache already holds — `didClose`
/// then `didOpen`, or an editor reloading a file from disk, produces revision 0
/// twice with different text, and the analyzer hands back the previous
/// analysis. One counter for the process makes that unrepresentable: no two
/// document states in one run ever share a revision.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Revision(pub u64);

impl Revision {
    /// The next revision. Never returns the same value twice in one process.
    #[must_use]
    pub fn next() -> Revision {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Revision(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// One open document.
pub struct Document {
    /// The current buffer text.
    text: String,
    /// The line table for `text`, rebuilt on every edit. A `LineMap` is a
    /// `Vec<u32>` of line starts plus the bytes; rebuilding it is cheaper than
    /// keeping an incremental one correct, at AoC file sizes.
    lines: LineMap,
    /// The server's own edit counter.
    revision: Revision,
    /// The client's `version`, echoed back on `publishDiagnostics` so an editor
    /// can discard a report for text it has already replaced.
    version: i32,
}

impl Document {
    #[must_use]
    pub fn new(text: String, version: i32) -> Document {
        let lines = LineMap::new(&text);
        Document {
            text,
            lines,
            revision: Revision::next(),
            version,
        }
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn lines(&self) -> &LineMap {
        &self.lines
    }

    #[must_use]
    pub fn revision(&self) -> Revision {
        self.revision
    }

    #[must_use]
    pub fn version(&self) -> i32 {
        self.version
    }

    /// The position map for this document's current text.
    #[must_use]
    pub fn positions(&self) -> PositionMap<'_> {
        PositionMap::new(&self.text, &self.lines)
    }

    /// Apply one `didChange` content change and bump the revision.
    ///
    /// A change with no range is a full replacement — what a client sends when
    /// it declined incremental sync, and what every client sends for `didOpen`.
    pub fn apply(&mut self, change: &TextDocumentContentChangeEvent, enc: Encoding) {
        match change.range {
            Some(range) => {
                let span = self.positions().span(range, enc);
                let (s, e) = (
                    (span.start().to_u32() as usize).min(self.text.len()),
                    (span.end().to_u32() as usize).min(self.text.len()),
                );
                self.text.replace_range(s..e, &change.text);
            }
            None => self.text.clone_from(&change.text),
        }
        self.lines = LineMap::new(&self.text);
        self.revision = Revision::next();
    }

    /// Record the client's new `version`. Separate from [`Document::apply`]
    /// because one `didChange` carries several changes and one version.
    pub fn set_version(&mut self, version: i32) {
        self.version = version;
    }
}

/// Every open document, keyed by URI.
#[derive(Default)]
pub struct DocumentStore {
    docs: HashMap<Uri, Document>,
}

impl DocumentStore {
    #[must_use]
    pub fn new() -> DocumentStore {
        DocumentStore::default()
    }

    pub fn open(&mut self, uri: Uri, text: String, version: i32) {
        self.docs.insert(uri, Document::new(text, version));
    }

    pub fn close(&mut self, uri: &Uri) {
        self.docs.remove(uri);
    }

    #[must_use]
    pub fn get(&self, uri: &Uri) -> Option<&Document> {
        self.docs.get(uri)
    }

    #[must_use]
    pub fn get_mut(&mut self, uri: &Uri) -> Option<&mut Document> {
        self.docs.get_mut(uri)
    }

    #[must_use]
    pub fn is_open(&self, uri: &Uri) -> bool {
        self.docs.contains_key(uri)
    }

    /// Every open URI, for the loop that republishes after a configuration
    /// change.
    pub fn uris(&self) -> impl Iterator<Item = &Uri> {
        self.docs.keys()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};
    use std::str::FromStr;

    fn uri() -> Uri {
        Uri::from_str("file:///day01.px").expect("a valid URI")
    }

    fn change(range: Option<Range>, text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range,
            range_length: None,
            text: text.to_string(),
        }
    }

    #[test]
    fn an_incremental_edit_splices_and_bumps_the_revision() {
        let mut store = DocumentStore::new();
        store.open(uri(), "let x = 1\n".to_string(), 1);
        let doc = store.get_mut(&uri()).expect("open");
        let before = doc.revision();

        // Replace the `1` with `42`.
        doc.apply(
            &change(
                Some(Range {
                    start: Position {
                        line: 0,
                        character: 8,
                    },
                    end: Position {
                        line: 0,
                        character: 9,
                    },
                }),
                "42",
            ),
            Encoding::Utf16,
        );
        assert_eq!(doc.text(), "let x = 42\n");
        assert!(doc.revision() > before, "an edit bumps the revision");
    }

    #[test]
    fn a_full_replacement_has_no_range() {
        let mut store = DocumentStore::new();
        store.open(uri(), "old".to_string(), 1);
        let doc = store.get_mut(&uri()).expect("open");
        doc.apply(&change(None, "new"), Encoding::Utf16);
        assert_eq!(doc.text(), "new");
    }

    /// An edit whose range is in the middle of a multi-byte line splices at the
    /// right byte, not at the code-unit count read as a byte.
    #[test]
    fn an_edit_after_a_multibyte_character_splices_correctly() {
        let mut store = DocumentStore::new();
        store.open(uri(), "let é = 1\n".to_string(), 1);
        let doc = store.get_mut(&uri()).expect("open");
        // `é` is one UTF-16 unit, so the `1` is at character 8.
        doc.apply(
            &change(
                Some(Range {
                    start: Position {
                        line: 0,
                        character: 8,
                    },
                    end: Position {
                        line: 0,
                        character: 9,
                    },
                }),
                "2",
            ),
            Encoding::Utf16,
        );
        assert_eq!(doc.text(), "let é = 2\n");
    }

    #[test]
    fn a_multi_line_edit_joins_lines() {
        let mut store = DocumentStore::new();
        store.open(uri(), "a\nb\nc\n".to_string(), 1);
        let doc = store.get_mut(&uri()).expect("open");
        doc.apply(
            &change(
                Some(Range {
                    start: Position {
                        line: 0,
                        character: 1,
                    },
                    end: Position {
                        line: 2,
                        character: 0,
                    },
                }),
                "",
            ),
            Encoding::Utf16,
        );
        assert_eq!(doc.text(), "ac\n");
        assert_eq!(doc.lines().line_count(), 2, "the line map was rebuilt");
    }

    /// Re-opening a URI is a **new** state, never the cached one. A per-document
    /// counter starting at zero made `didClose`/`didOpen` — and an editor
    /// reloading from disk — reuse the previous analysis.
    #[test]
    fn reopening_a_uri_is_a_new_revision() {
        let mut store = DocumentStore::new();
        store.open(uri(), "let x = 1\n".to_string(), 1);
        let first = store.get(&uri()).expect("open").revision();
        store.open(uri(), "let x = 2\n".to_string(), 1);
        let second = store.get(&uri()).expect("open").revision();
        assert_ne!(first, second);
    }

    #[test]
    fn closing_removes_the_document() {
        let mut store = DocumentStore::new();
        store.open(uri(), "x".to_string(), 1);
        assert!(store.is_open(&uri()));
        store.close(&uri());
        assert!(!store.is_open(&uri()));
    }
}
