//! Compile-time result-type synthesis (§7.8).
//!
//! Given a validated [`ParserAst`], derive its statically known result [`Type`].
//! This is the §7.8 derivation table: `lines(P)` → `Vec[result(P)]`, `grid(P)`
//! → `Grid[result(P)]`, named-capture templates → anonymous records, etc.
//!
//! The result type drives inference (`read` / `parse` expressions get this type
//! directly — there is no callee scheme to unify against) and is what hover
//! displays (acceptance criterion 4).

use crate::ast::{AtomicKind, ParserAst, TemplatePart};
use praxis_source::Span;
use praxis_types::{
    CollectionCtor, EnumVariantDef, FieldSet, TupleElems, Type, TypeCtorError, TypeDb, VariantSet,
};

/// Synthesize the result type of a parser expression (§7.8).
///
/// Normally called on an AST that has already passed
/// [`validate`](crate::validate::validate), but the error channel is real
/// rather than an `expect` (F5): the shapes this builds — an anonymous record
/// per named-capture template, an anonymous enum per `choice` — are exactly the
/// ones whose *names come from user source*, so a duplicate is user input, not
/// an internal inconsistency. `validate` catches those cases today; threading
/// the `Result` is what keeps the two from drifting apart silently.
pub fn synthesize(ast: &ParserAst, db: &mut TypeDb) -> Result<Type, TypeCtorError> {
    let mut discard = Vec::new();
    synth(ast, db, &mut discard)
}

/// [`synthesize`], keeping the type it derived for **every** node on the way
/// (ADR-098), not only the root's.
///
/// §15.3 asks for the synthesized type of *a* parser expression, and an inner
/// constructor is one. The recursion already computes every inner type and used
/// to drop each on the way back up, so hover on `lines(…)` inside
/// `sections(lines(…))` had nothing to read.
///
/// The entries come out in **post-order** — a node is recorded after the
/// children it was derived from — so where two nodes share a span the earlier
/// entry is the deeper one. That is the tie-break the cursor lookup uses; it is
/// not cosmetic, because `` `{int}` `` gives a single-capture template and its
/// capture's parser genuinely equal extents.
///
/// # Errors
/// The same [`TypeCtorError`]s [`synthesize`] answers.
pub fn synthesize_indexed(
    ast: &ParserAst,
    db: &mut TypeDb,
) -> Result<(Type, Vec<(Span, Type)>), TypeCtorError> {
    let mut out = Vec::new();
    let ty = synth(ast, db, &mut out)?;
    Ok((ty, out))
}

/// The §7.8 derivation table — **one implementation**, walked once, recording as
/// it goes. `synthesize` and `synthesize_indexed` differ only in whether they
/// keep the recording.
fn synth(
    ast: &ParserAst,
    db: &mut TypeDb,
    out: &mut Vec<(Span, Type)>,
) -> Result<Type, TypeCtorError> {
    let ty = synth_inner(ast, db, out)?;
    // Recorded **after** the recursion, so children precede their parent. A
    // `Type` has no forgeable value to reserve a slot with (it is a sealed arena
    // index, F5), which is the other reason this is post-order and not a
    // patched-in placeholder.
    out.push((ast.span(), ty));
    Ok(ty)
}

