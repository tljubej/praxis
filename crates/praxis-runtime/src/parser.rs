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

mod cursor;

use crate::context::RuntimeContext;
use crate::parse_detail::ParseFail;
use crate::scalars;
use crate::text::TextPayload;
use crate::GcRef;
use cursor::{split_lines, split_sections, ByteRegion, Cursor, Input, Walked};
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
    // The buffer **and its owner** both come from the `input` argument. They
    // used to come from two places — the bytes from here, the owner from
    // `ctx.input_source` — so `parse(text, P)` produced `Text` values that were
    // views of the stdin buffer at the offsets of a different string (IPR-03).
    // SAFETY: the caller guarantees `input` is a valid Text GcRef.
    let Some(i) = (unsafe { Input::new(input) }) else {
        unsafe { clear_parse_detail(ctx) };
        return unsafe { fault_sentinel(ctx) };
    };
    let region = i.whole();
    // Clear any stale detail from a prior parse, then run.
    unsafe { clear_parse_detail(ctx) };
    let result = unsafe { walk(ctx, &i, plan, plan.root, region) };
    match result {
        // The root does **not** require exhaustion. Every real input ends with
        // a newline (`praxis-cli`'s runner reads the file verbatim), so a root
        // that demanded its region be consumed would fault on every file in the
        // corpus. Exhaustion is a *parent's* decision, made by `walk_exact`.
        Ok(walked) => walked.value,
        Err(fail) => {
            // Record the deepest failure into the runtime's detail slot, then
            // raise the fault. The host reads the detail after `ParseFailed`.
            unsafe { record_fail(ctx, fail, region.bytes(&i)) };
            unsafe { fault_sentinel(ctx) }
        }
    }
}

