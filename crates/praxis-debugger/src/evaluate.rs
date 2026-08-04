//! The `p EXPR` / `type EXPR` read-only JIT evaluator (§9.5, M10b-WS4).
//!
//! §9.5's pipeline:
//! 1. Parse the expression using ordinary Praxis syntax.
//! 2. Resolve names against the selected frame snapshot.
//! 3. Type-check using captured local types.
//! 4. JIT-compile a synthetic read-only function.
//! 5. Execute with references to snapshot slots.
//! 6. Format the result.
//!
//! Steps 1–3 fall out for free by **synthesizing** a one-function source module
//! `fn __p_expr(<typed params>) { EXPR }` whose parameters mirror the selected
//! snapshot frame's locals (name + full static type). The standard pipeline
//! (parse → analyze → lower → mono → MIR) then resolves `EXPR`'s identifiers
//! against those params and type-checks it with the captured local types — no
//! bespoke resolver needed. The full static `Type` ids threaded in WS1 are what
//! make this work: the runtime `descriptor` alone loses element types, so
//! `vec[0]` / `vec.len()` / `rec.field` could not type-check without them.
//!
//! The read-only gate (`crate::purity`) runs between steps 3 and 4, rejecting
//! any mutating/diverging/input-consuming expression (§9.5, §19.10).
//!
//! Step 5 passes the snapshot's local `GcRef`s as the synthetic function's ABI
//! arguments, rooted for the call (§12.3 "active debugger-expression
//! arguments") so a GC inside the evaluator cannot collect them. Step 6 formats
//! the returned `GcRef` via its descriptor (ADR-032: allocate on the main heap).
//!
//! # What the frame contributes (DBG-06)
//!
//! Only the locals the expression **names**. The frame used to contribute all
//! of them, and since the synthetic function has to be *whole* to compile, one
//! unusable local was a total failure — every command, `p 1 + 2` included, died
//! on the frame rather than on the expression. Three ways a local was unusable,
//! all reported by the user in one sitting:
//!
//! - its type named a `struct` or `enum` the synthetic module never declared
//!   (``unknown type `Foo` ``);
//! - its type had no source syntax at all — an anonymous `read lines(…)` record,
//!   an unfilled `Vec()`'s `Vec[?T]` ("expected a type");
//! - there were more than [`MAX_SUPPORTED_ARITY`] of them, so the arity check
//!   refused the call before looking at what was asked.
//!
//! Binding by mention fixes the third outright and narrows the other two to the
//! expression that actually asks for the local; [`crate::synth`] then declares
//! what it can and refuses what it cannot, so an unusable local costs its own
//! name and nothing else. The mention set is over-approximated from the
//! expression's *tokens* — binding a local named `y` because `foo.y` was
//! written is harmless, missing one is not.

use std::io::Write;
use std::rc::Rc;

use praxis_ast::AstNode;
use praxis_codegen_cranelift::{Generation, Jit};
use praxis_hir::{analyze_root, lower, mono::monomorphize, TypedItem};
use praxis_mir::{annotate, lower_module};
use praxis_runtime::{
    crash_snapshot::SnapshotFrame, CrashSnapshot, GcRef, NativeScope, RootSet, Runtime,
    RuntimeContext,
};
use praxis_types::{Type, TypeDb};

use crate::purity::assert_read_only;

/// The synthesized function's name (a reserved, unlikely-to-collide identifier).
const P_EXPR_FN: &str = "__p_expr";

/// Which evaluation the REPL requested: print the value (`p`), its type
/// (`type`), or recursively inspect it (`heap`). All three share the synthesis
/// + pipeline; `type` stops before the purity gate and JIT.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Print,
    Type,
    Heap,
}

/// One named, typed, valued local extracted from a snapshot frame, ready to
/// render as a typed parameter and pass as an ABI argument.
struct LocalBinding {
    name: String,
    ty: Type,
    value: GcRef,
}

/// The maximum number of locals **one expression may name**. The arity-dispatched
/// call site supports up to this many ABI args (extend [`call_with_arity`] to
/// raise it).
///
/// It used to bound the whole *frame*, which is a different and much smaller
/// number to run out of: a program with seven top-level `var`s could not
/// evaluate `p 1 + 2`, because the frame was counted before the expression was
/// read. Six names in one debugger expression is a real ceiling; six bindings in
/// a function is not.
const MAX_SUPPORTED_ARITY: usize = 6;

/// The synthetic module one command compiles, and what building it cost.
struct Synthetic {
    /// The full source: type declarations, then `fn __p_expr(…) { EXPR }`.
    source: String,
    /// The locals that became parameters, in ABI order.
    bindings: Vec<LocalBinding>,
    /// Locals the expression named that could not be bound, each with the type
    /// that has no spelling. Attached to a failure as a note — on its own,
    /// "unknown name `points`" about a name the `locals` listing plainly shows
    /// is a worse answer than the truth.
    unbound: Vec<(String, String)>,
    /// Minted-name → structural-rendering pairs, so `type`/`heap` report the
    /// type the user's program has and not the one this module invented.
    minted: Vec<(String, String)>,
}

impl Synthetic {
    /// Add what could not be bound to a pipeline failure, so a diagnostic about
    /// a missing name says which local went missing and why.
    fn explain(&self, err: String) -> String {
        let mut out = err;
        for (name, ty) in &self.unbound {
            out.push_str(&format!(
                "\nnote: local `{name}` was not bound — its type `{ty}` has no source spelling"
            ));
        }
        out
    }
}

/// The outcome of evaluating `p EXPR` / `type EXPR`. `Ok(string)` on success
/// (the formatted value, or the rendered type); `Err(message)` for any
/// rejection, parse/type error, or evaluator fault.
pub type EvalResult = Result<String, String>;

