//! `.seq` sequences compiled and run on the `spacewasm` interpreter `fprime-wasm test` uses,
//! against a scripted spacecraft: what they command, what they read, and how they end.

use fprime_dictionary::Dictionary;
use fprime_test::interpreter::{self, Call, Limits, Outcome, Report, Script, host::script};
use fprime_wasm::seq::{self, Diagnostic, MAX_CONTROL_FRAMES};
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

fn reference() -> &'static Dictionary {
    static DICTIONARY: OnceLock<Dictionary> = OnceLock::new();
    DICTIONARY.get_or_init(|| {
        fprime_dictionary::parse(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../fprime_dictionary/src/test/RefTopologyDictionary.json"
        )))
    })
}

/// What the spacecraft reports and how it answers commands, by dictionary name.
#[derive(Default)]
struct Spacecraft {
    telemetry: HashMap<i64, Vec<u8>>,
    parameters: HashMap<i64, Vec<u8>>,
    telemetry_status: HashMap<i64, i32>,
    parameter_status: HashMap<i64, i32>,
    responses: HashMap<u32, i32>,
}

impl Script for Spacecraft {
    fn command(&mut self, opcode: u32, _payload: &[u8]) -> Option<i32> {
        self.responses.get(&opcode).copied()
    }
    fn telemetry(&mut self, id: i64) -> Option<Vec<u8>> {
        self.telemetry.get(&id).cloned()
    }
    fn parameter(&mut self, id: i64) -> Option<Vec<u8>> {
        self.parameters.get(&id).cloned()
    }
    fn telemetry_status(&mut self, id: i64) -> Option<i32> {
        self.telemetry_status.get(&id).copied()
    }
    fn parameter_status(&mut self, id: i64) -> Option<i32> {
        self.parameter_status.get(&id).copied()
    }
}

/// The one dictionary entry whose name ends with `.{name}`.
fn find<'d, T>(items: &'d [T], name: impl Fn(&T) -> &str, wanted: &str) -> &'d T {
    let suffix = format!(".{wanted}");
    let found: Vec<&T> = items
        .iter()
        .filter(|item| name(item).ends_with(&suffix))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "expected one dictionary entry ending in {wanted}"
    );
    found[0]
}

impl Spacecraft {
    fn new() -> Self {
        Self::default()
    }

    fn tlm(mut self, dict: &Dictionary, name: &str, value: impl AsRef<[u8]>) -> Self {
        let id = find(&dict.telemetry_channels, |c| &c.name, name).id as i64;
        self.telemetry.insert(id, value.as_ref().to_vec());
        self
    }

    fn prm(mut self, dict: &Dictionary, name: &str, value: impl AsRef<[u8]>) -> Self {
        let id = find(&dict.parameters, |p| &p.name, name).id as i64;
        self.parameters.insert(id, value.as_ref().to_vec());
        self
    }

    fn tlm_status(mut self, dict: &Dictionary, name: &str, status: i32) -> Self {
        let id = find(&dict.telemetry_channels, |c| &c.name, name).id as i64;
        self.telemetry_status.insert(id, status);
        self
    }

    fn prm_status(mut self, dict: &Dictionary, name: &str, status: i32) -> Self {
        let id = find(&dict.parameters, |p| &p.name, name).id as i64;
        self.parameter_status.insert(id, status);
        self
    }

    fn respond(mut self, dict: &Dictionary, command: &str, response: i32) -> Self {
        let opcode = find(&dict.commands, |c| &c.name, command).opcode as u32;
        self.responses.insert(opcode, response);
        self
    }
}

const EXECUTION_ERROR: i32 = 4;

fn compile(source: &str, dict: &Dictionary) -> Vec<u8> {
    match seq::compile(source, dict) {
        Ok(compiled) => {
            assert_eq!(compiled.warnings, vec![], "compiling:\n{source}");
            compiled.wasm
        }
        Err(diagnostics) => panic!(
            "compiling:\n{source}\nfailed:\n{}",
            render(&diagnostics).join("\n")
        ),
    }
}

/// Compile, prove the module loads as the on-board interpreter would, then run it.
fn run_on(source: &str, dict: &Dictionary, spacecraft: Spacecraft) -> Report {
    let wasm = compile(source, dict);
    interpreter::validate(wasm.clone(), &Limits::default())
        .unwrap_or_else(|err| panic!("the module should load: {err:#}"));
    interpreter::run(wasm, &Limits::default(), Some(script::shared(spacecraft)))
        .unwrap_or_else(|err| panic!("the module should run: {err:#}"))
}

fn run(source: &str, spacecraft: Spacecraft) -> Report {
    run_on(source, reference(), spacecraft)
}

fn errors(source: &str, dict: &Dictionary) -> Vec<String> {
    match seq::compile(source, dict) {
        Ok(_) => panic!("expected errors compiling:\n{source}"),
        Err(diagnostics) => render(&diagnostics),
    }
}

fn render(diagnostics: &[Diagnostic]) -> Vec<String> {
    diagnostics.iter().map(ToString::to_string).collect()
}

/// Commands sent, as `(short name, payload)`.
fn commands(report: &Report, dict: &Dictionary) -> Vec<(String, Vec<u8>)> {
    report
        .recording
        .commands()
        .map(|(opcode, payload)| {
            let name = dict
                .commands
                .iter()
                .find(|c| c.opcode == u64::from(opcode))
                .map(|c| c.name.rsplit('.').next().unwrap().to_string())
                .unwrap_or_else(|| format!("{opcode:#x}"));
            (name, payload.to_vec())
        })
        .collect()
}

