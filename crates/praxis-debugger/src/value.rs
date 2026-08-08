//! Bounded value rendering for the debugger's displays.
//!
//! A `GcRef` formats through its descriptor (§11.4), and that render is
//! unbounded: a `Vec[Int]` holding a million elements writes a million elements.
//! `praxis run`'s result line wants exactly that — it is the program's answer.
//! Every debugger surface wants the opposite: a `locals` row, a TUI pane cell,
//! and a backtrace line each have one line to work with, and a value that
//! overruns it hides the rows underneath rather than explaining the one it is on.
//!
//! So this module renders a value the way the debugger needs it: cut at an
//! element boundary, with an explicit `...` marking what was dropped.
//!
//! ```text
//!   data: Vec[Int] = [10, 20, 30, ...]
//! ```
//!
//! ## Why the cut is textual
//!
//! The obvious implementation walks the value structurally and stops after N
//! elements. It is not available: `GcRef` exposes `as_vec`/`as_text` and the
//! scalar readers, and nothing for `Map`, `Set`, `Deque`, a record or an enum
//! payload. A structural walk would therefore bound `Vec` and leave every other
//! container unbounded — and would need a new arm for each container added
//! later.
//!
//! Cutting the *rendered text* at a bracket boundary needs no per-type knowledge
//! and so bounds all of them, including nesting, from the one implementation.
//! The cost is that the full render happens first, which is what [`CappedSink`]
//! is for: the descriptor callbacks ignore `fmt::Write` errors (they are written
//! `let _ = out.write_str(..)`), so a sink that refuses bytes past a hard cap
//! keeps a million-element `Vec` from ever becoming a million-element `String`.
//! The traversal still runs; the allocation does not grow.

use std::fmt;

/// What a slot renders as when nothing has ever been written into it
/// (`DebugLocal::value == None`).
///
/// The absence is the type's, not a sentinel pointer's (F18): a slot nothing was
/// ever spilled into reads back as `None`, and every display turns that `None`
/// into this one word.
///
/// Most usefully it is the faulting expression's own temp — the value the
/// program never got as far as computing.
pub const UNINIT: &str = "<uninit>";

/// What a slot renders as when the value it held has been collected
/// ([`praxis_runtime::DebugValue::Reclaimed`]).
///
/// Distinct from [`UNINIT`], and the distinction is the point: one is a local
/// the program never wrote, the other a local it wrote and finished with. They
/// were one string until the slot could tell them apart.
pub const COLLECTED: &str = "<collected>";

/// What a value renders as when its descriptor wrote no bytes at all.
///
/// "The descriptor wrote no bytes" and "the read failed" are one observation
/// from here, so this word answers both. It is deliberately *not* what an empty
/// `Text` gets: the debugger's displays format with `format_debug`, which writes
/// a quoted literal (§11.4), so `""` shows up as the value it is. `p EXPR` and
/// the locals rows share the constant for the same reason they share that
/// rendering — the two displays disagreeing about one value is its own defect.
pub const UNREADABLE: &str = "<unreadable>";

/// The default budget for one value on one line: how many bytes of rendered
/// value a display keeps before cutting at the nearest element boundary.
///
/// Sized for a line of a locals pane that also carries a name and a type, on a
/// terminal nobody has widened. A caller with a whole pane to fill (`heap EXPR`)
/// passes its own.
pub const DEFAULT_BUDGET: usize = 60;

/// The hard ceiling on bytes [`CappedSink`] will buffer, independent of the
/// display budget.
///
/// This is the memory bound, not the display bound. It is far above any budget
/// so that the truncator always has enough context to find a boundary and to
/// know that more followed, while a pathological value still cannot allocate
/// without limit.
const HARD_CAP: usize = 8 * 1024;

/// A `fmt::Write` sink that accepts at most [`HARD_CAP`] bytes and remembers
/// that it stopped.
///
/// Returning `Err` from `write_str` is the signal to a well-behaved formatter to
/// give up early. The descriptor callbacks are not well-behaved in that sense —
/// they discard the result — so this is not a way to stop the traversal. It is a
/// way to stop the *buffer*: past the cap the bytes are counted and dropped.
struct CappedSink {
    buf: String,
    /// Whether any byte was refused. The truncator uses this to know a value was
    /// longer than what it can see, even when what it can see ends tidily.
    overflowed: bool,
}

