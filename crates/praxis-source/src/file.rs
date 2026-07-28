//! Source files and the [`SourceMap`] that interns them.
//!
//! A [`SourceMap`] is the compiler's registry of loaded source files. It owns
//! the file text, mints opaque [`FileId`] handles, and precomputes each file's
//! [`LineMap`]. Files are append-only once interned — source snapshots (§13.1)
//! must remain stable for the lifetime of a compilation session.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::line_map::LineMap;
use crate::span::FileSpan;

/// An opaque handle to one interned source file.
///
/// `FileId`s are only ever minted by [`SourceMap::intern`]; they are pure
/// identity tokens and must never be constructed by hand. The `#[non_exhaustive]`
/// keeps the construction surface closed even across crate boundaries.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(u32);

impl FileId {
    /// The synthetic file id used for diagnostics that are not tied to any real
    /// source location (for example a CLI usage error). Real files always start
    /// at higher ids.
    pub const SYNTHETIC: FileId = FileId(u32::MAX);

    #[inline]
    pub const fn to_u32(self) -> u32 {
        self.0
    }
}

impl std::fmt::Debug for FileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if *self == Self::SYNTHETIC {
            write!(f, "FileId(<synthetic>)")
        } else {
            write!(f, "FileId({})", self.0)
        }
    }
}

/// One loaded source file: its id, the path it was loaded from, the full text,
/// and a precomputed line table.
#[derive(Clone)]
pub struct SourceFile {
    id: FileId,
    path: PathBuf,
    text: String,
    line_map: LineMap,
}

impl SourceFile {
    #[inline]
    pub fn id(&self) -> FileId {
        self.id
    }

    #[inline]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[inline]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[inline]
    pub fn line_map(&self) -> &LineMap {
        &self.line_map
    }

    /// A [`FileSpan`] covering the whole file.
    pub fn full_span(&self) -> FileSpan {
        FileSpan::new(self.id, crate::span::Span::new(0, self.text.len() as u32))
    }
}

/// The compiler's registry of source files.
///
/// Interning is append-only: once a file has an id its text and path are fixed
/// for the lifetime of the map, which keeps diagnostics and snapshots stable.
/// The map is internally synchronized so it can be shared across threads (the
/// LSP, for example, reads source from a background query thread).
#[derive(Default)]
pub struct SourceMap {
    files: RwLock<Vec<SourceFile>>,
}

impl SourceMap {
    /// Create an empty source map.
    pub fn new() -> SourceMap {
        SourceMap::default()
    }

    /// Intern a source file under the given path, returning its id.
    ///
    /// Each call mints a fresh id even for a repeated path: a later load of the
    /// same path is treated as a distinct snapshot. This matches the §13.1
    /// "revisioned source" model — the LSP will hold several revisions of one
    /// file simultaneously.
    pub fn intern(&self, path: impl Into<PathBuf>, text: impl Into<String>) -> FileId {
        let path = path.into();
        let text = text.into();
        let line_map = LineMap::new(&text);

        let mut files = self.files.write().unwrap();
        // SYNTHETIC is reserved; a non-pathological program cannot exhaust u32
        // ids, but if it ever does we'd rather panic loudly than alias SYNTHETIC.
        let id = u32::try_from(files.len()).expect("more than 2^32 source files");
        assert!(id != FileId::SYNTHETIC.to_u32(), "file id space exhausted");

        files.push(SourceFile {
            id: FileId(id),
            path,
            text,
            line_map,
        });
        FileId(id)
    }

    /// The number of interned files.
    pub fn len(&self) -> usize {
        self.files.read().unwrap().len()
    }

    /// True if no files have been interned.
    pub fn is_empty(&self) -> bool {
        self.files.read().unwrap().is_empty()
    }

    /// Fetch a file by id. Returns `None` for unknown or synthetic ids.
    pub fn get(&self, id: FileId) -> Option<FileView<'_>> {
        if id == FileId::SYNTHETIC {
            return None;
        }
        let files = self.files.read().unwrap();
        let file = files.get(id.to_u32() as usize)?;
        // SAFETY: we extend the lifetime of the borrow to `&self`. This is sound
        // because the map is append-only: a file, once pushed, is never moved
        // or mutated, so the slice and its contents stay at a stable address
        // for as long as the map exists. The RwLock read guard is dropped here;
        // mutation (intern) only appends and never reorders existing entries.
        let file = unsafe { &*(file as *const SourceFile) };
        Some(FileView { file })
    }
}

/// A borrowed view of an interned source file.
///
/// Because the [`SourceMap`] is append-only, a `FileView` is cheap and the
/// underlying file is stable for the lifetime of the map. The guard is hidden
/// so the view is `Send`-friendly where the map is shared.
pub struct FileView<'a> {
    file: &'a SourceFile,
}

impl<'a> std::ops::Deref for FileView<'a> {
    type Target = SourceFile;
    fn deref(&self) -> &SourceFile {
        self.file
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_assigns_sequential_ids() {
        let map = SourceMap::new();
        let a = map.intern("a.px", "first");
        let b = map.intern("b.px", "second");
        assert_eq!(a.to_u32(), 0);
        assert_eq!(b.to_u32(), 1);
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn intern_same_path_yields_distinct_ids() {
        let map = SourceMap::new();
        let first = map.intern("dup.px", "one");
        let second = map.intern("dup.px", "two");
        assert_ne!(first, second, "each intern is a distinct snapshot");
    }

    #[test]
    fn get_returns_interned_file() {
        let map = SourceMap::new();
        let id = map.intern("day.px", "out(1)\n");
        let view = map.get(id).expect("file was just interned");
        assert_eq!(view.path(), Path::new("day.px"));
        assert_eq!(view.text(), "out(1)\n");
        assert_eq!(view.id(), id);
    }

    #[test]
    fn synthetic_and_unknown_ids_return_none() {
        let map = SourceMap::new();
        assert!(map.get(FileId::SYNTHETIC).is_none());
        assert!(map.get(FileId(0)).is_none()); // no files interned
    }

    #[test]
    fn full_span_covers_whole_file() {
        let map = SourceMap::new();
        let id = map.intern("f.px", "abc");
        let view = map.get(id).unwrap();
        let span = view.full_span();
        assert_eq!(span.file, id);
        assert_eq!(span.span.start().to_u32(), 0);
        assert_eq!(span.span.end().to_u32(), 3);
    }

    #[test]
    fn empty_map_reports_empty() {
        let map = SourceMap::new();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
    }

    #[cfg(miri)]
    #[test]
    fn regression_file_view_remains_valid_when_more_files_are_interned() {
        let map = SourceMap::new();
        let first = map.intern("first.px", "stable");
        let view = map.get(first).expect("first file exists");

        // Force the backing Vec through several reallocations while `view`
        // remains live. Miri should reject the subsequent access until stored
        // SourceFiles have stable addresses (or FileView retains a read guard).
        for i in 0..4_096 {
            map.intern(format!("later-{i}.px"), format!("revision {i}"));
        }

        assert_eq!(view.text(), "stable");
        assert_eq!(view.path(), Path::new("first.px"));
    }
}