fn names(report: &Report) -> Vec<String> {
    commands(report, reference())
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// The text of each `CMD_NO_OP_STRING` sent, the marker the condition tests use.
fn markers(report: &Report) -> Vec<String> {
    commands(report, reference())
        .into_iter()
        .filter(|(name, _)| name == "CMD_NO_OP_STRING")
        .map(|(_, payload)| {
            let length = u16::from_be_bytes([payload[0], payload[1]]) as usize;
            String::from_utf8(payload[2..2 + length].to_vec()).unwrap()
        })
        .collect()
}

/// One `IF` per line of `cases`, each sending its label when it holds.
fn each_if(cases: &[(&str, &str)]) -> String {
    cases
        .iter()
        .map(|(condition, label)| {
            format!("IF {condition}\n    R00:00:00 CMD_NO_OP_STRING \"{label}\"\nENDIF\n")
        })
        .collect()
}

mod commanding {
    use super::*;

    #[test]
    fn arguments_encode_as_f_prime_serialises_them() {
        let report = run(
            "R00:00:00 CMD_NO_OP\n\
             R00:00:00 CMD_NO_OP_STRING \"Hello\"\n\
             R00:00:00 CMD_TEST_CMD_1 -3, 1.5, 255\n\
             R00:00:00 SEND_INTS 1 -2 3 -4 5 -6 7 -8\n\
             R00:00:00 SEND_BOOL true\n\
             R00:00:00 SEND_ALIAS 0x0A0B0C0D\n\
             R00:00:00 SEND_WIDE_CHOICE WIDE_LOW\n\
             R00:00:00 CHOICE_PAIR {secondChoice: Ref.Choice.BLUE, firstChoice: RED}\n\
             R00:00:00 SEND_NAMES [\"a\", \"bc\"]\n\
             R00:00:00 SEND_FLOATS 2, -0.5\n",
            Spacecraft::new(),
        );
        assert_eq!(report.outcome, Outcome::Returned);

        let bytes = |parts: &[&[u8]]| parts.concat();
        assert_eq!(
            commands(&report, reference()),
            [
                ("CMD_NO_OP".to_string(), vec![]),
                ("CMD_NO_OP_STRING".into(), bytes(&[&[0, 5], b"Hello"])),
                (
                    "CMD_TEST_CMD_1".into(),
                    bytes(&[&(-3i32).to_be_bytes(), &1.5f32.to_be_bytes(), &[255]])
                ),
                (
                    "SEND_INTS".into(),
                    bytes(&[
                        &[1],
                        &(-2i8).to_be_bytes(),
                        &3u16.to_be_bytes(),
                        &(-4i16).to_be_bytes(),
                        &5u32.to_be_bytes(),
                        &(-6i32).to_be_bytes(),
                        &7u64.to_be_bytes(),
                        &(-8i64).to_be_bytes(),
                    ])
                ),
                // FW_SERIALIZE_TRUE_VALUE: anything else is a FORMAT_ERROR on board.
                ("SEND_BOOL".into(), vec![0xFF]),
                ("SEND_ALIAS".into(), vec![0x0A, 0x0B, 0x0C, 0x0D]),
                (
                    "SEND_WIDE_CHOICE".into(),
                    (-4_294_967_296i64).to_be_bytes().to_vec()
                ),
                ("CHOICE_PAIR".into(), vec![0, 0, 0, 2, 0, 0, 0, 3]),
                ("SEND_NAMES".into(), bytes(&[&[0, 1], b"a", &[0, 2], b"bc"])),
                (
                    "SEND_FLOATS".into(),
                    bytes(&[&2.0f32.to_be_bytes(), &(-0.5f64).to_be_bytes()])
                ),
            ]
        );
    }

    #[test]
    fn names_may_be_full_or_a_unique_tail() {
        let report = run(
            "R00:00:00 CdhCore.cmdDisp.CMD_NO_OP\n\
             R00:00:00 cmdDisp.CMD_NO_OP\n\
             R00:00:00 CMD_NO_OP\n",
            Spacecraft::new(),
        );
        assert_eq!(names(&report), ["CMD_NO_OP"; 3]);
    }

    #[test]
    fn time_tags_sleep_before_the_command() {
        let report = run(
            "R00:00:00 CMD_NO_OP\n\
             R00:00:01.5 CMD_NO_OP\n\
             A2026-001T00:00:00 CMD_NO_OP\n",
            Spacecraft::new(),
        );
        let kinds: Vec<String> = report
            .recording
            .calls
            .iter()
            .map(|call| match call {
                Call::Command { .. } => "cmd".to_string(),
                Call::RelativeSleep { us } => format!("rsleep {us}"),
                Call::AbsoluteSleep { us } => format!("asleep {us}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "cmd",
                "rsleep 1500000",
                "cmd",
                // 2026-01-01T00:00:00Z
                "asleep 1767225600000000",
                "cmd",
            ]
        );
    }

    #[test]
    fn a_failed_command_ends_the_sequence_with_its_line() {
        let spacecraft =
            Spacecraft::new().respond(reference(), "CMD_NO_OP_STRING", EXECUTION_ERROR);
        let report = run(
            "; the third line fails\n\
             R00:00:00 CMD_NO_OP\n\
             R00:00:00 CMD_NO_OP_STRING \"fails\"\n\
             R00:00:00 CMD_NO_OP\n",
            spacecraft,
        );
        assert_eq!(report.outcome, Outcome::Exited(3));
        assert_eq!(names(&report), ["CMD_NO_OP", "CMD_NO_OP_STRING"]);
    }

    #[test]
    fn continue_carries_on_and_last_cmd_sees_the_failure() {
        let spacecraft =
            Spacecraft::new().respond(reference(), "CMD_NO_OP_STRING", EXECUTION_ERROR);
        let report = run(
            "IF LAST_CMD != OK\n\
               R00:00:00 CMD_TEST_CMD_1 0 0 0\n\
             ENDIF\n\
             R00:00:00 CMD_NO_OP_STRING \"fails\" CONTINUE\n\
             IF LAST_CMD == EXECUTION_ERROR\n\
               R00:00:00 CMD_NO_OP\n\
             ENDIF\n\
             IF LAST_CMD == OK\n\
               R00:00:00 CMD_NO_OP\n\
             ENDIF\n",
            spacecraft,
        );
        assert_eq!(report.outcome, Outcome::Returned);
        // LAST_CMD starts OK; the failure is seen; the checked CMD_NO_OP then sets it back.
        assert_eq!(
            names(&report),
            ["CMD_NO_OP_STRING", "CMD_NO_OP", "CMD_NO_OP"]
        );
    }

    #[test]
    fn identical_commands_share_one_buffer() {
        let once = compile("R00:00:00 CMD_NO_OP_STRING \"hello\"\n", reference());
        let thrice = compile(
            &"R00:00:00 CMD_NO_OP_STRING \"hello\"\n".repeat(3),
            reference(),
        );
        let memory = |wasm: Vec<u8>| {
            interpreter::validate(wasm, &Limits::default())
                .unwrap()
                .declared_memory
        };
        assert_eq!(memory(once), 4 + 2 + 5);
        assert_eq!(memory(thrice), 4 + 2 + 5);
    }

    #[test]
    fn an_empty_sequence_succeeds() {
        let report = run("; nothing to do\n", Spacecraft::new());
        assert_eq!(report.outcome, Outcome::Returned);
        assert!(report.recording.calls.is_empty());
    }
}

mod conditions {
    use super::*;

    fn by_value(u32_value: u32) -> Vec<String> {
        let spacecraft = Spacecraft::new().tlm(reference(), "ScalarU32Ch", u32_value.to_be_bytes());
        markers(&run(
            "IF TLM ScalarU32Ch < 10\n\
               R00:00:00 CMD_NO_OP_STRING \"low\"\n\
             ELIF TLM ScalarU32Ch < 20\n\
               R00:00:00 CMD_NO_OP_STRING \"middle\"\n\
             ELIF TLM ScalarU32Ch < 30\n\
               R00:00:00 CMD_NO_OP_STRING \"high\"\n\
             ELSE\n\
               R00:00:00 CMD_NO_OP_STRING \"off the scale\"\n\
             ENDIF\n\
             R00:00:00 CMD_NO_OP_STRING \"after\"\n",
            spacecraft,
        ))
    }

    #[test]
    fn exactly_one_arm_runs() {
        assert_eq!(by_value(5), ["low", "after"]);
        assert_eq!(by_value(10), ["middle", "after"]);
        assert_eq!(by_value(25), ["high", "after"]);
        assert_eq!(by_value(30), ["off the scale", "after"]);
    }

    #[test]
    fn integers_compare_by_their_own_signedness() {
        let dict = reference();
        let spacecraft = Spacecraft::new()
            .tlm(dict, "ScalarI8Ch", (-1i8).to_be_bytes())
            .tlm(dict, "ScalarU8Ch", 200u8.to_be_bytes())
            .tlm(dict, "ScalarI16Ch", (-2i16).to_be_bytes())
            .tlm(dict, "ScalarU16Ch", 65535u16.to_be_bytes())
            .tlm(dict, "ScalarI32Ch", (-5i32).to_be_bytes())
            .tlm(dict, "ScalarU32Ch", u32::MAX.to_be_bytes())
            .tlm(dict, "ScalarI64Ch", i64::MIN.to_be_bytes())
            .tlm(dict, "ScalarU64Ch", u64::MAX.to_be_bytes());
        let source = each_if(&[
            ("TLM ScalarI8Ch < 0", "i8 negative"),
            ("TLM ScalarI8Ch == -1", "i8 is -1"),
            ("TLM ScalarU8Ch > 127", "u8 above 127"),
            ("TLM ScalarU8Ch >= 201", "never"),
            ("TLM ScalarI16Ch == -2", "i16 is -2"),
            ("TLM ScalarU16Ch > 32767", "u16 above 32767"),
            ("TLM ScalarI32Ch < -4", "i32 below -4"),
            ("TLM ScalarU32Ch > 0x7FFFFFFF", "u32 above i32 max"),
            ("TLM ScalarU32Ch == 4294967295", "u32 is max"),
            ("TLM ScalarI64Ch < -9223372036854775807", "i64 is min"),
            ("TLM ScalarU64Ch > 9223372036854775807", "u64 above i64 max"),
            ("TLM ScalarU64Ch == 18446744073709551615", "u64 is max"),
            // Two channels: compared where both fit, not as raw bits.
            ("TLM ScalarU32Ch > TLM ScalarI32Ch", "u32 above i32"),
            ("TLM ScalarI8Ch < TLM ScalarU8Ch", "i8 below u8"),
            ("TLM ScalarU64Ch > TLM ScalarU8Ch", "u64 above u8"),
            ("TLM ScalarI64Ch < TLM ScalarU32Ch", "i64 below u32"),
            (
                "TLM ScalarU16Ch == TLM ScalarU16Ch",
                "a channel equals itself",
            ),
        ]);
        assert_eq!(
            markers(&run(&source, spacecraft)),
            [
                "i8 negative",
                "i8 is -1",
                "u8 above 127",
                "i16 is -2",
                "u16 above 32767",
                "i32 below -4",
                "u32 above i32 max",
                "u32 is max",
                "i64 is min",
                "u64 above i64 max",
                "u64 is max",
                "u32 above i32",
                "i8 below u8",
                "u64 above u8",
                "i64 below u32",
                "a channel equals itself",
            ]
        );
    }

    #[test]
    fn floats_compare_at_the_channels_precision() {
        let dict = reference();
        let spacecraft = Spacecraft::new()
            .tlm(dict, "ScalarF32Ch", 0.1f32.to_be_bytes())
            .tlm(dict, "ScalarF64Ch", 0.1f64.to_be_bytes())
            .tlm(dict, "ScalarU8Ch", 3u8.to_be_bytes());
        let source = each_if(&[
            // The literal is taken as an F32, as the channel is.
            ("TLM ScalarF32Ch == 0.1", "f32 is 0.1"),
            ("TLM ScalarF64Ch == 0.1", "f64 is 0.1"),
            (
                "TLM ScalarF32Ch < 1",
                "an integer literal compares with a float",
            ),
            // 0.1f32 is a little above 0.1 once widened.
            ("TLM ScalarF32Ch > TLM ScalarF64Ch", "f32 above f64"),
            ("TLM ScalarU8Ch > TLM ScalarF32Ch", "u8 above f32"),
            ("TLM ScalarF64Ch >= 0.2", "never"),
        ]);
        assert_eq!(
            markers(&run(&source, spacecraft)),
            [
                "f32 is 0.1",
                "f64 is 0.1",
                "an integer literal compares with a float",
                "f32 above f64",
                "u8 above f32",
            ]
        );
    }

    #[test]
    fn enums_struct_members_array_elements_and_parameters() {
        let dict = reference();
        let scalars: Vec<u8> = [
            &(-8i8).to_be_bytes()[..],
            &(-16i16).to_be_bytes(),
            &(-32i32).to_be_bytes(),
            &(-64i64).to_be_bytes(),
            &8u8.to_be_bytes(),
            &16u16.to_be_bytes(),
            &32u32.to_be_bytes(),
            &64u64.to_be_bytes(),
            &3.5f32.to_be_bytes(),
            &(-7.25f64).to_be_bytes(),
        ]
        .concat();
        let spacecraft = Spacecraft::new()
            .tlm(dict, "ScalarStructCh", &scalars)
            .tlm(dict, "ChoicesCh", [0, 0, 0, 2, 0, 0, 0, 3])
            .tlm(dict, "ChoiceCh", 1i32.to_be_bytes())
            .prm(dict, "CHOICE_PRM", 1i32.to_be_bytes())
            .prm(dict, "parameter1", 10u32.to_be_bytes());
        let source = each_if(&[
            ("TLM ScalarStructCh.i8 == -8", "i8 member"),
            ("TLM ScalarStructCh.i16 == -16", "i16 member"),
            ("TLM ScalarStructCh.i64 == -64", "i64 member"),
            ("TLM ScalarStructCh.u16 == 16", "u16 member"),
            ("TLM ScalarStructCh.u64 == 64", "u64 member"),
            ("TLM ScalarStructCh.f32 == 3.5", "f32 member"),
            ("TLM ScalarStructCh.f64 < -7", "f64 member"),
            ("TLM ScalarStructCh.u32 == TLM ScalarStructCh.i32", "never"),
            (
                "TLM ChoicesCh[0] == RED AND TLM ChoicesCh[1] == Ref.Choice.BLUE",
                "elements",
            ),
            ("TLM ChoiceCh == TWO", "enum constant"),
            ("TLM ChoiceCh != ONE", "enum inequality"),
            ("TLM ChoiceCh == PRM CHOICE_PRM", "enum against a parameter"),
            ("PRM parameter1 >= 10", "parameter"),
            (
                "PRM Ref.recvBuffComp.parameter1 < TLM ScalarStructCh.u8",
                "never",
            ),
        ]);
        assert_eq!(
            markers(&run(&source, spacecraft)),
            [
                "i8 member",
                "i16 member",
                "i64 member",
                "u16 member",
                "u64 member",
                "f32 member",
                "f64 member",
                "elements",
                "enum constant",
                "enum inequality",
                "enum against a parameter",
                "parameter",
            ]
        );
    }

    #[test]
    fn and_or_not_and_parentheses() {
        let dict = reference();
        let spacecraft = Spacecraft::new()
            .tlm(dict, "ScalarU8Ch", 1u8.to_be_bytes())
            .tlm(dict, "ScalarU16Ch", 0u16.to_be_bytes());
        let source = each_if(&[
            ("TLM ScalarU8Ch == 1 AND TLM ScalarU16Ch == 0", "and"),
            ("TLM ScalarU8Ch == 1 AND TLM ScalarU16Ch == 1", "never"),
            ("TLM ScalarU8Ch == 0 OR TLM ScalarU16Ch == 0", "or"),
            ("NOT TLM ScalarU8Ch == 0", "not"),
            ("NOT (TLM ScalarU8Ch == 1 OR TLM ScalarU16Ch == 1)", "never"),
            (
                "TLM ScalarU8Ch == 0 OR TLM ScalarU16Ch == 0 AND TLM ScalarU8Ch == 1",
                "and binds tighter",
            ),
            (
                "(TLM ScalarU8Ch == 0 OR TLM ScalarU16Ch == 0) AND NOT NOT TLM ScalarU8Ch == 1",
                "grouped",
            ),
        ]);
        assert_eq!(
            markers(&run(&source, spacecraft)),
            ["and", "or", "not", "and binds tighter", "grouped"]
        );
    }

    #[test]
    fn and_and_or_stop_at_the_side_that_decides() {
        let dict = reference();
        let id = |name| find(&dict.telemetry_channels, |c| &c.name, name).id as i64;
        // Every channel reads as zero.
        let report = run(
            "IF TLM ScalarU8Ch > 0 AND TLM ScalarU16Ch > 0\n\
               R00:00:00 CMD_NO_OP\n\
             ENDIF\n\
             IF TLM ScalarU32Ch == 0 OR TLM ScalarU64Ch > 0\n\
               R00:00:00 CMD_NO_OP\n\
             ENDIF\n",
            Spacecraft::new(),
        );
        let mut expected = vec![id("ScalarU8Ch"), id("ScalarU32Ch")];
        expected.sort();
        assert_eq!(report.recording.telemetry_read(), expected);
        assert_eq!(names(&report), ["CMD_NO_OP"]);
    }

    #[test]
    fn an_invalid_channel_ends_the_sequence_with_the_line_reading_it() {
        let spacecraft = Spacecraft::new().tlm_status(reference(), "ScalarU8Ch", 1);
        let report = run(
            "R00:00:00 CMD_NO_OP\n\
             IF TLM ScalarU16Ch == 0\n\
             ELIF TLM ScalarU8Ch > 0\n\
               R00:00:00 CMD_NO_OP_STRING \"unreachable\"\n\
             ENDIF\n",
            spacecraft.tlm(reference(), "ScalarU16Ch", 1u16.to_be_bytes()),
        );
        assert_eq!(report.outcome, Outcome::Exited(3));
        assert_eq!(names(&report), ["CMD_NO_OP"]);
    }

    #[test]
    fn a_parameter_must_be_valid_or_default() {
        for (status, outcome) in [
            (0, Outcome::Exited(1)), // UNINIT
            (1, Outcome::Returned),  // VALID
            (2, Outcome::Exited(1)), // INVALID
            (3, Outcome::Returned),  // DEFAULT
        ] {
            let spacecraft = Spacecraft::new().prm_status(reference(), "parameter1", status);
            let report = run("IF PRM parameter1 == 0\nENDIF\n", spacecraft);
            assert_eq!(report.outcome, outcome, "Fw::ParamValid {status}");
        }
    }
}

mod limits {
    use super::*;
    use wasm_encoder::{
        BlockType, CodeSection, ExportKind, ExportSection, Function, FunctionSection, Instruction,
        Module, TypeSection,
    };

    /// `main` with `blocks` nested `block`s and nothing else.
    fn nested_blocks(blocks: usize) -> Vec<u8> {
        let mut types = TypeSection::new();
        types.ty().function([], []);
        let mut functions = FunctionSection::new();
        functions.function(0);
        let mut exports = ExportSection::new();
        exports.export("main", ExportKind::Func, 0);
        let mut body = Function::new([]);
        for _ in 0..blocks {
            body.instruction(&Instruction::Block(BlockType::Empty));
        }
        for _ in 0..=blocks {
            body.instruction(&Instruction::End);
        }
        let mut code = CodeSection::new();
        code.function(&body);
        let mut module = Module::new();
        module
            .section(&types)
            .section(&functions)
            .section(&exports)
            .section(&code);
        module.finish()
    }

    #[test]
    fn the_frame_limit_is_the_interpreters() {
        // The function's own frame counts, so one fewer block than frames.
        let fits = interpreter::validate(nested_blocks(MAX_CONTROL_FRAMES - 1), &Limits::default());
        assert!(fits.is_ok(), "{fits:?}");
        let over = interpreter::validate(nested_blocks(MAX_CONTROL_FRAMES), &Limits::default());
        assert!(over.is_err(), "one frame more should not load");
    }

    /// `levels` IF/ELIF blocks, each holding the next in its first arm, around an IF whose
    /// condition is `condition`.
    fn nested(levels: usize, condition: &str) -> String {
        let mut source = String::new();
        for level in 0..levels {
            source += &format!("IF TLM ScalarU8Ch == 0 ; level {level}\n");
        }
        source += &format!("IF {condition}\n  R00:00:00 CMD_NO_OP\nENDIF\n");
        for _ in 0..levels {
            source += "ELIF TLM ScalarU8Ch == 255\nENDIF\n";
        }
        source
    }

    #[test]
    fn the_deepest_sequence_that_compiles_loads() {
        // 31 IF/ELIF levels open two frames each; with the function's own, the innermost IF
        // reaches 64, and one AND in its condition still fits.
        let deepest = nested(31, "TLM ScalarU8Ch == 0 AND TLM ScalarU16Ch == 0");
        let report = run(&deepest, Spacecraft::new());
        assert_eq!(report.outcome, Outcome::Returned);
        assert_eq!(names(&report), ["CMD_NO_OP"]);

        let deeper = nested(
            31,
            "TLM ScalarU8Ch == 0 AND (TLM ScalarU16Ch == 0 AND TLM ScalarU32Ch == 0)",
        );
        let found = errors(&deeper, reference());
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].starts_with("32:1: error: nested too deeply"),
            "{found:?}"
        );
    }

    #[test]
    fn elif_chains_do_not_nest() {
        let mut source = "IF TLM ScalarU8Ch == 0\n  R00:00:00 CMD_NO_OP_STRING \"0\"\n".to_string();
        for arm in 1..200 {
            source +=
                &format!("ELIF TLM ScalarU8Ch == {arm}\n  R00:00:00 CMD_NO_OP_STRING \"{arm}\"\n");
        }
        source += "ENDIF\n";
        let spacecraft = Spacecraft::new().tlm(reference(), "ScalarU8Ch", [199]);
        assert_eq!(markers(&run(&source, spacecraft)), ["199"]);
    }
}

