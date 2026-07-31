//! The runtime input-parser interpreter (§7, M6).
//!
//! Evaluates a compiled [`ParserPlan`] against the process-input buffer (or a
//! `Text` value), allocating GC results (`Int`, `Char`, source-slice `Text`,
//! `Vec`, `Grid`, `Record`) and raising `FaultKind::ParseFailed` on mismatch.
//!
//! The plan type and global arena live in `praxis-input-parser`; this
//! interpreter looks up a plan by its [`PlanId`] and walks its node arena.
//!
//! **The arena is not `#[repr(C)]`.** It is ordinary Rust enums and slices, and
//! nothing here crosses an FFI boundary — only the plan *id* is a JIT immediate.
//! `praxis_input_parser::plan`'s own doc is the authority; this line claimed a C
//! layout the types never had.

use crate::context::RuntimeContext;
use crate::parse_detail::ParseFail;
use crate::scalars;
use crate::text::{text_bytes, TextPayload};
use crate::GcRef;
use praxis_input_parser::{AtomicKind, ParserPlan, PlanNode};

/// Run the parser plan named by `raw_id` against `input`, returning the parsed
/// result or `None` on failure (a value that names no plan → `None`; parse
/// mismatch → sets `ParseFailed` fault + `None`).
///
/// `raw_id` arrives as the payload of a boxed `Int` — an `i64` the ABI cannot
/// constrain — so it is validated here rather than narrowed with an `as`. The
/// predecessor wrote `idx as u32`, which silently folded `0x1_0000_0005` onto
/// plan 5 and every negative onto a huge index (IP-12). Zero is rejected too:
/// [`PlanId`](praxis_input_parser::PlanId) is non-zero precisely so the HIR's
/// old failure sentinel cannot name a plan.
///
/// # Safety
/// `ctx` must be live and wired; `input` must be a valid `Text` GcRef.
pub unsafe fn run_plan_by_id(ctx: *mut RuntimeContext, raw_id: i64, input: GcRef) -> Option<GcRef> {
    let id = u32::try_from(raw_id)
        .ok()
        .and_then(praxis_input_parser::PlanId::from_raw)?;
    let plan = praxis_input_parser::get_plan(id)?;
    // SAFETY: caller guarantees ctx/input validity.
    Some(unsafe { run_plan(ctx, plan, input) })
}

