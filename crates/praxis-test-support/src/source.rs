//! Source-map helpers for tests.

use praxis_source::{FileId, SourceMap};

/// Build a [`SourceMap`] containing one file and return both, ready to pass to
/// a diagnostic constructor or renderer.
///
/// # Example
/// ```no_run
/// use praxis_test_support::single_file;
/// let (map, id) = single_file("day.px", "out(1)\n");
/// ```
pub fn single_file(name: &str, text: &str) -> (SourceMap, FileId) {
    let map = SourceMap::new();
    let id = map.intern(name, text);
    (map, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_file_returns_consistent_pair() {
        let (map, id) = single_file("d.px", "hello");
        assert_eq!(id.to_u32(), 0);
        let view = map.get(id).expect("file just interned");
        assert_eq!(view.text(), "hello");
    }
}
