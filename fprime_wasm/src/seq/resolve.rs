//! Statements against the dictionary: every command encoded to the bytes it dispatches,
//! every condition typed down to comparisons of 32- and 64-bit numbers.
//!
//! A name is found by its full dictionary name (`Ref.power.PWR_OFF`) or by any unique
//! trailing part of it (`power.PWR_OFF`, `PWR_OFF`); an ambiguous one is an error.

use super::diag::{Diagnostic, Span};
use super::encode::{self, Shape, Wire};
use super::lex::RelOp;
use super::parse::{Accessor, Cond, Operand, OperandKind, Path, Stmt, ValueKind};
use super::time::TimeTag;
use fprime_dictionary::{Dictionary, EnumType, FloatKind, IntegerKind, TypeDefinition, TypeName};
use fprime_test::abi;

/// The sequence, ready to emit.
#[derive(Debug)]
pub struct Program {
    pub body: Vec<Step>,
    /// Bytes reserved for a channel or parameter value: the largest read.
    pub value_size: u32,
    /// `Fw::Time::SERIALIZED_SIZE`, which `tlm` needs an exact buffer for.
    pub time_size: u32,
    pub uses_last_cmd: bool,
}

#[derive(Debug)]
pub enum Step {
    Command(CommandStep),
    If {
        /// Each `IF`/`ELIF` with its condition and body.
        arms: Vec<(Span, Expr, Vec<Step>)>,
        otherwise: Option<Vec<Step>>,
    },
}

#[derive(Debug)]
pub struct CommandStep {
    pub line: u32,
    pub sleep: Option<Sleep>,
    /// `FwOpcodeType` then the arguments: what `fprime_v1.cmd` dispatches.
    pub buffer: Vec<u8>,
    /// Fail the sequence on a response other than `OK`, unless the line said `CONTINUE`.
    pub checked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sleep {
    Relative(u64),
    Absolute(u64),
}

/// A condition, as 0 or 1.
#[derive(Debug)]
pub enum Expr {
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Compare {
        op: RelOp,
        domain: Domain,
        left: Term,
        right: Term,
    },
}

/// One side of a comparison.
#[derive(Debug)]
pub enum Term {
    Read(Read),
    /// The last command's `Fw::CmdResponse`, an `i32`.
    LastCmd,
    /// Already in the comparison's domain.
    Const(Const),
}

/// A channel or parameter read, and the scalar within it a condition compares.
#[derive(Debug)]
pub struct Read {
    pub source: Source,
    pub id: u64,
    /// Where the scalar starts in the serialised value.
    pub offset: u32,
    pub scalar: Scalar,
    /// Exit code if the read comes back invalid.
    pub line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Telemetry,
    Parameter,
}

/// What is on the wire at [`Read::offset`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scalar {
    Int(IntegerKind),
    Float(FloatKind),
    /// True is anything but this byte.
    Bool {
        false_value: u8,
    },
}

/// The Wasm type and signedness two sides are compared in. Chosen so both sides convert to it
/// exactly, except for a 64-bit integer compared against a float.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    I32 { signed: bool },
    I64 { signed: bool },
    F32,
    F64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Const {
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
}

/// What a condition operand holds.
#[derive(Debug, Clone, Copy)]
enum Kind<'d> {
    Int(IntegerKind),
    Float(FloatKind),
    Bool,
    Enum(&'d EnumType, IntegerKind),
}

impl Kind<'_> {
    fn describe(&self) -> String {
        match self {
            Kind::Int(kind) => format!("{kind:?}"),
            Kind::Float(kind) => format!("{kind:?}"),
            Kind::Bool => "bool".into(),
            Kind::Enum(enumeration, _) => format!("{}, an enum", enumeration.qualified_name),
        }
    }

    /// The domain this kind is compared in against a constant.
    fn domain(&self) -> Domain {
        match self {
            Kind::Int(kind) | Kind::Enum(_, kind) => match kind {
                IntegerKind::U32 => Domain::I32 { signed: false },
                IntegerKind::U64 => Domain::I64 { signed: false },
                IntegerKind::I64 => Domain::I64 { signed: true },
                _ => Domain::I32 { signed: true },
            },
            Kind::Float(FloatKind::F32) => Domain::F32,
            Kind::Float(FloatKind::F64) => Domain::F64,
            Kind::Bool => Domain::I32 { signed: true },
        }
    }
}

