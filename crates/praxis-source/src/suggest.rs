//! "Did you mean …?" — one rule for the whole compiler (ADR-132).
//!
//! Four passes have the same shape of mistake to report: a name the user wrote
//! that no table holds. A parser constructor (`line` for `lines`, §15.3's own
//! example), an atomic parser, a binding, a method. Each of them has the right
//! answer within reach — a closed table, or the scope chain — and what they need
//! in common is *when* a near miss is near enough to offer.
//!
//! That threshold is the whole of this module, and it lives here so the four do
//! not each pick their own. A suggestion that fires too eagerly is worse than
//! none: an editor offering to rewrite `x` as `y` teaches the user to stop
//! reading the quick-fix list.

/// The most similar candidate to `name`, if any is close enough.
///
/// "Close enough" is an edit distance within `max(1, len / 3)` — rustc's rule,
/// for rustc's reason: it accepts one typo in a short name and proportionally
/// more in a long one, while refusing to call two three-letter names neighbours
/// because they differ everywhere.
///
/// Ties go to the **first** candidate, so a caller that passes a closed table in
/// its own order gets a stable answer rather than one that depends on how the
/// table was iterated.
#[must_use]
pub fn nearest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let budget = (name.chars().count() / 3).max(1);
    let mut best: Option<(usize, &'a str)> = None;
    for candidate in candidates {
        if candidate == name {
            // An exact match is not a suggestion — the caller is reporting that
            // this name is unusable *here*, and "did you mean the thing you
            // wrote" is noise.
            continue;
        }
        let distance = edit_distance(name, candidate);
        if distance > budget {
            continue;
        }
        match best {
            Some((d, _)) if d <= distance => {}
            _ => best = Some((distance, candidate)),
        }
    }
    best.map(|(_, c)| c)
}

/// Levenshtein distance in characters, not bytes.
///
/// Characters because a name is what a user typed: two accented letters differ
/// by one edit even though they differ by two bytes, and a byte-wise distance
/// would quietly refuse to suggest anything for a non-ASCII identifier — which
/// §4.1 permits.
#[must_use]
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    // One row at a time: the table is (a.len + 1) × (b.len + 1) and only the
    // previous row is ever read.
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = prev[j] + usize::from(ca != cb);
            let deletion = prev[j + 1] + 1;
            let insertion = current[j] + 1;
            current[j + 1] = substitution.min(deletion).min(insertion);
        }
        std::mem::swap(&mut prev, &mut current);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §15.3's own example.
    #[test]
    fn line_suggests_lines() {
        assert_eq!(
            nearest("line", ["lines", "sections", "csv", "grid"]),
            Some("lines")
        );
    }

    #[test]
    fn a_name_with_nothing_near_it_suggests_nothing() {
        assert_eq!(nearest("frobnicate", ["lines", "sections", "csv"]), None);
        // Two short names that differ everywhere are not neighbours, even
        // though the distance is small in absolute terms.
        assert_eq!(nearest("abc", ["xyz"]), None);
    }

    /// A one-character typo in a short name is within budget; two is not.
    #[test]
    fn the_budget_grows_with_the_name() {
        assert_eq!(nearest("csw", ["csv"]), Some("csv"));
        assert_eq!(nearest("cww", ["csv"]), None);
        assert_eq!(nearest("sectionz", ["sections"]), Some("sections"));
        assert_eq!(nearest("sektionz", ["sections"]), Some("sections"));
    }

    /// The name the user wrote is never the suggestion, even when the table
    /// holds it — the caller is reporting that it is unusable *here*.
    #[test]
    fn an_exact_match_is_not_a_suggestion() {
        assert_eq!(nearest("lines", ["lines"]), None);
    }

    #[test]
    fn the_nearest_wins_and_ties_go_to_the_first() {
        // Two edits away loses to one.
        assert_eq!(nearest("abc", ["axy", "abx"]), Some("abx"));
        // `axc` and `abx` are both one edit from `abc`; the first offered wins.
        assert_eq!(nearest("abc", ["axc", "abx"]), Some("axc"));
    }

    #[test]
    fn distance_counts_characters_not_bytes() {
        assert_eq!(edit_distance("é", "e"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }
}
