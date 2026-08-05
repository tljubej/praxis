//! Exhaustiveness and reachability for `match` expressions (§4.6), as a
//! **usefulness matrix** (HIR-06).
//!
//! A `match` must cover every value of its scrutinee type; a match that does
//! not is `Y120`, and an arm an earlier arm already covers is `Y121`.
//!
//! # Why a matrix
//!
//! Both questions are one question — *is this pattern useful against the ones
//! above it?* — and the two ad-hoc walks this replaces could each only ask a
//! flattened version of it:
//!
//! - `uncovered_constructors` compared the set of **top-level** variant indices
//!   an arm named against the enum's variants, so `match w { Wrap(On) => 1 }`
//!   over `enum Wrapped { Wrap(Flag) }` was "exhaustive": `Wrap` is covered, and
//!   nothing looked at whether the payload's `Off` was.
//! - the `pattern_catches_all` scan reported an arm unreachable only when an
//!   *earlier* arm was a bare `_` or a bind, so a repeated constructor —
//!   `match e { A => 1, A => 2, B => 3 }` — was silently dead code.
//!
//! # The algorithm
//!
//! Maranget's usefulness check, in its standard three cases. A *matrix* is the
//! arms seen so far as rows of patterns; a *query* `q` is the row being asked
//! about. [`useful`] returns the value shapes `q` matches that no row does —
//! empty means "not useful".
//!
//! - **`q`'s head is a constructor `c`**: keep only the rows that can match a
//!   `c` value, splice each one's fields into its head position, and recurse.
//! - **`q`'s head is a wildcard and the first column's constructors are
//!   complete** (every variant of the enum, both `Bool`s): recurse once per
//!   constructor and take the union.
//! - **`q`'s head is a wildcard and some constructor is missing**: recurse on
//!   the *default* matrix (the rows whose head matches anything) and prepend a
//!   missing constructor — or `_`, when the type has no enumerable signature.
//!
//! Exhaustiveness is then "is `_` still useful against every arm?", and arm `i`
//! is unreachable when its own pattern is not useful against arms `0..i`. Both
//! recurse into payloads, which is the whole of HIR-06.
//!
//! Termination: the wildcard-complete case only fires when every constructor
//! appears *literally* in the first column, so it can recurse no deeper than
//! real constructor patterns nest in the source.

use praxis_ast::AstNode;
use praxis_source::{Diagnostic, FileId, FileSpan, Span};
use praxis_syntax::SyntaxKind;
use praxis_types::{data::TypeData, EnumDefId, RecordDefId, ScalarType, Type, TypeDb};
use rowan::NodeOrToken;

use crate::diagnostics::{non_exhaustive, unreachable_arm};
use crate::lower::{Lit, TypedMatchArm, TypedPattern};

/// The wildcard the matrix uses for every padded field and every query it
/// invents. A `static` so a row can stay a slice of borrowed patterns: the
/// checker never owns a pattern the lowering did not write.
static WILDCARD: TypedPattern = TypedPattern::Wildcard;

/// How many uncovered shapes one `Y120` names. A match over a wide enum has no
/// use for forty of them, and the recursion stops as soon as it has this many.
const MAX_WITNESSES: usize = 3;

/// A zero-length span at position 0 — the last-resort fallback when a caller
/// supplies no span at all.
///
/// It used to be where *every* `Y120` pointed: a non-exhaustive match was
/// reported at byte 0 of the file, which for a program with two matches names
/// neither of them (HIR-07). `check` takes the match's own span now.
fn zero_span(file: FileId) -> FileSpan {
    FileSpan::new(
        file,
        Span::new(
            praxis_source::BytePos::from(0u32),
            praxis_source::BytePos::from(0u32),
        ),
    )
}