/// Run a parser plan against an input buffer.
///
/// Clears the runtime's [`ParseDetail`] slot at the start so a stale failure
/// from a prior parse does not leak in; on a mismatch, the deepest failure is
/// recorded there (§7.11, M10-WS1) before the `ParseFailed` fault is raised.
///
/// # Safety
/// `ctx` must be live and wired; `input` must be a valid `Text` GcRef.
unsafe fn run_plan(ctx: *mut RuntimeContext, plan: &ParserPlan, input: GcRef) -> GcRef {
    let payload = input.payload::<TextPayload>();
    let bytes = unsafe { text_bytes(payload) };
    // Clear any stale detail from a prior parse, then run.
    unsafe { clear_parse_detail(ctx) };
    let result = unsafe { walk(ctx, plan, plan.root, bytes, 0) };
    match result {
        Ok((value, _consumed)) => value,
        Err(fail) => {
            // Record the deepest failure into the runtime's detail slot, then
            // raise the fault. The host reads the detail after `ParseFailed`.
            unsafe { record_fail(ctx, fail, bytes) };
            unsafe { fault_sentinel(ctx) }
        }
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
    fault.set(crate::context::RaisedFault::PARSE_FAILED);
}

/// Clear the runtime's [`ParseDetail`] slot at the start of a parse.
///
/// # Safety
/// `ctx` must be live and wired with a non-null `parse_detail`.
unsafe fn clear_parse_detail(ctx: *mut RuntimeContext) {
    if (*ctx).parse_detail.is_null() {
        return;
    }
    // SAFETY: caller guarantees parse_detail points at a live ParseDetail.
    unsafe { (*(*ctx).parse_detail).clear() };
}

/// Record a [`ParseFail`] into the runtime's [`ParseDetail`] slot, keeping the
/// deepest (most specific) failure (§7.11).
///
/// # Safety
/// `ctx` must be live and wired; `input` is the buffer the failure was against
/// (used for the actual-preview).
unsafe fn record_fail(ctx: *mut RuntimeContext, fail: ParseFail, input: &[u8]) {
    if (*ctx).parse_detail.is_null() {
        return;
    }
    // SAFETY: caller guarantees parse_detail points at a live ParseDetail.
    unsafe { (*(*ctx).parse_detail).consider(fail, input) };
}

/// The outcome of walking a node: a value + the number of bytes consumed, or an
/// error carrying the §7.11 structured detail. The deepest (highest-offset)
/// failure wins at the [`run_plan`] boundary; inner failures propagate up with
/// their already-specific detail, so an outer constructor only overrides when
/// it has *more* specific information (it generally does not).
type WalkResult = Result<(GcRef, usize), ParseFail>;

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
        unsafe { heap_ref(self.ctx).alloc_unpaced(scalars::INT_PAYLOAD, value) }
    }

    /// Allocate a boxed `Char` from a Unicode scalar.
    fn alloc_char(&self, value: u32) -> GcRef {
        // SAFETY: ctx is valid.
        unsafe { heap_ref(self.ctx).alloc_unpaced(scalars::CHAR_PAYLOAD, value) }
    }

    /// Allocate a boxed `Float` (§7.4's `float` atomic).
    fn alloc_float(&self, value: f64) -> GcRef {
        // SAFETY: ctx is valid.
        unsafe { heap_ref(self.ctx).alloc_unpaced(scalars::FLOAT_PAYLOAD, value) }
    }

    /// Allocate a boxed `Byte` (§7.4's `byte` atomic).
    fn alloc_byte(&self, value: u8) -> GcRef {
        // SAFETY: ctx is valid.
        unsafe { heap_ref(self.ctx).alloc_unpaced(scalars::BYTE_PAYLOAD, value) }
    }

    /// Allocate a source-slice `Text` pointing into `owner`, or `None` if the
    /// range is not a `Text` (RT-06).
    ///
    /// The parser computes its offsets from byte positions in the very buffer
    /// it is slicing, so `None` means the interpreter has a bug — but it must
    /// still surface as a parse fault rather than a panic across the ABI
    /// (§10.4), which is why this is fallible rather than an assert.
    fn alloc_text_slice(&self, owner: GcRef, start: usize, len: usize) -> Option<GcRef> {
        // SAFETY: `owner` is the context's input buffer, a live Text.
        let slice = unsafe { crate::text::SourceSlice::new(owner, start, len) }?;
        let payload = TextPayload::Slice(slice);
        // SAFETY: ctx is valid; payload matches TEXT's layout.
        Some(unsafe {
            heap_ref(self.ctx).alloc_with_unpaced(
                &crate::text::TEXT,
                std::mem::size_of::<TextPayload>(),
                std::mem::align_of::<TextPayload>(),
                |ptr| (ptr as *mut TextPayload).write(payload),
            )
        })
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
            heap_ref(self.ctx).alloc_with_unpaced(
                &crate::collections::VEC,
                std::mem::size_of::<crate::collections::VecPayload>(),
                std::mem::align_of::<crate::collections::VecPayload>(),
                |ptr| (ptr as *mut crate::collections::VecPayload).write(payload),
            )
        }
    }

    /// Allocate an enum value (M9): `schema` says which enum type it is, `tag`
    /// selects the variant, and `items` are the payload values. Matches the
    /// `EnumPayload` layout that codegen-produced `match` code expects (§4.6,
    /// M7). Used by `choice`/`optional`.
    fn alloc_enum(
        &self,
        schema: *const crate::enums::EnumSchema,
        tag: u32,
        items: Vec<GcRef>,
    ) -> GcRef {
        let payload = crate::enums::EnumPayload { schema, tag, items };
        // SAFETY: ctx is valid; payload matches ENUM's layout.
        unsafe {
            heap_ref(self.ctx).alloc_with_unpaced(
                &crate::enums::ENUM,
                std::mem::size_of::<crate::enums::EnumPayload>(),
                std::mem::align_of::<crate::enums::EnumPayload>(),
                |ptr| (ptr as *mut crate::enums::EnumPayload).write(payload),
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
        PlanNode::SectionsNamed {
            fields,
            repeated_tail,
        } => walk_sections_named(&rt, plan, fields, *repeated_tail, bytes, offset),
        PlanNode::Block { items } => walk_block(&rt, plan, items, bytes, offset),
        PlanNode::Choice { cases } => walk_choice(&rt, plan, cases, bytes, offset),
        PlanNode::Optional { child } => walk_optional(&rt, plan, *child, bytes, offset),
        PlanNode::Scan { child } => walk_scan(&rt, plan, *child, bytes, offset),
        PlanNode::OneOf { chars_index } => {
            let chars = plan.literals[*chars_index as usize];
            walk_one_of(&rt, chars, bytes, offset)
        }
        PlanNode::Characters { child, skip } => {
            walk_characters(&rt, plan, *child, *skip, bytes, offset)
        }
        PlanNode::Matrix { child } => walk_matrix(&rt, plan, *child, bytes, offset),
        PlanNode::GridRagged { child, fill_index } => {
            let fill = plan.literals[*fill_index as usize];
            walk_grid_ragged(&rt, plan, *child, fill, bytes, offset)
        }
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
                return Err(ParseFail::at(offset + (rest.len() - s.len()), 0, "int"));
            }
            let value: i64 = digits
                .parse()
                .map_err(|_| ParseFail::at(offset + (rest.len() - s.len()), len, "int"))?;
            Ok((rt.alloc_int(value), offset + (rest.len() - s.len()) + len))
        }
        AtomicKind::Digit => {
            let s = trim_leading_ws(rest);
            let Some(&b) = s.first() else {
                return Err(ParseFail::at(offset + (rest.len() - s.len()), 0, "digit"));
            };
            if !b.is_ascii_digit() {
                return Err(ParseFail::at(offset + (rest.len() - s.len()), 1, "digit"));
            }
            let value = (b - b'0') as i64;
            let consumed = rest.len() - s.len() + 1;
            Ok((rt.alloc_int(value), offset + consumed))
        }
        AtomicKind::Char => {
            // One Unicode scalar value. Decode the first char of the (trimmed)
            // remaining input.
            let s = trim_leading_ws(rest);
            let s_str =
                std::str::from_utf8(s).map_err(|_| ParseFail::at(offset, rest.len(), "char"))?;
            let ch = s_str
                .chars()
                .next()
                .ok_or_else(|| ParseFail::at(offset + (rest.len() - s.len()), 0, "char"))?;
            let consumed = rest.len() - s.len() + ch.len_utf8();
            Ok((rt.alloc_char(ch as u32), offset + consumed))
        }
        AtomicKind::Word => {
            let s = trim_leading_ws(rest);
            let (word, len) = take_word_run(s);
            if word.is_empty() {
                return Err(ParseFail::at(offset + (rest.len() - s.len()), 0, "word"));
            }
            let leading = rest.len() - s.len();
            let slice = rt
                .alloc_text_slice(rt_owner(rt), offset + leading, len)
                .ok_or_else(|| ParseFail::at(offset + leading, len, "word"))?;
            Ok((slice, offset + leading + len))
        }
        AtomicKind::UInt => {
            // §7.4's `uint`. Its **type** is `Int` (`ScalarType::UInt` is
            // reserved and has no runtime object); the non-negativity is this
            // rule: a leading `-` is not a `uint`, it is a parse failure.
            let s = trim_leading_ws(rest);
            let leading = rest.len() - s.len();
            if s.first() == Some(&b'-') {
                return Err(ParseFail::at(offset + leading, 1, "uint"));
            }
            let (digits, len) = take_int_run(s);
            if digits.is_empty() {
                return Err(ParseFail::at(offset + leading, 0, "uint"));
            }
            let value: i64 = digits
                .parse()
                .map_err(|_| ParseFail::at(offset + leading, len, "uint"))?;
            Ok((rt.alloc_int(value), offset + leading + len))
        }
        AtomicKind::Float => {
            let s = trim_leading_ws(rest);
            let leading = rest.len() - s.len();
            let (text, len) = take_float_run(s);
            if text.is_empty() {
                return Err(ParseFail::at(offset + leading, 0, "float"));
            }
            let value: f64 = text
                .parse()
                .map_err(|_| ParseFail::at(offset + leading, len, "float"))?;
            Ok((rt.alloc_float(value), offset + leading + len))
        }
        AtomicKind::Byte => {
            // A decimal integer in `0..=255`, not a raw input byte: a raw byte
            // cannot be re-sliced as `Text` without breaking the UTF-8
            // invariant every source-slice `Text` relies on.
            let s = trim_leading_ws(rest);
            let leading = rest.len() - s.len();
            let (digits, len) = take_int_run(s);
            if digits.is_empty() {
                return Err(ParseFail::at(offset + leading, 0, "byte"));
            }
            let value: u8 = digits
                .parse()
                .map_err(|_| ParseFail::at(offset + leading, len, "byte"))?;
            Ok((rt.alloc_byte(value), offset + leading + len))
        }
        AtomicKind::Identifier => {
            // §4.1's identifier class, not a local ASCII rule (F3). §7.4 says
            // "ASCII-like … by default"; accepting fewer names than the
            // language itself declares would be the narrower mistake.
            let s = trim_leading_ws(rest);
            let leading = rest.len() - s.len();
            let len = take_ident_run(s);
            if len == 0 {
                return Err(ParseFail::at(offset + leading, 0, "identifier"));
            }
            let slice = rt
                .alloc_text_slice(rt_owner(rt), offset + leading, len)
                .ok_or_else(|| ParseFail::at(offset + leading, len, "identifier"))?;
            Ok((slice, offset + leading + len))
        }
        AtomicKind::Text | AtomicKind::Rest => {
            // `text`/`rest`: consume the remainder of the current region.
            // For a standalone atomic, the region is the whole remaining input.
            let slice = rt
                .alloc_text_slice(rt_owner(rt), offset, rest.len())
                .ok_or_else(|| ParseFail::at(offset, rest.len(), "text"))?;
            Ok((slice, bytes.len()))
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
        // Parse each section against a bounded byte view (just that section's
        // bytes), so a child like `block(...)` or `lines(...)` consumes only the
        // section's content rather than running to the end of input.
        let sec_offset = offset + section.start;
        let sec_bytes = &bytes[sec_offset..sec_offset + section.len];
        let (value, _consumed) = unsafe { walk(rt.ctx, plan, child, sec_bytes, 0)? };
        items.push(value);
    }
    let elem_desc = child_descriptor(plan, child);
    Ok((rt.alloc_vec(elem_desc, items), bytes.len() - offset))
}

