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
use praxis_hir::{analyze_root, lower, mono::monomorphize, TypedItem};
use praxis_mir::{annotate, lower_module, verify};
use praxis_runtime::{Runtime, RuntimeContext};
use praxis_types::TypeData;

use crate::debug_mode::DebugMode;
use crate::diagnostic_render;

/// Run the `run` command against `file`. Returns the process exit code.
///
/// `input_file` optionally overrides the process input (§7.1): when `None`,
/// standard input is read **by the program's first `read`** and not before
/// (§7.10, REP-51 — see [`lazy_stdin`]); when `Some(path)`, the file is read
/// up front, because a regular file cannot block and reporting an unreadable
/// `--input` before the program runs is worth more than the symmetry.
pub fn run(
    file: &str,
    input_file: Option<&str>,
    debug: DebugMode,
    color: crate::color_mode::ColorMode,
) -> anyhow::Result<i32> {
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
    let rendered = diagnostic_render::render_all(&source, &diagnostics, color.palette());
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
        let rendered = diagnostic_render::render_all(&source, &all, color.palette());
        diagnostic_render::write_to(&mut std::io::stderr(), &rendered)?;
        return Ok(1);
    }

    // Which function the host calls, and its declared return type — both read
    // before monomorphization consumes the module.
    //
    // A file's top-level statements are its program (REP-19, ADR-067), so
    // `<entry>` is the answer when the file has any. It always returns `Unit`,
    // which is also the rule that keeps `out(…)` at top level from printing
    // twice: a `Unit`-returning entry has no answer value to print, so the host
    // prints a result only for the `fn main` fallback and only when it is
    // non-`Unit`.
    let entry_name = praxis_hir::entry_point(|name| {
        module
            .items
            .iter()
            .any(|item| matches!(item, TypedItem::Fn(f) if f.name == name))
    });
    let main_return_type = entry_name
        .and_then(|name| {
            module.items.iter().find_map(|item| match item {
                TypedItem::Fn(f) if f.name == name => Some(f.return_type),
                _ => None,
            })
        })
        .unwrap_or_else(|| analysis.db.unit());

    // Monomorphization (WS8, §13.6): instantiate every polymorphic callee per
    // call site, between typed HIR and MIR. Produces a module of monomorphic
    // fns (one clone per generic callee + concrete type args); the MIR builder
    // then runs unchanged on it.
    let module = monomorphize(module, &analysis.names, &mut analysis.db);

    let mut funcs = lower_module(&module, &mut analysis.db);
    for f in &mut funcs {
        annotate(f);
        // MIR-10. A failure here is a compiler bug, never a program error, so
        // it is reported as one and no code is generated from it.
        if let Err(errs) = verify(f) {
            eprintln!("internal error: {}", praxis_mir::verify::report(&errs));
            return Ok(1);
        }
    }

    let mut jit = match Jit::new() {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: could not initialize the JIT: {e}");
            return Ok(1);
        }
    };
    let ids = match jit.compile(&funcs, &mut analysis.db) {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("error: JIT compilation failed: {e}");
            return Ok(1);
        }
    };

    let main_id = match entry_name.and_then(|name| ids.get(name)) {
        Some(id) => *id,
        None => {
            eprintln!("error: no statements to run and no `main` function");
            return Ok(1);
        }
    };

    // Execute. `main` takes no GcRef params beyond the hidden context.
    let mut runtime = Runtime::new();
    let mut ctx = runtime.context();

    // The process input (§7.10). An I/O failure is reported, never laundered
    // into empty input: a program that reads a missing `--input` file would
    // otherwise "succeed" against input the user never supplied, and a
    // truncated read would silently produce a wrong answer. Same exit code
    // (2, usage/I-O) as an unreadable source file.
    //
    // `--input FILE` is read here, before the program runs. A regular file
    // cannot block, and an unreadable one is worth reporting before any output
    // is printed.
    //
    // **Standard input is not.** §7.10: "The first `read` lazily reads standard
    // input once into an immutable GC-managed source buffer." Reading it here
    // meant a program with no `read` in it consumed stdin anyway, and against
    // an open pipe — a terminal, a CI harness holding the descriptor — `praxis
    // run` blocked forever waiting for an EOF nobody was going to send
    // (REP-51). The reader below is installed, not called; `praxis_get_input`
    // calls it the one time, from the program's first `read`.
    //
    // **A zero-byte file is input, not the absence of input** (REP-60). The
    // buffer used to be installed only `if !t.is_empty()`, so `--input` on an
    // empty file left `ctx.input_source` at the immortal Unit — and `Input::new`
    // answers `None` for a non-Text source and takes the "no detail was
    // recorded" path, so every `read` faulted with `input parse mismatch` and
    // no offset, no `expected` and no `actual`. Nothing about "the file is
    // empty" was in the message. Installing a zero-length `Text` unconditionally
    // makes `read` run against a zero-length buffer, which the constructors
    // already have answers for: `lines(int)` over it is `[]` under
    // `split_lines`'s own rule, and a constructor that requires content faults
    // at offset `0..0` naming what it expected.
    match input_file {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(t) => {
                let input_ref = runtime.alloc_text(&t);
                ctx.input_source = input_ref;
                lazy_stdin::record(t);
            }
            Err(err) => {
                eprintln!("error: failed to read input file `{path}`: {err}");
                return Ok(2);
            }
        },
        None => praxis_runtime::install_input_reader(lazy_stdin::read),
    }

    // SAFETY: `entry` was just finalized for `main_id`; the JIT outlives the
    // call. `main` declares no parameters, so the entry takes the context and
    // nothing else — the shape `abi_signature` emitted for it.
    let entry: praxis_debugger::session::MainEntry =
        unsafe { std::mem::transmute(jit.entry(main_id)) };
    let result = unsafe { entry(&mut ctx as *mut RuntimeContext) };

    if runtime.has_pending_fault() {
        let kind = runtime.fault();
        // The message a `panic`/`assert` carried (§9.1). Copied out now: the
        // interactive path moves `runtime` into the `DebugSession` below, and
        // the message has to survive that move to be rendered.
        let message = runtime.fault_message().map(str::to_string);
        // Decide whether to enter the interactive crash REPL (§9.4, §9.6).
        // `always` or TTY `auto` enters the REPL; `never` or non-TTY `auto`
        // prints the noninteractive diagnostic and exits nonzero.
        if debug.wants_repl() {
            // Take the snapshot out of the runtime; the REPL owns it for its
            // lifetime. A missing snapshot (host-side fault before any debug
            // frame) degrades to the noninteractive render.
            if let Some(snapshot) = runtime.take_crash_snapshot() {
                // Show the fault line + §7.11 detail before the prompt, so the
                // user sees what happened (§9.4's banner). Enrich the locals
                // render with the live TypeDb + source text so temps show their
                // type and materializing expression.
                let ctx = praxis_debugger::render::RenderCtx::new(&analysis.db, &text);
                praxis_debugger::render::render_noninteractive(
                    &mut std::io::stderr(),
                    kind,
                    message.as_deref(),
                    Some(&snapshot),
                    Some(runtime.parse_detail()),
                    color.palette(),
                    &ctx,
                )?;
                // M10b: hand the live compile/run state to the REPL as a
                // `DebugSession`, so `p EXPR`/`source`/`restart`/`reload` can
                // reach the Jit/Runtime/TypeDb/source/input. The snapshot was
                // taken out of `runtime` above, so the two are decoupled.
                // SAFETY: `main_entry` was just transmuted from a finalized
                // JIT entry for `main_id`; the `jit` outlives the REPL (it
                // moves into the session and is dropped with it).
                let session = praxis_debugger::session::DebugSession {
                    jit,
                    main_entry: entry,
                    func_ids: ids,
                    runtime,
                    analysis,
                    source_text: text.clone(),
                    source_path: path.to_path_buf(),
                    // What the program actually read — empty if it never
                    // evaluated a `read` (REP-51). The session re-installs it
                    // directly on each re-run, which is §9.7's guarantee that a
                    // restart sees the same input; `clear_input_reader` below
                    // is what stops a second read of an exhausted stdin.
                    input_text: lazy_stdin::text(),
                    input_path: input_file.map(Path::new).map(std::path::Path::to_path_buf),
                    eval_generation: std::rc::Rc::new(praxis_codegen_cranelift::Generation::new()),
                };
                // The session owns the input from here: every re-run
                // installs `input_text` directly (§9.7 — a restart must see
                // the same input), so the reader must not fire again against a
                // stdin that is now at EOF.
                praxis_runtime::clear_input_reader();
                let mut repl = praxis_debugger::repl::Repl::new_session(snapshot, session);
                let stdin = std::io::stdin();
                let mut stdin = stdin.lock();
                let stderr = std::io::stderr();
                let mut stderr = stderr.lock();
                repl.run(&mut stdin, &mut stderr);
                // Drop the snapshot, then the heap, then the JIT generations
                // its objects pointed into (F13, H15). `teardown` is what makes
                // that order a compile-time obligation.
                if let Some(session) = repl.into_session() {
                    session.teardown();
                }
            } else {
                let ctx = praxis_debugger::render::RenderCtx::new(&analysis.db, &text);
                praxis_debugger::render::render_noninteractive(
                    &mut std::io::stderr(),
                    kind,
                    message.as_deref(),
                    None,
                    Some(runtime.parse_detail()),
                    color.palette(),
                    &ctx,
                )?;
                jit.retire(runtime.teardown());
            }
        } else {
            let ctx = praxis_debugger::render::RenderCtx::new(&analysis.db, &text);
            praxis_debugger::render::render_noninteractive(
                &mut std::io::stderr(),
                kind,
                message.as_deref(),
                runtime.crash_snapshot(),
                Some(runtime.parse_detail()),
                color.palette(),
                &ctx,
            )?;
            jit.retire(runtime.teardown());
        }
        return Ok(1);
    }

    // Print the result value through its descriptor (§11.4) — but only when
    // `main` returns a non-`Unit` type. A `Unit`-returning `main` has no answer
    // value (its output is whatever `out(...)` wrote), so printing a result
    // line would only echo spurious noise like the last `out` argument.
    let main_returns_unit = matches!(
        analysis.db.data(analysis.db.follow(main_return_type)),
        TypeData::Unit
    );
    if !main_returns_unit {
        let mut out = String::new();
        result.format(&mut out);
        println!("{out}");
    }
    // The run is over and `result` has been rendered: drop the heap, then
    // reclaim the arenas its objects pointed into — the JIT generation (F13)
    // and the parser plans (IP-12). `Runtime::teardown` mints the proof both
    // demand, so this cannot be written the other way round (hazard H15).
    let proof = runtime.teardown();
    praxis_runtime::retire_parser_plans(&proof);
    jit.retire(proof);
    Ok(0)
}

