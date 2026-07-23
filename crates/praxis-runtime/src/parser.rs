//! The runtime input-parser interpreter (§7, M6).
//!
//! Evaluates a compiled [`ParserPlan`] against the process-input buffer (or a
//! `Text` value), allocating GC results (`Int`, `Char`, source-slice `Text`,
//! `Vec`, `Grid`, `Record`) and raising `FaultKind::ParseFailed` on mismatch.
//!
//! The plan type and global slab live in `praxis-input-parser`; this interpreter
//! looks up plans by index and walks their `#[repr(C)]` node arena.

use crate::context::RuntimeContext;
use crate::scalars;
use crate::text::{text_bytes, TextPayload};
use crate::GcRef;
use praxis_input_parser::{AtomicKind, ParserPlan, PlanNode};

/// Run the parser plan identified by `index` against `input`, returning the
/// parsed result or `None` on failure (out-of-range index → None; parse mismatch
/// → sets `ParseFailed` fault + None).
///
/// # Safety
/// `ctx` must be live and wired; `input` must be a valid `Text` GcRef.
pub unsafe fn run_plan_by_index(
    ctx: *mut RuntimeContext,
    index: u32,
    input: GcRef,
) -> Option<GcRef> {
    let plan = praxis_input_parser::get_plan(index)?;
    // SAFETY: caller guarantees ctx/input validity.
    Some(unsafe { run_plan(ctx, plan, input) })
}

/// Run a parser plan against an input buffer.
///
/// # Safety
/// `ctx` must be live and wired; `input` must be a valid `Text` GcRef.
unsafe fn run_plan(ctx: *mut RuntimeContext, plan: &ParserPlan, input: GcRef) -> GcRef {
    let payload = input.payload::<TextPayload>();
    let bytes = unsafe { text_bytes(payload) };
    let result = unsafe { walk(ctx, plan, plan.root, bytes, 0) };
    match result {
        Ok((value, _consumed)) => value,
        Err(_) => unsafe { fault_sentinel(ctx) },
    }
}

/// Set a `ParseFailed` fault and return the sentinel.
unsafe fn fault_sentinel(ctx: *mut RuntimeContext) -> GcRef {
    unsafe { set_parse_fault(ctx) };
    unsafe { (*ctx).unit_ref }
}

/// Mark a parse fault on the context.
unsafe fn set_parse_fault(ctx: *mut RuntimeContext) {
    let fault = unsafe { &mut *(*ctx).pending_fault };
    fault.set(crate::FaultKind::ParseFailed);
}

/// The outcome of walking a node: a value + the number of bytes consumed, or an
/// error (parse mismatch).
type WalkResult = Result<(GcRef, usize), ()>;

/// The runtime, extracted from the context for allocation calls.
struct Rt {
    ctx: *mut RuntimeContext,
}

/// Access the heap from the context (same-crate, so we read the raw pointer).
unsafe fn heap_ref<'a>(ctx: *mut RuntimeContext) -> &'a crate::Heap {
    // SAFETY: caller guarantees ctx is valid and wired.
    unsafe { &*(*ctx).heap }
}

impl Rt {
    /// Allocate a boxed `Int`.
    fn alloc_int(&self, value: i64) -> GcRef {
        // SAFETY: ctx is valid (caller upholds).
        unsafe { heap_ref(self.ctx).alloc(scalars::INT, value) }
    }

    /// Allocate a boxed `Char` from a Unicode scalar.
    fn alloc_char(&self, value: u32) -> GcRef {
        // SAFETY: ctx is valid.
        unsafe { heap_ref(self.ctx).alloc(scalars::CHAR, value) }
    }

    /// Allocate a source-slice `Text` pointing into `owner`.
    fn alloc_text_slice(&self, owner: GcRef, start: usize, len: usize) -> GcRef {
        let payload = TextPayload::Slice { owner, start, len };
        // SAFETY: ctx is valid; payload matches TEXT's layout.
        unsafe {
            heap_ref(self.ctx).alloc_with(
                crate::text::TEXT,
                std::mem::size_of::<TextPayload>(),
                std::mem::align_of::<TextPayload>(),
                |ptr| (ptr as *mut TextPayload).write(payload),
            )
        }
    }

