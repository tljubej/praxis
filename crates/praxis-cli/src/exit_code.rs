//! The process exit codes `praxis` returns.
//!
//! A closed set of three, and an OS contract: `crates/praxis-cli/tests` spawns
//! the binary and asserts the number, so these values can be named but never
//! renumbered. The split between [`FAILED`] and [`USAGE`] is the load-bearing
//! part — a script can tell "your program has a problem" from "I could not
//! start the job" without parsing stderr.
//!
//! `check::run` and `run::run` print their own diagnostics and never return
//! `Err` for a user-facing problem (a missing file is reported where it is
//! found, not via `anyhow`), so the code each returns is the final one.

/// No errors: the front end found none, and the program — if one ran — ran to
/// completion without faulting.
pub const OK: i32 = 0;

/// The program, or the compiler reading it, failed: one or more language errors
/// (parse / type / lowering) were reported, or the program faulted at runtime
/// (overflow / division by zero / …). An internal compiler error lands here
/// too; it is reported as one, and nothing is generated from it.
pub const FAILED: i32 = 1;

/// The CLI could not start the job: a source or `--input` file that cannot be
/// read. Never a verdict on the program, because none of it ran.
pub const USAGE: i32 = 2;
