//! The declaration pass and the sealed type environment (F19).
//!
//! Two things happen before a single expression is inferred:
//!
//! 1. **Type declarations.** Every top-level `struct` and `enum` is registered
//!    in the [`TypeDb`], in *dependency order* rather than source order, and its
//!    [`Type`] is recorded against the symbol the resolver minted for its name.
//! 2. **Function signatures.** Every top-level `fn` gets a monomorphic
//!    placeholder variable, so a call to one declared later unifies against the
//!    same variable that declaration will resolve (TY-22).
//!
//! Both land in a [`TypeEnv`] that is **sealed** — it has no mutator outside
//! this module — so "a type name the resolver accepted, with no `Type`" is not
//! a state expression inference can observe. It used to be the normal state:
//! `infer_top_stmt` ran in source order, so `fn bad(p: Point)` above
//! `struct Point { … }` read a symbol whose scheme was still `None`, silently
//! degraded the annotation to a fresh variable, and checked nothing (TY-10).
//!
//! Resolving an annotation to a [`Type`] also lives here, in [`Annotations`],
//! because the declaration pass and inference must do it the same way: a name
//! in type position resolves through the symbol the *resolver* bound it to
//! (`type_refs`), not through a scope lookup at the point of use. That is what
//! makes a user `enum` reachable at all — the old lookup asked for a
//! `SymbolKind::Struct` and answered `None` for everything else, so an `enum`
//! annotation resolved to a fresh variable and `lookup_enum_type` was dead
//! code (TY-09).

use std::collections::{HashMap, HashSet};

use praxis_ast::{AstNode, EnumItem, FnItem, SourceFile, StructItem, TypeRef};
use praxis_source::{BytePos, Diagnostic, FileId, FileSpan, Span};
use praxis_syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use praxis_types::{
    CollectionArgs, CollectionCtor, EnumVariantDef, FieldSet, ScalarType, Scheme, Type, TypeDb,
    VariantSet,
};
use rowan::TextRange;

use crate::name_table::NameTable;
use crate::symbol::SymbolId;

/// The type of every name that denotes a type, and the signature placeholder of
/// every function in the declaration group.
///
/// Sealed: [`declare`] is the only constructor and there is no way to add an
/// entry from outside this module, so inference reads a total environment or a
/// definite absence — never a half-built one.
#[derive(Debug, Default)]
pub(crate) struct TypeEnv {
    types: HashMap<SymbolId, Type>,
    signatures: HashMap<SymbolId, Type>,
}

impl TypeEnv {
    /// The type a `struct`/`enum` name denotes, if `symbol` is one.
    pub(crate) fn ty(&self, symbol: SymbolId) -> Option<Type> {
        self.types.get(&symbol).copied()
    }

    /// The signature placeholder of a function in this declaration group.
    pub(crate) fn signature(&self, symbol: SymbolId) -> Option<Type> {
        self.signatures.get(&symbol).copied()
    }
}

/// Run the declaration pass over `root` and return the sealed environment.
///
/// `decls` and `type_refs` come from name resolution: the first says which
/// symbol a declaration site minted, the second which symbol a name *in type
/// position* resolved to.
pub(crate) fn declare(
    file: FileId,
    root: &SourceFile,
    decls: &HashMap<TextRange, SymbolId>,
    type_refs: &HashMap<TextRange, SymbolId>,
    db: &mut TypeDb,
    names: &mut NameTable,
    diagnostics: &mut Vec<Diagnostic>,
) -> TypeEnv {
    let mut pass = Declare {
        file,
        db,
        names,
        decls,
        type_refs,
        diagnostics,
        env: TypeEnv::default(),
    };
    pass.declare_types(root);
    pass.declare_signatures(root);
    pass.env
}

/// One type declaration, paired with the symbol its name minted.
struct TypeDecl {
    symbol: SymbolId,
    item: TypeItem,
}

enum TypeItem {
    Struct(StructItem),
    Enum(EnumItem),
}

