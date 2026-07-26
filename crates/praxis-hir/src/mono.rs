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

use std::collections::HashMap;

use praxis_types::{Scheme, Type, TypeDb};

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
    /// Cache key: (callee symbol, rendered-structural arg types). Rendered (not
    /// type-id) so two call sites with the same concrete type share a clone even
    /// when inference gave them distinct arena slots.
    cache: HashMap<(SymbolId, Vec<String>), String>,
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

    /// Instantiate `callee` at the given concrete `arg_types`, returning the
    /// mangled clone name (cached). Emits the clone into `self.clones` on first
    /// use; rewrites the clone's own body (transitive instantiation).
    fn instantiate(&mut self, callee: SymbolId, arg_types: &[Type]) -> String {
        let canon = canonicalize(self.db, arg_types);
        let key = (callee, canon.clone());
        if let Some(name) = self.cache.get(&key) {
            return name.clone();
        }
        let name = mangled_name(callee, &canon, self.names);
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
        let mut clone = specialize(self.db, &original, &scheme, arg_types, &name);
        // Rewrite the clone's body for any *further* polymorphic calls it makes
        // (transitive instantiation). This may append more clones.
        rewrite_block(&mut clone.body, self);
        self.clones.push(TypedItem::Fn(clone));
        name
    }
}

/// Canonicalize a slice of types into stable structural strings for the cache
/// key. Uses the type pretty-printer so two call sites with the same concrete
/// type (e.g. two `Int` args with distinct arena slots from inference) share one
/// monomorphized clone.
fn canonicalize(db: &TypeDb, types: &[Type]) -> Vec<String> {
    types.iter().map(|t| db.render(db.follow(*t))).collect()
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
    match e {
        TypedExpr::Call {
            callee,
            callee_name,
            arg_types,
            args,
            callee_expr,
            ..
        } => {
            // Recurse into args first (they may contain polymorphic calls).
            for a in args.iter_mut() {
                rewrite_expr(a, pass);
            }
            // A postfix call's callee expression (a closure value) may itself
            // contain polymorphic calls in its body — recurse so they rewrite.
            if let Some(ce) = callee_expr {
                rewrite_expr(ce, pass);
            }
            // Is this callee a polymorphic user fn? (Closure-value callees have
            // no scheme / are not in originals; their `arg_types` may be empty.)
            let is_poly = scheme_of(pass.names, *callee).is_some_and(|s| s.is_polymorphic())
                && pass.originals.contains_key(callee);
            if is_poly && !arg_types.is_empty() {
                let mangled = pass.instantiate(*callee, arg_types);
                *callee_name = mangled;
            }
        }
        TypedExpr::Closure { body, .. } => {
            // A closure's body may contain polymorphic calls too.
            rewrite_block(body, pass);
        }
        TypedExpr::Bin { lhs, rhs, .. } => {
            rewrite_expr(lhs, pass);
            rewrite_expr(rhs, pass);
        }
        TypedExpr::Unary { operand, .. } => rewrite_expr(operand, pass),
        TypedExpr::Paren { inner, .. } => {
            if let Some(inner) = inner {
                rewrite_expr(inner, pass);
            }
        }
        TypedExpr::Block(b) => rewrite_block(b, pass),
        TypedExpr::If {
            cond,
            then_block,
            else_block,
            ..
        } => {
            rewrite_expr(cond, pass);
            rewrite_block(then_block, pass);
            if let Some(eb) = else_block.as_mut() {
                rewrite_block(eb, pass);
            }
        }
        TypedExpr::While { cond, body, .. } => {
            rewrite_expr(cond, pass);
            rewrite_block(body, pass);
        }
        TypedExpr::For { iter, body, .. } => {
            rewrite_expr(iter, pass);
            rewrite_block(body, pass);
        }
        TypedExpr::Loop { body, .. } => rewrite_block(body, pass),
        TypedExpr::Break { value, .. } => {
            if let Some(v) = value {
                rewrite_expr(v, pass);
            }
        }
        TypedExpr::Continue { .. } => {}
        TypedExpr::Return { value, .. } => {
            if let Some(v) = value {
                rewrite_expr(v, pass);
            }
        }
        TypedExpr::MethodCall { receiver, args, .. } => {
            rewrite_expr(receiver, pass);
            for a in args {
                rewrite_expr(a, pass);
            }
        }
        TypedExpr::Tuple { elements, .. } => {
            for el in elements {
                rewrite_expr(el, pass);
            }
        }
        TypedExpr::Parse { text, .. } => rewrite_expr(text, pass),
        TypedExpr::RecordLit { fields, .. } => {
            for (_, init) in fields {
                rewrite_expr(init, pass);
            }
        }
        TypedExpr::FieldGet { receiver, .. } => rewrite_expr(receiver, pass),
        TypedExpr::EnumVariant { args, .. } => {
            for a in args {
                rewrite_expr(a, pass);
            }
        }
        TypedExpr::Match {
            scrutinee, arms, ..
        } => {
            rewrite_expr(scrutinee, pass);
            for arm in arms {
                rewrite_expr(&mut arm.body, pass);
            }
        }
        // Leaves: no calls to rewrite.
        TypedExpr::Lit { .. } | TypedExpr::Path { .. } | TypedExpr::Read { .. } => {}
    }
}

