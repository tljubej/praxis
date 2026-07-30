//! Monomorphization (WS8, §13.6, ADR-018).
//!
//! Inferred polymorphic functions are instantiated for concrete use sites. The
//! pass runs *between* typed-HIR lowering and MIR building: it consumes a
//! [`TypedModule`] (produced by [`crate::lower`]) plus the [`crate::Analysis`]
//! call-site witnesses, and produces a new `TypedModule` in which every generic
//! `fn` is replaced by one monomorphic clone per concrete call site, and every
//! `TypedExpr::Call` to a generic callee is rewritten to target the clone's
//! mangled name.
//!
//! ## Algorithm
//!
//! 1. Collect every `(callee_symbol, concrete_arg_types)` call site from the
//!    typed tree whose callee scheme `is_polymorphic()`.
//! 2. For each, build a canonical key (the callee symbol + the canonicalized arg
//!    types) and cache the mangled clone name. Repeated call sites with the same
//!    canonical types share one clone.
//! 3. To specialize: instantiate the callee scheme, unify its param types with
//!    the concrete arg types (pinning the quantified vars), then clone the
//!    callee `TypedFn` and substitute every `Type` in it by its resolved form.
//!    Rename the clone to a mangled name.
//! 4. Transitive: re-scan each clone's body for further polymorphic calls;
//!    repeat to fixpoint.
//! 5. Drop the original generic `fn`s from the output (only clones survive).
//!    Monomorphic `fn`s pass through unchanged.
//!
//! `lower_module` then runs unchanged on the expanded module (it is a clean 1:1
//! map of `TypedItem::Fn → Function`).

use std::collections::{HashMap, HashSet};

use praxis_types::{Scheme, Type, TypeDb, TypeKey, VarId};

use crate::name_table::NameTable;
use crate::symbol::SymbolId;
use crate::{TypedBlock, TypedExpr, TypedFn, TypedItem, TypedModule, TypedStmt};

/// Monomorphize a typed module: instantiate every polymorphic callee per call
/// site, rewrite the call sites to target the clones, and drop the original
/// generic fns. Returns the expanded module.
///
/// `names` is the symbol table (read-only here); `db` is mutated because
/// instantiating/unifying schemes allocates fresh type slots.
#[must_use]
pub fn monomorphize(module: TypedModule, names: &NameTable, db: &mut TypeDb) -> TypedModule {
    let mut pass = MonoPass {
        db,
        names,
        // symbol → the original (generic or monomorphic) TypedFn, for cloning.
        originals: module
            .items
            .iter()
            .map(|i| match i {
                TypedItem::Fn(f) => (f.symbol, f.clone()),
            })
            .collect(),
        // (symbol, canonical arg types) → mangled clone name.
        cache: HashMap::new(),
        used_names: HashSet::new(),
        // The clones emitted so far (appended after the monomorphic originals).
        clones: Vec::new(),
        // symbol → is the original polymorphic? (to drop it from output)
        polymorphic: HashMap::new(),
    };
    // Pre-record which originals are polymorphic so we can drop them.
    for sym in pass.originals.keys() {
        let is_poly = scheme_of(names, *sym).is_some_and(|s| s.is_polymorphic());
        pass.polymorphic.insert(*sym, is_poly);
    }
    // Walk every fn body, instantiating polymorphic callees. This populates
    // `clones` and rewrites call sites in-place.
    let mut items: Vec<TypedItem> = module
        .items
        .into_iter()
        .filter_map(|item| match item {
            TypedItem::Fn(ref f) => {
                // Drop polymorphic originals (only clones survive); keep
                // monomorphic ones, with their call sites rewritten.
                if pass.polymorphic.get(&f.symbol).copied().unwrap_or(false) {
                    return None;
                }
                let mut clone = f.clone();
                pass.rewrite_body(&mut clone);
                Some(TypedItem::Fn(clone))
            }
        })
        .collect();
    items.append(&mut pass.clones);
    TypedModule {
        items,
        diagnostics: module.diagnostics,
        escaping_vars: module.escaping_vars,
    }
}