/// Check **every** `match` in the file, at the end of analysis (ADR-130).
///
/// This is where `Y120`/`Y121` are decided. It used to be lowering, which meant
/// coverage was asked only where MIR was being built: `praxis check` was silent
/// on a non-exhaustive match and `praxis run` reported one, and §15.2's
/// "exhaustiveness errors" never reached an editor at all.
///
/// It runs *after* inference rather than inside it because a scrutinee's type is
/// not final until the whole file has been inferred — `match e { … }` on an
/// unannotated parameter is pinned by a later call — and a coverage answer given
/// against a type that is still a variable is a `Y120` naming a catch-all the
/// program does not need.
///
/// The patterns come from [`crate::pattern::PatternBuilder`], the same builder
/// lowering uses, and **its diagnostics are kept** (ADR-133) — minus whatever
/// inference has already said at the same caret, which
/// [`crate::pattern::merge_pattern_diagnostics`] is what decides.
///
/// They used to be discarded, on the theory that inference had already walked
/// these patterns and reported the shape mistakes. Two of the four codes are
/// nobody else's: inference never decodes an integer literal and never counts a
/// payload, so a `Y013` or a `Y124` inside a match arm was reachable only from
/// lowering — and lowering is the pass `praxis check` and the editor do not run.
/// `match bla { A(i, j) => … }` therefore checked clean in the editor and
/// refused to run, which is exactly the divergence ADR-130 exists to close.
pub(crate) fn check_matches(
    file: FileId,
    root: &praxis_ast::SourceFile,
    db: &mut TypeDb,
    decls: &std::collections::HashMap<rowan::TextRange, crate::SymbolId>,
    expr_types: &std::collections::HashMap<crate::NodeKey, Type>,
    out: &mut Vec<Diagnostic>,
) {
    for node in root.syntax().descendants() {
        let Some(m) = praxis_ast::MatchExpr::cast(node.clone()) else {
            continue;
        };
        let Some(scrutinee) = m.scrutinee() else {
            continue;
        };
        // No recorded type means inference did not reach this match — a tree
        // recovery left it unreachable, and a coverage answer about it would be
        // about a program that does not exist.
        let Some(scrutinee_ty) = expr_types
            .get(&crate::NodeKey::of(scrutinee.syntax()))
            .copied()
        else {
            continue;
        };

        let mut built = Vec::new();
        let mut arms = Vec::new();
        let mut arm_spans = Vec::new();
        for arm in m.arms() {
            let pattern = match arm.pattern() {
                Some(pat) => crate::pattern::PatternBuilder {
                    file,
                    db,
                    decls,
                    diagnostics: &mut built,
                }
                .build(&pat, scrutinee_ty),
                None => TypedPattern::Wildcard,
            };
            arm_spans.push(file_span(file, arm.syntax().text_range()));
            // The **body** is not built: coverage is a question about patterns,
            // and lowering an arm's body here would be a second lowering of
            // every expression in the file. `TypedExpr::Unit` stands in for it.
            arms.push(TypedMatchArm {
                pattern,
                body: crate::lower::TypedExpr::Lit {
                    value: Lit::Unit,
                    ty: scrutinee_ty,
                    span: (0, 0),
                },
            });
        }

        crate::pattern::merge_pattern_diagnostics(built, out);

        check(
            db,
            file,
            &MatchToCheck {
                scrutinee_ty,
                arms: &arms,
                arm_spans: &arm_spans,
                match_span: file_span(file, m.syntax().text_range()),
                fix: arm_fix(&m),
            },
            out,
        );
    }
}

/// Where an added arm goes and how far in, read from the source's own layout.
///
/// The insertion point is the **end of the last arm**, not the closing brace:
/// inserting before the brace would put the new arm after whatever trailing
/// comment or blank line the author left, and appending after the last arm is
/// where a person writing one would put it. An arm-less `match e { }` inserts
/// just after the `{`.
fn arm_fix(m: &praxis_ast::MatchExpr) -> Option<ArmFix> {
    let node = m.syntax();
    let arms: Vec<_> = node
        .children()
        .filter(|c| c.kind() == SyntaxKind::MATCH_ARM)
        .collect();
    let insert_at = match arms.last() {
        Some(last) => u32::from(last.text_range().end()),
        None => node
            .children_with_tokens()
            .filter_map(NodeOrToken::into_token)
            .find(|t| t.kind() == SyntaxKind::L_BRACE)
            .map(|t| u32::from(t.text_range().end()))?,
    };
    Some(ArmFix {
        insert_at: Span::new(insert_at, insert_at),
        indent: arm_indent(node, arms.first()),
    })
}

/// The indentation the arms are written at.
///
/// Read from the whitespace token in front of the first arm, which is the one
/// piece of layout that says what this file does — four spaces, eight, or a tab.
/// A one-line `match e { A => 1 }` has no newline in that whitespace, and a
/// generated arm would be the first on its own line, so it falls back to the
/// `match`'s own column plus four.
fn arm_indent(
    match_node: &praxis_syntax::SyntaxNode,
    first_arm: Option<&praxis_syntax::SyntaxNode>,
) -> String {
    let leading = first_arm
        .and_then(|arm| arm.prev_sibling_or_token())
        .and_then(NodeOrToken::into_token)
        .filter(|t| t.kind() == SyntaxKind::Whitespace)
        .map(|t| t.text().to_string())
        .unwrap_or_default();
    if let Some((_, after_newline)) = leading.rsplit_once('\n') {
        return after_newline.to_string();
    }
    // No newline before the first arm: indent one level in from the `match`.
    let own = match_node
        .prev_sibling_or_token()
        .and_then(NodeOrToken::into_token)
        .filter(|t| t.kind() == SyntaxKind::Whitespace)
        .map(|t| t.text().to_string())
        .unwrap_or_default();
    let column = own.rsplit_once('\n').map_or("", |(_, tail)| tail);
    format!("{column}    ")
}

fn file_span(file: FileId, range: rowan::TextRange) -> FileSpan {
    FileSpan::new(
        file,
        Span::new(u32::from(range.start()), u32::from(range.end())),
    )
}

/// Where a missing arm would be written, and how far in.
///
/// Carried into [`check`] rather than derived from the `Y120` afterwards,
/// because the arms a fix writes are the **witnesses** — the same shapes the
/// message names — and recovering them from the rendered message would be a
/// second implementation of `describe` that parses the first one's output.
#[derive(Clone, Debug)]
pub(crate) struct ArmFix {
    /// A zero-width span: where the new arms are inserted, which is the end of
    /// the last arm (or just after the `{` when there are none).
    pub insert_at: Span,
    /// The indentation the existing arms are written at, repeated for the new
    /// ones. Read from the source's own whitespace, so a file indented with
    /// tabs gets tabs.
    pub indent: String,
}

