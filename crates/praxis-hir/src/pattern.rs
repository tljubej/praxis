//! Building a [`TypedPattern`] from a source pattern (§4.6, ADR-130).
//!
//! This is the one place a `praxis_ast::Pattern` becomes the shape the compiler
//! reasons about: which variant it names, which payload slot each sub-pattern
//! fills, which symbol a bare name binds. Two passes need that shape and they
//! must not each have an opinion about it:
//!
//! - **lowering**, which turns the arms into MIR's tag-compare chain, and
//! - **the coverage check** ([`crate::exhaustive`]), which runs at the end of
//!   analysis so `praxis check` and the editor see `Y120`/`Y121` at all.
//!
//! Keeping the builder out of the lowerer is what lets coverage be asked
//! without building MIR (ADR-130): §15.2's exhaustiveness errors have to reach
//! an editor, and the editor never lowers.
//!
//! # The diagnostics this emits
//!
//! A pattern whose *shape* cannot fit its scrutinee (`Y123`), a variant the enum
//! has not (`Y122`), a payload the pattern does not fit (`Y124`) and an
//! out-of-range literal (`Y013`).
//!
//! **They are analysis's answer** (ADR-133). Inference decides `Y122` and
//! `Y123` from its own walk over the same patterns, but it never decodes a
//! literal and never counts a payload, so `Y013` and `Y124` are reachable from
//! this builder alone — and lowering is the one pass `praxis check` and the
//! editor do not run. So the sink is kept, minus whatever inference has already
//! said at the same caret ([`crate::exhaustive::check_matches`]), and the
//! **binding** positions — a `for` header and a destructuring closure parameter
//! — are walked here by [`check_binding_patterns`] for the same reason.

use std::collections::HashMap;

use praxis_ast::AstNode;
use praxis_source::{DiagCode, Diagnostic, FileId, FileSpan, Severity};
use praxis_syntax::{SyntaxKind, span_bridge::range_to_span};
use praxis_typeck::Type;
use rowan::TextRange;

use crate::lower::{Lit, TypedPattern};
use crate::symbol::SymbolId;

/// The byte span `[start, end)` of the token a pattern binds its name at, in
/// the `(u32, u32)` shape the typed tree carries everywhere else
/// (`Lowerer::node_span`).
///
/// A binding's span is the *name's*, not the enclosing pattern's: it is what
/// the crash debugger echoes as the binding's provenance, and `Some(p)` should
/// point at `p` and not at `Some(p)` (ADR-139).
fn tok_span(tok: &praxis_syntax::SyntaxToken) -> (u32, u32) {
    let r = tok.text_range();
    (u32::from(r.start()), u32::from(r.end()))
}

/// Everything building a pattern needs, and nothing else.
///
/// The list is short on purpose: a field added here is a claim that pattern
/// shape depends on something more than the scrutinee's type, the declarations
/// resolution minted, and somewhere to report.
pub(crate) struct PatternBuilder<'a> {
    pub file: FileId,
    pub db: &'a mut praxis_typeck::TypeDb,
    /// Declaration-site ranges → `SymbolId` (from resolution; survives
    /// shadowing). A bare name in a pattern binds the symbol declared *at* its
    /// own range, which is the only lookup that tells two shadowed bindings
    /// apart.
    pub decls: &'a HashMap<TextRange, SymbolId>,
    pub diagnostics: &'a mut Vec<Diagnostic>,
}

