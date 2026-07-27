//! The `--debug` flag modes for `praxis run` (§9.6, M10).
//!
//! Controls whether a runtime fault drops into the interactive crash REPL or
//! prints the noninteractive diagnostic and exits:
//! - `auto` (default): enter the REPL iff stdin **and** stdout are a terminal.
//! - `always`: force the REPL even when not attached to a terminal.
//! - `never`: always print the noninteractive diagnostic and exit nonzero.

use std::io::IsTerminal;

/// The `--debug` mode (§9.6). Parsed by clap from the flag value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DebugMode {
    /// Enter the crash REPL iff stdin & stdout are a terminal (the default).
    #[default]
    Auto,
    /// Always enter the crash REPL, even when not a terminal.
    Always,
    /// Never enter the REPL; print the noninteractive diagnostic and exit.
    Never,
}

impl DebugMode {
    /// Decide whether to enter the interactive crash REPL given the current
    /// process's terminal state (§9.6). `Always` overrides; `Never` declines;
    /// `Auto` checks both stdin and stdout are TTYs.
    #[must_use]
    pub fn wants_repl(self) -> bool {
        match self {
            DebugMode::Always => true,
            DebugMode::Never => false,
            DebugMode::Auto => std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        }
    }
}

impl std::str::FromStr for DebugMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(DebugMode::Auto),
            "always" => Ok(DebugMode::Always),
            "never" => Ok(DebugMode::Never),
            other => Err(format!(
                "unknown --debug mode `{other}` (expected auto|always|never)"
            )),
        }
    }
}

impl std::fmt::Display for DebugMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            DebugMode::Auto => "auto",
            DebugMode::Always => "always",
            DebugMode::Never => "never",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_auto_always_never() {
        assert_eq!("auto".parse::<DebugMode>().unwrap(), DebugMode::Auto);
        assert_eq!("always".parse::<DebugMode>().unwrap(), DebugMode::Always);
        assert_eq!("never".parse::<DebugMode>().unwrap(), DebugMode::Never);
    }

    #[test]
    fn rejects_unknown_mode() {
        assert!("yes".parse::<DebugMode>().is_err());
        assert!("".parse::<DebugMode>().is_err());
    }

    #[test]
    fn always_wants_repl_never_does_not() {
        assert!(DebugMode::Always.wants_repl());
        assert!(!DebugMode::Never.wants_repl());
    }
}
