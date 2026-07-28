//! Parser plans: the flat runtime representation of a parser AST.
//!
//! After validation and type synthesis, a [`ParserAst`] is lowered into a
//! [`ParserPlan`] — a flat arena of [`PlanNode`]s that the runtime interpreter
//! (`praxis-runtime::parser`) walks against the input buffer. The compiled plan
//! is registered in a process-wide arena and identified by a
//! [`PlanId`]; MIR passes that id as an `i64` immediate (no
//! pointer-as-immediate needed).
//!
//! Design constraints:
//! - **Flat and self-contained**: nodes reference children by **index** (no
//!   `Box`, no owned `String` on the hot path). Separators and template
//!   literals are interned into a parallel `&'static [&'static str]` slice so
//!   the runtime reads them without dereferencing Rust owned data.
//! - Record schemas for named-capture templates are built at **runtime** (the
//!   interpreter knows the field descriptors from the child plans' result
//!   types); the plan stores only field names as `&'static str`.
//!
//! **Not `#[repr(C)]`.** These are ordinary Rust enums and slices with the
//! default representation, and nothing here crosses an FFI boundary: the plan
//! is consumed by `praxis-runtime`, which is Rust and links against this crate,
//! and only the plan *id* is passed as a JIT immediate. Earlier revisions of
//! this doc claimed a C layout the types never had; if a plan ever does need to
//! be read by generated code, that is a real representation change (explicit
//! `#[repr(C)]`, no enums with payloads, no `&str` fat pointers) and not
//! something to assume from this comment.
//!
//! # Ownership (IP-12)
//!
//! Every plan used to be `Box::leak`ed — the nodes, the template parts, the
//! literals, every field name — and pushed onto an unbounded global `Vec` whose
//! length was narrowed to `u32` with an unchecked `as`. Three things were wrong
//! with that: nothing was ever reclaimable, a long enough compile could wrap
//! the index and hand the runtime a *different* plan, and index `0` doubled as
//! the failure sentinel the HIR emitted when parser analysis failed — so a
//! broken `parse(...)` silently ran plan zero.
//!
//! Now each [`CompiledPlan`] owns a `bumpalo` arena holding everything the
//! plan's `&'static` fields point into, registration is bounded and checked,
//! and a [`PlanId`] is a `NonZeroU32`, so "no plan" is not spelled `0` — it is
//! not spellable at all.
//!
//! Reclamation has the same ordering obligation as the JIT generation arena:
//! record schemas the runtime builds for named-capture templates borrow their
//! field names from plan storage, so plans may only be retired once the heap
//! is drained. `praxis_runtime::retire_parser_plans` is the gate; see
//! [`retire_all_plans`].

use std::num::NonZeroU32;
use std::sync::Mutex;

use bumpalo::Bump;

use crate::ast::{AtomicKind, ParserAst, SkipPolicy, TemplatePart, WsPolicy};

// ===========================================================================
// The flat plan node arena.
// ===========================================================================