mod diagnostics {
    use super::*;

    fn error(source: &str) -> String {
        let found = errors(source, reference());
        assert_eq!(found.len(), 1, "{found:?}");
        found.into_iter().next().unwrap()
    }

    #[test]
    fn commands() {
        assert_eq!(
            error("R00:00:00 CMD_NOOP"),
            "1:11: error: no command matches `CMD_NOOP`"
        );
        assert_eq!(
            error("R00:00:00 CMD_TEST_CMD_1 1 2"),
            "1:11: error: CdhCore.cmdDisp.CMD_TEST_CMD_1 takes 3 arguments (arg1: I32, arg2: F32, \
             arg3: U8), but 2 were given"
        );
        assert_eq!(
            error("R00:00:00 CMD_NO_OP 1"),
            "1:11: error: CdhCore.cmdDisp.CMD_NO_OP takes 0 arguments, but 1 was given"
        );
        assert_eq!(
            error("R00:00:00 CMD_TEST_CMD_1 1 2 256"),
            "1:30: error: `arg3` is U8, which cannot hold 256 (0 to 255)"
        );
        assert_eq!(
            error("R00:00:00 CMD_NO_OP_STRING \"this string is much longer than forty bytes\""),
            "1:28: error: `arg1` is string size 40, too small for this 43-byte string"
        );
        assert_eq!(
            error("R00:00:00 CHOICE_PAIR {firstChoice: RED, secondChoice: GREEN}"),
            "1:56: error: `choices.secondChoice` is Ref.Choice, which has no constant `GREEN`; \
             it has ONE, TWO, RED, BLUE"
        );
    }

