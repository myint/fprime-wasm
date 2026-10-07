//! Logical lines into statements.
//!
//! A line is either a command (a time tag, a mnemonic and its arguments) or one of `IF`,
//! `ELIF`, `ELSE` and `ENDIF`, which nest. Conditions are, loosest first:
//!
//! ```text
//! condition := and ("OR" and)*
//! and       := not ("AND" not)*
//! not       := "NOT" not | primary
//! primary   := "(" condition ")" | operand [comparison operand]
//! operand   := "TLM" path | "PRM" path | "LAST_CMD" | literal
//! path      := name ("[" integer "]" | "." name)*
//! ```

use super::diag::{Diagnostic, Span};
use super::lex::{Line, RelOp, Token};
use super::time::{self, TimeTag};

/// `IF` blocks nested deeper than this are refused before anything recurses over them. The
/// interpreter's own control-frame limit binds well before it in practice.
pub const MAX_NESTING: usize = 32;

/// Parentheses and argument brackets nested deeper than this are refused, for the same
/// reason.
const MAX_EXPRESSION_DEPTH: usize = 32;

/// ANDs and ORs in one condition. Each joins the condition to the left of it, so this bounds
/// how deep the rest of the compiler recurses over one.
const MAX_OPERATORS: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Command(Command),
    If(If),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub span: Span,
    pub time: TimeTag,
    pub mnemonic: String,
    pub mnemonic_span: Span,
    pub args: Vec<Value>,
}