    /// Allocate a `Vec` from element refs.
    fn alloc_vec(
        &self,
        element_descriptor: &'static crate::TypeDescriptor,
        items: Vec<GcRef>,
    ) -> GcRef {
        let payload = crate::collections::VecPayload {
            element_descriptor,
            items,
        };
        // SAFETY: ctx is valid.
        unsafe {
            heap_ref(self.ctx).alloc_with(
                crate::collections::VEC,
                std::mem::size_of::<crate::collections::VecPayload>(),
                std::mem::align_of::<crate::collections::VecPayload>(),
                |ptr| (ptr as *mut crate::collections::VecPayload).write(payload),
            )
        }
    }
}

/// Walk a plan node against `bytes` starting at `offset`, producing a value.
///
/// # Safety
/// `ctx` must be live and wired.
unsafe fn walk(
    ctx: *mut RuntimeContext,
    plan: &ParserPlan,
    node: u32,
    bytes: &[u8],
    offset: usize,
) -> WalkResult {
    let rt = Rt { ctx };
    let node = &plan.nodes[node as usize];
    match node {
        PlanNode::Atomic { kind } => walk_atomic(&rt, *kind, bytes, offset),
        PlanNode::Lines { child } => walk_lines(&rt, plan, *child, bytes, offset),
        PlanNode::Sections { child } => walk_sections(&rt, plan, *child, bytes, offset),
        PlanNode::Csv { child } => walk_csv(&rt, plan, *child, bytes, offset),
        PlanNode::Ws { child } => walk_ws(&rt, plan, *child, bytes, offset),
        PlanNode::Sep {
            separator_index,
            child,
        } => {
            let sep = plan.literals[*separator_index as usize];
            walk_sep(&rt, plan, *child, sep, bytes, offset)
        }
        PlanNode::Grid { child } => walk_grid(&rt, plan, *child, bytes, offset),
        PlanNode::Template { parts } => walk_template(&rt, plan, parts, bytes, offset),
        PlanNode::Tuple { elements } => walk_tuple(&rt, plan, elements, bytes, offset),
    }
}

// ---- atomics (§7.4) -------------------------------------------------------

fn walk_atomic(rt: &Rt, kind: AtomicKind, bytes: &[u8], offset: usize) -> WalkResult {
    let rest = &bytes[offset..];
    match kind {
        AtomicKind::Int => {
            // Parse a signed decimal integer. Skip leading whitespace.
            let s = trim_leading_ws(rest);
            let (digits, len) = take_int_run(s);
            if digits.is_empty() {
                return Err(());
            }
            let value: i64 = digits.parse().map_err(|_| ())?;
            Ok((rt.alloc_int(value), offset + (rest.len() - s.len()) + len))
        }
        AtomicKind::Digit => {
            let s = trim_leading_ws(rest);
            let Some(&b) = s.first() else {
                return Err(());
            };
            if !b.is_ascii_digit() {
                return Err(());
            }
            let value = (b - b'0') as i64;
            let consumed = rest.len() - s.len() + 1;
            Ok((rt.alloc_int(value), offset + consumed))
        }
        AtomicKind::Char => {
            // One Unicode scalar value. Decode the first char of the (trimmed)
            // remaining input.
            let s = trim_leading_ws(rest);
            let s_str = std::str::from_utf8(s).map_err(|_| ())?;
            let ch = s_str.chars().next().ok_or(())?;
            let consumed = rest.len() - s.len() + ch.len_utf8();
            Ok((rt.alloc_char(ch as u32), offset + consumed))
        }
        AtomicKind::Word => {
            let s = trim_leading_ws(rest);
            let (word, len) = take_word_run(s);
            if word.is_empty() {
                return Err(());
            }
            let leading = rest.len() - s.len();
            Ok((
                rt.alloc_text_slice(rt_owner(rt), offset + leading, len),
                offset + leading + len,
            ))
        }
        AtomicKind::Text | AtomicKind::Rest => {
            // `text`/`rest`: consume the remainder of the current region.
            // For a standalone atomic, the region is the whole remaining input.
            Ok((
                rt.alloc_text_slice(rt_owner(rt), offset, rest.len()),
                bytes.len(),
            ))
        }
    }
}