struct MonoPass<'a> {
    db: &'a mut TypeDb,
    names: &'a NameTable,
    originals: HashMap<SymbolId, TypedFn>,
    /// Cache key: (callee symbol, the arg types' [`TypeKey`]s). Structural (not
    /// type-id) so two call sites with the same concrete type share a clone even
    /// when inference gave them distinct arena slots.
    ///
    /// **MONO-03**: this used to be the *rendered* type, which is display and
    /// not identity. `Option` printed as a bare name whatever it held, so
    /// `id(Some(1))` and `id(Some("a"))` hashed to one key and the second call
    /// silently reused the first's `Int` specialization.
    ///
    /// **MONO-02**: the key carries the call's *result* as well as its
    /// arguments. A callee whose quantified variable appears only in its result
    /// — `fn empty() { Vec() }` is `forall T. () -> Vec[T]` — has no argument to
    /// tell two instantiations apart, and used not to be specialized at all.
    cache: HashMap<(SymbolId, Vec<TypeKey>, TypeKey), String>,
    /// The mangled names handed out so far. The name is still built from the
    /// rendered types (it has to be readable), so two distinct keys that render
    /// alike must be pulled apart here rather than in the cache.
    used_names: HashSet<String>,
    clones: Vec<TypedItem>,
    polymorphic: HashMap<SymbolId, bool>,
}

impl<'a> MonoPass<'a> {
    /// Rewrite every `TypedExpr::Call` in a fn body: if the callee is
    /// polymorphic, instantiate a clone (or reuse a cached one) and retarget
    /// the call to the clone's mangled name.
    fn rewrite_body(&mut self, fn_: &mut TypedFn) {
        rewrite_block(&mut fn_.body, self);
    }

    /// Instantiate `callee` at the given concrete `arg_types` and `result`,
    /// returning the mangled clone name (cached). Emits the clone into
    /// `self.clones` on first use; rewrites the clone's own body (transitive
    /// instantiation).
    fn instantiate(&mut self, callee: SymbolId, arg_types: &[Type], result: Type) -> String {
        let key = (
            callee,
            canonical_keys(self.db, arg_types),
            self.db.canonical_key(result),
        );
        if let Some(name) = self.cache.get(&key) {
            return name.clone();
        }
        let name = self.fresh_mangled_name(callee, arg_types, result);
        self.cache.insert(key, name.clone());
        // Clone the original and specialize it. Borrow dance: take the original
        // out of the map temporarily so we can mutate `self` (db) while cloning.
        let original = match self.originals.get(&callee).cloned() {
            Some(o) => o,
            None => return name, // no original (shouldn't happen); bail.
        };
        let Some(scheme) = scheme_of(self.names, callee) else {
            return name;
        };
        let mut clone = specialize(self.db, &original, &scheme, arg_types, result, &name);
        // Rewrite the clone's body for any *further* polymorphic calls it makes
        // (transitive instantiation). This may append more clones.
        rewrite_block(&mut clone.body, self);
        self.clones.push(TypedItem::Fn(clone));
        name
    }

    /// A mangled clone name that no other specialization has taken.
    ///
    /// The readable form comes from the rendered argument types; if two
    /// specializations render the same — which the cache key no longer lets
    /// pass for one specialization — the second gets a numeric suffix rather
    /// than the first one's symbol.
    fn fresh_mangled_name(&mut self, callee: SymbolId, arg_types: &[Type], result: Type) -> String {
        // A zero-argument generic callee has nothing to render but its result;
        // naming every instantiation of `empty()` `empty__mono` would collide
        // them into one readable name (the numeric suffix would then pull them
        // apart, but `empty__Vec_Int_` says which is which).
        let rendered: Vec<String> = if arg_types.is_empty() {
            vec![self.db.render(self.db.follow(result))]
        } else {
            arg_types
                .iter()
                .map(|t| self.db.render(self.db.follow(*t)))
                .collect()
        };
        let base = mangled_name(callee, &rendered, self.names);
        let mut name = base.clone();
        let mut n = 1;
        while !self.used_names.insert(name.clone()) {
            name = format!("{base}__{n}");
            n += 1;
        }
        name
    }
}

/// The cache key's type half: each argument's structural identity, so two call
/// sites with the same concrete type (e.g. two `Int` args with distinct arena
/// slots from inference) share one monomorphized clone — and two with different
/// concrete types never do.
fn canonical_keys(db: &TypeDb, types: &[Type]) -> Vec<TypeKey> {
    types.iter().map(|t| db.canonical_key(*t)).collect()
}

/// Build a mangled clone name from the callee symbol and its canonical
/// (rendered) arg types. Sanitizes the rendered type so the name is a valid
/// symbol (e.g. `id__Int`, `fst__Int_Int`).
fn mangled_name(callee: SymbolId, canon: &[String], names: &NameTable) -> String {
    let base = names
        .get(callee)
        .map(|s| s.name.clone())
        .unwrap_or_else(|| format!("fn{}", callee.to_u32()));
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let suffix = canon
        .iter()
        .map(|s| sanitize(s))
        .collect::<Vec<_>>()
        .join("_");
    if suffix.is_empty() {
        format!("{base}__mono")
    } else {
        format!("{base}__{suffix}")
    }
}

