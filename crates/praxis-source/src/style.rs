//! Terminal styling: a tiny, dependency-free ANSI palette.
//!
//! `praxis-source` is the workspace's leaf crate (ADR-003) and must not pull in
//! any external dependency, so the palette is built from raw ANSI escape codes —
//! no `colored`, `nu-ansi-term`, or `yansi`. The output is identical to what
//! those crates produce; it is just spelled out here.
//!
//! ## Design
//!
//! [`ColorMode`] gates whether styling is emitted at all:
//! - `Never` (the default for the `Renderer` and for all snapshot tests) emits
//!   plain text, so diagnostics stay byte-stable across test runs.
//! - `Always` emits ANSI unconditionally.
//! - `Auto` emits ANSI only when stderr is a terminal (the CLI default).
//!
//! [`Palette`] is the set of (foreground, weight) pairs the diagnostic renderer
//! and the crash debugger use. `Palette::plain()` is a no-op palette; the styled
//! palette matches rustc's conventions: errors red & bold, warnings yellow &c.

/// Whether, and when, to emit ANSI styling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorMode {
    /// Never emit ANSI; produce plain text. The default for the `Renderer` and
    /// for snapshot tests so output is byte-stable.
    #[default]
    Never,
    /// Always emit ANSI, regardless of terminal detection.
    Always,
    /// Emit ANSI only when stderr is an interactive terminal.
    Auto,
}

impl ColorMode {
    /// Resolve to a plain yes/no for "should we style?", checking the terminal
    /// for `Auto`. `Never` and `Always` need no I/O check.
    #[must_use]
    pub fn should_style(self) -> bool {
        match self {
            ColorMode::Never => false,
            ColorMode::Always => true,
            ColorMode::Auto => std::io::stderr().is_terminal(),
        }
    }
}

// We need IsTerminal only for Auto; pull it in unconditionally (it's in std).
use std::io::IsTerminal;

/// An ANSI SGR (Select Graphic Rendition) code. Stored as the numeric parameter
/// so the palette is data, not a method per style. The full standard set is
/// named here even though only some are used today, so adding a style later is
/// a one-line change.
#[derive(Clone, Copy, Debug)]
struct Sgr(u8);

#[allow(dead_code)]
impl Sgr {
    const RESET: Sgr = Sgr(0);
    const BOLD: Sgr = Sgr(1);
    const DIM: Sgr = Sgr(2);
    const RED: Sgr = Sgr(31);
    const GREEN: Sgr = Sgr(32);
    const YELLOW: Sgr = Sgr(33);
    const BLUE: Sgr = Sgr(34);
    const MAGENTA: Sgr = Sgr(35);
    const CYAN: Sgr = Sgr(36);
}

/// The semantic styles a diagnostic uses. Each maps to zero or more SGR codes.
/// Keeping these semantic (rather than `Red`/`Bold`) lets the whole palette be
/// retuned in one place.
#[derive(Clone, Copy, Debug)]
pub enum Style {
    /// The `error`/`warning`/`note`/`help` label and the message in the header.
    Severity(Severity),
    /// The diagnostic code, e.g. `[Y001]`.
    Code,
    /// The caret run underlining a span, in the severity's color.
    Caret(Severity),
    /// The `path:line:col` location and the `|` gutter — dimmed.
    Location,
    /// Backtrace frame numbers (`#0`) in the crash debugger — dimmed.
    Dim,
}

/// The severity the renderer is formatting (drives the caret/label color).
#[derive(Clone, Copy, Debug)]
pub enum Severity {
    Error,
    Warning,
    Note,
    Help,
}

/// A palette: a function from [`Style`] to a sequence of SGR codes. `plain`
/// produces no codes at all (plain text); `styled` produces rustc-like colors.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    styled: bool,
}

impl Palette {
    /// A no-op palette: every style renders as plain text. Used by the default
    /// `Renderer` and by snapshot tests.
    pub const fn plain() -> Palette {
        Palette { styled: false }
    }

