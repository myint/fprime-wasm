//! The resolved program as a WebAssembly 1.0 module for `Svc::WasmSequencer`.
//!
//! The module imports from `fprime_v1` only what the sequence uses and exports `main`, a
//! `[] -> []` function that runs the sequence top to bottom. Its memory, sized to the byte
//! with a 1-byte page, holds every command buffer (deduplicated) followed by scratch space
//! for one channel or parameter read:
//!
//! ```text
//! 0                data_len        + time_size          + value_size
//! | command buffers | Fw::Time scratch | value scratch      |
//! ```
//!
//! A failure exits the sequence with its line number: a command that does not respond `OK`,
//! or a channel or parameter that reads as invalid. Small helper functions keep each command
//! and read a handful of bytes:
//!
//! * `check(status, line)` — `exit(line)` if `status` is non-zero
//! * `tlm_read(id, line)` / `prm_read(id, line)` — a read into the value scratch, checked
//! * `be32(addr)` / `be64(addr)` — a big-endian load; Wasm loads are little-endian

use super::diag::{Diagnostic, Span};
use super::encode::signed;
use super::lex::RelOp;
use super::resolve::{
    CommandStep, Const, Domain, Expr, Program, Scalar, Sleep, Source, Step, Term,
};
use fprime_dictionary::{FloatKind, IntegerKind};
use fprime_test::abi;
use std::collections::HashMap;
use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection,
    Function, FunctionSection, ImportSection, Instruction, MemArg, MemorySection, MemoryType,
    Module, TypeSection, ValType,
};

/// Control frames `spacewasm` validates a function against, its body's own frame included:
/// `MAX_CONTROL_FRAMES` in `spacewasm_c_api`, which `Svc::WasmSequencer` loads with.
pub const MAX_CONTROL_FRAMES: usize = 64;

