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
use praxis_types::{CollectionCtor, Type, TypeDb};

/// Synthesize the result type of a parser expression (§7.8).
///
/// Assumes the AST has already passed [`validate`](crate::validate::validate).
/// Panics only on an internal inconsistency (a shape validation should have
/// rejected), never on user input alone.
pub fn synthesize(ast: &ParserAst, db: &mut TypeDb) -> Type {
    match ast {
        ParserAst::Atomic { kind, .. } => atomic_type(*kind, db),
        ParserAst::Template { parts, .. } => template_type(parts, db),
        ParserAst::Lines { child, .. }
        | ParserAst::Sections { child, .. }
        | ParserAst::Csv { child, .. }
        | ParserAst::Ws { child, .. } => {
            let elem = synthesize(child, db);
            db.vec(elem)
        }
        ParserAst::Sep { child, .. } => {
            let elem = synthesize(child, db);
            db.vec(elem)
        }
        ParserAst::Grid { child, .. } => {
            let elem = synthesize(child, db);
            db.collection(CollectionCtor::Grid, vec![elem])
        }
        ParserAst::SectionsNamed {
            fields,
            repeated_tail,
            ..
        } => {
            // Anonymous record: one field per named section, plus a final
            // `Vec[result(P)]` field for the `repeated` tail (if any).
            let mut rec_fields: Vec<(String, Type)> = fields
                .iter()
                .map(|(name, p)| (name.clone(), synthesize(p, db)))
                .collect();
            if let Some((name, tail)) = repeated_tail {
                let elem = synthesize(tail, db);
                rec_fields.push((name.clone(), db.vec(elem)));
            }
            db.anon_record(rec_fields)
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
                                } = part
                                {
                                    rec_fields.push((n.clone(), synthesize(parser, db)));
                                }
                            }
                        }
                    }
                    crate::ast::BlockItem::Named { name, parser } => {
                        rec_fields.push((name.clone(), synthesize(parser, db)));
                    }
                }
            }
            db.anon_record(rec_fields)
        }
        ParserAst::Choice { cases, .. } => {
            // Anonymous enum (§7.5): one variant per case, each carrying the
            // case's result type as a single-element payload (so the parsed
            // value is recoverable via match). Identity is name+signature-based
            // via the M9 unify arm + anon_enum's synthetic name.
            let variants: Vec<(String, Option<Vec<Type>>)> = cases
                .iter()
                .map(|(name, p)| {
                    let payload_ty = synthesize(p, db);
                    (name.clone(), Some(vec![payload_ty]))
                })
                .collect();
            db.anon_enum(variants)
        }
        ParserAst::Optional { child, .. } => {
            // `Option[result(P)]` (§7.5/§7.8): a nominal Option enum (Some(T),
            // None) carrying the child's result type. Registered fresh per site;
            // unifies with other Option[T] values via the M9 same-named-enum arm.
            let elem = synthesize(child, db);
            db.register_enum(
                "Option",
                vec![("Some".into(), Some(vec![elem])), ("None".into(), None)],
            )
        }
        ParserAst::Scan { child, .. } => {
            // `scan(P)` → `Vec[result(P)]` (§7.5): matches in source order.
            let elem = synthesize(child, db);
            db.vec(elem)
        }
        ParserAst::OneOf { .. } => {
            // `one_of("LR")` → Char (§7.5).
            db.char()
        }
        ParserAst::Characters { .. } => {
            // `chars(P, skip:)` → Vec[Char] (§7.5).
            let ch = db.char();
            db.vec(ch)
        }
        ParserAst::Matrix { child, .. } | ParserAst::GridRagged { child, .. } => {
            // `matrix(P)` / ragged `grid(P)` → Grid[result(P)] (§7.5, ADR-030).
            let elem = synthesize(child, db);
            db.collection(CollectionCtor::Grid, vec![elem])
        }
    }
}

/// The result type of an atomic parser (§7.4).
fn atomic_type(kind: AtomicKind, db: &mut TypeDb) -> Type {
    match kind {
        AtomicKind::Int | AtomicKind::Digit => db.int(),
        AtomicKind::Char => db.char(),
        AtomicKind::Word | AtomicKind::Text | AtomicKind::Rest => db.text(),
    }
}