impl PatternBuilder<'_> {
    /// Build a pattern into a recursive [`TypedPattern`].
    ///
    /// Nested sub-patterns are recursed into and literal patterns carry their
    /// value, so `match n { 1 => a, 2 => b }` tests each arm's literal.
    ///
    /// A bare `Name` is ambiguous (variable bind vs payload-less variant) and is
    /// disambiguated against the scrutinee's enum type.
    pub(crate) fn build(&mut self, pat: &praxis_ast::Pattern, scrutinee_ty: Type) -> TypedPattern {
        use praxis_ast::PatternKind;
        match pat.kind() {
            PatternKind::Wildcard => TypedPattern::Wildcard,
            PatternKind::Literal => {
                // Read the literal value from the pattern's token.
                let Some(tok) = pat.literal_token() else {
                    return TypedPattern::Wildcard;
                };
                let value = match tok.kind() {
                    SyntaxKind::IntLit => {
                        // Out of range in a *pattern* is the same mistake as in
                        // an expression: a saturated literal would match a value
                        // the program never named. Inference reports the
                        // expression's; this one is the builder's, because
                        // inference never decodes a pattern's literal.
                        match praxis_syntax::numeric::parse_int_literal(tok.text()) {
                            Some(v) => Lit::Int(v),
                            None => {
                                let at = self.file_span(tok.text_range());
                                self.diagnostics.push(
                                    crate::diagnostics::int_literal_out_of_range(at, tok.text()),
                                );
                                Lit::Int(0)
                            }
                        }
                    }
                    SyntaxKind::FloatLit => Lit::Float(
                        praxis_syntax::numeric::strip_digit_separators(tok.text())
                            .parse::<f64>()
                            .unwrap_or(0.0),
                    ),
                    SyntaxKind::TextLit => {
                        Lit::Text(praxis_syntax::literal::unquote_text(tok.text()))
                    }
                    // Nothing is reported here: the lexer owns the one-character
                    // rule (ADR-141) and has already said so. The substituted
                    // U+0000 matches nothing a well-formed program can name,
                    // which is the right value for a pattern whose literal was
                    // refused.
                    SyntaxKind::CharLit => Lit::Char(
                        praxis_syntax::literal::decode_char_literal(tok.text()).unwrap_or('\0')
                            as u32,
                    ),
                    SyntaxKind::KW_TRUE => Lit::Bool(true),
                    SyntaxKind::KW_FALSE => Lit::Bool(false),
                    _ => return TypedPattern::Wildcard,
                };
                let ty = match &value {
                    Lit::Int(_) => self.db.int(),
                    Lit::Float(_) => self.db.float(),
                    Lit::Bool(_) => self.db.bool(),
                    Lit::Text(_) => self.db.text(),
                    // A `Char` pattern is a `Char`: answering `scrutinee_ty` here
                    // would make `match n { 'a' => … }` over an `Int` type-check
                    // by agreeing with whatever it was asked about. Every literal
                    // answers its own type.
                    Lit::Char(_) => self.db.char(),
                    // `Unit` literals are synthesized internally; the parser
                    // produces no Unit pattern, so this arm is defensive.
                    Lit::Unit => self.db.unit(),
                };
                TypedPattern::Lit { value, ty }
            }
            PatternKind::Name(name) => {
                // Disambiguate payload-less variant from variable bind by checking
                // the scrutinee's enum type.
                let resolved = self.db.follow(scrutinee_ty);
                if let praxis_typeck::TypeData::Enum { def, .. } = self.db.data(resolved) {
                    let def = *def;
                    let edef = self.db.enum_def(def);
                    if let Some(idx) = edef.variant(&name) {
                        let arity = edef.variants[idx].payload.len();
                        // A bare name naming a variant that **carries** a payload
                        // is `Y124` (ADR-134): such an arm says nothing about the
                        // value the variant holds, and reads exactly like a
                        // payload-less variant to anyone who has not gone and
                        // looked at the declaration. `Step(_)` says it out loud
                        // and is one character longer.
                        //
                        // The pattern is still **padded** to the variant's arity
                        // after the report: the usefulness matrix pairs each
                        // column with a type, and a row narrower than the payload
                        // would pair them off by one — so a second, wrong
                        // diagnostic about coverage would land on top of this one.
                        if arity > 0 {
                            let rendered = self.db.render(resolved);
                            let at = pat
                                .name_token()
                                .map(|t| t.text_range())
                                .unwrap_or_else(|| pat.syntax().text_range());
                            let diag = self
                                .arity_diag(at, &name, &rendered, arity, 0)
                                .with_suggestion(
                                    self.file_span(at),
                                    format!("{name}({})", vec!["_"; arity].join(", ")),
                                    "name the payload, or `_` for each slot you do not need",
                                );
                            self.diagnostics.push(diag);
                        }
                        return TypedPattern::EnumVariant {
                            enum_def_id: def,
                            variant_idx: idx as u32,
                            subpatterns: vec![TypedPattern::Wildcard; arity],
                            ty: scrutinee_ty,
                        };
                    }
                }
                // Not a variant: a variable bind. Resolve the declared symbol.
                if let Some(tok) = pat.name_token()
                    && let Some(symbol) = self.decls.get(&tok.text_range()).copied()
                {
                    return TypedPattern::Bind {
                        symbol,
                        name: tok.text().to_string(),
                        ty: scrutinee_ty,
                        span: tok_span(&tok),
                    };
                }
                // Fallback: treat as wildcard if the symbol is unresolved.
                TypedPattern::Wildcard
            }
            PatternKind::Variant(vname) => {
                // A pattern that names nothing the scrutinee has is **not** a
                // wildcard: lowering it as one would let a typo cover every
                // remaining case, so the match would come out exhaustive and the
                // typo's arm would silently run for every value.
                let at = pat
                    .name_token()
                    .map(|t| t.text_range())
                    .unwrap_or_else(|| pat.syntax().text_range());
                let resolved = self.db.follow(scrutinee_ty);
                let (enum_def_id, enum_args) = match self.db.data(resolved) {
                    praxis_typeck::TypeData::Enum { def, args } => (*def, args.clone()),
                    // An unconstrained scrutinee is one inference could not
                    // pin, and it has already reported; anything else is a
                    // pattern whose shape the type cannot take.
                    praxis_typeck::TypeData::Var(_) => return TypedPattern::Wildcard,
                    _ => {
                        let rendered = self.db.render(resolved);
                        self.diag(
                            at,
                            DiagCode::NotAPatternForType,
                            format!("`{vname}(…)` is not a pattern for `{rendered}`"),
                        );
                        return TypedPattern::Wildcard;
                    }
                };
                let Some(idx) = self.db.enum_def(enum_def_id).variant(&vname) else {
                    let rendered = self.db.render(resolved);
                    self.diag(
                        at,
                        DiagCode::UnknownEnumVariant,
                        format!("`{rendered}` has no variant `{vname}`"),
                    );
                    return TypedPattern::Wildcard;
                };
                // Recurse into sub-patterns against payload types. The payload
                // comes from the scrutinee's *arguments*, so `Some(n)` against an
                // `Option[Int]` binds `n` at `Int` rather than at the def's own
                // parameter.
                let payload_types: Vec<Type> =
                    self.db.variant_payload_of(enum_def_id, &enum_args, idx);
                let sub_pats: Vec<_> = pat.sub_patterns().collect();
                let mut subpatterns = Vec::new();
                for (i, sub) in sub_pats.iter().enumerate() {
                    let sub_ty = payload_types.get(i).copied().unwrap_or(scrutinee_ty);
                    subpatterns.push(self.build(sub, sub_ty));
                }
                // Exactly one sub-pattern per payload slot. A pattern written
                // with parentheses that names fewer is padded with wildcards —
                // `Some(_)` and `Some(n)` are the same test — and one that names
                // *more* is reported and then truncated: the extras are lowered
                // above so anything wrong inside them still reports, truncating
                // is what keeps MIR from reading a payload index past the object,
                // and the report is what stops `Wrap(a, b)` on a one-slot variant
                // from *compiling and running*.
                //
                // The **bare** spelling is the other side of the same code, and
                // it is handled at `PatternKind::Name`.
                if sub_pats.len() > payload_types.len() {
                    let rendered = self.db.render(resolved);
                    let want = payload_types.len();
                    let got = sub_pats.len();
                    let diag = self.arity_diag(at, &vname, &rendered, want, got);
                    self.diagnostics.push(diag);
                }
                subpatterns.resize(payload_types.len(), TypedPattern::Wildcard);
                TypedPattern::EnumVariant {
                    enum_def_id,
                    variant_idx: idx as u32,
                    subpatterns,
                    ty: scrutinee_ty,
                }
            }
            // `(a, b)` — one sub-pattern per element (§4.4). Inference unified
            // the scrutinee with a tuple of the pattern's own arity, so a shape
            // that does not fit has already reported; this reads the element
            // types and recurses.
            PatternKind::Tuple => {
                let resolved = self.db.follow(scrutinee_ty);
                let element_types = match self.db.data(resolved) {
                    praxis_typeck::TypeData::Tuple(els) => els.clone(),
                    praxis_typeck::TypeData::Var(_) => return TypedPattern::Wildcard,
                    _ => {
                        let rendered = self.db.render(resolved);
                        self.diag(
                            pat.syntax().text_range(),
                            DiagCode::NotAPatternForType,
                            format!("`(…)` is not a pattern for `{rendered}`"),
                        );
                        return TypedPattern::Wildcard;
                    }
                };
                let subs: Vec<_> = pat.sub_patterns().collect();
                let mut subpatterns = Vec::new();
                for (i, sub) in subs.iter().enumerate() {
                    let sub_ty = element_types.get(i).copied().unwrap_or(scrutinee_ty);
                    subpatterns.push(self.build(sub, sub_ty));
                }
                // Exactly one sub-pattern per element, for the reason given at a
                // variant's payload: a row narrower or wider than the column list
                // pairs the matrix's types off by one, and MIR would read an
                // element the tuple does not have.
                subpatterns.resize(element_types.len(), TypedPattern::Wildcard);
                TypedPattern::Tuple {
                    subpatterns,
                    ty: scrutinee_ty,
                }
            }
            // `P { x, y: p }` — one sub-pattern per *declared* field, in
            // declaration order (§4.5). A field the pattern does not name stays
            // a wildcard. The head is optional (ADR-091): the record def comes
            // from the *scrutinee*, never from the head.
            PatternKind::Record(rname) => {
                let resolved = self.db.follow(scrutinee_ty);
                let (record_def_id, record_args) = match self.db.data(resolved) {
                    praxis_typeck::TypeData::Record { def, args } => (*def, args.clone()),
                    praxis_typeck::TypeData::Var(_) => return TypedPattern::Wildcard,
                    _ => {
                        let rendered = self.db.render(resolved);
                        let head = match &rname {
                            Some(n) => format!("{n} {{ … }}"),
                            None => "{ … }".to_string(),
                        };
                        self.diag(
                            pat.syntax().text_range(),
                            DiagCode::NotAPatternForType,
                            format!("`{head}` is not a pattern for `{rendered}`"),
                        );
                        return TypedPattern::Wildcard;
                    }
                };
                let fields = self.db.record_fields_of(record_def_id, &record_args);
                let mut subpatterns = vec![TypedPattern::Wildcard; fields.len()];
                for field in pat.fields() {
                    let Some(name_tok) = field.name() else {
                        continue;
                    };
                    let fname = name_tok.text().to_string();
                    // A field the record does not have is inference's `Y114`.
                    let Some((idx, field_ty)) = fields
                        .iter()
                        .enumerate()
                        .find_map(|(i, f)| (f.name == fname).then_some((i, f.ty)))
                    else {
                        continue;
                    };
                    subpatterns[idx] = match field.pattern() {
                        Some(sub) => self.build(&sub, field_ty),
                        // A punned field `P { x }` binds the field to its own
                        // name — the same binding a `Name` pattern makes, at the
                        // field's type rather than the whole record's.
                        None => match self.decls.get(&name_tok.text_range()).copied() {
                            Some(symbol) => TypedPattern::Bind {
                                symbol,
                                name: fname.clone(),
                                ty: field_ty,
                                span: tok_span(&name_tok),
                            },
                            None => TypedPattern::Wildcard,
                        },
                    };
                }
                TypedPattern::Record {
                    record_def_id,
                    subpatterns,
                    ty: scrutinee_ty,
                }
            }
        }
    }

    fn diag(&mut self, at: TextRange, code: DiagCode, msg: impl Into<String>) {
        let at = self.file_span(at);
        self.diagnostics
            .push(Diagnostic::new(Severity::Error, code, msg.into(), at));
    }

    /// `Y124`'s wording, for both shapes that reach it: `got` is how many
    /// sub-patterns the pattern named, and a bare variant name named zero.
    ///
    /// One sentence, so the two are not two different complaints about the same
    /// mismatch — the reader needs the same two numbers either way.
    fn arity_diag(
        &self,
        at: TextRange,
        variant: &str,
        rendered: &str,
        want: usize,
        got: usize,
    ) -> Diagnostic {
        Diagnostic::new(
            Severity::Error,
            DiagCode::PayloadArityMismatch,
            format!(
                "`{variant}` in `{rendered}` holds {want} value(s), but this pattern names {got}"
            ),
            self.file_span(at),
        )
    }

    fn file_span(&self, at: TextRange) -> FileSpan {
        FileSpan::new(self.file, range_to_span(at))
    }
}