/// The body a generated arm gets.
///
/// `panic` is `forall T. (T) -> Never` (§16.1), so it fits whatever the other
/// arms joined to and the file the fix produces still type-checks. A generated
/// arm that did not compile would trade one diagnostic for another.
const GENERATED_ARM_BODY: &str = "panic(\"todo\")";

/// One `match` as the coverage check sees it: a scrutinee type, the arms'
/// patterns, and where to report.
pub(crate) struct MatchToCheck<'a> {
    pub scrutinee_ty: Type,
    pub arms: &'a [TypedMatchArm],
    pub arm_spans: &'a [FileSpan],
    /// The whole `match` expression's span — where a `Y120` belongs, since the
    /// thing that is not exhaustive is the match and not any one arm.
    pub match_span: FileSpan,
    /// Where a missing arm would be written, when there is somewhere to write
    /// one. `None` from a caller that has no source layout to read.
    pub fix: Option<ArmFix>,
}

/// Check one `match` expression for exhaustiveness and unreachable arms,
/// appending `Y120`/`Y121` diagnostics to `out`.
pub(crate) fn check(
    db: &mut TypeDb,
    file: FileId,
    m: &MatchToCheck<'_>,
    out: &mut Vec<Diagnostic>,
) {
    let MatchToCheck {
        scrutinee_ty,
        arms,
        arm_spans,
        match_span,
        fix,
    } = m;
    let (scrutinee_ty, match_span) = (*scrutinee_ty, *match_span);
    let types = [scrutinee_ty];

    // --- Unreachable arms -------------------------------------------------
    // Arm `i` is reachable iff its pattern matches some value the arms above it
    // do not. An arm after a catch-all is the easy case; a repeated constructor
    // and a payload an earlier arm already covered are the ones the old scan
    // could not see.
    let mut matrix: Vec<Row<'_>> = Vec::with_capacity(arms.len());
    for (i, arm) in arms.iter().enumerate() {
        let q = [&arm.pattern];
        if useful(db, &matrix, &q, &types).is_empty() {
            let span = arm_spans.get(i).copied().unwrap_or_else(|| zero_span(file));
            out.push(unreachable_arm(span));
        }
        // An unreachable arm still *covers* what it names, so it goes into the
        // matrix either way: `{ _ => 1, A => 2 }` must not then report `B`
        // missing on account of the arm it already rejected.
        matrix.push(vec![&arm.pattern]);
    }

    // --- Exhaustiveness ---------------------------------------------------
    // A match covers everything iff a bare `_` would add nothing to it.
    let wild: Row<'_> = vec![&WILDCARD];
    let witnesses = useful(db, &matrix, &wild, &types);
    if !witnesses.is_empty() {
        let mut diag = non_exhaustive(match_span, &describe(db, &witnesses));
        if let Some(fix) = fix {
            // The fix names exactly what the message names: both are these
            // witnesses, and `MAX_WITNESSES` bounds both. A match missing more
            // shapes than that reports again after the fix is applied, which is
            // the honest behaviour — the alternative is a fix that claims to be
            // complete and is not.
            if let Some(arms_text) = arm_text(db, &witnesses, &fix.indent) {
                diag = add_arm_fix(diag, file, fix.insert_at, arms_text);
            }
        }
        out.push(diag);
    }
}

/// The source text of the arms a fix would insert, or `None` when the missing
/// shape is `_` — a wildcard arm is a decision about what the program *does*
/// with the rest, and writing one for the author would silently answer it.
fn arm_text(db: &TypeDb, witnesses: &[Vec<Witness>], indent: &str) -> Option<String> {
    let heads: Vec<&Witness> = witnesses.iter().filter_map(|row| row.first()).collect();
    if heads.iter().all(|w| matches!(w, Witness::Wild)) {
        return None;
    }
    let mut out = String::new();
    for head in heads {
        out.push('\n');
        out.push_str(indent);
        out.push_str(&render_witness(db, head));
        out.push_str(" => ");
        out.push_str(GENERATED_ARM_BODY);
    }
    Some(out)
}

/// Attach the arms as a machine-applicable suggestion (ADR-132): the editor's
/// quick fix is this replacement, and `praxis check` prints the same text under
/// `help:`.
fn add_arm_fix(diag: Diagnostic, file: FileId, insert_at: Span, arms: String) -> Diagnostic {
    diag.with_suggestion(
        FileSpan::new(file, insert_at),
        arms,
        "add the missing match arms",
    )
}

/// Render the uncovered shapes as the `Y120` message's tail.
///
/// A lone `_` witness means the type has no signature to enumerate (`Int`,
/// `Text`, an unresolved variable), so the fix is an arm rather than a name.
fn describe(db: &TypeDb, witnesses: &[Vec<Witness>]) -> String {
    let heads: Vec<&Witness> = witnesses.iter().filter_map(|row| row.first()).collect();
    if heads.iter().all(|w| matches!(w, Witness::Wild)) {
        return "a `_` catch-all arm".to_string();
    }
    let rendered: Vec<String> = heads
        .iter()
        .map(|w| format!("`{}`", render_witness(db, w)))
        .collect();
    rendered.join(", ")
}

// --- the matrix ------------------------------------------------------------

/// One row of the matrix: the patterns in each column, borrowed from the arms.
type Row<'p> = Vec<&'p TypedPattern>;

