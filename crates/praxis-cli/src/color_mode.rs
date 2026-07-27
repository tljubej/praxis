//! The `--color` flag: when to emit ANSI styling in diagnostics.
//!
//! Mirrors `--debug`'s mode enum. The compiler's diagnostic renderer and the
//! crash debugger both honor it:
//! - `auto` (default): style output iff stderr is a terminal.
//! - `always`: always style, even when piped.
//! - `never`: never style (plain text; the form snapshot tests assert).
//!
//! `praxis-source` stays dependency-free; this crate resolves `auto` to a
//! concrete yes/no by checking the terminal, then hands a plain [`Palette`] down.

use praxis_source::style::{ColorMode as SourceColorMode, Palette};
use std::io::IsTerminal;

/// The `--color` mode. Parsed by clap from the flag value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorMode {
    /// Style iff stderr is a terminal (the default).
    #[default]
    Auto,
    /// Always style, regardless of terminal detection.
    Always,
    /// Never style; emit plain text.
    Never,
}

impl ColorMode {
    /// Resolve to the `praxis-source` color mode (for `should_style` checks).
    #[must_use]
    pub fn to_source(self) -> SourceColorMode {
        match self {
            ColorMode::Auto => SourceColorMode::Auto,
            ColorMode::Always => SourceColorMode::Always,
            ColorMode::Never => SourceColorMode::Never,
        }
    }

    /// Resolve to a concrete [`Palette`] by checking the terminal for `Auto`.
    #[must_use]
    pub fn palette(self) -> Palette {
        let enabled = match self {
            ColorMode::Never => false,
            ColorMode::Always => true,
            ColorMode::Auto => std::io::stderr().is_terminal(),
        };
        Palette::from_enabled(enabled)
    }
}

impl std::str::FromStr for ColorMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(ColorMode::Auto),
            "always" => Ok(ColorMode::Always),
            "never" => Ok(ColorMode::Never),
            other => Err(format!(
                "unknown --color mode `{other}` (expected auto|always|never)"
            )),
        }
    }
}

impl std::fmt::Display for ColorMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ColorMode::Auto => "auto",
            ColorMode::Always => "always",
            ColorMode::Never => "never",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_auto_always_never() {
        assert_eq!("auto".parse::<ColorMode>().unwrap(), ColorMode::Auto);
        assert_eq!("always".parse::<ColorMode>().unwrap(), ColorMode::Always);
        assert_eq!("never".parse::<ColorMode>().unwrap(), ColorMode::Never);
    }

    #[test]
    fn rejects_unknown_mode() {
        assert!("yes".parse::<ColorMode>().is_err());
        assert!("".parse::<ColorMode>().is_err());
    }

    #[test]
    fn never_palette_is_plain_always_is_styled() {
        assert!(!ColorMode::Never.palette().is_styled());
        assert!(ColorMode::Always.palette().is_styled());
    }
}