/// A resolved operand: what to evaluate, what it holds, and how to name it in an error.
struct Typed<'d> {
    term: Term,
    kind: Kind<'d>,
    what: String,
}

enum Side<'a, 'd> {
    Typed(Typed<'d>),
    Literal(&'a ValueKind, Span),
}

pub fn resolve(
    statements: &[Stmt],
    dict: &Dictionary,
    diagnostics: &mut Vec<Diagnostic>,
) -> Program {
    let mut resolver = Resolver {
        wire: Wire::new(dict),
        dict,
        diagnostics,
        value_size: 0,
        first_last_cmd: None,
        any_continue: false,
    };
    let body = resolver.block(statements);

    if let (Some(span), false) = (resolver.first_last_cmd, resolver.any_continue) {
        resolver.diagnostics.push(Diagnostic::warning(
            span,
            "LAST_CMD is always OK here: no command in this sequence has CONTINUE, so a failed \
             command ends the sequence before LAST_CMD can see it",
        ));
    }

    let time_size = resolver
        .wire
        .fixed_size(&TypeName::QualifiedIdentifier {
            name: "Fw.TimeValue".into(),
        })
        .ok()
        .flatten()
        .unwrap_or(abi::TIME_SERIALIZED_SIZE);

    Program {
        body,
        value_size: resolver.value_size,
        time_size,
        uses_last_cmd: resolver.first_last_cmd.is_some(),
    }
}

struct Resolver<'d, 'x> {
    wire: Wire<'d>,
    dict: &'d Dictionary,
    diagnostics: &'x mut Vec<Diagnostic>,
    value_size: u32,
    first_last_cmd: Option<Span>,
    any_continue: bool,
}

/// What looking a name up found.
enum Lookup<'d, T> {
    Found(&'d T),
    Ambiguous(Vec<&'d str>),
    Missing,
}

/// `wanted` as a whole name, else as the unique tail of one.
fn lookup<'d, T>(items: &'d [T], name: impl Fn(&T) -> &str, wanted: &str) -> Lookup<'d, T> {
    if let Some(exact) = items.iter().find(|item| name(item) == wanted) {
        return Lookup::Found(exact);
    }
    let suffix = format!(".{wanted}");
    let matches: Vec<&T> = items
        .iter()
        .filter(|item| name(item).ends_with(&suffix))
        .collect();
    match matches.as_slice() {
        [] => Lookup::Missing,
        [one] => Lookup::Found(one),
        many => Lookup::Ambiguous(many.iter().map(|item| name(item)).collect()),
    }
}

fn ambiguous(what: &str, wanted: &str, candidates: &[&str]) -> String {
    const SHOWN: usize = 5;
    let mut list = candidates
        .iter()
        .take(SHOWN)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if candidates.len() > SHOWN {
        list.push_str(&format!(" and {} more", candidates.len() - SHOWN));
    }
    format!("`{wanted}` matches more than one {what}: {list}; write more of the name")
}

fn line(span: Span) -> u32 {
    u32::try_from(span.line).unwrap_or(u32::MAX)
}

/// A channel or a parameter, which conditions read the same way.
struct Point<'d> {
    name: &'d str,
    id: u64,
    type_name: &'d TypeName,
}