/// A value constructor — what the head of a pattern tests for, and what a
/// column's type can be built with.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Ctor {
    /// Variant `idx` of enum definition `def`. The def is part of the identity
    /// so a variant pattern of *some other* enum — which only an already
    /// reported `Y122`/`Y123` can produce — matches nothing rather than
    /// colliding with this column's variant of the same index.
    Variant { def: EnumDefId, idx: u32 },
    /// A literal value.
    Lit(LitKey),
    /// A tuple (REP-10). One constructor for the whole type, so a column of
    /// tuples is `Closed` on it: `match p { (x, y) => … }` needs no `_`, and
    /// what is left uncovered is a question about the *elements*.
    Tuple,
    /// A record of definition `def` (REP-10) — one constructor, for the same
    /// reason a tuple has one. The def is part of the identity for the reason a
    /// variant's is.
    Record { def: RecordDefId },
}

/// A literal's identity.
///
/// [`Lit`] holds an `f64`, which is not `Eq`; a `Float` pattern is keyed by its
/// bits, so two spellings of one value are one constructor and `NaN` is its own
/// — which is right, since a `NaN` pattern matches nothing and therefore covers
/// nothing.
#[derive(Clone, PartialEq, Eq, Debug)]
enum LitKey {
    Int(i64),
    Float(u64),
    Text(String),
    Bool(bool),
    Char(u32),
    Unit,
}

impl LitKey {
    fn of(lit: &Lit) -> Self {
        match lit {
            Lit::Int(v) => LitKey::Int(*v),
            Lit::Float(v) => LitKey::Float(v.to_bits()),
            Lit::Text(s) => LitKey::Text(s.clone()),
            Lit::Bool(b) => LitKey::Bool(*b),
            Lit::Char(c) => LitKey::Char(*c),
            Lit::Unit => LitKey::Unit,
        }
    }

    fn render(&self) -> String {
        match self {
            LitKey::Int(v) => v.to_string(),
            LitKey::Float(bits) => f64::from_bits(*bits).to_string(),
            LitKey::Text(s) => format!("{s:?}"),
            LitKey::Bool(b) => b.to_string(),
            LitKey::Char(c) => match char::from_u32(*c) {
                Some(ch) => format!("'{ch}'"),
                None => format!("'\\u{{{c:x}}}'"),
            },
            LitKey::Unit => "()".to_string(),
        }
    }
}

/// The constructors a type's values can be built with.
enum Signature {
    /// A set that can be enumerated: an enum's variants, `Bool`'s two literals,
    /// or the single constructor a tuple and a record each have. A match over one
    /// is exhaustive without a `_`.
    Closed(Vec<Ctor>),
    /// Too many to enumerate (`Int`, `Float`, `Text`, `Char`), or a type the
    /// checker cannot see into (an unresolved variable). Either way a `_` arm is
    /// required.
    Open,
}

fn signature(db: &TypeDb, ty: Type) -> Signature {
    let resolved = db.follow(ty);
    match db.data(resolved) {
        TypeData::Enum { def, .. } => {
            let def = *def;
            let n = db.enum_def(def).variants.len();
            Signature::Closed(
                (0..n as u32)
                    .map(|idx| Ctor::Variant { def, idx })
                    .collect(),
            )
        }
        TypeData::Scalar(ScalarType::Bool) => Signature::Closed(vec![
            Ctor::Lit(LitKey::Bool(false)),
            Ctor::Lit(LitKey::Bool(true)),
        ]),
        // One constructor each (REP-10), which is what makes
        // `match p { P { x, y } => … }` exhaustive where it used to need a `_`.
        // Both were `Open` only because no pattern could name them.
        TypeData::Tuple(_) => Signature::Closed(vec![Ctor::Tuple]),
        TypeData::Record { def, .. } => Signature::Closed(vec![Ctor::Record { def: *def }]),
        _ => Signature::Open,
    }
}

/// The constructor a pattern tests for, or `None` when it matches anything.
fn head_ctor(pat: &TypedPattern) -> Option<Ctor> {
    match pat {
        TypedPattern::Wildcard | TypedPattern::Bind { .. } => None,
        TypedPattern::Lit { value, .. } => Some(Ctor::Lit(LitKey::of(value))),
        TypedPattern::EnumVariant {
            enum_def_id,
            variant_idx,
            ..
        } => Some(Ctor::Variant {
            def: *enum_def_id,
            idx: *variant_idx,
        }),
        TypedPattern::Tuple { .. } => Some(Ctor::Tuple),
        TypedPattern::Record { record_def_id, .. } => Some(Ctor::Record {
            def: *record_def_id,
        }),
    }
}