impl CappedSink {
    fn new() -> CappedSink {
        CappedSink {
            buf: String::new(),
            overflowed: false,
        }
    }
}

impl fmt::Write for CappedSink {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.buf.len() >= HARD_CAP {
            self.overflowed = true;
            return Err(fmt::Error);
        }
        let room = HARD_CAP - self.buf.len();
        if s.len() <= room {
            self.buf.push_str(s);
            return Ok(());
        }
        // Push what fits, on a char boundary so `buf` stays valid UTF-8.
        let mut cut = room;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        self.buf.push_str(&s[..cut]);
        self.overflowed = true;
        Err(fmt::Error)
    }
}

/// Render `value` through its descriptor, bounded to `budget` bytes with an
/// element-boundary cut.
///
/// This is the debugger's counterpart to the unbounded `GcRef::format` the
/// result line uses. Scalars (which `ADR-120` elides the box for) go through
/// `ScalarValue`'s own `Display`, exactly as the unbounded path does, so the two
/// renderings cannot drift.
pub fn format_bounded(value: praxis_runtime::DebugValue, budget: usize) -> String {
    use praxis_runtime::DebugValue;
    let reference = match value {
        DebugValue::Reference(r) => r,
        // No object, so no descriptor to dispatch through. A scalar's rendering
        // is short by construction; truncating it would only ever hide digits.
        DebugValue::Scalar(s) => return s.to_string(),
        // A value was here; the collector has taken it (ADR-106). Worth its own
        // word rather than `<uninit>`, which is what this used to print and
        // which says the opposite: that the line never ran. `COLLECTED` names
        // the mechanism the user can act on — the binding's last *use*, not its
        // scope, is where it stopped being a root (ADR-044 decision 2), so the
        // way to see it at the fault is to use it later.
        DebugValue::Reclaimed => return COLLECTED.to_string(),
    };
    let mut sink = CappedSink::new();
    // `format_debug`, so a `Text` is a quoted literal — at this level and at
    // every level below it, since a container passes its style down with its
    // writer. Without it the empty string wrote no bytes and fell into the
    // `<unreadable>` answer below, and `["a", "", "b"]` rendered `[a, , b]`,
    // where the middle element is not short but *absent*.
    reference.format_debug(&mut sink);
    if sink.buf.is_empty() {
        return UNREADABLE.to_string();
    }
    truncate_rendered(&sink.buf, budget, sink.overflowed)
}

