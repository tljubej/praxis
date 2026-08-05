//! The `praxis` command-line entry point.
//!
//! Wires the §3.1 command surface (`run`, `check`, `watch`, `repl`, `lsp`).
//! `run`, `check` and `lsp` do real work; `watch` and `repl` are honest stubs.
//!
//! **The doc comments below are user-facing text.** clap renders each one into
//! `praxis --help`, so a §-reference or a milestone marker in one is a note to
//! the implementer printed to somebody trying to learn the flag. The notes are
//! worth keeping and they go in a plain `//` comment beside the doc comment,
//! where clap cannot reach them (REP-73).

mod check;
mod color_mode;
mod debug_mode;
mod diagnostic_render;
mod run;

use anyhow::Result;
use clap::{Parser, Subcommand};
pub use color_mode::ColorMode;
pub use debug_mode::DebugMode;

/// The Praxis command-line interface.
///
/// See `praxis_technical_design.md` §3.1 for the full command surface.
#[derive(Parser, Debug)]
#[command(
    name = "praxis",
    version,
    about = "The Praxis programming language compiler and tools.",
    long_about = "Praxis is a small, statically typed, garbage-collected language for \
                  Advent of Code-style puzzle solving. See praxis_technical_design.md."
)]
struct Cli {
    /// When to color diagnostic output: `auto` (default) colors iff stderr is a
    /// terminal; `always` forces color; `never` emits plain text.
    #[arg(long, default_value = "auto", global = true)]
    color: ColorMode,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    // §3.1, M4+.
    /// Parse, type-check, JIT-compile, and run the program.
    Run {
        /// The `.px` source file to run.
        file: String,
        // §7.1, M6.
        /// Read the process input from this file instead of stdin.
        #[arg(long)]
        input: Option<String>,
        // §9.6, M10.
        /// When to drop into the crash debugger on a runtime fault:
        /// `auto` (default) enters the REPL iff stdin & stdout are a terminal;
        /// `always` forces the REPL; `never` always prints the noninteractive
        /// diagnostic and exits nonzero.
        #[arg(long, default_value = "auto")]
        debug: DebugMode,
    },
    /// Run the front end (lex + parse + type-check) without executing.
    Check {
        /// The `.px` source file to check.
        file: String,
    },
    // §19 M-later. The design document's §3.1 invocation shows `--input`, and
    // this variant declares only `file`: settle that when `watch` is built,
    // rather than shipping a flag whose behaviour nothing has decided.
    /// Keep the program and input alive, recompile on source changes. (Not implemented yet.)
    Watch {
        /// The `.px` source file to watch.
        file: String,
    },
    // No milestone. §19 does not schedule it.
    /// Start an ordinary interactive REPL session. (Not implemented yet.)
    Repl,
    // §15, M11.
    /// Start the language server over stdio. Speaks JSON-RPC LSP on
    /// stdin/stdout; not meant to be run by hand.
    Lsp {
        /// Accepted and ignored. Several LSP clients append `--stdio` to the
        /// server's argv to select a transport — `vscode-languageclient` does
        /// it whenever `TransportKind.stdio` is set, and a number of Neovim and
        /// Helix configurations pass it by hand. stdio is the **only** transport
        /// this server has, so the flag names what is already true.
        ///
        /// It is here because the alternative is exiting 2 on an argument the
        /// convention says is harmless, before a byte of protocol is spoken —
        /// which is what happened, and which every client reports as
        /// "the server crashed" rather than as a bad flag.
        #[arg(long)]
        stdio: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // The runtime ABI version is checked once at startup so an inconsistent
    // build fails loudly instead of producing corrupt code (§11.6).
    praxis_runtime::assert_abi_version();

    let exit = match cli.command {
        Command::Check { file } => check::run(&file, cli.color),
        Command::Run { file, input, debug } => run::run(&file, input.as_deref(), debug, cli.color),
        Command::Watch { file } => not_implemented("watch", Some(&file)),
        Command::Repl => not_implemented("repl", None),
        // `stdio` is the only transport, so the flag selects nothing.
        Command::Lsp { stdio: _ } => praxis_lsp::run(),
    }?;

    std::process::exit(exit);
}

/// Emit an honest "not implemented" message and return the "usage error"
/// exit code (2). Never silently no-op a command.
///
/// **No milestone number**, which is what it used to end with. Both call sites
/// passed a hardcoded `0`, so the message pointed at a milestone that completed
/// long ago — `watch` is §19 M-later and `repl` is scheduled nowhere. A number
/// nobody maintains is worse than no number: it reads as a commitment and it is
/// a typo that never went red (REP-73).
fn not_implemented(name: &str, file: Option<&str>) -> Result<i32> {
    let where_ = match file {
        Some(f) => format!(" `{f}`"),
        None => String::new(),
    };
    eprintln!("error: `praxis {name}`{where_} is not implemented yet");
    Ok(2)
}