/// The module, or the `IF` that nests too deeply for the on-board validator.
pub fn generate(program: &Program) -> Result<Vec<u8>, Diagnostic> {
    let mut uses = Uses::default();
    uses.steps(&program.body);

    // Every distinct command buffer once, in first-use order.
    let mut data: Vec<u8> = vec![];
    let mut offsets: HashMap<&[u8], u32> = HashMap::new();
    uses.each_command(&program.body, &mut |command| {
        offsets.entry(&command.buffer).or_insert_with(|| {
            let offset = data.len() as u32;
            data.extend_from_slice(&command.buffer);
            offset
        });
    });

    let time_size = if uses.tlm { program.time_size } else { 0 };
    let value_size = if uses.tlm || uses.prm {
        program.value_size
    } else {
        0
    };
    let total = data.len() as u64 + u64::from(time_size) + u64::from(value_size);
    if total > u64::from(u32::MAX) {
        return Err(Diagnostic::error(
            Span::new(1, 1),
            format!("the sequence needs {total} bytes of memory, more than a module can address"),
        ));
    }
    let time_ptr = data.len() as u32;
    let value_ptr = time_ptr + time_size;
    let memory_size = value_ptr + value_size;

    let mut types = Types::default();
    let mut imports = ImportSection::new();
    let mut index = 0u32;
    let mut import = |used: bool, name: &str, params: &[ValType], results: &[ValType]| {
        used.then(|| {
            let ty = types.index(params, results);
            imports.import(abi::MODULE, name, EntityType::Function(ty));
            index += 1;
            index - 1
        })
    };
    use ValType::{I32, I64};
    let exit = import(uses.check, "exit", &[I32], &[]);
    let cmd = import(uses.cmd, "cmd", &[I32, I32], &[I32]);
    let tlm = import(uses.tlm, "tlm", &[I64, I32, I32, I32, I32], &[I32]);
    let prm = import(uses.prm, "prm", &[I64, I32, I32], &[I32]);
    let rsleep = import(uses.rsleep, "rsleep", &[I64], &[]);
    let asleep = import(uses.asleep, "asleep", &[I64], &[]);

    // Defined functions follow the imports, `main` first.
    let mut defined: Vec<(u32, Function)> = vec![];
    let mut next = index;
    let mut define = |used: bool| {
        used.then(|| {
            next += 1;
            next - 1
        })
    };
    let main = define(true).expect("main is always defined");
    let check = define(uses.check);
    let tlm_read = define(uses.tlm);
    let prm_read = define(uses.prm);
    let be32 = define(uses.be32 || uses.be64);
    let be64 = define(uses.be64);

    let emitter = Emitter {
        cmd,
        rsleep,
        asleep,
        check,
        tlm_read,
        prm_read,
        be32,
        be64,
        offsets: &offsets,
        value_ptr,
        last_cmd: program.uses_last_cmd.then_some(0),
    };

    let locals = if program.uses_last_cmd {
        vec![(1, I32)]
    } else {
        vec![]
    };
    let mut body = Function::new(locals);
    emitter.steps(&mut body, &program.body, 1)?;
    body.instruction(&Instruction::End);
    defined.push((types.index(&[], &[]), body));

    if let (Some(exit), Some(_)) = (exit, check) {
        let mut f = Function::new([]);
        f.instruction(&Instruction::LocalGet(0))
            .instruction(&Instruction::If(BlockType::Empty))
            .instruction(&Instruction::LocalGet(1))
            .instruction(&Instruction::Call(exit))
            .instruction(&Instruction::Unreachable)
            .instruction(&Instruction::End)
            .instruction(&Instruction::End);
        defined.push((types.index(&[I32, I32], &[]), f));
    }
    if let (Some(tlm), Some(check)) = (tlm, check) {
        let mut f = Function::new([]);
        f.instruction(&Instruction::LocalGet(0))
            .instruction(&Instruction::I32Const(time_ptr as i32))
            .instruction(&Instruction::I32Const(time_size as i32))
            .instruction(&Instruction::I32Const(value_ptr as i32))
            .instruction(&Instruction::I32Const(value_size as i32))
            .instruction(&Instruction::Call(tlm))
            // `Fw::TlmValid::VALID` is 0, so the status is itself the failure flag.
            .instruction(&Instruction::LocalGet(1))
            .instruction(&Instruction::Call(check))
            .instruction(&Instruction::End);
        defined.push((types.index(&[I64, I32], &[]), f));
    }
    if let (Some(prm), Some(check)) = (prm, check) {
        let mut f = Function::new([]);
        f.instruction(&Instruction::LocalGet(0))
            .instruction(&Instruction::I32Const(value_ptr as i32))
            .instruction(&Instruction::I32Const(value_size as i32))
            .instruction(&Instruction::Call(prm))
            // `Fw::ParamValid`: VALID (1) and DEFAULT (3) are values; UNINIT (0), INVALID (2)
            // and anything else are not. `| 2` folds the two good ones onto 3.
            .instruction(&Instruction::I32Const(2))
            .instruction(&Instruction::I32Or)
            .instruction(&Instruction::I32Const(3))
            .instruction(&Instruction::I32Ne)
            .instruction(&Instruction::LocalGet(1))
            .instruction(&Instruction::Call(check))
            .instruction(&Instruction::End);
        defined.push((types.index(&[I64, I32], &[]), f));
    }
    if be32.is_some() {
        let byte = |offset| {
            Instruction::I32Load8U(MemArg {
                offset,
                align: 0,
                memory_index: 0,
            })
        };
        let mut f = Function::new([]);
        f.instruction(&Instruction::LocalGet(0))
            .instruction(&byte(0))
            .instruction(&Instruction::I32Const(24))
            .instruction(&Instruction::I32Shl);
        for (offset, shift) in [(1, 16), (2, 8)] {
            f.instruction(&Instruction::LocalGet(0))
                .instruction(&byte(offset))
                .instruction(&Instruction::I32Const(shift))
                .instruction(&Instruction::I32Shl)
                .instruction(&Instruction::I32Or);
        }
        f.instruction(&Instruction::LocalGet(0))
            .instruction(&byte(3))
            .instruction(&Instruction::I32Or)
            .instruction(&Instruction::End);
        defined.push((types.index(&[I32], &[I32]), f));
    }
    if let (Some(_), Some(be32)) = (be64, be32) {
        let mut f = Function::new([]);
        f.instruction(&Instruction::LocalGet(0))
            .instruction(&Instruction::Call(be32))
            .instruction(&Instruction::I64ExtendI32U)
            .instruction(&Instruction::I64Const(32))
            .instruction(&Instruction::I64Shl)
            .instruction(&Instruction::LocalGet(0))
            .instruction(&Instruction::I32Const(4))
            .instruction(&Instruction::I32Add)
            .instruction(&Instruction::Call(be32))
            .instruction(&Instruction::I64ExtendI32U)
            .instruction(&Instruction::I64Or)
            .instruction(&Instruction::End);
        defined.push((types.index(&[I32], &[I64]), f));
    }

    let mut functions = FunctionSection::new();
    let mut code = CodeSection::new();
    for (ty, body) in &defined {
        functions.function(*ty);
        code.function(body);
    }

    let mut exports = ExportSection::new();
    exports.export(abi::ENTRY_POINT, ExportKind::Func, main);

    let mut module = Module::new();
    module.section(&types.section());
    module.section(&imports);
    module.section(&functions);
    if memory_size > 0 {
        let mut memories = MemorySection::new();
        memories.memory(MemoryType {
            minimum: u64::from(memory_size),
            maximum: Some(u64::from(memory_size)),
            memory64: false,
            shared: false,
            // A 1-byte page, so the memory is exactly what the sequence needs.
            page_size_log2: Some(0),
        });
        module.section(&memories);
        exports.export("memory", ExportKind::Memory, 0);
    }
    module.section(&exports);
    module.section(&code);
    if !data.is_empty() {
        let mut segments = DataSection::new();
        segments.active(0, &ConstExpr::i32_const(0), data.iter().copied());
        module.section(&segments);
    }
    Ok(module.finish())
}

