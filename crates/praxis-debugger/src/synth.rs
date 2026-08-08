//! Spelling a snapshot's static types back into Praxis source (§9.5 steps 1–3).
//!
//! `p EXPR` type-checks by synthesizing `fn __p_expr(<typed params>) { EXPR }`
//! and running the ordinary pipeline over it, so every parameter annotation has
//! to be something the *type grammar* can parse and the synthetic module can
//! resolve. A program's locals routinely have neither, and
//! [`TypeDb::render`](praxis_types::TypeDb::render) does not notice: it answers
//! "what does this type look like to a human", which is a different question.
//! It prints a nominal record as the bare name `Foo` — a name the synthetic
//! module never declares — an anonymous parser-template record as
//! `{ x: Int, y: Int }`, which the type grammar has no syntax for at all, and an
//! unresolved variable as `?T`. Handing any of those to the parser fails the
//! whole command — `p 1 + 2` included, which mentions no local at all — because
//! one unparseable parameter annotation sinks the synthetic module.
//!
//! [`Speller`] answers the other question. It walks a [`Type`] and returns
//! source the synthetic module compiles, **emitting the `struct`/`enum`
//! declarations that spelling needs as it goes**: the program's own
//! declarations are not in scope there, so a `Foo` parameter is only meaningful
//! next to the `struct Foo { … }` this module writes out. Field order is the
//! definition's, and that is load-bearing rather than cosmetic — a field read
//! lowers to an *indexed* load (`Inst::LoadField`), so same order means same
//! index means the emitted type describes the value that is actually there.
//!
//! An anonymous record or enum has no name to write, so one is minted for it
//! (`__p_rec0`, `__p_enum0`) and declared like any other. Nominal identity is
//! static-only — a record object carries a field schema, not a name — so a
//! minted name describes the value exactly as well as the original did.
//! [`humanize`] puts the structural rendering back for display, so `type` still
//! answers `Vec[{ x: Int, y: Int }]`.
//!
//! Spelling is **fallible on purpose**: a type with no source syntax (an
//! unresolved `?T`, a `Byte`, the internal `Seq`) returns `None` rather than
//! text the parser will reject. The caller drops that one binding, which costs
//! the local and not the whole command.

use std::collections::HashMap;

use praxis_types::{CollectionCtor, EnumDefId, RecordDefId, ScalarType, Type, TypeData, TypeDb};

/// The prefix every name this module mints carries. It shares `__p_expr`'s
/// reservation (§9.5): an identifier the language allows but no program is
/// expected to declare.
const MINTED_PREFIX: &str = "__p_";

/// Walks types, producing source-level spellings and the declarations they
/// need. One `Speller` serves one synthesized module — the declarations it
/// accumulates and the spellings it returns only mean anything together.
pub struct Speller<'a> {
    db: &'a TypeDb,
    /// The `struct`/`enum` declarations to emit, in the order they were needed.
    /// Order is presentation only: items are hoisted, so a declaration may
    /// precede or follow its use.
    decls: Vec<String>,
    /// Record defs already declared → the name to spell them with. Recorded
    /// *before* the fields are walked, so a self-referring type terminates
    /// rather than recursing forever (the language rejects one with `N006`, so
    /// this is a guard, not a feature).
    records: HashMap<RecordDefId, String>,
    /// Enum defs already declared → the name to spell them with.
    enums: HashMap<EnumDefId, String>,
    /// Every name declared in the module so far, across both kinds. A second
    /// declaration under a name already taken is a redeclaration error that
    /// would fail the whole command, so a def that would need one is declined
    /// instead — and a minted name skips past anything already here.
    claimed: std::collections::HashSet<String>,
    /// Minted name → the rendering of the type it stands for, so `type EXPR`
    /// and `heap EXPR` report `{ x: Int }` and not `__p_rec0`.
    minted: Vec<(String, String)>,
}

