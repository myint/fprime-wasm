//! Characters into tokens, grouped into logical lines.
//!
//! The lexical rules are `fprime-seqgen`'s, so a sequence it accepts lexes the same way here:
//! `;` comments to the end of the line, a trailing `\` continues a line, strings are quoted
//! with `"` or `'` and kept verbatim between the quotes, and numbers may carry a sign, `_`
//! separators and a `0x` prefix. Added for conditions: parentheses, comparison operators,
//! and `.` after `]` to reach into an array element.

use super::diag::{Diagnostic, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// A time tag, still unparsed: whether it was `A` rather than `R`, and the text after it.
    Time {
        absolute: bool,
        text: String,
    },
    /// An identifier, its dotted segments joined: `Ref.power.PWR_OFF`.
    Name(String),
    Int(i128),
    Float(f64),
    Str(String),
    Bool(bool),
    Comma,
    Colon,
    Dot,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    LParen,
    RParen,
    Op(RelOp),
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl RelOp {
    pub fn symbol(self) -> &'static str {
        match self {
            RelOp::Eq => "==",
            RelOp::Ne => "!=",
            RelOp::Lt => "<",
            RelOp::Le => "<=",
            RelOp::Gt => ">",
            RelOp::Ge => ">=",
        }
    }

    /// Whether this only asks if two values are the same, which is all an enum or a bool
    /// supports.
    pub fn is_equality(self) -> bool {
        matches!(self, RelOp::Eq | RelOp::Ne)
    }
}

impl Token {
    /// How the token reads in an error message.
    pub fn describe(&self) -> String {
        match self {
            Token::Time { absolute, text } => {
                format!("time tag `{}{text}`", if *absolute { 'A' } else { 'R' })
            }
            Token::Name(name) => format!("`{name}`"),
            Token::Int(value) => format!("`{value}`"),
            Token::Float(value) => format!("`{value}`"),
            Token::Str(value) => format!("string \"{value}\""),
            Token::Bool(value) => format!("`{value}`"),
            Token::Comma => "`,`".into(),
            Token::Colon => "`:`".into(),
            Token::Dot => "`.`".into(),
            Token::LBracket => "`[`".into(),
            Token::RBracket => "`]`".into(),
            Token::LBrace => "`{`".into(),
            Token::RBrace => "`}`".into(),
            Token::LParen => "`(`".into(),
            Token::RParen => "`)`".into(),
            Token::Op(op) => format!("`{}`", op.symbol()),
        }
    }
}

/// One logical line: a physical line, plus any it was continued onto with `\`.
#[derive(Debug, Default)]
pub struct Line {
    pub tokens: Vec<(Token, Span)>,
    /// Something on this line did not lex. The error is already reported; the tokens are
    /// what came before it.
    pub broken: bool,
}

/// Every non-blank logical line, in order.
pub fn lex(source: &str, diagnostics: &mut Vec<Diagnostic>) -> Vec<Line> {
    let mut lexer = Lexer {
        chars: source.chars().collect(),
        pos: 0,
        line: 1,
        column: 1,
    };
    let mut lines = vec![];
    let mut current = Line::default();

    while let Some(c) = lexer.peek(0) {
        match c {
            '\n' => {
                lexer.bump();
                if !current.tokens.is_empty() {
                    lines.push(std::mem::take(&mut current));
                }
                current.broken = false;
            }
            ' ' | '\t' | '\r' | '\x0c' => lexer.bump(),
            ';' => lexer.skip_line(),
            '\\' if lexer.continuation() => {}
            _ => match lexer.token(current.tokens.is_empty() && !current.broken) {
                Ok(token) => current.tokens.push(token),
                Err(diagnostic) => {
                    diagnostics.push(diagnostic);
                    current.broken = true;
                    lexer.skip_line();
                }
            },
        }
    }
    if !current.tokens.is_empty() {
        lines.push(current);
    }
    lines
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
    line: usize,
    column: usize,
}

