//! The `praxis` command-line entry point.
//!
//! Wires the §3.1 command surface (`run`, `check`, `watch`, `repl`, `lsp`).
//! For Milestone 0 only `check` does real work; the rest are honest stubs that
//! report which milestone will implement them.

mod check;
mod debug_mode;
mod diagnostic_render;
mod run;

use anyhow::Result;
use clap::{Parser, Subcommand};
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
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Parse, type-check, JIT-compile, and run the program. (Milestone 4+)
    Run {
        /// The `.px` source file to run.
        file: String,
        /// Read the process input from this file instead of stdin (§7.1, M6).
        #[arg(long)]
        input: Option<String>,
        /// When to drop into the crash debugger on a runtime fault (§9.6, M10):
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
    /// Keep the program and input alive, recompile on source changes. (Later milestone)
    Watch {
        /// The `.px` source file to watch.
        file: String,
    },
    /// Start an ordinary interactive REPL session. (Later milestone)
    Repl,
    /// Start the language server over stdio. (Milestone 11)
    Lsp,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // The runtime ABI version is checked once at startup so an inconsistent
    // build fails loudly instead of producing corrupt code (§11.6).
    praxis_runtime::assert_abi_version();

    let exit = match cli.command {
        Command::Check { file } => check::run(&file),
        Command::Run { file, input, debug } => run::run(&file, input.as_deref(), debug),
        Command::Watch { file } => not_implemented("watch", Some(&file), 0),
        Command::Repl => not_implemented("repl", None, 0),
        Command::Lsp => not_implemented("lsp", None, 11),
    }?;

    std::process::exit(exit);
}

/// Emit an honest "not implemented" message and return the "usage error"
/// exit code (2). Never silently no-op a command.
fn not_implemented(name: &str, file: Option<&str>, milestone: u32) -> Result<i32> {
    let where_ = match file {
        Some(f) => format!(" `{f}`"),
        None => String::new(),
    };
    eprintln!(
        "error: `praxis {name}`{where_} is not implemented yet (planned for Milestone {milestone})"
    );
    Ok(2)
}