/// Standard input, read by the program's **first** `read` and not before
/// (§7.10, REP-51).
///
/// The runtime takes a plain `fn` — it is stored across the ABI boundary and
/// called from generated code's stack, so it carries no captured state — which
/// is why the source and the result live in thread-locals here rather than in a
/// closure. The runtime is single-threaded (§12.1) and `praxis run` runs one
/// program per process, so there is one of each.
///
/// [`record`] exists for the `--input FILE` path, which is still read up front:
/// the crash debugger's session needs the text the program actually saw, and it
/// needs it from one place regardless of where the input came from.
mod lazy_stdin {
    use std::cell::RefCell;

    thread_local! {
        /// What the program read, for the crash debugger's re-runs (§9.7).
        /// Empty until something reads, which for a `read`-free program is
        /// never — and empty is then the truth.
        static TEXT: RefCell<String> = const { RefCell::new(String::new()) };
    }

    /// The input the program has seen so far.
    pub(super) fn text() -> String {
        TEXT.with(|slot| slot.borrow().clone())
    }

    /// Record an input the host read itself (the `--input FILE` path).
    pub(super) fn record(input: String) {
        TEXT.with(|slot| *slot.borrow_mut() = input);
    }

    /// Read standard input to EOF, once. Installed as the runtime's
    /// [`praxis_runtime::InputReader`]; the runtime calls it from the first
    /// `read` a program evaluates, and never otherwise.
    ///
    /// A terminal stdin reads as empty rather than blocking on a human who was
    /// not asked for anything — the same rule the eager read used, kept here.
    ///
    /// An I/O failure exits the process with the same message and the same
    /// code (2, usage/I-O) the eager read used. It cannot be returned instead:
    /// the runtime's reader is infallible by design, because what an unreadable
    /// stdin *means* is the host's question. Laundering it into empty input is
    /// the one thing that would be wrong — a truncated read would silently
    /// produce a wrong answer.
    pub(super) fn read() -> Vec<u8> {
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() {
            return Vec::new();
        }
        match std::io::read_to_string(std::io::stdin()) {
            Ok(t) => {
                let bytes = t.as_bytes().to_vec();
                record(t);
                bytes
            }
            Err(err) => {
                eprintln!("error: failed to read input from stdin: {err}");
                std::process::exit(2);
            }
        }
    }
}