/// Cut `s` so the **result** is at most `budget` characters wide, at a structural
/// boundary, closing whatever brackets the cut left open.
///
/// `more` says a caller already knows characters were dropped before this was
/// called (the [`CappedSink`] hit its cap), which forces the ellipsis even when
/// `s` itself fits the budget.
///
/// The scan tracks bracket depth and quoted runs, so:
/// - `[10, 20, 30, 40]` → `[10, 20, ...]` (cut at a separator, bracket closed)
/// - `{a: 1, b: 2}` → `{a: 1, ...}` (same, for a `Map` or a record)
/// - `[[1, 2], [3, 4]]` → `[[1, 2], ...]` (a nested value is kept whole or dropped whole)
/// - `"a long string"` → `"a long…"` (no boundary inside a quoted run, so cut in it)
///
/// A value with no boundary that fits (one very long token) is cut mid-token with
/// a trailing `…`, because showing a prefix beats showing a blank.
///
/// ## Why the budget is the output width, not a scan limit
///
/// The budget is the space a caller has — a pane column. So it has to bound what
/// this *returns*, including the `, ...` and the closing brackets appended after
/// the cut. Treating it as the point to stop scanning instead overshoots by that
/// overhead, and the caller's pane then clips the overhang mid-element — which is
/// the ragged ending the boundary cut exists to prevent. Each candidate cut below
/// is therefore accepted only if its *finished* width fits.
///
/// Width is counted in `char`s rather than bytes for the same reason: a pane is
/// that many columns wide, and `…` is one column but three bytes.
pub fn truncate_rendered(s: &str, budget: usize, more: bool) -> String {
    if s.chars().count() <= budget && !more {
        return s.to_string();
    }

    // Walk the value, remembering the last cut whose finished width still fits:
    // a `,` belonging to the outermost container still open (cutting there keeps
    // whole elements and never splits a nested value), and — as a fallback — the
    // last mid-token position.
    //
    // Every candidate is a byte index into `s`, so the closers below can be
    // derived from `s[..cut]` itself rather than from the output, whose width the
    // ellipsis has already changed.
    let mut depth = 0usize;
    let mut in_quotes = false;
    let mut escaped = false;
    let mut best_boundary: Option<usize> = None;
    let mut best_token: Option<usize> = None;

    for (char_i, (byte_i, ch)) in s.char_indices().enumerate() {
        // Nothing past here can fit, boundary or not.
        if char_i > budget {
            break;
        }
        // Cutting *at* `byte_i` keeps `s[..byte_i]` (`char_i` chars) and then owes
        // `…`, a closing quote if one is open, and one closer per open bracket.
        if char_i + 1 + usize::from(in_quotes) + depth <= budget {
            best_token = Some(byte_i);
        }
        if escaped {
            escaped = false;
            continue;
        }
        if in_quotes {
            match ch {
                '\\' => escaped = true,
                '"' => in_quotes = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_quotes = true,
            '[' | '{' | '(' => depth += 1,
            ']' | '}' | ')' => depth = depth.saturating_sub(1),
            // Only a separator of the *outermost* open container is a clean cut:
            // inside a nested one, cutting would leave a half-rendered element
            // that reads like a complete one. Such a cut owes `, ...` (5) plus one
            // closer per open bracket.
            ',' if depth == 1 && char_i + 5 + depth <= budget => {
                best_boundary = Some(byte_i);
            }
            _ => {}
        }
    }

    let (cut, ellipsis) = match (best_boundary, best_token) {
        // Keep the elements before the separator, then say more followed.
        (Some(i), _) => (i, ", ...".to_string()),
        (None, Some(i)) => {
            // A cut inside a quoted run closes the quote, so the value still
            // reads as the `Text` it is rather than as an unterminated literal.
            let mut e = String::from("…");
            if quote_open_at(&s[..i]) {
                e.push('"');
            }
            (i, e)
        }
        // A budget too small for even one character plus its closers.
        (None, None) => return "…".to_string(),
    };

    let mut out = String::with_capacity(cut + ellipsis.len() + 8);
    out.push_str(&s[..cut]);
    out.push_str(&ellipsis);
    // Close every container the cut left open, innermost first, so the value
    // stays balanced and its shape (a Vec of Maps, say) stays legible.
    for ch in open_brackets(&s[..cut]).iter().rev() {
        out.push(match ch {
            '[' => ']',
            '{' => '}',
            _ => ')',
        });
    }
    out
}

/// Cut `s` to at most `budget` characters with a trailing `…`, without any
/// structural interpretation.
///
/// For text that is *not* a value: a source snippet, a path, a label. Running
/// those through [`truncate_rendered`] would close their brackets, so a snippet
/// cut inside `xs[scaled]` would come back as `xs[sc…]` — balanced, and claiming
/// an index expression the program never wrote.
pub fn elide(s: &str, budget: usize) -> String {
    if s.chars().count() <= budget {
        return s.to_string();
    }
    if budget <= 1 {
        return "…".to_string();
    }
    // One column is owed to the ellipsis.
    let keep = budget - 1;
    let end = s
        .char_indices()
        .nth(keep)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    let mut out = String::with_capacity(end + 3);
    out.push_str(&s[..end]);
    out.push('…');
    out
}

/// The brackets still open at the end of `prefix`, outermost first. Quoted runs
/// are skipped so a bracket inside a `Text` value is not counted as structure.
fn open_brackets(prefix: &str) -> Vec<char> {
    let mut stack = Vec::new();
    let mut in_quotes = false;
    let mut escaped = false;
    for ch in prefix.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_quotes {
            match ch {
                '\\' => escaped = true,
                '"' => in_quotes = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_quotes = true,
            '[' | '{' | '(' => stack.push(ch),
            ']' | '}' | ')' => {
                stack.pop();
            }
            _ => {}
        }
    }
    stack
}

/// Whether `prefix` ends inside a quoted run (so a cut there owes a closing `"`).
fn quote_open_at(prefix: &str) -> bool {
    let mut in_quotes = false;
    let mut escaped = false;
    for ch in prefix.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_quotes {
            match ch {
                '\\' => escaped = true,
                '"' => in_quotes = false,
                _ => {}
            }
            continue;
        }
        if ch == '"' {
            in_quotes = true;
        }
    }
    in_quotes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Text` renders quoted here, at every depth, and an empty one is
    /// therefore visible.
    ///
    /// This is [`format_bounded`]'s half of the change; `text_renders_one_way_
    /// for_the_program_and_another_for_the_debugger` in `praxis-runtime` is the
    /// descriptor's. Both are needed: the callback can quote all it likes if this
    /// function asks for the other rendering.
    ///
    /// It also makes [`truncate_rendered`]'s quoted-run tracking reachable. That
    /// scan has always understood `"` and `\\` — see the `"a long string"` row in
    /// its doc — against a renderer that never produced either.
    #[test]
    fn a_text_is_quoted_so_the_empty_one_is_visible() {
        let mut rt = praxis_runtime::Runtime::new();
        let render = |r: praxis_runtime::GcRef| {
            format_bounded(praxis_runtime::DebugValue::Reference(r), DEFAULT_BUDGET)
        };

        assert_eq!(render(rt.alloc_text("asdf")), "\"asdf\"");
        // The row that used to read `b: Text = <unreadable>`: zero bytes out of
        // the descriptor was the only evidence the renderer had.
        assert_eq!(render(rt.alloc_text("")), "\"\"");

        // Nested, which is the case a per-type debug callback could not have
        // reached: the element is quoted because the `Vec` passed its style down.
        let mut ctx = rt.context();
        // SAFETY: `ctx` is wired to `rt`; every argument is a live `GcRef` of the
        // type the wrapper names.
        let xs = unsafe {
            let xs = praxis_runtime::abi::praxis_vec_new(
                &mut ctx,
                praxis_runtime::BuiltinTypeId::Text.descriptor(),
            );
            let empty = rt.alloc_text("");
            let b = rt.alloc_text("b");
            praxis_runtime::abi::praxis_vec_push(&mut ctx, xs, empty);
            praxis_runtime::abi::praxis_vec_push(&mut ctx, xs, b);
            xs
        };
        assert_eq!(
            render(xs),
            r#"["", "b"]"#,
            "an empty element is an element, not a gap"
        );
    }

    /// The shape the whole module exists for: a collection longer than the line
    /// keeps whole elements and says that more followed.
    #[test]
    fn a_long_vec_cuts_at_an_element_boundary() {
        let s = "[10, 20, 30, 40, 50, 60, 70, 80]";
        let out = truncate_rendered(s, 16, false);
        assert_eq!(out, "[10, 20, ...]");
        // Never a split element: whatever survives parses as a whole value.
        assert!(!out.contains("3,"), "no half-written element: {out}");
        // And it fits the space the caller actually had.
        assert!(out.chars().count() <= 16, "within budget: {out}");
    }

    /// The invariant the callers depend on: the *result* fits the budget, because
    /// the budget is a pane column and not a scan limit. A result wider than the
    /// column gets clipped by the pane mid-element, which is exactly what the
    /// boundary cut exists to prevent.
    #[test]
    fn the_result_never_exceeds_the_budget() {
        let cases = [
            "[10, 20, 30, 40, 50, 60, 70, 80]",
            "{alpha: 1, beta: 2, gamma: 3, delta: 4}",
            "[[1, 2], [3, 4], [5, 6]]",
            "[{a: [1, 2]}, {b: [3, 4]}, {c: [5, 6]}]",
            "\"the quick brown fox jumps over the lazy dog\"",
            "[\"one, two, three\", \"four\"]",
            "999999999999999999999999999999",
            "[]",
            "[[[[[[1]]]]]]",
        ];
        for s in cases {
            for budget in 1..=40 {
                let out = truncate_rendered(s, budget, false);
                assert!(
                    out.chars().count() <= budget.max(1),
                    "budget {budget} on {s:?} produced {} chars: {out}",
                    out.chars().count()
                );
                assert!(
                    open_brackets(&out).is_empty(),
                    "budget {budget} on {s:?} left brackets open: {out}"
                );
            }
        }
    }

    #[test]
    fn a_value_within_budget_is_returned_unchanged() {
        let s = "[1, 2, 3]";
        assert_eq!(truncate_rendered(s, 60, false), s);
    }

    /// `more` is the [`CappedSink`] having refused bytes. The visible text can
    /// end tidily and still be incomplete, so the ellipsis is owed even though
    /// the budget was not reached.
    #[test]
    fn overflow_forces_an_ellipsis_even_when_the_text_fits() {
        let out = truncate_rendered("[1, 2, 3]", 60, true);
        assert!(out.ends_with("...]"), "overflow is reported: {out}");
    }

    /// A `Map`/record renders with braces; the same boundary rule applies, and
    /// the brace is closed so the shape stays readable.
    #[test]
    fn a_long_map_cuts_at_a_boundary_and_closes_its_brace() {
        let s = "{alpha: 1, beta: 2, gamma: 3, delta: 4}";
        assert_eq!(truncate_rendered(s, 24, false), "{alpha: 1, beta: 2, ...}");
        // A tighter budget drops back to the previous boundary rather than
        // overshooting: the result has to fit the space, not merely start in it.
        assert_eq!(truncate_rendered(s, 20, false), "{alpha: 1, ...}");
    }

    /// A nested element is kept whole or dropped whole — never cut open. A cut
    /// inside `[3, 4` would render as a complete two-element inner Vec that the
    /// program never held.
    #[test]
    fn a_nested_value_is_never_cut_open() {
        let s = "[[1, 2], [3, 4], [5, 6]]";
        assert_eq!(truncate_rendered(s, 13, false), "[[1, 2], ...]");
        // Too tight for the boundary cut, so it falls back to a mid-token cut —
        // which still must not present a partial inner Vec as a whole one.
        let tight = truncate_rendered(s, 12, false);
        assert!(!tight.contains("[3]"), "no invented inner element: {tight}");
        assert!(open_brackets(&tight).is_empty(), "balanced: {tight}");
    }

    /// The inner separator of `[[1, 2], ...]` sits at depth 2 and must not be
    /// mistaken for a cut point, even when it is the only comma in range.
    #[test]
    fn an_inner_separator_is_not_a_cut_point() {
        let s = "[[100, 200, 300, 400]]";
        let out = truncate_rendered(s, 10, false);
        // No depth-1 comma exists, so this falls to the mid-token cut and closes
        // both brackets rather than cutting at the depth-2 comma.
        assert!(out.starts_with("[[100"), "{out}");
        assert!(out.ends_with("]]"), "both brackets closed: {out}");
    }

    /// A long `Text` has no structural boundary, so it is cut inside the quoted
    /// run — and the quote is closed, so it still reads as Text.
    #[test]
    fn a_long_text_is_cut_inside_its_quotes() {
        let s = "\"the quick brown fox jumps over the lazy dog\"";
        let out = truncate_rendered(s, 12, false);
        assert!(out.starts_with("\"the quick"), "{out}");
        assert!(out.ends_with("…\""), "quote is closed: {out}");
    }

    /// A separator *inside* a Text value is not structure. Cutting there would
    /// split the string mid-word and claim it was an element boundary.
    #[test]
    fn a_comma_inside_text_is_not_a_boundary() {
        let s = "[\"one, two, three, four, five\", \"six\"]";
        let out = truncate_rendered(s, 20, false);
        // The only structural comma is the one after the first Text, at offset 29
        // — past the budget. So the commas *within* the Text stay literal text
        // and the cut lands inside the quoted run, not at any of them.
        assert!(
            !out.ends_with(", ...]"),
            "did not cut at a comma inside Text: {out}"
        );
        assert!(
            out.ends_with("…\"]"),
            "cut inside the quotes, closing both quote and bracket: {out}"
        );
        assert!(
            out.contains("one, two"),
            "the commas survive as text: {out}"
        );
    }

    /// `elide` is for text that is not a value, so it must not invent structure.
    /// The value truncator would balance the brackets and hand back `xs[sc…]`,
    /// which reads as an index expression the program never wrote.
    #[test]
    fn elide_does_not_balance_brackets() {
        let snippet = "return xs[scaled]";
        let out = elide(snippet, 14);
        assert_eq!(out, "return xs[sca…");
        assert!(!out.ends_with(']'), "no invented closer: {out}");
        assert!(out.chars().count() <= 14, "within budget: {out}");
    }

    #[test]
    fn elide_leaves_short_text_alone_and_survives_tiny_budgets() {
        assert_eq!(elide("short", 10), "short");
        assert_eq!(elide("short", 5), "short", "exactly fitting is unchanged");
        assert_eq!(elide("longer", 1), "…");
        assert_eq!(elide("longer", 0), "…");
    }

    /// Multi-byte characters are one column each; cutting by bytes would both
    /// mis-measure the width and risk slicing a char in half.
    #[test]
    fn elide_counts_characters_not_bytes() {
        let s = "ééééééééé";
        let out = elide(s, 4);
        assert_eq!(out.chars().count(), 4, "four columns: {out}");
        assert_eq!(out, "ééé…");
    }

    /// An escaped quote does not end the quoted run; treating it as the end
    /// would make the rest of a Text value look like structure.
    #[test]
    fn an_escaped_quote_does_not_end_the_run() {
        assert!(quote_open_at("\"a \\\" b"), "still inside the quoted run");
        assert!(
            !quote_open_at("\"a \\\" b\""),
            "run closed by the real quote"
        );
    }

    /// A cut always leaves a balanced value, whatever depth it landed at.
    #[test]
    fn every_cut_closes_the_brackets_it_left_open() {
        for budget in 1..40 {
            let s = "[{a: [1, 2]}, {b: [3, 4]}, {c: [5, 6]}]";
            let out = truncate_rendered(s, budget, false);
            assert!(
                open_brackets(&out).is_empty(),
                "budget {budget} left brackets open: {out}"
            );
        }
    }

    /// The memory bound: a value far past the cap does not become a `String` far
    /// past the cap, and the fact that it overflowed survives.
    #[test]
    fn the_sink_stops_buffering_at_the_hard_cap() {
        use std::fmt::Write;
        let mut sink = CappedSink::new();
        // Write well past the cap in chunks, ignoring errors the way the
        // descriptor callbacks do.
        for _ in 0..4000 {
            let _ = sink.write_str("0123456789, ");
        }
        assert!(sink.overflowed, "the cap was reached and recorded");
        assert!(
            sink.buf.len() <= HARD_CAP,
            "buffer stayed within the cap: {} bytes",
            sink.buf.len()
        );
    }

    /// The cap must not split a multi-byte char: `buf` is a `String` and has to
    /// stay valid UTF-8 no matter where the boundary falls.
    #[test]
    fn the_sink_cuts_on_a_char_boundary() {
        use std::fmt::Write;
        let mut sink = CappedSink::new();
        // Fill to one byte short of the cap, then offer a 3-byte char.
        let filler = "a".repeat(HARD_CAP - 1);
        let _ = sink.write_str(&filler);
        let _ = sink.write_str("→");
        assert!(sink.overflowed);
        // The char was refused whole rather than half-written.
        assert_eq!(sink.buf.len(), HARD_CAP - 1, "no partial char was pushed");
        assert!(sink.buf.ends_with('a'));
    }
}