impl TypeItem {
    fn syntax(&self) -> &SyntaxNode {
        match self {
            TypeItem::Struct(s) => s.syntax(),
            TypeItem::Enum(e) => e.syntax(),
        }
    }
}

struct Declare<'a> {
    file: FileId,
    db: &'a mut TypeDb,
    names: &'a mut NameTable,
    decls: &'a HashMap<TextRange, SymbolId>,
    type_refs: &'a HashMap<TextRange, SymbolId>,
    diagnostics: &'a mut Vec<Diagnostic>,
    env: TypeEnv,
}

impl Declare<'_> {
    fn file_span(&self, range: TextRange) -> FileSpan {
        FileSpan::new(
            self.file,
            Span::new(
                BytePos::from(u32::from(range.start())),
                BytePos::from(u32::from(range.end())),
            ),
        )
    }

    fn annotations(&mut self) -> Annotations<'_> {
        Annotations {
            file: self.file,
            db: self.db,
            env: &self.env,
            type_refs: self.type_refs,
            diagnostics: self.diagnostics,
        }
    }

    /// Register every top-level type declaration, a declaration whose
    /// dependencies are all registered first.
    ///
    /// Source order is not dependency order and never was — resolution has been
    /// two-pass since M7 precisely so a `struct` name is visible above its
    /// declaration. Inference was not, so the *name* resolved and the *type*
    /// did not exist yet (TY-10).
    ///
    /// A declaration that never becomes ready is part of a cycle (`struct A { b: B }`
    /// / `struct B { a: A }`, or a self-reference). The language has no
    /// equirecursive types, so there is nothing better to do than register it
    /// with what is known — an unresolvable member becomes a fresh variable, as
    /// every unresolvable annotation does.
    fn declare_types(&mut self, root: &SourceFile) {
        let mut pending: Vec<TypeDecl> = root
            .stmts()
            .filter_map(|node| {
                let item = if let Some(s) = StructItem::cast(node.clone()) {
                    TypeItem::Struct(s)
                } else {
                    TypeItem::Enum(EnumItem::cast(node)?)
                };
                let name = match &item {
                    TypeItem::Struct(s) => s.name(),
                    TypeItem::Enum(e) => e.name(),
                }?;
                let symbol = self.decls.get(&name.text_range()).copied()?;
                Some(TypeDecl { symbol, item })
            })
            .collect();
        let mut undeclared: HashSet<SymbolId> = pending.iter().map(|d| d.symbol).collect();
        while let Some(i) = pending
            .iter()
            .position(|d| !mentions(d, &undeclared, self.type_refs))
        {
            let decl = pending.remove(i);
            undeclared.remove(&decl.symbol);
            self.declare_one(&decl);
        }
        for decl in pending {
            self.declare_one(&decl);
        }
    }

    fn declare_one(&mut self, decl: &TypeDecl) {
        match &decl.item {
            TypeItem::Struct(s) => self.declare_struct(decl.symbol, s),
            TypeItem::Enum(e) => self.declare_enum(decl.symbol, e),
        }
    }

    /// Register a `struct` declaration's type (M7, §4.5): resolve each field's
    /// annotation, build the `RecordDef`, and record the resulting `Type`.
    fn declare_struct(&mut self, symbol: SymbolId, item: &StructItem) {
        let Some(name_tok) = item.name() else { return };
        let range = name_tok.text_range();
        let mut fields = Vec::new();
        if let Some(fl) = item.field_list() {
            for f in fl.fields() {
                let fname = f.name().map(|t| t.text().to_string()).unwrap_or_default();
                let fty = self.resolve_or_fresh(f.ty().as_ref());
                fields.push((fname, fty));
            }
        }
        let fields = match FieldSet::from_pairs(fields) {
            Ok(fields) => fields,
            Err(praxis_types::TypeCtorError::DuplicateField(dup)) => {
                self.diagnostics.push(crate::diagnostics::duplicate_member(
                    self.file_span(range),
                    "field",
                    &dup,
                ));
                return;
            }
            Err(_) => return,
        };
        let name = name_tok.text().to_string();
        let ty = self.db.record(Some(name), fields);
        self.bind_type(symbol, ty);
    }

    /// Register an `enum` declaration's type (M7, §4.6) and give each variant
    /// constructor a type: `(payload…) -> Enum`, or the enum type itself for a
    /// payload-less variant (which is used as a path, not a call).
    fn declare_enum(&mut self, symbol: SymbolId, item: &EnumItem) {
        let Some(name_tok) = item.name() else { return };
        let range = name_tok.text_range();
        let mut variants = Vec::new();
        // (variant name, payload types, declaration range) for the constructors.
        let mut variant_info: Vec<(Vec<Type>, TextRange)> = Vec::new();
        for v in item.variants() {
            let vname = v.name().map(|t| t.text().to_string()).unwrap_or_default();
            let payload: Vec<Type> = v
                .payload_types()
                .unwrap_or_default()
                .iter()
                .map(|t| self.resolve_or_fresh(Some(t)))
                .collect();
            // TY-05: one payload representation. The `is_empty` normalization
            // this replaces was the manual half of the same equivalence.
            variants.push(EnumVariantDef::new(vname, payload.clone()));
            if let Some(vtok) = v.name() {
                variant_info.push((payload, vtok.text_range()));
            }
        }
        let variants = match VariantSet::new(variants) {
            Ok(variants) => variants,
            Err(praxis_types::TypeCtorError::DuplicateVariant(dup)) => {
                self.diagnostics.push(crate::diagnostics::duplicate_member(
                    self.file_span(range),
                    "variant",
                    &dup,
                ));
                return;
            }
            Err(_) => return,
        };
        let name = name_tok.text().to_string();
        let enum_ty = self.db.enum_(Some(name), variants);
        self.bind_type(symbol, enum_ty);
        for (payload, vrange) in &variant_info {
            let Some(vsymbol) = self.decls.get(vrange).copied() else {
                continue;
            };
            let ctor_ty = if payload.is_empty() {
                enum_ty
            } else {
                self.db.func(payload.clone(), enum_ty)
            };
            if let Some(sym) = self.names.get_mut(vsymbol) {
                sym.scheme = Some(Scheme::monotype(ctor_ty));
            }
        }
    }

    /// Record `ty` as what `symbol` denotes, in the environment *and* on the
    /// symbol itself (which is what hover and the record-literal path read).
    fn bind_type(&mut self, symbol: SymbolId, ty: Type) {
        self.env.types.insert(symbol, ty);
        if let Some(sym) = self.names.get_mut(symbol) {
            sym.scheme = Some(Scheme::monotype(ty));
        }
    }

    fn resolve_or_fresh(&mut self, ty: Option<&TypeRef>) -> Type {
        match ty.and_then(|t| self.annotations().resolve(t)) {
            Some(t) => t,
            None => self.db.fresh_var(),
        }
    }

    /// Mint the signature placeholder of every top-level `fn` (TY-22).
    ///
    /// The placeholder is a plain variable, minted at whatever level the caller
    /// has open — which must be the group's body level, not the level the group
    /// binds at (TY-01): unifying it with the derived function type lowers that
    /// type's variables to the placeholder's level, so a placeholder one level
    /// out would clamp every parameter and result and no signature could ever
    /// generalize.
    fn declare_signatures(&mut self, root: &SourceFile) {
        for stmt in root.stmts() {
            let Some(item) = FnItem::cast(stmt) else {
                continue;
            };
            let Some(name_tok) = item.name() else {
                continue;
            };
            // A `fn` with no `decls` entry is one resolution refused: a nested
            // function or the second of a duplicate pair. Both are already
            // reported (N005 / N004) and neither gets a signature.
            let Some(symbol) = self.decls.get(&name_tok.text_range()).copied() else {
                continue;
            };
            let placeholder = self.db.fresh_var();
            self.env.signatures.insert(symbol, placeholder);
            if let Some(sym) = self.names.get_mut(symbol) {
                sym.scheme = Some(Scheme::monotype(placeholder));
            }
        }
    }
}