/// One node in the flattened parser plan. Index-based: children refer to other
/// nodes by their position in the `ParserPlan::nodes` slice.
#[derive(Debug)]
pub enum PlanNode {
    /// An atomic parser.
    Atomic { kind: AtomicKind },
    /// `lines(P)`.
    Lines { child: u32 },
    /// `sections(P)` (homogeneous).
    Sections { child: u32 },
    /// Named heterogeneous `sections(name: P, ..., tail: repeated(P))` (M9).
    /// `fields` are `(name, child_index)` pairs; `repeated_tail` is the named
    /// tail field's `(name, child_index)`, if present.
    SectionsNamed {
        fields: &'static [(&'static str, u32)],
        repeated_tail: Option<(&'static str, u32)>,
    },
    /// `block(item, ...)` (M9, §7.5). Sequential parsers within one region;
    /// positional named-capture templates flatten their fields into the result
    /// record, named items contribute one field each.
    Block { items: &'static [BlockItemNode] },
    /// `choice(Name: P, ...)` (M9, §7.5). Try each case in source order; the
    /// first match wins and its value becomes the variant's payload. `cases`
    /// are `(name, child_index)` pairs.
    Choice {
        cases: &'static [(&'static str, u32)],
    },
    /// `optional(P)` (M9, §7.5). Parse `P`; on success return Some(value)
    /// (Option tag 0), on failure consume nothing and return None (tag 1).
    Optional { child: u32 },
    /// `scan(P)` (M9, §7.5). Slide a cursor; at each position try `P`; collect
    /// matches in source order, ignoring unmatched text.
    Scan { child: u32 },
    /// `one_of("LR")` (M9, §7.5). `chars_index` is into `ParserPlan::literals`.
    OneOf { chars_index: u32 },
    /// `chars(P, skip:)` (M9, §7.5). Apply a char-parser repeatedly.
    Characters { child: u32, skip: SkipPolicy },
    /// `matrix(P)` (M9, §7.5, ADR-030). Whitespace-tokenized rectangular Grid.
    Matrix { child: u32 },
    /// Ragged `grid(P, ragged, fill:)` (M9, §7.5). `fill_index` into literals.
    GridRagged { child: u32, fill_index: u32 },
    /// `csv(P)`.
    Csv { child: u32 },
    /// `ws(P)`.
    Ws { child: u32 },
    /// `sep(separator_index, P)`.
    Sep { separator_index: u32, child: u32 },
    /// `grid(P)`.
    Grid { child: u32 },
    /// A backtick template. `parts` are indices into [`ParserPlan::template_parts`].
    Template { parts: &'static [TemplatePartNode] },
    /// A tuple of captures (anonymous, ≥2) — the result is assembled element-wise.
    Tuple { elements: &'static [u32] },
}

/// One part of a template, in plan form.
#[derive(Debug)]
pub enum TemplatePartNode {
    /// A literal match.
    Literal { text: &'static str, ws: WsPolicy },
    /// A capture whose value comes from the child plan node. `field_index` is the
    /// position in the resulting record/tuple (None for single-capture scalars).
    Capture {
        child: u32,
        field_index: Option<u16>,
        name: Option<&'static str>,
    },
}

/// One item of a `block(...)` (M9, §7.5), in plan form.
#[derive(Debug)]
pub enum BlockItemNode {
    /// A positional parser. A named-capture template's fields flatten into the
    /// block record (the runtime reads the record's fields from the produced
    /// value); any other positional must have been named (validation rejects).
    Positional { child: u32 },
    /// A named item contributing one field.
    Named { name: &'static str, child: u32 },
}

/// A compiled parser plan: the node arena plus auxiliary interned data.
pub struct ParserPlan {
    /// The flat node arena, indexed by `PlanNode` child references.
    pub nodes: &'static [PlanNode],
    /// Template literal/capture parts, referenced by `PlanNode::Template`.
    pub template_parts: &'static [TemplatePartNode],
    /// Interned string literals (separators, template literals), so the runtime
    /// reads `&'static str` without touching Rust owned data.
    pub literals: &'static [&'static str],
    /// The root node index (entry point).
    pub root: u32,
}

impl std::fmt::Debug for ParserPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParserPlan")
            .field("nodes", &self.nodes)
            .field("template_parts_len", &self.template_parts.len())
            .field("literals", &self.literals)
            .field("root", &self.root)
            .finish()
    }
}

// ===========================================================================
// Lowering: ParserAst → &'static ParserPlan
// ===========================================================================

/// A builder that accumulates plan nodes into owned Vecs, then moves them into
/// the compiled plan's arena in [`PlanBuilder::finish`].
struct PlanBuilder<'a> {
    arena: &'a Bump,
    nodes: Vec<PlanNode>,
    template_parts: Vec<TemplatePartNode>,
    literals: Vec<&'static str>,
}

impl<'a> PlanBuilder<'a> {
    fn new(arena: &'a Bump) -> Self {
        PlanBuilder {
            arena,
            nodes: Vec::new(),
            template_parts: Vec::new(),
            literals: Vec::new(),
        }
    }

    /// Push a node and return its index.
    fn push_node(&mut self, node: PlanNode) -> u32 {
        let idx = self.nodes.len() as u32;
        self.nodes.push(node);
        idx
    }

    /// Intern a literal string, returning its index. The input `s` must already
    /// live in this plan's arena (see [`PlanBuilder::alloc_str`]).
    fn intern_literal(&mut self, s: &'static str) -> u32 {
        let idx = self.literals.len() as u32;
        self.literals.push(s);
        idx
    }