/// Specialize `original` (a generic fn) for the concrete `arg_types`. Instantiates
/// the scheme, unifies the param types with the arg types (pinning the quantified
/// vars), clones the body, and substitutes every `Type` by its resolved form.
/// Renames the clone to `name`.
fn specialize(
    db: &mut TypeDb,
    original: &TypedFn,
    scheme: &Scheme,
    arg_types: &[Type],
    name: &str,
) -> TypedFn {
    // Instantiate the scheme fresh, then unify the instantiated Func's param
    // types with the concrete arg types. This pins the quantified vars to the
    // concrete types.
    let instantiated = db.instantiate(scheme);
    let (param_types, _result_ty) = func_shape(db, instantiated);
    for (pt, at) in param_types.iter().zip(arg_types.iter()) {
        let _ = db.unify(*pt, *at);
    }
    // Clone the original and substitute every Type by its resolved (followed)
    // form. After unification, generic vars are linked to concrete types, so
    // `follow` resolves them.
    let mut clone = original.clone();
    clone.name = name.to_string();
    clone.fn_type = resolve_type(db, clone.fn_type);
    clone.return_type = resolve_type(db, clone.return_type);
    for p in &mut clone.params {
        p.ty = resolve_type(db, p.ty);
    }
    resolve_block(db, &mut clone.body);
    clone
}

/// Walk a typed block substituting every `Type` by its resolved form.
fn resolve_block(db: &TypeDb, block: &mut TypedBlock) {
    for stmt in &mut block.stmts {
        resolve_stmt(db, stmt);
    }
    resolve_expr(db, &mut block.tail);
    block.ty = resolve_type(db, block.ty);
}

fn resolve_stmt(db: &TypeDb, stmt: &mut TypedStmt) {
    match stmt {
        TypedStmt::Let { ty, init, .. } | TypedStmt::Var { ty, init, .. } => {
            *ty = resolve_type(db, *ty);
            resolve_expr(db, init);
        }
        TypedStmt::Assign { value, .. } => resolve_expr(db, value),
        TypedStmt::Expr(e) => resolve_expr(db, e),
    }
}