/// Evaluate `expr_text` against the selected frame's locals and return the
/// formatted result (§9.5 step 6). Implements the full pipeline: synthesize →
/// pipeline → purity-gate → JIT → call → format.
///
/// `db` is the program's `TypeDb` (used to render param annotations + read
/// `type_id`s); `runtime` hosts the synthetic call (and is checked for faults
/// after); `snapshot` roots the call args; `frame` is the selected frame.
pub fn evaluate(
    db: &mut TypeDb,
    runtime: &mut Runtime,
    snapshot: &CrashSnapshot,
    frame: &SnapshotFrame,
    expr_text: &str,
    generation: &Rc<Generation>,
) -> EvalResult {
    let synthetic = synthesize(db, frame, expr_text);
    let (typed_fn, _expr_ty, _fresh_db) =
        build_pipeline(&synthetic.source).map_err(|e| synthetic.explain(e))?;
    // Purity gate (§9.5, §19.10): reject mutating/diverging expressions. Runs
    // on the HIR tail expression, before JIT.
    assert_read_only(&typed_fn.body.tail)?;
    let result = exec(runtime, snapshot, &synthetic, generation)?;
    let mut out = String::new();
    result.format(&mut out);
    Ok(if out.is_empty() {
        "<unreadable>".to_string()
    } else {
        out
    })
}

/// Compute the inferred type of `expr_text` against the selected frame's locals
/// (§9.4 `type EXPR`). Same synthesis + pipeline as `evaluate`, but stops
/// before JIT/purity and renders the expression's type instead.
pub fn type_of(db: &mut TypeDb, frame: &SnapshotFrame, expr_text: &str) -> EvalResult {
    let synthetic = synthesize(db, frame, expr_text);
    match build_pipeline(&synthetic.source) {
        Ok((_typed_fn, expr_ty, fresh_db)) => Ok(crate::synth::humanize(
            &synthetic.minted,
            &fresh_db.render(expr_ty),
        )),
        Err(e) => Err(synthetic.explain(e)),
    }
}

/// Recursively inspect a value (§9.4 `heap EXPR`, M10b-WS5). Evaluates `EXPR`
/// (reusing the `p EXPR` path, including the purity gate) and renders the
/// result with its type prefix and the descriptor's recursive `format`
/// (which already walks record fields, collection elements, tuple members).
/// The type prefix is what distinguishes `heap` from `p`: `heap xs` shows
/// `Vec[Int]: [11, 22]`, making the structure + type visible at a glance.
pub fn heap(
    db: &mut TypeDb,
    runtime: &mut Runtime,
    snapshot: &CrashSnapshot,
    frame: &SnapshotFrame,
    expr_text: &str,
    generation: &Rc<Generation>,
) -> EvalResult {
    let synthetic = synthesize(db, frame, expr_text);
    let (typed_fn, expr_ty, fresh_db) =
        build_pipeline(&synthetic.source).map_err(|e| synthetic.explain(e))?;
    assert_read_only(&typed_fn.body.tail)?;
    let result = exec(runtime, snapshot, &synthetic, generation)?;
    let type_str = crate::synth::humanize(&synthetic.minted, &fresh_db.render(expr_ty));
    let mut value_str = String::new();
    result.format(&mut value_str);
    Ok(if value_str.is_empty() {
        format!("{type_str}: <unreadable>")
    } else {
        format!("{type_str}: {value_str}")
    })
}

/// Extract the named, real-valued user locals from `frame`, pairing each with a
/// static `Type` and its current `GcRef`. Skips sentinel (uninit) values and
/// compiler temporaries (only bindings the programmer wrote are valid `p EXPR`
/// parameters). The temp filter is structural — `l.is_user()` — replacing the
/// old `name != "<tmp>"` string match (the codegen no longer emits `"<tmp>"`;
/// temps now carry an empty name and the `Temp` kind).
///
/// The type is derived primarily from **what the value itself records** (via
/// [`praxis_repr::type_for_value`]), which is always concrete — the static
/// `type_id` (WS1) is only a fallback. This is necessary because Praxis's
/// inference leaves collection element types as unbound vars when a `Vec()` is
/// filled by later `push` calls (`var xs = Vec(); xs.push(11)` types `xs` as
/// `Vec[?T]`, not `Vec[Int]`); the payload carries the real element type, so
/// `p xs.len()` / `p xs.get(0)` type-check correctly.
///
/// The bridge reads the payload rather than guessing from the top-level
/// descriptor, so a `Vec[Text]` is now a `Vec[Text]` here and not the `Vec[Int]`
/// the hand-written map answered for every vector (DBG-02, bounded by P0-11).
///
/// Only the locals `expr_text` **mentions** are candidates (DBG-06). Every local
/// used to be one, which made the synthetic function's health a property of the
/// frame rather than of the expression: one local the module could not declare,
/// or a seventh local of any kind, and `p 1 + 2` failed too.
fn collect_bindings(
    frame: &SnapshotFrame,
    db: &mut TypeDb,
    mentioned: &std::collections::HashSet<String>,
) -> Vec<LocalBinding> {
    let candidates: Vec<(u32, String, praxis_runtime::GcRef)> = frame
        .locals
        .iter()
        .filter(|l| l.is_user() && is_bindable_name(&l.name()))
        .filter(|l| mentioned.contains(&l.name()))
        .filter_map(|l| {
            // `reference()` rather than the value: a local whose box ADR-120
            // elided holds a raw payload, and `p EXPR` binds *objects* — it
            // roots them (`scope.root` below), recovers their type through
            // their descriptor, and hands them to compiled code as `GcRef`
            // arguments. None of that is available for a word. In practice no
            // such local is ever a candidate anyway: the filter above keeps
            // user bindings, and the forwarding only elides compiler temps.
            l.value
                .and_then(praxis_runtime::DebugValue::reference)
                .map(|value| (l.type_id, l.name().to_string(), value))
        })
        .collect();
    let bindings: Vec<LocalBinding> = candidates
        .into_iter()
        .filter_map(|(type_id, name, value)| {
            // SAFETY: a `Some` value was spilled by generated code, and a
            // snapshot local's `GcRef` is rooted by the snapshot (ADR-033).
            let recovered = unsafe { praxis_repr::type_for_value(value, db) }.ok();
            // F5: the fallback id is rehydrated through the arena's checked
            // route. A frame whose `type_id` this `TypeDb` never minted has no
            // type to bind, so the local is dropped rather than bound to
            // whatever slot the raw index named.
            let ty = recovered.or_else(|| db.type_from_raw(type_id))?;
            Some(LocalBinding { name, ty, value })
        })
        .collect();
    keep_innermost(bindings)
}