    /// The colored palette (rustc-like). Only emits ANSI when `styled`.
    pub const fn styled() -> Palette {
        Palette { styled: true }
    }

    /// Build a palette from a resolved color decision.
    pub fn from_enabled(enabled: bool) -> Palette {
        if enabled {
            Palette::styled()
        } else {
            Palette::plain()
        }
    }

    /// Whether this palette emits ANSI styling.
    #[must_use]
    pub fn is_styled(self) -> bool {
        self.styled
    }

    /// Wrap `text` in the ANSI codes for `style`, or return it unchanged when
    /// this palette is plain. Returns a freshly-allocated `String`.
    #[must_use]
    pub fn paint(&self, style: Style, text: &str) -> String {
        if !self.styled {
            return text.to_string();
        }
        let codes = self.codes(style);
        if codes.is_empty() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len() + codes.len() * 4 + 5);
        write_codes(&mut out, &codes);
        out.push_str(text);
        write_codes(&mut out, &[Sgr::RESET]);
        out
    }

    fn codes(&self, style: Style) -> Vec<Sgr> {
        match style {
            Style::Severity(Severity::Error) => vec![Sgr::BOLD, Sgr::RED],
            Style::Severity(Severity::Warning) => vec![Sgr::BOLD, Sgr::YELLOW],
            Style::Severity(Severity::Note) => vec![Sgr::BOLD, Sgr::BLUE],
            Style::Severity(Severity::Help) => vec![Sgr::BOLD, Sgr::CYAN],
            Style::Code => vec![Sgr::BOLD],
            Style::Caret(Severity::Error) => vec![Sgr::RED],
            Style::Caret(Severity::Warning) => vec![Sgr::YELLOW],
            Style::Caret(Severity::Note) => vec![Sgr::BLUE],
            Style::Caret(Severity::Help) => vec![Sgr::CYAN],
            Style::Location | Style::Dim => vec![Sgr::DIM],
        }
    }
}

/// Write `\x1b[` + the SGR codes joined by `;` + `m` into `out`.
fn write_codes(out: &mut String, codes: &[Sgr]) {
    use std::fmt::Write;
    out.push_str("\x1b[");
    for (i, c) in codes.iter().enumerate() {
        if i > 0 {
            out.push(';');
        }
        let _ = write!(out, "{}", c.0);
    }
    out.push('m');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_palette_emits_no_ansi() {
        let p = Palette::plain();
        assert_eq!(p.paint(Style::Severity(Severity::Error), "boom"), "boom");
        assert_eq!(p.paint(Style::Code, "[Y001]"), "[Y001]");
    }

    #[test]
    fn styled_palette_wraps_text() {
        let p = Palette::styled();
        // Error: bold + red => ESC[1;31m ... ESC[0m
        assert_eq!(
            p.paint(Style::Severity(Severity::Error), "boom"),
            "\x1b[1;31mboom\x1b[0m"
        );
        // Code: bold only => ESC[1m ... ESC[0m
        assert_eq!(p.paint(Style::Code, "[Y001]"), "\x1b[1m[Y001]\x1b[0m");
        // Location: dim => ESC[2m ... ESC[0m
        assert_eq!(
            p.paint(Style::Location, "f.px:1:0"),
            "\x1b[2mf.px:1:0\x1b[0m"
        );
    }

    #[test]
    fn from_enabled_toggles() {
        assert!(Palette::from_enabled(true).is_styled());
        assert!(!Palette::from_enabled(false).is_styled());
    }

    #[test]
    fn colormode_should_style_never_is_false() {
        assert!(!ColorMode::Never.should_style());
        assert!(ColorMode::Always.should_style());
        // Auto depends on the terminal; in tests stderr is usually not a TTY,
        // so this is normally false, but we only assert the deterministic modes.
    }
}