/// The types of `ctor`'s fields, when a value of `col_ty` is built with it.
///
/// The payload comes from the column type's *arguments*, so `Some(n)` against
/// an `Option[Int]` recurses at `Int` and not at the def's own parameter (F12),
/// and a record's fields are read at the instance's arguments for the same
/// reason.
fn ctor_field_types(db: &mut TypeDb, col_ty: Type, ctor: &Ctor) -> Vec<Type> {
    let resolved = db.follow(col_ty);
    match ctor {
        Ctor::Lit(_) => Vec::new(),
        Ctor::Variant { def, idx } => {
            let args = match db.data(resolved) {
                TypeData::Enum { def: col_def, args } if col_def == def => args.clone(),
                // Not this constructor's enum: an ill-typed pattern, already
                // reported. No fields, so specialization drops every row rather
                // than pairing sub-patterns with types they do not have.
                _ => return Vec::new(),
            };
            db.variant_payload_of(*def, &args, *idx as usize)
        }
        Ctor::Tuple => match db.data(resolved) {
            TypeData::Tuple(els) => els.clone(),
            _ => Vec::new(),
        },
        Ctor::Record { def } => {
            let args = match db.data(resolved) {
                TypeData::Record { def: col_def, args } if col_def == def => args.clone(),
                _ => return Vec::new(),
            };
            db.record_fields_of(*def, &args)
                .into_iter()
                .map(|f| f.ty)
                .collect()
        }
    }
}

/// `S(c, P)` — the rows that can match a value built with `c`, each with its
/// head replaced by that head's `arity` fields.
fn specialize<'p>(matrix: &[Row<'p>], ctor: &Ctor, arity: usize) -> Vec<Row<'p>> {
    let mut out = Vec::new();
    for row in matrix {
        let Some((head, rest)) = row.split_first() else {
            continue;
        };
        match head_ctor(head) {
            // A wildcard matches every field of every constructor.
            None => {
                let mut new_row: Row<'p> = vec![&WILDCARD; arity];
                new_row.extend_from_slice(rest);
                out.push(new_row);
            }
            Some(c) if c == *ctor => {
                let mut new_row: Row<'p> = Vec::with_capacity(arity + rest.len());
                // Lowering pads to arity; `get` is what keeps a mis-shaped
                // pattern from making the row a different width than the column
                // list.
                let subpatterns = head.sub_patterns();
                for i in 0..arity {
                    new_row.push(subpatterns.get(i).unwrap_or(&WILDCARD));
                }
                new_row.extend_from_slice(rest);
                out.push(new_row);
            }
            // A different constructor: this row cannot match a `c` value.
            Some(_) => {}
        }
    }
    out
}

/// `D(P)` — the rows that match a value built with a constructor the first
/// column does not mention, with that column dropped.
fn default_matrix<'p>(matrix: &[Row<'p>]) -> Vec<Row<'p>> {
    matrix
        .iter()
        .filter_map(|row| {
            let (head, rest) = row.split_first()?;
            head_ctor(head).is_none().then(|| rest.to_vec())
        })
        .collect()
}

/// A value shape no row matches — what a `Y120` names.
#[derive(Clone, Debug)]
enum Witness {
    /// Any value: the type has no signature to name a missing case from.
    Wild,
    Variant {
        def: EnumDefId,
        idx: u32,
        fields: Vec<Witness>,
    },
    Lit(LitKey),
    /// `(_, _)` — a tuple whose elements are the uncovered shapes (REP-10).
    Tuple(Vec<Witness>),
    /// `P { x: _, y: _ }` — a record, rendered with its own field names, which
    /// is why the def travels with it (REP-10).
    Record {
        def: RecordDefId,
        fields: Vec<Witness>,
    },
}

fn render_witness(db: &TypeDb, w: &Witness) -> String {
    match w {
        Witness::Wild => "_".to_string(),
        Witness::Lit(k) => k.render(),
        Witness::Variant { def, idx, fields } => {
            let name = db
                .enum_def(*def)
                .variants
                .get(*idx as usize)
                .map_or("?", |v| v.name.as_str());
            if fields.is_empty() {
                name.to_string()
            } else {
                let rendered: Vec<String> = fields.iter().map(|f| render_witness(db, f)).collect();
                format!("{name}({})", rendered.join(", "))
            }
        }
        Witness::Tuple(fields) => {
            let rendered: Vec<String> = fields.iter().map(|f| render_witness(db, f)).collect();
            format!("({})", rendered.join(", "))
        }
        Witness::Record { def, fields } => {
            let rdef = db.record_def(*def);
            let name = rdef.name.as_deref().unwrap_or("record");
            let rendered: Vec<String> = fields
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    let fname = rdef.fields.get(i).map_or("?", |d| d.name.as_str());
                    format!("{fname}: {}", render_witness(db, f))
                })
                .collect();
            format!("{name} {{ {} }}", rendered.join(", "))
        }
    }
}

/// A missing constructor as a witness, with every field left `_`.
fn witness_for(db: &mut TypeDb, col_ty: Type, ctor: &Ctor) -> Witness {
    let arity = ctor_field_types(db, col_ty, ctor).len();
    match ctor {
        Ctor::Variant { def, idx } => Witness::Variant {
            def: *def,
            idx: *idx,
            fields: vec![Witness::Wild; arity],
        },
        Ctor::Lit(k) => Witness::Lit(k.clone()),
        Ctor::Tuple => Witness::Tuple(vec![Witness::Wild; arity]),
        Ctor::Record { def } => Witness::Record {
            def: *def,
            fields: vec![Witness::Wild; arity],
        },
    }
}

