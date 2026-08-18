//! The `praxis run` command: load a `.px` file, run the full pipeline
//! (parse → analyze → typed HIR → MIR → Cranelift JIT → execute), and print the
//! result of the program's `main` function.
//!
//! Exit codes are [`crate::exit_code`]'s closed set: `OK` when the program ran
//! to completion with no fault, `FAILED` for a language error (parse / type /
//! lowering) *or* a runtime fault (overflow / division by zero / …), `USAGE`
//! when a source or `--input` file cannot be read. `run::run` prints its own
//! diagnostics and never returns `Err` for a user-facing problem, so the code
//! it returns is the final one.

use std::path::Path;

use praxis_ast::AstNode;
use praxis_codegen_cranelift::Jit;
use praxis_hir::{TypedItem, analyze_root, lower, mono::monomorphize};
use praxis_mir::{annotate, lower_module, verify};
use praxis_runtime::{Runtime, RuntimeContext};
use praxis_source::diagnostic::sort_by_position;
use praxis_typeck::TypeData;

use crate::breakpoint_host;
use crate::debug_mode::DebugMode;
use crate::{diagnostic_render, exit_code, source_file};

/// Run the `run` command against `file`. Returns the process exit code.
///
/// `input_file` optionally overrides the process input (§7.1): when `None`,
/// standard input is read **by the program's first `read`** and not before
/// (§7.10 — see [`lazy_stdin`]); when `Some(path)`, the file is read up front,
/// because a regular file cannot block and reporting an unreadable `--input`
/// before the program runs is worth more than the symmetry.
pub fn run(
    file: &str,
    input_file: Option<&str>,
    debug: DebugMode,
    color: crate::color_mode::ColorMode,
) -> anyhow::Result<i32> {
    let path = Path::new(file);
    let text = match source_file::read(file) {
        Ok(t) => t,
        Err(code) => return Ok(code),
    };

    let source = praxis_source::SourceMap::new();
    let id = source.intern(path, text.clone());

    // Front end: parse → resolve → infer.
    //
    // Spelled out here rather than through `praxis_lsp::query::Snapshot`, which
    // is where ADR-097 put this sequence and where `praxis check` reads it from:
    // `run` goes on to lower the tree, and `Snapshot::parse` is crate-private so
    // that a `SyntaxNode` never crosses the crate boundary (ADR-095). The order
    // is the shared one either way — `sort_by_position` is the same comparator
    // `Snapshot::diagnostics` sorts with.
    let parsed = praxis_parser::parse(id, &text);
    let mut diagnostics = parsed.diagnostics;
    let mut analysis = analyze_root(id, &parsed.tree);
    diagnostics.extend(analysis.diagnostics.clone());
    sort_by_position(&mut diagnostics);

    // Honesty gate: never JIT malformed input. If any language errors exist
    // (parse / type / lowering), report them and stop.
    let rendered = diagnostic_render::render_all(&source, &diagnostics, color.palette());
    if rendered.has_errors() {
        diagnostic_render::write_to(&mut std::io::stderr(), &rendered)?;
        return Ok(exit_code::FAILED);
    }

    // Lower to typed HIR, then MIR, then JIT. HIR lowering may emit its own
    // `Y1xx` diagnostics (e.g. generic-fn-not-supported); surface those too.
    let root = match praxis_ast::SourceFile::cast(parsed.tree.clone()) {
        Some(r) => r,
        None => {
            eprintln!("error: internal — parse tree root is not a SOURCE_FILE");
            return Ok(exit_code::FAILED);
        }
    };
    let module = lower(id, &root, &mut analysis);
    if !module.diagnostics.is_empty() {
        let mut all = module.diagnostics.clone();
        sort_by_position(&mut all);
        let rendered = diagnostic_render::render_all(&source, &all, color.palette());
        diagnostic_render::write_to(&mut std::io::stderr(), &rendered)?;
        return Ok(exit_code::FAILED);
    }

    // Which function the host calls, and its declared return type — both read
    // before monomorphization consumes the module.
    //
    // A file's top-level statements are its program (ADR-067), so `<entry>` is
    // the answer when the file has any. It always returns `Unit`, which is also
    // the rule that keeps `out(…)` at top level from printing twice: a
    // `Unit`-returning entry has no answer value to print, so the host prints a
    // result only for the `fn main` fallback and only when it is non-`Unit`.
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

    // Monomorphization (§13.6): instantiate every polymorphic callee per
    // call site, between typed HIR and MIR. Produces a module of monomorphic
    // fns (one clone per generic callee + concrete type args); the MIR builder
    // then runs unchanged on it.
    let module = monomorphize(module, &analysis.names, &mut analysis.db);

    let mut funcs = lower_module(&module, &mut analysis.db);
    for f in &mut funcs {
        annotate(f);
        // A failure here is a compiler bug, never a program error, so it is
        // reported as one and no code is generated from it.
        if let Err(errs) = verify(f) {
            eprintln!("internal error: {}", praxis_mir::verify::report(&errs));
            return Ok(exit_code::FAILED);
        }
    }

    let mut jit = match Jit::new() {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: could not initialize the JIT: {e}");
            return Ok(exit_code::FAILED);
        }
    };
    let ids = match jit.compile(&funcs, &mut analysis.db) {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("error: JIT compilation failed: {e}");
            return Ok(exit_code::FAILED);
        }
    };

    let main_id = match entry_name.and_then(|name| ids.get(name)) {
        Some(id) => *id,
        None => {
            eprintln!("error: no statements to run and no `main` function");
            return Ok(exit_code::FAILED);
        }
    };

    // Execute. `main` takes no GcRef params beyond the hidden context.
    let mut runtime = Runtime::new();
    let mut ctx = runtime.context();

    // The process input (§7.10). An I/O failure is reported, never laundered
    // into empty input: a program that reads a missing `--input` file would
    // otherwise "succeed" against input the user never supplied, and a
    // truncated read would silently produce a wrong answer. Same exit code
    // (`exit_code::USAGE`) as an unreadable source file.
    //
    // `--input FILE` is read here, before the program runs. A regular file
    // cannot block, and an unreadable one is worth reporting before any output
    // is printed.
    //
    // **Standard input is not.** §7.10: "The first `read` lazily reads standard
    // input once into an immutable GC-managed source buffer." Reading it here
    // would consume stdin for a program with no `read` in it, and against an
    // open pipe — a terminal, a CI harness holding the descriptor — `praxis
    // run` would block forever waiting for an EOF nobody is going to send. The
    // reader below is installed, not called; `praxis_get_input` calls it the
    // one time, from the program's first `read`.
    //
    // The `Text` is installed unconditionally, a zero-byte file included: empty
    // input is input, and the rule and its reasons are stated once, at
    // `praxis_get_input` (ADR-087). The CLI's own decision is only the one
    // above — `--input` is eager, standard input is lazy.
    match input_file {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(t) => {
                let input_ref = runtime.alloc_text(&t);
                ctx.input_source = input_ref;
                lazy_stdin::record(t);
            }
            Err(err) => {
                eprintln!("error: failed to read input file `{path}`: {err}");
                return Ok(exit_code::USAGE);
            }
        },
        None => praxis_runtime::install_input_reader(lazy_stdin::read),
    }

    // Arm the `:bp` stops (§9.8). This has to happen before the call below and
    // not inside it: the handler is reached from generated code, several native
    // frames under this one, so what it renders with is state it finds rather
    // than state it is passed. `analysis.db` is the database codegen just
    // compiled against, which is what makes a local's positional `type_id` mean
    // anything.
    //
    // A program with no marker in it never calls the wrapper, so this costs a
    // clone of the type database and nothing else.
    let source_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    breakpoint_host::install(&analysis.db, &text, &source_name, debug, color);

    // SAFETY: `entry` was just finalized for `main_id`; the JIT outlives the
    // call. `main` declares no parameters, so the entry takes the context and
    // nothing else — the shape `abi_signature` emitted for it.
    let entry: praxis_debugger::session::MainEntry =
        unsafe { std::mem::transmute(jit.entry(main_id)) };
    let result = unsafe { entry(&mut ctx as *mut RuntimeContext) };

    // The run is over, so no later stop has a program to stop. Disarming here
    // rather than only on the fault path means the crash debugger's `restart`
    // (§9.7) cannot fire a handler into the terminal the debugger is holding.
    breakpoint_host::disarm();

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
                // Hand the live compile/run state to the REPL as a
                // `DebugSession`, so `p EXPR`/`source`/`restart`/`reload` can
                // reach the Jit/Runtime/TypeDb/source/input. The snapshot was
                // taken out of `runtime` above, so the two are decoupled.
                // SAFETY: `main_entry` was just transmuted from a finalized
                // JIT entry for `main_id`; the `jit` outlives the REPL (it
                // moves into the session and is dropped with it).
                let session = praxis_debugger::session::DebugSession {
                    jit,
                    main_entry: entry,
                    runtime,
                    analysis,
                    source_text: text.clone(),
                    source_path: path.to_path_buf(),
                    // What the program actually read — empty if it never
                    // evaluated a `read`. The session re-installs it directly on
                    // each re-run, which is §9.7's guarantee that a restart sees
                    // the same input; `clear_input_reader` below is what stops a
                    // second read of an exhausted stdin.
                    input_text: lazy_stdin::text(),
                    eval_generation: std::rc::Rc::new(praxis_codegen_cranelift::Generation::new()),
                };
                // The session owns the input from here: every re-run
                // installs `input_text` directly (§9.7 — a restart must see
                // the same input), so the reader must not fire again against a
                // stdin that is now at EOF.
                praxis_runtime::clear_input_reader();
                let mut repl = praxis_debugger::repl::Repl::new_session(snapshot, session);
                // The full-screen debugger when there is a terminal to take over,
                // the line REPL otherwise. `--debug=always` in a script and a
                // piped command list both land in the second branch, and must:
                // the TUI needs keystrokes to read and a screen to draw on, and
                // with neither it would show a frozen screen against EOF.
                //
                // The noninteractive report above has already been written to
                // stderr, i.e. to the *primary* screen. The TUI draws on the
                // alternate screen, so quitting it restores that report — the
                // crash stays in the scrollback instead of vanishing with the UI.
                if praxis_debugger::tui::should_use_tui() {
                    let stop = praxis_debugger::tui::Stop::Fault(kind, message.clone());
                    let tui = praxis_debugger::tui::Tui::new(repl, stop);
                    (repl, _) = praxis_debugger::tui::run(tui)?;
                } else {
                    let stdin = std::io::stdin();
                    let mut stdin = stdin.lock();
                    let stderr = std::io::stderr();
                    let mut stderr = stderr.lock();
                    let _ = repl.run(&mut stdin, &mut stderr);
                }
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
        return Ok(exit_code::FAILED);
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
    // and the parser plans. `Runtime::teardown` mints the proof both demand, so
    // this cannot be written the other way round (hazard H15).
    let proof = runtime.teardown();
    praxis_runtime::retire_parser_plans(&proof);
    jit.retire(proof);
    Ok(exit_code::OK)
}

/// Standard input, read by the program's **first** `read` and not before
/// (§7.10).
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
    /// not asked for anything.
    ///
    /// An I/O failure exits the process with [`crate::exit_code::USAGE`]. It
    /// cannot be returned instead: the runtime's reader is infallible by
    /// design, because what an unreadable stdin *means* is the host's question.
    /// Laundering it into empty input is the one thing that would be wrong — a
    /// truncated read would silently produce a wrong answer.
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
                std::process::exit(crate::exit_code::USAGE);
            }
        }
    }
}