/// The owner GcRef for source-slice Texts (the original input buffer). Extracted
/// from the runtime context's `input_source`.
fn rt_owner(rt: &Rt) -> GcRef {
    // SAFETY: ctx is valid.
    unsafe { (*rt.ctx).input_source }
}

// ---- constructors (§7.5) --------------------------------------------------

fn walk_lines(rt: &Rt, plan: &ParserPlan, child: u32, bytes: &[u8], offset: usize) -> WalkResult {
    let region = &bytes[offset..];
    let mut items = Vec::new();
    for line in split_lines(region) {
        let line_offset = offset + line.start;
        let (value, _consumed) = unsafe { walk(rt.ctx, plan, child, bytes, line_offset)? };
        items.push(value);
    }
    let elem_desc = child_descriptor(plan, child);
    let vec_ref = rt.alloc_vec(elem_desc, items);
    Ok((vec_ref, bytes.len() - offset))
}

fn walk_sections(
    rt: &Rt,
    plan: &ParserPlan,
    child: u32,
    bytes: &[u8],
    offset: usize,
) -> WalkResult {
    let region = &bytes[offset..];
    let mut items = Vec::new();
    for section in split_sections(region) {
        let sec_offset = offset + section.start;
        let (value, _consumed) = unsafe { walk(rt.ctx, plan, child, bytes, sec_offset)? };
        items.push(value);
    }
    let elem_desc = child_descriptor(plan, child);
    Ok((rt.alloc_vec(elem_desc, items), bytes.len() - offset))
}

fn walk_csv(rt: &Rt, plan: &ParserPlan, child: u32, bytes: &[u8], offset: usize) -> WalkResult {
    let region = &bytes[offset..];
    let region_str = std::str::from_utf8(region).unwrap_or("");
    let mut items = Vec::new();
    for token in region_str.split(',') {
        let token_trimmed = token.trim();
        // Allocate a temporary owned Text for the token, then parse it.
        // The offset within the original buffer:
        let token_start = offset + region_offset_of(region, token_trimmed);
        let token_end = token_start + token_trimmed.len();
        // Create a sub-slice of the original bytes for this token.
        let (value, _) = unsafe { walk(rt.ctx, plan, child, bytes, token_start)? };
        let _ = token_end;
        items.push(value);
    }
    let elem_desc = child_descriptor(plan, child);
    Ok((rt.alloc_vec(elem_desc, items), bytes.len() - offset))
}

fn walk_ws(rt: &Rt, plan: &ParserPlan, child: u32, bytes: &[u8], offset: usize) -> WalkResult {
    let region = &bytes[offset..];
    let mut items = Vec::new();
    let mut pos = 0;
    while pos < region.len() {
        // Skip leading whitespace.
        while pos < region.len() && is_ws(region[pos]) {
            pos += 1;
        }
        if pos >= region.len() {
            break;
        }
        let token_start = pos;
        while pos < region.len() && !is_ws(region[pos]) {
            pos += 1;
        }
        let (value, _) = unsafe { walk(rt.ctx, plan, child, bytes, offset + token_start)? };
        items.push(value);
    }
    let elem_desc = child_descriptor(plan, child);
    Ok((rt.alloc_vec(elem_desc, items), bytes.len() - offset))
}

fn walk_sep(
    rt: &Rt,
    plan: &ParserPlan,
    child: u32,
    sep: &str,
    bytes: &[u8],
    offset: usize,
) -> WalkResult {
    let region = &bytes[offset..];
    let region_str = std::str::from_utf8(region).unwrap_or("");
    let sep_bytes = sep.as_bytes();
    let mut items = Vec::new();
    let mut token_start = 0usize;
    let mut pos = 0usize;
    while pos < region.len() {
        if region[pos..].starts_with(sep_bytes) {
            // Parse the token [token_start, pos).
            let (value, _) = unsafe { walk(rt.ctx, plan, child, bytes, offset + token_start)? };
            items.push(value);
            pos += sep_bytes.len();
            token_start = pos;
        } else {
            pos += 1;
        }
    }
    // Parse the final token.
    if (token_start < region.len() || !items.is_empty()) && token_start < region_str.len() {
        let (value, _) = unsafe { walk(rt.ctx, plan, child, bytes, offset + token_start)? };
        items.push(value);
    }
    let elem_desc = child_descriptor(plan, child);
    Ok((rt.alloc_vec(elem_desc, items), bytes.len() - offset))
}

