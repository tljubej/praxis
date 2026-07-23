//! Interior re-scan of a backtick template (§7.2, §7.3).
//!
//! A `BacktickTemplate` token spans both backticks but its interior is opaque at
//! lex time. This module re-scans the interior bytes into a sequence of
//! [`TemplatePart`]s: literal runs and `{...}` captures. Whitespace policy
//! escapes (`\s*`, `\s+`, `\n`, `\t`, `\x20`) are recognized here (§7.2).
//!
//! The parser-expression parser (in `praxis-parser`) feeds the capture
//! *interior* (`{...}`) back through the ordinary expression grammar, so this
//! scanner only classifies template structure — it does not parse capture bodies.

use crate::ast::{TemplatePart, WsPolicy};

/// An error encountered while scanning a template interior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanError {
    /// An invalid escape sequence (e.g. `\q`).
    InvalidEscape { byte_offset: usize, seq: String },
    /// An unterminated capture `{...` (no closing `}`).
    UnterminatedCapture { byte_offset: usize },
    /// An empty capture `{}`.
    EmptyCapture { byte_offset: usize },
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::InvalidEscape { byte_offset, seq } => {
                write!(f, "invalid escape `{seq}` at byte {byte_offset}")
            }
            ScanError::UnterminatedCapture { byte_offset } => {
                write!(f, "unterminated capture starting at byte {byte_offset}")
            }
            ScanError::EmptyCapture { byte_offset } => {
                write!(f, "empty capture `{{}}` at byte {byte_offset}")
            }
        }
    }
}

impl std::error::Error for ScanError {}

/// Re-scan the interior bytes of a backtick template into template parts.
///
/// `interior` is the text *between* the backticks (the caller strips them). The
/// returned parts alternate between literal runs and captures; consecutive
/// literal characters are coalesced into one `Literal` part with the whitespace
/// policy of the run.
///
/// # Errors
/// Returns [`ScanError`] on a malformed template (bad escape, unterminated or
/// empty capture).
pub fn scan_template(interior: &str) -> Result<Vec<TemplatePart>, ScanError> {
    let bytes = interior.as_bytes();
    let mut parts = Vec::new();
    let mut lit = String::new();
    let mut lit_ws = WsPolicy::SpaceRun; // default for ordinary space runs
    let mut i = 0;

    // Flush the accumulated literal run with its policy.
    let flush = |lit: &mut String, lit_ws: &mut WsPolicy, parts: &mut Vec<TemplatePart>| {
        if !lit.is_empty() {
            parts.push(TemplatePart::Literal {
                text: std::mem::take(lit),
                ws: *lit_ws,
            });
            *lit_ws = WsPolicy::SpaceRun;
        }
    };

    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'{' => {
                // Start of a capture. Flush any pending literal first.
                flush(&mut lit, &mut lit_ws, &mut parts);
                // Find the matching `}`.
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b'}' {
                    i += 1;
                }
                if i >= bytes.len() {
                    return Err(ScanError::UnterminatedCapture { byte_offset: start });
                }
                let body = &interior[start + 1..i];
                if body.trim().is_empty() {
                    return Err(ScanError::EmptyCapture { byte_offset: start });
                }
                // The body is `{name:parser}` or `{parser}`. Split on the first
                // `:` to get the optional name. The parser body is parsed by the
                // ordinary grammar later; here we just record the text.
                let (name, _parser_body) = split_capture(body);
                parts.push(TemplatePart::Capture {
                    name: name.map(str::to_string),
                    // The parser AST is filled in by the HIR conversion, which
                    // re-parses the body through the expression grammar. Here we
                    // leave a placeholder atomic; the HIR overwrites it.
                    parser: Box::new(crate::ast::ParserAst::Atomic {
                        kind: crate::ast::AtomicKind::Int,
                        span: praxis_source::Span::at(start as u32),
                    }),
                });
                i += 1; // skip `}`
            }
            b'\\' if i + 1 < bytes.len() => {
                let next = bytes[i + 1];
                // Whitespace-policy escapes (§7.2). Each starts a fresh policy run.
                match next {
                    b's' => {
                        // `\s*` or `\s+` — peek the char after `s`.
                        if i + 2 < bytes.len() && bytes[i + 2] == b'*' {
                            flush(&mut lit, &mut lit_ws, &mut parts);
                            parts.push(TemplatePart::Literal {
                                text: String::new(),
                                ws: WsPolicy::ZeroOrMore,
                            });
                            i += 3;
                        } else if i + 2 < bytes.len() && bytes[i + 2] == b'+' {
                            flush(&mut lit, &mut lit_ws, &mut parts);
                            parts.push(TemplatePart::Literal {
                                text: String::new(),
                                ws: WsPolicy::OneOrMore,
                            });
                            i += 3;
                        } else {
                            return Err(ScanError::InvalidEscape {
                                byte_offset: i,
                                seq: format!("\\s{}", char::from(next)),
                            });
                        }
                    }
                    b'n' => {
                        flush(&mut lit, &mut lit_ws, &mut parts);
                        parts.push(TemplatePart::Literal {
                            text: String::new(),
                            ws: WsPolicy::Newline,
                        });
                        i += 2;
                    }
                    b't' => {
                        flush(&mut lit, &mut lit_ws, &mut parts);
                        parts.push(TemplatePart::Literal {
                            text: String::new(),
                            ws: WsPolicy::Tab,
                        });
                        i += 2;
                    }
                    b'x' if i + 3 < bytes.len() && bytes[i + 2] == b'2' && bytes[i + 3] == b'0' => {
                        flush(&mut lit, &mut lit_ws, &mut parts);
                        parts.push(TemplatePart::Literal {
                            text: String::new(),
                            ws: WsPolicy::ExactSpace,
                        });
                        i += 4;
                    }
                    b'`' | b'\\' => {
                        // Escaped literal backtick / backslash.
                        lit.push(char::from(next));
                        i += 2;
                    }
                    _ => {
                        return Err(ScanError::InvalidEscape {
                            byte_offset: i,
                            seq: format!("\\{}", char::from(next)),
                        });
                    }
                }
            }
            _ => {
                // Ordinary byte. A run of spaces/tabs gets the flexible policy.
                let ch = char::from(b);
                lit.push(ch);
                i += 1;
            }
        }
    }
    flush(&mut lit, &mut lit_ws, &mut parts);
    Ok(parts)
}