/// Walk named heterogeneous `sections(name: P, ..., tail: repeated(P))` (M9,
/// §7.5). The region is split on blank lines into sections; the first `N`
/// sections (where N = number of named fields, or fewer if a `repeated` tail is
/// present — it takes all the rest) are parsed by the named fields in order;
/// any remaining sections are parsed by the `repeated` tail into a `Vec`. The
/// result is an anonymous record assembled via [`alloc_record`].
fn walk_sections_named(
    rt: &Rt,
    plan: &ParserPlan,
    fields: &'static [(&'static str, u32)],
    repeated_tail: Option<(&'static str, u32)>,
    bytes: &[u8],
    offset: usize,
) -> WalkResult {
    let region = &bytes[offset..];
    let sections: Vec<ByteRange> = split_sections(region).collect();
    // Too few sections is a parse fault.
    let min_needed = fields.len();
    if sections.len() < min_needed {
        return Err(ParseFail::at(offset, region.len(), "section header"));
    }
    // Build the record captures: each named field parses its section, in order.
    // The repeated tail (if any) parses every remaining section into a Vec.
    //
    // Each section is parsed against a *bounded* byte view (just that section's
    // bytes), so a child like `lines(int)` consumes only the section's lines
    // rather than running to the end of input. The owner reference for any
    // source-slice Texts remains the original input owner (`rt_owner`), which
    // the child walks recover via `rt_owner(rt)` — but here we pass absolute
    // offsets into the full `bytes`, so source-slice Texts stay correct.
    let mut captures: Vec<(Option<&'static str>, u32, GcRef)> = Vec::new();
    for (i, (name, child)) in fields.iter().enumerate() {
        let sec = &sections[i];
        let sec_offset = offset + sec.start;
        let sec_bytes = &bytes[sec_offset..sec_offset + sec.len];
        let (value, _consumed) = unsafe { walk(rt.ctx, plan, *child, sec_bytes, 0)? };
        captures.push((Some(name), *child, value));
    }
    if let Some((tail_name, tail_child)) = repeated_tail {
        // The tail consumes every remaining section, parsed per-section by its
        // child into a Vec.
        let mut tail_items = Vec::new();
        for sec in &sections[fields.len()..] {
            let sec_offset = offset + sec.start;
            let sec_bytes = &bytes[sec_offset..sec_offset + sec.len];
            let (value, _consumed) = unsafe { walk(rt.ctx, plan, tail_child, sec_bytes, 0)? };
            tail_items.push(value);
        }
        let elem_desc = child_descriptor(plan, tail_child);
        let tail_vec = rt.alloc_vec(elem_desc, tail_items);
        // The tail field's "child" node for descriptor purposes is the tail
        // child; its value is the assembled Vec.
        captures.push((Some(tail_name), tail_child, tail_vec));
    }
    let record = alloc_record(rt, &captures);
    Ok((record, bytes.len() - offset))
}

/// Walk `block(item, ...)` (M9, §7.5): apply sequential parsers within one
/// region, advancing the cursor after each. A positional named-capture template
/// *flattens* its fields into the block record; a named item contributes one
/// field. The result is a flattened anonymous record assembled via
/// [`alloc_record`].
///
/// Cursor model: each item is walked against the remaining region from the
/// current cursor; the item's returned absolute offset becomes the next
/// cursor. `walk_template` returns the real position where matching stopped, so
/// a chain of line-anchored templates advances line by line.
fn walk_block(
    rt: &Rt,
    plan: &ParserPlan,
    items: &'static [praxis_input_parser::BlockItemNode],
    bytes: &[u8],
    offset: usize,
) -> WalkResult {
    let mut cursor = offset;
    // Captures collected as (name, child_node_for_descriptor, value). For a
    // flattened positional record, we expand its fields into separate entries.
    let mut captures: Vec<(Option<&'static str>, u32, GcRef)> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        // Before every item after the first, skip the line boundary: any run of
        // horizontal whitespace plus one newline (§7.5 block items are
        // line-anchored). The first item starts at the region head.
        if i > 0 {
            cursor = skip_line_boundary(bytes, cursor);
        }
        match item {
            praxis_input_parser::BlockItemNode::Positional { child } => {
                let (value, new_offset) = unsafe { walk(rt.ctx, plan, *child, bytes, cursor)? };
                cursor = new_offset;
                // If the positional produced a record (named-capture template),
                // flatten its fields into the block record. We detect a record
                // by pointer-equality of its descriptor against RECORD.
                if std::ptr::eq(value.descriptor(), &crate::records::RECORD) {
                    flatten_record_into(rt, value, &mut captures);
                }
                // A non-record positional (scalar) was rejected by validation
                // (I026); if we reach one here it contributes no field.
            }
            praxis_input_parser::BlockItemNode::Named { name, child } => {
                let (value, new_offset) = unsafe { walk(rt.ctx, plan, *child, bytes, cursor)? };
                cursor = new_offset;
                captures.push((Some(name), *child, value));
            }
        }
    }
    let record = alloc_record(rt, &captures);
    Ok((record, cursor))
}

/// Skip the line boundary between sequential `block` items (§7.5): any run of
/// horizontal whitespace, then an optional single line ending (`\n` or `\r\n`).
/// Returns the new cursor. If no line ending is present (e.g. the items are on
/// one line separated by spaces), only the horizontal whitespace is consumed.
fn skip_line_boundary(bytes: &[u8], mut cursor: usize) -> usize {
    // Horizontal whitespace (spaces/tabs).
    while cursor < bytes.len() && (bytes[cursor] == b' ' || bytes[cursor] == b'\t') {
        cursor += 1;
    }
    // One optional line ending.
    if cursor < bytes.len() && bytes[cursor] == b'\r' {
        cursor += 1;
    }
    if cursor < bytes.len() && bytes[cursor] == b'\n' {
        cursor += 1;
    }
    cursor
}

/// Flatten a positional record's fields into the block captures (§7.5
/// flattening). Reads the value's `RecordPayload` schema + items and pushes one
/// `(name, child_for_descriptor, value)` entry per field. The per-field
/// descriptor is read from the value's own header at record-format/eq/hash time,
/// so the `child` placeholder here is only a fallback tag.
fn flatten_record_into(
    _rt: &Rt,
    record_ref: GcRef,
    captures: &mut Vec<(Option<&'static str>, u32, GcRef)>,
) {
    let payload = record_ref.payload::<u8>() as *const crate::records::RecordPayload;
    // SAFETY: record_ref is a valid RECORD GcRef (descriptor checked by caller).
    let (schema, items) = unsafe {
        let p = &*payload;
        (p.schema, &p.items)
    };
    // SAFETY: schema is a valid leaked RecordSchema pointer.
    let schema = unsafe { &*schema };
    for (i, field) in schema.fields.iter().enumerate() {
        if let Some(value) = items.get(i) {
            captures.push((Some(field.name), u32::MAX, *value));
        }
    }
}

/// Walk `choice(Name: P, ...)` (M9, §7.5): try each case in source order from
/// the current offset. The first case whose parser succeeds wins; its value
/// becomes the variant's payload and the cursor advances to where that parser
/// stopped. If a case fails, the cursor is restored (backtracking) and the next
/// case is tried. If no case matches, this is a parse fault.
///
/// Backtracking note: a failed case may have allocated GC objects (since `walk`
/// allocates eagerly); those are unreferenced and collected later. Only the
/// cursor is restored — there is no allocator rollback, which is fine because
/// failed allocations are simply garbage.
fn walk_choice(
    rt: &Rt,
    plan: &ParserPlan,
    cases: &'static [(&'static str, u32)],
    bytes: &[u8],
    offset: usize,
) -> WalkResult {
    for (tag, (_name, child)) in cases.iter().enumerate() {
        match unsafe { walk(rt.ctx, plan, *child, bytes, offset) } {
            Ok((value, new_offset)) => {
                // First match wins. Tag with this case's index; the value is
                // the single payload slot.
                let schema = enum_schema_for(cases);
                let enum_ref = rt.alloc_enum(schema, tag as u32, vec![value]);
                return Ok((enum_ref, new_offset));
            }
            Err(_inner) => {
                // Backtrack: try the next case from the same offset. We discard
                // the inner failure here; if no case matches, the choice's own
                // failure below is the user-visible one. (Recording the deepest
                // inner failure across cases is a documented hardening
                // follow-up.)
                continue;
            }
        }
    }
    Err(ParseFail::at(offset, 0, "any choice case"))
}

/// Walk `optional(P)` (M9, §7.5): parse `P`; on success return `Some(value)`
/// (Option tag 0) advancing the cursor, on failure return `None` (tag 1) and
/// consume NO input (the cursor stays at `offset`). No fault is raised on a
/// miss — this is parser-level optionality, not exception recovery.
fn walk_optional(
    rt: &Rt,
    plan: &ParserPlan,
    child: u32,
    bytes: &[u8],
    offset: usize,
) -> WalkResult {
    match unsafe { walk(rt.ctx, plan, child, bytes, offset) } {
        Ok((value, new_offset)) => {
            let some_ref = rt.alloc_enum(crate::enums::option_schema(), 0, vec![value]);
            Ok((some_ref, new_offset))
        }
        Err(_) => {
            // Consume nothing; return None (tag 1, no payload). The inner
            // failure is intentionally swallowed — `optional` is parser-level
            // optionality, not exception recovery.
            let none_ref = rt.alloc_enum(crate::enums::option_schema(), 1, Vec::new());
            Ok((none_ref, offset))
        }
    }
}

/// Walk `scan(P)` (M9, §7.5): slide a cursor across the region; at each
/// position try `P`. On success, push the value and advance past the match
/// (so overlapping matches aren't found); on failure, advance one byte. All
/// unmatched text is ignored. Returns `Vec[result(P)]` in source order.
fn walk_scan(rt: &Rt, plan: &ParserPlan, child: u32, bytes: &[u8], offset: usize) -> WalkResult {
    let mut items = Vec::new();
    let mut cursor = offset;
    while cursor < bytes.len() {
        match unsafe { walk(rt.ctx, plan, child, bytes, cursor) } {
            Ok((value, new_offset)) => {
                // A match must advance the cursor (otherwise we'd loop forever
                // on a zero-width match). If it didn't, advance one byte.
                items.push(value);
                if new_offset > cursor {
                    cursor = new_offset;
                } else {
                    cursor += 1;
                }
            }
            Err(_) => {
                cursor += 1;
            }
        }
    }
    let elem_desc = child_descriptor(plan, child);
    Ok((rt.alloc_vec(elem_desc, items), bytes.len() - offset))
}