fn walk_grid(rt: &Rt, plan: &ParserPlan, child: u32, bytes: &[u8], offset: usize) -> WalkResult {
    let region = &bytes[offset..];
    let lines: Vec<_> = split_lines(region).collect();
    let width = lines.first().map(|l| l.len).unwrap_or(0);
    let mut items = Vec::with_capacity(lines.len() * width);
    for line in &lines {
        let line_bytes = &region[line.start..line.start + line.len];
        if line_bytes.len() != width {
            // Grid rows must be uniform (§7.5). Ragged grids are M9.
            return Err(());
        }
        for (i, _) in line_bytes.iter().enumerate() {
            let cell_offset = offset + line.start + i;
            let (value, _) = unsafe { walk(rt.ctx, plan, child, bytes, cell_offset)? };
            items.push(value);
        }
    }
    let elem_desc = child_descriptor(plan, child);
    let payload = crate::collections::GridPayload {
        element_descriptor: elem_desc,
        items,
        width,
    };
    // SAFETY: ctx is valid.
    let grid_ref = unsafe {
        heap_ref(rt.ctx).alloc_with(
            crate::collections::GRID,
            std::mem::size_of::<crate::collections::GridPayload>(),
            std::mem::align_of::<crate::collections::GridPayload>(),
            |ptr| (ptr as *mut crate::collections::GridPayload).write(payload),
        )
    };
    Ok((grid_ref, bytes.len() - offset))
}

// ---- templates (§7.2, §7.3) -----------------------------------------------

fn walk_template(
    _rt: &Rt,
    _plan: &ParserPlan,
    _parts: &[praxis_input_parser::TemplatePartNode],
    _bytes: &[u8],
    _offset: usize,
) -> WalkResult {
    // Template matching is complex (literal matching + capture extraction).
    // For M6 v1, templates inside `lines()` are the common case and are handled
    // by the per-line child walk. A full template interpreter (matching literal
    // text between captures) is a follow-up within M6 or M9.
    Err(())
}

fn walk_tuple(
    _rt: &Rt,
    _plan: &ParserPlan,
    _elements: &[u32],
    _bytes: &[u8],
    _offset: usize,
) -> WalkResult {
    // Tuples materialize as part of template capture grouping (M6 follow-up).
    Err(())
}

// ---- byte-splitting helpers -----------------------------------------------

/// A byte range within a region.
struct ByteRange {
    start: usize,
    len: usize,
}

/// Split a byte region into logical lines (stripping trailing `\r` and `\n`).
fn split_lines(region: &[u8]) -> impl Iterator<Item = ByteRange> + '_ {
    let mut pos = 0;
    std::iter::from_fn(move || {
        if pos >= region.len() {
            return None;
        }
        let start = pos;
        while pos < region.len() && region[pos] != b'\n' {
            pos += 1;
        }
        let mut end = pos;
        // Strip a trailing `\r`.
        if end > start && region[end - 1] == b'\r' {
            end -= 1;
        }
        let len = end - start;
        if pos < region.len() {
            pos += 1; // skip `\n`
        }
        // Skip empty trailing line.
        if pos >= region.len() && len == 0 && start == region.len() {
            return None;
        }
        Some(ByteRange { start, len })
    })
}

/// Split a byte region on blank lines (one or more empty lines).
fn split_sections(region: &[u8]) -> impl Iterator<Item = ByteRange> + '_ {
    let mut pos = 0;
    std::iter::from_fn(move || {
        // Skip leading blank lines.
        while pos < region.len() {
            let line_start = pos;
            while pos < region.len() && region[pos] != b'\n' {
                pos += 1;
            }
            let line_end = if pos > line_start && region[pos - 1] == b'\r' {
                pos - 1
            } else {
                pos
            };
            if pos < region.len() {
                pos += 1;
            }
            if line_end > line_start {
                // Found a non-empty line; this section starts here.
                let section_start = line_start;
                // Consume until the next blank line (two consecutive newlines).
                while pos < region.len() {
                    // Check for blank line.
                    let peek = pos;
                    let mut p = peek;
                    while p < region.len() && region[p] != b'\n' {
                        p += 1;
                    }
                    let blank = p == peek || (p == peek + 1 && region[peek] == b'\r');
                    if blank {
                        // This is the end of the section (before the blank line).
                        return Some(ByteRange {
                            start: section_start,
                            len: peek - section_start,
                        });
                    }
                    if p < region.len() {
                        p += 1;
                    }
                    pos = p;
                }
                return Some(ByteRange {
                    start: section_start,
                    len: pos - section_start,
                });
            }
        }
        None
    })
}