    /// Copy `s` into the plan's arena.
    fn alloc_str(&self, s: &str) -> &'static str {
        alloc_str(self.arena, s)
    }

    /// Move a `Vec<T>` into the plan's arena.
    fn alloc_slice<T>(&self, v: Vec<T>) -> &'static [T] {
        alloc_slice(self.arena, v)
    }

    /// Move the accumulated data into the arena and return the plan.
    fn finish(self, root: u32) -> &'static ParserPlan {
        let PlanBuilder {
            arena,
            nodes,
            template_parts,
            literals,
        } = self;
        let plan: &ParserPlan = arena.alloc(ParserPlan {
            nodes: alloc_slice(arena, nodes),
            template_parts: alloc_slice(arena, template_parts),
            literals: alloc_slice(arena, literals),
            root,
        });
        // SAFETY: see `alloc_str`.
        unsafe { &*(plan as *const ParserPlan) }
    }
}

/// Copy `s` into `arena`, erasing the lifetime to `'static`.
///
/// The `'static` is a lie the plan's own field types force: `PlanNode` and
/// `ParserPlan` declare `&'static str`. The truth is *arena* lifetime, and
/// [`CompiledPlan`] is what keeps the arena alive exactly as long as the plan.
/// This function and [`alloc_slice`] are the only two places the erasure
/// happens, and [`retire_all_plans`]'s safety contract is what discharges it.
fn alloc_str(arena: &Bump, s: &str) -> &'static str {
    let stored: &str = arena.alloc_str(s);
    // SAFETY: the bytes live in the arena the `CompiledPlan` owns alongside the
    // plan, and both are released together by `retire_all`.
    unsafe { &*(stored as *const str) }
}

/// Move a `Vec<T>` into `arena`. Same lifetime erasure as [`alloc_str`].
fn alloc_slice<T>(arena: &Bump, v: Vec<T>) -> &'static [T] {
    let stored: &[T] = arena.alloc_slice_fill_iter(v);
    // SAFETY: as `alloc_str`.
    unsafe { &*(stored as *const [T]) }
}

/// A lowered plan together with the arena that owns everything it points at.
///
/// Self-referential by construction — `plan` addresses storage inside `arena` —
/// which is sound because `bumpalo` keeps its chunks on the heap: moving a
/// `CompiledPlan` moves the `Bump` *handle*, never the bytes.
pub struct CompiledPlan {
    /// Owns the nodes, template parts, literals and every interned string. It
    /// is never read — its whole job is to keep `plan`'s storage alive and to
    /// release it on drop, which is what makes a plan reclaimable at all.
    #[allow(dead_code)]
    arena: Bump,
    plan: *const ParserPlan,
}

impl CompiledPlan {
    /// The compiled plan.
    pub fn plan(&self) -> &ParserPlan {
        // SAFETY: `plan` was allocated in `self.arena`, which this borrow keeps
        // alive, and is never mutated after construction.
        unsafe { &*self.plan }
    }
}

impl std::fmt::Debug for CompiledPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.plan().fmt(f)
    }
}

// SAFETY: `CompiledPlan` owns everything it points at, and nothing inside is
// shared with another thread until it is registered behind the arena's mutex.
// The raw pointer is what makes the auto-impl fail; the data is plain,
// immutable, and self-contained.
unsafe impl Send for CompiledPlan {}

/// Lower a validated `ParserAst` into a self-owning [`CompiledPlan`].
///
/// The caller registers it with [`register_plan`] to get the [`PlanId`] MIR
/// embeds.
///
/// # Panics
/// Only on an internal inconsistency (the AST should have passed validation).
pub fn lower_to_plan(ast: &ParserAst) -> CompiledPlan {
    let arena = Bump::new();
    let plan = {
        let mut b = PlanBuilder::new(&arena);
        let root = lower_node(&mut b, ast);
        b.finish(root) as *const ParserPlan
    };
    CompiledPlan { arena, plan }
}

// ===========================================================================
// The plan arena: maps `PlanId`s (passed as an i64 immediate through MIR) to
// their compiled plans. Lives here (not in HIR) so both HIR (which registers)
// and the runtime interpreter (which looks up) depend on this crate without
// creating a dependency cycle.
// ===========================================================================

/// The identity of a registered parser plan.
///
/// `NonZeroU32` on purpose: the HIR used to emit `plan_index: 0` when parser
/// analysis *failed*, which is indistinguishable from the first successfully
/// registered plan. There is now no `u32` that means "no plan", so that
/// encoding is unwritable — a failed analysis lowers to an error expression
/// instead.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PlanId(NonZeroU32);

