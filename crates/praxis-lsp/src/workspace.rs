//! Workspace symbols (§15.2, §19.12), and the `file:` URI ⇄ path conversion
//! they need.
//!
//! `workspace/symbol` is the one query that cannot be answered from the file
//! the cursor is in, so this module is where "the workspace" becomes a thing
//! the server has at all.
//!
//! # The index is the walk
//!
//! There is no persistent index, and that is a measurement rather than a
//! shortcut: an AoC workspace is tens of small files, parsing one is under a
//! millisecond, and `workspace/symbol` fires when a person opens a picker and
//! types. A cache would have to be invalidated by file-system events the server
//! would then have to be right about — for a query that runs when a human asks
//! and waits.
//!
//! Two rules the walk keeps:
//!
//! - **An open document's text wins over the file on disk.** The editor's buffer
//!   is what the user is looking at, and a symbol picker that offers a name the
//!   user just deleted is worse than one that is a keystroke behind.
//! - **The walk is bounded.** [`MAX_FILES`] and [`MAX_DEPTH`] stop a root that
//!   was opened on a home directory from turning a picker into a disk scan, and
//!   the caps are stated rather than assumed — see [`Walk::truncated`].

use std::path::{Path, PathBuf};

use lsp_types::{Location, OneOf, SymbolKind, Uri, WorkspaceSymbol};

use crate::Revision;
use crate::document::DocumentStore;
use crate::navigation::document_symbols;
use crate::position::Encoding;
use crate::query::Snapshot;

/// How many `.px` files one query will read. Chosen to be far above any real
/// puzzle workspace and far below "the user opened their home directory".
pub const MAX_FILES: usize = 2_000;

/// How deep the walk descends from a root.
pub const MAX_DEPTH: usize = 16;

/// Directory names the walk never enters: build output, and anything hidden.
/// `target/` alone can hold more files than the rest of a workspace put
/// together, and none of them are source.
fn is_skipped_dir(name: &str) -> bool {
    name == "target" || name == "node_modules" || name.starts_with('.')
}

/// The result of one walk: the files it found, and whether it stopped early.
#[derive(Debug, Default)]
pub struct Walk {
    pub files: Vec<PathBuf>,
    /// `true` when a cap was hit, so a caller can say the answer is partial
    /// rather than presenting it as the whole workspace.
    pub truncated: bool,
}

/// Every `.px` file under `roots`, in a deterministic order.
///
/// Sorted, because an editor showing symbols in directory-iteration order shows
/// them in a different order on every machine — and a test asserting a symbol is
/// present would pass or fail on the file system's mood.
#[must_use]
pub fn walk(roots: &[PathBuf]) -> Walk {
    let mut out = Walk::default();
    for root in roots {
        descend(root, 0, &mut out);
    }
    out.files.sort();
    out.files.dedup();
    out
}