    #[test]
    fn every_error_is_reported_in_one_run() {
        let found = errors(
            "R00:00:00 NOPE\n\
             IF TLM NOPE > 1\n\
             R00:00:00 CMD_NO_OP 1\n\
             ENDIF\n",
            reference(),
        );
        assert_eq!(found.len(), 3, "{found:?}");
    }

    #[test]
    fn conditions() {
        for (condition, expected) in [
            (
                "TLM ScalarU8Ch > 300",
                "1:21: error: TLM Ref.typeDemo.ScalarU8Ch is U8, which never holds 300 (0 to 255)",
            ),
            (
                "TLM ScalarU8Ch > 1.5",
                "1:21: error: TLM Ref.typeDemo.ScalarU8Ch is U8, so it cannot be compared with \
                 `1.5`; integers compare with integers",
            ),
            (
                "TLM ChoiceCh < RED",
                "1:4: error: TLM Ref.typeDemo.ChoiceCh is Ref.Choice, an enum, which is compared \
                 with == or !=, not <",
            ),
            (
                "TLM ChoiceCh == 2",
                "1:20: error: TLM Ref.typeDemo.ChoiceCh is Ref.Choice, an enum: compare it with \
                 one of its constants (ONE, TWO, RED, BLUE), not `2`",
            ),
            (
                "TLM ChoiceCh == PURPLE",
                "1:20: error: TLM Ref.typeDemo.ChoiceCh is Ref.Choice, which has no constant \
                 `PURPLE`; it has ONE, TWO, RED, BLUE",
            ),
            (
                "TLM ScalarU8Ch == \"x\"",
                "1:22: error: strings cannot be compared; conditions compare numbers, bools and \
                 enums",
            ),
            (
                "TLM NameCh == 1",
                "1:4: error: TLM Ref.typeDemo.NameCh is string size 40; conditions compare \
                 numbers, bools and enums",
            ),
            (
                "TLM ScalarStructCh > 1",
                "1:4: error: TLM Ref.typeDemo.ScalarStructCh is Ref.ScalarStruct, a struct; \
                 compare one of its members: i8, i16, i32, i64, u8, u16, u32, u64, f32, f64",
            ),
            (
                "TLM ScalarStructCh.u128 > 1",
                "1:4: error: TLM Ref.typeDemo.ScalarStructCh is Ref.ScalarStruct, which has no \
                 member `u128`; its members are i8, i16, i32, i64, u8, u16, u32, u64, f32, f64",
            ),
            (
                "TLM ChoicesCh == RED",
                "1:4: error: TLM Ref.typeDemo.ChoicesCh is Ref.ManyChoices, an array of 2; \
                 compare one element with [i]",
            ),
            (
                "TLM ChoicesCh[2] == RED",
                "1:17: error: TLM Ref.typeDemo.ChoicesCh has 2 elements; [2] is outside it",
            ),
            (
                "TLM ScalarU64Ch > TLM ScalarI8Ch",
                "1:4: error: cannot compare TLM Ref.typeDemo.ScalarU64Ch (U64) with \
                 TLM Ref.typeDemo.ScalarI8Ch (I8): no integer type holds every value of both",
            ),
            (
                "TLM ChoiceCh == TLM ScalarI32Ch",
                "1:4: error: cannot compare TLM Ref.typeDemo.ChoiceCh (Ref.Choice, an enum) \
                 with TLM Ref.typeDemo.ScalarI32Ch (I32)",
            ),
            (
                "TLM ScalarU8Ch",
                "1:4: error: TLM Ref.typeDemo.ScalarU8Ch is U8, not a bool; compare it with \
                 something, e.g. `TLM Ref.typeDemo.ScalarU8Ch > 0`",
            ),
            (
                "1 == 1",
                "1:4: error: a comparison needs TLM, PRM or LAST_CMD on at least one side",
            ),
            (
                "NoSuchChannel",
                "1:4: error: a constant on its own is not a condition; test TLM, PRM or LAST_CMD",
            ),
            (
                "TLM Scalar",
                "1:4: error: no telemetry channel matches `Scalar`",
            ),
            (
                "PRM ScalarU8Ch > 1",
                "1:4: error: no parameter matches `ScalarU8Ch`",
            ),
        ] {
            assert_eq!(
                error(&format!("IF {condition}\nENDIF")),
                expected,
                "IF {condition}"
            );
        }
    }

