//! Opening the `.px` file named on the command line.
//!
//! `check` and `run` both begin here, message text included, so a mistyped path
//! cannot read differently depending on which of the two commands was typed.

use crate::exit_code;

/// Read `file` as UTF-8, reporting an unreadable one on stderr.
///
/// `Err` carries the exit code to return rather than an `anyhow::Error`: an
/// unreadable file is a user-facing problem the command reports itself, so it
/// must not travel up the `anyhow` channel and be printed a second time. The
/// code is [`exit_code::USAGE`] — nothing of the program ran, so this is not a
/// verdict on it.
pub fn read(file: &str) -> Result<String, i32> {
    std::fs::read_to_string(file).map_err(|err| {
        eprintln!("error: failed to read source file `{file}`: {err}");
        exit_code::USAGE
    })
}
