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

use praxis_codegen_cranelift::Jit;
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
    /// The `fn name -> FuncId` map (at least `main`); used by `restart`/lookup.
    pub func_ids: std::collections::HashMap<String, cranelift_module::FuncId>,
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
    /// The input source path, if `--input` was given (retained for §9.7
    /// metadata; not re-read — `input_text` is the source of truth).
    pub input_path: Option<PathBuf>,
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
        if !self.input_text.is_empty() {
            ctx.input_source = self.runtime.alloc_text(&self.input_text);
        }
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
        }
        let mut new_jit =
            praxis_codegen_cranelift::Jit::new().map_err(|e| format!("JIT init failed: {e}"))?;
        let ids = new_jit
            .compile(&funcs, &mut analysis.db)
            .map_err(|e| format!("JIT compile failed: {e}"))?;
        let main_id = *ids
            .get("main")
            .ok_or_else(|| "no `main` function in reloaded source".to_string())?;
        // SAFETY: main_id is a finalized entry in new_jit.
        let new_entry: MainEntry = unsafe { std::mem::transmute(new_jit.entry(main_id)) };
        // 3. Compilation succeeded — swap in the new state (§9.7: discard old
        // JIT + snapshots only after success). The old self.jit drops here.
        self.jit = new_jit;
        self.main_entry = new_entry;
        self.func_ids = ids;
        self.analysis = analysis;
        self.source_text = text;
        // 4. Rerun with the same input.
        Ok(self.restart())
    }
}