fn synth_inner(
    ast: &ParserAst,
    db: &mut TypeDb,
    out: &mut Vec<(Span, Type)>,
) -> Result<Type, TypeCtorError> {
    Ok(match ast {
        ParserAst::Atomic { kind, .. } => atomic_type(*kind, db),
        ParserAst::Template { parts, .. } => template_type(parts, db, out)?,
        ParserAst::Lines { child, .. }
        | ParserAst::Sections { child, .. }
        | ParserAst::Csv { child, .. }
        | ParserAst::Ws { child, .. } => {
            let elem = synth(child, db, out)?;
            db.vec(elem)
        }
        ParserAst::Sep { child, .. } => {
            let elem = synth(child, db, out)?;
            db.vec(elem)
        }
        ParserAst::Grid { child, .. } => {
            let elem = synth(child, db, out)?;
            db.unary_collection(CollectionCtor::Grid, elem)
        }
        ParserAst::SectionsNamed {
            fields,
            repeated_tail,
            ..
        } => {
            // Anonymous record: one field per named argument, in source order,
            // plus a final `Vec[result(P)]` field for the unbounded `repeated`
            // tail (if any). A counted group is a `Vec[result(P)]` too — the
            // same field the tail contributes, in the position it was written,
            // which is what lets a fixed field follow one.
            let mut rec_fields: Vec<(String, Type)> = Vec::with_capacity(fields.len());
            for item in fields {
                let elem = synth(item.parser(), db, out)?;
                let ty = match item {
                    crate::ast::SectionItem::One { .. } => elem,
                    crate::ast::SectionItem::Counted { .. } => db.vec(elem),
                };
                rec_fields.push((item.name().to_string(), ty));
            }
            if let Some((name, tail)) = repeated_tail {
                let elem = synth(tail, db, out)?;
                rec_fields.push((name.clone(), db.vec(elem)));
            }
            db.record(None, FieldSet::from_pairs(rec_fields)?)
        }
        ParserAst::Block { items, .. } => {
            // Flattened anonymous record (§7.5): positional named-capture
            // templates contribute their capture fields; named items contribute
            // one field each.
            let mut rec_fields: Vec<(String, Type)> = Vec::new();
            for item in items {
                match item {
                    crate::ast::BlockItem::Positional(p) => {
                        if let ParserAst::Template { parts, .. } = p {
                            for part in parts {
                                if let TemplatePart::Capture {
                                    name: Some(n),
                                    parser,
                                    ..
                                } = part
                                {
                                    rec_fields
                                        .push((n.as_str().to_string(), synth(parser, db, out)?));
                                }
                            }
                        }
                    }
                    crate::ast::BlockItem::Named { name, parser } => {
                        rec_fields.push((name.clone(), synth(parser, db, out)?));
                    }
                }
            }
            db.record(None, FieldSet::from_pairs(rec_fields)?)
        }
        ParserAst::Choice { cases, .. } => {
            // Anonymous enum (§7.5): one variant per case, each carrying the
            // case's result type as a single-element payload (so the parsed
            // value is recoverable via match). Identity is name+signature-based
            // via the M9 unify arm and the absent name.
            let mut variants: Vec<EnumVariantDef> = Vec::with_capacity(cases.len());
            for (name, p) in cases {
                let payload_ty = synth(p, db, out)?;
                variants.push(EnumVariantDef::new(name.clone(), vec![payload_ty]));
            }
            db.enum_(None, VariantSet::new(variants)?)
        }
        ParserAst::Optional { child, .. } => {
            // `Option[result(P)]` (§7.5/§7.8): the prelude's one `Option` def
            // (F12), applied to the child's result type. This used to spell the
            // variant list out and register a *fresh nominal def* per site,
            // which is one of TY-06's three copies of `Option`.
            let elem = synth(child, db, out)?;
            db.option_of(elem)
        }
        ParserAst::Scan { child, .. } => {
            // `scan(P)` → `Vec[result(P)]` (§7.5): matches in source order.
            let elem = synth(child, db, out)?;
            db.vec(elem)
        }
        ParserAst::OneOf { .. } => {
            // `one_of("LR")` → Char (§7.5).
            db.char()
        }
        ParserAst::Characters { child, .. } => {
            // `chars(P, skip:)` → `Vec[result(P)]` (§7.5, D-S20-A).
            //
            // This used to be `Vec[Char]` regardless of `P`, while the
            // interpreter stored whatever `P` produced and tagged the payload
            // `CHAR`. `chars(int, skip: none)` therefore advertised `Vec[Char]`
            // statically, tagged the elements `Char`, and stored `Int` objects
            // — a descriptor that disagrees with the values behind it, which is
            // the class of defect P0-11 closed for collections generally.
            // `chars(one_of("LR"))` is still `Vec[Char]`, because `one_of`
            // synthesizes `Char`; it is derived now rather than assumed.
            let elem = synth(child, db, out)?;
            db.vec(elem)
        }
        ParserAst::Matrix { child, .. } | ParserAst::GridRagged { child, .. } => {
            // `matrix(P)` / ragged `grid(P)` → Grid[result(P)] (§7.5, ADR-030).
            let elem = synth(child, db, out)?;
            db.unary_collection(CollectionCtor::Grid, elem)
        }
    })
}