/// The identifiers `expr_text` mentions, over-approximated from its tokens.
///
/// Over-approximation is the safe direction and the reason this is a lexer and
/// not a resolver: `foo.y` yields `y` as well as `foo`, so a local named `y` is
/// bound needlessly — costing one ABI argument — while *missing* a mention would
/// silently drop a name the expression needs. Malformed input needs no special
/// case either: `lex` is total, and the parse step reports the syntax error.
fn mentioned_idents(expr_text: &str) -> std::collections::HashSet<String> {
    praxis_parser::lex(praxis_source::FileId::SYNTHETIC, expr_text)
        .tokens
        .iter()
        .filter(|t| t.kind == praxis_syntax::SyntaxKind::Ident)
        .map(|t| expr_text[t.span.start().to_usize()..t.span.end().to_usize()].to_string())
        .collect()
}

/// Keep the last local of each name, preserving order.
///
/// A snapshot frame flattens every scope of its function, so a shadowed name
/// appears once per binding — `var x = 1` and a block's `var x = "s"` are two
/// locals both called `x`. Two parameters of one name compile (the later wins,
/// which is the innermost one and the answer the user means), but a synthetic
/// function whose signature *cannot* be malformed is worth more than relying on
/// that: the duplicate is dropped here, and with it the ABI argument it would
/// have consumed against [`MAX_SUPPORTED_ARITY`].
fn keep_innermost(bindings: Vec<LocalBinding>) -> Vec<LocalBinding> {
    let mut seen = std::collections::HashSet::new();
    let mut kept: Vec<LocalBinding> = bindings
        .into_iter()
        .rev()
        .filter(|b| seen.insert(b.name.clone()))
        .collect();
    kept.reverse();
    kept
}

/// Build the synthetic module for `expr_text` against `frame`: the `struct`/
/// `enum` declarations the parameter types need (see [`crate::synth`]), then
/// `fn __p_expr(<typed params>) { EXPR }`.
///
/// A local whose type has no source spelling is left out rather than written
/// down wrong — the module has to compile as a whole, so an unwritable
/// annotation would take the expression with it.
fn synthesize(db: &mut TypeDb, frame: &SnapshotFrame, expr_text: &str) -> Synthetic {
    let mentioned = mentioned_idents(expr_text);
    let candidates = collect_bindings(frame, db, &mentioned);
    synthesize_from(db, candidates, &mentioned, expr_text)
}

/// The half of [`synthesize`] that turns already-collected bindings into a
/// module. Split out because "which locals does this frame offer" and "what
/// module do these bindings make" fail in different ways and are worth testing
/// apart: the second one is where a type's spelling is decided.
fn synthesize_from(
    db: &mut TypeDb,
    candidates: Vec<LocalBinding>,
    mentioned: &std::collections::HashSet<String>,
    expr_text: &str,
) -> Synthetic {
    let mut params = Vec::with_capacity(candidates.len());
    let mut bindings = Vec::with_capacity(candidates.len());
    let mut unspellable = Vec::new();
    let mut speller = crate::synth::Speller::new(db);
    for candidate in candidates {
        match speller.spell(candidate.ty) {
            Some(ty) => {
                params.push(format!("{}: {ty}", candidate.name));
                bindings.push(candidate);
            }
            None => unspellable.push((candidate.name, candidate.ty)),
        }
    }
    // The types the *expression* names, which need not be any local's type:
    // `p Pt{x: 1, y: 2}` and `type Move` name a declaration and no value.
    speller.declare_named(mentioned);
    let decls = speller.declarations();
    let minted = speller.into_minted();
    let unbound = unspellable
        .into_iter()
        .map(|(name, ty)| (name, db.render(ty)))
        .collect();
    // The declarations are their own statements, so the function starts on the
    // next line; a module that declared nothing is exactly the one-line source
    // this synthesis has always produced.
    let source = format!(
        "{decls}{}fn {P_EXPR_FN}({}) {{ {expr_text} }}",
        if decls.is_empty() { "" } else { "\n" },
        params.join(", ")
    );
    Synthetic {
        source,
        bindings,
        unbound,
        minted,
    }
}