impl<'a> Speller<'a> {
    #[must_use]
    pub fn new(db: &'a TypeDb) -> Speller<'a> {
        Speller {
            db,
            decls: Vec::new(),
            records: HashMap::new(),
            enums: HashMap::new(),
            claimed: std::collections::HashSet::new(),
            minted: Vec::new(),
        }
    }

    /// Spell `ty` as a type annotation the synthetic module can resolve, or
    /// `None` when the language cannot write it. Declarations the spelling
    /// depends on are recorded; see [`declarations`](Self::declarations).
    pub fn spell(&mut self, ty: Type) -> Option<String> {
        let ty = self.db.follow(ty);
        // Cloned because spelling the children needs `&mut self`, and the arena
        // borrow behind `data` would outlive the match otherwise.
        match self.db.data(ty).clone() {
            TypeData::Scalar(s) => spell_scalar(s).map(str::to_string),
            TypeData::Unit => Some("Unit".to_string()),
            // No value has type `Never` (§4.3), so no local is one and there is
            // nothing to bind.
            TypeData::Never => None,
            TypeData::Tuple(els) => {
                let els = self.spell_all(&els)?;
                Some(format!("({})", els.join(", ")))
            }
            TypeData::Func { params, result } => self.spell_func(&params, result),
            TypeData::Collection { ctor, args } => {
                // `Seq` is the compiler-internal pipeline source (§6.3) and is
                // never user-named, so `Seq[T]` in source is an unknown type.
                if ctor == CollectionCtor::Seq {
                    return None;
                }
                let args = self.spell_all(&args)?;
                Some(if args.is_empty() {
                    // A nullary ctor prints bare: `Range`, not `Range[]`.
                    ctor.name().to_string()
                } else {
                    format!("{}[{}]", ctor.name(), args.join(", "))
                })
            }
            // A record def's own type parameters are always empty — there is no
            // `struct P[T]` syntax — so an instance supplying arguments is one
            // this grammar cannot write.
            TypeData::Record { def, args } if args.is_empty() => self.record_name(def, Some(ty)),
            TypeData::Record { .. } => None,
            TypeData::Enum { def, args } => self.spell_enum(def, &args, ty),
            // An unresolved variable renders `?T`, which is not a type name.
            // Inference leaves one behind for an unfilled `Vec()` and for any
            // local whose element type nothing pinned down.
            TypeData::Var(_) => None,
        }
    }

    /// The declarations the spellings issued so far depend on, as source. Empty
    /// when no user type was named.
    #[must_use]
    pub fn declarations(&self) -> String {
        self.decls.join("\n")
    }

    /// The minted name → structural rendering pairs, for [`humanize`]. Consuming
    /// releases the `TypeDb` borrow, which is what lets the caller keep the
    /// substitutions after the speller is done with the arena.
    #[must_use]
    pub fn into_minted(self) -> Vec<(String, String)> {
        self.minted
    }

    /// Spell every type in `tys`, or `None` if any one of them cannot be spelled
    /// — a composite is only as writable as its least writable part.
    fn spell_all(&mut self, tys: &[Type]) -> Option<Vec<String>> {
        tys.iter().map(|t| self.spell(*t)).collect()
    }

    /// `(P, …) -> R`. The parameter group is read back by flattening a tuple
    /// annotation into the parameter list, so a *single* tuple parameter cannot
    /// be spelled: `((Int, Text)) -> R` reads as the two-parameter function, and
    /// writing a type that means something else is worse than declining to
    /// write one.
    fn spell_func(&mut self, params: &[Type], result: Type) -> Option<String> {
        if let [only] = params {
            if matches!(self.db.data(self.db.follow(*only)), TypeData::Tuple(_)) {
                return None;
            }
        }
        let params = self.spell_all(params)?;
        let result = self.spell(result)?;
        Some(format!("({}) -> {result}", params.join(", ")))
    }

    /// `Option[T]` is the prelude's own def and is in scope everywhere, so it is
    /// spelled and never declared. Every other enum is the user's.
    fn spell_enum(&mut self, def: EnumDefId, args: &[Type], ty: Type) -> Option<String> {
        if def == self.db.option_def() {
            // The one generic def: exactly one argument, always.
            let [elem] = args else { return None };
            let elem = self.spell(*elem)?;
            return Some(format!("Option[{elem}]"));
        }
        // As for records: no `enum E[T]` syntax, so an applied def is unwritable.
        if args.is_empty() {
            self.enum_name(def, Some(ty))
        } else {
            None
        }
    }

    /// Declare the nominal `struct`/`enum` types whose names appear in `names`,
    /// so an expression can *write* one: `p Pt{x: 1, y: 2}`, `type Move`,
    /// `p match Step(6, 7) { … }`. Spelling a local's type only reaches the types
    /// some local *has*, and a program's declarations are no more in scope in the
    /// synthetic module than its locals are.
    ///
    /// A record is written by its own name; an **enum is written by its variants'**
    /// (`Step(1, 2)`, `Stay`), so both spellings select an enum. A name matching
    /// nothing, and a def whose members cannot be spelled, are skipped — this
    /// adds declarations and never fails.
    pub fn declare_named(&mut self, names: &std::collections::HashSet<String>) {
        let records: Vec<RecordDefId> = self
            .db
            .record_defs()
            .filter(|(_, d)| d.name.as_ref().is_some_and(|n| names.contains(n)))
            .map(|(id, _)| id)
            .collect();
        for def in records {
            self.record_name(def, None);
        }
        let option = self.db.option_def();
        let enums: Vec<EnumDefId> = self
            .db
            .enum_defs()
            // The prelude `Option` is always in scope and must never be
            // re-declared; `Some`/`None` would otherwise select it by variant.
            .filter(|(id, _)| *id != option)
            .filter(|(_, d)| {
                d.name.as_ref().is_some_and(|n| names.contains(n))
                    || d.variants.iter().any(|v| names.contains(&v.name))
            })
            .map(|(id, _)| id)
            .collect();
        for def in enums {
            self.enum_name(def, None);
        }
    }

    /// The name to spell `def` with, declaring it on first use.
    ///
    /// `instance` is the type being spelled, needed only to render what a minted
    /// name stands for — so it is `None` exactly when the def was reached by
    /// *name* ([`declare_named`](Self::declare_named)), and an anonymous def has
    /// no name to have been reached by. The `?` is that impossibility, written
    /// down rather than asserted.
    fn record_name(&mut self, def: RecordDefId, instance: Option<Type>) -> Option<String> {
        if let Some(name) = self.records.get(&def) {
            return Some(name.clone());
        }
        let rdef = self.db.record_def(def).clone();
        let name = match &rdef.name {
            Some(n) => self.claim(n)?,
            None => self.mint("rec", instance?),
        };
        self.records.insert(def, name.clone());
        let mut fields = Vec::with_capacity(rdef.fields.len());
        for f in &rdef.fields {
            let Some(ty) = self.spell(f.ty) else {
                self.records.remove(&def);
                self.claimed.remove(&name);
                return None;
            };
            fields.push(format!("{}: {ty}", f.name));
        }
        self.decls
            .push(format!("struct {name} {{ {} }}", fields.join(", ")));
        Some(name)
    }

    /// The name to spell `def` with, declaring it on first use. Variant order is
    /// the definition's, load-bearing for the reason a record's field order is:
    /// a variant is told apart by its tag index. `instance` is `None` under the
    /// same rule as [`record_name`](Self::record_name).
    fn enum_name(&mut self, def: EnumDefId, instance: Option<Type>) -> Option<String> {
        if let Some(name) = self.enums.get(&def) {
            return Some(name.clone());
        }
        let edef = self.db.enum_def(def).clone();
        // `params` is non-empty only for the prelude `Option`, which
        // `spell_enum` has already taken; anything else generic is unwritable.
        if !edef.params.is_empty() {
            return None;
        }
        let name = match &edef.name {
            Some(n) => self.claim(n)?,
            None => self.mint("enum", instance?),
        };
        self.enums.insert(def, name.clone());
        let mut variants = Vec::with_capacity(edef.variants.len());
        for v in &edef.variants {
            if v.payload.is_empty() {
                variants.push(v.name.clone());
                continue;
            }
            let Some(payload) = self.spell_all(&v.payload) else {
                self.enums.remove(&def);
                self.claimed.remove(&name);
                return None;
            };
            variants.push(format!("{}({})", v.name, payload.join(", ")));
        }
        self.decls
            .push(format!("enum {name} {{ {} }}", variants.join(", ")));
        Some(name)
    }

    /// Take `name` for a declaration, or `None` if the module already declared
    /// something under it. Two defs of one name cannot both be written down —
    /// the second is a redeclaration error that would fail the command — so the
    /// second declines and its local is dropped, like any other type this module
    /// cannot express.
    fn claim(&mut self, name: &str) -> Option<String> {
        self.claimed
            .insert(name.to_string())
            .then(|| name.to_string())
    }

    /// Mint a fresh name for an anonymous def, remembering the rendering it
    /// stands for so [`humanize`] can undo it. The counter skips anything
    /// already declared, so `__p_` being a *convention* rather than a reserved
    /// word costs nothing: a program that declares `__p_rec0` itself just moves
    /// the minted name along.
    fn mint(&mut self, what: &str, ty: Type) -> String {
        let mut n = self.minted.len();
        let name = loop {
            let candidate = format!("{MINTED_PREFIX}{what}{n}");
            if self.claimed.insert(candidate.clone()) {
                break candidate;
            }
            n += 1;
        };
        self.minted.push((name.clone(), self.db.render(ty)));
        name
    }
}

/// Replace every minted name in `rendered` with the structural rendering it
/// stands for, so a type the user's program never contained is not reported back
/// at them: `type points` answers `Vec[{ x: Int, y: Int }]`, not `Vec[__p_rec0]`.
///
/// Textual substitution is sound here because a minted name is unique in the
/// module and one is only ever a prefix of another by its trailing counter,
/// which the longest-first order settles (`__p_rec10` before `__p_rec1`).
#[must_use]
pub fn humanize(minted: &[(String, String)], rendered: &str) -> String {
    let mut names: Vec<&(String, String)> = minted.iter().collect();
    names.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
    let mut out = rendered.to_string();
    for (name, display) in names {
        out = out.replace(name.as_str(), display);
    }
    out
}

/// The scalar names the type grammar accepts. `UInt` and `Byte` are reserved but
/// unwritable (§4.3, ADR-007): annotating one is `N002 unknown type`, so a local
/// of that type has no spelling and is dropped rather than mis-spelled.
fn spell_scalar(s: ScalarType) -> Option<&'static str> {
    match s {
        ScalarType::Int
        | ScalarType::Text
        | ScalarType::Bool
        | ScalarType::Char
        | ScalarType::Float => Some(s.name()),
        ScalarType::UInt | ScalarType::Byte => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_types::{CollectionArgs, FieldSet, TupleElems, VariantSet};

    /// The round trip that matters: the emitted declarations plus the spelling
    /// have to parse *and* type-check, which is exactly what the synthetic
    /// module asks of them.
    fn compiles(decls: &str, annotation: &str) {
        let source = format!("{decls}\nfn __probe(v: {annotation}) {{ v }}");
        let map = praxis_source::SourceMap::new();
        let file = map.intern("probe.px", &source);
        let parsed = praxis_parser::parse(file, &source);
        if let Some(d) = parsed.diagnostics.first() {
            panic!("parse error: {} in\n{source}", d.message());
        }
        let analysis = praxis_hir::analyze_root(file, &parsed.tree);
        if let Some(d) = analysis.diagnostics.first() {
            panic!("analysis error: {} in\n{source}", d.message());
        }
    }

    fn vec_of(db: &mut TypeDb, elem: Type) -> Type {
        let args = CollectionArgs::new(CollectionCtor::Vec, vec![elem]).expect("Vec takes one arg");
        db.collection(CollectionCtor::Vec, args).expect("Vec[T]")
    }

    #[test]
    fn scalars_and_composites_spell_themselves() {
        let mut db = TypeDb::new();
        let int = db.int();
        let text = db.text();
        let tuple = db.tuple(TupleElems::pair(int, text));
        let vec = vec_of(&mut db, int);
        let unit = db.unit();
        let mut speller = Speller::new(&db);
        for (ty, expected) in [
            (int, "Int"),
            (text, "Text"),
            (unit, "Unit"),
            (tuple, "(Int, Text)"),
            (vec, "Vec[Int]"),
        ] {
            assert_eq!(speller.spell(ty).as_deref(), Some(expected));
            compiles("", expected);
        }
        assert_eq!(speller.declarations(), "", "no user type was named");
    }

    /// A nominal record spelled as its bare name is an *undeclared* name in the
    /// synthetic module, so the spelling has to bring the declaration with it.
    #[test]
    fn a_nominal_record_brings_its_declaration() {
        let mut db = TypeDb::new();
        let text = db.text();
        let int = db.int();
        let inner = db.record(
            Some("Poo".to_string()),
            FieldSet::from_pairs(vec![("z".to_string(), text)]).expect("one field"),
        );
        let outer = db.record(
            Some("Foo".to_string()),
            FieldSet::from_pairs(vec![("x".to_string(), inner), ("y".to_string(), int)])
                .expect("two fields"),
        );

        let mut speller = Speller::new(&db);
        assert_eq!(speller.spell(outer).as_deref(), Some("Foo"));
        let decls = speller.declarations();
        assert!(decls.contains("struct Poo { z: Text }"), "{decls}");
        assert!(decls.contains("struct Foo { x: Poo, y: Int }"), "{decls}");
        compiles(&decls, "Foo");
    }

    /// A transitively-referenced record is declared once, however many fields
    /// name it — a second `struct P` in the module is a redeclaration error.
    #[test]
    fn a_shared_record_is_declared_once() {
        let mut db = TypeDb::new();
        let int = db.int();
        let inner = db.record(
            Some("P".to_string()),
            FieldSet::from_pairs(vec![("n".to_string(), int)]).expect("one field"),
        );
        let outer = db.record(
            Some("Q".to_string()),
            FieldSet::from_pairs(vec![("a".to_string(), inner), ("b".to_string(), inner)])
                .expect("two fields"),
        );
        let mut speller = Speller::new(&db);
        speller.spell(outer).expect("Q spells");
        speller.spell(inner).expect("P spells");
        let decls = speller.declarations();
        assert_eq!(decls.matches("struct P ").count(), 1, "{decls}");
        compiles(&decls, "Q");
    }

    /// The ``read lines(`{x:int},{y:int}`)`` case: an anonymous structural record
    /// has *no* syntax in type position, so it gets a minted declaration instead
    /// — nominal identity is static-only, so the name describes the value just as
    /// completely.
    #[test]
    fn an_anonymous_record_is_minted_a_declaration() {
        let mut db = TypeDb::new();
        let int = db.int();
        let anon = db.record(
            None,
            FieldSet::from_pairs(vec![("x".to_string(), int), ("y".to_string(), int)])
                .expect("two fields"),
        );
        let vec = vec_of(&mut db, anon);

        let mut speller = Speller::new(&db);
        let spelled = speller.spell(vec).expect("the anonymous record spells");
        assert_eq!(spelled, "Vec[__p_rec0]");
        let decls = speller.declarations();
        assert_eq!(decls, "struct __p_rec0 { x: Int, y: Int }");
        compiles(&decls, &spelled);
        // …and the minted name never reaches the user.
        assert_eq!(
            humanize(&speller.into_minted(), "Vec[__p_rec0]"),
            "Vec[{ x: Int, y: Int }]",
            "the structural rendering is what `type` reports"
        );
    }

    #[test]
    fn a_nominal_enum_brings_its_declaration() {
        let mut db = TypeDb::new();
        let int = db.int();
        let ty = db.enum_(
            Some("Move".to_string()),
            VariantSet::from_pairs(vec![
                ("Step".to_string(), vec![int, int]),
                ("Stay".to_string(), Vec::new()),
            ])
            .expect("two variants"),
        );
        let mut speller = Speller::new(&db);
        assert_eq!(speller.spell(ty).as_deref(), Some("Move"));
        let decls = speller.declarations();
        assert_eq!(decls, "enum Move { Step(Int, Int), Stay }");
        compiles(&decls, "Move");
    }

    /// A type the expression *writes* is declared too. Spelling reaches only the
    /// types some local has, so `p Pt{x: 1, y: 2}` in a frame holding no `Pt`
    /// needs the declaration to come from the name the expression mentions.
    #[test]
    fn a_type_the_expression_names_is_declared_even_with_no_local_of_it() {
        let mut db = TypeDb::new();
        let int = db.int();
        db.record(
            Some("Pt".to_string()),
            FieldSet::from_pairs(vec![("x".to_string(), int), ("y".to_string(), int)])
                .expect("two fields"),
        );
        db.enum_(
            Some("Move".to_string()),
            VariantSet::from_pairs(vec![("Stay".to_string(), Vec::new())]).expect("one variant"),
        );
        db.record(
            Some("Unmentioned".to_string()),
            FieldSet::from_pairs(vec![("n".to_string(), int)]).expect("one field"),
        );

        let mut speller = Speller::new(&db);
        speller.declare_named(&["Pt".to_string(), "Move".to_string()].into_iter().collect());
        let decls = speller.declarations();
        assert!(decls.contains("struct Pt { x: Int, y: Int }"), "{decls}");
        assert!(decls.contains("enum Move { Stay }"), "{decls}");
        assert!(
            !decls.contains("Unmentioned"),
            "only what the expression named: {decls}"
        );
        compiles(&decls, "Pt");
    }

    /// An enum is written by its *variants*, not by its own name — `p Stay` and
    /// `p match m { Step(a, b) => … }` never spell `Move` — so a variant name
    /// selects the declaration too.
    #[test]
    fn an_enum_is_declared_by_a_variant_name() {
        let mut db = TypeDb::new();
        let int = db.int();
        db.enum_(
            Some("Move".to_string()),
            VariantSet::from_pairs(vec![
                ("Step".to_string(), vec![int, int]),
                ("Stay".to_string(), Vec::new()),
            ])
            .expect("two variants"),
        );
        let mut speller = Speller::new(&db);
        speller.declare_named(&["Stay".to_string()].into_iter().collect());
        assert_eq!(speller.declarations(), "enum Move { Step(Int, Int), Stay }");
    }

    /// One name, one declaration. Two defs of a name is not a shape a program's
    /// own `TypeDb` takes — a redeclaration is rejected at analysis — but the
    /// module this builds is assembled rather than parsed, and "declared twice"
    /// is the one way to assemble a source file that fails as a whole.
    #[test]
    fn a_name_is_declared_once_and_the_second_def_declines() {
        let mut db = TypeDb::new();
        let int = db.int();
        let text = db.text();
        let first = db.record(
            Some("Dup".to_string()),
            FieldSet::from_pairs(vec![("n".to_string(), int)]).expect("one field"),
        );
        let second = db.record(
            Some("Dup".to_string()),
            FieldSet::from_pairs(vec![("s".to_string(), text)]).expect("one field"),
        );
        let mut speller = Speller::new(&db);
        assert_eq!(speller.spell(first).as_deref(), Some("Dup"));
        assert_eq!(
            speller.spell(second),
            None,
            "the second def has no name left to be written under"
        );
        let decls = speller.declarations();
        assert_eq!(decls, "struct Dup { n: Int }");
        compiles(&decls, "Dup");
    }

    /// A minted name steps past a program's own declaration of it, so `__p_`
    /// stays a convention rather than a word the language has to reserve.
    #[test]
    fn a_minted_name_steps_past_a_declaration_that_already_took_it() {
        let mut db = TypeDb::new();
        let int = db.int();
        let squatter = db.record(
            Some("__p_rec0".to_string()),
            FieldSet::from_pairs(vec![("n".to_string(), int)]).expect("one field"),
        );
        let anon = db.record(
            None,
            FieldSet::from_pairs(vec![("x".to_string(), int)]).expect("one field"),
        );
        let mut speller = Speller::new(&db);
        assert_eq!(speller.spell(squatter).as_deref(), Some("__p_rec0"));
        assert_eq!(speller.spell(anon).as_deref(), Some("__p_rec1"));
        compiles(&speller.declarations(), "__p_rec1");
    }

    /// …and `Some`/`None` must not select the prelude `Option` that way:
    /// re-declaring it in the module is a redeclaration error, and it is already
    /// in scope.
    #[test]
    fn the_prelude_option_is_never_declared_by_its_variants() {
        let db = TypeDb::new();
        let mut speller = Speller::new(&db);
        speller.declare_named(
            &["Some".to_string(), "None".to_string(), "Option".to_string()]
                .into_iter()
                .collect(),
        );
        assert_eq!(speller.declarations(), "");
    }

    /// Declaring by name and by spelling meet on the same def, and a def is
    /// declared once however it was reached.
    #[test]
    fn declaring_by_name_and_by_spelling_do_not_double_declare() {
        let mut db = TypeDb::new();
        let int = db.int();
        let pt = db.record(
            Some("Pt".to_string()),
            FieldSet::from_pairs(vec![("x".to_string(), int)]).expect("one field"),
        );
        let mut speller = Speller::new(&db);
        speller.spell(pt).expect("Pt spells");
        speller.declare_named(&["Pt".to_string()].into_iter().collect());
        let decls = speller.declarations();
        assert_eq!(decls.matches("struct Pt").count(), 1, "{decls}");
        compiles(&decls, "Pt");
    }

    /// `Option` is the prelude's own def and is in scope everywhere, so it is
    /// spelled and never declared — re-declaring it would be a redeclaration
    /// error, and declaring nothing would be an unknown type.
    #[test]
    fn option_is_spelled_but_not_declared() {
        let mut db = TypeDb::new();
        let int = db.int();
        let ty = db.option_of(int);
        let mut speller = Speller::new(&db);
        assert_eq!(speller.spell(ty).as_deref(), Some("Option[Int]"));
        assert_eq!(speller.declarations(), "");
        compiles("", "Option[Int]");
    }

    /// A type with no source syntax declines to be written rather than producing
    /// text the parser rejects. `?T` is the common one: an unfilled `Vec()` types
    /// as `Vec[?T]`.
    #[test]
    fn a_type_with_no_syntax_is_not_spelled() {
        let mut db = TypeDb::new();
        let var = db.fresh_var();
        let vec_of_var = vec_of(&mut db, var);
        let never = db.never();
        let byte = db.scalar(ScalarType::Byte);
        let uint = db.scalar(ScalarType::UInt);
        let int = db.int();
        let seq = db
            .collection(
                CollectionCtor::Seq,
                CollectionArgs::new(CollectionCtor::Seq, vec![int]).expect("Seq takes one arg"),
            )
            .expect("Seq[Int]");
        let mut speller = Speller::new(&db);
        for (ty, what) in [
            (var, "?T"),
            (vec_of_var, "Vec[?T]"),
            (never, "Never"),
            (byte, "Byte"),
            (uint, "UInt"),
            (seq, "Seq[Int]"),
        ] {
            assert_eq!(speller.spell(ty), None, "{what} has no source spelling");
        }
        assert_eq!(
            speller.declarations(),
            "",
            "a refused spelling leaves no half-built declaration behind"
        );
    }

    /// A record is only as writable as its least writable field, and the
    /// declaration it would have needed must not survive the refusal — a
    /// `struct Holder { v: ?T }` in the module is a parse error for the whole
    /// command, which is the failure being ruled out.
    #[test]
    fn a_record_with_an_unwritable_field_is_not_spelled() {
        let mut db = TypeDb::new();
        let var = db.fresh_var();
        let ty = db.record(
            Some("Holder".to_string()),
            FieldSet::from_pairs(vec![("v".to_string(), var)]).expect("one field"),
        );
        let mut speller = Speller::new(&db);
        assert_eq!(speller.spell(ty), None);
        assert_eq!(speller.declarations(), "");
    }

    /// `(P) -> R` round-trips, but a single *tuple* parameter does not: the
    /// annotation `((Int, Text)) -> Int` reads back as the two-parameter
    /// function, so it is refused rather than written to mean something else.
    #[test]
    fn function_types_spell_only_when_they_read_back_the_same() {
        let mut db = TypeDb::new();
        let int = db.int();
        let text = db.text();
        let unary = db.func(vec![int], text);
        let binary = db.func(vec![int, text], int);
        let nullary = db.func(Vec::new(), int);
        let tupled = db.tuple(TupleElems::pair(int, text));
        let of_tuple = db.func(vec![tupled], int);
        let mut speller = Speller::new(&db);
        assert_eq!(speller.spell(unary).as_deref(), Some("(Int) -> Text"));
        assert_eq!(speller.spell(binary).as_deref(), Some("(Int, Text) -> Int"));
        assert_eq!(speller.spell(nullary).as_deref(), Some("() -> Int"));
        assert_eq!(speller.spell(of_tuple), None);
        for annotation in ["(Int) -> Text", "(Int, Text) -> Int", "() -> Int"] {
            compiles("", annotation);
        }
    }
}