impl PlanId {
    /// The raw id, for embedding as a MIR immediate.
    pub fn get(self) -> u32 {
        self.0.get()
    }

    /// Recover a `PlanId` from a raw value, rejecting `0`.
    ///
    /// The runtime calls this on the integer it reads back out of a boxed `Int`
    /// argument: a value that never named a plan must become a parse fault, not
    /// an index into the arena.
    pub fn from_raw(raw: u32) -> Option<PlanId> {
        NonZeroU32::new(raw).map(PlanId)
    }
}

/// Registration refused: the process has compiled more parser plans than the
/// arena admits.
///
/// This is a compile-time registration bound, not a language limit — one plan
/// per `read`/`parse` expression per compile. Reaching it means something is
/// recompiling in a loop, and saying so beats wrapping the index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TooManyPlans {
    /// The bound that was hit.
    pub limit: usize,
}

impl std::fmt::Display for TooManyPlans {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "too many parser plans registered in one process (limit {})",
            self.limit
        )
    }
}

impl std::error::Error for TooManyPlans {}

/// The registration bound. Chosen so an id always fits a `u32` with room to
/// spare, and so a runaway registration loop is caught long before the
/// narrowing could matter.
pub const MAX_PLANS: usize = 1 << 20;

/// The narrowing `register_plan` performs is checked *and* provable: an id is
/// `len()` after a push, which the bound keeps strictly inside `u32`.
const _: () = assert!(MAX_PLANS < u32::MAX as usize);

/// The process-wide store. Index `i` holds the plan with id `i + 1`, which is
/// what keeps [`PlanId`] non-zero.
static PLAN_ARENA: Mutex<Vec<CompiledPlan>> = Mutex::new(Vec::new());

/// Register a compiled plan, returning its id.
///
/// # Errors
/// [`TooManyPlans`] once [`MAX_PLANS`] plans have been registered. The
/// predecessor pushed unconditionally and narrowed `Vec::len()` with an
/// unchecked `as u32` (IP-12).
pub fn register_plan(plan: CompiledPlan) -> Result<PlanId, TooManyPlans> {
    register_with_limit(plan, MAX_PLANS)
}

/// [`register_plan`] with the bound as a parameter, so a test can reach the
/// refusal without registering a million plans.
fn register_with_limit(plan: CompiledPlan, limit: usize) -> Result<PlanId, TooManyPlans> {
    let mut arena = PLAN_ARENA
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if arena.len() >= limit {
        // Refuse *before* pushing, so a rejected registration leaves the arena
        // exactly as it found it.
        return Err(TooManyPlans { limit });
    }
    arena.push(plan);
    let raw = u32::try_from(arena.len()).map_err(|_| TooManyPlans { limit })?;
    Ok(PlanId(
        NonZeroU32::new(raw).expect("len is at least 1 after the push"),
    ))
}

/// Look up a plan by id. `None` if the id names no registered plan (the caller
/// treats that as a parse fault).
pub fn get_plan(id: PlanId) -> Option<&'static ParserPlan> {
    let arena = PLAN_ARENA
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let plan: *const ParserPlan = arena.get(id.get() as usize - 1)?.plan;
    // SAFETY: registered plans are never removed except by `retire_all_plans`, whose
    // caller has proven the heap is drained; the storage lives in the
    // `CompiledPlan`'s arena, on the heap, so the address is stable across the
    // `Vec`'s own reallocations.
    Some(unsafe { &*plan })
}

/// How many plans are registered. For tests and diagnostics.
pub fn plan_count() -> usize {
    PLAN_ARENA
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len()
}