fn descend(dir: &Path, depth: usize, out: &mut Walk) {
    if depth > MAX_DEPTH || out.files.len() >= MAX_FILES {
        out.truncated = true;
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    children.sort();
    for path in children {
        if out.files.len() >= MAX_FILES {
            out.truncated = true;
            return;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if !is_skipped_dir(name) {
                descend(&path, depth + 1, out);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("px") {
            out.files.push(path);
        }
    }
}

/// `workspace/symbol`: every top-level declaration in the workspace whose name
/// matches `query`.
///
/// The match is a case-insensitive substring, which is what an empty query
/// makes total — the client sends `""` when the picker opens and expects
/// everything.
///
/// The symbols themselves come from [`document_symbols`], the same function
/// `textDocument/documentSymbol` answers with, so a name the outline shows and
/// the picker does not is unrepresentable.
#[must_use]
pub fn symbols(
    query: &str,
    roots: &[PathBuf],
    docs: &DocumentStore,
    enc: Encoding,
) -> Vec<WorkspaceSymbol> {
    let needle = query.to_lowercase();
    let mut out = Vec::new();

    for path in walk(roots).files {
        let Some(uri) = path_to_uri(&path) else {
            continue;
        };
        // The open buffer, or the bytes on disk.
        let text = match docs.get(&uri) {
            Some(doc) => doc.text().to_string(),
            None => match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(_) => continue,
            },
        };
        let snapshot = Snapshot::new(&path.to_string_lossy(), text, Revision(0));
        collect(&snapshot, &uri, enc, &needle, &mut out);
    }
    out
}

/// The open documents' symbols alone, for a server with no folder roots.
///
/// A client may open a single file with no workspace at all — VS Code's
/// "open file" mode does — and answering nothing there would make the picker
/// look broken on the one file the user has.
#[must_use]
pub fn open_document_symbols(
    query: &str,
    docs: &DocumentStore,
    enc: Encoding,
) -> Vec<WorkspaceSymbol> {
    let needle = query.to_lowercase();
    let mut out = Vec::new();
    let mut uris: Vec<&Uri> = docs.uris().collect();
    uris.sort_by_key(|u| u.as_str());
    for uri in uris {
        let Some(doc) = docs.get(uri) else { continue };
        let snapshot = Snapshot::for_document(uri.as_str(), doc);
        collect(&snapshot, uri, enc, &needle, &mut out);
    }
    out
}

fn collect(
    snapshot: &Snapshot,
    uri: &Uri,
    enc: Encoding,
    needle: &str,
    out: &mut Vec<WorkspaceSymbol>,
) {
    for symbol in document_symbols(snapshot, enc) {
        push_matching(&symbol, None, uri, needle, out);
    }
}

#[allow(deprecated)]
fn push_matching(
    symbol: &lsp_types::DocumentSymbol,
    container: Option<&str>,
    uri: &Uri,
    needle: &str,
    out: &mut Vec<WorkspaceSymbol>,
) {
    if needle.is_empty() || symbol.name.to_lowercase().contains(needle) {
        out.push(WorkspaceSymbol {
            name: symbol.name.clone(),
            kind: symbol.kind,
            tags: None,
            container_name: container.map(ToString::to_string),
            location: OneOf::Left(Location {
                uri: uri.clone(),
                range: symbol.selection_range,
            }),
            data: None,
        });
    }
    // A struct's fields and an enum's variants are symbols too, and they carry
    // the declaration they belong to as their container — which is what makes
    // `Point` and `Point.x` distinguishable in a flat picker.
    for child in symbol.children.iter().flatten() {
        push_matching(child, Some(&symbol.name), uri, needle, out);
    }
}

/// The `SymbolKind` a picker shows for a file with no parsed symbols.
pub const FILE_KIND: SymbolKind = SymbolKind::FILE;

/// A `file:` URI for `path`, or `None` if the path is not absolute.
///
/// Percent-encoding is done here rather than by a dependency because the rule is
/// four lines and the alternative is a URL crate in a workspace that has none.
/// Only the bytes RFC 3986 calls unreserved, plus `/`, `:`, `.`, `-`, `_` and
/// `~`, survive unescaped — which covers every path a puzzle workspace has and
/// escapes the spaces that break the rest.
#[must_use]
pub fn path_to_uri(path: &Path) -> Option<Uri> {
    if !path.is_absolute() {
        return None;
    }
    let mut encoded = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'.' | b'_' | b'~' | b':' => {
                encoded.push(byte as char);
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    std::str::FromStr::from_str(&encoded).ok()
}

/// The path a `file:` URI names, or `None` for any other scheme.
#[must_use]
pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let text = uri.as_str();
    let rest = text
        .strip_prefix("file://")
        .map(|r| r.strip_prefix("localhost").unwrap_or(r))?;
    // Everything before a `?query` or `#fragment` is the path.
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    Some(PathBuf::from(percent_decode(rest)))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_round_trips_through_a_uri() {
        for path in [
            "/tmp/day01.px",
            "/tmp/a folder/day 02.px",
            "/tmp/héllo/día.px",
        ] {
            let uri = path_to_uri(Path::new(path)).expect("an absolute path has a URI");
            assert!(
                !uri.as_str().contains(' '),
                "a space must be escaped: {}",
                uri.as_str()
            );
            assert_eq!(
                uri_to_path(&uri).expect("a file URI has a path"),
                PathBuf::from(path)
            );
        }
    }

    #[test]
    fn a_relative_path_has_no_uri() {
        assert!(path_to_uri(Path::new("day01.px")).is_none());
    }

    #[test]
    fn a_non_file_uri_has_no_path() {
        let uri: Uri = std::str::FromStr::from_str("untitled:Untitled-1").expect("valid");
        assert!(uri_to_path(&uri).is_none());
    }
}