/// Split a capture body `name:parser` or `parser` into `(optional name, body)`.
fn split_capture(body: &str) -> (Option<&str>, &str) {
    // The name is an identifier followed by `:`. The parser body may itself
    // contain `:` (e.g. nested), so split only on the first `:` that follows an
    // identifier.
    if let Some(colon) = body.find(':') {
        let name = &body[..colon];
        // Validate it looks like an identifier (defensive; full validation in HIR).
        if !name.is_empty()
            && name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return (Some(name), body[colon + 1..].trim());
        }
    }
    (None, body.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_literal_template() {
        let parts = scan_template("hello").unwrap();
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            TemplatePart::Literal { text, .. } => assert_eq!(text, "hello"),
            _ => panic!("expected literal"),
        }
    }

    #[test]
    fn single_anonymous_capture() {
        let parts = scan_template("{int}").unwrap();
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            TemplatePart::Capture { name, .. } => assert!(name.is_none()),
            _ => panic!("expected capture"),
        }
    }

    #[test]
    fn named_capture_with_literal() {
        let parts = scan_template("{x:int},{y:int}").unwrap();
        assert_eq!(parts.len(), 3);
        match &parts[0] {
            TemplatePart::Capture { name, .. } => assert_eq!(name.as_deref(), Some("x")),
            _ => panic!("expected capture"),
        }
        match &parts[1] {
            TemplatePart::Literal { text, .. } => assert_eq!(text, ","),
            _ => panic!("expected literal"),
        }
        match &parts[2] {
            TemplatePart::Capture { name, .. } => assert_eq!(name.as_deref(), Some("y")),
            _ => panic!("expected capture"),
        }
    }

    #[test]
    fn whitespace_escape_policies() {
        let parts = scan_template("a\\s*b").unwrap();
        assert_eq!(parts.len(), 3);
        match &parts[1] {
            TemplatePart::Literal { ws, .. } => assert_eq!(*ws, WsPolicy::ZeroOrMore),
            _ => panic!("expected ws literal"),
        }
    }

    #[test]
    fn unterminated_capture_errors() {
        assert!(matches!(
            scan_template("{int"),
            Err(ScanError::UnterminatedCapture { .. })
        ));
    }

    #[test]
    fn empty_capture_errors() {
        assert!(matches!(
            scan_template("{}"),
            Err(ScanError::EmptyCapture { .. })
        ));
    }

    #[test]
    fn escaped_backtick_is_literal() {
        let parts = scan_template("a\\`b").unwrap();
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            TemplatePart::Literal { text, .. } => assert_eq!(text, "a`b"),
            _ => panic!("expected literal"),
        }
    }
}