/// Run parse → analyze → lower on the synthetic source, returning the
/// `__p_expr` typed function (whose `body.tail` the purity gate walks), the
/// tail expression's type, and the fresh `TypeDb` (type ids are positional and
/// do NOT cross dbs, so the caller must render the type with this db). Stops
/// before mono/MIR — the JIT path (`exec`) re-runs those steps on its own fresh
/// analysis (it needs the `db` owned, and a separate `Jit`).
fn build_pipeline(source: &str) -> Result<(praxis_hir::TypedFn, Type, TypeDb), String> {
    let map = praxis_source::SourceMap::new();
    let file = map.intern("p_expr.px", source);
    let parsed = praxis_parser::parse(file, source);
    if let Some(d) = parsed.diagnostics.first() {
        return Err(format!("parse error: {}", d.message()));
    }
    let mut analysis = analyze_root(file, &parsed.tree);
    if let Some(d) = analysis.diagnostics.first() {
        return Err(format!("type error: {}", d.message()));
    }
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone())
        .ok_or_else(|| "internal: parse tree root is not a SOURCE_FILE".to_string())?;
    let module = lower(file, &root, &mut analysis);
    if let Some(d) = module.diagnostics.first() {
        return Err(format!("lowering error: {}", d.message()));
    }
    let typed_fn = module
        .items
        .iter()
        .find_map(|item| match item {
            TypedItem::Fn(f) if f.name == P_EXPR_FN => Some(f.clone()),
            _ => None,
        })
        .ok_or_else(|| "internal: __p_expr not found after lowering".to_string())?;
    let expr_ty = praxis_hir::expr_ty(&typed_fn.body.tail);
    Ok((typed_fn, expr_ty, analysis.db))
}

/// JIT-compile the synthetic source into a fresh `Jit` and call `__p_expr`
/// with the snapshot locals as ABI arguments (§9.5 step 4–5). Roots the args
/// for the call so a GC inside the evaluator cannot collect them (§12.3).
fn exec(
    runtime: &mut Runtime,
    snapshot: &CrashSnapshot,
    synthetic: &Synthetic,
    generation: &Rc<Generation>,
) -> Result<GcRef, String> {
    let Synthetic {
        source, bindings, ..
    } = synthetic;
    if bindings.len() > MAX_SUPPORTED_ARITY {
        return Err(format!(
            "the expression names {} locals; `p` supports up to {MAX_SUPPORTED_ARITY}",
            bindings.len()
        ));
    }
    // Recompile into a fresh Jit (the session's main Jit is untouched; multiple
    // Jits coexist — confirmed by the jit.rs test suite creating one per test).
    // The *generation* is shared, though: the module is thrown away after the
    // call, but the schemas and debug metadata it minted may still be named by
    // values this call left in the heap, and interning them is what keeps a
    // long session from growing without bound (DBG-05).
    let map = praxis_source::SourceMap::new();
    let file = map.intern("p_expr.px", source);
    let parsed = praxis_parser::parse(file, source);
    let mut analysis = analyze_root(file, &parsed.tree);
    let root = praxis_ast::SourceFile::cast(parsed.tree.clone())
        .ok_or_else(|| "internal: parse tree root is not a SOURCE_FILE".to_string())?;
    let module = lower(file, &root, &mut analysis);
    let module = monomorphize(module, &analysis.names, &mut analysis.db);
    let mut funcs = lower_module(&module, &mut analysis.db);
    for f in &mut funcs {
        annotate(f);
        praxis_mir::verify(f)
            .map_err(|errs| format!("internal: {}", praxis_mir::verify::report(&errs)))?;
    }
    let mut jit =
        Jit::in_generation(Rc::clone(generation)).map_err(|e| format!("JIT init failed: {e}"))?;
    let ids = jit
        .compile(&funcs, &mut analysis.db)
        .map_err(|e| format!("JIT compile failed: {e}"))?;
    let id = *ids
        .get(P_EXPR_FN)
        .ok_or_else(|| "internal: __p_expr FuncId not found".to_string())?;

    let mut ctx: RuntimeContext = runtime.context();
    // Root the snapshot's values + the call args for the duration of the call
    // (§12.3). The synthetic fn pushes its own shadow frame at entry, but the
    // args are in flight before the prologue spills them, and the REPL has
    // *taken* the snapshot out of the runtime — so `RuntimeRoots` cannot see
    // it through `ctx.crash_snapshot`. This used to be a `RootScope` attached
    // to nothing at all: the collector never consulted it, so a collection
    // inside `__p_expr` could reclaim the very locals being printed (DBG-04).
    // A `NativeScope` chains onto the context, which is what the collector
    // actually walks.
    // SAFETY: `ctx` is live for the whole call below, and the scope is dropped
    // before it.
    let scope = unsafe { NativeScope::new(&mut ctx) };
    let mut snapshot_roots = Vec::new();
    snapshot.push_roots(&mut snapshot_roots);
    for r in snapshot_roots {
        scope.root(r);
    }
    for b in bindings {
        scope.root(b.value);
    }

    // Clear the *stale* fault left by the original crash that triggered the
    // REPL — `__p_expr`'s safepoints (`CheckFault`) would otherwise see it and
    // bail immediately. We do NOT clear the snapshot (still rooting the args).
    // The parse-detail slot is left untouched too (it records the original
    // fault's input context, which `input`/`parser` may still render).
    let _ = runtime.take_fault();
    // SAFETY: `id` is a finalized __p_expr FuncId in `jit` (just compiled); the
    // `jit` outlives the call.
    let entry_ptr = unsafe { jit.entry(id) };
    let vals: Vec<GcRef> = bindings.iter().map(|b| b.value).collect();
    // SAFETY: entry_ptr is a finalized __p_expr entry in `jit` (alive for the
    // call); the args match the synthetic fn's params by construction (same
    // bindings → same arity → same ABI); the scope roots them across any GC.
    let result = unsafe { call_with_arity(entry_ptr, &mut ctx, &vals) };
    drop(jit);

    // `p EXPR` may itself fault (div0, OOB, …). Report the fault kind rather
    // than formatting the (sentinel Unit) result. Clear it so the next command
    // starts clean.
    if runtime.has_pending_fault() {
        let kind = runtime.fault();
        let _ = runtime.take_fault();
        return Err(format!("expression faulted: {kind}"));
    }
    Ok(result)
}