/// Walk `one_of("LR")` (M9, §7.5): match one character from a literal set.
fn walk_one_of(rt: &Rt, chars: &str, bytes: &[u8], offset: usize) -> WalkResult {
    let rest = &bytes[offset..];
    let s = trim_leading_ws(rest);
    let s_str = std::str::from_utf8(s).map_err(|_| ParseFail::at(offset, rest.len(), "char"))?;
    let ch = s_str
        .chars()
        .next()
        .ok_or_else(|| ParseFail::at(offset + (rest.len() - s.len()), 0, "char"))?;
    if !chars.contains(ch) {
        return Err(ParseFail::at(
            offset + (rest.len() - s.len()),
            ch.len_utf8(),
            format!("one of \"{chars}\""),
        ));
    }
    let consumed = rest.len() - s.len() + ch.len_utf8();
    Ok((rt.alloc_char(ch as u32), offset + consumed))
}

/// Walk `chars(P, skip:)` (M9, §7.5): apply a char-parser repeatedly, trimming
/// between matches per the skip policy. Result is `Vec[Char]`.
fn walk_characters(
    rt: &Rt,
    plan: &ParserPlan,
    child: u32,
    skip: praxis_input_parser::SkipPolicy,
    bytes: &[u8],
    offset: usize,
) -> WalkResult {
    let mut items = Vec::new();
    let mut cursor = offset;
    loop {
        cursor = skip_chars(bytes, cursor, skip);
        if cursor >= bytes.len() {
            break;
        }
        match unsafe { walk(rt.ctx, plan, child, bytes, cursor) } {
            Ok((value, new_offset)) => {
                if new_offset <= cursor {
                    cursor += 1;
                } else {
                    cursor = new_offset;
                }
                items.push(value);
            }
            Err(_) => break,
        }
    }
    Ok((rt.alloc_vec(&scalars::CHAR, items), bytes.len() - offset))
}

/// Skip bytes at `cursor` per the `chars` skip policy (§7.5).
fn skip_chars(bytes: &[u8], mut cursor: usize, skip: praxis_input_parser::SkipPolicy) -> usize {
    use praxis_input_parser::SkipPolicy;
    match skip {
        SkipPolicy::None => {}
        SkipPolicy::Whitespace => {
            while cursor < bytes.len() && (bytes[cursor] == b' ' || bytes[cursor] == b'\t') {
                cursor += 1;
            }
        }
        SkipPolicy::Newlines => {
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
        }
    }
    cursor
}

/// Walk `matrix(P)` (M9, §7.5, ADR-030): parse lines of whitespace-separated
/// tokens into a rectangular `Grid[result(P)]`. Each row must have the same
/// token count.
fn walk_matrix(rt: &Rt, plan: &ParserPlan, child: u32, bytes: &[u8], offset: usize) -> WalkResult {
    let region = &bytes[offset..];
    let region_str = std::str::from_utf8(region).unwrap_or("");
    let lines: Vec<&str> = region_str
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let width = lines
        .first()
        .map(|l| l.split_whitespace().count())
        .unwrap_or(0);
    let mut items = Vec::with_capacity(lines.len() * width);
    for line in &lines {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() != width {
            return Err(ParseFail::at(
                offset,
                region.len(),
                "rectangular matrix row",
            ));
        }
        for token in tokens {
            let token_bytes = token.as_bytes();
            let (value, _) = unsafe { walk(rt.ctx, plan, child, token_bytes, 0)? };
            items.push(value);
        }
    }
    let elem_desc = child_descriptor(plan, child);
    alloc_grid(rt, elem_desc, items, width, bytes.len() - offset)
}

/// Walk ragged `grid(P, ragged, fill:)` (M9, §7.5): permit uneven rows and pad
/// to the maximum width with the `fill` value (parsed by the cell parser).
fn walk_grid_ragged(
    rt: &Rt,
    plan: &ParserPlan,
    child: u32,
    fill: &str,
    bytes: &[u8],
    offset: usize,
) -> WalkResult {
    let region = &bytes[offset..];
    let lines: Vec<_> = split_lines(region).collect();
    let width = lines.iter().map(|l| l.len).max().unwrap_or(0);
    let mut items = Vec::with_capacity(lines.len() * width);
    let fill_bytes = fill.as_bytes();
    let (fill_value, _) = unsafe { walk(rt.ctx, plan, child, fill_bytes, 0)? };
    for line in &lines {
        let line_bytes = &region[line.start..line.start + line.len];
        for (i, _) in line_bytes.iter().enumerate() {
            let cell_offset = offset + line.start + i;
            let (value, _) = unsafe { walk(rt.ctx, plan, child, bytes, cell_offset)? };
            items.push(value);
        }
        for _ in line_bytes.len()..width {
            items.push(fill_value);
        }
    }
    let elem_desc = child_descriptor(plan, child);
    alloc_grid(rt, elem_desc, items, width, bytes.len() - offset)
}

/// Allocate a `Grid` from element refs + width (shared by grid/matrix/ragged).
/// `consumed` is the byte count the grid consumed (caller-supplied).
fn alloc_grid(
    rt: &Rt,
    elem_desc: &'static crate::TypeDescriptor,
    items: Vec<GcRef>,
    width: usize,
    consumed: usize,
) -> WalkResult {
    let payload = crate::collections::GridPayload {
        element_descriptor: elem_desc,
        items,
        width,
    };
    // SAFETY: ctx is valid.
    let grid_ref = unsafe {
        heap_ref(rt.ctx).alloc_with_unpaced(
            &crate::collections::GRID,
            std::mem::size_of::<crate::collections::GridPayload>(),
            std::mem::align_of::<crate::collections::GridPayload>(),
            |ptr| (ptr as *mut crate::collections::GridPayload).write(payload),
        )
    };
    Ok((grid_ref, consumed))
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
    // The loop below advances by `sep_bytes.len()` on a match, and
    // `starts_with(&[])` is unconditionally true — so an empty separator is an
    // infinite loop that allocates a value per iteration. The compiler makes
    // that unrepresentable (`praxis_input_parser::Separator`, IP-10); this
    // records what the loop is relying on.
    debug_assert!(
        !sep_bytes.is_empty(),
        "Separator::new refuses an empty separator (IP-10): the loop below cannot advance past one"
    );
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
            return Err(ParseFail::at(offset, region.len(), "uniform grid row"));
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
        heap_ref(rt.ctx).alloc_with_unpaced(
            &crate::collections::GRID,
            std::mem::size_of::<crate::collections::GridPayload>(),
            std::mem::align_of::<crate::collections::GridPayload>(),
            |ptr| (ptr as *mut crate::collections::GridPayload).write(payload),
        )
    };
    Ok((grid_ref, bytes.len() - offset))
}

// ---- templates (§7.2, §7.3) -----------------------------------------------