impl<'d> Resolver<'d, '_> {
    fn error(&mut self, span: Span, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic::error(span, message));
    }

    fn block(&mut self, statements: &[Stmt]) -> Vec<Step> {
        statements
            .iter()
            .filter_map(|statement| match statement {
                Stmt::Command(command) => self.command(command).map(Step::Command),
                Stmt::If(chain) => {
                    let arms: Vec<_> = chain
                        .arms
                        .iter()
                        .map(|arm| {
                            let condition = self.condition(&arm.condition, line(arm.span));
                            let body = self.block(&arm.body);
                            condition.map(|condition| (arm.span, condition, body))
                        })
                        .collect();
                    let otherwise = chain.otherwise.as_ref().map(|body| self.block(body));
                    // Something failed and was reported; nothing will be emitted.
                    let arms: Option<Vec<_>> = arms.into_iter().collect();
                    Some(Step::If {
                        arms: arms?,
                        otherwise,
                    })
                }
            })
            .collect()
    }

    fn command(&mut self, command: &super::parse::Command) -> Option<CommandStep> {
        let definition = match lookup(&self.dict.commands, |c| &c.name, &command.mnemonic) {
            Lookup::Found(definition) => definition,
            Lookup::Ambiguous(candidates) => {
                let message = ambiguous("command", &command.mnemonic, &candidates);
                self.error(command.mnemonic_span, message);
                return None;
            }
            Lookup::Missing => {
                self.error(
                    command.mnemonic_span,
                    format!("no command matches `{}`", command.mnemonic),
                );
                return None;
            }
        };

        // `CONTINUE` after a full set of arguments is the modifier, not an argument: a
        // command whose last argument is an enum with a `CONTINUE` constant still gets it.
        let params = &definition.formal_params;
        let mut args = command.args.as_slice();
        let mut checked = true;
        if args.len() == params.len() + 1
            && matches!(&args[args.len() - 1].kind, ValueKind::Name(name) if name == "CONTINUE")
        {
            checked = false;
            self.any_continue = true;
            args = &args[..args.len() - 1];
        }

        if args.len() != params.len() {
            let signature = params
                .iter()
                .map(|param| format!("{}: {}", param.name, encode::describe(&param.type_name)))
                .collect::<Vec<_>>()
                .join(", ");
            self.error(
                command.mnemonic_span,
                format!(
                    "{} takes {} argument{}{}, but {} {} given",
                    definition.name,
                    params.len(),
                    if params.len() == 1 { "" } else { "s" },
                    if signature.is_empty() {
                        String::new()
                    } else {
                        format!(" ({signature})")
                    },
                    args.len(),
                    if args.len() == 1 { "was" } else { "were" },
                ),
            );
            return None;
        }

        let opcode_type = TypeName::QualifiedIdentifier {
            name: "FwOpcodeType".into(),
        };
        let opcode = match self.wire.shape(&opcode_type) {
            Ok(Shape::Int(kind)) => encode::integer(kind, i128::from(definition.opcode)),
            _ => None,
        };
        let Some(mut buffer) = opcode else {
            self.error(
                command.mnemonic_span,
                format!(
                    "cannot encode opcode {:#x}: the dictionary needs an integer FwOpcodeType \
                     that holds it",
                    definition.opcode
                ),
            );
            return None;
        };

        let mut ok = true;
        for (param, arg) in params.iter().zip(args) {
            if let Err(diagnostic) =
                self.wire
                    .encode(&param.type_name, arg, &param.name, &mut buffer)
            {
                self.diagnostics.push(diagnostic);
                ok = false;
            }
        }
        if !ok {
            return None;
        }

        if buffer.len() > abi::CMD_MAX_PAYLOAD as usize {
            self.error(
                command.mnemonic_span,
                format!(
                    "this command encodes to {} bytes; the sequencer dispatches at most {}",
                    buffer.len(),
                    abi::CMD_MAX_PAYLOAD
                ),
            );
            return None;
        }

        let sleep = match command.time {
            TimeTag::Relative(0) => None,
            TimeTag::Relative(us) => Some(Sleep::Relative(us)),
            TimeTag::Absolute(us) => Some(Sleep::Absolute(us)),
        };

        Some(CommandStep {
            line: line(command.span),
            sleep,
            buffer,
            checked,
        })
    }

    /// Every operand is resolved even after one fails, so each problem is reported.
    fn condition(&mut self, condition: &Cond, line: u32) -> Option<Expr> {
        match condition {
            Cond::Invalid => None,
            Cond::Or(left, right) | Cond::And(left, right) => {
                let left = self.condition(left, line);
                let right = self.condition(right, line);
                let (left, right) = (Box::new(left?), Box::new(right?));
                Some(match condition {
                    Cond::Or(..) => Expr::Or(left, right),
                    _ => Expr::And(left, right),
                })
            }
            Cond::Not(inner) => Some(Expr::Not(Box::new(self.condition(inner, line)?))),
            Cond::Test(operand) => {
                let typed = match self.side(operand, line)? {
                    Side::Typed(typed) => typed,
                    Side::Literal(..) => {
                        self.error(
                            operand.span,
                            "a constant on its own is not a condition; test TLM, PRM or LAST_CMD",
                        );
                        return None;
                    }
                };
                if !matches!(typed.kind, Kind::Bool) {
                    self.error(
                        operand.span,
                        format!(
                            "{} is {}, not a bool; compare it with something, e.g. `{} > 0`",
                            typed.what,
                            typed.kind.describe(),
                            typed.what
                        ),
                    );
                    return None;
                }
                Some(Expr::Compare {
                    op: RelOp::Ne,
                    domain: Domain::I32 { signed: true },
                    left: typed.term,
                    right: Term::Const(Const::I32(0)),
                })
            }
            Cond::Compare(left, op, right) => self.compare(left, *op, right, line),
        }
    }

    fn compare(&mut self, left: &Operand, op: RelOp, right: &Operand, line: u32) -> Option<Expr> {
        let resolved_left = self.side(left, line);
        let resolved_right = self.side(right, line);
        let (resolved_left, resolved_right) = (resolved_left?, resolved_right?);

        let (domain, left_term, right_term, kind, what) = match (resolved_left, resolved_right) {
            (Side::Literal(..), Side::Literal(..)) => {
                self.error(
                    left.span,
                    "a comparison needs TLM, PRM or LAST_CMD on at least one side",
                );
                return None;
            }
            (Side::Typed(typed), Side::Literal(value, span)) => {
                let constant = self.constant(&typed, value, span)?;
                let domain = typed.kind.domain();
                (
                    domain,
                    typed.term,
                    Term::Const(constant),
                    typed.kind,
                    typed.what,
                )
            }
            (Side::Literal(value, span), Side::Typed(typed)) => {
                let constant = self.constant(&typed, value, span)?;
                let domain = typed.kind.domain();
                (
                    domain,
                    Term::Const(constant),
                    typed.term,
                    typed.kind,
                    typed.what,
                )
            }
            (Side::Typed(a), Side::Typed(b)) => {
                let Some(domain) = unify(&a.kind, &b.kind) else {
                    self.error(
                        left.span,
                        format!(
                            "cannot compare {} ({}) with {} ({}){}",
                            a.what,
                            a.kind.describe(),
                            b.what,
                            b.kind.describe(),
                            match (&a.kind, &b.kind) {
                                (Kind::Int(_), Kind::Int(_)) =>
                                    ": no integer type holds every value of both",
                                _ => "",
                            }
                        ),
                    );
                    return None;
                };
                (domain, a.term, b.term, a.kind, a.what)
            }
        };

        if !op.is_equality() && matches!(kind, Kind::Bool | Kind::Enum(..)) {
            self.error(
                left.span,
                format!(
                    "{what} is {}, which is compared with == or !=, not {}",
                    kind.describe(),
                    op.symbol()
                ),
            );
            return None;
        }

        Some(Expr::Compare {
            op,
            domain,
            left: left_term,
            right: right_term,
        })
    }

    fn side<'a>(&mut self, operand: &'a Operand, line: u32) -> Option<Side<'a, 'd>> {
        match &operand.kind {
            OperandKind::Literal(value) => {
                if matches!(value, ValueKind::Str(_)) {
                    self.error(
                        operand.span,
                        "strings cannot be compared; conditions compare numbers, bools and enums",
                    );
                    return None;
                }
                Some(Side::Literal(value, operand.span))
            }
            OperandKind::LastCmd => {
                self.first_last_cmd.get_or_insert(operand.span);
                match self.dict.type_definitions.get("Fw.CmdResponse") {
                    Some(TypeDefinition::Enum(responses)) => Some(Side::Typed(Typed {
                        term: Term::LastCmd,
                        // The host hands the response back as an `i32`, whatever the enum is
                        // serialised as.
                        kind: Kind::Enum(responses, IntegerKind::I32),
                        what: "LAST_CMD".into(),
                    })),
                    _ => {
                        self.error(
                            operand.span,
                            "LAST_CMD needs the Fw.CmdResponse enum, which the dictionary does \
                             not define",
                        );
                        None
                    }
                }
            }
            OperandKind::Telemetry(path) => {
                let points: Vec<Point<'d>> = self
                    .dict
                    .telemetry_channels
                    .iter()
                    .map(|c| Point {
                        name: &c.name,
                        id: c.id,
                        type_name: &c.type_name,
                    })
                    .collect();
                self.read(Source::Telemetry, &points, path, operand.span, line)
                    .map(Side::Typed)
            }
            OperandKind::Parameter(path) => {
                let points: Vec<Point<'d>> = self
                    .dict
                    .parameters
                    .iter()
                    .map(|p| Point {
                        name: &p.name,
                        id: p.id,
                        type_name: &p.type_name,
                    })
                    .collect();
                self.read(Source::Parameter, &points, path, operand.span, line)
                    .map(Side::Typed)
            }
        }
    }

    /// A channel or parameter, and the member or element within it the path reaches.
    fn read(
        &mut self,
        source: Source,
        points: &[Point<'d>],
        path: &Path,
        span: Span,
        line: u32,
    ) -> Option<Typed<'d>> {
        let (keyword, noun) = match source {
            Source::Telemetry => ("TLM", "telemetry channel"),
            Source::Parameter => ("PRM", "parameter"),
        };

        // The longest leading part of the dotted name that is a point; the rest are members.
        let parts: Vec<&str> = path.name.split('.').collect();
        let mut found = None;
        for take in (1..=parts.len()).rev() {
            let candidate = parts[..take].join(".");
            match lookup(points, |p| p.name, &candidate) {
                Lookup::Found(point) => {
                    found = Some((point, take));
                    break;
                }
                Lookup::Ambiguous(candidates) => {
                    let message = ambiguous(noun, &candidate, &candidates);
                    self.error(span, message);
                    return None;
                }
                Lookup::Missing => {}
            }
        }
        let Some((point, take)) = found else {
            self.error(span, format!("no {noun} matches `{}`", path.name));
            return None;
        };

        let accessors = parts[take..]
            .iter()
            .map(|member| (Accessor::Member((*member).to_string()), span))
            .chain(path.accessors.iter().cloned());

        // Sized first: every offset below lies within it, so cannot overflow.
        match self.wire.max_size(point.type_name) {
            Ok(size) => self.value_size = self.value_size.max(size),
            Err(message) => {
                self.error(span, message);
                return None;
            }
        }

        let mut what = format!("{keyword} {}", point.name);
        let mut ty: &'d TypeName = point.type_name;
        let mut offset = 0u32;
        // A struct member declared `[N] T` is N `T`s, indexed like an array.
        let mut member_array: Option<u32> = None;

        for (accessor, accessor_span) in accessors {
            match accessor {
                Accessor::Member(name) => {
                    if let Some(size) = member_array {
                        self.error(
                            accessor_span,
                            format!("{what} is an array of {size}; pick an element with [i] before `.{name}`"),
                        );
                        return None;
                    }
                    let structure = match self.wire.shape(ty) {
                        Ok(Shape::Struct(structure)) => structure,
                        Ok(_) => {
                            self.error(
                                accessor_span,
                                format!(
                                    "{what} is {}, which has no members; there is no `.{name}`",
                                    encode::describe(ty)
                                ),
                            );
                            return None;
                        }
                        Err(message) => {
                            self.error(accessor_span, message);
                            return None;
                        }
                    };
                    let Some(index) = structure.members.iter().position(|m| m.name == name) else {
                        self.error(
                            accessor_span,
                            format!(
                                "{what} is {}, which has no member `{name}`; its members are {}",
                                structure.qualified_name,
                                encode::member_names(structure)
                            ),
                        );
                        return None;
                    };
                    for before in &structure.members[..index] {
                        match self.wire.fixed_size(&before.type_name) {
                            Ok(Some(size)) => offset += size * before.size.unwrap_or(1),
                            Ok(None) => {
                                self.error(
                                    accessor_span,
                                    format!(
                                        "`{name}` comes after `{}`, which holds a string, so \
                                         where `{name}` lands in {what} depends on that string",
                                        before.name
                                    ),
                                );
                                return None;
                            }
                            Err(message) => {
                                self.error(accessor_span, message);
                                return None;
                            }
                        }
                    }
                    let member = &structure.members[index];
                    ty = &member.type_name;
                    member_array = member.size;
                    what = format!("{what}.{name}");
                }
                Accessor::Index(index) => {
                    let (element, size) = match member_array.take() {
                        Some(size) => (ty, size),
                        None => match self.wire.shape(ty) {
                            Ok(Shape::Array { element, size }) => (element, size),
                            Ok(_) => {
                                self.error(
                                    accessor_span,
                                    format!("{what} is {}, not an array", encode::describe(ty)),
                                );
                                return None;
                            }
                            Err(message) => {
                                self.error(accessor_span, message);
                                return None;
                            }
                        },
                    };
                    if index < 0 || index >= i128::from(size) {
                        self.error(
                            accessor_span,
                            format!(
                                "{what} has {size} element{}; [{index}] is outside it",
                                if size == 1 { "" } else { "s" }
                            ),
                        );
                        return None;
                    }
                    let element_size = match self.wire.fixed_size(element) {
                        Ok(Some(element_size)) => element_size,
                        Ok(None) => {
                            self.error(
                                accessor_span,
                                format!(
                                    "the elements of {what} hold strings, so where [{index}] \
                                     lands depends on them"
                                ),
                            );
                            return None;
                        }
                        Err(message) => {
                            self.error(accessor_span, message);
                            return None;
                        }
                    };
                    offset += element_size * index as u32;
                    ty = element;
                    what = format!("{what}[{index}]");
                }
            }
        }

        if let Some(size) = member_array {
            self.error(
                span,
                format!("{what} is an array of {size}; pick an element with [i]"),
            );
            return None;
        }

        let (scalar, kind) = match self.wire.shape(ty) {
            Ok(Shape::Int(kind)) => (Scalar::Int(kind), Kind::Int(kind)),
            Ok(Shape::Float(kind)) => (Scalar::Float(kind), Kind::Float(kind)),
            Ok(Shape::Bool) => (
                Scalar::Bool {
                    false_value: self.wire.false_value,
                },
                Kind::Bool,
            ),
            Ok(Shape::Enum(enumeration)) => match self.wire.representation(enumeration) {
                Ok(kind) => (Scalar::Int(kind), Kind::Enum(enumeration, kind)),
                Err(message) => {
                    self.error(span, message);
                    return None;
                }
            },
            Ok(Shape::Str { .. }) => {
                self.error(
                    span,
                    format!(
                        "{what} is {}; conditions compare numbers, bools and enums",
                        encode::describe(ty)
                    ),
                );
                return None;
            }
            Ok(Shape::Struct(structure)) => {
                self.error(
                    span,
                    format!(
                        "{what} is {}, a struct; compare one of its members: {}",
                        structure.qualified_name,
                        encode::member_names(structure)
                    ),
                );
                return None;
            }
            Ok(Shape::Array { size, .. }) => {
                self.error(
                    span,
                    format!(
                        "{what} is {}, an array of {size}; compare one element with [i]",
                        encode::describe(ty)
                    ),
                );
                return None;
            }
            Err(message) => {
                self.error(span, message);
                return None;
            }
        };

        Some(Typed {
            term: Term::Read(Read {
                source,
                id: point.id,
                offset,
                scalar,
                line,
            }),
            kind,
            what,
        })
    }

    /// `value` as a constant of the kind it is compared with, in that kind's domain.
    fn constant(&mut self, typed: &Typed<'d>, value: &ValueKind, span: Span) -> Option<Const> {
        let what = &typed.what;
        let mismatch = |found: &ValueKind| {
            format!(
                "{what} is {}, so it cannot be compared with {}",
                typed.kind.describe(),
                found.describe()
            )
        };

        let constant = match (typed.kind, value) {
            (Kind::Int(kind), ValueKind::Int(number)) => {
                let (low, high) = encode::range(kind);
                if *number < low || *number > high {
                    self.error(
                        span,
                        format!(
                            "{what} is {kind:?}, which never holds {number} ({})",
                            encode::range_text(kind)
                        ),
                    );
                    return None;
                }
                in_domain(typed.kind.domain(), *number)
            }
            (Kind::Float(kind), ValueKind::Int(_) | ValueKind::Float(_)) => {
                let number = match value {
                    ValueKind::Int(number) => *number as f64,
                    ValueKind::Float(number) => *number,
                    _ => unreachable!("matched above"),
                };
                match kind {
                    FloatKind::F32 => {
                        if (number as f32).is_infinite() {
                            self.error(span, format!("{what} is F32, which never holds {number}"));
                            return None;
                        }
                        Const::F32(number as f32)
                    }
                    FloatKind::F64 => Const::F64(number),
                }
            }
            (Kind::Bool, ValueKind::Bool(flag)) => Const::I32(i32::from(*flag)),
            (Kind::Enum(enumeration, representation), ValueKind::Name(name)) => {
                let Some(number) = encode::enum_constant(enumeration, name) else {
                    self.error(
                        span,
                        format!(
                            "{what} is {}, which has no constant `{name}`; it has {}",
                            enumeration.qualified_name,
                            encode::constants(enumeration)
                        ),
                    );
                    return None;
                };
                let (low, high) = encode::range(representation);
                if i128::from(number) < low || i128::from(number) > high {
                    self.error(
                        span,
                        format!(
                            "{}.{name} is {number}, outside its {representation:?} \
                             representation",
                            enumeration.qualified_name
                        ),
                    );
                    return None;
                }
                in_domain(typed.kind.domain(), i128::from(number))
            }
            (Kind::Enum(enumeration, _), other) => {
                self.error(
                    span,
                    format!(
                        "{what} is {}, an enum: compare it with one of its constants ({}), not {}",
                        enumeration.qualified_name,
                        encode::constants(enumeration),
                        other.describe()
                    ),
                );
                return None;
            }
            (Kind::Int(_), ValueKind::Float(_)) => {
                self.error(
                    span,
                    format!("{}; integers compare with integers", mismatch(value)),
                );
                return None;
            }
            (_, other) => {
                self.error(span, mismatch(other));
                return None;
            }
        };
        Some(constant)
    }
}