/// Drop every registered plan, releasing its arena.
///
/// # Safety
/// Every `&'static str` a plan handed out must be dead. In particular the
/// runtime builds record schemas for named-capture templates whose field names
/// *borrow from plan storage*, and caches them; those caches must be cleared in
/// the same breath. `praxis_runtime::retire_parser_plans` is the only intended
/// caller and does exactly that, behind a `HeapDrained` proof.
pub unsafe fn retire_all_plans() {
    PLAN_ARENA
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// Lower one node, returning its index in the plan arena.
fn lower_node(b: &mut PlanBuilder<'_>, ast: &ParserAst) -> u32 {
    match ast {
        ParserAst::Atomic { kind, .. } => b.push_node(PlanNode::Atomic { kind: *kind }),
        ParserAst::Lines { child, .. } => {
            let c = lower_node(b, child);
            b.push_node(PlanNode::Lines { child: c })
        }
        ParserAst::Sections { child, .. } => {
            let c = lower_node(b, child);
            b.push_node(PlanNode::Sections { child: c })
        }
        ParserAst::SectionsNamed {
            fields,
            repeated_tail,
            ..
        } => {
            // Lower each field's child, recording (name, child_index).
            let field_entries: Vec<(&'static str, u32)> = fields
                .iter()
                .map(|(name, p)| {
                    let n = b.alloc_str(name);
                    let c = lower_node(b, p);
                    (n, c)
                })
                .collect();
            let tail_entry = repeated_tail.as_ref().map(|(name, p)| {
                let n = b.alloc_str(name);
                let c = lower_node(b, p);
                (n, c)
            });
            let field_slice = b.alloc_slice(field_entries);
            b.push_node(PlanNode::SectionsNamed {
                fields: field_slice,
                repeated_tail: tail_entry,
            })
        }
        ParserAst::Csv { child, .. } => {
            let c = lower_node(b, child);
            b.push_node(PlanNode::Csv { child: c })
        }
        ParserAst::Ws { child, .. } => {
            let c = lower_node(b, child);
            b.push_node(PlanNode::Ws { child: c })
        }
        ParserAst::Sep {
            separator, child, ..
        } => {
            let sep_static: &'static str = b.alloc_str(separator);
            let sep_idx = b.intern_literal(sep_static);
            let c = lower_node(b, child);
            b.push_node(PlanNode::Sep {
                separator_index: sep_idx,
                child: c,
            })
        }
        ParserAst::Grid { child, .. } => {
            let c = lower_node(b, child);
            b.push_node(PlanNode::Grid { child: c })
        }
        ParserAst::Block { items, .. } => {
            let item_nodes: Vec<BlockItemNode> = items
                .iter()
                .map(|item| match item {
                    crate::ast::BlockItem::Positional(p) => BlockItemNode::Positional {
                        child: lower_node(b, p),
                    },
                    crate::ast::BlockItem::Named { name, parser } => BlockItemNode::Named {
                        name: b.alloc_str(name),
                        child: lower_node(b, parser),
                    },
                })
                .collect();
            let items_slice = b.alloc_slice(item_nodes);
            b.push_node(PlanNode::Block { items: items_slice })
        }
        ParserAst::Choice { cases, .. } => {
            let case_entries: Vec<(&'static str, u32)> = cases
                .iter()
                .map(|(name, p)| {
                    let n = b.alloc_str(name);
                    let c = lower_node(b, p);
                    (n, c)
                })
                .collect();
            let cases_slice = b.alloc_slice(case_entries);
            b.push_node(PlanNode::Choice { cases: cases_slice })
        }
        ParserAst::Optional { child, .. } => {
            let c = lower_node(b, child);
            b.push_node(PlanNode::Optional { child: c })
        }
        ParserAst::Scan { child, .. } => {
            let c = lower_node(b, child);
            b.push_node(PlanNode::Scan { child: c })
        }
        ParserAst::OneOf { chars, .. } => {
            let chars_static = b.alloc_str(chars);
            let idx = b.intern_literal(chars_static);
            b.push_node(PlanNode::OneOf { chars_index: idx })
        }
        ParserAst::Characters { child, skip, .. } => {
            let c = lower_node(b, child);
            b.push_node(PlanNode::Characters {
                child: c,
                skip: *skip,
            })
        }
        ParserAst::Matrix { child, .. } => {
            let c = lower_node(b, child);
            b.push_node(PlanNode::Matrix { child: c })
        }
        ParserAst::GridRagged { child, fill, .. } => {
            let c = lower_node(b, child);
            let fill_static = b.alloc_str(fill);
            let fill_idx = b.intern_literal(fill_static);
            b.push_node(PlanNode::GridRagged {
                child: c,
                fill_index: fill_idx,
            })
        }
        ParserAst::Template { parts, .. } => lower_template(b, parts),
    }
}

/// Lower a template: collect its parts, possibly build a record schema for
/// named captures, and emit a `PlanNode::Template` (or a scalar/tuple).
fn lower_template(b: &mut PlanBuilder<'_>, parts: &[TemplatePart]) -> u32 {
    // Collect captures to decide scalar / tuple / record.
    let captures: Vec<(usize, &TemplatePart)> = parts
        .iter()
        .enumerate()
        .filter(|(_, p)| matches!(p, TemplatePart::Capture { .. }))
        .collect();

    if captures.is_empty() {
        // No captures → the template matches literally, producing Unit. We still
        // emit the parts so the runtime can match them.
        let part_indices = lower_template_parts(b, parts, &[]);
        return b.push_node(PlanNode::Template {
            parts: part_indices,
        });
    }

    let any_named = captures
        .iter()
        .any(|(_, p)| matches!(p, TemplatePart::Capture { name: Some(_), .. }));

    // Lower each capture's child parser and record its node index.
    let part_indices = lower_template_parts(b, parts, &captures);

    if any_named {
        // Named captures → record. The record schema (field names + descriptors)
        // is built at runtime by the interpreter, which knows the child result
        // types. The plan stores field names in the capture parts.
        b.push_node(PlanNode::Template {
            parts: part_indices,
        })
    } else {
        // Single anonymous capture → scalar; multiple anonymous captures → tuple.
        // Both lower to a `Template` node (preserving the literal parts between
        // captures, so the runtime can match the separators). The interpreter
        // assembles a scalar (1 capture) or a tuple (≥2 captures) from the
        // captured values.
        b.push_node(PlanNode::Template {
            parts: part_indices,
        })
    }
}

/// Lower template parts into the `template_parts` arena, returning a static
/// slice reference. `captures` lets us assign field indices.
fn lower_template_parts(
    b: &mut PlanBuilder<'_>,
    parts: &[TemplatePart],
    captures: &[(usize, &TemplatePart)],
) -> &'static [TemplatePartNode] {
    let mut nodes = Vec::new();
    for part in parts {
        match part {
            TemplatePart::Literal { text, ws } => {
                let text_static = b.alloc_str(text);
                nodes.push(TemplatePartNode::Literal {
                    text: text_static,
                    ws: *ws,
                });
            }
            TemplatePart::Capture { name, parser } => {
                let child = lower_node(b, parser);
                let field_index = captures
                    .iter()
                    .position(|(_, p)| std::ptr::eq(*p, part))
                    .map(|i| i as u16);
                let name_static = name.as_ref().map(|n| b.alloc_str(n));
                nodes.push(TemplatePartNode::Capture {
                    child,
                    field_index,
                    name: name_static,
                });
            }
        }
    }
    b.alloc_slice(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{AtomicKind, TemplatePart, WsPolicy};
    use praxis_source::Span;

    #[test]
    fn atomic_lower_to_plan() {
        let ast = ParserAst::Atomic {
            kind: AtomicKind::Int,
            span: Span::at(0),
        };
        let compiled = lower_to_plan(&ast);
        let plan = compiled.plan();
        assert_eq!(plan.root, 0);
        assert!(matches!(
            plan.nodes[0],
            PlanNode::Atomic {
                kind: AtomicKind::Int
            }
        ));
    }

    #[test]
    fn lines_of_int_lower_to_plan() {
        let ast = ParserAst::Lines {
            child: Box::new(ParserAst::Atomic {
                kind: AtomicKind::Int,
                span: Span::at(0),
            }),
            span: Span::at(0),
        };
        let compiled = lower_to_plan(&ast);
        let plan = compiled.plan();
        // Children are lowered first (lower index); the parent (Lines) is root.
        assert!(matches!(
            plan.nodes[0],
            PlanNode::Atomic {
                kind: AtomicKind::Int
            }
        ));
        assert!(matches!(
            plan.nodes[plan.root as usize],
            PlanNode::Lines { child: 0 }
        ));
    }

    #[test]
    fn sep_lower_interns_separator() {
        let ast = ParserAst::Sep {
            separator: " -> ".to_string(),
            child: Box::new(ParserAst::Atomic {
                kind: AtomicKind::Word,
                span: Span::at(0),
            }),
            span: Span::at(0),
        };
        let compiled = lower_to_plan(&ast);
        let plan = compiled.plan();
        match plan.nodes[plan.root as usize] {
            PlanNode::Sep {
                separator_index, ..
            } => {
                assert_eq!(plan.literals[separator_index as usize], " -> ");
            }
            _ => panic!("expected Sep at root"),
        }
    }

    /// A `PlanId` cannot be zero, so the HIR's old `plan_index: 0` failure
    /// sentinel has no encoding (IP-12).
    #[test]
    fn zero_is_not_a_plan_id() {
        assert!(PlanId::from_raw(0).is_none());
        assert_eq!(PlanId::from_raw(1).map(PlanId::get), Some(1));
    }

    /// Registration hands out non-zero ids that round-trip through the raw
    /// `u32` MIR embeds, and each one resolves to the plan it named.
    #[test]
    fn registered_plans_round_trip_through_their_raw_id() {
        let first = register_plan(lower_to_plan(&ParserAst::Atomic {
            kind: AtomicKind::Int,
            span: Span::at(0),
        }))
        .expect("the arena is far from full");
        let second = register_plan(lower_to_plan(&ParserAst::Atomic {
            kind: AtomicKind::Word,
            span: Span::at(0),
        }))
        .expect("the arena is far from full");
        assert_ne!(first, second);
        for (id, expected) in [(first, AtomicKind::Int), (second, AtomicKind::Word)] {
            let raw = id.get();
            assert!(raw > 0, "a plan id is never zero");
            let recovered = PlanId::from_raw(raw).expect("a registered id is non-zero");
            let plan = get_plan(recovered).expect("a registered plan resolves");
            assert!(
                matches!(plan.nodes[plan.root as usize], PlanNode::Atomic { kind } if kind == expected)
            );
        }
    }

    /// Registration is bounded and refuses cleanly: the caller gets a
    /// diagnostic instead of a wrapped index (IP-12). The predecessor pushed
    /// unconditionally and narrowed `Vec::len()` with an unchecked `as u32`.
    ///
    /// The bound is a parameter here only so the test need not register a
    /// million plans; a limit of zero refuses deterministically regardless of
    /// what else this test binary has registered in parallel.
    #[test]
    fn registration_past_the_bound_is_refused() {
        let atom = || ParserAst::Atomic {
            kind: AtomicKind::Int,
            span: Span::at(0),
        };
        let refused = register_with_limit(lower_to_plan(&atom()), 0)
            .expect_err("a zero limit admits no plans at all");
        assert_eq!(refused.limit, 0);
        assert!(refused.to_string().contains("too many parser plans"));
        // The refusal happens before the push, so it consumed nothing: an
        // ordinary registration still succeeds and still yields a usable id.
        let accepted = register_plan(lower_to_plan(&atom())).expect("the real arena has room");
        assert!(get_plan(accepted).is_some());
    }

    /// An id past the end of the arena resolves to nothing rather than
    /// indexing. The runtime turns that `None` into a parse fault.
    #[test]
    fn an_unregistered_id_resolves_to_nothing() {
        let beyond = PlanId::from_raw(u32::MAX).expect("non-zero");
        assert!(get_plan(beyond).is_none());
    }

    /// The plan's storage really is owned: an interned literal survives being
    /// read back out of the arena, and the `CompiledPlan` is what keeps it
    /// alive (the predecessor `Box::leak`ed it instead).
    #[test]
    fn a_compiled_plan_owns_its_interned_strings() {
        let compiled = lower_to_plan(&ParserAst::Sep {
            separator: " -> ".to_string(),
            child: Box::new(ParserAst::Atomic {
                kind: AtomicKind::Word,
                span: Span::at(0),
            }),
            span: Span::at(0),
        });
        assert_eq!(compiled.plan().literals, &[" -> "]);
    }

    #[test]
    fn template_literal_lower_to_plan() {
        let ast = ParserAst::Template {
            parts: vec![
                TemplatePart::Capture {
                    name: None,
                    parser: Box::new(ParserAst::Atomic {
                        kind: AtomicKind::Int,
                        span: Span::at(0),
                    }),
                },
                TemplatePart::Literal {
                    text: ",".to_string(),
                    ws: WsPolicy::SpaceRun,
                },
                TemplatePart::Capture {
                    name: None,
                    parser: Box::new(ParserAst::Atomic {
                        kind: AtomicKind::Int,
                        span: Span::at(0),
                    }),
                },
            ],
            span: Span::at(0),
        };
        let compiled = lower_to_plan(&ast);
        let plan = compiled.plan();
        // Two anonymous captures → a Template node at root (the literals between
        // captures are preserved so the runtime can match the separators; the
        // interpreter assembles a tuple from the captured values). The root is
        // the last-pushed node.
        assert!(matches!(
            plan.nodes[plan.root as usize],
            PlanNode::Template { .. }
        ));
    }
}