/// Call a JIT entry pointer with `vals.len()` GcRef arguments. The Fast calling
/// convention places each GcRef in successive arg slots, so we transmute to the
/// matching fixed-arity function-pointer type per arm.
///
/// # Safety
/// `entry` must be a finalized JIT entry whose function's MIR param count
/// equals `vals.len()`; `ctx` must be a live wired context.
unsafe fn call_with_arity(entry: *const u8, ctx: *mut RuntimeContext, vals: &[GcRef]) -> GcRef {
    match vals.len() {
        0 => {
            // A zero-parameter synthetic function is `fn(ctx) -> GcRef`, which
            // is what codegen emitted for it. The previous arm transmuted to a
            // one-slot signature and filled the slot with a dangling `GcRef` —
            // an invalid value handed across an ABI that promises a valid one.
            let f: unsafe extern "C" fn(*mut RuntimeContext) -> GcRef =
                unsafe { std::mem::transmute(entry) };
            unsafe { f(ctx) }
        }
        1 => {
            let f: unsafe extern "C" fn(*mut RuntimeContext, GcRef) -> GcRef =
                unsafe { std::mem::transmute(entry) };
            unsafe { f(ctx, vals[0]) }
        }
        2 => {
            let f: unsafe extern "C" fn(*mut RuntimeContext, GcRef, GcRef) -> GcRef =
                unsafe { std::mem::transmute(entry) };
            unsafe { f(ctx, vals[0], vals[1]) }
        }
        3 => {
            let f: unsafe extern "C" fn(*mut RuntimeContext, GcRef, GcRef, GcRef) -> GcRef =
                unsafe { std::mem::transmute(entry) };
            unsafe { f(ctx, vals[0], vals[1], vals[2]) }
        }
        4 => {
            let f: unsafe extern "C" fn(*mut RuntimeContext, GcRef, GcRef, GcRef, GcRef) -> GcRef =
                unsafe { std::mem::transmute(entry) };
            unsafe { f(ctx, vals[0], vals[1], vals[2], vals[3]) }
        }
        5 => {
            let f: unsafe extern "C" fn(
                *mut RuntimeContext,
                GcRef,
                GcRef,
                GcRef,
                GcRef,
                GcRef,
            ) -> GcRef = unsafe { std::mem::transmute(entry) };
            unsafe { f(ctx, vals[0], vals[1], vals[2], vals[3], vals[4]) }
        }
        6 => {
            let f: unsafe extern "C" fn(
                *mut RuntimeContext,
                GcRef,
                GcRef,
                GcRef,
                GcRef,
                GcRef,
                GcRef,
            ) -> GcRef = unsafe { std::mem::transmute(entry) };
            unsafe { f(ctx, vals[0], vals[1], vals[2], vals[3], vals[4], vals[5]) }
        }
        n => unreachable!("arity {n} exceeds MAX_SUPPORTED_ARITY, guarded by exec"),
    }
}

/// Whether a snapshot local's name can be a `__p_expr` parameter (DBG-03).
///
/// Snapshot source names are already valid identifiers — they came from the
/// parser — so this is the guard for a synthetic name that slipped past the
/// temp filter. It **rejects** rather than rewrites, for two reasons. Rewriting
/// mapped every unusable name to the single name `_x`, which is not injective:
/// two such locals became two parameters both called `_x`, and a synthetic
/// function with a duplicate parameter is a worse failure than the one being
/// papered over. And the question it asked was ASCII-only, so §4.1's Unicode
/// identifiers — which the lexer accepts and which are therefore real local
/// names — were "sanitized" into `_x` as if they were malformed.
///
/// A rejected local is dropped, exactly as one whose `type_id` this `TypeDb`
/// never minted is: `p EXPR` cannot mention a name the language cannot spell,
/// so there is nothing lost by not binding it.
fn is_bindable_name(name: &str) -> bool {
    praxis_syntax::ident::is_ident(name)
}

