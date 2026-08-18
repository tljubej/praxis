//! The `praxis` command-line entry point.
//!
//! Wires the command surface: `run`, `check` and `lsp`.
//!
//! **The doc comments below are user-facing text.** clap renders each one into
//! `praxis --help`, so a note to the implementer put in one is printed to
//! somebody trying to learn the flag. The notes are worth keeping and they go
//! in a plain `//` comment beside the doc comment, where clap cannot reach
//! them.

mod breakpoint_host;
mod check;
mod color_mode;
mod debug_mode;
mod diagnostic_render;
mod exit_code;
mod run;
mod source_file;

use anyhow::Result;
use clap::{Parser, Subcommand};
pub use color_mode::ColorMode;
pub use debug_mode::DebugMode;

/// The Praxis command-line interface.
#[derive(Parser, Debug)]
#[command(
    name = "praxis",
    version,
    about = "The Praxis programming language compiler and tools.",
    long_about = "Praxis is a small, statically typed, garbage-collected language for \
                  Advent of Code-style puzzle solving.\n\n\
                  Homepage: https://github.com/tljubej/praxis"
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
    /// Parse, type-check, JIT-compile, and run the program.
    Run {
        /// The `.px` source file to run.
        file: String,
        /// Read the process input from this file instead of stdin.
        #[arg(long)]
        input: Option<String>,
        /// When to drop into the crash debugger on a runtime fault:
        /// `auto` (default) enters it iff stdin and stdout are a terminal;
        /// `always` forces it; `never` always prints the noninteractive
        /// diagnostic and exits nonzero.
        #[arg(long, default_value = "auto")]
        debug: DebugMode,
    },
    /// Run the front end (lex + parse + type-check) without executing.
    Check {
        /// The `.px` source file to check.
        file: String,
    },
    /// Start the language server over stdio. Speaks JSON-RPC LSP on
    /// stdin/stdout; not meant to be run by hand.
    Lsp {
        // The flag exists because the alternative is exiting 2 on an argument
        // the convention says is harmless, before a byte of protocol is
        // spoken, which every client reports as "the server crashed" rather
        // than as a bad flag.
        /// Accepted and ignored. Several LSP clients append `--stdio` to the
        /// server's argv to select a transport — `vscode-languageclient` does
        /// it whenever `TransportKind.stdio` is set, and a number of Neovim and
        /// Helix configurations pass it by hand. stdio is the only transport
        /// this server has, so the flag names what is already true.
        #[arg(long)]
        stdio: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // The runtime ABI version is checked once at startup so an inconsistent
    // build fails loudly instead of producing corrupt code.
    praxis_runtime::assert_abi_version();

    let exit = match cli.command {
        Command::Check { file } => check::run(&file, cli.color),
        Command::Run { file, input, debug } => run::run(&file, input.as_deref(), debug, cli.color),
        // `stdio` is the only transport, so the flag selects nothing.
        Command::Lsp { stdio: _ } => praxis_lsp::run(),
    }?;

    // One of `exit_code`'s three, except from `lsp`, which returns the LSP
    // protocol's own 0/1 (a clean `shutdown`/`exit` vs. a client that never
    // shut down) — the same two numbers, decided by a different rule.
    std::process::exit(exit);
}