/// Which imports and helpers the sequence needs.
#[derive(Default)]
struct Uses {
    cmd: bool,
    check: bool,
    tlm: bool,
    prm: bool,
    rsleep: bool,
    asleep: bool,
    be32: bool,
    be64: bool,
}

impl Uses {
    fn steps(&mut self, steps: &[Step]) {
        for step in steps {
            match step {
                Step::Command(command) => {
                    self.cmd = true;
                    self.check |= command.checked;
                    match command.sleep {
                        Some(Sleep::Relative(_)) => self.rsleep = true,
                        Some(Sleep::Absolute(_)) => self.asleep = true,
                        None => {}
                    }
                }
                Step::If { arms, otherwise } => {
                    for (_, condition, body) in arms {
                        self.expr(condition);
                        self.steps(body);
                    }
                    if let Some(otherwise) = otherwise {
                        self.steps(otherwise);
                    }
                }
            }
        }
    }

    fn expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Or(a, b) | Expr::And(a, b) => {
                self.expr(a);
                self.expr(b);
            }
            Expr::Not(a) => self.expr(a),
            Expr::Compare { left, right, .. } => {
                for term in [left, right] {
                    let Term::Read(read) = term else { continue };
                    self.check = true;
                    match read.source {
                        Source::Telemetry => self.tlm = true,
                        Source::Parameter => self.prm = true,
                    }
                    match read.scalar {
                        Scalar::Int(IntegerKind::U32 | IntegerKind::I32)
                        | Scalar::Float(FloatKind::F32) => self.be32 = true,
                        Scalar::Int(IntegerKind::U64 | IntegerKind::I64)
                        | Scalar::Float(FloatKind::F64) => self.be64 = true,
                        _ => {}
                    }
                }
            }
        }
    }

    fn each_command<'p>(&self, steps: &'p [Step], visit: &mut impl FnMut(&'p CommandStep)) {
        for step in steps {
            match step {
                Step::Command(command) => visit(command),
                Step::If { arms, otherwise } => {
                    for (_, _, body) in arms {
                        self.each_command(body, visit);
                    }
                    if let Some(otherwise) = otherwise {
                        self.each_command(otherwise, visit);
                    }
                }
            }
        }
    }
}

/// Function types, each once.
#[derive(Default)]
struct Types {
    known: Vec<(Vec<ValType>, Vec<ValType>)>,
}

impl Types {
    fn index(&mut self, params: &[ValType], results: &[ValType]) -> u32 {
        let wanted = (params.to_vec(), results.to_vec());
        let position = match self.known.iter().position(|known| *known == wanted) {
            Some(position) => position,
            None => {
                self.known.push(wanted);
                self.known.len() - 1
            }
        };
        position as u32
    }

    fn section(&self) -> TypeSection {
        let mut section = TypeSection::new();
        for (params, results) in &self.known {
            section
                .ty()
                .function(params.iter().copied(), results.iter().copied());
        }
        section
    }
}

/// How a loaded value sits on the operand stack before it is converted to a domain.
#[derive(Clone, Copy)]
enum Repr {
    I32 { signed: bool },
    I64 { signed: bool },
    F32,
    F64,
}

struct Emitter<'a> {
    cmd: Option<u32>,
    rsleep: Option<u32>,
    asleep: Option<u32>,
    check: Option<u32>,
    tlm_read: Option<u32>,
    prm_read: Option<u32>,
    be32: Option<u32>,
    be64: Option<u32>,
    offsets: &'a HashMap<&'a [u8], u32>,
    value_ptr: u32,
    /// The local holding the last command's response.
    last_cmd: Option<u32>,
}