/// Render an `EvalResult` to the REPL output stream (value, or `error: …`).
pub fn write_eval_result<W: Write>(out: &mut W, result: &EvalResult) -> std::io::Result<()> {
    match result {
        Ok(value) => writeln!(out, "{value}"),
        Err(err) => writeln!(out, "error: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame with no bindable local synthesizes the zero-parameter function.
    /// The leading newline is the (empty) declaration block; nothing depends on
    /// it being absent, and the parser treats it as it treats any blank line.
    #[test]
    fn synthesize_no_locals_is_zero_arg() {
        let mut db = TypeDb::new();
        let frame = frame_with(Vec::new());
        let synthetic = synthesize(&mut db, &frame, "1 + 2");
        assert_eq!(synthetic.source, "fn __p_expr() { 1 + 2 }");
        assert!(synthetic.bindings.is_empty());
    }

    #[test]
    fn synthesize_with_typed_params() {
        let runtime = Runtime::new();
        let mut db = TypeDb::new();
        let int = db.int();
        let frame = frame_with(vec![snapshot_local(
            "n",
            runtime.alloc_int(7),
            int,
            praxis_runtime::LOCAL_KIND_USER,
        )]);
        let synthetic = synthesize(&mut db, &frame, "n + 1");
        assert_eq!(synthetic.source, "fn __p_expr(n: Int) { n + 1 }");
    }

    /// DBG-06, the reported defect, at the level `p` builds its module: a
    /// `struct` local made the module name a type it never declared, so the
    /// pipeline answered ``unknown type `Foo` `` — to `p foo`, and equally to
    /// `p 1 + 2`, which names nothing.
    #[test]
    fn a_struct_local_brings_its_declaration_into_the_module() {
        let runtime = Runtime::new();
        let mut db = TypeDb::new();
        let text = db.text();
        let inner = db.record(
            Some("Poo".to_string()),
            praxis_types::FieldSet::from_pairs(vec![("z".to_string(), text)]).expect("one field"),
        );
        let int = db.int();
        let foo = db.record(
            Some("Foo".to_string()),
            praxis_types::FieldSet::from_pairs(vec![
                ("x".to_string(), inner),
                ("y".to_string(), int),
            ])
            .expect("two fields"),
        );
        let bindings = vec![LocalBinding {
            name: "foo".to_string(),
            ty: foo,
            // A `LocalBinding` carries a live `GcRef`; that synthesis reads only
            // the name and type is not a licence to put an invalid one there.
            value: runtime.alloc_int(0),
        }];

        let synthetic = synthesize_from(&mut db, bindings, &mentioned_idents("foo.y"), "foo.y");
        assert!(
            synthetic.source.contains("struct Foo { x: Poo, y: Int }"),
            "{}",
            synthetic.source
        );
        assert!(
            synthetic.source.contains("struct Poo { z: Text }"),
            "the field's own type is declared too: {}",
            synthetic.source
        );
        assert!(
            synthetic.source.contains("fn __p_expr(foo: Foo) { foo.y }"),
            "{}",
            synthetic.source
        );
        build_pipeline(&synthetic.source).expect("the synthesized module type-checks");
    }

    /// …and the same struct local costs an expression that never mentions it
    /// nothing at all. This is the half of DBG-06 that made the report "`p`
    /// doesn't work" rather than "`p foo` doesn't work".
    #[test]
    fn an_expression_binds_only_the_locals_it_names() {
        let runtime = Runtime::new();
        let mut db = TypeDb::new();
        let int = db.int();
        let var = db.fresh_var();
        let unwritable = db.record(
            Some("Foo".to_string()),
            praxis_types::FieldSet::from_pairs(vec![("v".to_string(), var)]).expect("one field"),
        );
        let locals = || {
            vec![
                LocalBinding {
                    name: "foo".to_string(),
                    ty: unwritable,
                    value: runtime.alloc_int(1),
                },
                LocalBinding {
                    name: "n".to_string(),
                    ty: int,
                    value: runtime.alloc_int(2),
                },
            ]
        };

        // What `collect_bindings` offers an expression that names nothing: no
        // local at all, so the unusable one is not in the way.
        let synthetic = synthesize_from(&mut db, Vec::new(), &mentioned_idents("1 + 2"), "1 + 2");
        assert_eq!(synthetic.source, "fn __p_expr() { 1 + 2 }");
        assert!(synthetic.unbound.is_empty(), "nothing was asked for");
        build_pipeline(&synthetic.source).expect("`p 1 + 2` compiles beside a `struct` local");

        // And with both offered, the unwritable one is dropped rather than
        // taking the expression with it.
        let synthetic = synthesize_from(&mut db, locals(), &mentioned_idents("n * 2"), "n * 2");
        assert_eq!(synthetic.source, "fn __p_expr(n: Int) { n * 2 }");
        assert_eq!(synthetic.unbound.len(), 1, "`foo` was dropped, not written");
        build_pipeline(&synthetic.source).expect("the surviving binding compiles");
    }

    /// A local the expression *does* name but whose type has no spelling is
    /// dropped — and the failure says so. "unknown name `foo`" about a name the
    /// `locals` listing shows is the answer this note exists to replace.
    #[test]
    fn an_unspellable_local_is_dropped_with_an_explanation() {
        let runtime = Runtime::new();
        let mut db = TypeDb::new();
        let var = db.fresh_var();
        let vec_of_var = db
            .collection(
                praxis_types::CollectionCtor::Vec,
                praxis_types::CollectionArgs::new(praxis_types::CollectionCtor::Vec, vec![var])
                    .expect("Vec takes one arg"),
            )
            .expect("Vec[?T]");
        let bindings = vec![LocalBinding {
            name: "ys".to_string(),
            ty: vec_of_var,
            value: runtime.alloc_int(1),
        }];

        let synthetic =
            synthesize_from(&mut db, bindings, &mentioned_idents("ys.len()"), "ys.len()");
        assert_eq!(synthetic.source, "fn __p_expr() { ys.len() }");
        let err = build_pipeline(&synthetic.source).expect_err("`ys` is not in scope");
        let explained = synthetic.explain(err);
        assert!(
            explained.contains("local `ys` was not bound"),
            "{explained}"
        );
        assert!(explained.contains("Vec[?T]"), "{explained}");
    }

    /// A shadowed name is two locals in one flattened frame. Both used to become
    /// parameters — legal, since the later wins, but it spent an ABI slot on a
    /// binding nothing could refer to. The innermost is the one kept.
    #[test]
    fn a_shadowed_local_binds_once_innermost() {
        let runtime = Runtime::new();
        let mut db = TypeDb::new();
        let int = db.int();
        let text = db.text();
        let frame = frame_with(vec![
            snapshot_local(
                "x",
                runtime.alloc_int(1),
                int,
                praxis_runtime::LOCAL_KIND_USER,
            ),
            snapshot_local(
                "x",
                runtime.alloc_text("inner"),
                text,
                praxis_runtime::LOCAL_KIND_USER,
            ),
        ]);
        let synthetic = synthesize(&mut db, &frame, "x");
        assert_eq!(synthetic.source, "fn __p_expr(x: Text) { x }");
        assert_eq!(synthetic.bindings.len(), 1);
    }

    /// The arity ceiling counts what the *expression* names, not what the frame
    /// holds. Eight top-level `var`s used to mean `p 1 + 2` reported "frame has
    /// 8 named locals" — a refusal about the program, before the expression was
    /// even parsed.
    #[test]
    fn the_arity_ceiling_counts_the_expressions_names_not_the_frames() {
        let runtime = Runtime::new();
        let mut db = TypeDb::new();
        let int = db.int();
        let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
        let frame = frame_with(
            names
                .iter()
                .map(|n| {
                    snapshot_local(
                        n,
                        runtime.alloc_int(1),
                        int,
                        praxis_runtime::LOCAL_KIND_USER,
                    )
                })
                .collect(),
        );
        let snapshot = CrashSnapshot {
            frames: vec![frame],
            fault_kind: praxis_runtime::FaultKind::IndexOutOfBounds,
        };
        let mut runtime = Runtime::new();
        let generation = Rc::new(Generation::new());

        assert_eq!(
            evaluate(
                &mut db,
                &mut runtime,
                &snapshot,
                &snapshot.frames[0],
                "1 + 2",
                &generation,
            ),
            Ok("3".to_string()),
            "a frame of eight locals does not refuse an expression that names none"
        );
        // Seven *named* in one expression is still past the ABI dispatch, and
        // the message now says which count it is talking about.
        let err = evaluate(
            &mut db,
            &mut runtime,
            &snapshot,
            &snapshot.frames[0],
            "a + b + c + d + e + f + g + h",
            &generation,
        )
        .expect_err("eight names exceed the arity");
        assert!(err.contains("the expression names 8 locals"), "{err}");
    }

    #[test]
    fn mentioned_idents_over_approximates_from_tokens() {
        let idents = mentioned_idents("foo.y + xs.len()");
        for name in ["foo", "y", "xs", "len"] {
            assert!(idents.contains(name), "`{name}` is a mention: {idents:?}");
        }
        // A literal is not a name, and a malformed expression still lexes.
        assert!(!mentioned_idents("1 + 2").contains("1"));
        assert!(mentioned_idents("foo +").contains("foo"));
    }

    /// DBG-03. This test **asserted the defect** it now rules out (plan §8.2):
    /// it pinned `sanitize_name`'s rewrite of every unusable name to `_x`, and
    /// with it the ASCII-only rule that treated a Unicode identifier as damage
    /// to be repaired. A name is now either usable as written or its local is
    /// dropped; there is no third name, so nothing can collide with anything.
    #[test]
    fn an_unusable_local_name_is_rejected_rather_than_rewritten() {
        assert!(!is_bindable_name("1abc"));
        assert!(!is_bindable_name("a-b"));
        assert!(!is_bindable_name(""));
        assert!(is_bindable_name("ok_name"));
        assert!(is_bindable_name("x9"));
        // §4.1 allows Unicode identifiers, so these are names the lexer really
        // produces — the old rule rewrote all three to `_x`.
        assert!(is_bindable_name("δx"));
        assert!(is_bindable_name("Ünicode"));
        assert!(is_bindable_name("日本語"));
    }

    /// …and the collision the rewrite created is what `collect_bindings` must
    /// not be able to build: two unspellable locals used to become two
    /// parameters both called `_x`, so the synthetic
    /// `fn __p_expr(_x: Int, _x: Int)` failed to compile — and `p` reported a
    /// duplicate-declaration error about a name the program never contained.
    #[test]
    fn two_unusable_names_do_not_collide_into_one_parameter() {
        let runtime = Runtime::new();
        let mut db = TypeDb::new();
        let int = db.int();
        let user = praxis_runtime::LOCAL_KIND_USER;
        let frame = SnapshotFrame {
            parent: 0,
            func_name: "main".as_ptr(),
            func_name_len: 4,
            locals: vec![
                snapshot_local("1abc", runtime.alloc_int(1), int, user),
                snapshot_local("a-b", runtime.alloc_int(2), int, user),
                snapshot_local("δx", runtime.alloc_int(3), int, user),
            ],
            source_span: (0, 0),
        };

        let bindings = collect_bindings(&frame, &mut db, &mentioned_idents("1abc + a-b + δx"));
        let names: Vec<&str> = bindings.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["δx"],
            "only the name the language can spell is bound, and it is bound as written"
        );
        let synthetic = synthesize(&mut db, &frame, "δx");
        assert_eq!(synthetic.source, "fn __p_expr(δx: Int) { δx }");
    }

    /// DBG-04: the values the `p` evaluator holds must be in the collector's
    /// root set for the duration of the synthetic call.
    ///
    /// The evaluator used to build a `RootScope::child(snapshot)` — a root set
    /// attached to nothing at all. Nothing consulted it: automatic collection
    /// roots from the context, and the REPL has already *taken* the snapshot
    /// out of the runtime, so `RuntimeRoots` cannot reach it through
    /// `ctx.crash_snapshot` either.
    ///
    /// Generated prologues mask the argument case (the first safepoint spills
    /// the params into the shadow frame before anything can collect), so the
    /// exposed values are the snapshot locals `collect_bindings` filters out —
    /// the compiler temps, which are never passed as parameters. This drives
    /// the heap past its threshold first, so the first allocation inside
    /// `__p_expr` collects, then reads the temp back.
    #[test]
    fn snapshot_values_survive_a_collection_inside_the_evaluated_expression() {
        let mut runtime = Runtime::new();
        let mut db = TypeDb::new();
        let int = db.int();

        let named = runtime.alloc_vec(
            &praxis_runtime::scalars::INT,
            vec![runtime.alloc_int(11), runtime.alloc_int(22)],
        );
        let temp = runtime.alloc_vec(
            &praxis_runtime::scalars::INT,
            vec![runtime.alloc_int(33), runtime.alloc_int(44)],
        );
        let frame = SnapshotFrame {
            parent: 0,
            func_name: "main".as_ptr(),
            func_name_len: 4,
            locals: vec![
                snapshot_local("xs", named, int, praxis_runtime::LOCAL_KIND_USER),
                // A temp: in the snapshot, never a `__p_expr` parameter.
                snapshot_local("", temp, int, praxis_runtime::LOCAL_KIND_TEMP),
            ],
            source_span: (0, 0),
        };
        let snapshot = CrashSnapshot {
            frames: vec![frame],
            fault_kind: praxis_runtime::FaultKind::IndexOutOfBounds,
        };

        // Push well past the 64 KiB pacing threshold without collecting, so the
        // first allocation the synthetic function makes triggers a collection.
        for i in 0..5000_i64 {
            let _ = runtime.alloc_int(i);
        }

        let out = evaluate(
            &mut db,
            &mut runtime,
            &snapshot,
            &snapshot.frames[0],
            "1 + 2",
            &Rc::new(Generation::new()),
        )
        .expect("p 1 + 2 evaluates");
        assert_eq!(out, "3");

        // Both snapshot values must still be readable. An unrooted Vec is swept:
        // its element buffer is dropped, so this reads freed memory.
        assert_eq!(
            snapshot.frames[0].locals[0]
                .value
                .expect("the named local has a value")
                .as_vec()
                .iter()
                .map(|v| v.as_int())
                .collect::<Vec<_>>(),
            vec![11, 22],
            "the named local survived"
        );
        assert_eq!(
            snapshot.frames[0].locals[1]
                .value
                .expect("the temp has a value")
                .as_vec()
                .iter()
                .map(|v| v.as_int())
                .collect::<Vec<_>>(),
            vec![33, 44],
            "the temp, which is never a parameter, survived too"
        );
    }

    /// DBG-05/MIR-13: a debugger session that evaluates `p EXPR` a hundred
    /// times must not leak a hundred copies of the metadata.
    ///
    /// Each `p` compiles a throwaway module. Before S8 every one of them minted
    /// its function-name strings, debug-local metadata and schemas with
    /// `Box::leak`, so the process grew for the life of the session and nothing
    /// was ever reclaimable. They now belong to the session's one
    /// [`Generation`] and are interned by content, so after the caches are
    /// primed the arena stops moving entirely.
    #[test]
    fn repeated_evaluation_stops_growing_the_generation() {
        let mut runtime = Runtime::new();
        let mut db = TypeDb::new();
        let int = db.int();
        let xs = runtime.alloc_vec(
            &praxis_runtime::scalars::INT,
            vec![runtime.alloc_int(11), runtime.alloc_int(22)],
        );
        let snapshot = CrashSnapshot {
            frames: vec![SnapshotFrame {
                parent: 0,
                func_name: "main".as_ptr(),
                func_name_len: 4,
                locals: vec![snapshot_local(
                    "xs",
                    xs,
                    int,
                    praxis_runtime::LOCAL_KIND_USER,
                )],
                source_span: (0, 0),
            }],
            fault_kind: praxis_runtime::FaultKind::IndexOutOfBounds,
        };
        let generation = Rc::new(Generation::new());
        let mut eval = |n: usize| {
            for _ in 0..n {
                let out = evaluate(
                    &mut db,
                    &mut runtime,
                    &snapshot,
                    &snapshot.frames[0],
                    "xs.len()",
                    &generation,
                )
                .expect("`p xs.len()` evaluates");
                assert_eq!(out, "2");
            }
        };
        // Two rounds to prime every cache, then measure across twenty more.
        eval(2);
        let primed = generation.stats();
        assert!(
            primed.allocated_bytes > 0,
            "the evaluation must actually have put metadata in the arena"
        );
        eval(20);
        assert_eq!(
            generation.stats(),
            primed,
            "twenty more evaluations of one expression must allocate nothing new"
        );
    }

    /// A one-frame `main` snapshot holding `locals`.
    fn frame_with(locals: Vec<praxis_runtime::context::DebugLocal>) -> SnapshotFrame {
        SnapshotFrame {
            parent: 0,
            func_name: "main".as_ptr(),
            func_name_len: 4,
            locals,
            source_span: (0, 0),
        }
    }

    /// A `DebugLocal` holding a real value, as `praxis_snapshot_debug_chain`
    /// would have copied it out of a live frame.
    fn snapshot_local(
        name: &'static str,
        value: GcRef,
        ty: Type,
        kind: u8,
    ) -> praxis_runtime::context::DebugLocal {
        praxis_runtime::context::DebugLocal {
            source_name: name.as_ptr(),
            name_len: name.len() as u32,
            symbol_id: 0,
            descriptor: &praxis_runtime::collections::VEC as *const _,
            value: Some(praxis_runtime::DebugValue::Reference(value)),
            type_id: ty.to_u32(),
            kind,
            span_start: 0,
            span_end: 0,
        }
    }

    #[test]
    fn regression_runtime_vec_descriptor_recovers_its_real_element_type() {
        let runtime = Runtime::new();
        let text = runtime.alloc_text("hello");
        let value = runtime.alloc_vec(&praxis_runtime::text::TEXT, vec![text]);
        let mut db = TypeDb::new();

        // SAFETY: `value` was just allocated on `runtime`'s live heap.
        let ty =
            unsafe { praxis_repr::type_for_value(value, &mut db) }.expect("Vec has a runtime type");
        assert_eq!(db.render(ty), "Vec[Text]");
    }

    #[test]
    fn regression_runtime_scalar_descriptors_recover_their_actual_types() {
        let runtime = Runtime::new();
        let values = [
            (runtime.alloc_unit(), "Unit"),
            (runtime.alloc_bool(true), "Bool"),
            (runtime.alloc_int(1), "Int"),
            (runtime.alloc_byte(1), "Byte"),
            (runtime.alloc_char('x' as u32), "Char"),
            (runtime.alloc_float(1.5), "Float"),
            (runtime.alloc_text("x"), "Text"),
        ];

        for (value, expected) in values {
            let mut db = TypeDb::new();
            // SAFETY: `value` was just allocated on `runtime`'s live heap.
            let ty = unsafe { praxis_repr::type_for_value(value, &mut db) }
                .unwrap_or_else(|e| panic!("no debugger type for {expected}: {e}"));
            assert_eq!(
                db.render(ty),
                expected,
                "descriptor {:?} was recovered as the wrong debugger type",
                value.descriptor().id()
            );
        }
    }
}
