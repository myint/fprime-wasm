//! Errors and warnings, positioned in the source.

use std::fmt;

/// A position in the source: 1-based line and column, columns counted in characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Span {
    pub line: usize,
    pub column: usize,
}

impl Span {
    pub fn new(line: usize, column: usize) -> Self {
        Span { line, column }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// The sequence cannot be compiled.
    Error,
    /// The sequence compiles, but probably does not do what was meant.
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: Level,
    pub span: Span,
    pub message: String,
}

impl Diagnostic {
    pub fn error(span: Span, message: impl Into<String>) -> Self {
        Diagnostic {
            level: Level::Error,
            span,
            message: message.into(),
        }
    }

    pub fn warning(span: Span, message: impl Into<String>) -> Self {
        Diagnostic {
            level: Level::Warning,
            span,
            message: message.into(),
        }
    }

    pub fn is_error(&self) -> bool {
        self.level == Level::Error
    }

    /// `file:line:column: error: message`, the shape editors and `fprime-seqgen` use.
    pub fn render(&self, file: &str) -> String {
        format!("{file}:{self}")
    }
}

/// `line:column: error: message`.
impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let level = match self.level {
            Level::Error => "error",
            Level::Warning => "warning",
        };
        write!(
            f,
            "{}:{}: {level}: {}",
            self.span.line, self.span.column, self.message
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_like_a_compiler() {
        let error = Diagnostic::error(Span::new(3, 11), "no command matches `CMD_NOOP`");
        assert_eq!(
            error.render("safing.seq"),
            "safing.seq:3:11: error: no command matches `CMD_NOOP`"
        );
        let warning = Diagnostic::warning(Span::new(1, 1), "careful");
        assert_eq!(warning.to_string(), "1:1: warning: careful");
        assert!(error.is_error());
        assert!(!warning.is_error());
    }
}
