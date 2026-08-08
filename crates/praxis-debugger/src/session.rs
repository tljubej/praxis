//! The debug session state the crash REPL reaches into for `p EXPR`, `source`,
//! `input`, `parser`, `restart`, and `reload` (§9.4–§9.7, M10b).
//!
//! M10a's `Repl` owned only the snapshot. M10b's commands need far more: the
//! live `Runtime` (to host `p EXPR`'s synthetic call and to read
//! `ParseDetail`), the `Jit` (to compile the synthetic function and to look up
//! `main` for `restart`), the `TypeDb` (to type-check `p EXPR` against the
//! selected frame's locals and to render `type EXPR`), the program source
//! (for `source`), and the original input (for `restart`/`reload`'s "same
//! input" guarantee, §9.7).
//!
//! Rather than thread half a dozen `&mut` borrows through every `handle`
//! call, the [`DebugSession`] owns all of it. The [`crate::repl::Repl`] holds
//! an `Option<DebugSession>`: `Some` when driven from a real fault (the CLI
//! hands the live state off), `None` in pure-navigation unit tests that build
//! a synthetic snapshot directly.

use std::path::PathBuf;
use std::rc::Rc;

use praxis_codegen_cranelift::{Generation, Jit};
use praxis_hir::Analysis;
use praxis_runtime::Runtime;

/// The type-erased entry pointer for `main`: `unsafe extern "C"
/// fn(*mut RuntimeContext) -> GcRef`, which is what codegen emits for a
/// zero-parameter function. Cached so `restart` can re-call without
/// re-resolving the `FuncId`.
pub type MainEntry =
    unsafe extern "C" fn(*mut praxis_runtime::RuntimeContext) -> praxis_runtime::GcRef;

/// Everything the crash REPL needs to evaluate expressions, render context,
/// and restart/reload. Owned by the [`crate::repl::Repl`].
///
/// Lifetime note: the `Runtime` and `Jit` are independent owners; nothing here
/// borrows the other, so the session is freely movable. The snapshot (held by
/// the `Repl`) was *taken* out of the `Runtime` before the session was built,
/// so the two are decoupled — the snapshot is a root set the GC walks, the
/// `Runtime` is the heap that root set refers into.
pub struct DebugSession {
    /// The compiled JIT module + the `main` entry pointer. Kept alive for the
    /// whole REPL session so `p EXPR` can compile synthetic functions into a
    /// fresh `Jit` and `restart` can re-call `main`. `reload` swaps this.
    pub jit: Jit,
    /// The transmuted `main` entry, ready to call. Recomputed by `reload`.
    pub main_entry: MainEntry,
    /// The heap + fault/snapshot/parse-detail slots. `p EXPR` hosts its
    /// synthetic call here; `restart`/`reload` re-run `main` against it. The
    /// snapshot the REPL inspects refers into this heap (non-moving GC keeps
    /// addresses stable, ADR-011).
    pub runtime: Runtime,
    /// The analysis (owns the `TypeDb`): needed to type-check `p EXPR` against
    /// the selected frame's locals and to render `type EXPR`. `reload`
    /// recomputes this from the re-read source.
    pub analysis: Analysis,
    /// The program source text (for `source`).
    pub source_text: String,
    /// The program source path (for `reload` — re-reads this from disk).
    pub source_path: PathBuf,
    /// The original input bytes (for `restart`/`reload`'s "same input"
    /// guarantee, §9.7). Re-installed as `input_source` on each re-run.
    pub input_text: String,
    /// The JIT generation every `p EXPR` / `heap EXPR` compiles into (F13,
    /// DBG-05).
    ///
    /// `p` compiles a throwaway module per command. Before S8 each one leaked
    /// its schemas, names and debug metadata for the life of the process, so a
    /// long session grew without bound. Sharing one generation makes that
    /// metadata *interned*: the same expression evaluated a hundred times
    /// allocates once. It is deliberately separate from `jit`'s generation and
    /// survives `reload`, because the values a `p` left in the heap keep
    /// pointing at schemas this arena owns.
    pub eval_generation: Rc<Generation>,
}

impl DebugSession {
    /// Run `main` against this session's runtime, returning the result `GcRef`.
    /// Used by `restart`/`reload` (§9.7) to re-execute the program.
    ///
    /// Resets the fault + snapshot + parse-detail slots first (a fresh run
    /// must not inherit the prior fault's state), then re-installs the input
    /// buffer and calls `main`.
    ///
    /// # Safety
    /// `main_entry` must be a finalized JIT entry for `main` in `self.jit`.
    pub unsafe fn rerun_main(&mut self) -> praxis_runtime::GcRef {
        use praxis_runtime::RuntimeContext;
        // The runtime's fault/snapshot/parse-detail slots are owned by it; the
        // context view we take below aliases them. We must clear the snapshot
        // before re-running so the new fault (if any) captures a fresh chain.
        self.runtime.clear_for_rerun();
        let mut ctx: RuntimeContext = self.runtime.context();
        // Installed unconditionally, including when it is empty: a zero-length
        // buffer is empty input, not the absence of input, and the rule is
        // stated at `praxis_get_input` (ADR-087). The guard that used to stand
        // here is why a `restart` after a zero-byte-input fault re-ran with no
        // buffer at all — the second banner was contentless and `input` answered
        // "(no input context — not a parse failure)" about a run that had failed
        // to parse, which is not the "same input" §9.7 promises.
        //
        // What stays true and is worth saying: `input_text` is the source of
        // truth from here, because `praxis-cli`'s `clear_input_reader` disarms
        // the reader before the REPL starts. For a program that faulted *before*
        // its first `read`, that text is empty because nothing was read — so a
        // `reload` that moves the `read` earlier sees empty input rather than the
        // original stdin. That is a property of an exhausted stdin, not of this
        // line.
        ctx.input_source = self.runtime.alloc_text(&self.input_text);
        // SAFETY: caller guarantees main_entry is a finalized entry in self.jit.
        unsafe { (self.main_entry)(&mut ctx as *mut RuntimeContext) }
    }

