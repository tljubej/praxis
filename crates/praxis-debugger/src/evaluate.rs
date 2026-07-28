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
    generation: &Rc<Generation>,
) -> EvalResult {
    let bindings = collect_bindings(frame, db);
    let source = synthesize(db, &bindings, expr_text);
    let (typed_fn, _expr_ty, _fresh_db) = build_pipeline(&source)?;
    // Purity gate (§9.5, §19.10): reject mutating/diverging expressions. Runs
    // on the HIR tail expression, before JIT.
    assert_read_only(&typed_fn.body.tail)?;
    let result = exec(runtime, snapshot, &bindings, &source, generation)?;
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
    generation: &Rc<Generation>,
) -> EvalResult {
    let bindings = collect_bindings(frame, db);
    let source = synthesize(db, &bindings, expr_text);
    let (typed_fn, expr_ty, fresh_db) = build_pipeline(&source)?;
    assert_read_only(&typed_fn.body.tail)?;
    let result = exec(runtime, snapshot, &bindings, &source, generation)?;
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
/// The type is derived primarily from **what the value itself records** (via
/// [`praxis_repr::type_for_value`]), which is always concrete — the static
/// `type_id` (WS1) is only a fallback. This is necessary because Praxis's
/// inference leaves collection element types as unbound vars when a `Vec()` is
/// filled by later `push` calls (`let xs = Vec(); xs.push(11)` types `xs` as
/// `Vec[?T]`, not `Vec[Int]`); the payload carries the real element type, so
/// `p xs.len()` / `p xs.get(0)` type-check correctly.
///
/// The bridge reads the payload rather than guessing from the top-level
/// descriptor, so a `Vec[Text]` is now a `Vec[Text]` here and not the `Vec[Int]`
/// the hand-written map answered for every vector (DBG-02, bounded by P0-11).
fn collect_bindings(frame: &SnapshotFrame, db: &mut TypeDb) -> Vec<LocalBinding> {
    frame
        .locals
        .iter()
        .filter(|l| l.is_user() && !l.name().is_empty())
        .filter_map(|l| l.value.map(|value| (l, value)))
        .map(|(l, value)| LocalBinding {
            name: sanitize_name(&l.name()),
            // SAFETY: a `Some` value was spilled by generated code, and a
            // snapshot local's `GcRef` is rooted by the snapshot (ADR-033).
            ty: unsafe { praxis_repr::type_for_value(value, db) }.unwrap_or(Type(l.type_id)),
            value,
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
    generation: &Rc<Generation>,
) -> Result<GcRef, String> {
    if bindings.len() > MAX_SUPPORTED_ARITY {
        return Err(format!(
            "frame has {} named locals; `p` supports up to {MAX_SUPPORTED_ARITY}",
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
        // A real heap value: a `LocalBinding` carries a live `GcRef`, and
        // `synthesize` reading only the name and type is not a licence to put
        // an invalid one there.
        let runtime = praxis_runtime::Runtime::new();
        let bindings = vec![LocalBinding {
            name: "n".to_string(),
            ty: int,
            value: runtime.alloc_int(7),
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
            value: Some(value),
            type_id: ty.0,
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