/// The result type of a template (§7.3).
///
/// - A single anonymous capture → the scalar type.
/// - Multiple anonymous captures → a tuple.
/// - Any named capture → an anonymous record (§5.6; formalized in M7).
fn template_type(parts: &[TemplatePart], db: &mut TypeDb) -> Type {
    let captures: Vec<&TemplatePart> = parts
        .iter()
        .filter(|p| matches!(p, TemplatePart::Capture { .. }))
        .collect();

    if captures.is_empty() {
        // A template with no captures matches literally and produces Unit.
        return db.unit();
    }

    let any_named = captures
        .iter()
        .any(|p| matches!(p, TemplatePart::Capture { name: Some(_), .. }));

    if any_named {
        // Named captures → anonymous structural record (§5.6, M7 ADR-025).
        record_type(&captures, db)
    } else {
        // All anonymous: scalar if one, tuple if many (§7.3).
        let elem_types: Vec<Type> = captures
            .iter()
            .map(|p| match p {
                TemplatePart::Capture { parser, .. } => synthesize(parser, db),
                _ => unreachable!("filtered to captures"),
            })
            .collect();
        if elem_types.len() == 1 {
            elem_types[0]
        } else {
            db.tuple(elem_types)
        }
    }
}

/// Build an anonymous record type from named captures (M7).
///
/// Named-capture templates produce anonymous structural records (§5.6). The
/// record type is a proper `TypeData::Record` variant (M7, ADR-025), with fields
/// keyed by name. Two records with the same field names (in any order) and
/// structurally-equal types share one type.
fn record_type(captures: &[&TemplatePart], db: &mut TypeDb) -> Type {
    // Collect (name, type) pairs in source order. Display preserves this order;
    // identity is name-set-based (§5.6), handled by db.anon_record's
    // canonicalization.
    let mut fields = Vec::with_capacity(captures.len());
    for part in captures {
        match part {
            TemplatePart::Capture { name, parser } => {
                let name_str = name.clone().unwrap_or_default();
                fields.push((name_str, synthesize(parser, db)));
            }
            _ => unreachable!("filtered to captures"),
        }
    }
    db.anon_record(fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{AtomicKind, TemplatePart};
    use praxis_source::Span;

    fn atom(kind: AtomicKind) -> ParserAst {
        ParserAst::Atomic {
            kind,
            span: Span::at(0),
        }
    }

    #[test]
    fn atomic_int_synthesizes_int() {
        let mut db = TypeDb::new();
        let t = synthesize(&atom(AtomicKind::Int), &mut db);
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
        let t = synthesize(&ast, &mut db);
        match db.data(t) {
            praxis_types::TypeData::Collection { ctor, args } => {
                assert_eq!(*ctor, CollectionCtor::Vec);
                assert_eq!(args.len(), 1);
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
        let t = synthesize(&ast, &mut db);
        match db.data(t) {
            praxis_types::TypeData::Collection { ctor, args } => {
                assert_eq!(*ctor, CollectionCtor::Grid);
                assert_eq!(args.len(), 1);
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
        let t = synthesize(&ast, &mut db);
        // Walk three Vec levels.
        let d = db.data(t);
        let praxis_types::TypeData::Collection { ctor, args } = d else {
            panic!("expected Collection");
        };
        assert_eq!(*ctor, CollectionCtor::Vec);
        assert_eq!(args.len(), 1);
    }

    #[test]
    fn template_single_anonymous_capture_is_scalar() {
        let mut db = TypeDb::new();
        let ast = ParserAst::Template {
            parts: vec![TemplatePart::Capture {
                name: None,
                parser: Box::new(atom(AtomicKind::Int)),
            }],
            span: Span::at(0),
        };
        let t = synthesize(&ast, &mut db);
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
                },
                TemplatePart::Capture {
                    name: None,
                    parser: Box::new(atom(AtomicKind::Int)),
                },
            ],
            span: Span::at(0),
        };
        let t = synthesize(&ast, &mut db);
        assert!(matches!(db.data(t), praxis_types::TypeData::Tuple(_)));
    }

    #[test]
    fn template_named_captures_synthesize_anonymous_record() {
        // `{x:int},{y:int}` → anonymous record { x: Int, y: Int }.
        let mut db = TypeDb::new();
        let ast = ParserAst::Template {
            parts: vec![
                TemplatePart::Capture {
                    name: Some("x".into()),
                    parser: Box::new(atom(AtomicKind::Int)),
                },
                TemplatePart::Capture {
                    name: Some("y".into()),
                    parser: Box::new(atom(AtomicKind::Int)),
                },
            ],
            span: Span::at(0),
        };
        let t = synthesize(&ast, &mut db);
        let praxis_types::TypeData::Record { def } = db.data(t) else {
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
                        name: Some("x".into()),
                        parser: Box::new(atom(AtomicKind::Int)),
                    },
                    TemplatePart::Capture {
                        name: Some("y".into()),
                        parser: Box::new(atom(AtomicKind::Int)),
                    },
                ],
                span: Span::at(0),
            }),
            span: Span::at(0),
        };
        let t = synthesize(&ast, &mut db);
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
}