impl Lexer {
    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.pos + ahead).copied()
    }

    fn bump(&mut self) {
        if let Some(c) = self.peek(0) {
            self.pos += 1;
            if c == '\n' {
                self.line += 1;
                self.column = 1;
            } else {
                self.column += 1;
            }
        }
    }

    fn span(&self) -> Span {
        Span::new(self.line, self.column)
    }

    /// Up to, not including, the end of the line.
    fn skip_line(&mut self) {
        while self.peek(0).is_some_and(|c| c != '\n') {
            self.bump();
        }
    }

    /// A `\` with only blanks after it continues the line onto the next one. Consumed if so.
    fn continuation(&mut self) -> bool {
        let mut ahead = 1;
        while matches!(self.peek(ahead), Some(' ' | '\t' | '\r' | '\x0c')) {
            ahead += 1;
        }
        if !matches!(self.peek(ahead), Some('\n') | None) {
            return false;
        }
        for _ in 0..=ahead {
            self.bump();
        }
        true
    }

    fn token(&mut self, line_start: bool) -> Result<(Token, Span), Diagnostic> {
        let span = self.span();
        let c = self.peek(0).expect("called with input remaining");
        let next = self.peek(1);

        let token = match c {
            'R' | 'A' if line_start && self.time_follows() => self.time(),
            '"' | '\'' => self.string(span)?,
            '0'..='9' => self.number(span)?,
            '+' | '-' if next.is_some_and(|n| n.is_ascii_digit() || n == '.') => {
                self.number(span)?
            }
            '.' if next.is_some_and(|n| n.is_ascii_digit()) => self.number(span)?,
            c if c.is_ascii_alphabetic() || c == '_' || c == '$' => self.name(span)?,
            '=' if next == Some('=') => self.two(Token::Op(RelOp::Eq)),
            '!' if next == Some('=') => self.two(Token::Op(RelOp::Ne)),
            '<' if next == Some('=') => self.two(Token::Op(RelOp::Le)),
            '>' if next == Some('=') => self.two(Token::Op(RelOp::Ge)),
            '<' => self.one(Token::Op(RelOp::Lt)),
            '>' => self.one(Token::Op(RelOp::Gt)),
            ',' => self.one(Token::Comma),
            ':' => self.one(Token::Colon),
            '.' => self.one(Token::Dot),
            '[' => self.one(Token::LBracket),
            ']' => self.one(Token::RBracket),
            '{' => self.one(Token::LBrace),
            '}' => self.one(Token::RBrace),
            '(' => self.one(Token::LParen),
            ')' => self.one(Token::RParen),
            '=' => {
                return Err(Diagnostic::error(
                    span,
                    "`=` is not an operator; compare with `==`",
                ));
            }
            other => {
                return Err(Diagnostic::error(
                    span,
                    format!("unexpected character `{other}`"),
                ));
            }
        };
        Ok((token, span))
    }

    fn one(&mut self, token: Token) -> Token {
        self.bump();
        token
    }

    fn two(&mut self, token: Token) -> Token {
        self.bump();
        self.bump();
        token
    }

    /// `R` or `A`, optional blanks, then a digit: a time tag rather than a name.
    fn time_follows(&self) -> bool {
        let mut ahead = 1;
        while matches!(self.peek(ahead), Some(' ' | '\t')) {
            ahead += 1;
        }
        self.peek(ahead).is_some_and(|c| c.is_ascii_digit())
    }

    fn time(&mut self) -> Token {
        let absolute = self.peek(0) == Some('A');
        self.bump();
        while matches!(self.peek(0), Some(' ' | '\t')) {
            self.bump();
        }
        let mut text = String::new();
        while let Some(c) = self
            .peek(0)
            .filter(|c| c.is_ascii_digit() || matches!(c, ':' | '.' | '-' | 'T'))
        {
            text.push(c);
            self.bump();
        }
        Token::Time { absolute, text }
    }

    /// Kept exactly as written between the quotes, as `fprime-seqgen` does: a `\` keeps the
    /// quote after it from ending the string, and is itself kept.
    fn string(&mut self, span: Span) -> Result<Token, Diagnostic> {
        let quote = self.peek(0).expect("at a quote");
        self.bump();
        let mut text = String::new();
        loop {
            match self.peek(0) {
                None | Some('\n') => {
                    return Err(Diagnostic::error(
                        span,
                        "this string has no closing quote on its line",
                    ));
                }
                Some(c) if c == quote => {
                    self.bump();
                    return Ok(Token::Str(text));
                }
                Some('\\') if self.peek(1).is_some_and(|c| c != '\n') => {
                    text.push('\\');
                    self.bump();
                    text.push(self.peek(0).expect("checked above"));
                    self.bump();
                }
                Some(c) => {
                    text.push(c);
                    self.bump();
                }
            }
        }
    }

    fn number(&mut self, span: Span) -> Result<Token, Diagnostic> {
        let mut text = String::new();
        if let Some(sign @ ('+' | '-')) = self.peek(0) {
            text.push(sign);
            self.bump();
        }

        let hex = self.peek(0) == Some('0') && matches!(self.peek(1), Some('x' | 'X'));
        let mut float = false;
        if hex {
            self.bump();
            self.bump();
            let digits = self.take_while(|c| c.is_ascii_hexdigit() || c == '_');
            let digits = digits.replace('_', "");
            let value = i128::from_str_radix(&digits, 16)
                .map_err(|_| Diagnostic::error(span, "malformed hexadecimal number"))?;
            self.end_of_number(span)?;
            return Ok(Token::Int(if text == "-" { -value } else { value }));
        }

        text.push_str(&self.take_while(|c| c.is_ascii_digit() || c == '_'));
        if self.peek(0) == Some('.') && self.peek(1).is_some_and(|c| c.is_ascii_digit()) {
            float = true;
            text.push('.');
            self.bump();
            text.push_str(&self.take_while(|c| c.is_ascii_digit() || c == '_'));
        }
        if matches!(self.peek(0), Some('e' | 'E')) {
            let signed = matches!(self.peek(1), Some('+' | '-'));
            let digit_at = if signed { 2 } else { 1 };
            if self.peek(digit_at).is_some_and(|c| c.is_ascii_digit()) {
                float = true;
                for _ in 0..digit_at {
                    text.push(self.peek(0).expect("checked above"));
                    self.bump();
                }
                text.push_str(&self.take_while(|c| c.is_ascii_digit()));
            }
        }
        self.end_of_number(span)?;

        let text = text.replace('_', "");
        if float {
            let value: f64 = text
                .parse()
                .map_err(|_| Diagnostic::error(span, format!("malformed number `{text}`")))?;
            Ok(Token::Float(value))
        } else {
            let value: i128 = text.parse().map_err(|_| {
                Diagnostic::error(span, format!("`{text}` is too large to be a number here"))
            })?;
            Ok(Token::Int(value))
        }
    }

    /// A number runs into the next token only at a delimiter: `12abc` is a mistake.
    fn end_of_number(&self, span: Span) -> Result<(), Diagnostic> {
        match self.peek(0) {
            Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '.' => Err(Diagnostic::error(
                span,
                format!("malformed number: unexpected `{c}`"),
            )),
            _ => Ok(()),
        }
    }

    /// An identifier, with any `.segment`s that follow it. A leading `$`, which FPP uses to
    /// escape a keyword, is dropped from each segment.
    fn name(&mut self, span: Span) -> Result<Token, Diagnostic> {
        let mut name = self.segment(span)?;
        while self.peek(0) == Some('.')
            && self
                .peek(1)
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        {
            self.bump();
            name.push('.');
            name.push_str(&self.segment(span)?);
        }

        Ok(match name.as_str() {
            "True" | "true" | "TRUE" => Token::Bool(true),
            "False" | "false" | "FALSE" => Token::Bool(false),
            _ => Token::Name(name),
        })
    }

    fn segment(&mut self, span: Span) -> Result<String, Diagnostic> {
        if self.peek(0) == Some('$') {
            self.bump();
        }
        let segment = self.take_while(|c| c.is_ascii_alphanumeric() || c == '_');
        if segment.is_empty() {
            return Err(Diagnostic::error(span, "expected a name after `$`"));
        }
        Ok(segment)
    }

    fn take_while(&mut self, keep: impl Fn(char) -> bool) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek(0).filter(|&c| keep(c)) {
            out.push(c);
            self.bump();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(source: &str) -> Vec<Vec<Token>> {
        let mut diagnostics = vec![];
        let lines = lex(source, &mut diagnostics);
        assert_eq!(diagnostics, vec![], "lexing {source:?}");
        lines
            .into_iter()
            .map(|line| line.tokens.into_iter().map(|(token, _)| token).collect())
            .collect()
    }

    fn error(source: &str) -> String {
        let mut diagnostics = vec![];
        lex(source, &mut diagnostics);
        assert_eq!(diagnostics.len(), 1, "lexing {source:?}: {diagnostics:?}");
        diagnostics.remove(0).message
    }

    fn name(name: &str) -> Token {
        Token::Name(name.into())
    }

    #[test]
    fn command_lines() {
        assert_eq!(
            tokens(
                "R01:00:01.050 CMD_NO_OP_STRING \"Awesome string!\" ; a comment\n\
                 A2015-075T22:32:40.123 cmdDisp.CMD_NO_OP\n"
            ),
            vec![
                vec![
                    Token::Time {
                        absolute: false,
                        text: "01:00:01.050".into()
                    },
                    name("CMD_NO_OP_STRING"),
                    Token::Str("Awesome string!".into()),
                ],
                vec![
                    Token::Time {
                        absolute: true,
                        text: "2015-075T22:32:40.123".into()
                    },
                    name("cmdDisp.CMD_NO_OP"),
                ],
            ]
        );
    }

    #[test]
    fn blank_and_comment_lines_vanish() {
        assert_eq!(
            tokens("\n  ; only a comment\n\r\n\tR00:00:00 X\n\n"),
            vec![vec![
                Token::Time {
                    absolute: false,
                    text: "00:00:00".into()
                },
                name("X")
            ]]
        );
    }

    #[test]
    fn time_tag_may_be_spaced_from_its_letter() {
        assert_eq!(
            tokens("R 00:00:01 X")[0][0],
            Token::Time {
                absolute: false,
                text: "00:00:01".into()
            }
        );
    }

    #[test]
    fn r_and_a_are_names_away_from_the_start_of_a_line() {
        assert_eq!(
            tokens("R00:00:00 X R A1")[0][1..],
            [name("X"), name("R"), name("A1")]
        );
        // A line that starts with a name that only begins with R
        assert_eq!(tokens("RESET")[0], [name("RESET")]);
    }

    #[test]
    fn arguments() {
        assert_eq!(
            tokens("R00:00:00 C 42, -7 +3 0x1F -0x10 1_000 3.5 -.25 1e3 2.5E-1 true FALSE ONE")[0]
                [2..],
            [
                Token::Int(42),
                Token::Comma,
                Token::Int(-7),
                Token::Int(3),
                Token::Int(31),
                Token::Int(-16),
                Token::Int(1000),
                Token::Float(3.5),
                Token::Float(-0.25),
                Token::Float(1000.0),
                Token::Float(0.25),
                Token::Bool(true),
                Token::Bool(false),
                name("ONE"),
            ]
        );
    }

    #[test]
    fn structures() {
        assert_eq!(
            tokens("R00:00:00 C [1, 2], {a: 'x', b: [true]}")[0][2..],
            [
                Token::LBracket,
                Token::Int(1),
                Token::Comma,
                Token::Int(2),
                Token::RBracket,
                Token::Comma,
                Token::LBrace,
                name("a"),
                Token::Colon,
                Token::Str("x".into()),
                Token::Comma,
                name("b"),
                Token::Colon,
                Token::LBracket,
                Token::Bool(true),
                Token::RBracket,
                Token::RBrace,
            ]
        );
    }

    #[test]
    fn strings_are_verbatim() {
        assert_eq!(
            tokens(r#"R00:00:00 C "say \"hi\" \\" 'it''s' """#)[0][2..],
            [
                Token::Str(r#"say \"hi\" \\"#.into()),
                Token::Str("it".into()),
                Token::Str("s".into()),
                Token::Str(String::new()),
            ]
        );
        assert_eq!(
            tokens("R00:00:00 C \"; not a comment\"")[0][2],
            Token::Str("; not a comment".into())
        );
    }

    #[test]
    fn conditions() {
        assert_eq!(
            tokens("IF (TLM a.b[2].c >= -1.5) AND NOT PRM x != y OR LAST_CMD == OK")[0],
            [
                name("IF"),
                Token::LParen,
                name("TLM"),
                name("a.b"),
                Token::LBracket,
                Token::Int(2),
                Token::RBracket,
                Token::Dot,
                name("c"),
                Token::Op(RelOp::Ge),
                Token::Float(-1.5),
                Token::RParen,
                name("AND"),
                name("NOT"),
                name("PRM"),
                name("x"),
                Token::Op(RelOp::Ne),
                name("y"),
                name("OR"),
                name("LAST_CMD"),
                Token::Op(RelOp::Eq),
                name("OK"),
            ]
        );
        assert_eq!(
            tokens("IF a < b <= c > d")[0][1..],
            [
                name("a"),
                Token::Op(RelOp::Lt),
                name("b"),
                Token::Op(RelOp::Le),
                name("c"),
                Token::Op(RelOp::Gt),
                name("d"),
            ]
        );
    }

    #[test]
    fn dollar_escapes_are_dropped() {
        assert_eq!(tokens("IF TLM a.$id")[0][2], name("a.id"));
    }

    #[test]
    fn continuation_joins_lines() {
        let mut diagnostics = vec![];
        let lines = lex("R00:00:00 C 1, \\  \n  2\nIF x", &mut diagnostics);
        assert!(diagnostics.is_empty());
        assert_eq!(lines.len(), 2);
        let (last, span) = lines[0].tokens.last().unwrap();
        assert_eq!(*last, Token::Int(2));
        assert_eq!(*span, Span::new(2, 3), "spans stay on the physical line");
    }

    #[test]
    fn spans_are_one_based() {
        let mut diagnostics = vec![];
        let lines = lex("\n  IF\tTLM x", &mut diagnostics);
        let spans: Vec<Span> = lines[0].tokens.iter().map(|(_, span)| *span).collect();
        assert_eq!(spans, [Span::new(2, 3), Span::new(2, 6), Span::new(2, 10)]);
    }

    #[test]
    fn errors_skip_the_rest_of_the_line() {
        let mut diagnostics = vec![];
        let lines = lex("R00:00:00 C 1 # 2\nR00:00:00 D", &mut diagnostics);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].span, Span::new(1, 15));
        assert_eq!(lines.len(), 2);
        assert!(lines[0].broken);
        assert_eq!(lines[0].tokens.len(), 3);
        assert!(!lines[1].broken);
    }

    #[test]
    fn lexical_errors() {
        assert!(error("R00:00:00 C \"open").contains("no closing quote"));
        assert!(error("IF TLM x = 3").contains("compare with `==`"));
        assert!(error("R00:00:00 C 12abc").contains("malformed number"));
        assert!(error("R00:00:00 C 0xZZ").contains("hexadecimal"));
        assert!(error("R00:00:00 C @").contains("unexpected character `@`"));
        assert!(
            error("R00:00:00 C 99999999999999999999999999999999999999999").contains("too large")
        );
    }
}