/// Rewrite every call in a block: retarget polymorphic callees to their clones.
fn rewrite_block(block: &mut TypedBlock, pass: &mut MonoPass<'_>) {
    for stmt in &mut block.stmts {
        rewrite_stmt(stmt, pass);
    }
    rewrite_expr(&mut block.tail, pass);
}

fn rewrite_stmt(stmt: &mut TypedStmt, pass: &mut MonoPass<'_>) {
    match stmt {
        TypedStmt::Let { init, .. } | TypedStmt::Var { init, .. } => rewrite_expr(init, pass),
        TypedStmt::Assign { value, .. } => rewrite_expr(value, pass),
        TypedStmt::Expr(e) => rewrite_expr(e, pass),
    }
}

fn rewrite_expr(e: &mut TypedExpr, pass: &mut MonoPass<'_>) {
    // The one thing this pass does: retarget a polymorphic callee to its clone.
    if let TypedExpr::Call {
        callee,
        callee_name,
        arg_types,
        ty,
        ..
    } = e
    {
        // Is this callee a polymorphic user fn? (Closure-value callees have no
        // scheme / are not in originals.)
        let is_poly = scheme_of(pass.names, *callee).is_some_and(|s| s.is_polymorphic())
            && pass.originals.contains_key(callee);
        if is_poly {
            // An empty argument list is still a real generic call site: the
            // guard that skipped it dropped `fn empty() { Vec() }`'s original
            // without ever emitting a clone, so the call had no target at all
            // (MONO-02). The call's own type is what pins such a callee.
            let mangled = pass.instantiate(*callee, arg_types, *ty);
            *callee_name = mangled;
        }
    }
    // …everywhere else, recurse. The child list is F20's, written once: this
    // used to be its own 29-arm match, and a field missing from it was a call
    // that never got retargeted.
    for child in e.children_mut() {
        rewrite_expr(child, pass);
    }
    for block in e.blocks_mut() {
        rewrite_block(block, pass);
    }
}

/// Specialize `original` (a generic fn) for the concrete `arg_types` and
/// `result`, and rename the clone to `name`.
///
/// **MONO-01.** This used to instantiate the scheme, unify *that* copy's params
/// with the argument types, and then `follow` every type in the clone. The copy
/// and the clone shared no variables: the fresh instantiation's variables were
/// the ones unification pinned, and the clone's were the ones the typed tree
/// carried — so `follow` found them exactly as unbound as it left them and the
/// "specialized" clone was the generic original with a new name.
///
/// The two are one set now. Lowering writes the scheme's own variables into the
/// typed tree (see `lower_fn`), so what a use site chooses for each **binder**
/// is a substitution the clone can be rewritten by. `instantiate_with_mapping`
/// is what says which variable stands for which binder; unifying that copy
/// against the call site is what decides them.
fn specialize(
    db: &mut TypeDb,
    original: &TypedFn,
    scheme: &Scheme,
    arg_types: &[Type],
    result: Type,
    name: &str,
) -> TypedFn {
    // One fresh variable per binder, in binder order. Unifying this copy against
    // the call site pins each of them.
    let (instantiated, mapping) = db.instantiate_with_mapping(scheme);
    let (param_types, result_ty) = func_shape(db, instantiated);
    for (pt, at) in param_types.iter().zip(arg_types.iter()) {
        let _ = db.unify(*pt, *at);
    }
    // The result is a witness too, and for a zero-argument generic callee it is
    // the only one (MONO-02).
    let _ = db.unify(result_ty, result);
    let binders: Vec<VarId> = scheme.binders().to_vec();
    let args: Vec<Type> = mapping.iter().map(|m| db.deep_resolve(*m)).collect();

    let mut clone = original.clone();
    clone.name = name.to_string();
    clone.fn_type = specialize_type(db, &binders, &args, clone.fn_type);
    clone.return_type = specialize_type(db, &binders, &args, clone.return_type);
    for p in &mut clone.params {
        p.ty = specialize_type(db, &binders, &args, p.ty);
    }
    resolve_block(db, &binders, &args, &mut clone.body);
    clone
}

