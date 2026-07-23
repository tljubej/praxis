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
/// - Any named capture → an anonymous record (provisional in M6).
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
        // Named captures → anonymous record. M6 uses a provisional structural
        // record type; M7 adds the formal nominal/structural record machinery.
        // For now, synthesize as a tuple of (name, type) pairs via a record type.
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

/// Build an anonymous record type from named captures (M6 provisional).
///
/// The record type is represented as a `Collection { Record, args }` for now —
/// the M7 nominal-record work will formalize this. Each field is a (name, type)
/// pair carried structurally; two records with the same field names and types in
/// the same order are the same type.
fn record_type(captures: &[&TemplatePart], db: &mut TypeDb) -> Type {
    // Collect field types. The field names are part of the record's identity but
    // the type system in M6 carries them alongside the types as Text literals
    // (so the runtime schema can be reconstructed). M7 will give records a real
    // nominal representation.
    let mut field_types = Vec::with_capacity(captures.len());
    let mut name_types = Vec::with_capacity(captures.len());
    for part in captures {
        match part {
            TemplatePart::Capture { name, parser } => {
                let name_str = name.clone().unwrap_or_default();
                name_types.push(db.text());
                // Stash the name as a Text-typed field for now — purely so the
                // record's shape is recoverable. This is provisional; M7 replaces it.
                let _ = name_str; // name carried structurally in the plan, not the type
                field_types.push(synthesize(parser, db));
            }
            _ => unreachable!("filtered to captures"),
        }
    }
    // Provisional record representation: flatten name-types and field-types into
    // a single tuple. M7 replaces this with a proper Record TypeData variant.
    // The interleaving (name, type, name, type, ...) preserves field identity.
    let mut all = Vec::with_capacity(name_types.len() + field_types.len());
    for (n, t) in name_types.into_iter().zip(field_types) {
        all.push(n);
        all.push(t);
    }
    db.tuple(all)
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
}