fn resolve_expr(db: &TypeDb, e: &mut TypedExpr) {
    match e {
        TypedExpr::Lit { ty, .. }
        | TypedExpr::Path { ty, .. }
        | TypedExpr::Bin { ty, .. }
        | TypedExpr::Unary { ty, .. }
        | TypedExpr::Paren { ty, .. }
        | TypedExpr::If { ty, .. }
        | TypedExpr::While { ty, .. }
        | TypedExpr::For { ty, .. }
        | TypedExpr::Loop { ty, .. }
        | TypedExpr::Break { ty, .. }
        | TypedExpr::Continue { ty, .. }
        | TypedExpr::Return { ty, .. }
        | TypedExpr::Call { ty, .. }
        | TypedExpr::MethodCall { ty, .. }
        | TypedExpr::Tuple { ty, .. }
        | TypedExpr::Read { ty, .. }
        | TypedExpr::Parse { ty, .. }
        | TypedExpr::RecordLit { ty, .. }
        | TypedExpr::FieldGet { ty, .. }
        | TypedExpr::EnumVariant { ty, .. }
        | TypedExpr::Match { ty, .. } => *ty = resolve_type(db, *ty),
        TypedExpr::Block(b) => resolve_block(db, b),
        TypedExpr::Closure {
            params,
            body,
            fn_type,
            ty,
            captures,
            ..
        } => {
            for p in params {
                p.ty = resolve_type(db, p.ty);
            }
            resolve_block(db, body);
            *fn_type = resolve_type(db, *fn_type);
            *ty = resolve_type(db, *ty);
            for c in captures {
                c.ty = resolve_type(db, c.ty);
            }
        }
    }
    // Recurse into children to resolve their types too.
    match e {
        TypedExpr::Bin { lhs, rhs, .. } => {
            resolve_expr(db, lhs);
            resolve_expr(db, rhs);
        }
        TypedExpr::Unary { operand, .. } => resolve_expr(db, operand),
        TypedExpr::Paren {
            inner: Some(inner), ..
        } => resolve_expr(db, inner),
        TypedExpr::Paren { inner: None, .. } => {}
        TypedExpr::If {
            cond,
            then_block,
            else_block,
            ..
        } => {
            resolve_expr(db, cond);
            resolve_block(db, then_block);
            if let Some(eb) = else_block.as_mut() {
                resolve_block(db, eb);
            }
        }
        TypedExpr::While { cond, body, .. } => {
            resolve_expr(db, cond);
            resolve_block(db, body);
        }
        TypedExpr::For {
            iter,
            body,
            item_ty,
            ..
        } => {
            resolve_expr(db, iter);
            *item_ty = resolve_type(db, *item_ty);
            resolve_block(db, body);
        }
        TypedExpr::Loop { body, .. } => resolve_block(db, body),
        TypedExpr::Break { value: Some(v), .. } => resolve_expr(db, v),
        TypedExpr::Continue { .. }
        | TypedExpr::Break { value: None, .. }
        | TypedExpr::Return { value: None, .. } => {}
        TypedExpr::Return { value: Some(v), .. } => resolve_expr(db, v),
        TypedExpr::Call {
            args,
            arg_types,
            callee_expr,
            ..
        } => {
            for a in args {
                resolve_expr(db, a);
            }
            for at in arg_types {
                *at = resolve_type(db, *at);
            }
            if let Some(ce) = callee_expr {
                resolve_expr(db, ce);
            }
        }
        TypedExpr::MethodCall { receiver, args, .. } => {
            resolve_expr(db, receiver);
            for a in args {
                resolve_expr(db, a);
            }
        }
        TypedExpr::Tuple { elements, .. } => {
            for el in elements {
                resolve_expr(db, el);
            }
        }
        TypedExpr::Parse { text, .. } => resolve_expr(db, text),
        TypedExpr::RecordLit { fields, .. } => {
            for (_, init) in fields {
                resolve_expr(db, init);
            }
        }
        TypedExpr::FieldGet { receiver, .. } => resolve_expr(db, receiver),
        TypedExpr::EnumVariant { args, .. } => {
            for a in args {
                resolve_expr(db, a);
            }
        }
        TypedExpr::Match {
            scrutinee, arms, ..
        } => {
            resolve_expr(db, scrutinee);
            for arm in arms {
                resolve_expr(db, &mut arm.body);
            }
        }
        _ => {}
    }
}

/// Resolve a type to its followed (link-free) form. After mono unification,
/// generic vars are linked to concrete types; `follow` chases the chain.
fn resolve_type(db: &TypeDb, t: Type) -> Type {
    db.follow(t)
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
}
