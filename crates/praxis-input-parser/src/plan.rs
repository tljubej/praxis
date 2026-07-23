//! Parser plans: the flat, `#[repr(C)]` runtime representation of a parser AST.
//!
//! After validation and type synthesis, a [`ParserAst`] is lowered into a
//! [`ParserPlan`] — a flat arena of [`PlanNode`]s that the runtime interpreter
//! (`praxis-runtime::parser`) walks against the input buffer. The plan is
//! **leaked to `&'static`** and registered in a global slab; MIR passes its
//! index as an `i64` immediate (no pointer-as-immediate needed).
//!
//! Design constraints:
//! - `#[repr(C)]` and self-contained: nodes reference children by **index**
//!   (no `Box`, no owned `String` on the hot path). Separators and template
//!   literals are interned into a parallel `&'static [&'static str]` slice so
//!   the runtime reads them without dereferencing Rust owned data.
//! - Record schemas for named-capture templates are built at **runtime** (the
//!   interpreter knows the field descriptors from the child plans' result
//!   types); the plan stores only field names as `&'static str`.
//!
//! This mirrors how the JIT already leaks function-name strings as `String` in
//! `CallTarget::User` — acceptable for a JIT process.

use crate::ast::{AtomicKind, ParserAst, TemplatePart, WsPolicy};

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

/// A builder that accumulates plan nodes into owned Vecs, then leaks them into
/// `&'static` slices in [`PlanBuilder::finish`].
struct PlanBuilder {
    nodes: Vec<PlanNode>,
    template_parts: Vec<TemplatePartNode>,
    literals: Vec<&'static str>,
}

impl PlanBuilder {
    fn new() -> Self {
        PlanBuilder {
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
    /// be `'static` (leaked by the caller or embedded in the AST).
    fn intern_literal(&mut self, s: &'static str) -> u32 {
        let idx = self.literals.len() as u32;
        self.literals.push(s);
        idx
    }

    /// Leak the accumulated data into `&'static` slices and return the plan.
    fn finish(self, root: u32) -> &'static ParserPlan {
        let nodes = leak(self.nodes);
        let template_parts = leak(self.template_parts);
        let literals = leak(self.literals);
        Box::leak(Box::new(ParserPlan {
            nodes,
            template_parts,
            literals,
            root,
        }))
    }
}

/// Leak a `Vec<T>` into a `&'static [T]`.
fn leak<T>(v: Vec<T>) -> &'static [T] {
    Box::leak(v.into_boxed_slice())
}

/// Lower a validated `ParserAst` into a `&'static ParserPlan`, register it in
/// the global plan slab, and return its index. The returned reference lives for
/// the process lifetime (JIT process — acceptable leak).
///
/// # Panics
/// Only on an internal inconsistency (the AST should have passed validation).
pub fn lower_to_plan(ast: &ParserAst) -> &'static ParserPlan {
    let mut b = PlanBuilder::new();
    let root = lower_node(&mut b, ast);
    b.finish(root)
}

// ===========================================================================
// The global plan slab: maps plan indices (passed as i64 through MIR) to their
// compiled `&'static ParserPlan`. Lives here (not in HIR) so both HIR (which
// registers) and the runtime interpreter (which looks up) depend on this crate
// without creating a dependency cycle.
// ===========================================================================

/// A wrapper asserting `Send + Sync` — the plan's raw pointers point at
/// process-static descriptor data, so sharing across the compile/run boundary is
/// safe.
struct PlanEntry(&'static ParserPlan);
unsafe impl Send for PlanEntry {}
unsafe impl Sync for PlanEntry {}

static PLAN_SLAB: std::sync::Mutex<Vec<PlanEntry>> = std::sync::Mutex::new(Vec::new());

/// Register a plan in the global slab, returning its index.
pub fn register_plan(plan: &'static ParserPlan) -> u32 {
    let mut slab = PLAN_SLAB.lock().unwrap();
    let idx = slab.len() as u32;
    slab.push(PlanEntry(plan));
    idx
}

/// Look up a plan by its index. Returns `None` if out of range (the caller
/// treats this as a parse fault).
pub fn get_plan(index: u32) -> Option<&'static ParserPlan> {
    PLAN_SLAB.lock().ok()?.get(index as usize).map(|e| e.0)
}

/// Lower one node, returning its index in the plan arena.
fn lower_node(b: &mut PlanBuilder, ast: &ParserAst) -> u32 {
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
            let sep_static: &'static str = leak_str(separator);
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
        ParserAst::Template { parts, .. } => lower_template(b, parts),
    }
}

/// Leak an owned `String` into `&'static str`.
fn leak_str(s: &str) -> &'static str {
    // `s` is borrowed; we must copy it into a leaked allocation.
    let boxed: Box<str> = s.into();
    Box::leak(boxed)
}

/// Lower a template: collect its parts, possibly build a record schema for
/// named captures, and emit a `PlanNode::Template` (or a scalar/tuple).
fn lower_template(b: &mut PlanBuilder, parts: &[TemplatePart]) -> u32 {
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
    } else if captures.len() == 1 {
        // Single anonymous capture → scalar. The template node carries the one
        // capture; the runtime returns its value directly.
        b.push_node(PlanNode::Template {
            parts: part_indices,
        })
    } else {
        // Multiple anonymous captures → tuple. Emit a Tuple node whose elements
        // are the capture child nodes.
        let elements: Vec<u32> = captures
            .iter()
            .map(|(_, p)| match p {
                TemplatePart::Capture { parser, .. } => lower_node(b, parser),
                _ => unreachable!(),
            })
            .collect();
        b.push_node(PlanNode::Tuple {
            elements: leak(elements),
        })
    }
}

/// Lower template parts into the `template_parts` arena, returning a static
/// slice reference. `captures` lets us assign field indices.
fn lower_template_parts(
    b: &mut PlanBuilder,
    parts: &[TemplatePart],
    captures: &[(usize, &TemplatePart)],
) -> &'static [TemplatePartNode] {
    let mut nodes = Vec::new();
    for part in parts {
        match part {
            TemplatePart::Literal { text, ws } => {
                let text_static = leak_str(text);
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
                let name_static = name.as_ref().map(|n| leak_str(n));
                nodes.push(TemplatePartNode::Capture {
                    child,
                    field_index,
                    name: name_static,
                });
            }
        }
    }
    leak(nodes)
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
        let plan = lower_to_plan(&ast);
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
        let plan = lower_to_plan(&ast);
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
        let plan = lower_to_plan(&ast);
        match plan.nodes[plan.root as usize] {
            PlanNode::Sep {
                separator_index, ..
            } => {
                assert_eq!(plan.literals[separator_index as usize], " -> ");
            }
            _ => panic!("expected Sep at root"),
        }
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
        let plan = lower_to_plan(&ast);
        // Two anonymous captures → Tuple node at root.
        assert!(matches!(plan.nodes.last(), Some(PlanNode::Tuple { .. })));
    }
}