/// Fold a witness row back up through `ctor`: its first `arity` entries are the
/// constructor's fields, the rest are the columns that followed it.
fn rebuild(ctor: &Ctor, arity: usize, row: Vec<Witness>) -> Vec<Witness> {
    let mut row = row;
    let take = arity.min(row.len());
    let fields: Vec<Witness> = row.drain(..take).collect();
    let head = match ctor {
        Ctor::Variant { def, idx } => Witness::Variant {
            def: *def,
            idx: *idx,
            fields,
        },
        Ctor::Lit(k) => Witness::Lit(k.clone()),
        Ctor::Tuple => Witness::Tuple(fields),
        Ctor::Record { def } => Witness::Record { def: *def, fields },
    };
    let mut out = Vec::with_capacity(row.len() + 1);
    out.push(head);
    out.extend(row);
    out
}

/// The value shapes `q` matches that no row of `matrix` does. Empty means `q`
/// is **not useful**: every value it could match, some row already matches.
///
/// `types` names each column's type and stays the same length as `q`.
fn useful<'p>(
    db: &mut TypeDb,
    matrix: &[Row<'p>],
    q: &[&'p TypedPattern],
    types: &[Type],
) -> Vec<Vec<Witness>> {
    // No columns left: `q` is useful exactly when no row survived to here, and
    // the value it witnesses has no components left to name.
    let Some((head, q_rest)) = q.split_first() else {
        return if matrix.is_empty() {
            vec![Vec::new()]
        } else {
            Vec::new()
        };
    };
    let Some(&col_ty) = types.first() else {
        return Vec::new();
    };
    let types_rest = &types[1..];

    match head_ctor(head) {
        // `q` tests for one constructor: ask the same question one level down,
        // among the rows that can match it.
        Some(ctor) => {
            let field_types = ctor_field_types(db, col_ty, &ctor);
            let arity = field_types.len();
            let spec = specialize(matrix, &ctor, arity);
            let mut sub_q: Vec<&'p TypedPattern> = Vec::with_capacity(arity + q_rest.len());
            let subpatterns = head.sub_patterns();
            for i in 0..arity {
                sub_q.push(subpatterns.get(i).unwrap_or(&WILDCARD));
            }
            sub_q.extend_from_slice(q_rest);
            let mut sub_types = field_types;
            sub_types.extend_from_slice(types_rest);
            useful(db, &spec, &sub_q, &sub_types)
                .into_iter()
                .map(|row| rebuild(&ctor, arity, row))
                .collect()
        }
        // `q` matches anything here. Whether that is useful depends on whether
        // the rows between them already name every constructor.
        None => {
            let sig = signature(db, col_ty);
            let heads: Vec<Ctor> = matrix
                .iter()
                .filter_map(|row| row.first().and_then(|p| head_ctor(p)))
                .collect();
            let missing: Vec<Ctor> = match &sig {
                Signature::Closed(all) => {
                    all.iter().filter(|c| !heads.contains(c)).cloned().collect()
                }
                // An open signature is never complete: some value is always
                // unnamed, so a `_` is always useful unless a row has one too.
                Signature::Open => Vec::new(),
            };
            let complete =
                matches!(&sig, Signature::Closed(all) if !all.is_empty()) && missing.is_empty();

            if complete {
                let Signature::Closed(all) = sig else {
                    return Vec::new();
                };
                let mut out = Vec::new();
                for ctor in all {
                    let field_types = ctor_field_types(db, col_ty, &ctor);
                    let arity = field_types.len();
                    let spec = specialize(matrix, &ctor, arity);
                    let mut sub_q: Vec<&'p TypedPattern> = vec![&WILDCARD; arity];
                    sub_q.extend_from_slice(q_rest);
                    let mut sub_types = field_types;
                    sub_types.extend_from_slice(types_rest);
                    for row in useful(db, &spec, &sub_q, &sub_types) {
                        out.push(rebuild(&ctor, arity, row));
                        if out.len() >= MAX_WITNESSES {
                            return out;
                        }
                    }
                }
                out
            } else {
                let rows = useful(db, &default_matrix(matrix), q_rest, types_rest);
                if rows.is_empty() {
                    return Vec::new();
                }
                let mut out = Vec::new();
                if missing.is_empty() {
                    // Nothing to name: the witness is "a value no arm mentions".
                    for row in rows {
                        let mut w = vec![Witness::Wild];
                        w.extend(row);
                        out.push(w);
                        if out.len() >= MAX_WITNESSES {
                            break;
                        }
                    }
                } else {
                    for ctor in &missing {
                        let head = witness_for(db, col_ty, ctor);
                        for row in &rows {
                            let mut w = vec![head.clone()];
                            w.extend(row.iter().cloned());
                            out.push(w);
                            if out.len() >= MAX_WITNESSES {
                                return out;
                            }
                        }
                    }
                }
                out
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_source::FileId;

    /// Helper: check a match and return the count of Y120 (non-exhaustive) and
    /// Y121 (unreachable) diagnostics it produced.
    fn check_counts(db: &mut TypeDb, scrutinee_ty: Type, arms: &[TypedMatchArm]) -> (usize, usize) {
        let mut diags = Vec::new();
        let spans = vec![zero_span(FileId::SYNTHETIC); arms.len()];
        let match_span = zero_span(FileId::SYNTHETIC);
        check(
            db,
            FileId::SYNTHETIC,
            &MatchToCheck {
                scrutinee_ty,
                arms,
                arm_spans: &spans,
                match_span,
                fix: None,
            },
            &mut diags,
        );
        let y120 = diags.iter().filter(|d| d.code().number() == 120).count();
        let y121 = diags.iter().filter(|d| d.code().number() == 121).count();
        (y120, y121)
    }

    /// The `Y120` message, for the tests that care which shape it names.
    fn missing_message(db: &mut TypeDb, scrutinee_ty: Type, arms: &[TypedMatchArm]) -> String {
        let mut diags = Vec::new();
        let spans = vec![zero_span(FileId::SYNTHETIC); arms.len()];
        let match_span = zero_span(FileId::SYNTHETIC);
        check(
            db,
            FileId::SYNTHETIC,
            &MatchToCheck {
                scrutinee_ty,
                arms,
                arm_spans: &spans,
                match_span,
                fix: None,
            },
            &mut diags,
        );
        diags
            .iter()
            .find(|d| d.code().number() == 120)
            .map(|d| d.message().to_string())
            .unwrap_or_default()
    }

    #[test]
    fn exhaustive_enum_with_wildcard_is_ok() {
        let mut db = TypeDb::new();
        let et = tile_enum(&mut db);
        let def = enum_def_of(&db, et);
        let arms = vec![
            arm(TypedPattern::Wildcard, et),
            arm(variant(def, 0, vec![], et), et),
        ];
        let (y120, y121) = check_counts(&mut db, et, &arms);
        assert_eq!(y120, 0, "wildcard makes it exhaustive");
        assert_eq!(y121, 1, "arm after wildcard is unreachable");
    }

    #[test]
    fn non_exhaustive_enum_reports_y120() {
        let mut db = TypeDb::new();
        let et = tile_enum(&mut db);
        let def = enum_def_of(&db, et);
        // Only Empty is covered; Wall is missing.
        let arms = vec![arm(variant(def, 0, vec![], et), et)];
        let (y120, y121) = check_counts(&mut db, et, &arms);
        assert_eq!(y120, 1, "missing Wall variant");
        assert_eq!(y121, 0);
        assert!(
            missing_message(&mut db, et, &arms).contains("`Wall`"),
            "the message names the variant that is missing"
        );
    }

    #[test]
    fn int_match_without_wildcard_reports_y120() {
        let mut db = TypeDb::new();
        let int = db.int();
        let arms = vec![
            arm(
                TypedPattern::Lit {
                    value: Lit::Int(1),
                    ty: int,
                },
                int,
            ),
            arm(
                TypedPattern::Lit {
                    value: Lit::Int(2),
                    ty: int,
                },
                int,
            ),
        ];
        let (y120, _) = check_counts(&mut db, int, &arms);
        assert_eq!(y120, 1, "Int match needs a wildcard");
        assert!(
            missing_message(&mut db, int, &arms).contains("catch-all"),
            "an open signature asks for an arm, not for a value name"
        );
    }

    /// **HIR-06's first half at the unit level.** The old walk compared
    /// top-level variant indices only, so a covered constructor's *payload*
    /// was never asked about.
    #[test]
    fn a_covered_constructor_with_an_uncovered_payload_is_not_exhaustive() {
        let mut db = TypeDb::new();
        let flag = flag_enum(&mut db);
        let flag_def = enum_def_of(&db, flag);
        let wrapped = wrapped_enum(&mut db, flag);
        let wrapped_def = enum_def_of(&db, wrapped);

        // `match w { Wrap(On) => … }` — `Wrap` is the only variant, so the old
        // top-level check called this exhaustive.
        let arms = vec![arm(
            variant(
                wrapped_def,
                0,
                vec![variant(flag_def, 0, vec![], flag)],
                wrapped,
            ),
            wrapped,
        )];
        let (y120, y121) = check_counts(&mut db, wrapped, &arms);
        assert_eq!(y120, 1, "`Wrap(Off)` is uncovered");
        assert_eq!(y121, 0);
        assert!(
            missing_message(&mut db, wrapped, &arms).contains("`Wrap(Off)`"),
            "the witness names the nested constructor, not just the outer one"
        );

        // …and covering both payload constructors closes it.
        let both = vec![
            arm(
                variant(
                    wrapped_def,
                    0,
                    vec![variant(flag_def, 0, vec![], flag)],
                    wrapped,
                ),
                wrapped,
            ),
            arm(
                variant(
                    wrapped_def,
                    0,
                    vec![variant(flag_def, 1, vec![], flag)],
                    wrapped,
                ),
                wrapped,
            ),
        ];
        let (y120, y121) = check_counts(&mut db, wrapped, &both);
        assert_eq!(y120, 0, "both payload constructors are covered");
        assert_eq!(y121, 0, "neither arm subsumes the other");
    }

    /// **HIR-06's second half at the unit level.** Unreachability used to mean
    /// "after a catch-all" and nothing else.
    #[test]
    fn a_repeated_constructor_adds_no_coverage() {
        let mut db = TypeDb::new();
        let et = tile_enum(&mut db);
        let def = enum_def_of(&db, et);
        let arms = vec![
            arm(variant(def, 0, vec![], et), et),
            arm(variant(def, 0, vec![], et), et),
            arm(variant(def, 1, vec![], et), et),
        ];
        let (y120, y121) = check_counts(&mut db, et, &arms);
        assert_eq!(y120, 0, "both variants are covered");
        assert_eq!(y121, 1, "the second `Empty` is dead");
    }

    /// A `Bool` scrutinee has a signature of two, so it needs no `_`; and the
    /// arm that follows a complete pair is dead.
    #[test]
    fn bool_is_covered_by_its_two_literals() {
        let mut db = TypeDb::new();
        let b = db.bool();
        let lit = |v: bool| TypedPattern::Lit {
            value: Lit::Bool(v),
            ty: b,
        };
        let (y120, y121) = check_counts(&mut db, b, &[arm(lit(true), b), arm(lit(false), b)]);
        assert_eq!((y120, y121), (0, 0), "`true` and `false` are all of Bool");

        let (y120, _) = check_counts(&mut db, b, &[arm(lit(true), b)]);
        assert_eq!(y120, 1, "one of the two is not both");

        let (_, y121) = check_counts(
            &mut db,
            b,
            &[arm(lit(true), b), arm(lit(false), b), arm(lit(true), b)],
        );
        assert_eq!(y121, 1, "a third Bool arm can never run");
    }

    /// A payload sub-pattern that binds covers the whole payload, so an arm
    /// naming one of its constructors afterwards is dead. This is the case a
    /// top-level scan reports backwards: `Wrap(x)` and `Wrap(On)` are two
    /// distinct arms naming one variant.
    #[test]
    fn a_binding_payload_covers_every_constructor_under_it() {
        let mut db = TypeDb::new();
        let flag = flag_enum(&mut db);
        let flag_def = enum_def_of(&db, flag);
        let wrapped = wrapped_enum(&mut db, flag);
        let wrapped_def = enum_def_of(&db, wrapped);
        let arms = vec![
            arm(
                variant(wrapped_def, 0, vec![TypedPattern::Wildcard], wrapped),
                wrapped,
            ),
            arm(
                variant(
                    wrapped_def,
                    0,
                    vec![variant(flag_def, 0, vec![], flag)],
                    wrapped,
                ),
                wrapped,
            ),
        ];
        let (y120, y121) = check_counts(&mut db, wrapped, &arms);
        assert_eq!(y120, 0, "`Wrap(_)` is all of Wrapped");
        assert_eq!(y121, 1, "`Wrap(On)` is already covered");
    }

    /// An unreachable arm still contributes coverage: reporting it must not
    /// then make the match look non-exhaustive as well.
    #[test]
    fn an_unreachable_arm_still_counts_as_covered() {
        let mut db = TypeDb::new();
        let et = tile_enum(&mut db);
        let def = enum_def_of(&db, et);
        let arms = vec![
            arm(TypedPattern::Wildcard, et),
            arm(variant(def, 0, vec![], et), et),
        ];
        let (y120, y121) = check_counts(&mut db, et, &arms);
        assert_eq!(y120, 0);
        assert_eq!(y121, 1);
    }

    /// The two-variant `Tile` enum most enum tests here match on.
    fn tile_enum(db: &mut TypeDb) -> Type {
        let variants = praxis_types::VariantSet::from_pairs(vec![
            ("Empty".into(), Vec::new()),
            ("Wall".into(), Vec::new()),
        ])
        .expect("distinct variant names");
        db.enum_(Some("Tile".into()), variants)
    }

    /// `enum Flag { On, Off }` — the payload the nested tests recurse into.
    fn flag_enum(db: &mut TypeDb) -> Type {
        let variants = praxis_types::VariantSet::from_pairs(vec![
            ("On".into(), Vec::new()),
            ("Off".into(), Vec::new()),
        ])
        .expect("distinct variant names");
        db.enum_(Some("Flag".into()), variants)
    }

    /// `enum Wrapped { Wrap(Flag) }` — one variant, so a top-level check calls
    /// every match on it exhaustive.
    fn wrapped_enum(db: &mut TypeDb, flag: Type) -> Type {
        let variants = praxis_types::VariantSet::from_pairs(vec![("Wrap".into(), vec![flag])])
            .expect("distinct variant names");
        db.enum_(Some("Wrapped".into()), variants)
    }

    /// The `EnumDefId` behind an enum type, so a test's patterns name the def
    /// its scrutinee really has rather than a forged index.
    fn enum_def_of(db: &TypeDb, ty: Type) -> praxis_types::EnumDefId {
        match db.data(db.follow(ty)) {
            TypeData::Enum { def, .. } => *def,
            other => panic!("not an enum: {other:?}"),
        }
    }

    fn variant(
        def: praxis_types::EnumDefId,
        idx: u32,
        subpatterns: Vec<TypedPattern>,
        ty: Type,
    ) -> TypedPattern {
        TypedPattern::EnumVariant {
            enum_def_id: def,
            variant_idx: idx,
            subpatterns,
            ty,
        }
    }

    /// Build a trivial arm with the given pattern (body is an Int-0 lit).
    ///
    /// `body_ty` used to be a forged `Type(0)`, which named whatever sat in
    /// slot zero of whichever arena the test built. F5 seals the handle, so the
    /// caller passes a real one.
    fn arm(pattern: TypedPattern, body_ty: Type) -> TypedMatchArm {
        TypedMatchArm {
            pattern,
            body: crate::lower::TypedExpr::Lit {
                value: Lit::Int(0),
                ty: body_ty,
                span: (0, 0),
            },
        }
    }
}