/// Indices are assigned for exactly what [`Uses`] found, so a missing one is a bug here.
fn used(index: Option<u32>) -> u32 {
    index.expect("an import or helper the sequence uses was not defined")
}

impl Emitter<'_> {
    /// `depth` counts the control frames open around `steps`, the function's own included.
    fn steps(&self, f: &mut Function, steps: &[Step], depth: usize) -> Result<(), Diagnostic> {
        for step in steps {
            match step {
                Step::Command(command) => self.command(f, command),
                Step::If { arms, otherwise } => self.chain(f, arms, otherwise.as_deref(), depth)?,
            }
        }
        Ok(())
    }

    fn command(&self, f: &mut Function, command: &CommandStep) {
        match command.sleep {
            Some(Sleep::Relative(us)) => {
                f.instruction(&Instruction::I64Const(us as i64))
                    .instruction(&Instruction::Call(used(self.rsleep)));
            }
            Some(Sleep::Absolute(us)) => {
                f.instruction(&Instruction::I64Const(us as i64))
                    .instruction(&Instruction::Call(used(self.asleep)));
            }
            None => {}
        }

        let offset = self.offsets[command.buffer.as_slice()];
        f.instruction(&Instruction::I32Const(offset as i32))
            .instruction(&Instruction::I32Const(command.buffer.len() as i32))
            .instruction(&Instruction::Call(used(self.cmd)));

        match (command.checked, self.last_cmd) {
            (true, Some(last)) => {
                f.instruction(&Instruction::LocalTee(last));
                self.check_line(f, command.line);
            }
            (true, None) => self.check_line(f, command.line),
            (false, Some(last)) => {
                f.instruction(&Instruction::LocalSet(last));
            }
            (false, None) => {
                f.instruction(&Instruction::Drop);
            }
        }
    }

    /// `check(status, line)`, with the status already on the stack.
    fn check_line(&self, f: &mut Function, line: u32) {
        f.instruction(&Instruction::I32Const(line as i32))
            .instruction(&Instruction::Call(used(self.check)));
    }

    /// One `IF` with no `ELIF` is a Wasm `if`/`else`. With `ELIF`s, each arm is an `if` that
    /// branches out of one enclosing `block` when taken, so the frames in use stay at two
    /// however many arms there are.
    fn chain(
        &self,
        f: &mut Function,
        arms: &[(Span, Expr, Vec<Step>)],
        otherwise: Option<&[Step]>,
        depth: usize,
    ) -> Result<(), Diagnostic> {
        if let [(span, condition, body)] = arms {
            self.expr(f, condition, depth, *span)?;
            enter(depth + 1, *span)?;
            f.instruction(&Instruction::If(BlockType::Empty));
            self.steps(f, body, depth + 1)?;
            if let Some(otherwise) = otherwise {
                f.instruction(&Instruction::Else);
                self.steps(f, otherwise, depth + 1)?;
            }
            f.instruction(&Instruction::End);
            return Ok(());
        }

        let first = arms.first().map(|(span, _, _)| *span).unwrap_or_default();
        enter(depth + 1, first)?;
        f.instruction(&Instruction::Block(BlockType::Empty));
        for (i, (span, condition, body)) in arms.iter().enumerate() {
            self.expr(f, condition, depth + 1, *span)?;
            enter(depth + 2, *span)?;
            f.instruction(&Instruction::If(BlockType::Empty));
            self.steps(f, body, depth + 2)?;
            if i + 1 < arms.len() || otherwise.is_some() {
                // Out of the enclosing block: no later arm, and not the ELSE.
                f.instruction(&Instruction::Br(1));
            }
            f.instruction(&Instruction::End);
        }
        if let Some(otherwise) = otherwise {
            self.steps(f, otherwise, depth + 1)?;
        }
        f.instruction(&Instruction::End);
        Ok(())
    }

    /// Leaves 1 or 0 on the stack. AND and OR stop at the first side that decides, so a read
    /// on the other side is not made.
    fn expr(
        &self,
        f: &mut Function,
        expr: &Expr,
        depth: usize,
        span: Span,
    ) -> Result<(), Diagnostic> {
        match expr {
            Expr::Or(left, right) => {
                self.expr(f, left, depth, span)?;
                enter(depth + 1, span)?;
                f.instruction(&Instruction::If(BlockType::Result(ValType::I32)))
                    .instruction(&Instruction::I32Const(1))
                    .instruction(&Instruction::Else);
                self.expr(f, right, depth + 1, span)?;
                f.instruction(&Instruction::End);
            }
            Expr::And(left, right) => {
                self.expr(f, left, depth, span)?;
                enter(depth + 1, span)?;
                f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
                self.expr(f, right, depth + 1, span)?;
                f.instruction(&Instruction::Else)
                    .instruction(&Instruction::I32Const(0))
                    .instruction(&Instruction::End);
            }
            Expr::Not(inner) => {
                self.expr(f, inner, depth, span)?;
                f.instruction(&Instruction::I32Eqz);
            }
            Expr::Compare {
                op,
                domain,
                left,
                right,
            } => {
                self.term(f, left, *domain);
                self.term(f, right, *domain);
                f.instruction(&compare(*op, *domain));
            }
        }
        Ok(())
    }

    fn term(&self, f: &mut Function, term: &Term, domain: Domain) {
        let repr = match term {
            Term::Const(constant) => {
                f.instruction(&match *constant {
                    Const::I32(value) => Instruction::I32Const(value),
                    Const::I64(value) => Instruction::I64Const(value),
                    Const::F32(value) => Instruction::F32Const(value.into()),
                    Const::F64(value) => Instruction::F64Const(value.into()),
                });
                return;
            }
            Term::LastCmd => {
                f.instruction(&Instruction::LocalGet(used(self.last_cmd)));
                Repr::I32 { signed: true }
            }
            Term::Read(read) => {
                let helper = match read.source {
                    Source::Telemetry => self.tlm_read,
                    Source::Parameter => self.prm_read,
                };
                f.instruction(&Instruction::I64Const(read.id as i64))
                    .instruction(&Instruction::I32Const(read.line as i32))
                    .instruction(&Instruction::Call(used(helper)));
                self.load(f, read.scalar, self.value_ptr + read.offset)
            }
        };
        convert(f, repr, domain);
    }

    /// The big-endian scalar at `addr`, onto the stack.
    fn load(&self, f: &mut Function, scalar: Scalar, addr: u32) -> Repr {
        let byte = |f: &mut Function, addr: u32, signed: bool| {
            let memarg = MemArg {
                offset: 0,
                align: 0,
                memory_index: 0,
            };
            f.instruction(&Instruction::I32Const(addr as i32))
                .instruction(&if signed {
                    Instruction::I32Load8S(memarg)
                } else {
                    Instruction::I32Load8U(memarg)
                });
        };
        let call = |f: &mut Function, helper: Option<u32>| {
            f.instruction(&Instruction::I32Const(addr as i32))
                .instruction(&Instruction::Call(used(helper)));
        };

        match scalar {
            Scalar::Bool { false_value } => {
                byte(f, addr, false);
                f.instruction(&Instruction::I32Const(i32::from(false_value)))
                    .instruction(&Instruction::I32Ne);
                Repr::I32 { signed: false }
            }
            Scalar::Int(kind @ (IntegerKind::U8 | IntegerKind::I8)) => {
                byte(f, addr, signed(kind));
                Repr::I32 {
                    signed: signed(kind),
                }
            }
            Scalar::Int(kind @ (IntegerKind::U16 | IntegerKind::I16)) => {
                byte(f, addr, false);
                f.instruction(&Instruction::I32Const(8))
                    .instruction(&Instruction::I32Shl);
                byte(f, addr + 1, false);
                f.instruction(&Instruction::I32Or);
                if signed(kind) {
                    // Sign-extend from bit 15; `i32.extend16_s` is past Wasm 1.0.
                    f.instruction(&Instruction::I32Const(16))
                        .instruction(&Instruction::I32Shl)
                        .instruction(&Instruction::I32Const(16))
                        .instruction(&Instruction::I32ShrS);
                }
                Repr::I32 {
                    signed: signed(kind),
                }
            }
            Scalar::Int(kind @ (IntegerKind::U32 | IntegerKind::I32)) => {
                call(f, self.be32);
                Repr::I32 {
                    signed: signed(kind),
                }
            }
            Scalar::Int(kind @ (IntegerKind::U64 | IntegerKind::I64)) => {
                call(f, self.be64);
                Repr::I64 {
                    signed: signed(kind),
                }
            }
            Scalar::Float(FloatKind::F32) => {
                call(f, self.be32);
                f.instruction(&Instruction::F32ReinterpretI32);
                Repr::F32
            }
            Scalar::Float(FloatKind::F64) => {
                call(f, self.be64);
                f.instruction(&Instruction::F64ReinterpretI64);
                Repr::F64
            }
        }
    }
}