    /// `restart` (§9.7): rerun the *same* compiled code with the same input.
    /// Returns the result `GcRef` (or the Unit sentinel on fault). The caller
    /// takes the new snapshot and resets the frame cursor. No recompilation.
    pub fn restart(&mut self) -> praxis_runtime::GcRef {
        // SAFETY: main_entry is a finalized entry in self.jit (set at session
        // construction or the last successful reload).
        unsafe { self.rerun_main() }
    }

    /// Tear the session down in the one order that is sound: heap first, then
    /// the generation arenas its objects pointed into (F13, hazard H15).
    ///
    /// Every `RecordPayload` and `TuplePayload` in the heap holds a raw
    /// `*const …Schema` into one of these two arenas — `jit`'s for values `main`
    /// built, `eval_generation`'s for values a `p EXPR` built. Dropping the
    /// runtime runs their finalizers and yields the
    /// [`HeapDrained`](praxis_runtime::HeapDrained) that `retire` demands, so
    /// the ordering is checked by the compiler rather than by this comment.
    ///
    /// A session that is merely dropped leaks both arenas, which is what every
    /// pre-S8 run did.
    pub fn teardown(self) {
        let DebugSession {
            jit,
            runtime,
            eval_generation,
            ..
        } = self;
        let proof = runtime.teardown();
        jit.retire(proof.clone());
        Generation::retire(eval_generation, proof.clone());
        // The parser plans are the third arena: a `read`/`parse` in the program
        // registered one per compile, and every `reload` registered more
        // (IP-12).
        praxis_runtime::retire_parser_plans(&proof);
    }

    /// `reload` (§9.7): re-read the source from `source_path`, recompile, and
    /// on success swap in the new `Jit`/analysis/`main_entry` then rerun with
    /// the same input. On failure (diagnostics or compile error), leaves the
    /// current session intact and returns `Err(diagnostics)`. Per §9.7, old
    /// JIT code and snapshots are discarded only after the new compilation
    /// succeeds.
    pub fn reload(&mut self) -> Result<praxis_runtime::GcRef, String> {
        use praxis_ast::AstNode;
        // 1. Re-read the source from disk (retains input bytes per §9.7).
        let text = std::fs::read_to_string(&self.source_path)
            .map_err(|e| format!("failed to re-read source: {e}"))?;
        // 2. Recompile: parse → analyze → lower → mono → MIR → JIT.
        let map = praxis_source::SourceMap::new();
        let path_str = self.source_path.to_string_lossy();
        let file = map.intern(&*path_str, &text);
        let parsed = praxis_parser::parse(file, &text);
        if let Some(d) = parsed.diagnostics.first() {
            return Err(format!("parse error: {}", d.message()));
        }
        let mut analysis = praxis_hir::analyze_root(file, &parsed.tree);
        if let Some(d) = analysis.diagnostics.first() {
            return Err(format!("type error: {}", d.message()));
        }
        let root = praxis_ast::SourceFile::cast(parsed.tree.clone())
            .ok_or_else(|| "internal: parse tree root is not a SOURCE_FILE".to_string())?;
        let module = praxis_hir::lower(file, &root, &mut analysis);
        if let Some(d) = module.diagnostics.first() {
            return Err(format!("lowering error: {}", d.message()));
        }
        let module = praxis_hir::mono::monomorphize(module, &analysis.names, &mut analysis.db);
        let mut funcs = praxis_mir::lower_module(&module, &mut analysis.db);
        for f in &mut funcs {
            praxis_mir::annotate(f);
            praxis_mir::verify(f)
                .map_err(|errs| format!("internal: {}", praxis_mir::verify::report(&errs)))?;
        }
        let mut new_jit =
            praxis_codegen_cranelift::Jit::new().map_err(|e| format!("JIT init failed: {e}"))?;
        let ids = new_jit
            .compile(&funcs, &mut analysis.db)
            .map_err(|e| format!("JIT compile failed: {e}"))?;
        // The same entry-point rule the CLI's `run` uses (REP-19, ADR-067): a
        // file's top-level statements are its program, and `fn main` is the
        // fallback for a file with none.
        let main_id = *praxis_hir::entry_point(|name| ids.contains_key(name))
            .and_then(|name| ids.get(name))
            .ok_or_else(|| {
                "no statements to run and no `main` function in reloaded source".to_string()
            })?;
        // SAFETY: main_id is a finalized entry in new_jit.
        let new_entry: MainEntry = unsafe { std::mem::transmute(new_jit.entry(main_id)) };
        // 3. Compilation succeeded — swap in the new state (§9.7: discard old
        // JIT + snapshots only after success). The old self.jit drops here.
        self.jit = new_jit;
        self.main_entry = new_entry;
        self.analysis = analysis;
        self.source_text = text;
        // 4. Rerun with the same input.
        Ok(self.restart())
    }
}