/// Interpret a backtick template against `bytes` from `offset` (§7.2, §7.3).
///
/// Walks the `parts` in order: a `Literal` part matches its bytes (honoring the
/// whitespace policy), a `Capture` part recursively walks its child parser to
/// extract one value. The result is assembled per §7.3:
/// - 0 captures → `Unit` (a pure literal match).
/// - 1 anonymous capture → the scalar value directly.
/// - any named capture → a `Record` (schema built at runtime from the capture
///   names + the child result descriptors).
///
/// Multi-anon-capture templates lower to a `Tuple` node (handled by
/// [`walk_tuple`]); this function never sees that case.
fn walk_template(
    rt: &Rt,
    plan: &ParserPlan,
    parts: &[praxis_input_parser::TemplatePartNode],
    bytes: &[u8],
    offset: usize,
) -> WalkResult {
    let mut cursor = offset;
    // Capture values in field-index order. Each entry is (name, child_node,
    // value): the child node is kept so a multi-anon-capture tuple can build its
    // TupleSchema from the child result descriptors.
    let mut captures: Vec<(Option<&'static str>, u32, GcRef)> = Vec::new();

    for part in parts {
        match part {
            praxis_input_parser::TemplatePartNode::Literal { text, ws } => {
                // Honor the whitespace policy before matching the literal.
                cursor = match consume_ws(bytes, cursor, *ws) {
                    None => {
                        return Err(ParseFail::at(cursor, 0, "whitespace"));
                    }
                    Some(c) => c,
                };
                // Match the literal bytes verbatim.
                let lit = text.as_bytes();
                if !bytes[cursor..].starts_with(lit) {
                    return Err(ParseFail::at(
                        cursor,
                        lit.len(),
                        format!("literal {:?}", text),
                    ));
                }
                cursor += lit.len();
            }
            praxis_input_parser::TemplatePartNode::Capture {
                child,
                field_index: _,
                name,
            } => {
                // Skip any flexible leading whitespace before a capture, then
                // walk the child parser to extract one value.
                cursor = match consume_ws(bytes, cursor, consume_ws_default()) {
                    None => {
                        return Err(ParseFail::at(cursor, 0, "whitespace"));
                    }
                    Some(c) => c,
                };
                // `walk` returns the *absolute* new offset (not a delta), so
                // assign rather than add.
                let (value, new_offset) = unsafe { walk(rt.ctx, plan, *child, bytes, cursor)? };
                cursor = new_offset;
                captures.push((*name, *child, value));
            }
        }
    }

    // Assemble the result per §7.3. Return the *real* cursor position (the
    // absolute offset where matching stopped) so a `block(...)` parent can
    // advance item-by-item (§7.5). Nothing in the M6/M7 tree consumed this
    // value, so changing from `bytes.len() - offset` to the true cursor is safe.
    let any_named = captures.iter().any(|(n, _, _)| n.is_some());
    if any_named {
        // Named captures → Record. Build the schema at runtime.
        Ok((alloc_record(rt, &captures), cursor))
    } else if captures.len() == 1 {
        // Single anonymous capture → the scalar value.
        Ok((captures.into_iter().next().unwrap().2, cursor))
    } else if captures.is_empty() {
        // No captures → Unit.
        Ok((alloc_unit(rt), cursor))
    } else {
        // Multiple anonymous captures → Tuple. Build the schema from the child
        // result descriptors and fill the payload with the captured values.
        let children: Vec<u32> = captures.iter().map(|(_, c, _)| *c).collect();
        let values: Vec<GcRef> = captures.into_iter().map(|(_, _, v)| v).collect();
        Ok((alloc_tuple(rt, &children, plan, values), cursor))
    }
}

/// Interpret a multi-anon-capture template lowered as a `Tuple` node (§7.3).
/// Each element is walked against successive sub-regions; the values are
/// assembled into a `Tuple`. The region is the whole remaining input split
/// across the captures by the literals' boundaries.
///
/// In practice the lowering emits a `Tuple` only for a bare `{a},{b}` template
/// (no surrounding literal context per capture), so each element parses the next
/// chunk of input greedily up to the following element's literal boundary. For
/// the M7 scope we walk each element against the full remaining region and
/// advance by the consumed amount.
fn walk_tuple(
    rt: &Rt,
    plan: &ParserPlan,
    elements: &[u32],
    bytes: &[u8],
    offset: usize,
) -> WalkResult {
    let mut cursor = offset;
    let mut values: Vec<GcRef> = Vec::with_capacity(elements.len());
    for &elem in elements {
        cursor = match consume_ws(bytes, cursor, consume_ws_default()) {
            None => {
                return Err(ParseFail::at(cursor, 0, "whitespace"));
            }
            Some(c) => c,
        };
        // `walk` returns the absolute new offset, not a delta.
        let (value, new_offset) = unsafe { walk(rt.ctx, plan, elem, bytes, cursor)? };
        cursor = new_offset;
        values.push(value);
    }
    let tuple_ref = alloc_tuple(rt, elements, plan, values);
    Ok((tuple_ref, bytes.len() - offset))
}

/// The default whitespace policy applied before a capture: a flexible run of
/// spaces/tabs (the §7.2 SpaceRun rule, so AoC column alignment works). This
/// matches `walk_atomic`'s `trim_leading_ws` behavior for atomics.
fn consume_ws_default() -> praxis_input_parser::WsPolicy {
    praxis_input_parser::WsPolicy::SpaceRun
}

/// Consume bytes at `cursor` per `ws`, returning the new cursor or `None` if the
/// policy is not satisfied (§7.2).
fn consume_ws(bytes: &[u8], cursor: usize, ws: praxis_input_parser::WsPolicy) -> Option<usize> {
    use praxis_input_parser::WsPolicy;
    let rest = bytes.get(cursor..)?;
    let mut i = 0;
    match ws {
        WsPolicy::SpaceRun => {
            // One or more spaces or tabs (flexible; §7.2 default).
            // Note: for a literal with leading SpaceRun we require ≥1; if the
            // literal is the very first part the run may be empty — handled by
            // allowing zero when cursor == 0. Practically, allow zero-or-more
            // here so templates starting at offset 0 match.
            while i < rest.len() && (rest[i] == b' ' || rest[i] == b'\t') {
                i += 1;
            }
        }
        WsPolicy::ZeroOrMore => {
            while i < rest.len() && (rest[i].is_ascii_whitespace()) {
                i += 1;
            }
        }
        WsPolicy::OneOrMore => {
            while i < rest.len() && rest[i].is_ascii_whitespace() {
                i += 1;
            }
            if i == 0 {
                return None;
            }
        }
        WsPolicy::ExactSpace => {
            if rest.first() == Some(&b' ') {
                i = 1;
            } else {
                return None;
            }
        }
        WsPolicy::Newline => {
            // Match `\n`, optionally preceded by `\r`.
            if rest.first() == Some(&b'\r') {
                i = 1;
            }
            if rest.get(i) == Some(&b'\n') {
                i += 1;
            } else {
                return None;
            }
        }
        WsPolicy::Tab => {
            if rest.first() == Some(&b'\t') {
                i = 1;
            } else {
                return None;
            }
        }
    }
    Some(cursor + i)
}

/// Allocate a `Unit` sentinel.
fn alloc_unit(rt: &Rt) -> GcRef {
    // SAFETY: ctx is valid.
    unsafe { (*rt.ctx).unit_ref }
}

/// Allocate a record from named captures (§7.3). Builds (and caches) the
/// `RecordSchema` from the capture names + the child result descriptors, leaks
/// it to `&'static`, and fills the payload with the captured values.
fn alloc_record(rt: &Rt, captures: &[(Option<&'static str>, u32, GcRef)]) -> GcRef {
    // Build the schema fields. Named captures only (the record case requires
    // every capture to have a name in well-formed input; anonymous ones in a
    // named template are a parser-validation concern, treated as `_` here).
    //
    // Each field's descriptor is taken from the CAPTURED VALUE's own header
    // (`value.descriptor()`). record_equals/format/hash dispatch through the
    // schema's per-field descriptor (records.rs), so it must match the value's
    // real type — hardcoding INT here miscompares/misformats/segsfaults on any
    // non-Int field (Text, Char, nested record, …) because the INT callback
    // reinterprets the foreign payload as an i64.
    let fields: Vec<crate::records::RecordField> = captures
        .iter()
        .map(|(name, _child, value)| crate::records::RecordField {
            name: name.unwrap_or("_"),
            descriptor: value.descriptor(),
        })
        .collect();
    let schema = record_schema_for(fields);
    let items: Vec<GcRef> = captures.iter().map(|(_, _, v)| *v).collect();
    let payload = crate::records::RecordPayload { schema, items };
    // SAFETY: ctx is valid; payload matches RECORD's layout.
    unsafe {
        heap_ref(rt.ctx).alloc_with_unpaced(
            &crate::records::RECORD,
            std::mem::size_of::<crate::records::RecordPayload>(),
            std::mem::align_of::<crate::records::RecordPayload>(),
            |ptr| (ptr as *mut crate::records::RecordPayload).write(payload),
        )
    }
}

/// Allocate a tuple from positional capture values (§7.3). Builds (and caches)
/// the `TupleSchema` from the element descriptors, leaks it to `&'static`, and
/// fills the payload.
fn alloc_tuple(rt: &Rt, elements: &[u32], plan: &ParserPlan, values: Vec<GcRef>) -> GcRef {
    let descriptors: Vec<*const crate::TypeDescriptor> = elements
        .iter()
        .map(|&e| child_descriptor(plan, e) as *const _)
        .collect();
    let schema = tuple_schema_for(descriptors);
    let payload = crate::tuples::TuplePayload {
        schema,
        items: values,
    };
    // SAFETY: ctx is valid; payload matches TUPLE's layout.
    unsafe {
        heap_ref(rt.ctx).alloc_with_unpaced(
            &crate::tuples::TUPLE,
            std::mem::size_of::<crate::tuples::TuplePayload>(),
            std::mem::align_of::<crate::tuples::TuplePayload>(),
            |ptr| (ptr as *mut crate::tuples::TuplePayload).write(payload),
        )
    }
}

// ---- parser-built schemas --------------------------------------------------
//
// A named-capture template produces an anonymous record, and an anonymous
// multi-capture template produces a tuple. Both need a schema, and the
// interpreter is the only thing that knows the field descriptors — it learns
// them from the values the child plans produced. So the schemas are built here,
// at runtime, and cached by shape so repeated parses of one template share one.
//
// **These entries own their storage** (IP-12). They used to be `Box::leak`ed,
// which was not merely a leak: a `RecordField::name` is a `&'static str`
// *borrowed from plan storage*, so a cache that outlives the plans holds
// dangling names. Owning them lets `retire` drop the schemas in the same breath
// as the plans, which is what makes reclaiming either one sound.

/// One cached record schema and everything it points at.
struct RecordSchemaEntry {
    /// `(field name, descriptor address)` — the shape this schema serves.
    key: Vec<(&'static str, usize)>,
    /// The fields the schema borrows. Boxed so the address is stable across the
    /// registry `Vec`'s reallocations. Never read directly.
    #[allow(dead_code)]
    fields: Box<[crate::records::RecordField]>,
    schema: Box<crate::records::RecordSchema>,
}

/// One cached tuple schema and everything it points at.
struct TupleSchemaEntry {
    /// The descriptor-address sequence this schema serves.
    key: Vec<usize>,
    /// The descriptors the schema borrows. See [`RecordSchemaEntry::fields`].
    #[allow(dead_code)]
    descriptors: Box<[*const crate::TypeDescriptor]>,
    schema: Box<crate::tuples::TupleSchema>,
}

/// One cached enum schema and everything it points at.
struct EnumSchemaEntry {
    /// The case-name sequence this schema serves.
    key: Vec<&'static str>,
    /// The variant shapes the schema borrows. See [`RecordSchemaEntry::fields`].
    #[allow(dead_code)]
    variants: Box<[crate::enums::EnumVariantShape]>,
    /// The one-slot payload arrays each variant shape borrows.
    #[allow(dead_code)]
    payloads: Box<[*const crate::TypeDescriptor]>,
    schema: Box<crate::enums::EnumSchema>,
}

/// The parser interpreter's schema cache.
#[derive(Default)]
struct ParserSchemas {
    records: Vec<RecordSchemaEntry>,
    tuples: Vec<TupleSchemaEntry>,
    enums: Vec<EnumSchemaEntry>,
}

// SAFETY: the entries hold raw `*const TypeDescriptor`s into process-static
// descriptor data and `&'static str`s into plan storage. Nothing is mutated
// after construction, and every access goes through the mutex below.
unsafe impl Send for ParserSchemas {}

static SCHEMAS: std::sync::Mutex<Option<ParserSchemas>> = std::sync::Mutex::new(None);

/// Drop every schema the parser interpreter has built.
///
/// # Safety
/// Every schema pointer handed out must be dead — no live `RecordPayload` or
/// `TuplePayload` may still name one. `retire_parser_plans` is the only
/// intended caller and holds the [`HeapDrained`](crate::HeapDrained) proof of
/// exactly that.
pub(crate) unsafe fn retire_schemas() {
    *SCHEMAS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// The `RecordSchema` for a template shape, built once and shared afterwards.
///
/// Cached by the `(field-name, descriptor)` sequence. The descriptor half is
/// load-bearing: two templates with the same field *names* but different
/// capture types (e.g. `{x:int}` vs `{x:word}`) must NOT share a schema —
/// `alloc_record` records each field's real descriptor, and
/// `record_equals`/`record_format`/`record_hash` dispatch through the schema's
/// per-field descriptor, so a name-only cache would hand the second template
/// the first template's descriptor and recompare/reformat via the wrong
/// callback (the same class of segfault the §6.1 `alloc_record` fix closed).
fn record_schema_for(
    fields: Vec<crate::records::RecordField>,
) -> *const crate::records::RecordSchema {
    let mut guard = SCHEMAS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cache = guard.get_or_insert_with(ParserSchemas::default);
    let key: Vec<(&'static str, usize)> = fields
        .iter()
        .map(|f| (f.name, f.descriptor as usize))
        .collect();
    if let Some(entry) = cache.records.iter().find(|e| e.key == key) {
        return &*entry.schema as *const _;
    }
    let fields: Box<[crate::records::RecordField]> = fields.into_boxed_slice();
    // SAFETY (lifetime erasure): `RecordSchema::fields` declares `&'static`, and
    // the slice lives in the boxed `fields` this entry owns. `retire_schemas`
    // is what discharges the obligation.
    let borrowed: &'static [crate::records::RecordField] =
        unsafe { &*(&*fields as *const [crate::records::RecordField]) };
    // A named-capture template produces an *anonymous* structural record
    // (§5.6): its identity is its shape, so two templates with the same fields
    // yield records that compare equal (RT-12).
    let schema = Box::new(crate::records::RecordSchema {
        identity: crate::records::SchemaIdentity::Anonymous,
        fields: borrowed,
    });
    let raw: *const crate::records::RecordSchema = &*schema;
    cache.records.push(RecordSchemaEntry {
        key,
        fields,
        schema,
    });
    raw
}

/// The `EnumSchema` for a `choice`'s case list, built once and shared
/// afterwards, so two parses of one template produce values that compare equal.
///
/// `choice(Name: P, …)` synthesizes an **anonymous** enum (§7.5,
/// `synthesize::ParserAst::Choice`), so its identity is its case-name shape and
/// the key is that sequence.
///
/// Every payload slot is **null** — unknown. The interpreter learns a case's
/// value type from the value the child plan produced, never from a static type,
/// and a null slot says exactly that: the value's own descriptor answers, and
/// it is read off the object's header, so it is never wrong. The arity is still
/// exact (one payload per case), which is what sizes the payload.
fn enum_schema_for(cases: &'static [(&'static str, u32)]) -> *const crate::enums::EnumSchema {
    let mut guard = SCHEMAS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cache = guard.get_or_insert_with(ParserSchemas::default);
    let key: Vec<&'static str> = cases.iter().map(|(name, _)| *name).collect();
    if let Some(entry) = cache.enums.iter().find(|e| e.key == key) {
        return &*entry.schema as *const _;
    }
    // One unknown slot per case, in one owned array the variant shapes borrow
    // disjoint single-element windows of.
    let payloads: Box<[*const crate::TypeDescriptor]> =
        vec![std::ptr::null(); cases.len()].into_boxed_slice();
    let variants: Box<[crate::enums::EnumVariantShape]> = key
        .iter()
        .enumerate()
        .map(|(i, name)| {
            // SAFETY (lifetime erasure): the window lives in the boxed
            // `payloads` this entry owns; `retire_schemas` discharges it.
            let slot: &'static [*const crate::TypeDescriptor] =
                unsafe { &*(&payloads[i..=i] as *const [*const crate::TypeDescriptor]) };
            crate::enums::EnumVariantShape {
                name,
                payload: slot,
            }
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();
    // SAFETY (lifetime erasure): as `record_schema_for`.
    let borrowed: &'static [crate::enums::EnumVariantShape] =
        unsafe { &*(&*variants as *const [crate::enums::EnumVariantShape]) };
    let schema = Box::new(crate::enums::EnumSchema {
        identity: crate::records::SchemaIdentity::Anonymous,
        variants: borrowed,
    });
    let raw: *const crate::enums::EnumSchema = &*schema;
    cache.enums.push(EnumSchemaEntry {
        key,
        variants,
        payloads,
        schema,
    });
    raw
}

/// The `TupleSchema` for a descriptor sequence, built once and shared
/// afterwards, so same-shaped tuples compare structurally equal.
fn tuple_schema_for(
    descriptors: Vec<*const crate::TypeDescriptor>,
) -> *const crate::tuples::TupleSchema {
    let mut guard = SCHEMAS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cache = guard.get_or_insert_with(ParserSchemas::default);
    let key: Vec<usize> = descriptors.iter().map(|p| *p as usize).collect();
    if let Some(entry) = cache.tuples.iter().find(|e| e.key == key) {
        return &*entry.schema as *const _;
    }
    let descriptors: Box<[*const crate::TypeDescriptor]> = descriptors.into_boxed_slice();
    // SAFETY (lifetime erasure): as `record_schema_for`.
    let borrowed: &'static [*const crate::TypeDescriptor] =
        unsafe { &*(&*descriptors as *const [*const crate::TypeDescriptor]) };
    let schema = Box::new(crate::tuples::TupleSchema {
        descriptors: borrowed,
    });
    let raw: *const crate::tuples::TupleSchema = &*schema;
    cache.tuples.push(TupleSchemaEntry {
        key,
        descriptors,
        schema,
    });
    raw
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

/// Take a run of decimal floating-point characters (optional `-`, digits, an
/// optional `.` and fraction, an optional `e±NN` exponent), returning the text
/// and the byte length consumed (§7.4 `float`).
fn take_float_run(bytes: &[u8]) -> (&str, usize) {
    let mut end = 0;
    if end < bytes.len() && (bytes[end] == b'-' || bytes[end] == b'+') {
        end += 1;
    }
    let int_start = end;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    let mut saw_digit = end > int_start;
    if end < bytes.len() && bytes[end] == b'.' {
        let after_dot = end + 1;
        let mut frac = after_dot;
        while frac < bytes.len() && bytes[frac].is_ascii_digit() {
            frac += 1;
        }
        // A trailing `.` with no fraction is not part of the number: `1.` in
        // `1.` is a `1` followed by a literal `.` the template may need.
        if frac > after_dot {
            saw_digit = true;
            end = frac;
        }
    }
    if !saw_digit {
        return ("", 0);
    }
    // An exponent only counts if it is complete; `1e` is `1` followed by `e`.
    if end < bytes.len() && (bytes[end] == b'e' || bytes[end] == b'E') {
        let mut exp = end + 1;
        if exp < bytes.len() && (bytes[exp] == b'-' || bytes[exp] == b'+') {
            exp += 1;
        }
        let digits_start = exp;
        while exp < bytes.len() && bytes[exp].is_ascii_digit() {
            exp += 1;
        }
        if exp > digits_start {
            end = exp;
        }
    }
    // SAFETY: every byte accepted above is ASCII.
    let s = std::str::from_utf8(&bytes[..end]).unwrap_or("");
    (s, end)
}

/// Take an identifier run under §4.1's **one** character class, returning the
/// byte length consumed (§7.4 `identifier`).
///
/// Zero if the run does not start one. Invalid UTF-8 simply ends the run —
/// `identifier` produces a source-slice `Text`, whose invariant is that its
/// bytes are valid UTF-8.
fn take_ident_run(bytes: &[u8]) -> usize {
    // Scan as far as the input decodes; a bad byte simply ends the run.
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => std::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or_default(),
    };
    let mut chars = s.char_indices();
    let Some((_, first)) = chars.next() else {
        return 0;
    };
    if !praxis_syntax::ident::is_ident_start(first) {
        return 0;
    }
    let mut end = first.len_utf8();
    for (i, c) in chars {
        if praxis_syntax::ident::is_ident_continue(c) {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    end
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

/// Determine the element descriptor for a child plan node's *result* type. A
/// constructor `lines(P)` produces `Vec[result(P)]`, so its element descriptor is
/// the descriptor of `result(P)`.
///
/// Because the collection descriptors (`VEC`, `GRID`) are **uniform** — the
/// per-instance element type lives in the payload, not the descriptor — a nested
/// constructor's result descriptor is just `VEC`/`GRID` regardless of how deep
/// the nesting goes. The payload chain carries the inner element types, so
/// `vec_format`/`vec_equals`/`vec_hash` recurse correctly through it. (The prior
/// implementation collapsed the whole subtree to its leaf atomic and returned
/// that scalar, mis-tagging every intermediate Vec/Grid — a silent mis-dispatch
/// in any nested-collection format/eq/hash.)
fn child_descriptor(plan: &ParserPlan, child: u32) -> &'static crate::TypeDescriptor {
    match &plan.nodes[child as usize] {
        // Atomics produce their scalar.
        PlanNode::Atomic { kind } => atomic_descriptor(*kind),
        // Collection constructors produce a Vec (lines/sections/csv/ws/sep) or a
        // Grid. Uniform descriptors — the element type is in the payload.
        PlanNode::Lines { .. }
        | PlanNode::Sections { .. }
        | PlanNode::Csv { .. }
        | PlanNode::Ws { .. }
        | PlanNode::Sep { .. }
        | PlanNode::Scan { .. } => &crate::collections::VEC,
        PlanNode::Grid { .. } => &crate::collections::GRID,
        // Named sections produce an anonymous record (uniform descriptor; the
        // schema is in the payload, built at runtime by `walk_sections_named`).
        PlanNode::SectionsNamed { .. } => &crate::records::RECORD,
        // A block produces a flattened anonymous record (uniform descriptor).
        PlanNode::Block { .. } => &crate::records::RECORD,
        // choice/optional produce an enum (uniform descriptor; tag + payload).
        PlanNode::Choice { .. } | PlanNode::Optional { .. } => &crate::enums::ENUM,
        // one_of produces a Char; chars produces a Vec[Char].
        PlanNode::OneOf { .. } => &scalars::CHAR,
        PlanNode::Characters { .. } => &crate::collections::VEC,
        // matrix / ragged grid produce a Grid.
        PlanNode::Matrix { .. } | PlanNode::GridRagged { .. } => &crate::collections::GRID,
        // A template's result is a scalar (single anon capture), a record (named
        // captures), or Unit (no captures). A tuple's result is a tuple. These
        // are uniform descriptors too (schema in the payload).
        PlanNode::Template { parts } => template_result_descriptor(parts),
        PlanNode::Tuple { .. } => &crate::tuples::TUPLE,
    }
}

/// The scalar descriptor for an atomic kind.
fn atomic_descriptor(kind: AtomicKind) -> &'static crate::TypeDescriptor {
    match kind {
        // `uint` is an `Int` at runtime as well as in the type (§7.4, IP-11):
        // `ScalarType::UInt` has no runtime object to describe.
        AtomicKind::Int | AtomicKind::UInt | AtomicKind::Digit => &scalars::INT,
        AtomicKind::Float => &scalars::FLOAT,
        AtomicKind::Byte => &scalars::BYTE,
        AtomicKind::Char => &scalars::CHAR,
        AtomicKind::Word | AtomicKind::Identifier | AtomicKind::Text | AtomicKind::Rest => {
            &crate::text::TEXT
        }
    }
}

/// The descriptor of a template's *result*: a scalar if it has exactly one
/// anonymous capture, a record if it has named captures, Unit if none. (A
/// multi-anon-capture template lowers to a `Tuple` node, handled above.)
fn template_result_descriptor(
    parts: &[praxis_input_parser::TemplatePartNode],
) -> &'static crate::TypeDescriptor {
    let mut captures = 0usize;
    let mut any_named = false;
    for p in parts {
        if let praxis_input_parser::TemplatePartNode::Capture { name, .. } = p {
            captures += 1;
            if name.is_some() {
                any_named = true;
            }
        }
    }
    if any_named {
        &crate::records::RECORD
    } else if captures == 1 {
        // Single anonymous capture → scalar. The exact scalar descriptor depends
        // on the capture's child, but for descriptor-table purposes the element
        // is a GC value; the per-value descriptor is read from the value's own
        // header at trace/format/eq/hash time. Returning a generic GC scalar
        // descriptor here would be more precise, but the collection wrappers
        // (vec_equals etc.) dispatch through the value's own descriptor, so this
        // is only consulted for the collection's *element* tag. Use INT as a
        // sound default (the value's real descriptor governs tracing).
        &scalars::INT
    } else {
        &scalars::UNIT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_plan(nodes: Vec<PlanNode>, root: u32) -> ParserPlan {
        ParserPlan {
            nodes: Box::leak(nodes.into_boxed_slice()),
            template_parts: &[],
            literals: &[],
            root,
        }
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

    /// **IP-11.** §7.4 lists ten atomic parsers and four of them did not exist:
    /// `uint`, `float`, `byte`, `identifier`. This is the runtime half — every
    /// kind parses something and has a descriptor, and the four new rules mean
    /// what §7.4 says they mean.
    ///
    /// The type half is in `praxis-input-parser`'s `synthesize`; the closed-set
    /// half is `atomic_round_trips_keywords` in its `ast.rs`.
    #[test]
    fn every_atomic_the_design_requires_has_a_parser_and_a_type() {
        /// Parse `input` with one atomic and return the consumed length, or
        /// `None` on a parse failure.
        fn parse_one(kind: AtomicKind, input: &str) -> Option<(crate::Runtime, GcRef, usize)> {
            let mut rt = crate::Runtime::new();
            let text = rt.alloc_text(input);
            let mut ctx = rt.context();
            ctx.input_source = text;
            let plan = test_plan(vec![PlanNode::Atomic { kind }], 0);
            let bytes = text.as_text().as_bytes();
            let out = unsafe { walk(&mut ctx, &plan, plan.root, bytes, 0) };
            out.ok().map(|(v, consumed)| (rt, v, consumed))
        }

        // Every kind has a descriptor. Exhaustive by `ALL`, so a new atomic
        // cannot be added without one.
        for kind in AtomicKind::ALL {
            let _ = atomic_descriptor(*kind);
        }

        // `uint` is an Int and refuses a leading `-` — the non-negativity is
        // the parse rule, because `ScalarType::UInt` has no runtime object.
        let (_rt, v, consumed) = parse_one(AtomicKind::UInt, "42rest").expect("uint reads 42");
        assert_eq!(v.as_int(), 42);
        assert_eq!(consumed, 2);
        assert!(
            parse_one(AtomicKind::UInt, "-1").is_none(),
            "`uint` refuses a negative"
        );
        // …and `int` still accepts it, so the two are different rules.
        let (_rt, v, _) = parse_one(AtomicKind::Int, "-1").expect("int reads -1");
        assert_eq!(v.as_int(), -1);

        // `float`.
        for (input, expected, consumed) in [
            ("3.5", 3.5_f64, 3),
            ("-0.25x", -0.25, 5),
            ("2", 2.0, 1),
            ("1e3", 1000.0, 3),
            ("1.5e-2", 0.015, 6),
            // A trailing `.` is not part of the number: the template may need it.
            ("7.", 7.0, 1),
        ] {
            let (_rt, v, got) = parse_one(AtomicKind::Float, input)
                .unwrap_or_else(|| panic!("float reads {input}"));
            assert_eq!(v.as_float(), expected, "for {input}");
            assert_eq!(got, consumed, "for {input}");
        }
        assert!(parse_one(AtomicKind::Float, "x").is_none());

        // `byte` is a decimal integer in 0..=255 — not a raw input byte, which
        // could not be re-sliced as Text without breaking the UTF-8 invariant.
        let (_rt, v, _) = parse_one(AtomicKind::Byte, "255").expect("byte reads 255");
        assert_eq!(v.as_byte(), 255);
        assert!(
            parse_one(AtomicKind::Byte, "256").is_none(),
            "256 is not a byte"
        );
        assert!(
            parse_one(AtomicKind::Byte, "-1").is_none(),
            "-1 is not a byte"
        );

        // `identifier` uses §4.1's one class, so a Unicode name is a name, and
        // the run stops where an identifier stops.
        for (input, expected) in [
            ("name rest", "name"),
            ("λx-1", "λx"),
            ("_x9=2", "_x9"),
            ("日本語:", "日本語"),
        ] {
            let (_rt, v, _) = parse_one(AtomicKind::Identifier, input)
                .unwrap_or_else(|| panic!("identifier reads {input}"));
            assert_eq!(v.as_text(), expected, "for {input}");
        }
        assert!(
            parse_one(AtomicKind::Identifier, "9x").is_none(),
            "a digit does not start an identifier"
        );
    }

    #[test]
    #[ignore = "known bug: bounded section parsing loses the source's absolute offset"]
    fn text_slices_in_later_sections_point_at_their_actual_source_bytes() {
        let mut rt = crate::Runtime::new();
        let input = rt.alloc_text("first\n\nsecond");
        let mut ctx = rt.context();
        ctx.input_source = input;
        let plan = test_plan(
            vec![
                PlanNode::Atomic {
                    kind: AtomicKind::Word,
                },
                PlanNode::Sections { child: 0 },
            ],
            1,
        );

        let (result, _) =
            unsafe { walk(&mut ctx, &plan, plan.root, input.as_text().as_bytes(), 0) }
                .expect("sections(word) should parse");
        let values: Vec<&str> = result.as_vec().iter().map(GcRef::as_text).collect();

        assert_eq!(values, vec!["first", "second"]);
    }

    #[test]
    #[ignore = "known bug: Grid width/iteration count UTF-8 bytes rather than scalar values"]
    fn unicode_grid_cells_are_parsed_once_per_scalar() {
        let mut rt = crate::Runtime::new();
        let input = rt.alloc_text("é");
        let mut ctx = rt.context();
        ctx.input_source = input;
        let plan = test_plan(
            vec![
                PlanNode::Atomic {
                    kind: AtomicKind::Char,
                },
                PlanNode::Grid { child: 0 },
            ],
            1,
        );

        let (grid, _) = unsafe { walk(&mut ctx, &plan, plan.root, input.as_text().as_bytes(), 0) }
            .expect("one Unicode scalar is one valid grid cell");
        let payload = unsafe { &*grid.payload::<crate::collections::GridPayload>() };

        assert_eq!(payload.width, 1);
        assert_eq!(payload.items.len(), 1);
    }

    #[test]
    #[ignore = "known bug: csv does not bound child parsers to an individual token"]
    fn csv_rest_parser_is_bounded_to_each_token() {
        let mut rt = crate::Runtime::new();
        let input = rt.alloc_text("a,b");
        let mut ctx = rt.context();
        ctx.input_source = input;
        let plan = test_plan(
            vec![
                PlanNode::Atomic {
                    kind: AtomicKind::Rest,
                },
                PlanNode::Csv { child: 0 },
            ],
            1,
        );

        let (result, _) =
            unsafe { walk(&mut ctx, &plan, plan.root, input.as_text().as_bytes(), 0) }
                .expect("csv(rest) should parse");
        let values: Vec<&str> = result.as_vec().iter().map(GcRef::as_text).collect();

        assert_eq!(values, vec!["a", "b"]);
    }

    // --- M7-WS9: whitespace matcher (§7.2) -----------------------------------

    #[test]
    #[ignore = "known bug: SpaceRun currently accepts an empty run"]
    fn consume_ws_space_run_requires_one_or_more_spaces_or_tabs() {
        use praxis_input_parser::WsPolicy;
        assert_eq!(consume_ws(b"  ,x", 0, WsPolicy::SpaceRun), Some(2));
        assert_eq!(consume_ws(b"\t\t,x", 0, WsPolicy::SpaceRun), Some(2));
        assert_eq!(
            consume_ws(b"x", 0, WsPolicy::SpaceRun),
            None,
            "SpaceRun is the one-or-more policy; absence of whitespace must not match"
        );
    }

    #[test]
    fn consume_ws_one_or_more_requires_at_least_one() {
        use praxis_input_parser::WsPolicy;
        assert_eq!(consume_ws(b"  x", 0, WsPolicy::OneOrMore), Some(2));
        assert_eq!(consume_ws(b"x", 0, WsPolicy::OneOrMore), None);
    }

    #[test]
    fn consume_ws_exact_space_matches_one() {
        use praxis_input_parser::WsPolicy;
        assert_eq!(consume_ws(b" x", 0, WsPolicy::ExactSpace), Some(1));
        assert_eq!(consume_ws(b"\tx", 0, WsPolicy::ExactSpace), None);
    }

    #[test]
    fn consume_ws_newline_matches_crlf_and_lf() {
        use praxis_input_parser::WsPolicy;
        assert_eq!(consume_ws(b"\r\nx", 0, WsPolicy::Newline), Some(2));
        assert_eq!(consume_ws(b"\nx", 0, WsPolicy::Newline), Some(1));
        assert_eq!(consume_ws(b"x", 0, WsPolicy::Newline), None);
    }

    #[test]
    #[ignore = "known bug: single-capture templates hard-code the Int descriptor"]
    fn single_anonymous_template_capture_uses_its_child_descriptor() {
        let parts: &'static [praxis_input_parser::TemplatePartNode] = Box::leak(
            vec![praxis_input_parser::TemplatePartNode::Capture {
                child: 0,
                field_index: None,
                name: None,
            }]
            .into_boxed_slice(),
        );
        let nodes: &'static [PlanNode] = Box::leak(
            vec![
                PlanNode::Atomic {
                    kind: AtomicKind::Word,
                },
                PlanNode::Template { parts },
            ]
            .into_boxed_slice(),
        );
        let plan = ParserPlan {
            nodes,
            template_parts: &[],
            literals: &[],
            root: 1,
        };

        assert_eq!(
            child_descriptor(&plan, plan.root).id(),
            crate::text::TEXT.id(),
            "lines(`{{word}}`) must carry Text as its Vec element descriptor"
        );
    }
}