/// The byte offset of `needle` within `hay`, used to locate CSV tokens.
fn region_offset_of(hay: &[u8], needle: &str) -> usize {
    let needle_bytes = needle.as_bytes();
    hay.windows(needle_bytes.len())
        .position(|w| w == needle_bytes)
        .unwrap_or(0)
}

/// Skip leading horizontal whitespace (spaces and tabs).
fn trim_leading_ws(bytes: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    &bytes[i..]
}

/// Take a run of integer characters (optional `-` + digits), returning the text
/// and the byte length consumed.
fn take_int_run(bytes: &[u8]) -> (&str, usize) {
    let mut end = 0;
    if end < bytes.len() && bytes[end] == b'-' {
        end += 1;
    }
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    // SAFETY: ASCII digits are valid UTF-8.
    let s = std::str::from_utf8(&bytes[..end]).unwrap_or("");
    (s, end)
}

/// Take a run of word characters (non-whitespace, non-delimiter).
fn take_word_run(bytes: &[u8]) -> (&str, usize) {
    let mut end = 0;
    while end < bytes.len()
        && !is_ws(bytes[end])
        && bytes[end] != b','
        && bytes[end] != b'\n'
        && bytes[end] != b'\r'
    {
        end += 1;
    }
    let s = std::str::from_utf8(&bytes[..end]).unwrap_or("");
    (s, end)
}

fn is_ws(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

/// Determine the element descriptor for a child plan node's result type. The
/// runtime maps atomic kinds to their GC descriptors.
fn child_descriptor(plan: &ParserPlan, child: u32) -> &'static crate::TypeDescriptor {
    // Walk the child to find its result atomic kind.
    match find_atomic_kind(plan, child) {
        Some(AtomicKind::Int) | Some(AtomicKind::Digit) => scalars::INT,
        Some(AtomicKind::Char) => scalars::CHAR,
        Some(AtomicKind::Word) | Some(AtomicKind::Text) | Some(AtomicKind::Rest) => {
            crate::text::TEXT
        }
        None => scalars::INT, // default for nested collections
    }
}

/// Recursively find the atomic kind at the leaf of a plan subtree.
fn find_atomic_kind(plan: &ParserPlan, node: u32) -> Option<AtomicKind> {
    match &plan.nodes[node as usize] {
        PlanNode::Atomic { kind } => Some(*kind),
        PlanNode::Lines { child }
        | PlanNode::Sections { child }
        | PlanNode::Csv { child }
        | PlanNode::Ws { child }
        | PlanNode::Grid { child } => find_atomic_kind(plan, *child),
        PlanNode::Sep { child, .. } => find_atomic_kind(plan, *child),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_module_present() {
        // The interpreter is exercised through the JIT integration tests.
    }

    #[test]
    fn split_lines_handles_crlf() {
        let bytes = b"abc\r\ndef\nghi";
        let lines: Vec<_> = split_lines(bytes)
            .map(|r| &bytes[r.start..r.start + r.len])
            .collect();
        assert_eq!(lines, vec![b"abc".as_slice(), b"def", b"ghi"]);
    }

    #[test]
    fn split_lines_drops_trailing_empty() {
        let bytes = b"a\nb\n";
        let lines: Vec<_> = split_lines(bytes)
            .map(|r| &bytes[r.start..r.start + r.len])
            .collect();
        assert_eq!(lines, vec![b"a".as_slice(), b"b"]);
    }

    #[test]
    fn split_sections_on_blank_lines() {
        let bytes = b"a\nb\n\nc\nd";
        let sections: Vec<_> = split_sections(bytes)
            .map(|r| &bytes[r.start..r.start + r.len])
            .collect();
        assert_eq!(sections.len(), 2);
    }

    #[test]
    fn take_int_run_parses_negative() {
        let (s, len) = take_int_run(b"-42abc");
        assert_eq!(s, "-42");
        assert_eq!(len, 3);
    }
}