/// Fail if opening one more frame would put `depth` past what the interpreter validates.
fn enter(depth: usize, span: Span) -> Result<(), Diagnostic> {
    if depth > MAX_CONTROL_FRAMES {
        return Err(Diagnostic::error(
            span,
            format!(
                "nested too deeply: the on-board interpreter accepts at most \
                 {MAX_CONTROL_FRAMES} levels of blocks and conditions in a sequence, and \
                 this needs more. Nest fewer IFs, or split AND/OR conditions across them"
            ),
        ));
    }
    Ok(())
}

/// From how a value was loaded to the domain it is compared in.
fn convert(f: &mut Function, repr: Repr, domain: Domain) {
    let instruction = match (repr, domain) {
        (Repr::I32 { .. }, Domain::I32 { .. })
        | (Repr::I64 { .. }, Domain::I64 { .. })
        | (Repr::F32, Domain::F32)
        | (Repr::F64, Domain::F64) => return,
        (Repr::I32 { signed: true }, Domain::I64 { .. }) => Instruction::I64ExtendI32S,
        (Repr::I32 { signed: false }, Domain::I64 { .. }) => Instruction::I64ExtendI32U,
        (Repr::I32 { signed: true }, Domain::F64) => Instruction::F64ConvertI32S,
        (Repr::I32 { signed: false }, Domain::F64) => Instruction::F64ConvertI32U,
        (Repr::I64 { signed: true }, Domain::F64) => Instruction::F64ConvertI64S,
        (Repr::I64 { signed: false }, Domain::F64) => Instruction::F64ConvertI64U,
        (Repr::F32, Domain::F64) => Instruction::F64PromoteF32,
        _ => unreachable!("resolve only picks a domain both sides convert into exactly"),
    };
    f.instruction(&instruction);
}