/// Run `plan`'s root against `input` and hand back the value or the failure,
/// with no fault raised and no detail recorded.
///
/// The interpreter's own unit tests used to call `walk` directly with a
/// `(bytes, offset)` pair. That pair is exactly what this stage deleted, and
/// the tests are gates for defects this stage closes, so they get a root entry
/// rather than a rewrite against internals or a deletion.
#[cfg(test)]
unsafe fn run_root(
    ctx: *mut RuntimeContext,
    plan: &ParserPlan,
    input: GcRef,
) -> Result<GcRef, ParseFail> {
    // SAFETY: the caller guarantees `input` is a valid Text GcRef.
    let i = unsafe { Input::new(input) }.expect("the test's input is a Text");
    let region = i.whole();
    // SAFETY: the caller guarantees ctx is live and wired.
    unsafe { walk(ctx, &i, plan, plan.root, region) }.map(|w| w.value)
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

/// The outcome of walking a node: a value + **the absolute position parsing
/// stopped at**, or an error carrying the §7.11 structured detail. The deepest
/// (highest-offset) failure wins at the [`run_plan`] boundary; inner failures
/// propagate up with their already-specific detail, so an outer constructor
/// only overrides when it has *more* specific information (it generally does
/// not).
type WalkResult = Result<Walked, ParseFail>;

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

    /// Allocate an **owned** `Text` holding a copy of `s`.
    ///
    /// Used only for a ragged grid's `fill` literal, which lives in plan
    /// storage rather than in the input. Giving it a `Text` of its own is what
    /// lets the cell parser slice it: the predecessor walked the fill's bytes
    /// while allocating slices against the *input*, so a `Text` fill cell named
    /// input bytes chosen by the fill's length (IPR-03).
    fn alloc_text_owned(&self, s: &str) -> GcRef {
        let payload = TextPayload::Owned(s.into());
        // SAFETY: ctx is valid; payload matches TEXT's layout.
        unsafe {
            heap_ref(self.ctx).alloc_with_unpaced(
                &crate::text::TEXT,
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

/// Walk a plan node against `region`, producing a value and the absolute
/// position where matching stopped.
///
/// The node begins at `region.start()` and may not read past `region.end()`.
/// Whether it must *reach* `region.end()` is the parent's decision, made by
/// [`walk_exact`]: `lines` requires it of each line, `scan` does not require it
/// of a match. That is one rule in one place, which is what makes
/// `scan(choice(…))` and `lines(choice(…))` both correct without `choice`
/// itself having a policy.
///
/// # Safety
/// `ctx` must be live and wired.
unsafe fn walk(
    ctx: *mut RuntimeContext,
    i: &Input<'_>,
    plan: &ParserPlan,
    node: u32,
    region: ByteRegion,
) -> WalkResult {
    let rt = Rt { ctx };
    let node = &plan.nodes[node as usize];
    match node {
        PlanNode::Atomic { kind } => walk_atomic(&rt, i, *kind, region),
        PlanNode::Lines { child } => walk_lines(&rt, i, plan, *child, region),
        PlanNode::Sections { child } => walk_sections(&rt, i, plan, *child, region),
        PlanNode::SectionsNamed {
            fields,
            repeated_tail,
        } => walk_sections_named(&rt, i, plan, fields, *repeated_tail, region),
        PlanNode::Block { items } => walk_block(&rt, i, plan, items, region),
        PlanNode::Choice { cases } => walk_choice(&rt, i, plan, cases, region),
        PlanNode::Optional { child } => walk_optional(&rt, i, plan, *child, region),
        PlanNode::Scan { child } => walk_scan(&rt, i, plan, *child, region),
        PlanNode::OneOf { chars_index } => {
            let chars = plan.literals[*chars_index as usize];
            walk_one_of(&rt, i, chars, region)
        }
        PlanNode::Characters { child, skip } => {
            walk_characters(&rt, i, plan, *child, *skip, region)
        }
        PlanNode::Matrix { child } => walk_matrix(&rt, i, plan, *child, region),
        PlanNode::GridRagged { child, fill_index } => {
            let fill = plan.literals[*fill_index as usize];
            walk_grid_ragged(&rt, i, plan, *child, fill, region)
        }
        PlanNode::Csv { child } => walk_csv(&rt, i, plan, *child, region),
        PlanNode::Ws { child } => walk_ws(&rt, i, plan, *child, region),
        PlanNode::Sep {
            separator_index,
            child,
        } => {
            let sep = plan.literals[*separator_index as usize];
            walk_sep(&rt, i, plan, *child, sep, region)
        }
        PlanNode::Grid { child } => walk_grid(&rt, i, plan, *child, region),
        PlanNode::Template { parts } => walk_template(&rt, i, plan, parts, region),
        PlanNode::Tuple { elements } => walk_tuple(&rt, i, plan, elements, region),
    }
}

// ---- atomics (§7.4) -------------------------------------------------------

fn walk_atomic(rt: &Rt, i: &Input<'_>, kind: AtomicKind, region: ByteRegion) -> WalkResult {
    let rest = region.bytes(i);
    // Every atomic starts by skipping horizontal whitespace; `at` is where the
    // value itself begins, in the input's own coordinates.
    let s = trim_leading_ws(rest);
    let at = region.start().advance(rest.len() - s.len());
    match kind {
        AtomicKind::Int => {
            // Parse a signed decimal integer.
            let (digits, len) = take_int_run(s);
            if digits.is_empty() {
                return Err(ParseFail::at(at.offset(), 0, "int"));
            }
            let value: i64 = digits
                .parse()
                .map_err(|_| ParseFail::at(at.offset(), len, "int"))?;
            Ok(Walked {
                value: rt.alloc_int(value),
                next: at.advance(len),
            })
        }
        AtomicKind::Digit => {
            let Some(&b) = s.first() else {
                return Err(ParseFail::at(at.offset(), 0, "digit"));
            };
            if !b.is_ascii_digit() {
                return Err(ParseFail::at(at.offset(), 1, "digit"));
            }
            let value = (b - b'0') as i64;
            Ok(Walked {
                value: rt.alloc_int(value),
                next: at.advance(1),
            })
        }
        AtomicKind::Char => {
            // One Unicode scalar value, stepped by the region rather than
            // decoded out of an ad-hoc `from_utf8` of the tail.
            let Some(next) = region.next_scalar(i, at) else {
                return Err(ParseFail::at(at.offset(), 0, "char"));
            };
            let text = region
                .subregion(at, next)
                .str(i)
                .ok_or_else(|| ParseFail::at(at.offset(), 0, "char"))?;
            let ch = text
                .chars()
                .next()
                .ok_or_else(|| ParseFail::at(at.offset(), 0, "char"))?;
            Ok(Walked {
                value: rt.alloc_char(ch as u32),
                next,
            })
        }
        AtomicKind::Word => {
            let (word, len) = take_word_run(s);
            if word.is_empty() {
                return Err(ParseFail::at(at.offset(), 0, "word"));
            }
            let slice = rt
                .alloc_text_slice(i.owner(), at.offset(), len)
                .ok_or_else(|| ParseFail::at(at.offset(), len, "word"))?;
            Ok(Walked {
                value: slice,
                next: at.advance(len),
            })
        }
        AtomicKind::UInt => {
            // §7.4's `uint`. Its **type** is `Int` (`ScalarType::UInt` is
            // reserved and has no runtime object); the non-negativity is this
            // rule: a leading `-` is not a `uint`, it is a parse failure.
            if s.first() == Some(&b'-') {
                return Err(ParseFail::at(at.offset(), 1, "uint"));
            }
            let (digits, len) = take_int_run(s);
            if digits.is_empty() {
                return Err(ParseFail::at(at.offset(), 0, "uint"));
            }
            let value: i64 = digits
                .parse()
                .map_err(|_| ParseFail::at(at.offset(), len, "uint"))?;
            Ok(Walked {
                value: rt.alloc_int(value),
                next: at.advance(len),
            })
        }
        AtomicKind::Float => {
            let (text, len) = take_float_run(s);
            if text.is_empty() {
                return Err(ParseFail::at(at.offset(), 0, "float"));
            }
            let value: f64 = text
                .parse()
                .map_err(|_| ParseFail::at(at.offset(), len, "float"))?;
            Ok(Walked {
                value: rt.alloc_float(value),
                next: at.advance(len),
            })
        }
        AtomicKind::Byte => {
            // A decimal integer in `0..=255`, not a raw input byte: a raw byte
            // cannot be re-sliced as `Text` without breaking the UTF-8
            // invariant every source-slice `Text` relies on.
            let (digits, len) = take_int_run(s);
            if digits.is_empty() {
                return Err(ParseFail::at(at.offset(), 0, "byte"));
            }
            let value: u8 = digits
                .parse()
                .map_err(|_| ParseFail::at(at.offset(), len, "byte"))?;
            Ok(Walked {
                value: rt.alloc_byte(value),
                next: at.advance(len),
            })
        }
        AtomicKind::Identifier => {
            // §4.1's identifier class, not a local ASCII rule (F3). §7.4 says
            // "ASCII-like … by default"; accepting fewer names than the
            // language itself declares would be the narrower mistake.
            let len = take_ident_run(s);
            if len == 0 {
                return Err(ParseFail::at(at.offset(), 0, "identifier"));
            }
            let slice = rt
                .alloc_text_slice(i.owner(), at.offset(), len)
                .ok_or_else(|| ParseFail::at(at.offset(), len, "identifier"))?;
            Ok(Walked {
                value: slice,
                next: at.advance(len),
            })
        }
        AtomicKind::Text | AtomicKind::Rest => {
            // `text`/`rest` consume the rest of **the region**, which is what
            // the doc comment always claimed and the code never did: it ran to
            // `bytes.len()`, so a `text` capture swallowed the literal that
            // followed it and every `pre{body:text}post` template was
            // unmatchable (IPR-10). Leading whitespace is part of the text.
            let start = region.start();
            let len = region.end().delta_from(start);
            let slice = rt
                .alloc_text_slice(i.owner(), start.offset(), len)
                .ok_or_else(|| ParseFail::at(start.offset(), len, "text"))?;
            Ok(Walked {
                value: slice,
                next: region.end(),
            })
        }
    }
}

// ---- constructors (§7.5) --------------------------------------------------

/// Walk `node` against `region` and require it to consume the region **exactly**.
///
/// §7.5's rule for a bounded construct is that "each application must consume
/// the entire line" (and the same for a section, a CSV field, a
/// whitespace-delimited token, a matrix cell). The predecessor computed those
/// bounds and then walked the child against everything from the bound's start
/// to the end of the buffer, discarding the child's cursor — five separate
/// `let (value, _consumed) = …` and one explicit `let _ = token_end;`. So
/// `lines(int)` accepted `12junk` and `csv(rest)` returned the whole remainder
/// for its first field.
///
/// Returning a bare `GcRef` is the point: there is no cursor left for a caller
/// to forget to check, so "I bounded the child but did not require it to fill
/// the bound" stops being expressible.
///
/// # Safety
/// `ctx` must be live and wired.
unsafe fn walk_exact(
    rt: &Rt,
    i: &Input<'_>,
    plan: &ParserPlan,
    node: u32,
    region: ByteRegion,
    what: &'static str,
) -> Result<GcRef, ParseFail> {
    // SAFETY: forwarded from this function's contract.
    let walked = unsafe { walk(rt.ctx, i, plan, node, region)? };
    if walked.next != region.end() {
        return Err(ParseFail::at(
            walked.next.offset(),
            region.end().delta_from(walked.next),
            what,
        ));
    }
    Ok(walked.value)
}

/// The text a region spans, or a parse failure naming `what`.
///
/// The predecessor wrote `str::from_utf8(region).unwrap_or("")` in three
/// places, which turned a region whose ends were not scalar boundaries into an
/// *empty* one — a zero-row, zero-width `Grid` where there should have been a
/// mismatch (IPR-05). A region of a validated [`Input`] can only fail this by
/// splitting a scalar, which is an interpreter bug; it is reported as a parse
/// failure rather than asserted, because this runs inside `extern "C"`.
fn region_str<'a>(
    i: &Input<'a>,
    region: ByteRegion,
    what: &'static str,
) -> Result<&'a str, ParseFail> {
    region
        .str(i)
        .ok_or_else(|| ParseFail::at(region.start().offset(), region.len(), what))
}

/// The whitespace-delimited tokens of `region`, whose text is `s`, as absolute
/// subregions.
///
/// Bounds are computed while splitting rather than recovered afterwards by
/// searching the region for the token's text — which is what `csv` did, so
/// every duplicate field mapped to the first occurrence (IPR-04).
fn whitespace_tokens(region: ByteRegion, s: &str) -> Vec<ByteRegion> {
    let base = region.start();
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (idx, ch) in s.char_indices() {
        if ch.is_whitespace() {
            if let Some(st) = start.take() {
                out.push(region.subregion(base.advance(st), base.advance(idx)));
            }
        } else if start.is_none() {
            start = Some(idx);
        }
    }
    if let Some(st) = start {
        out.push(region.subregion(base.advance(st), region.end()));
    }
    out
}

/// The comma-separated fields of `region`, whose text is `s`, each trimmed of
/// surrounding whitespace, as absolute subregions.
///
/// A field that trims to nothing yields an **empty region**, not a search for
/// an empty needle: `region_offset_of` used to call `hay.windows(0)`, which
/// panics, and `"10,20,\n"` was enough to reach it — a panic inside
/// `extern "C"` (IPR-04, D12).
fn csv_tokens(region: ByteRegion, s: &str) -> Vec<ByteRegion> {
    let base = region.start();
    let mut out = Vec::new();
    let mut field_start = 0usize;
    let push = |field: &str, at: usize, out: &mut Vec<ByteRegion>| {
        let lead = field.len() - field.trim_start().len();
        let trimmed = field.trim();
        out.push(region.subregion(
            base.advance(at + lead),
            base.advance(at + lead + trimmed.len()),
        ));
    };
    for (idx, ch) in s.char_indices() {
        if ch == ',' {
            push(&s[field_start..idx], field_start, &mut out);
            field_start = idx + ch.len_utf8();
        }
    }
    push(&s[field_start..], field_start, &mut out);
    out
}

/// A grid row's width in Unicode scalars.
fn row_width(i: &Input<'_>, line: ByteRegion) -> Result<usize, ParseFail> {
    line.scalar_count(i)
        .ok_or_else(|| ParseFail::at(line.start().offset(), line.len(), "a grid row"))
}

fn walk_lines(
    rt: &Rt,
    i: &Input<'_>,
    plan: &ParserPlan,
    child: u32,
    region: ByteRegion,
) -> WalkResult {
    let mut items = Vec::new();
    for line in split_lines(i, region) {
        // One line, consumed exactly. The predecessor walked the child against
        // everything from the line's start to the end of the buffer and threw
        // the cursor away, so `lines(int)` accepted `12junk` and `lines(rest)`
        // handed every element the whole remaining input (IPR-02).
        // SAFETY: ctx is valid (upheld by `walk`'s caller).
        items.push(unsafe { walk_exact(rt, i, plan, child, line, "the rest of the line")? });
    }
    let elem_desc = child_descriptor(plan, child);
    Ok(Walked {
        value: rt.alloc_vec(elem_desc, items),
        next: region.end(),
    })
}

fn walk_sections(
    rt: &Rt,
    i: &Input<'_>,
    plan: &ParserPlan,
    child: u32,
    region: ByteRegion,
) -> WalkResult {
    let mut items = Vec::new();
    for section in split_sections(i, region) {
        // A **narrowing of the same buffer**, not a re-slice walked at offset
        // zero. The predecessor handed the child `&bytes[sec..sec+len]` with an
        // offset of 0 while its Texts were still allocated against the whole
        // input, so a `word` in section 2 named bytes at the start of the file
        // (IPR-03, the stage's P0).
        // SAFETY: ctx is valid.
        items.push(unsafe { walk_exact(rt, i, plan, child, section, "the rest of the section")? });
    }
    let elem_desc = child_descriptor(plan, child);
    Ok(Walked {
        value: rt.alloc_vec(elem_desc, items),
        next: region.end(),
    })
}

/// Walk named heterogeneous `sections(name: P, ..., tail: repeated(P))` (M9,
/// §7.5). The region is split on blank lines into sections; the first `N`
/// sections (where N = number of named fields, or fewer if a `repeated` tail is
/// present — it takes all the rest) are parsed by the named fields in order;
/// any remaining sections are parsed by the `repeated` tail into a `Vec`. The
/// result is an anonymous record assembled via [`alloc_record`].
fn walk_sections_named(
    rt: &Rt,
    i: &Input<'_>,
    plan: &ParserPlan,
    fields: &'static [(&'static str, u32)],
    repeated_tail: Option<(&'static str, u32)>,
    region: ByteRegion,
) -> WalkResult {
    let sections = split_sections(i, region);
    // Too few sections is a parse fault.
    if sections.len() < fields.len() {
        return Err(ParseFail::at(
            region.start().offset(),
            region.len(),
            "section header",
        ));
    }
    // Each section is a narrowing of the input, so the child's offsets are the
    // input's own offsets and a source-slice `Text` is right by construction.
    let mut captures: Vec<(Option<&'static str>, u32, GcRef)> = Vec::new();
    for (n, (name, child)) in fields.iter().enumerate() {
        // SAFETY: ctx is valid.
        let value =
            unsafe { walk_exact(rt, i, plan, *child, sections[n], "the rest of the section")? };
        captures.push((Some(name), *child, value));
    }
    if let Some((tail_name, tail_child)) = repeated_tail {
        // The tail consumes every remaining section, parsed per-section by its
        // child into a Vec.
        let mut tail_items = Vec::new();
        for section in &sections[fields.len()..] {
            // SAFETY: ctx is valid.
            tail_items.push(unsafe {
                walk_exact(rt, i, plan, tail_child, *section, "the rest of the section")?
            });
        }
        let elem_desc = child_descriptor(plan, tail_child);
        let tail_vec = rt.alloc_vec(elem_desc, tail_items);
        // The tail field's "child" node for descriptor purposes is the tail
        // child; its value is the assembled Vec.
        captures.push((Some(tail_name), tail_child, tail_vec));
    }
    let record = alloc_record(rt, &captures);
    Ok(Walked {
        value: record,
        next: region.end(),
    })
}

/// Walk `block(item, ...)` (M9, §7.5): apply sequential parsers within one
/// region, advancing the cursor after each. A positional named-capture template
/// *flattens* its fields into the block record; a named item contributes one
/// field. The result is a flattened anonymous record assembled via
/// [`alloc_record`].
///
/// Cursor model: each item is walked against the region's tail from the current
/// cursor, and the item's returned position becomes the next cursor. Every
/// position in play is absolute, so a chain of line-anchored templates advances
/// line by line.
fn walk_block(
    rt: &Rt,
    i: &Input<'_>,
    plan: &ParserPlan,
    items: &'static [praxis_input_parser::BlockItemNode],
    region: ByteRegion,
) -> WalkResult {
    let mut cursor = region.start();
    // Captures collected as (name, child_node_for_descriptor, value). For a
    // flattened positional record, we expand its fields into separate entries.
    let mut captures: Vec<(Option<&'static str>, u32, GcRef)> = Vec::new();
    for (n, item) in items.iter().enumerate() {
        // Before every item after the first, skip the line boundary: any run of
        // horizontal whitespace plus one newline (§7.5 block items are
        // line-anchored). The first item starts at the region head.
        if n > 0 {
            cursor = skip_line_boundary(i, region, cursor);
        }
        match item {
            praxis_input_parser::BlockItemNode::Positional { child } => {
                // SAFETY: ctx is valid.
                let walked = unsafe { walk(rt.ctx, i, plan, *child, region.from(cursor))? };
                cursor = walked.next;
                // If the positional produced a record (named-capture template),
                // flatten its fields into the block record. We detect a record
                // by pointer-equality of its descriptor against RECORD.
                if std::ptr::eq(walked.value.descriptor(), &crate::records::RECORD) {
                    flatten_record_into(rt, walked.value, &mut captures);
                }
                // A non-record positional (scalar) was rejected by validation
                // (I026); if we reach one here it contributes no field.
            }
            praxis_input_parser::BlockItemNode::Named { name, child } => {
                // SAFETY: ctx is valid.
                let walked = unsafe { walk(rt.ctx, i, plan, *child, region.from(cursor))? };
                cursor = walked.next;
                captures.push((Some(name), *child, walked.value));
            }
        }
    }
    let record = alloc_record(rt, &captures);
    Ok(Walked {
        value: record,
        next: cursor,
    })
}

/// Skip the line boundary between sequential `block` items (§7.5): any run of
/// horizontal whitespace, then an optional single line ending (`\n` or `\r\n`).
/// Returns the new cursor. If no line ending is present (e.g. the items are on
/// one line separated by spaces), only the horizontal whitespace is consumed.
///
/// Byte-wise on purpose: space, tab, CR and LF are single-byte scalars and
/// cannot occur inside a multi-byte one, so scanning bytes here can never land
/// mid-scalar. (The cell and scan loops step by scalar because *they* can.)
fn skip_line_boundary(i: &Input<'_>, region: ByteRegion, cursor: Cursor) -> Cursor {
    let tail = region.from(cursor);
    let bytes = tail.bytes(i);
    let mut n = 0usize;
    while n < bytes.len() && (bytes[n] == b' ' || bytes[n] == b'\t') {
        n += 1;
    }
    if bytes.get(n) == Some(&b'\r') {
        n += 1;
    }
    if bytes.get(n) == Some(&b'\n') {
        n += 1;
    }
    cursor.advance(n)
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
    for (n, field) in schema.fields.iter().enumerate() {
        if let Some(value) = items.get(n) {
            captures.push((Some(field.name), u32::MAX, *value));
        }
    }
}

/// Walk `choice(Name: P, ...)` (M9, §7.5): try each case in source order from
/// the region's start. The first case whose parser succeeds wins; its value
/// becomes the variant's payload and the cursor advances to where that parser
/// stopped. If a case fails, the next case is tried from the same start
/// (backtracking). If no case matches, this is a parse fault.
///
/// `choice` does **not** require its region to be exhausted. Whether a match
/// must fill its region is the bounded parent's question — `lines(choice(…))`
/// requires it through `walk_exact`, `scan(choice(…))` matches fragments by
/// design — and answering it in one place is what makes both correct.
///
/// Backtracking note: a failed case may have allocated GC objects (since `walk`
/// allocates eagerly); those are unreferenced and collected later. Only the
/// cursor is restored — there is no allocator rollback, which is fine because
/// failed allocations are simply garbage.
fn walk_choice(
    rt: &Rt,
    i: &Input<'_>,
    plan: &ParserPlan,
    cases: &'static [(&'static str, u32)],
    region: ByteRegion,
) -> WalkResult {
    for (tag, (_name, child)) in cases.iter().enumerate() {
        // SAFETY: ctx is valid.
        match unsafe { walk(rt.ctx, i, plan, *child, region) } {
            Ok(walked) => {
                // First match wins. Tag with this case's index; the value is
                // the single payload slot.
                let schema = enum_schema_for(cases);
                let enum_ref = rt.alloc_enum(schema, tag as u32, vec![walked.value]);
                return Ok(Walked {
                    value: enum_ref,
                    next: walked.next,
                });
            }
            Err(_inner) => {
                // Backtrack: try the next case from the same position. We
                // discard the inner failure here; if no case matches, the
                // choice's own failure below is the user-visible one.
                continue;
            }
        }
    }
    Err(ParseFail::at(region.start().offset(), 0, "any choice case"))
}

/// Walk `optional(P)` (M9, §7.5): parse `P`; on success return `Some(value)`
/// (Option tag 0) advancing the cursor, on failure return `None` (tag 1) and
/// consume NO input (the cursor stays at the region's start). No fault is
/// raised on a miss — this is parser-level optionality, not exception recovery.
fn walk_optional(
    rt: &Rt,
    i: &Input<'_>,
    plan: &ParserPlan,
    child: u32,
    region: ByteRegion,
) -> WalkResult {
    // SAFETY: ctx is valid.
    match unsafe { walk(rt.ctx, i, plan, child, region) } {
        Ok(walked) => {
            let some_ref = rt.alloc_enum(crate::enums::option_schema(), 0, vec![walked.value]);
            Ok(Walked {
                value: some_ref,
                next: walked.next,
            })
        }
        Err(_) => {
            // Consume nothing; return None (tag 1, no payload). The inner
            // failure is intentionally swallowed — `optional` is parser-level
            // optionality, not exception recovery.
            let none_ref = rt.alloc_enum(crate::enums::option_schema(), 1, Vec::new());
            Ok(Walked {
                value: none_ref,
                next: region.start(),
            })
        }
    }
}

/// Walk `scan(P)` (M9, §7.5): slide a cursor across the region; at each
/// position try `P`. On success, push the value and advance past the match
/// (so overlapping matches aren't found); on failure, advance one position.
/// All unmatched text is ignored. Returns `Vec[result(P)]` in source order.
fn walk_scan(
    rt: &Rt,
    i: &Input<'_>,
    plan: &ParserPlan,
    child: u32,
    region: ByteRegion,
) -> WalkResult {
    let mut items = Vec::new();
    let mut cursor = region.start();
    while cursor < region.end() {
        // SAFETY: ctx is valid.
        match unsafe { walk(rt.ctx, i, plan, child, region.from(cursor)) } {
            Ok(walked) => {
                // A match must advance the cursor (otherwise we'd loop forever
                // on a zero-width match). If it didn't, step one position.
                items.push(walked.value);
                cursor = if walked.next > cursor {
                    walked.next
                } else {
                    match region.next_scalar(i, cursor) {
                        Some(next) => next,
                        None => break,
                    }
                };
            }
            Err(_) => {
                cursor = match region.next_scalar(i, cursor) {
                    Some(next) => next,
                    None => break,
                };
            }
        }
    }
    let elem_desc = child_descriptor(plan, child);
    Ok(Walked {
        value: rt.alloc_vec(elem_desc, items),
        next: region.end(),
    })
}

/// Walk `one_of("LR")` (M9, §7.5): match one character from a literal set.
fn walk_one_of(rt: &Rt, i: &Input<'_>, chars: &str, region: ByteRegion) -> WalkResult {
    let rest = region.bytes(i);
    let s = trim_leading_ws(rest);
    let at = region.start().advance(rest.len() - s.len());
    let Some(next) = region.next_scalar(i, at) else {
        return Err(ParseFail::at(at.offset(), 0, "char"));
    };
    let ch = region
        .subregion(at, next)
        .str(i)
        .and_then(|t| t.chars().next())
        .ok_or_else(|| ParseFail::at(at.offset(), 0, "char"))?;
    if !chars.contains(ch) {
        return Err(ParseFail::at(
            at.offset(),
            ch.len_utf8(),
            format!("one of \"{chars}\""),
        ));
    }
    Ok(Walked {
        value: rt.alloc_char(ch as u32),
        next,
    })
}

/// Walk `chars(P, skip:)` (M9, §7.5): apply a char-parser repeatedly, trimming
/// between matches per the skip policy.
fn walk_characters(
    rt: &Rt,
    i: &Input<'_>,
    plan: &ParserPlan,
    child: u32,
    skip: praxis_input_parser::SkipPolicy,
    region: ByteRegion,
) -> WalkResult {
    let mut items = Vec::new();
    let mut cursor = region.start();
    loop {
        cursor = skip_chars(i, region, cursor, skip);
        if cursor >= region.end() {
            break;
        }
        // **A child failure is the parse's failure** (IPR-07). This used to
        // `break`, so `chars` returned `Ok` at the first mismatch and silently
        // dropped the rest of the region — `chars(digit)` over `"12x34"`
        // answered `[1, 2]` and reported nothing. The loop's own shape is what
        // implements §7.5's rule: the skip policy runs once more after the last
        // match, so `skip: whitespace` / `skip: newlines` can absorb a trailing
        // run, and under `skip: none` a trailing byte the child cannot read
        // correctly faults.
        // SAFETY: ctx is valid.
        let walked = unsafe { walk(rt.ctx, i, plan, child, region.from(cursor))? };
        cursor = if walked.next > cursor {
            walked.next
        } else {
            match region.next_scalar(i, cursor) {
                Some(next) => next,
                None => break,
            }
        };
        items.push(walked.value);
    }
    // The element descriptor is the child's, not a hardcoded `CHAR`. The Vec
    // used to be tagged `Char` whatever it held, so `chars(int, …)` filled a
    // `Vec[Char]` with `Int` objects and `vec_format`/`vec_equals`/`vec_hash`
    // dispatched through the wrong callback (IPR-07, D-S20-A).
    let elem_desc = child_descriptor(plan, child);
    Ok(Walked {
        value: rt.alloc_vec(elem_desc, items),
        next: region.end(),
    })
}

/// Skip bytes at `cursor` per the `chars` skip policy (§7.5).
///
/// Byte-wise like [`skip_line_boundary`], and sound for the same reason: every
/// byte it tests is ASCII whitespace, which cannot appear inside a multi-byte
/// scalar.
fn skip_chars(
    i: &Input<'_>,
    region: ByteRegion,
    cursor: Cursor,
    skip: praxis_input_parser::SkipPolicy,
) -> Cursor {
    use praxis_input_parser::SkipPolicy;
    let bytes = region.from(cursor).bytes(i);
    let mut n = 0usize;
    match skip {
        SkipPolicy::None => {}
        SkipPolicy::Whitespace => {
            while n < bytes.len() && (bytes[n] == b' ' || bytes[n] == b'\t') {
                n += 1;
            }
        }
        SkipPolicy::Newlines => {
            while n < bytes.len() && bytes[n].is_ascii_whitespace() {
                n += 1;
            }
        }
    }
    cursor.advance(n)
}

/// Walk `matrix(P)` (M9, §7.5, ADR-030): parse lines of whitespace-separated
/// tokens into a rectangular `Grid[result(P)]`. Each row must have the same
/// token count.
fn walk_matrix(
    rt: &Rt,
    i: &Input<'_>,
    plan: &ParserPlan,
    child: u32,
    region: ByteRegion,
) -> WalkResult {
    let mut rows: Vec<Vec<ByteRegion>> = Vec::new();
    for line in split_lines(i, region) {
        let text = region_str(i, line, "matrix row")?;
        if text.trim().is_empty() {
            continue;
        }
        rows.push(whitespace_tokens(line, text));
    }
    let width = rows.first().map(Vec::len).unwrap_or(0);
    let mut items = Vec::with_capacity(rows.len() * width);
    for row in &rows {
        if row.len() != width {
            return Err(ParseFail::at(
                region.start().offset(),
                region.len(),
                "rectangular matrix row",
            ));
        }
        for token in row {
            // The token's own region, not its bytes copied into a fresh buffer
            // walked at offset zero (IPR-03/IPR-05), and consumed exactly.
            // SAFETY: ctx is valid.
            items.push(unsafe { walk_exact(rt, i, plan, child, *token, "the rest of the token")? });
        }
    }
    let elem_desc = child_descriptor(plan, child);
    alloc_grid(rt, elem_desc, items, width, region.end())
}

/// Walk ragged `grid(P, ragged, fill:)` (M9, §7.5): permit uneven rows and pad
/// to the maximum width with the `fill` value (parsed by the cell parser).
fn walk_grid_ragged(
    rt: &Rt,
    i: &Input<'_>,
    plan: &ParserPlan,
    child: u32,
    fill: &str,
    region: ByteRegion,
) -> WalkResult {
    let lines = split_lines(i, region);
    // Widths are **scalar** counts, not byte counts (IPR-06, D11): a row is a
    // row of characters, so a row holding one `é` is one column wide and not
    // two.
    let mut widths = Vec::with_capacity(lines.len());
    for line in &lines {
        widths.push(row_width(i, *line)?);
    }
    let width = widths.iter().copied().max().unwrap_or(0);
    // **The fill is not a region of the input.** It is a plan literal, and the
    // predecessor walked `fill.as_bytes()` at offset 0 while the cell parser
    // allocated its Texts against the input — so a `Text` fill cell named input
    // bytes with no relationship at all to the fill text (IPR-03). It gets its
    // own owned `Text` and its own `Input`, which is what makes a sliced fill
    // cell name the fill.
    let fill_owner = rt.alloc_text_owned(fill);
    // SAFETY: `alloc_text_owned` just produced a live Text.
    let fill_input = unsafe { Input::new(fill_owner) }
        .ok_or_else(|| ParseFail::at(region.start().offset(), 0, "grid fill"))?;
    let fill_region = fill_input.whole();
    // SAFETY: ctx is valid.
    let fill_value = unsafe {
        walk_exact(
            rt,
            &fill_input,
            plan,
            child,
            fill_region,
            "the rest of the fill",
        )?
    };
    let mut items = Vec::with_capacity(lines.len() * width);
    for (line, row) in lines.iter().zip(&widths) {
        let mut cell = line.start();
        while let Some(next) = line.next_scalar(i, cell) {
            // One scalar, consumed exactly — which is D11's answer for
            // `grid(int)`: a cell parser parses a cell exactly as it would
            // anywhere else, and a cell is one character.
            // SAFETY: ctx is valid.
            items.push(unsafe {
                walk_exact(
                    rt,
                    i,
                    plan,
                    child,
                    line.subregion(cell, next),
                    "the rest of the cell",
                )?
            });
            cell = next;
        }
        for _ in *row..width {
            items.push(fill_value);
        }
    }
    let elem_desc = child_descriptor(plan, child);
    alloc_grid(rt, elem_desc, items, width, region.end())
}

/// Allocate a `Grid` from element refs + width (shared by grid/matrix/ragged).
/// `next` is the position the constructor stopped at.
fn alloc_grid(
    rt: &Rt,
    elem_desc: &'static crate::TypeDescriptor,
    items: Vec<GcRef>,
    width: usize,
    next: Cursor,
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
    Ok(Walked {
        value: grid_ref,
        next,
    })
}

fn walk_csv(
    rt: &Rt,
    i: &Input<'_>,
    plan: &ParserPlan,
    child: u32,
    region: ByteRegion,
) -> WalkResult {
    let text = region_str(i, region, "csv")?;
    let mut items = Vec::new();
    for token in csv_tokens(region, text) {
        // The field's own region. `csv` used to hand the child everything from
        // the field's start to the end of the input and discard the cursor —
        // the discard was even written out, `let _ = token_end;` (IPR-04).
        // SAFETY: ctx is valid.
        items.push(unsafe { walk_exact(rt, i, plan, child, token, "the rest of the field")? });
    }
    let elem_desc = child_descriptor(plan, child);
    Ok(Walked {
        value: rt.alloc_vec(elem_desc, items),
        next: region.end(),
    })
}

fn walk_ws(
    rt: &Rt,
    i: &Input<'_>,
    plan: &ParserPlan,
    child: u32,
    region: ByteRegion,
) -> WalkResult {
    let bytes = region.bytes(i);
    let base = region.start();
    let mut items = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        // Skip leading whitespace.
        while pos < bytes.len() && is_ws(bytes[pos]) {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        let token_start = pos;
        while pos < bytes.len() && !is_ws(bytes[pos]) {
            pos += 1;
        }
        let token = region.subregion(base.advance(token_start), base.advance(pos));
        // SAFETY: ctx is valid.
        items.push(unsafe { walk_exact(rt, i, plan, child, token, "the rest of the token")? });
    }
    let elem_desc = child_descriptor(plan, child);
    Ok(Walked {
        value: rt.alloc_vec(elem_desc, items),
        next: region.end(),
    })
}

fn walk_sep(
    rt: &Rt,
    i: &Input<'_>,
    plan: &ParserPlan,
    child: u32,
    sep: &str,
    region: ByteRegion,
) -> WalkResult {
    let bytes = region.bytes(i);
    let base = region.start();
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
    if sep_bytes.is_empty() {
        return Err(ParseFail::at(base.offset(), 0, "a non-empty separator"));
    }
    let mut items = Vec::new();
    let mut token_start = 0usize;
    let mut pos = 0usize;
    while pos < bytes.len() {
        if bytes[pos..].starts_with(sep_bytes) {
            let token = region.subregion(base.advance(token_start), base.advance(pos));
            // SAFETY: ctx is valid.
            items.push(unsafe { walk_exact(rt, i, plan, child, token, "the rest of the token")? });
            pos += sep_bytes.len();
            token_start = pos;
        } else {
            pos += 1;
        }
    }
    // Parse the final token.
    if token_start < bytes.len() {
        let token = region.subregion(base.advance(token_start), region.end());
        // SAFETY: ctx is valid.
        items.push(unsafe { walk_exact(rt, i, plan, child, token, "the rest of the token")? });
    }
    let elem_desc = child_descriptor(plan, child);
    Ok(Walked {
        value: rt.alloc_vec(elem_desc, items),
        next: region.end(),
    })
}

fn walk_grid(
    rt: &Rt,
    i: &Input<'_>,
    plan: &ParserPlan,
    child: u32,
    region: ByteRegion,
) -> WalkResult {
    let lines = split_lines(i, region);
    let width = match lines.first() {
        Some(line) => row_width(i, *line)?,
        None => 0,
    };
    let mut items = Vec::with_capacity(lines.len() * width);
    for line in &lines {
        if row_width(i, *line)? != width {
            // Grid rows must be uniform (§7.5). Ragged grids are M9. Uniform in
            // **characters**: a row of `##é` and a row of `###` are both three.
            return Err(ParseFail::at(
                region.start().offset(),
                region.len(),
                "uniform grid row",
            ));
        }
        let mut cell = line.start();
        while let Some(next) = line.next_scalar(i, cell) {
            // SAFETY: ctx is valid.
            items.push(unsafe {
                walk_exact(
                    rt,
                    i,
                    plan,
                    child,
                    line.subregion(cell, next),
                    "the rest of the cell",
                )?
            });
            cell = next;
        }
    }
    let elem_desc = child_descriptor(plan, child);
    alloc_grid(rt, elem_desc, items, width, region.end())
}

// ---- templates (§7.2, §7.3) -----------------------------------------------

/// The first literal part after `index` that has text to match, with its
/// whitespace policy. A policy-only literal (`\\s*`, `\\n`) constrains nothing on
/// its own, so it is not a bound.
fn following_literal(
    parts: &[praxis_input_parser::TemplatePartNode],
    index: usize,
) -> Option<(&str, praxis_input_parser::WsPolicy)> {
    parts[index + 1..].iter().find_map(|p| match p {
        praxis_input_parser::TemplatePartNode::Literal { text, ws } if !text.is_empty() => {
            Some((&**text, *ws))
        }
        _ => None,
    })
}

/// The earliest position at or after `cursor` where `lit` can match after its
/// whitespace policy — i.e. where the capture before it must stop.
///
/// "Earliest" is what makes `text` non-greedy, and taking the position *before*
/// the policy runs is what keeps the whitespace out of the capture: for
/// `{a:int},{b:int}` on `"12 ,34"` the comma's `SpaceRun` eats the space, so
/// the bound is after `12` and `int` consumes its region exactly.
///
/// `None` means the literal does not appear in the rest of the region at all,
/// which is a mismatch the literal itself will report.
fn capture_bound(
    i: &Input<'_>,
    region: ByteRegion,
    base: Cursor,
    cursor: Cursor,
    lit: &str,
    ws: praxis_input_parser::WsPolicy,
) -> Option<Cursor> {
    let bytes = region.bytes(i);
    let mut at = cursor;
    loop {
        if let Some(after) = consume_ws(bytes, at.delta_from(base), ws) {
            let q = base.advance(after);
            if region.from(q).bytes(i).starts_with(lit.as_bytes()) {
                return Some(at);
            }
        }
        // Step by scalar, so a bound never lands inside a multi-byte character.
        at = region.next_scalar(i, at)?;
    }
}

/// Interpret a backtick template against `region` (§7.2, §7.3).
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
    i: &Input<'_>,
    plan: &ParserPlan,
    parts: &[praxis_input_parser::TemplatePartNode],
    region: ByteRegion,
) -> WalkResult {
    let base = region.start();
    let bytes = region.bytes(i);
    let mut cursor = base;
    // Capture values in field-index order. Each entry is (name, child_node,
    // value): the child node is kept so a multi-anon-capture tuple can build its
    // TupleSchema from the child result descriptors.
    let mut captures: Vec<(Option<&'static str>, u32, GcRef)> = Vec::new();

    for (index, part) in parts.iter().enumerate() {
        match part {
            praxis_input_parser::TemplatePartNode::Literal { text, ws } => {
                // Honor the whitespace policy before matching the literal.
                let Some(after) = consume_ws(bytes, cursor.delta_from(base), *ws) else {
                    return Err(ParseFail::at(cursor.offset(), 0, "whitespace"));
                };
                cursor = base.advance(after);
                // Match the literal bytes verbatim, within the region.
                let lit = text.as_bytes();
                if !region.from(cursor).bytes(i).starts_with(lit) {
                    return Err(ParseFail::at(
                        cursor.offset(),
                        lit.len(),
                        format!("literal {:?}", text),
                    ));
                }
                cursor = cursor.advance(lit.len());
            }
            praxis_input_parser::TemplatePartNode::Capture {
                child,
                field_index: _,
                name,
            } => {
                // Skip any flexible leading whitespace before a capture, then
                // walk the child parser to extract one value.
                cursor = base.advance(skip_capture_ws(bytes, cursor.delta_from(base)));
                // **Bound the capture by the literal that follows it** (IPR-10).
                // §7.4 says `text` "minimally consumes text until the following
                // template literal can match", and the predecessor consumed to
                // the end of the whole buffer — so `pre{body:text}post` ate its
                // own suffix and no template with a trailing literal could ever
                // match. Done here rather than in `walk_atomic` because it is
                // uniform: every capture is bounded, not only the `text` ones,
                // which is also what stops a `word` at a `-` without adding `-`
                // to `word`'s delimiter set (IPR-11).
                match following_literal(parts, index) {
                    Some((lit, lit_ws)) => {
                        match capture_bound(i, region, base, cursor, lit, lit_ws) {
                            Some(bound) => {
                                // SAFETY: ctx is valid.
                                let value = unsafe {
                                    walk_exact(
                                        rt,
                                        i,
                                        plan,
                                        *child,
                                        region.subregion(cursor, bound),
                                        "the rest of the capture",
                                    )?
                                };
                                cursor = bound;
                                captures.push((*name, *child, value));
                            }
                            None => {
                                // The following literal does not appear at all.
                                // Let the capture parse naturally so the
                                // *literal* reports the mismatch, at the
                                // position where it was actually looked for.
                                // SAFETY: ctx is valid.
                                let walked =
                                    unsafe { walk(rt.ctx, i, plan, *child, region.from(cursor))? };
                                cursor = walked.next;
                                captures.push((*name, *child, walked.value));
                            }
                        }
                    }
                    None => {
                        // Nothing follows, so there is nothing to stop before:
                        // the capture takes the rest of the region and keeps
                        // its own cursor. Requiring exhaustion here would fault
                        // every root-level template on its input's trailing
                        // newline; whether the region must be filled is the
                        // *parent's* question.
                        // SAFETY: ctx is valid.
                        let walked = unsafe { walk(rt.ctx, i, plan, *child, region.from(cursor))? };
                        cursor = walked.next;
                        captures.push((*name, *child, walked.value));
                    }
                }
            }
        }
    }

    // Assemble the result per §7.3, returning the position where matching
    // stopped so a `block(...)` parent can advance item by item (§7.5).
    let any_named = captures.iter().any(|(n, _, _)| n.is_some());
    let value = if any_named {
        // Named captures → Record. Build the schema at runtime.
        alloc_record(rt, &captures)
    } else if captures.len() == 1 {
        // Single anonymous capture → the scalar value.
        captures.into_iter().next().unwrap().2
    } else if captures.is_empty() {
        // No captures → Unit.
        alloc_unit(rt)
    } else {
        // Multiple anonymous captures → Tuple. Build the schema from the child
        // result descriptors and fill the payload with the captured values.
        let children: Vec<u32> = captures.iter().map(|(_, c, _)| *c).collect();
        let values: Vec<GcRef> = captures.into_iter().map(|(_, _, v)| v).collect();
        alloc_tuple(rt, &children, plan, values)
    };
    Ok(Walked {
        value,
        next: cursor,
    })
}

/// Interpret a multi-anon-capture template lowered as a `Tuple` node (§7.3).
///
/// Each element is walked against the region's tail from the current cursor and
/// the cursor advances to where that element stopped — the same model as
/// [`walk_template`], and the same fix: the predecessor returned
/// `bytes.len() - offset`, a *length*, where its caller wanted a position.
fn walk_tuple(
    rt: &Rt,
    i: &Input<'_>,
    plan: &ParserPlan,
    elements: &[u32],
    region: ByteRegion,
) -> WalkResult {
    let base = region.start();
    let bytes = region.bytes(i);
    let mut cursor = base;
    let mut values: Vec<GcRef> = Vec::with_capacity(elements.len());
    for &elem in elements {
        cursor = base.advance(skip_capture_ws(bytes, cursor.delta_from(base)));
        // SAFETY: ctx is valid.
        let walked = unsafe { walk(rt.ctx, i, plan, elem, region.from(cursor))? };
        cursor = walked.next;
        values.push(walked.value);
    }
    let tuple_ref = alloc_tuple(rt, elements, plan, values);
    Ok(Walked {
        value: tuple_ref,
        next: cursor,
    })
}

/// Skip the horizontal whitespace before a capture: zero or more spaces or
/// tabs. Returns the number of bytes skipped.
///
/// This is **not** a [`WsPolicy`](praxis_input_parser::WsPolicy) and it used to
/// be one — `SpaceRun`, which is the one-or-more policy. It only worked because
/// `SpaceRun` was implemented as zero-or-more; making `SpaceRun` mean what it
/// says would otherwise have made every capture demand leading whitespace.
/// §7.4 puts surrounding horizontal space on the caller, and this is the
/// caller: it matches `walk_atomic`'s own `trim_leading_ws`.
fn skip_capture_ws(bytes: &[u8], cursor: usize) -> usize {
    let Some(rest) = bytes.get(cursor..) else {
        return cursor;
    };
    let mut i = 0;
    while i < rest.len() && (rest[i] == b' ' || rest[i] == b'\t') {
        i += 1;
    }
    cursor + i
}

/// Consume bytes at `cursor` per `ws`, returning the new cursor or `None` if the
/// policy is not satisfied (§7.2).
fn consume_ws(bytes: &[u8], cursor: usize, ws: praxis_input_parser::WsPolicy) -> Option<usize> {
    use praxis_input_parser::WsPolicy;
    let rest = bytes.get(cursor..)?;
    let mut i = 0;
    match ws {
        WsPolicy::None => {
            // The template wrote no run in front of this literal, so no run is
            // consumed. Without this variant every literal claimed `SpaceRun`
            // and `SpaceRun` had to accept an empty run to compensate.
        }
        WsPolicy::SpaceRun => {
            // **One or more** spaces or tabs — the flexible §7.2 default, as
            // `WsPolicy`'s own definition states it. It used to accept an empty
            // run, with a comment admitting the contradiction, because the
            // scanner tagged every literal `SpaceRun` and requiring one would
            // have made a template starting with a literal unmatchable. The
            // scanner distinguishes them now (IPR-12).
            while i < rest.len() && (rest[i] == b' ' || rest[i] == b'\t') {
                i += 1;
            }
            if i == 0 {
                return None;
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
//
// `split_lines` and `split_sections` live in `cursor.rs` now, because they
// produce positions and positions are that module's business. `region_offset_of`
// is gone entirely: it located a CSV token by *searching the region for the
// token's text*, so duplicate fields all mapped to the first occurrence, and it
// called `hay.windows(0)` — a panic — for any token that trimmed to nothing,
// which `"10,20,\n"` was enough to reach. Inside `extern "C"` that is not a
// panic, it is undefined behaviour. `csv_tokens` computes the bounds while it
// splits, so there is nothing to search for and nothing to be empty.

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
        PlanNode::Template { parts } => template_result_descriptor(plan, parts),
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
    plan: &ParserPlan,
    parts: &[praxis_input_parser::TemplatePartNode],
) -> &'static crate::TypeDescriptor {
    let mut single_anonymous: Option<u32> = None;
    let mut captures = 0usize;
    let mut any_named = false;
    for p in parts {
        if let praxis_input_parser::TemplatePartNode::Capture { name, child, .. } = p {
            captures += 1;
            if name.is_some() {
                any_named = true;
            } else {
                single_anonymous = Some(*child);
            }
        }
    }
    if any_named {
        &crate::records::RECORD
    } else if captures == 1 {
        // Single anonymous capture → the child's own result descriptor.
        //
        // This used to be `&scalars::INT`, defended by a comment arguing it was
        // "a sound default" because the per-value descriptor is read from the
        // object's header at trace time. It is not a default at all: it is the
        // tag a *collection* carries for its elements, and `vec_format`,
        // `vec_equals` and `vec_hash` dispatch through exactly that tag. So
        // `lines(`{word}`)` produced a `Vec` of `Text` objects whose element
        // descriptor said `Int`, and rendering it read a `Text` payload through
        // the `Int` callback (IPR-13).
        //
        // Deriving it is only correct because a capture names its own parser
        // body (IP-05, S19). Before that, every capture in a template shared
        // one guessed kind, and reading the child here would have shipped a
        // green test asserting the wrong descriptor.
        match single_anonymous {
            Some(child) => child_descriptor(plan, child),
            None => &scalars::UNIT,
        }
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

    // `split_lines`/`split_sections` are covered by `cursor.rs`'s own tests
    // now: they produce `ByteRegion`s over an `Input`, so their gates live
    // beside the types whose invariants they establish.

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
            let i = unsafe { Input::new(text) }.expect("a Text is UTF-8");
            let out = unsafe { walk(&mut ctx, &i, &plan, plan.root, i.whole()) };
            out.ok().map(|w| (rt, w.value, w.next.offset()))
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

        let result =
            unsafe { run_root(&mut ctx, &plan, input) }.expect("sections(word) should parse");
        let values: Vec<&str> = result.as_vec().iter().map(GcRef::as_text).collect();

        assert_eq!(values, vec!["first", "second"]);
    }

    /// The other half of IPR-03: the owner of a slice is the buffer that was
    /// *parsed*, not whatever the context happens to call its input.
    ///
    /// `parse(text, P)` hands the interpreter a `Text` that is not
    /// `ctx.input_source`. The predecessor read the bytes from the argument and
    /// the owner from the context, so every `Text` a `parse` produced was a
    /// view of the stdin buffer at offsets chosen by a different string.
    #[test]
    fn a_parse_of_a_non_input_text_owns_its_slices() {
        let mut rt = crate::Runtime::new();
        // The context's input is one buffer…
        let stdin_buffer = rt.alloc_text("XXXXXXXXXXXXXXXX");
        // …and the thing being parsed is a different one.
        let subject = rt.alloc_text("alpha beta");
        let mut ctx = rt.context();
        ctx.input_source = stdin_buffer;
        let plan = test_plan(
            vec![
                PlanNode::Atomic {
                    kind: AtomicKind::Word,
                },
                PlanNode::Ws { child: 0 },
            ],
            1,
        );

        let result = unsafe { run_root(&mut ctx, &plan, subject) }.expect("ws(word) should parse");
        let values: Vec<&str> = result.as_vec().iter().map(GcRef::as_text).collect();

        assert_eq!(
            values,
            vec!["alpha", "beta"],
            "a parse's slices must be views of the text it parsed, not of ctx.input_source"
        );
    }

    #[test]
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

        let grid = unsafe { run_root(&mut ctx, &plan, input) }
            .expect("one Unicode scalar is one valid grid cell");
        let payload = unsafe { &*grid.payload::<crate::collections::GridPayload>() };

        assert_eq!(payload.width, 1);
        assert_eq!(payload.items.len(), 1);
    }

    #[test]
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

        let result = unsafe { run_root(&mut ctx, &plan, input) }.expect("csv(rest) should parse");
        let values: Vec<&str> = result.as_vec().iter().map(GcRef::as_text).collect();

        assert_eq!(values, vec!["a", "b"]);
    }

    /// IPR-04's panic path, as a test that would have aborted the process.
    ///
    /// `csv` used to locate a token by searching the region for the token's
    /// text; a token that trims to nothing made that `slice::windows(0)`, which
    /// panics — inside `extern "C"`, where a panic is undefined behaviour.
    /// `"10,20,"` is enough to reach it.
    #[test]
    fn a_csv_token_that_trims_to_empty_does_not_panic() {
        let mut rt = crate::Runtime::new();
        let input = rt.alloc_text("10,20,");
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

        let result = unsafe { run_root(&mut ctx, &plan, input) }
            .expect("an empty csv field is an empty Text, not an abort");
        let values: Vec<&str> = result.as_vec().iter().map(GcRef::as_text).collect();
        assert_eq!(
            values,
            vec!["10", "20", ""],
            "the field after the last comma is empty, and being empty is not a panic"
        );
    }

    // --- M7-WS9: whitespace matcher (§7.2) -----------------------------------

    /// **IPR-07.** `chars` returned `Ok` at the first child failure, so it
    /// silently dropped the rest of its region: `chars(digit, skip: none)` over
    /// `"12x34"` answered `[1, 2]` and reported nothing at all.
    ///
    /// The rule §7.5 wants falls out of running the skip policy once more after
    /// the last match: whatever the skip does not absorb, the child must read.
    #[test]
    fn chars_that_cannot_read_the_whole_region_is_a_parse_failure() {
        fn parse(input: &str, skip: praxis_input_parser::SkipPolicy) -> Option<Vec<i64>> {
            let mut rt = crate::Runtime::new();
            let text = rt.alloc_text(input);
            let mut ctx = rt.context();
            ctx.input_source = text;
            let plan = test_plan(
                vec![
                    PlanNode::Atomic {
                        kind: AtomicKind::Digit,
                    },
                    PlanNode::Characters { child: 0, skip },
                ],
                1,
            );
            unsafe { run_root(&mut ctx, &plan, text) }
                .ok()
                .map(|v| v.as_vec().iter().map(GcRef::as_int).collect())
        }

        use praxis_input_parser::SkipPolicy;
        assert_eq!(parse("1234", SkipPolicy::None), Some(vec![1, 2, 3, 4]));
        assert_eq!(
            parse("12x34", SkipPolicy::None),
            None,
            "a child failure inside the region is the parse's failure, not a short answer"
        );
        // The skip policy is what a trailing run is for, and it is applied
        // after the last match as well as between matches.
        assert_eq!(
            parse("1 2 3 \t", SkipPolicy::Whitespace),
            Some(vec![1, 2, 3])
        );
        assert_eq!(parse("1 2\n", SkipPolicy::Newlines), Some(vec![1, 2]));
        assert_eq!(
            parse("12\n", SkipPolicy::None),
            None,
            "`skip: none` absorbs nothing, so a trailing newline is a mismatch"
        );
    }

    /// **D11's answer to IPR-06, spelled out.** A `grid` cell is one Unicode
    /// scalar, so `grid(int)` reads one digit per cell — a cell parser parses a
    /// cell exactly as it would anywhere else, and the cell is one character.
    ///
    /// This pins the shape the finding named: over `"12\n34\n"` the
    /// predecessor answered **four** cells `[12, 2, 34, 4]`, because it read
    /// the whole token at cell 0 and then read the token's tail again at cell
    /// 1 as well. That is neither semantics. `matrix(int)` is the
    /// whitespace-tokenized constructor, and it is a different one.
    #[test]
    fn a_grid_cell_is_one_scalar_so_grid_int_reads_one_digit_per_cell() {
        let mut rt = crate::Runtime::new();
        let input = rt.alloc_text("12\n34\n");
        let mut ctx = rt.context();
        ctx.input_source = input;
        let plan = test_plan(
            vec![
                PlanNode::Atomic {
                    kind: AtomicKind::Int,
                },
                PlanNode::Grid { child: 0 },
            ],
            1,
        );

        let grid = unsafe { run_root(&mut ctx, &plan, input) }.expect("digits are int cells");
        let payload = unsafe { &*grid.payload::<crate::collections::GridPayload>() };
        assert_eq!(payload.width, 2);
        let cells: Vec<i64> = payload.items.iter().map(|r| r.as_int()).collect();
        assert_eq!(
            cells,
            vec![1, 2, 3, 4],
            "one whole token per cell would be [12, 34]; re-reading the tail was [12, 2, 34, 4]"
        );
    }

    /// **IPR-08.** `scan` used to advance one *byte* at a time, so on a
    /// multi-byte run it attempted a match at continuation bytes — positions
    /// that are not characters at all.
    ///
    /// Over `"ééé"` there are exactly three scalar starts and three
    /// continuation bytes. A byte-stepping `scan` visits six positions; a
    /// scalar-stepping one visits three, and `one_of("é")` matches at each.
    #[test]
    fn scan_advances_by_scalar_across_a_multibyte_run() {
        let mut rt = crate::Runtime::new();
        let input = rt.alloc_text("ééé");
        let mut ctx = rt.context();
        ctx.input_source = input;
        let literals: &'static [&'static str] = Box::leak(vec!["é"].into_boxed_slice());
        let nodes: &'static [PlanNode] = Box::leak(
            vec![
                PlanNode::OneOf { chars_index: 0 },
                PlanNode::Scan { child: 0 },
            ]
            .into_boxed_slice(),
        );
        let plan = ParserPlan {
            nodes,
            template_parts: &[],
            literals,
            root: 1,
        };

        let result = unsafe { run_root(&mut ctx, &plan, input) }.expect("scan never fails");
        let chars: Vec<char> = result
            .as_vec()
            .iter()
            .map(|r| char::from_u32(unsafe { *r.payload::<u32>() }).expect("a Char"))
            .collect();
        assert_eq!(
            chars,
            vec!['é', 'é', 'é'],
            "three scalars, and no attempt at the three continuation bytes between them"
        );
    }

    #[test]
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