/// Whether any type name written inside `decl` resolves to a symbol that has
/// not been registered yet.
fn mentions(
    decl: &TypeDecl,
    undeclared: &HashSet<SymbolId>,
    type_refs: &HashMap<TextRange, SymbolId>,
) -> bool {
    decl.item
        .syntax()
        .descendants_with_tokens()
        .filter_map(|e| match e {
            rowan::NodeOrToken::Token(t) if t.kind() == SyntaxKind::Ident => Some(t.text_range()),
            _ => None,
        })
        // Only annotation tokens are in `type_refs`, so the declaration's own
        // name, its field names and its variant names never match.
        .filter_map(|range| type_refs.get(&range))
        .any(|symbol| undeclared.contains(symbol))
}

// ---------------------------------------------------------------------------
// Annotations
// ---------------------------------------------------------------------------

/// Resolving a written type annotation to a [`Type`].
///
/// Borrows exactly what that needs, so the declaration pass (still building the
/// environment) and inference (reading a sealed one) resolve annotations
/// through the same code rather than two near-duplicates.
pub(crate) struct Annotations<'a> {
    file: FileId,
    db: &'a mut TypeDb,
    env: &'a TypeEnv,
    type_refs: &'a HashMap<TextRange, SymbolId>,
    diagnostics: &'a mut Vec<Diagnostic>,
}