    #[test]
    fn last_cmd_without_continue_warns() {
        let compiled = seq::compile(
            "R00:00:00 CMD_NO_OP\nIF LAST_CMD == OK\nENDIF\n",
            reference(),
        )
        .unwrap();
        assert_eq!(
            render(&compiled.warnings),
            [
                "2:4: warning: LAST_CMD is always OK here: no command in this sequence has \
              CONTINUE, so a failed command ends the sequence before LAST_CMD can see it"
            ]
        );
    }
}

/// A dictionary for what the Ref fixture does not have: a bool channel, a non-default
/// `FW_SERIALIZE_TRUE_VALUE` and `FwSizeStoreType`, names that are only unique in full,
/// struct members after a string and member arrays, and an enum constant called `CONTINUE`.
mod configured {
    use super::*;
    use serde_json::json;

    fn integer(name: &str) -> serde_json::Value {
        json!({"name": name, "kind": "integer", "size": 8, "signed": name.starts_with('I')})
    }

    fn named(name: &str) -> serde_json::Value {
        json!({"name": name, "kind": "qualifiedIdentifier"})
    }

    fn dictionary() -> &'static Dictionary {
        static DICTIONARY: OnceLock<Dictionary> = OnceLock::new();
        DICTIONARY.get_or_init(|| {
            let bool_type = json!({"name": "bool", "kind": "bool", "size": 8});
            serde_json::from_value(json!({
                "metadata": {
                    "deploymentName": "Edge",
                    "frameworkVersion": "test",
                    "projectVersion": "test",
                    "libraryVersions": [],
                    "dictionarySpecVersion": "1.0.0"
                },
                "typeDefinitions": [
                    {"kind": "alias", "qualifiedName": "FwOpcodeType",
                     "type": integer("U32"), "underlyingType": integer("U32")},
                    {"kind": "alias", "qualifiedName": "FwSizeStoreType",
                     "type": integer("U32"), "underlyingType": integer("U32")},
                    {"kind": "enum", "qualifiedName": "Fw.CmdResponse",
                     "representationType": integer("U8"),
                     "enumeratedConstants": [{"name": "OK", "value": 0},
                                             {"name": "EXECUTION_ERROR", "value": 4}],
                     "default": "Fw.CmdResponse.OK"},
                    {"kind": "enum", "qualifiedName": "Edge.Mode",
                     "representationType": integer("U8"),
                     "enumeratedConstants": [{"name": "CONTINUE", "value": 7},
                                             {"name": "STOP", "value": 9}],
                     "default": "Edge.Mode.STOP"},
                    {"kind": "struct", "qualifiedName": "Edge.Health",
                     "members": {
                        "ok": {"type": bool_type, "index": 0},
                        "history": {"type": integer("I16"), "index": 1, "size": 3},
                        "level": {"type": integer("U16"), "index": 2}
                     }},
                    {"kind": "struct", "qualifiedName": "Edge.Labelled",
                     "members": {
                        "label": {"type": {"name": "string", "kind": "string", "size": 8}, "index": 0},
                        "after": {"type": integer("U8"), "index": 1}
                     }}
                ],
                "constants": [
                    {"qualifiedName": "FW_SERIALIZE_TRUE_VALUE", "type": integer("U8"), "value": 1},
                    {"qualifiedName": "FW_SERIALIZE_FALSE_VALUE", "type": integer("U8"), "value": 0}
                ],
                "commands": [
                    {"name": "Edge.a.FLAG", "commandKind": "async", "opcode": 1,
                     "formalParams": [{"name": "on", "type": bool_type, "ref": false}]},
                    {"name": "Edge.b.FLAG", "commandKind": "async", "opcode": 2,
                     "formalParams": [{"name": "on", "type": bool_type, "ref": false}]},
                    {"name": "Edge.a.NAME", "commandKind": "async", "opcode": 3,
                     "formalParams": [{"name": "name", "type": {"name": "string", "kind": "string", "size": 8}, "ref": false}]},
                    {"name": "Edge.a.SET_MODE", "commandKind": "async", "opcode": 4,
                     "formalParams": [{"name": "mode", "type": named("Edge.Mode"), "ref": false}]}
                ],
                "parameters": [
                    {"name": "Edge.a.LIMIT", "type": {"name": "F64", "kind": "float", "size": 64}, "id": 1}
                ],
                "telemetryChannels": [
                    {"name": "Edge.a.Enabled", "type": bool_type, "id": 1},
                    {"name": "Edge.a.Health", "type": named("Edge.Health"), "id": 2},
                    {"name": "Edge.a.Labelled", "type": named("Edge.Labelled"), "id": 3},
                    {"name": "Edge.a.Count", "type": integer("U8"), "id": 4},
                    {"name": "Edge.b.Count", "type": integer("U8"), "id": 5}
                ]
            }))
            .expect("the edge-case dictionary deserialises")
        })
    }

    fn sent(report: &Report) -> Vec<(String, Vec<u8>)> {
        commands(report, dictionary())
    }

    #[test]
    fn the_dictionary_says_how_bools_and_string_lengths_go_on_the_wire() {
        let report = run_on(
            "R00:00:00 a.FLAG true\nR00:00:00 a.FLAG false\nR00:00:00 a.NAME \"ab\"\n",
            dictionary(),
            Spacecraft::new(),
        );
        assert_eq!(
            sent(&report),
            [
                ("FLAG".to_string(), vec![1]),
                ("FLAG".to_string(), vec![0]),
                ("NAME".to_string(), vec![0, 0, 0, 2, b'a', b'b']),
            ]
        );
    }

    #[test]
    fn bool_channels() {
        let source = "IF TLM Enabled\n  R00:00:00 a.NAME \"yes\"\nENDIF\n\
                      IF NOT TLM Enabled\n  R00:00:00 a.NAME \"no\"\nENDIF\n\
                      IF TLM Enabled == true\n  R00:00:00 a.NAME \"true\"\nENDIF\n\
                      IF TLM Health.ok != TLM Enabled\n  R00:00:00 a.NAME \"differ\"\nENDIF\n";
        let on = Spacecraft::new().tlm(dictionary(), "Enabled", [1]);
        let off = Spacecraft::new().tlm(dictionary(), "Enabled", [0]);
        let payloads = |report: Report| -> Vec<String> {
            sent(&report)
                .into_iter()
                .map(|(_, payload)| String::from_utf8(payload[4..].to_vec()).unwrap())
                .collect()
        };
        assert_eq!(
            payloads(run_on(source, dictionary(), on)),
            ["yes", "true", "differ"]
        );
        assert_eq!(payloads(run_on(source, dictionary(), off)), ["no"]);
    }

    #[test]
    fn members_after_a_member_array() {
        let health = [
            &[1u8][..],
            &1i16.to_be_bytes(),
            &(-2i16).to_be_bytes(),
            &(-3i16).to_be_bytes(),
            &7u16.to_be_bytes(),
        ]
        .concat();
        let spacecraft = Spacecraft::new().tlm(dictionary(), "Health", health).prm(
            dictionary(),
            "LIMIT",
            2.5f64.to_be_bytes(),
        );
        let report = run_on(
            "IF TLM Health.history[2] == -3 AND TLM Health.level == 7 AND TLM Health.ok\n\
               R00:00:00 a.SET_MODE STOP\n\
             ENDIF\n\
             IF PRM LIMIT > 2 AND TLM Health.history[1] < TLM Health.history[0]\n\
               R00:00:00 a.SET_MODE STOP\n\
             ENDIF\n",
            dictionary(),
            spacecraft,
        );
        assert_eq!(sent(&report).len(), 2);
    }

    #[test]
    fn continue_after_a_full_argument_list_is_the_modifier() {
        let spacecraft = Spacecraft::new().respond(dictionary(), "SET_MODE", EXECUTION_ERROR);
        // The enum constant: still checked, so the failure ends the sequence.
        let report = run_on("R00:00:00 a.SET_MODE CONTINUE\n", dictionary(), spacecraft);
        assert_eq!(report.outcome, Outcome::Exited(1));
        assert_eq!(sent(&report), [("SET_MODE".to_string(), vec![7])]);

        // The constant, then the modifier.
        let spacecraft = Spacecraft::new().respond(dictionary(), "SET_MODE", EXECUTION_ERROR);
        let report = run_on(
            "R00:00:00 a.SET_MODE CONTINUE CONTINUE\nR00:00:00 a.SET_MODE STOP CONTINUE\n",
            dictionary(),
            spacecraft,
        );
        assert_eq!(report.outcome, Outcome::Returned);
        assert_eq!(
            sent(&report),
            [
                ("SET_MODE".to_string(), vec![7]),
                ("SET_MODE".to_string(), vec![9])
            ]
        );
    }

    #[test]
    fn errors() {
        for (source, expected) in [
            (
                "R00:00:00 FLAG true",
                "1:11: error: `FLAG` matches more than one command: Edge.a.FLAG, Edge.b.FLAG; \
                 write more of the name",
            ),
            (
                "IF TLM Count > 1\nENDIF",
                "1:4: error: `Count` matches more than one telemetry channel: Edge.a.Count, \
                 Edge.b.Count; write more of the name",
            ),
            (
                "IF TLM Labelled.after > 1\nENDIF",
                "1:4: error: `after` comes after `label`, which holds a string, so where `after` \
                 lands in TLM Edge.a.Labelled depends on that string",
            ),
            (
                "IF TLM Health.history > 1\nENDIF",
                "1:4: error: TLM Edge.a.Health.history is an array of 3; pick an element with [i]",
            ),
            (
                "IF TLM Health.history[3] > 1\nENDIF",
                "1:22: error: TLM Edge.a.Health.history has 3 elements; [3] is outside it",
            ),
            (
                "IF TLM Health.ok > false\nENDIF",
                "1:4: error: TLM Edge.a.Health.ok is bool, which is compared with == or !=, not >",
            ),
            (
                "R00:00:00 a.NAME \"123456789\"",
                "1:18: error: `name` is string size 8, too small for this 9-byte string",
            ),
        ] {
            let found = super::errors(source, dictionary());
            assert_eq!(found, [expected], "{source}");
        }
    }
}
