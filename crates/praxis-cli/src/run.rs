//! The `praxis run` command: load a `.px` file, run the full pipeline
//! (parse → analyze → typed HIR → MIR → Cranelift JIT → execute), and print the
//! result of the program's `main` function.
//!
//! Exit codes:
//! - `0` — program ran to completion with no fault.
//! - `1` — one or more language errors (parse/type/lowering) reported, OR the
//!   program faulted at runtime (overflow / division by zero / …).
//! - `2` — usage error (file missing, unreadable, etc.).
//!
//! `run::run` prints its own diagnostics and never returns `Err` for a
//! user-facing problem, so the exit code it returns is the final one.

use std::path::Path;

use praxis_ast::AstNode;
use praxis_codegen_cranelift::Jit;
use praxis_hir::{analyze_root, lower, mono::monomorphize};
use praxis_mir::{annotate, lower_module};
use praxis_runtime::{GcRef, Runtime, RuntimeContext};

use crate::debug_mode::DebugMode;
use crate::diagnostic_render;

/// Run the `run` command against `file`. Returns the process exit code.
///
/// `input_file` optionally overrides the process input (§7.1): when `None`,
/// stdin is read; when `Some(path)`, the file's contents are used. The input is
/// read lazily — only if the program contains a `read` expression — but for M6
/// we always read it upfront (a single read is the common case).
pub fn run(file: &str, input_file: Option<&str>, debug: DebugMode) -> anyhow::Result<i32> {
    let path = Path::new(file);
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(err) => {
            eprintln!("error: failed to read source file `{file}`: {err}");
            return Ok(2);
        }
    };

    let source = praxis_source::SourceMap::new();
    let id = source.intern(path, text.clone());

    // Front end: parse → resolve → infer.
    let parsed = praxis_parser::parse(id, &text);
    let mut diagnostics = parsed.diagnostics;
    let mut analysis = analyze_root(id, &parsed.tree);
    diagnostics.extend(analysis.diagnostics.clone());
    diagnostics.sort_by_key(|d| {
        let s = d.primary().span;
        (s.start(), s.end())
    });

    // Honesty gate: never JIT malformed input. If any language errors exist
    // (parse / type / lowering), report them and stop.
    let rendered = diagnostic_render::render_all(&source, &diagnostics);
    if rendered.has_errors() {
        diagnostic_render::write_to(&mut std::io::stderr(), &rendered)?;
        return Ok(1);
    }

    // Lower to typed HIR, then MIR, then JIT. HIR lowering may emit its own
    // `Y1xx` diagnostics (e.g. generic-fn-not-supported); surface those too.
    let root = match praxis_ast::SourceFile::cast(parsed.tree.clone()) {
        Some(r) => r,
        None => {
            eprintln!("error: internal — parse tree root is not a SOURCE_FILE");
            return Ok(1);
        }
    };
    let module = lower(id, &root, &mut analysis);
    if !module.diagnostics.is_empty() {
        let mut all = module.diagnostics.clone();
        all.sort_by_key(|d| {
            let s = d.primary().span;
            (s.start(), s.end())
        });
        let rendered = diagnostic_render::render_all(&source, &all);
        diagnostic_render::write_to(&mut std::io::stderr(), &rendered)?;
        return Ok(1);
    }

    // Monomorphization (WS8, §13.6): instantiate every polymorphic callee per
    // call site, between typed HIR and MIR. Produces a module of monomorphic
    // fns (one clone per generic callee + concrete type args); the MIR builder
    // then runs unchanged on it.
    let module = monomorphize(module, &analysis.names, &mut analysis.db);

    let mut funcs = lower_module(&module, &mut analysis.db);
    for f in &mut funcs {
        annotate(f);
    }

    let mut jit = match Jit::new() {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: could not initialize the JIT: {e}");
            return Ok(1);
        }
    };
    let ids = match jit.compile(&funcs, &analysis.db) {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("error: JIT compilation failed: {e}");
            return Ok(1);
        }
    };

    let main_id = match ids.get("main") {
        Some(id) => *id,
        None => {
            eprintln!("error: no `main` function to run");
            return Ok(1);
        }
    };

    // Execute. `main` takes no GcRef params beyond the hidden context.
    let mut runtime = Runtime::new();
    let mut ctx = runtime.context();

    // Read the process input (§7.10, M6). The first `read` expression lazily
    // reads this buffer; we read it once here and install it as `input_source`.
    // Empty input keeps the default (immortal Unit).
    let input_text = match input_file {
        Some(path) => std::fs::read_to_string(path).unwrap_or_default(),
        None => {
            // Read stdin if it's not a terminal (piped input); otherwise empty.
            use std::io::IsTerminal;
            if std::io::stdin().is_terminal() {
                String::new()
            } else {
                std::io::read_to_string(std::io::stdin()).unwrap_or_default()
            }
        }
    };
    if !input_text.is_empty() {
        let input_ref = runtime.alloc_text(&input_text);
        ctx.input_source = input_ref;
    }

    // SAFETY: `entry` was just finalized for `main_id`; the JIT outlives the
    // call. `main` takes one unused GcRef slot (the uniform calling convention
    // passes the context plus the declared params — zero here — but the entry
    // pointer type carries one placeholder slot).
    let entry: unsafe extern "C" fn(*mut RuntimeContext, GcRef) -> GcRef =
        unsafe { std::mem::transmute(jit.entry(main_id)) };
    let unit = runtime.alloc_unit();
    let result = unsafe { entry(&mut ctx as *mut RuntimeContext, unit) };

    if runtime.has_pending_fault() {
        let kind = runtime.fault();
        // Decide whether to enter the interactive crash REPL (§9.4, §9.6).
        // `always` or TTY `auto` enters the REPL; `never` or non-TTY `auto`
        // prints the noninteractive diagnostic and exits nonzero.
        if debug.wants_repl() {
            // Take the snapshot out of the runtime; the REPL owns it for its
            // lifetime. A missing snapshot (host-side fault before any debug
            // frame) degrades to the noninteractive render.
            if let Some(snapshot) = runtime.take_crash_snapshot() {
                // Show the fault line + §7.11 detail before the prompt, so the
                // user sees what happened (§9.4's banner).
                praxis_debugger::render::render_noninteractive(
                    &mut std::io::stderr(),
                    kind,
                    Some(&snapshot),
                    Some(runtime.parse_detail()),
                )?;
                let mut repl = praxis_debugger::repl::Repl::new(snapshot);
                let stdin = std::io::stdin();
                let mut stdin = stdin.lock();
                let stderr = std::io::stderr();
                let mut stderr = stderr.lock();
                repl.run(&mut stdin, &mut stderr);
            } else {
                praxis_debugger::render::render_noninteractive(
                    &mut std::io::stderr(),
                    kind,
                    None,
                    Some(runtime.parse_detail()),
                )?;
            }
        } else {
            praxis_debugger::render::render_noninteractive(
                &mut std::io::stderr(),
                kind,
                runtime.crash_snapshot(),
                Some(runtime.parse_detail()),
            )?;
        }
        // Keep the JIT alive through the print/REPL; drop after.
        drop(jit);
        return Ok(1);
    }

    // Print the result value through its descriptor (§11.4).
    let mut out = String::new();
    result.format(&mut out);
    println!("{out}");
    drop(jit);
    Ok(0)
}