/// Walk a typed block, rewriting every `Type` in it by the specialization.
fn resolve_block(db: &mut TypeDb, binders: &[VarId], args: &[Type], block: &mut TypedBlock) {
    for stmt in &mut block.stmts {
        resolve_stmt(db, binders, args, stmt);
    }
    resolve_expr(db, binders, args, &mut block.tail);
    block.ty = specialize_type(db, binders, args, block.ty);
}

fn resolve_stmt(db: &mut TypeDb, binders: &[VarId], args: &[Type], stmt: &mut TypedStmt) {
    match stmt {
        TypedStmt::Let { ty, init, .. } | TypedStmt::Var { ty, init, .. } => {
            *ty = specialize_type(db, binders, args, *ty);
            resolve_expr(db, binders, args, init);
        }
        TypedStmt::Assign { value, .. } => resolve_expr(db, binders, args, value),
        TypedStmt::Expr(e) => resolve_expr(db, binders, args, e),
    }
}

fn resolve_expr(db: &mut TypeDb, binders: &[VarId], args: &[Type], e: &mut TypedExpr) {
    // The types this expression carries *itself*. Exhaustive on purpose: a new
    // variant, or a new type-bearing field on an existing one, is a compile
    // error here rather than a slot the specialization silently skips.
    match e {
        TypedExpr::Lit { ty, .. }
        | TypedExpr::Path { ty, .. }
        // A function value carries only its own `Func` type; the function it
        // names is specialized (or not) by its own call sites, and a *generic*
        // one used as a value has no call site to specialize from — see
        // `a_generic_fn_used_as_a_value_is_reported_rather_than_run`.
        | TypedExpr::FnValue { ty, .. }
        | TypedExpr::Bin { ty, .. }
        | TypedExpr::Range { ty, .. }
        | TypedExpr::Unary { ty, .. }
        | TypedExpr::Paren { ty, .. }
        | TypedExpr::If { ty, .. }
        | TypedExpr::While { ty, .. }
        | TypedExpr::Loop { ty, .. }
        | TypedExpr::Break { ty, .. }
        | TypedExpr::Continue { ty, .. }
        | TypedExpr::Return { ty, .. }
        | TypedExpr::MethodCall { ty, .. }
        | TypedExpr::Tuple { ty, .. }
        | TypedExpr::Read { ty, .. }
        | TypedExpr::Parse { ty, .. }
        | TypedExpr::RecordLit { ty, .. }
        | TypedExpr::FieldGet { ty, .. }
        | TypedExpr::TupleIndex { ty, .. }
        | TypedExpr::EnumVariant { ty, .. } => *ty = specialize_type(db, binders, args, *ty),
        TypedExpr::For { ty, item_ty, .. } => {
            *ty = specialize_type(db, binders, args, *ty);
            *item_ty = specialize_type(db, binders, args, *item_ty);
        }
        TypedExpr::Call { ty, arg_types, .. } => {
            *ty = specialize_type(db, binders, args, *ty);
            for at in arg_types {
                *at = specialize_type(db, binders, args, *at);
            }
        }
        TypedExpr::Match { ty, arms, .. } => {
            *ty = specialize_type(db, binders, args, *ty);
            for arm in arms {
                resolve_pattern(db, binders, args, &mut arm.pattern);
            }
        }
        TypedExpr::Closure {
            params,
            fn_type,
            ty,
            captures,
            ..
        } => {
            for p in params {
                p.ty = specialize_type(db, binders, args, p.ty);
            }
            *fn_type = specialize_type(db, binders, args, *fn_type);
            *ty = specialize_type(db, binders, args, *ty);
            for c in captures {
                c.ty = specialize_type(db, binders, args, c.ty);
            }
        }
        // A block carries its type on the `TypedBlock` below, which the block
        // walk reaches.
        TypedExpr::Block(_) => {}
    }
    // The recursion is F20's child walker, written once. This used to be its own
    // 29-arm match — the second of the three the audit found, each independently
    // forgettable and each already having forgotten something.
    for child in e.children_mut() {
        resolve_expr(db, binders, args, child);
    }
    for block in e.blocks_mut() {
        resolve_block(db, binders, args, block);
    }
}

/// A pattern's types, rewritten by the specialization. A generic function that
/// matches on a payload binds it at the def's type parameter, so the binding's
/// type is one of the variables the clone is substituting.
fn resolve_pattern(
    db: &mut TypeDb,
    binders: &[VarId],
    args: &[Type],
    pat: &mut crate::TypedPattern,
) {
    use crate::TypedPattern as P;
    match pat {
        P::Wildcard => {}
        P::Lit { ty, .. } | P::Bind { ty, .. } => *ty = specialize_type(db, binders, args, *ty),
        P::EnumVariant {
            ty, subpatterns, ..
        } => {
            *ty = specialize_type(db, binders, args, *ty);
            for sub in subpatterns {
                resolve_pattern(db, binders, args, sub);
            }
        }
    }
}