/// The result type of an atomic parser (§7.4).
fn atomic_type(kind: AtomicKind, db: &mut TypeDb) -> Type {
    match kind {
        // `uint` is `Int`, deliberately: `ScalarType::UInt` is reserved and has
        // no runtime object (`praxis_repr::builtin_for_type` answers
        // `NoRuntimeRepr`), so typing a `uint` capture as `UInt` would make
        // every program containing one fail to compile under D9. The
        // non-negativity is the *parse rule*, in `walk_atomic`.
        AtomicKind::Int | AtomicKind::UInt | AtomicKind::Digit => db.int(),
        AtomicKind::Float => db.float(),
        AtomicKind::Byte => db.scalar(praxis_types::ScalarType::Byte),
        AtomicKind::Char => db.char(),
        AtomicKind::Word | AtomicKind::Identifier | AtomicKind::Text | AtomicKind::Rest => {
            db.text()
        }
    }
}

/// The result type of a template (§7.3). The shapes and the reason they are
/// what they are live on [`TemplateShape`](crate::plan::TemplateShape); this is
/// the same classification over *AST* parts, one step before lowering, so it
/// answers a `Type` where that one answers a runtime descriptor.
///
/// The two are deliberately not one generic function: threading a trait across
/// `TemplatePart`/`TemplatePartNode` would buy nothing, since this side has
/// always been right. It was the descriptor side that drifted (REP-54,
/// ADR-092) — `praxis check` typed ``lines(`{int},{int}`)`` as
/// `Vec[(Int, Int)]` throughout, while the value it produced was tagged `Unit`.
/// If a shape is ever added, it is added in both places.
fn template_type(
    parts: &[TemplatePart],
    db: &mut TypeDb,
    out: &mut Vec<(Span, Type)>,
) -> Result<Type, TypeCtorError> {
    let captures: Vec<&TemplatePart> = parts
        .iter()
        .filter(|p| matches!(p, TemplatePart::Capture { .. }))
        .collect();

    if captures.is_empty() {
        // A template with no captures matches literally and produces Unit.
        return Ok(db.unit());
    }

    let any_named = captures
        .iter()
        .any(|p| matches!(p, TemplatePart::Capture { name: Some(_), .. }));

    if any_named {
        // Named captures → anonymous structural record (§5.6, M7 ADR-025).
        return record_type(&captures, db, out);
    }

    // All anonymous: scalar if one, tuple if many (§7.3).
    let mut elem_types: Vec<Type> = Vec::with_capacity(captures.len());
    for p in &captures {
        let TemplatePart::Capture { parser, .. } = p else {
            unreachable!("filtered to captures")
        };
        elem_types.push(synth(parser, db, out)?);
    }
    if elem_types.len() == 1 {
        Ok(elem_types[0])
    } else {
        Ok(db.tuple(TupleElems::new(elem_types)?))
    }
}