/// An integer known to fit the domain, as a constant in it.
fn in_domain(domain: Domain, number: i128) -> Const {
    match domain {
        Domain::I32 { signed: true } => Const::I32(number as i32),
        Domain::I32 { signed: false } => Const::I32(number as u32 as i32),
        Domain::I64 { signed: true } => Const::I64(number as i64),
        Domain::I64 { signed: false } => Const::I64(number as u64 as i64),
        Domain::F32 | Domain::F64 => unreachable!("integer kinds have integer domains"),
    }
}

/// Where two operands of these kinds can be compared, if anywhere.
fn unify(a: &Kind, b: &Kind) -> Option<Domain> {
    match (a, b) {
        (Kind::Enum(x, _), Kind::Enum(y, _)) => {
            (x.qualified_name == y.qualified_name).then(|| a.domain())
        }
        (Kind::Bool, Kind::Bool) => Some(Domain::I32 { signed: true }),
        (Kind::Bool | Kind::Enum(..), _) | (_, Kind::Bool | Kind::Enum(..)) => None,
        (Kind::Float(FloatKind::F32), Kind::Float(FloatKind::F32)) => Some(Domain::F32),
        (Kind::Float(_), _) | (_, Kind::Float(_)) => Some(Domain::F64),
        (Kind::Int(x), Kind::Int(y)) => {
            let holds = |domain: Domain, kind: IntegerKind| {
                let (low, high) = encode::range(kind);
                let (min, max) = match domain {
                    Domain::I32 { signed: true } => (i128::from(i32::MIN), i128::from(i32::MAX)),
                    Domain::I32 { signed: false } => (0, i128::from(u32::MAX)),
                    Domain::I64 { signed: true } => (i128::from(i64::MIN), i128::from(i64::MAX)),
                    Domain::I64 { signed: false } => (0, i128::from(u64::MAX)),
                    Domain::F32 | Domain::F64 => unreachable!("integer domains only"),
                };
                min <= low && high <= max
            };
            [
                Domain::I32 { signed: true },
                Domain::I32 { signed: false },
                Domain::I64 { signed: true },
                Domain::I64 { signed: false },
            ]
            .into_iter()
            .find(|domain| holds(*domain, *x) && holds(*domain, *y))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_kinds_meet_in_the_narrowest_domain_that_holds_both() {
        use IntegerKind::*;
        let i32s = Some(Domain::I32 { signed: true });
        let i32u = Some(Domain::I32 { signed: false });
        let i64s = Some(Domain::I64 { signed: true });
        let i64u = Some(Domain::I64 { signed: false });
        for (a, b, expected) in [
            (U8, I8, i32s),
            (U16, I32, i32s),
            (U32, U8, i32u),
            (U32, I32, i64s),
            (U32, I64, i64s),
            (U64, U32, i64u),
            (I64, I8, i64s),
            (U64, I8, None),
            (U64, I64, None),
        ] {
            assert_eq!(
                unify(&Kind::Int(a), &Kind::Int(b)),
                expected,
                "{a:?} with {b:?}"
            );
            assert_eq!(
                unify(&Kind::Int(b), &Kind::Int(a)),
                expected,
                "{b:?} with {a:?}"
            );
        }
    }

    #[test]
    fn floats_widen_unless_both_are_single() {
        let f32k = Kind::Float(FloatKind::F32);
        let f64k = Kind::Float(FloatKind::F64);
        assert_eq!(unify(&f32k, &f32k), Some(Domain::F32));
        assert_eq!(unify(&f32k, &f64k), Some(Domain::F64));
        assert_eq!(unify(&f32k, &Kind::Int(IntegerKind::U8)), Some(Domain::F64));
        assert_eq!(unify(&Kind::Bool, &f32k), None);
    }

    #[test]
    fn constants_land_in_the_domain() {
        assert_eq!(
            in_domain(Domain::I32 { signed: false }, 0xFFFF_FFFF),
            Const::I32(-1)
        );
        assert_eq!(
            in_domain(Domain::I64 { signed: false }, u64::MAX as i128),
            Const::I64(-1)
        );
        assert_eq!(in_domain(Domain::I32 { signed: true }, -5), Const::I32(-5));
    }

    #[test]
    fn lookup_prefers_whole_names_then_unique_tails() {
        let names = ["Ref.a.X", "Ref.b.X", "Ref.b.Y", "X"];
        let find = |wanted| match lookup(&names, |n| n, wanted) {
            Lookup::Found(found) => Ok(*found),
            Lookup::Ambiguous(candidates) => Err(candidates.len()),
            Lookup::Missing => Err(0),
        };
        assert_eq!(find("X"), Ok("X"), "a whole name wins over tails");
        assert_eq!(find("a.X"), Ok("Ref.a.X"));
        assert_eq!(find("Y"), Ok("Ref.b.Y"));
        assert_eq!(find(".X"), Err(0));
        assert_eq!(find("b"), Err(0), "tails are whole segments");
        let only_tails = ["Ref.a.X", "Ref.b.X"];
        assert!(matches!(
            lookup(&only_tails, |n| n, "X"),
            Lookup::Ambiguous(candidates) if candidates == ["Ref.a.X", "Ref.b.X"]
        ));
    }
}