/// `t` under the specialization: each of the scheme's `binders` replaced by the
/// type the use site chose for it.
///
/// A clone with no binders is a monomorphic original passing through; it still
/// gets resolved, because a type inference left as a link is one the backend
/// would have to follow itself.
fn specialize_type(db: &mut TypeDb, binders: &[VarId], args: &[Type], t: Type) -> Type {
    if binders.is_empty() {
        db.deep_resolve(t)
    } else {
        db.substitute_params(t, binders, args)
    }
}

/// The (param_types, result) of a Func type, or (empty, t) if not a Func.
fn func_shape(db: &TypeDb, t: Type) -> (Vec<Type>, Type) {
    match db.data(db.follow(t)) {
        praxis_types::TypeData::Func { params, result } => (params.clone(), *result),
        _ => (Vec::new(), t),
    }
}

/// Look up a symbol's scheme.
fn scheme_of(names: &NameTable, sym: SymbolId) -> Option<Scheme> {
    names.get(sym).and_then(|s| s.scheme.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_ast::AstNode;
    use praxis_parser::parse;
    use praxis_source::SourceMap;

    use crate::{analyze_root, lower};

    /// Run the full front-end + mono on `src`, returning the monomorphized
    /// module's item names (in order).
    fn mono_names(src: &str) -> Vec<String> {
        let map = SourceMap::new();
        let id = map.intern("mono_test.px", src);
        let parsed = parse(id, src);
        let mut analysis = analyze_root(id, &parsed.tree);
        let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
        let module = lower(id, &root, &mut analysis);
        assert!(
            module.diagnostics.is_empty(),
            "lowering diagnostics: {:?}",
            module.diagnostics
        );
        let mono = monomorphize(module, &analysis.names, &mut analysis.db);
        mono.items
            .iter()
            .map(|i| match i {
                TypedItem::Fn(f) => f.name.clone(),
            })
            .collect()
    }

    #[test]
    fn monomorphic_fn_passes_through() {
        // No generics: the module is unchanged (just `main`).
        let names = mono_names("fn inc(n: Int) -> Int { n + 1 }\nfn main() -> Int { inc(41) }");
        assert!(names.contains(&"inc".to_string()), "got {names:?}");
        assert!(names.contains(&"main".to_string()), "got {names:?}");
    }

    #[test]
    fn generic_fn_is_instantiated_and_original_dropped() {
        // `fn id(x) { x }` (no annotation) generalizes to `forall a. a -> a`;
        // called with Int, it becomes a clone and the original `id` is dropped.
        let names = mono_names("fn id(x) { x }\nfn main() -> Int { id(42) }");
        assert!(
            !names.contains(&"id".to_string()),
            "original generic `id` should be dropped, got {names:?}"
        );
        assert!(
            names.iter().any(|n| n.starts_with("id__")),
            "expected an `id__*` clone, got {names:?}"
        );
        assert!(names.contains(&"main".to_string()), "got {names:?}");
    }

    #[test]
    fn two_instantiations_of_same_generic_fn() {
        // `id` called with Int twice should share one clone (cache hit).
        let names = mono_names("fn id(x) { x }\nfn main() -> Int { id(1) + id(2) }");
        let id_clones = names.iter().filter(|n| n.starts_with("id__")).count();
        assert_eq!(id_clones, 1, "expected 1 shared clone, got {names:?}");
    }

    #[test]
    fn specialized_clone_carries_concrete_types_throughout() {
        // Checking only the clone's mangled name does not prove
        // monomorphization happened: every Type in the cloned function must be
        // substituted as well. This matters for descriptor selection, GC
        // tracing, debug metadata, and any non-uniform optimization.
        let src = "fn id(x) { x }\nfn main() -> Int { id(42) }";
        let map = SourceMap::new();
        let id = map.intern("mono_type_test.px", src);
        let parsed = parse(id, src);
        let mut analysis = analyze_root(id, &parsed.tree);
        let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
        let module = lower(id, &root, &mut analysis);
        let mono = monomorphize(module, &analysis.names, &mut analysis.db);
        let clone = mono
            .items
            .iter()
            .find_map(|item| match item {
                TypedItem::Fn(f) if f.name.starts_with("id__") => Some(f),
                _ => None,
            })
            .expect("id clone");

        assert_eq!(
            analysis.db.render(clone.fn_type),
            "(Int) -> Int",
            "the clone's function type must be concrete"
        );
        assert_eq!(
            analysis.db.render(clone.params[0].ty),
            "Int",
            "the cloned parameter must use the specialization"
        );
        assert_eq!(
            analysis.db.render(clone.body.ty),
            "Int",
            "the cloned body/result must use the specialization"
        );
    }

    #[test]
    fn zero_argument_generic_result_is_specialized_from_use_context() {
        // `empty` is `forall T. () -> Vec[T]`; the subsequent push pins T to
        // Int. An empty arg list is still a real generic call site and must not
        // cause the original to be dropped without emitting a clone.
        let names = mono_names(
            "fn empty() { Vec() }\n\
             fn main() -> Int { let values = empty(); values.push(1); values.len() }",
        );
        assert!(
            names.iter().any(|n| n.starts_with("empty__")),
            "expected a specialized zero-arg clone, got {names:?}"
        );
    }

    /// **MONO-01.** Two instantiations of one generic are two *concrete*
    /// functions, not two names for the same unresolved one. The clone used to
    /// be renamed and nothing else: specialization unified a fresh
    /// instantiation of the scheme, then `follow`ed the clone's own types —
    /// which mentioned different variables entirely — so both clones came out
    /// carrying the binder as unbound as it started.
    #[test]
    fn two_instantiations_of_one_generic_are_two_concrete_functions() {
        let src = "fn id(x) { x }\n\
                   fn main() -> Unit { out(id(1)); out(id(\"t\")) }";
        let map = SourceMap::new();
        let file = map.intern("mono_two_concrete.px", src);
        let parsed = parse(file, src);
        let mut analysis = analyze_root(file, &parsed.tree);
        let root = praxis_ast::SourceFile::cast(parsed.tree.clone()).unwrap();
        let module = lower(file, &root, &mut analysis);
        let mono = monomorphize(module, &analysis.names, &mut analysis.db);
        let mut shapes: Vec<String> = mono
            .items
            .iter()
            .filter_map(|i| match i {
                TypedItem::Fn(f) if f.name.starts_with("id__") => {
                    Some(analysis.db.render(f.fn_type))
                }
                _ => None,
            })
            .collect();
        shapes.sort();
        assert_eq!(
            shapes,
            vec!["(Int) -> Int".to_string(), "(Text) -> Text".to_string()],
            "each clone's own type must be the one its call site chose"
        );
    }

    /// **MONO-02.** A zero-argument generic is specialized *per result type*.
    /// `empty` is `forall T. () -> Vec[T]`; nothing in its argument list says
    /// which `T`, so keying on arguments alone made two uses one clone — after
    /// the guard that skipped zero-argument sites entirely stopped dropping the
    /// original without emitting any clone at all.
    #[test]
    fn a_zero_argument_generic_is_specialized_per_result_type() {
        let names = mono_names(
            "fn empty() { Vec() }\n\
             fn main() -> Int {\n\
               let ints = empty(); ints.push(1)\n\
               let texts = empty(); texts.push(\"t\")\n\
               ints.len() + texts.len()\n\
             }",
        );
        let clones: Vec<&String> = names.iter().filter(|n| n.starts_with("empty__")).collect();
        assert_eq!(
            clones.len(),
            2,
            "`Vec[Int]` and `Vec[Text]` are two instantiations: {names:?}"
        );
        assert!(
            !names.contains(&"empty".to_string()),
            "the generic original is dropped: {names:?}"
        );
    }

    /// MONO-03. The cache key is a [`TypeKey`], and an enum's key carries its
    /// def **and its arguments** — so `Option[Int]` and `Option[Text]` are two
    /// keys. The rendered string it replaces was one: `render` emitted the
    /// nominal name alone, because before F12 the element type lived in a fresh
    /// def rather than in the type.
    #[test]
    fn enum_payload_types_participate_in_monomorphization_cache_key() {
        let names = mono_names(
            "fn id(x) { x }\n\
             fn main() -> Unit { out(id(Some(1))); out(id(Some(\"text\"))) }",
        );
        let id_clones = names.iter().filter(|n| n.starts_with("id__")).count();
        assert_eq!(
            id_clones, 2,
            "Option payload types are distinct concrete instantiations: {names:?}"
        );
    }
}