/// Build an anonymous record type from named captures (M7).
///
/// Named-capture templates produce anonymous structural records (§5.6). The
/// record type is a proper `TypeData::Record` variant (M7, ADR-025), with fields
/// keyed by name. Two records with the same field names (in any order) and
/// structurally-equal types share one type.
fn record_type(
    captures: &[&TemplatePart],
    db: &mut TypeDb,
    out: &mut Vec<(Span, Type)>,
) -> Result<Type, TypeCtorError> {
    // Collect (name, type) pairs in source order. Display preserves this order;
    // identity is name-set-based (§5.6), established through unification.
    let mut fields = Vec::with_capacity(captures.len());
    for part in captures {
        match part {
            TemplatePart::Capture { name, parser, .. } => {
                let name_str = name
                    .as_ref()
                    .map(|n| n.as_str().to_string())
                    .unwrap_or_default();
                fields.push((name_str, synth(parser, db, out)?));
            }
            _ => unreachable!("filtered to captures"),
        }
    }
    Ok(db.record(None, FieldSet::from_pairs(fields)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{AtomicKind, CaptureName, TemplatePart};
    use praxis_source::Span;

    fn atom(kind: AtomicKind) -> ParserAst {
        ParserAst::Atomic {
            kind,
            span: Span::at(0),
        }
    }

    /// **IP-11's type half.** Every one of §7.4's ten atomics has a result
    /// type, and `uint`'s is `Int`.
    ///
    /// Not `ScalarType::UInt`: `praxis_repr::builtin_for_type` answers
    /// `NoRuntimeRepr` for `UInt` (it is reserved and has no runtime object,
    /// pinned by `a_type_with_no_runtime_object_has_no_descriptor`), and under
    /// D9 a JIT compile *fails* when a descriptor is missing — so a `uint`
    /// capture typed `UInt` would make every program containing one fail to
    /// compile. §7.4's non-negativity is the parse rule instead.
    #[test]
    fn every_atomic_the_design_requires_has_a_type() {
        use praxis_types::{ScalarType, TypeData};
        let mut db = TypeDb::new();
        for kind in AtomicKind::ALL {
            let t = synthesize(&atom(*kind), &mut db).expect("an atomic synthesizes");
            let expected = match kind {
                AtomicKind::Int | AtomicKind::UInt | AtomicKind::Digit => ScalarType::Int,
                AtomicKind::Float => ScalarType::Float,
                AtomicKind::Byte => ScalarType::Byte,
                AtomicKind::Char => ScalarType::Char,
                AtomicKind::Word | AtomicKind::Identifier | AtomicKind::Text | AtomicKind::Rest => {
                    ScalarType::Text
                }
            };
            match db.data(t) {
                TypeData::Scalar(s) => assert_eq!(*s, expected, "for `{}`", kind.keyword()),
                other => panic!("`{}` must be a scalar, got {other:?}", kind.keyword()),
            }
            assert!(
                !matches!(db.data(t), TypeData::Scalar(ScalarType::UInt)),
                "`{}` must not be typed UInt: it has no runtime object",
                kind.keyword()
            );
        }
    }

    #[test]
    fn atomic_int_synthesizes_int() {
        let mut db = TypeDb::new();
        let t = synthesize(&atom(AtomicKind::Int), &mut db).expect("int synthesizes");
        // TypeDb does not deduplicate handles; compare by data shape.
        assert!(matches!(
            db.data(t),
            praxis_types::TypeData::Scalar(praxis_types::ScalarType::Int)
        ));
    }

    #[test]
    fn lines_of_int_is_vec_int() {
        let mut db = TypeDb::new();
        let ast = ParserAst::Lines {
            child: Box::new(atom(AtomicKind::Int)),
            span: Span::at(0),
        };
        let t = synthesize(&ast, &mut db).expect("a valid AST synthesizes");
        match db.data(t) {
            praxis_types::TypeData::Collection { ctor, args } => {
                assert_eq!(*ctor, CollectionCtor::Vec);
                assert_eq!(args.len(), 1);
                assert!(
                    matches!(
                        db.data(args[0]),
                        praxis_types::TypeData::Scalar(praxis_types::ScalarType::Int)
                    ),
                    "the Vec element must be Int, got {}",
                    db.render(args[0])
                );
            }
            other => panic!("expected Vec, got {other:?}"),
        }
    }

    #[test]
    fn grid_of_char_is_grid_char() {
        let mut db = TypeDb::new();
        let ast = ParserAst::Grid {
            child: Box::new(atom(AtomicKind::Char)),
            span: Span::at(0),
        };
        let t = synthesize(&ast, &mut db).expect("a valid AST synthesizes");
        match db.data(t) {
            praxis_types::TypeData::Collection { ctor, args } => {
                assert_eq!(*ctor, CollectionCtor::Grid);
                assert_eq!(args.len(), 1);
                assert!(
                    matches!(
                        db.data(args[0]),
                        praxis_types::TypeData::Scalar(praxis_types::ScalarType::Char)
                    ),
                    "the Grid element must be Char, got {}",
                    db.render(args[0])
                );
            }
            other => panic!("expected Grid, got {other:?}"),
        }
    }

    #[test]
    fn nested_sections_lines_csv_int() {
        // sections(lines(csv(int))) → Vec[Vec[Vec[Int]]]
        let mut db = TypeDb::new();
        let ast = ParserAst::Sections {
            child: Box::new(ParserAst::Lines {
                child: Box::new(ParserAst::Csv {
                    child: Box::new(atom(AtomicKind::Int)),
                    span: Span::at(0),
                }),
                span: Span::at(0),
            }),
            span: Span::at(0),
        };
        let t = synthesize(&ast, &mut db).expect("a valid AST synthesizes");
        // Walk three Vec levels.
        let mut current = t;
        for level in 1..=3 {
            let praxis_types::TypeData::Collection { ctor, args } = db.data(current) else {
                panic!("level {level} should be Vec, got {}", db.render(current));
            };
            assert_eq!(*ctor, CollectionCtor::Vec, "wrong ctor at level {level}");
            assert_eq!(args.len(), 1, "wrong arity at level {level}");
            current = args[0];
        }
        assert!(
            matches!(
                db.data(current),
                praxis_types::TypeData::Scalar(praxis_types::ScalarType::Int)
            ),
            "nested leaf must be Int, got {}",
            db.render(current)
        );
    }

    #[test]
    fn template_single_anonymous_capture_is_scalar() {
        let mut db = TypeDb::new();
        let ast = ParserAst::Template {
            parts: vec![TemplatePart::Capture {
                name: None,
                parser: Box::new(atom(AtomicKind::Int)),
                span: Span::at(0),
                name_span: None,
            }],
            span: Span::at(0),
        };
        let t = synthesize(&ast, &mut db).expect("a valid AST synthesizes");
        // Single anonymous capture → scalar Int.
        assert!(matches!(
            db.data(t),
            praxis_types::TypeData::Scalar(praxis_types::ScalarType::Int)
        ));
    }

    #[test]
    fn template_two_anonymous_captures_is_tuple() {
        let mut db = TypeDb::new();
        let ast = ParserAst::Template {
            parts: vec![
                TemplatePart::Capture {
                    name: None,
                    parser: Box::new(atom(AtomicKind::Int)),
                    span: Span::at(0),
                    name_span: None,
                },
                TemplatePart::Capture {
                    name: None,
                    parser: Box::new(atom(AtomicKind::Int)),
                    span: Span::at(0),
                    name_span: None,
                },
            ],
            span: Span::at(0),
        };
        let t = synthesize(&ast, &mut db).expect("a valid AST synthesizes");
        assert!(matches!(db.data(t), praxis_types::TypeData::Tuple(_)));
    }

    #[test]
    fn template_named_captures_synthesize_anonymous_record() {
        // `{x:int},{y:int}` → anonymous record { x: Int, y: Int }.
        let mut db = TypeDb::new();
        let ast = ParserAst::Template {
            parts: vec![
                TemplatePart::Capture {
                    name: Some(CaptureName::parse("x").expect("an identifier")),
                    parser: Box::new(atom(AtomicKind::Int)),
                    span: Span::at(0),
                    name_span: None,
                },
                TemplatePart::Capture {
                    name: Some(CaptureName::parse("y").expect("an identifier")),
                    parser: Box::new(atom(AtomicKind::Int)),
                    span: Span::at(0),
                    name_span: None,
                },
            ],
            span: Span::at(0),
        };
        let t = synthesize(&ast, &mut db).expect("a valid AST synthesizes");
        let praxis_types::TypeData::Record { def, .. } = db.data(t) else {
            panic!("expected Record, got {:?}", db.data(t));
        };
        let rdef = db.record_def(*def);
        assert!(rdef.name.is_none(), "anonymous record has no name");
        assert_eq!(rdef.arity(), 2);
        let (idx, _) = rdef.field("x").expect("field x");
        assert_eq!(idx, 0);
        // Renders as the structural record form.
        assert_eq!(db.render(t), "{ x: Int, y: Int }");
    }

    #[test]
    fn lines_of_named_captures_is_vec_of_record() {
        // lines(`{x:int},{y:int}`) → Vec[{ x: Int, y: Int }].
        let mut db = TypeDb::new();
        let ast = ParserAst::Lines {
            child: Box::new(ParserAst::Template {
                parts: vec![
                    TemplatePart::Capture {
                        name: Some(CaptureName::parse("x").expect("an identifier")),
                        parser: Box::new(atom(AtomicKind::Int)),
                        span: Span::at(0),
                        name_span: None,
                    },
                    TemplatePart::Capture {
                        name: Some(CaptureName::parse("y").expect("an identifier")),
                        parser: Box::new(atom(AtomicKind::Int)),
                        span: Span::at(0),
                        name_span: None,
                    },
                ],
                span: Span::at(0),
            }),
            span: Span::at(0),
        };
        let t = synthesize(&ast, &mut db).expect("a valid AST synthesizes");
        match db.data(t) {
            praxis_types::TypeData::Collection { ctor, args } => {
                assert_eq!(*ctor, CollectionCtor::Vec);
                assert_eq!(args.len(), 1);
                assert!(matches!(
                    db.data(args[0]),
                    praxis_types::TypeData::Record { .. }
                ));
            }
            other => panic!("expected Vec[Record], got {other:?}"),
        }
        assert_eq!(db.render(t), "Vec[{ x: Int, y: Int }]");
    }

    /// **A counted group is a `Vec` field where it was written.** The unbounded
    /// tail contributes the same `Vec[result(P)]`, but only ever at the end;
    /// the record's field order is the source order of the named arguments, so
    /// a fixed field after a counted group lands after it in the record too.
    /// This is the whole type story of the counted form — `read`'s inference
    /// takes whatever type comes back and needed no arm of its own.
    #[test]
    fn a_counted_group_is_a_vec_field_in_the_position_it_was_written() {
        use crate::ast::{RepeatCount, SectionItem};

        let mut db = TypeDb::new();
        let ast = ParserAst::SectionsNamed {
            fields: vec![
                SectionItem::Counted {
                    name: "shapes".to_string(),
                    count: RepeatCount::new(6).expect("six sections"),
                    parser: ParserAst::Lines {
                        child: Box::new(atom(AtomicKind::Int)),
                        span: Span::at(0),
                    },
                },
                SectionItem::One {
                    name: "regions".to_string(),
                    parser: ParserAst::Lines {
                        child: Box::new(atom(AtomicKind::Int)),
                        span: Span::at(0),
                    },
                },
            ],
            repeated_tail: None,
            span: Span::at(0),
        };
        let t = synthesize(&ast, &mut db).expect("a valid AST synthesizes");
        assert_eq!(db.render(t), "{ shapes: Vec[Vec[Int]], regions: Vec[Int] }");
    }
}