fn compare(op: RelOp, domain: Domain) -> Instruction<'static> {
    use Instruction::*;
    match (domain, op) {
        (Domain::I32 { .. }, RelOp::Eq) => I32Eq,
        (Domain::I32 { .. }, RelOp::Ne) => I32Ne,
        (Domain::I32 { signed: true }, RelOp::Lt) => I32LtS,
        (Domain::I32 { signed: true }, RelOp::Le) => I32LeS,
        (Domain::I32 { signed: true }, RelOp::Gt) => I32GtS,
        (Domain::I32 { signed: true }, RelOp::Ge) => I32GeS,
        (Domain::I32 { signed: false }, RelOp::Lt) => I32LtU,
        (Domain::I32 { signed: false }, RelOp::Le) => I32LeU,
        (Domain::I32 { signed: false }, RelOp::Gt) => I32GtU,
        (Domain::I32 { signed: false }, RelOp::Ge) => I32GeU,
        (Domain::I64 { .. }, RelOp::Eq) => I64Eq,
        (Domain::I64 { .. }, RelOp::Ne) => I64Ne,
        (Domain::I64 { signed: true }, RelOp::Lt) => I64LtS,
        (Domain::I64 { signed: true }, RelOp::Le) => I64LeS,
        (Domain::I64 { signed: true }, RelOp::Gt) => I64GtS,
        (Domain::I64 { signed: true }, RelOp::Ge) => I64GeS,
        (Domain::I64 { signed: false }, RelOp::Lt) => I64LtU,
        (Domain::I64 { signed: false }, RelOp::Le) => I64LeU,
        (Domain::I64 { signed: false }, RelOp::Gt) => I64GtU,
        (Domain::I64 { signed: false }, RelOp::Ge) => I64GeU,
        (Domain::F32, RelOp::Eq) => F32Eq,
        (Domain::F32, RelOp::Ne) => F32Ne,
        (Domain::F32, RelOp::Lt) => F32Lt,
        (Domain::F32, RelOp::Le) => F32Le,
        (Domain::F32, RelOp::Gt) => F32Gt,
        (Domain::F32, RelOp::Ge) => F32Ge,
        (Domain::F64, RelOp::Eq) => F64Eq,
        (Domain::F64, RelOp::Ne) => F64Ne,
        (Domain::F64, RelOp::Lt) => F64Lt,
        (Domain::F64, RelOp::Le) => F64Le,
        (Domain::F64, RelOp::Gt) => F64Gt,
        (Domain::F64, RelOp::Ge) => F64Ge,
    }
}
