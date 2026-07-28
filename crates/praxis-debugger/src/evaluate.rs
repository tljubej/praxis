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

use std::io::Write;

use praxis_ast::AstNode;
use praxis_codegen_cranelift::Jit;
use praxis_hir::{analyze_root, lower, mono::monomorphize, TypedItem};
use praxis_mir::{annotate, lower_module};
use praxis_runtime::{
    crash_snapshot::SnapshotFrame, CrashSnapshot, GcRef, RootScope, Runtime, RuntimeContext,
};
use praxis_types::{CollectionCtor, Type, TypeData, TypeDb};

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

/// The maximum number of locals a frame may have for `p EXPR` to evaluate
/// against it. The arity-dispatched call site supports up to this many ABI
/// args (extend `call_multi` to raise it).
const MAX_SUPPORTED_ARITY: usize = 6;

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
) -> EvalResult {
    let bindings = collect_bindings(frame, db);
    let source = synthesize(db, &bindings, expr_text);
    let (typed_fn, _expr_ty, _fresh_db) = build_pipeline(&source)?;
    // Purity gate (§9.5, §19.10): reject mutating/diverging expressions. Runs
    // on the HIR tail expression, before JIT.
    assert_read_only(&typed_fn.body.tail)?;
    let result = exec(runtime, snapshot, &bindings, &source)?;
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
    let bindings = collect_bindings(frame, db);
    let source = synthesize(db, &bindings, expr_text);
    match build_pipeline(&source) {
        Ok((_typed_fn, expr_ty, fresh_db)) => Ok(fresh_db.render(expr_ty)),
        Err(e) => Err(e),
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
) -> EvalResult {
    let bindings = collect_bindings(frame, db);
    let source = synthesize(db, &bindings, expr_text);
    let (typed_fn, expr_ty, fresh_db) = build_pipeline(&source)?;
    assert_read_only(&typed_fn.body.tail)?;
    let result = exec(runtime, snapshot, &bindings, &source)?;
    let type_str = fresh_db.render(expr_ty);
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
/// The type is derived primarily from the **runtime value's descriptor** (via
/// [`descriptor_to_type`]), which is always concrete — the static `type_id`
/// (WS1) is only a fallback. This is necessary because Praxis's inference
/// leaves collection element types as unbound vars when a `Vec()` is filled by
/// later `push` calls (`let xs = Vec(); xs.push(11)` types `xs` as `Vec[?T]`,
/// not `Vec[Int]`); the runtime descriptor carries the real element type, so
/// `p xs.len()` / `p xs.get(0)` type-check correctly.
fn collect_bindings(frame: &SnapshotFrame, db: &mut TypeDb) -> Vec<LocalBinding> {
    frame
        .locals
        .iter()
        .filter(|l| is_real_ref(l.value))
        .filter(|l| l.is_user() && !l.name().is_empty())
        .map(|l| LocalBinding {
            name: sanitize_name(&l.name()),
            ty: descriptor_to_type(l.value, db).unwrap_or(Type(l.type_id)),
            value: l.value,
        })
        .collect()
}

/// Build the synthetic source `fn __p_expr(<typed params>) { EXPR }`.
fn synthesize(db: &TypeDb, bindings: &[LocalBinding], expr_text: &str) -> String {
    if bindings.is_empty() {
        return format!("fn {P_EXPR_FN}() {{ {expr_text} }}");
    }
    let params: Vec<String> = bindings
        .iter()
        .map(|b| format!("{}: {}", b.name, db.render(b.ty)))
        .collect();
    format!("fn {P_EXPR_FN}({}) {{ {expr_text} }}", params.join(", "))
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
    bindings: &[LocalBinding],
    source: &str,
) -> Result<GcRef, String> {
    if bindings.len() > MAX_SUPPORTED_ARITY {
        return Err(format!(
            "frame has {} named locals; `p` supports up to {MAX_SUPPORTED_ARITY}",
            bindings.len()
        ));
    }
    // Recompile into a fresh Jit (the session's main Jit is untouched; multiple
    // Jits coexist — confirmed by the jit.rs test suite creating one per test).
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
    }
    let mut jit = Jit::new().map_err(|e| format!("JIT init failed: {e}"))?;
    let ids = jit
        .compile(&funcs, &mut analysis.db)
        .map_err(|e| format!("JIT compile failed: {e}"))?;
    let id = *ids
        .get(P_EXPR_FN)
        .ok_or_else(|| "internal: __p_expr FuncId not found".to_string())?;

    // Root the snapshot + the call args for the duration of the call (§12.3).
    // The synthetic fn pushes its own shadow frame at entry, but the args are
    // in flight before the prologue spills them; this scope is the safety net.
    let mut scope = RootScope::child(snapshot);
    for b in bindings {
        scope.root(b.value);
    }

    let mut ctx: RuntimeContext = runtime.context();
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
            // The ABI always passes ctx + ≥1 GcRef slot; a zero-arg fn never
            // reads the placeholder, so the immortal Unit (or any
            // non-dereferenced GcRef) is safe. Reuse the dangling sentinel.
            let f: unsafe extern "C" fn(*mut RuntimeContext, GcRef) -> GcRef =
                unsafe { std::mem::transmute(entry) };
            let placeholder = null_sentinel();
            unsafe { f(ctx, placeholder) }
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

/// A null-ish GcRef for the unused ABI slot when calling a zero-arg synthetic
/// function. The dangling sentinel is never dereferenced (mirrors the debug
/// frame's null-slot pattern); a zero-arg `__p_expr` never reads its slot.
fn null_sentinel() -> GcRef {
    use std::ptr::NonNull;
    let nn = NonNull::<praxis_runtime::GcHeader>::dangling();
    // SAFETY: dangling NonNull is non-null and aligned; never dereferenced.
    unsafe { GcRef::from_non_null(nn) }
}

/// Make a snapshot local name safe as a Praxis identifier. Snapshot source
/// names are already valid identifiers (they came from the parser); this is a
/// defensive guard for any synthetic temp that slipped past the `<tmp>` filter.
fn sanitize_name(name: &str) -> String {
    if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.is_empty()
        && !name.chars().next().unwrap().is_ascii_digit()
    {
        name.to_string()
    } else {
        "_x".to_string()
    }
}

/// Map a runtime value's descriptor to its static `Type` (M10b-WS4). The
/// runtime `TypeDescriptor` carries a stable `TypeId` per kind, so the
/// top-level shape (Vec/Map/Int/Text/…) is always recoverable exactly — even
/// when inference left the static `type_id` as `Vec[?T]` (the inference gap
/// for `Vec()` filled by later `push` calls).
///
/// Collection element types default to `Int` (the overwhelmingly common `p
/// EXPR` case). This is sound for evaluation: `p xs.len()` doesn't read the
/// element type; `p xs.get(0)` type-checks against `Int` and the runtime
/// returns + formats the real value through its own descriptor. Returns `None`
/// for descriptors we don't map (the caller falls back to the static
/// `type_id`).
fn descriptor_to_type(value: GcRef, db: &mut TypeDb) -> Option<Type> {
    descriptor_id_to_type(value.descriptor().id, db)
}

/// Map a top-level descriptor id to its static `Type` (collections default
/// their element type to `Int`).
fn descriptor_id_to_type(id: praxis_runtime::TypeId, db: &mut TypeDb) -> Option<Type> {
    use praxis_runtime::TypeId;
    match id {
        // Scalars.
        TypeId(0) | TypeId(1) | TypeId(4) => Some(db.int()), // INT / BYTE → Int
        TypeId(2) => Some(db.bool()),
        TypeId(3) => Some(db.char()),
        TypeId(5) => Some(db.text()),
        // Single-element collections (element defaults to Int).
        TypeId(6) | TypeId(13) | TypeId(7) | TypeId(15) | TypeId(16) | TypeId(17) | TypeId(18) => {
            let ctor = match id {
                TypeId(6) => CollectionCtor::Vec,
                TypeId(7) => CollectionCtor::Grid,
                TypeId(13) => CollectionCtor::Deque,
                TypeId(15) => CollectionCtor::Set,
                TypeId(16) => CollectionCtor::Counter,
                TypeId(17) => CollectionCtor::MinHeap,
                _ => CollectionCtor::MaxHeap,
            };
            let elem = db.int();
            Some(db.intern(TypeData::Collection {
                ctor,
                args: vec![elem],
            }))
        }
        // Map(14): default to Map[Int, Int].
        TypeId(14) => {
            let k = db.int();
            Some(db.intern(TypeData::Collection {
                ctor: CollectionCtor::Map,
                args: vec![k, k],
            }))
        }
        // Tuples(10), records(8), enums(9), closures(11), var-cell(12),
        // bitset(19): fall back to the static type_id (the caller handles it).
        _ => None,
    }
}

/// True iff `r` is a real GC reference (not the null sentinel). Mirrors the
/// check in crash_snapshot / render.
fn is_real_ref(r: GcRef) -> bool {
    use std::ptr::NonNull;
    let dangling = NonNull::<praxis_runtime::GcHeader>::dangling();
    !std::ptr::eq(r.as_ptr(), dangling.as_ptr())
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

    #[test]
    fn synthesize_no_locals_is_zero_arg() {
        let db = TypeDb::new();
        let src = synthesize(&db, &[], "1 + 2");
        assert_eq!(src, "fn __p_expr() { 1 + 2 }");
    }

    #[test]
    fn synthesize_with_typed_params() {
        let mut db = TypeDb::new();
        let int = db.int();
        let bindings = vec![LocalBinding {
            name: "n".to_string(),
            ty: int,
            value: null_sentinel(),
        }];
        let src = synthesize(&db, &bindings, "n + 1");
        assert_eq!(src, "fn __p_expr(n: Int) { n + 1 }");
    }

    #[test]
    fn sanitize_rejects_digit_leading_and_punct() {
        assert_eq!(sanitize_name("1abc"), "_x");
        assert_eq!(sanitize_name("a-b"), "_x");
        assert_eq!(sanitize_name("ok_name"), "ok_name");
        assert_eq!(sanitize_name("x9"), "x9");
    }
}