/// `IF`, any `ELIF`s, and an optional `ELSE`.
#[derive(Debug, Clone, PartialEq)]
pub struct If {
    pub arms: Vec<Arm>,
    pub otherwise: Option<Vec<Stmt>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Arm {
    /// The `IF` or `ELIF` keyword.
    pub span: Span,
    pub condition: Cond,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cond {
    Or(Box<Cond>, Box<Cond>),
    And(Box<Cond>, Box<Cond>),
    Not(Box<Cond>),
    Compare(Operand, RelOp, Operand),
    /// An operand on its own, which must be a bool.
    Test(Operand),
    /// A condition that did not parse; already reported.
    Invalid,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Operand {
    pub span: Span,
    pub kind: OperandKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OperandKind {
    Telemetry(Path),
    Parameter(Path),
    LastCmd,
    Literal(ValueKind),
}

/// A channel or parameter name, and the members and elements reached into after it.
#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    pub name: String,
    pub accessors: Vec<(Accessor, Span)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Accessor {
    Member(String),
    Index(i128),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Value {
    pub span: Span,
    pub kind: ValueKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ValueKind {
    Int(i128),
    Float(f64),
    Bool(bool),
    Str(String),
    /// An enumerated constant, or the `CONTINUE` modifier.
    Name(String),
    Array(Vec<Value>),
    Struct(Vec<Member>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub name: String,
    pub span: Span,
    pub value: Value,
}

impl ValueKind {
    /// How the value reads in an error message.
    pub fn describe(&self) -> String {
        match self {
            ValueKind::Int(value) => format!("`{value}`"),
            ValueKind::Float(value) => format!("`{value:?}`"),
            ValueKind::Bool(value) => format!("`{value}`"),
            ValueKind::Str(value) => format!("string \"{value}\""),
            ValueKind::Name(name) => format!("`{name}`"),
            ValueKind::Array(_) => "an array".into(),
            ValueKind::Struct(_) => "a struct".into(),
        }
    }
}

/// Words with a meaning of their own in a condition; not usable as bare constants there.
const CONDITION_KEYWORDS: &[&str] = &["AND", "OR", "NOT", "TLM", "PRM", "LAST_CMD"];

const STATEMENT_KEYWORDS: &[&str] = &["IF", "ELIF", "ELSE", "ENDIF"];

/// The statements of a whole sequence. Problems are reported and parsing carries on, so
/// one run finds as many as it can.
pub fn parse(lines: &[Line], diagnostics: &mut Vec<Diagnostic>) -> Vec<Stmt> {
    let mut root = vec![];
    let mut open: Vec<Open> = vec![];
    let mut too_deep = false;

    for line in lines {
        let (first, span) = &line.tokens[0];
        let keyword = match first {
            Token::Name(name) if STATEMENT_KEYWORDS.contains(&name.as_str()) => Some(name.as_str()),
            _ => None,
        };

        match (first, keyword) {
            (Token::Time { .. }, _) => {
                if line.broken {
                    continue;
                }
                let mut cursor = Cursor::new(&line.tokens);
                match cursor.command() {
                    Ok(command) => body(&mut root, &mut open).push(Stmt::Command(command)),
                    Err(diagnostic) => diagnostics.push(diagnostic),
                }
            }
            (_, Some(keyword @ ("IF" | "ELIF"))) => {
                let condition = if line.broken {
                    Cond::Invalid
                } else {
                    let mut cursor = Cursor::new(&line.tokens[1..]);
                    cursor.end = line.tokens.last().map(|(_, span)| *span).unwrap_or(*span);
                    match cursor.whole_condition() {
                        Ok(condition) => condition,
                        Err(diagnostic) => {
                            diagnostics.push(diagnostic);
                            Cond::Invalid
                        }
                    }
                };
                let arm = Arm {
                    span: *span,
                    condition,
                    body: vec![],
                };

                if keyword == "IF" {
                    if open.len() >= MAX_NESTING && !too_deep {
                        too_deep = true;
                        diagnostics.push(Diagnostic::error(
                            *span,
                            format!("IF blocks are nested more than {MAX_NESTING} deep"),
                        ));
                    }
                    open.push(Open {
                        span: *span,
                        arms: vec![arm],
                        otherwise: None,
                    });
                } else {
                    match open.last_mut() {
                        None => diagnostics.push(Diagnostic::error(*span, "ELIF without IF")),
                        Some(block) if block.otherwise.is_some() => diagnostics.push(
                            Diagnostic::error(*span, "ELIF after ELSE; move it above the ELSE"),
                        ),
                        Some(block) => block.arms.push(arm),
                    }
                }
            }
            (_, Some(keyword @ ("ELSE" | "ENDIF"))) => {
                if let Some((extra, extra_span)) = line.tokens.get(1) {
                    diagnostics.push(Diagnostic::error(
                        *extra_span,
                        format!("nothing may follow {keyword}, found {}", extra.describe()),
                    ));
                }
                if keyword == "ELSE" {
                    match open.last_mut() {
                        None => diagnostics.push(Diagnostic::error(*span, "ELSE without IF")),
                        Some(block) if block.otherwise.is_some() => diagnostics
                            .push(Diagnostic::error(*span, "a second ELSE for the same IF")),
                        Some(block) => block.otherwise = Some(vec![]),
                    }
                } else {
                    match open.pop() {
                        None => diagnostics.push(Diagnostic::error(*span, "ENDIF without IF")),
                        Some(block) => {
                            let statement = Stmt::If(If {
                                arms: block.arms,
                                otherwise: block.otherwise,
                            });
                            body(&mut root, &mut open).push(statement);
                        }
                    }
                }
            }
            (Token::Name(name), _)
                if STATEMENT_KEYWORDS
                    .iter()
                    .any(|keyword| keyword.eq_ignore_ascii_case(name)) =>
            {
                diagnostics.push(Diagnostic::error(
                    *span,
                    format!(
                        "keywords are upper case: write `{}`",
                        name.to_ascii_uppercase()
                    ),
                ));
            }
            _ => diagnostics.push(Diagnostic::error(
                *span,
                format!(
                    "expected a command (a line starting with an `R` or `A` time tag) or IF, \
                     ELIF, ELSE or ENDIF, found {}",
                    first.describe()
                ),
            )),
        }
    }

    for block in open.iter().rev() {
        diagnostics.push(Diagnostic::error(block.span, "this IF has no ENDIF"));
    }
    // Nothing later recurses over a tree this deep; its errors are reported already.
    if too_deep { vec![] } else { root }
}

/// An `IF` whose `ENDIF` has not been reached yet.
struct Open {
    span: Span,
    arms: Vec<Arm>,
    otherwise: Option<Vec<Stmt>>,
}

/// Where the next statement goes: the innermost open arm, or the top level.
fn body<'a>(root: &'a mut Vec<Stmt>, open: &'a mut [Open]) -> &'a mut Vec<Stmt> {
    match open.last_mut() {
        None => root,
        Some(block) => match &mut block.otherwise {
            Some(otherwise) => otherwise,
            None => &mut block.arms.last_mut().expect("an IF has an arm").body,
        },
    }
}

struct Cursor<'a> {
    tokens: &'a [(Token, Span)],
    pos: usize,
    /// Where "the end of the line" is, for errors about something missing there.
    end: Span,
    depth: usize,
    operators: usize,
}

type Parsed<T> = Result<T, Diagnostic>;

impl<'a> Cursor<'a> {
    fn new(tokens: &'a [(Token, Span)]) -> Self {
        Cursor {
            tokens,
            pos: 0,
            end: tokens.last().map(|(_, span)| *span).unwrap_or_default(),
            depth: 0,
            operators: 0,
        }
    }

    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.pos).map(|(token, _)| token)
    }

    fn span(&self) -> Span {
        self.tokens
            .get(self.pos)
            .map(|(_, span)| *span)
            .unwrap_or(self.end)
    }

    fn next(&mut self) -> Option<(&'a Token, Span)> {
        let (token, span) = self.tokens.get(self.pos)?;
        self.pos += 1;
        Some((token, *span))
    }

    fn eat(&mut self, token: &Token) -> bool {
        if self.peek() == Some(token) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn at_keyword(&self, keyword: &str) -> bool {
        matches!(self.peek(), Some(Token::Name(name)) if name == keyword)
    }

    /// An error at the next token, or at the end of the line if there is none.
    fn expected(&self, what: &str) -> Diagnostic {
        match self.peek() {
            Some(token) => Diagnostic::error(
                self.span(),
                format!("expected {what}, found {}", token.describe()),
            ),
            None => Diagnostic::error(self.end, format!("expected {what} at the end of the line")),
        }
    }

    fn descend(&mut self) -> Parsed<()> {
        self.depth += 1;
        if self.depth > MAX_EXPRESSION_DEPTH {
            return Err(Diagnostic::error(
                self.span(),
                format!("nested more than {MAX_EXPRESSION_DEPTH} deep"),
            ));
        }
        Ok(())
    }

    /// Past an AND or OR.
    fn operator(&mut self) -> Parsed<()> {
        self.operators += 1;
        if self.operators > MAX_OPERATORS {
            return Err(Diagnostic::error(
                self.span(),
                format!("a condition joins at most {MAX_OPERATORS} comparisons with AND and OR"),
            ));
        }
        self.pos += 1;
        Ok(())
    }

    fn command(&mut self) -> Parsed<Command> {
        let (Token::Time { absolute, text }, span) = self.next().expect("a command line") else {
            unreachable!("a command line starts with a time tag")
        };
        let time = if *absolute {
            time::absolute(text).map(TimeTag::Absolute)
        } else {
            time::relative(text).map(TimeTag::Relative)
        }
        .map_err(|message| Diagnostic::error(span, message))?;

        let mnemonic_span = self.span();
        let Some(Token::Name(mnemonic)) = self.peek() else {
            return Err(self.expected("a command mnemonic after the time tag"));
        };
        self.pos += 1;
        let mnemonic = mnemonic.clone();

        // `fprime-seqgen` allows a comma before the first argument and between any two.
        let mut args = vec![];
        let mut comma = self.eat(&Token::Comma);
        while self.peek().is_some() {
            args.push(self.value()?);
            comma = self.eat(&Token::Comma);
        }
        if comma {
            return Err(self.expected("an argument after `,`"));
        }

        Ok(Command {
            span,
            time,
            mnemonic,
            mnemonic_span,
            args,
        })
    }

    fn value(&mut self) -> Parsed<Value> {
        let span = self.span();
        let kind = match self.peek() {
            Some(Token::LBracket) => {
                self.descend()?;
                self.pos += 1;
                let mut elements = vec![];
                while !self.eat(&Token::RBracket) {
                    elements.push(self.value()?);
                    if !self.eat(&Token::Comma) && self.peek() != Some(&Token::RBracket) {
                        return Err(self.expected("`,` or `]`"));
                    }
                }
                self.depth -= 1;
                ValueKind::Array(elements)
            }
            Some(Token::LBrace) => {
                self.descend()?;
                self.pos += 1;
                let mut members = vec![];
                while !self.eat(&Token::RBrace) {
                    let member_span = self.span();
                    let Some(Token::Name(name)) = self.peek() else {
                        return Err(self.expected("a member name or `}`"));
                    };
                    self.pos += 1;
                    let name = name.clone();
                    if !self.eat(&Token::Colon) {
                        return Err(self.expected(&format!("`:` after `{name}`")));
                    }
                    let value = self.value()?;
                    members.push(Member {
                        name,
                        span: member_span,
                        value,
                    });
                    if !self.eat(&Token::Comma) && self.peek() != Some(&Token::RBrace) {
                        return Err(self.expected("`,` or `}`"));
                    }
                }
                self.depth -= 1;
                ValueKind::Struct(members)
            }
            _ => self.scalar("an argument")?,
        };
        Ok(Value { span, kind })
    }

    /// A number, string, bool or name.
    fn scalar(&mut self, what: &str) -> Parsed<ValueKind> {
        let kind = match self.peek() {
            Some(Token::Int(value)) => ValueKind::Int(*value),
            Some(Token::Float(value)) => ValueKind::Float(*value),
            Some(Token::Bool(value)) => ValueKind::Bool(*value),
            Some(Token::Str(value)) => ValueKind::Str(value.clone()),
            Some(Token::Name(name)) => ValueKind::Name(name.clone()),
            _ => return Err(self.expected(what)),
        };
        self.pos += 1;
        Ok(kind)
    }

    /// A condition filling the rest of the line.
    fn whole_condition(&mut self) -> Parsed<Cond> {
        if self.peek().is_none() {
            return Err(self.expected("a condition"));
        }
        let condition = self.condition()?;
        match self.peek() {
            None => Ok(condition),
            Some(Token::Op(_)) => Err(Diagnostic::error(
                self.span(),
                "comparisons do not chain; join them with AND or OR",
            )),
            Some(token) => Err(Diagnostic::error(
                self.span(),
                format!("unexpected {} after the condition", token.describe()),
            )),
        }
    }

    fn condition(&mut self) -> Parsed<Cond> {
        let mut left = self.and()?;
        while self.at_keyword("OR") {
            self.operator()?;
            let right = self.and()?;
            left = Cond::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Parsed<Cond> {
        let mut left = self.not()?;
        while self.at_keyword("AND") {
            self.operator()?;
            let right = self.not()?;
            left = Cond::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn not(&mut self) -> Parsed<Cond> {
        if self.at_keyword("NOT") {
            self.descend()?;
            self.pos += 1;
            let inner = self.not()?;
            self.depth -= 1;
            return Ok(Cond::Not(Box::new(inner)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Parsed<Cond> {
        if self.eat(&Token::LParen) {
            self.descend()?;
            let inner = self.condition()?;
            if !self.eat(&Token::RParen) {
                return Err(self.expected("`)`"));
            }
            self.depth -= 1;
            return Ok(inner);
        }

        let left = self.operand()?;
        match self.peek() {
            Some(Token::Op(op)) => {
                self.pos += 1;
                let right = self.operand()?;
                Ok(Cond::Compare(left, *op, right))
            }
            _ => Ok(Cond::Test(left)),
        }
    }

    fn operand(&mut self) -> Parsed<Operand> {
        let span = self.span();
        let kind = match self.peek() {
            Some(Token::Name(name)) if name == "TLM" => {
                self.pos += 1;
                OperandKind::Telemetry(self.path("telemetry channel")?)
            }
            Some(Token::Name(name)) if name == "PRM" => {
                self.pos += 1;
                OperandKind::Parameter(self.path("parameter")?)
            }
            Some(Token::Name(name)) if name == "LAST_CMD" => {
                self.pos += 1;
                OperandKind::LastCmd
            }
            Some(Token::Name(name)) if CONDITION_KEYWORDS.contains(&name.as_str()) => {
                return Err(self.expected("TLM, PRM, LAST_CMD or a value"));
            }
            _ => OperandKind::Literal(self.scalar("TLM, PRM, LAST_CMD or a value")?),
        };
        Ok(Operand { span, kind })
    }

    fn path(&mut self, what: &str) -> Parsed<Path> {
        let name = match self.peek() {
            Some(Token::Name(name)) => name.clone(),
            _ => return Err(self.expected(&format!("a {what} name"))),
        };
        self.pos += 1;

        let mut accessors = vec![];
        loop {
            let span = self.span();
            if self.eat(&Token::LBracket) {
                let Some(Token::Int(index)) = self.peek() else {
                    return Err(self.expected("an element index"));
                };
                self.pos += 1;
                let index = *index;
                if !self.eat(&Token::RBracket) {
                    return Err(self.expected("`]`"));
                }
                accessors.push((Accessor::Index(index), span));
            } else if self.eat(&Token::Dot) {
                let member_span = self.span();
                let Some(Token::Name(members)) = self.peek() else {
                    return Err(self.expected("a member name after `.`"));
                };
                self.pos += 1;
                for member in members.split('.') {
                    accessors.push((Accessor::Member(member.to_string()), member_span));
                }
            } else {
                break;
            }
        }
        Ok(Path { name, accessors })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seq::lex::lex;

    fn parsed(source: &str) -> Vec<Stmt> {
        let mut diagnostics = vec![];
        let lines = lex(source, &mut diagnostics);
        let statements = parse(&lines, &mut diagnostics);
        assert_eq!(diagnostics, vec![], "parsing {source:?}");
        statements
    }

    fn errors(source: &str) -> Vec<String> {
        let mut diagnostics = vec![];
        let lines = lex(source, &mut diagnostics);
        parse(&lines, &mut diagnostics);
        assert!(!diagnostics.is_empty(), "{source:?} should not parse");
        diagnostics
            .into_iter()
            .map(|diagnostic| diagnostic.to_string())
            .collect()
    }

    fn condition(text: &str) -> Cond {
        match parsed(&format!("IF {text}\nENDIF")).remove(0) {
            Stmt::If(mut chain) => chain.arms.remove(0).condition,
            other => panic!("expected an IF, got {other:?}"),
        }
    }

    fn tlm(name: &str) -> OperandKind {
        OperandKind::Telemetry(Path {
            name: name.into(),
            accessors: vec![],
        })
    }

    fn kinds(cond: &Cond) -> String {
        match cond {
            Cond::Or(a, b) => format!("({} OR {})", kinds(a), kinds(b)),
            Cond::And(a, b) => format!("({} AND {})", kinds(a), kinds(b)),
            Cond::Not(a) => format!("NOT {}", kinds(a)),
            Cond::Compare(a, op, b) => {
                format!("{} {} {}", operand(a), op.symbol(), operand(b))
            }
            Cond::Test(a) => operand(a),
            Cond::Invalid => "?".into(),
        }
    }

    fn operand(operand: &Operand) -> String {
        match &operand.kind {
            OperandKind::Telemetry(path) => format!("TLM {}", path.name),
            OperandKind::Parameter(path) => format!("PRM {}", path.name),
            OperandKind::LastCmd => "LAST_CMD".into(),
            OperandKind::Literal(value) => value.describe().replace('`', ""),
        }
    }

    #[test]
    fn a_command_line() {
        let statements = parsed("R00:00:01.5 Ref.power.PWR_ON, 3 \"x\" [1, 2,] {a: ONE}");
        let [Stmt::Command(command)] = statements.as_slice() else {
            panic!("expected one command, got {statements:?}");
        };
        assert_eq!(command.time, TimeTag::Relative(1_500_000));
        assert_eq!(command.mnemonic, "Ref.power.PWR_ON");
        assert_eq!(command.mnemonic_span, Span::new(1, 13));
        let kinds: Vec<&ValueKind> = command.args.iter().map(|arg| &arg.kind).collect();
        assert_eq!(
            kinds,
            [
                &ValueKind::Int(3),
                &ValueKind::Str("x".into()),
                &ValueKind::Array(vec![
                    Value {
                        span: Span::new(1, 38),
                        kind: ValueKind::Int(1)
                    },
                    Value {
                        span: Span::new(1, 41),
                        kind: ValueKind::Int(2)
                    },
                ]),
                &ValueKind::Struct(vec![Member {
                    name: "a".into(),
                    span: Span::new(1, 46),
                    value: Value {
                        span: Span::new(1, 49),
                        kind: ValueKind::Name("ONE".into())
                    }
                }]),
            ]
        );
    }

    #[test]
    fn nested_blocks() {
        let statements = parsed(
            "R00:00:00 A_CMD\n\
             IF TLM a\n\
               R00:00:00 B_CMD\n\
               IF TLM b\n\
                 R00:00:00 C_CMD\n\
               ENDIF\n\
             ELIF TLM c\n\
             ELSE\n\
               R00:00:00 D_CMD\n\
             ENDIF\n\
             R00:00:00 E_CMD",
        );
        assert_eq!(statements.len(), 3);
        let Stmt::If(chain) = &statements[1] else {
            panic!("expected an IF");
        };
        assert_eq!(chain.arms.len(), 2);
        assert_eq!(chain.arms[0].span, Span::new(2, 1));
        assert_eq!(chain.arms[0].body.len(), 2);
        assert!(matches!(chain.arms[0].body[1], Stmt::If(_)));
        assert_eq!(chain.arms[1].body, vec![]);
        assert_eq!(chain.otherwise.as_ref().map(Vec::len), Some(1));
    }

    #[test]
    fn precedence() {
        assert_eq!(
            kinds(&condition("TLM a OR TLM b AND NOT TLM c")),
            "(TLM a OR (TLM b AND NOT TLM c))"
        );
        assert_eq!(
            kinds(&condition("(TLM a OR TLM b) AND TLM c")),
            "((TLM a OR TLM b) AND TLM c)"
        );
        assert_eq!(
            kinds(&condition("NOT TLM a > 3 AND LAST_CMD == OK")),
            "(NOT TLM a > 3 AND LAST_CMD == OK)"
        );
        assert_eq!(
            kinds(&condition("TLM a OR TLM b OR TLM c")),
            "((TLM a OR TLM b) OR TLM c)"
        );
        assert_eq!(kinds(&condition("NOT NOT TLM a")), "NOT NOT TLM a");
    }

    #[test]
    fn operands() {
        let Cond::Compare(left, op, right) = condition("PRM x.LIMIT <= -2.5") else {
            panic!("expected a comparison");
        };
        assert_eq!(
            left.kind,
            OperandKind::Parameter(Path {
                name: "x.LIMIT".into(),
                accessors: vec![]
            })
        );
        assert_eq!(op, RelOp::Le);
        assert_eq!(right.kind, OperandKind::Literal(ValueKind::Float(-2.5)));
        assert_eq!(right.span, Span::new(1, 19));

        let Cond::Compare(_, _, right) = condition("TLM a == \"s\"") else {
            panic!("expected a comparison");
        };
        assert_eq!(right.kind, OperandKind::Literal(ValueKind::Str("s".into())));

        assert_eq!(
            condition("TLM flag"),
            Cond::Test(Operand {
                span: Span::new(1, 4),
                kind: tlm("flag")
            })
        );
    }

    #[test]
    fn accessors() {
        let Cond::Test(operand) = condition("TLM a.b[2].c.d[0]") else {
            panic!("expected a test");
        };
        let OperandKind::Telemetry(path) = operand.kind else {
            panic!("expected telemetry");
        };
        assert_eq!(path.name, "a.b");
        let accessors: Vec<Accessor> = path.accessors.into_iter().map(|(a, _)| a).collect();
        assert_eq!(
            accessors,
            [
                Accessor::Index(2),
                Accessor::Member("c".into()),
                Accessor::Member("d".into()),
                Accessor::Index(0),
            ]
        );
    }

    #[test]
    fn structural_errors() {
        assert_eq!(errors("ELSE"), ["1:1: error: ELSE without IF"]);
        assert_eq!(errors("ENDIF"), ["1:1: error: ENDIF without IF"]);
        assert_eq!(errors("ELIF TLM a"), ["1:1: error: ELIF without IF"]);
        assert_eq!(
            errors("IF TLM a\nELSE\nELIF TLM b\nENDIF"),
            ["3:1: error: ELIF after ELSE; move it above the ELSE"]
        );
        assert_eq!(
            errors("IF TLM a\nELSE\nELSE\nENDIF"),
            ["3:1: error: a second ELSE for the same IF"]
        );
        assert_eq!(
            errors("IF TLM a\n  IF TLM b\nENDIF"),
            ["1:1: error: this IF has no ENDIF"]
        );
        assert_eq!(
            errors("IF TLM a\nENDIF TLM b"),
            ["2:7: error: nothing may follow ENDIF, found `TLM`"]
        );
        assert_eq!(
            errors("if TLM a\nENDIF"),
            [
                "1:1: error: keywords are upper case: write `IF`",
                "2:1: error: ENDIF without IF"
            ]
        );
        assert!(errors("CMD_NO_OP")[0].contains("expected a command"));
    }

    #[test]
    fn condition_errors() {
        assert_eq!(
            errors("IF\nENDIF"),
            ["1:1: error: expected a condition at the end of the line"]
        );
        assert_eq!(
            errors("IF TLM a < 1 < 2\nENDIF"),
            ["1:14: error: comparisons do not chain; join them with AND or OR"]
        );
        assert_eq!(
            errors("IF (TLM a\nENDIF"),
            ["1:9: error: expected `)` at the end of the line"]
        );
        assert_eq!(
            errors("IF TLM a AND\nENDIF"),
            ["1:10: error: expected TLM, PRM, LAST_CMD or a value at the end of the line"]
        );
        assert_eq!(
            errors("IF TLM 3\nENDIF"),
            ["1:8: error: expected a telemetry channel name, found `3`"]
        );
        assert_eq!(
            errors("IF TLM a[x]\nENDIF"),
            ["1:10: error: expected an element index, found `x`"]
        );
        assert_eq!(
            errors("IF TLM a b\nENDIF"),
            ["1:10: error: unexpected `b` after the condition"]
        );
        let deep = format!("IF {}TLM a{}\nENDIF", "(".repeat(40), ")".repeat(40));
        assert!(errors(&deep)[0].contains("nested more than 32 deep"));
        let long = format!("IF TLM a{}\nENDIF", " AND TLM a".repeat(65));
        assert_eq!(
            errors(&long),
            ["1:650: error: a condition joins at most 64 comparisons with AND and OR"]
        );
        let long = format!("IF TLM a{}\nENDIF", " OR TLM a".repeat(64));
        condition(&long["IF ".len()..long.len() - "\nENDIF".len()]);
    }

    #[test]
    fn a_broken_condition_keeps_the_block_structure() {
        // The `#` fails to lex; the IF still pairs with its ENDIF.
        let mut diagnostics = vec![];
        let lines = lex("IF TLM a # 3\n  R00:00:00 X\nENDIF", &mut diagnostics);
        let statements = parse(&lines, &mut diagnostics);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        let [Stmt::If(chain)] = statements.as_slice() else {
            panic!("expected one IF");
        };
        assert_eq!(chain.arms[0].condition, Cond::Invalid);
        assert_eq!(chain.arms[0].body.len(), 1);
    }

    #[test]
    fn command_errors() {
        assert_eq!(
            errors("R00:00:00"),
            ["1:1: error: expected a command mnemonic after the time tag at the end of the line"]
        );
        assert_eq!(
            errors("R00:00:00 3"),
            ["1:11: error: expected a command mnemonic after the time tag, found `3`"]
        );
        assert_eq!(
            errors("R00:00:00 C 1,"),
            ["1:14: error: expected an argument after `,` at the end of the line"]
        );
        assert_eq!(
            errors("R00:00:00 C [1 2]"),
            ["1:16: error: expected `,` or `]`, found `2`"]
        );
        assert_eq!(
            errors("R00:00:00 C {a 1}"),
            ["1:16: error: expected `:` after `a`, found `1`"]
        );
        assert_eq!(
            errors("R25:00:00 C"),
            [
                "1:1: error: `25:00:00` is out of range: hours run 00 to 23, minutes and seconds 00 to 59"
            ]
        );
    }

    #[test]
    fn deep_nesting_is_refused_once_and_parses_to_nothing() {
        let levels = 10 * MAX_NESTING;
        let source = "IF TLM a\n".repeat(levels) + &"ENDIF\n".repeat(levels);
        let mut diagnostics = vec![];
        let lines = lex(&source, &mut diagnostics);
        let statements = parse(&lines, &mut diagnostics);
        assert_eq!(statements, vec![]);
        let found: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
        assert_eq!(
            found,
            [format!(
                "{}:1: error: IF blocks are nested more than 32 deep",
                MAX_NESTING + 1
            )]
        );
    }
}
