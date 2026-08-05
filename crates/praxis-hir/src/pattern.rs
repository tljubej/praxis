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
//! Before ADR-130 the builder was a private method on the lowerer, so coverage
//! could only be asked where MIR was being built — which is why a non-exhaustive
//! match was clean under `praxis check` and an error under `praxis run`, and why
//! §15.2's "exhaustiveness errors" never reached an editor.
//!
//! # The diagnostics this emits
//!
//! A pattern whose *shape* cannot fit its scrutinee (`Y123`), a variant the enum
//! has not (`Y122`), a payload named past its arity (`Y124`) and an out-of-range
//! literal (`Y013`). The coverage pass runs this builder with a sink it throws
//! away, because inference has already reported those same mistakes from its own
//! walk over the same patterns — see [`crate::exhaustive::check_matches`].

use std::collections::HashMap;

use praxis_ast::AstNode;
use praxis_source::{DiagCode, Diagnostic, FileId, FileSpan, Severity, Span};
use praxis_syntax::SyntaxKind;
use praxis_types::Type;
use rowan::TextRange;

use crate::lower::{Lit, TypedPattern};
use crate::symbol::SymbolId;

/// Everything building a pattern needs, and nothing else.
///
/// The list is short on purpose: it is what made the extraction from the lowerer
/// possible at all. A field added here is a claim that pattern shape depends on
/// something more than the scrutinee's type, the declarations resolution minted,
/// and somewhere to report.
pub(crate) struct PatternBuilder<'a> {
    pub file: FileId,
    pub db: &'a mut praxis_types::TypeDb,
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
    /// value — the two M7-Part-1 gaps that made `match n { 1 => a, 2 => b }`
    /// always take the first arm.
    ///
    /// A bare `Name` is ambiguous (variable bind vs payload-less variant) and is
    /// disambiguated against the scrutinee's enum type, as in WS5.
    pub(crate) fn build(&mut self, pat: &praxis_ast::Pattern, scrutinee_ty: Type) -> TypedPattern {
        use praxis_ast::PatternKind;
        match pat.kind() {
            PatternKind::Wildcard => TypedPattern::Wildcard,
            PatternKind::Literal => {
                // Read the literal value from the pattern's token (the WS5 bug
                // was that literals were dropped to a catch-all wildcard).
                let Some(tok) = pat.literal_token() else {
                    return TypedPattern::Wildcard;
                };
                let value = match tok.kind() {
                    SyntaxKind::IntLit => {
                        let cleaned = praxis_syntax::numeric::strip_digit_separators(tok.text());
                        // Out of range in a *pattern* is the same mistake as in
                        // an expression (TY-28): a saturated literal would match
                        // a value the program never named.
                        match cleaned.parse::<i64>() {
                            Ok(v) => Lit::Int(v),
                            Err(_) => {
                                let text = tok.text().to_string();
                                self.diag(
                                    tok.text_range(),
                                    DiagCode::IntLiteralOutOfRange,
                                    format!("`{text}` is outside the range of `Int`"),
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
                    SyntaxKind::KW_TRUE => Lit::Bool(true),
                    SyntaxKind::KW_FALSE => Lit::Bool(false),
                    _ => return TypedPattern::Wildcard,
                };
                let ty = match &value {
                    Lit::Int(_) => self.db.int(),
                    Lit::Float(_) => self.db.float(),
                    Lit::Bool(_) => self.db.bool(),
                    Lit::Text(_) => self.db.text(),
                    // Char literals don't appear in patterns (no char-literal
                    // pattern syntax); use the scrutinee type as a fallback.
                    Lit::Char(_) => scrutinee_ty,
                    // `Unit` literals are synthesized internally; the parser
                    // produces no Unit pattern, so this arm is defensive.
                    Lit::Unit => self.db.unit(),
                };
                TypedPattern::Lit { value, ty }
            }
            PatternKind::Name(name) => {
                // Disambiguate payload-less variant from variable bind by checking
                // the scrutinee's enum type (the WS5 fix).
                let resolved = self.db.follow(scrutinee_ty);
                if let praxis_types::TypeData::Enum { def, .. } = self.db.data(resolved) {
                    let edef = self.db.enum_def(*def);
                    if let Some(idx) = edef.variant(&name) {
                        // A bare name naming a variant *with* a payload means
                        // "any payload", so it is padded to the variant's arity
                        // (HIR-06). The usefulness matrix pairs each column
                        // with a type, and a row narrower than the payload
                        // would pair them off by one.
                        let arity = edef.variants[idx].payload.len();
                        return TypedPattern::EnumVariant {
                            enum_def_id: *def,
                            variant_idx: idx as u32,
                            subpatterns: vec![TypedPattern::Wildcard; arity],
                            ty: scrutinee_ty,
                        };
                    }
                }
                // Not a variant: a variable bind. Resolve the declared symbol.
                if let Some(tok) = pat.name_token() {
                    if let Some(symbol) = self.decls.get(&tok.text_range()).copied() {
                        return TypedPattern::Bind {
                            symbol,
                            ty: scrutinee_ty,
                        };
                    }
                }
                // Fallback: treat as wildcard if the symbol is unresolved.
                TypedPattern::Wildcard
            }
            PatternKind::Variant(vname) => {
                // A pattern that names nothing the scrutinee has is **not** a
                // wildcard (HIR-07). Lowering it as one made a typo cover every
                // remaining case, so the match came out exhaustive and the arm
                // it should have been silently ran for every value.
                let at = pat
                    .name_token()
                    .map(|t| t.text_range())
                    .unwrap_or_else(|| pat.syntax().text_range());
                let resolved = self.db.follow(scrutinee_ty);
                let (enum_def_id, enum_args) = match self.db.data(resolved) {
                    praxis_types::TypeData::Enum { def, args } => (*def, args.clone()),
                    // An unconstrained scrutinee is one inference could not
                    // pin, and it has already reported; anything else is a
                    // pattern whose shape the type cannot take.
                    praxis_types::TypeData::Var(_) => return TypedPattern::Wildcard,
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
                // Recurse into sub-patterns against payload types — the WS5 bug
                // was that only flat Name sub-patterns were collected and nested
                // variant patterns were silently dropped. The payload comes from
                // the scrutinee's *arguments*, so `Some(n)` against an
                // `Option[Int]` binds `n` at `Int` rather than at the def's own
                // parameter (F12).
                let payload_types: Vec<Type> =
                    self.db.variant_payload_of(enum_def_id, &enum_args, idx);
                let sub_pats: Vec<_> = pat.sub_patterns().collect();
                let mut subpatterns = Vec::new();
                for (i, sub) in sub_pats.iter().enumerate() {
                    let sub_ty = payload_types.get(i).copied().unwrap_or(scrutinee_ty);
                    subpatterns.push(self.build(sub, sub_ty));
                }
                // Exactly one sub-pattern per payload slot (HIR-06). A pattern
                // that names fewer is padded with wildcards — `Some` and
                // `Some(_)` are the same test — and one that names *more* is
                // reported and then truncated (REP-05): the extras are lowered
                // above so anything wrong inside them still reports, truncating
                // is what keeps MIR from reading a payload index past the object,
                // and the report is what stops `Wrap(a, b)` on a one-slot variant
                // from *compiling and running*.
                if sub_pats.len() > payload_types.len() {
                    let rendered = self.db.render(resolved);
                    let want = payload_types.len();
                    let got = sub_pats.len();
                    self.diag(
                        at,
                        DiagCode::TooManySubPatterns,
                        format!(
                            "`{vname}` in `{rendered}` holds {want} value(s), \
                             but this pattern names {got}"
                        ),
                    );
                }
                subpatterns.resize(payload_types.len(), TypedPattern::Wildcard);
                TypedPattern::EnumVariant {
                    enum_def_id,
                    variant_idx: idx as u32,
                    subpatterns,
                    ty: scrutinee_ty,
                }
            }
            // `(a, b)` — one sub-pattern per element (REP-10, §4.4). Inference
            // unified the scrutinee with a tuple of the pattern's own arity, so
            // a shape that does not fit has already reported; this reads the
            // element types and recurses.
            PatternKind::Tuple => {
                let resolved = self.db.follow(scrutinee_ty);
                let element_types = match self.db.data(resolved) {
                    praxis_types::TypeData::Tuple(els) => els.clone(),
                    praxis_types::TypeData::Var(_) => return TypedPattern::Wildcard,
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
                // Exactly one sub-pattern per element, for the reason REP-05
                // gives at a variant's payload: a row narrower or wider than the
                // column list pairs the matrix's types off by one, and MIR would
                // read an element the tuple does not have.
                subpatterns.resize(element_types.len(), TypedPattern::Wildcard);
                TypedPattern::Tuple {
                    subpatterns,
                    ty: scrutinee_ty,
                }
            }
            // `P { x, y: p }` — one sub-pattern per *declared* field, in
            // declaration order (REP-10, §4.5). A field the pattern does not
            // name stays a wildcard. The head is optional (ADR-091), and this
            // arm never needed it: the record has always come from the
            // *scrutinee* here, which is exactly what inference now does too.
            PatternKind::Record(rname) => {
                let resolved = self.db.follow(scrutinee_ty);
                let (record_def_id, record_args) = match self.db.data(resolved) {
                    praxis_types::TypeData::Record { def, args } => (*def, args.clone()),
                    praxis_types::TypeData::Var(_) => return TypedPattern::Wildcard,
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
                                ty: field_ty,
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
        self.diagnostics.push(Diagnostic::new(
            Severity::Error,
            code,
            msg.into(),
            FileSpan::new(
                self.file,
                Span::new(
                    praxis_source::BytePos::from(u32::from(at.start())),
                    praxis_source::BytePos::from(u32::from(at.end())),
                ),
            ),
        ));
    }
}