/// Add `built` to `out`, minus every diagnostic `out` already carries under the
/// same code at the same caret.
///
/// The builder and inference walk the same patterns and overlap on two codes: a
/// variant the enum has not (`Y122`) and a shape the scrutinee cannot take
/// (`Y123`) are decided by both. Keeping the builder's sink is what puts `Y013`
/// and `Y124` in front of `praxis check` at all; keeping *all* of it would put
/// the two shared messages under the same caret twice.
///
/// Same code and same span is the test, and it is the right one: a diagnostic is
/// identified by what it says and where, and two passes that agree on both have
/// made one report, not two. It is deliberately not a message comparison — the
/// two wordings of `Y122` are byte-identical today and nothing keeps them so.
pub(crate) fn merge_pattern_diagnostics(built: Vec<Diagnostic>, out: &mut Vec<Diagnostic>) {
    let mut seen: std::collections::HashSet<(DiagCode, FileSpan)> =
        out.iter().map(|d| (d.kind(), d.primary())).collect();
    for d in built {
        if seen.insert((d.kind(), d.primary())) {
            out.push(d);
        }
    }
}

/// Check every **binding** pattern in the file — a `for` header and a
/// destructuring closure parameter — at the end of analysis (ADR-133).
///
/// A binding has no second arm, so a pattern that can *fail* is `Y125`, and the
/// pattern's own shape mistakes (`Y013`, `Y124`) are the builder's. Asking both
/// questions here rather than in lowering is what puts them in front of
/// `praxis check` and the editor, which never lower.
///
/// Match arms are **not** here: [`crate::exhaustive::check_matches`] already
/// builds those patterns for coverage, and building them twice would report
/// twice. The three positions together are every place the grammar puts a
/// pattern, which `every_pattern_position_is_checked_by_analysis` is what keeps
/// true.
pub(crate) fn check_binding_patterns(
    file: FileId,
    root: &praxis_ast::SourceFile,
    db: &mut praxis_typeck::TypeDb,
    names: &crate::NameTable,
    decls: &HashMap<TextRange, SymbolId>,
    ref_types: &HashMap<TextRange, Type>,
    out: &mut Vec<Diagnostic>,
) {
    for node in root.syntax().descendants() {
        if let Some(f) = praxis_ast::ForExpr::cast(node.clone()) {
            // The item type is recorded on the binding *pattern*'s range: a
            // `for` binding is not an expression, so this is a `ref_types` read
            // and not an `expr_types` one. A miss means inference did not reach
            // this header, and a report about it would be about a program that
            // does not exist.
            let Some(pat) = f.binding() else { continue };
            let Some(item_ty) = ref_types.get(&pat.syntax().text_range()).copied() else {
                continue;
            };
            check_binding(
                file,
                db,
                decls,
                &pat,
                item_ty,
                "a `for` binding must match every item",
                out,
            );
            continue;
        }
        let Some(c) = praxis_ast::ClosureExpr::cast(node.clone()) else {
            continue;
        };
        for p in c.params() {
            // A named parameter binds its whole argument, and `|_|` binds
            // nothing (ADR-049 D7): neither takes anything apart.
            if p.name().is_some() {
                continue;
            }
            let Some(pat) = p.pattern() else { continue };
            if matches!(pat.kind(), praxis_ast::PatternKind::Wildcard) {
                continue;
            }
            let Some(symbol) = decls.get(&pat.syntax().text_range()).copied() else {
                continue;
            };
            // A symbol with no scheme errored during inference and is already
            // reported; a shape answer about it would be a second complaint.
            let Some(param_ty) = names
                .get(symbol)
                .and_then(|s| s.scheme.as_ref())
                .map(|s| s.body())
            else {
                continue;
            };
            check_binding(
                file,
                db,
                decls,
                &pat,
                param_ty,
                "a closure parameter must match every argument",
                out,
            );
        }
    }
}

/// One binding position: build the pattern, keep what the builder found, and
/// report `Y125` when the pattern can fail.
///
/// `must` is the half of the sentence that names the position — the rest of it
/// is the same at both, because it is the same rule.
fn check_binding(
    file: FileId,
    db: &mut praxis_typeck::TypeDb,
    decls: &HashMap<TextRange, SymbolId>,
    pat: &praxis_ast::Pattern,
    ty: Type,
    must: &str,
    out: &mut Vec<Diagnostic>,
) {
    let mut built = Vec::new();
    let pattern = PatternBuilder {
        file,
        db,
        decls,
        diagnostics: &mut built,
    }
    .build(pat, ty);
    if let Some(reason) = crate::lower::refutable_reason(&pattern) {
        let at = pat.syntax().text_range();
        built.push(Diagnostic::new(
            Severity::Error,
            DiagCode::RefutableBinding,
            format!("{must}, and {reason} does not"),
            FileSpan::new(file, range_to_span(at)),
        ));
    }
    merge_pattern_diagnostics(built, out);
}