impl<'a> Annotations<'a> {
    pub(crate) fn new(
        file: FileId,
        db: &'a mut TypeDb,
        env: &'a TypeEnv,
        type_refs: &'a HashMap<TextRange, SymbolId>,
        diagnostics: &'a mut Vec<Diagnostic>,
    ) -> Annotations<'a> {
        Annotations {
            file,
            db,
            env,
            type_refs,
            diagnostics,
        }
    }

    fn file_span(&self, range: TextRange) -> FileSpan {
        FileSpan::new(
            self.file,
            Span::new(
                BytePos::from(u32::from(range.start())),
                BytePos::from(u32::from(range.end())),
            ),
        )
    }

    /// Resolve a written annotation. `None` means the annotation named
    /// something with no type — already reported by resolution as `N002`/`N003`,
    /// or by this pass as `Y007` — and the caller falls back to inference.
    pub(crate) fn resolve(&mut self, ty: &TypeRef) -> Option<Type> {
        // The wrapper accepts all three annotation node kinds (TY-08), so its
        // node is already the thing to dispatch on.
        self.resolve_node(ty.syntax())
    }

    /// Resolve one annotation node. Total over the three node kinds
    /// [`SyntaxKind::is_type_node`] admits; anything else is `None`.
    fn resolve_node(&mut self, node: &SyntaxNode) -> Option<Type> {
        match node.kind() {
            SyntaxKind::TYPE_REF => self.resolve_named_or_grouped(node),
            SyntaxKind::TUPLE_TYPE => {
                let els = self.resolve_children(node);
                Some(tuple_or_degenerate(self.db, els))
            }
            SyntaxKind::FN_TYPE => {
                // An FN_TYPE node has a param-type group (a TYPE_REF group or a
                // TUPLE_TYPE) and a result type, separated by `->`.
                let mut parts = self.resolve_children(node);
                if parts.len() >= 2 {
                    let result = parts.pop().expect(">=2 elements");
                    let params = self.flatten_param_group(parts.pop().expect(">=2 elements"));
                    Some(self.db.func(params, result))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// The three shapes that wear the `TYPE_REF` kind, told apart by what the
    /// node holds rather than by where it was found:
    ///
    /// - `Int` — a bare name: an `Ident` token of this node itself.
    /// - `Vec[Int]` — a collection: no `Ident` of its own; the *first* type-node
    ///   child holds the constructor name and the rest are its arguments (the
    ///   parser emits the name as its own `TYPE_REF`, then reopens at a
    ///   checkpoint to wrap the brackets).
    /// - `(T)` — a parenthesized group: exactly one type-node child and no name.
    ///   `()` is the degenerate case and is [`Unit`](praxis_types::TypeData::Unit),
    ///   which is what makes `() -> Int` a nullary function type rather than one
    ///   taking an invented variable.
    fn resolve_named_or_grouped(&mut self, node: &SyntaxNode) -> Option<Type> {
        if let Some(name) = direct_ident(node) {
            return self.named_type(&name);
        }
        let children: Vec<_> = node
            .children()
            .filter(|c| c.kind().is_type_node())
            .collect();
        match children.split_first() {
            None => Some(self.db.unit()),
            Some((only, [])) => self.resolve_node(only),
            Some((ctor, args)) => {
                let name = direct_ident(ctor)?;
                let type_args: Vec<Type> = args
                    .iter()
                    .map(|c| self.resolve_node(c).unwrap_or_else(|| self.db.fresh_var()))
                    .collect();
                self.collection_from_name(name.text(), type_args, node.text_range())
            }
        }
    }

    /// Resolve every type-node child of `node`, in order. An unresolvable child
    /// becomes a fresh variable so one bad element does not discard the shape.
    fn resolve_children(&mut self, node: &SyntaxNode) -> Vec<Type> {
        node.children()
            .filter(|c| c.kind().is_type_node())
            .collect::<Vec<_>>()
            .iter()
            .map(|c| self.resolve_node(c).unwrap_or_else(|| self.db.fresh_var()))
            .collect()
    }

    /// Given a type that represents a parameter group, return the parameter
    /// types. `(A, B)` (a tuple type) flattens to `[A, B]`, `()` to no
    /// parameters at all, and anything else stays `[itself]`.
    fn flatten_param_group(&mut self, ty: Type) -> Vec<Type> {
        let rep = self.db.follow(ty);
        match self.db.data(rep) {
            praxis_types::TypeData::Tuple(els) => els.clone(),
            praxis_types::TypeData::Unit => Vec::new(),
            // A single param: the type itself, not a re-interned copy of its
            // shape. `intern` is `pub(crate)` since F5, and this was the one
            // site outside the arena that needed it — for no reason, since the
            // representative handle was already in hand.
            _ => vec![rep],
        }
    }

    /// A bare name in type position: a built-in scalar, a **nullary collection**
    /// (`BitSet`, `Range`), or a user `struct`/`enum`.
    ///
    /// The user case reads the symbol the *resolver* bound the name to. A scope
    /// lookup here would be a second, weaker answer to a question resolution
    /// already answered — and the one it gave was wrong for an `enum`, which it
    /// simply did not look for (TY-09).
    ///
    /// The nullary-collection case is TY-34's: a name with no brackets never
    /// reached [`collection_from_name`], so `fn f(r: Range)` and
    /// `fn f(b: BitSet)` resolved to *nothing* and the parameter silently became
    /// a fresh variable — which then unified with whatever the body did to it.
    /// A first-class range you cannot annotate is half a value (D6), and `BitSet`
    /// had the same hole. Routing a bare name through the same door also makes a
    /// bracket-less `Vec` a `Y007` ("expected 1 type argument, got 0") instead of
    /// a silent variable, which is exactly what that code says.
    fn named_type(&mut self, name: &SyntaxToken) -> Option<Type> {
        let scalar = match name.text() {
            "Int" => ScalarType::Int,
            "Text" => ScalarType::Text,
            "Bool" => ScalarType::Bool,
            "Char" => ScalarType::Char,
            "Float" => ScalarType::Float,
            "Unit" => return Some(self.db.unit()),
            "Never" => return Some(self.db.never()),
            text if is_type_ctor_name(text) => {
                return self.collection_from_name(text, Vec::new(), name.text_range());
            }
            _ => {
                let symbol = self.type_refs.get(&name.text_range()).copied()?;
                return self.env.ty(symbol);
            }
        };
        Some(self.db.scalar(scalar))
    }

    /// Resolve a collection type name + args to a [`Type`] (§4.4, §11.2). Every
    /// §6.1 ctor resolves to its [`CollectionCtor`]; `Seq` is compiler-internal
    /// (§6.3) and never user-named, so `Seq[T]` in source surfaces as an unknown
    /// type.
    fn collection_from_name(
        &mut self,
        name: &str,
        args: Vec<Type>,
        range: TextRange,
    ) -> Option<Type> {
        // `Option[T]` (M9): an application of the prelude's *one* `Option` def
        // (F12). It used to register a fresh def per annotation site, so the
        // annotation and the `Some`/`None` value it described were two nominal
        // types that only a relaxed unification arm put back together (TY-06).
        if name == "Option" {
            let want = 1;
            let got = args.len();
            if got > want {
                self.diagnostics
                    .push(crate::diagnostics::wrong_type_argument_count(
                        self.file_span(range),
                        "Option",
                        got,
                        want,
                    ));
                return None;
            }
            let elem = args
                .into_iter()
                .next()
                .unwrap_or_else(|| self.db.fresh_var());
            return Some(self.db.option_of(elem));
        }
        let ctor = collection_ctor_for(name)?;
        // The ctor declares how many type args it takes, and F5 is what finally
        // consults it (TY-07). A wrong arity used to intern a type nothing could
        // unify with, so the user saw a `Y001` about a type they never wrote.
        let want = ctor.arity();
        let got = args.len();
        match CollectionArgs::new(ctor, args) {
            Ok(args) => self.db.collection(ctor, args).ok(),
            Err(_) => {
                self.diagnostics
                    .push(crate::diagnostics::wrong_type_argument_count(
                        self.file_span(range),
                        ctor.name(),
                        got,
                        want,
                    ));
                None
            }
        }
    }
}

/// The `Ident` token belonging to `node` **itself**, not to a descendant. A
/// type node names a type iff it has one: `Int` does, the group `(Int)` does
/// not (its `Ident` sits inside a nested `TYPE_REF`), and neither does the
/// bracket-wrapping `TYPE_REF` of `Vec[Int]`.
fn direct_ident(node: &SyntaxNode) -> Option<SyntaxToken> {
    node.children_with_tokens().find_map(|e| match e {
        rowan::NodeOrToken::Token(t) if t.kind() == SyntaxKind::Ident => Some(t),
        _ => None,
    })
}

/// A tuple type from `els`, honouring the arity invariant (F5).
///
/// The parser and the tuple-expression path can both hand over fewer than two
/// elements, and `TupleElems` refuses to represent either degenerate case as a
/// tuple — correctly, because neither *is* one: `()` is `Unit`, and a lone
/// parenthesized element is that element. Interning a one-element `Tuple` was
/// how the old `db.tuple(els)` produced a type that could never unify with
/// anything.
pub(crate) fn tuple_or_degenerate(db: &mut TypeDb, mut els: Vec<Type>) -> Type {
    match els.len() {
        0 => db.unit(),
        1 => els.remove(0),
        _ => {
            let elems = praxis_types::TupleElems::new(els).expect("two or more elements");
            db.tuple(elems)
        }
    }
}

/// Resolve a collection ctor name (e.g. `"Deque"`) to its [`CollectionCtor`].
/// `Seq` is compiler-internal (§6.3) and deliberately absent.
pub(crate) fn collection_ctor_for(name: &str) -> Option<CollectionCtor> {
    Some(match name {
        "Vec" => CollectionCtor::Vec,
        "Deque" => CollectionCtor::Deque,
        "Map" => CollectionCtor::Map,
        "Set" => CollectionCtor::Set,
        "Counter" => CollectionCtor::Counter,
        "MinHeap" => CollectionCtor::MinHeap,
        "MaxHeap" => CollectionCtor::MaxHeap,
        "BitSet" => CollectionCtor::BitSet,
        "Grid" => CollectionCtor::Grid,
        "Range" => CollectionCtor::Range,
        _ => return None,
    })
}

/// Whether `name` is a compiler-owned type constructor — a §6.1 collection or
/// `Option`. These are legal type-annotation names that are *not* scope
/// symbols, so name resolution accepts them without a lookup and
/// [`Annotations`] turns them into types.
pub(crate) fn is_type_ctor_name(name: &str) -> bool {
    name == "Option" || collection_ctor_for(name).is_some()
}
