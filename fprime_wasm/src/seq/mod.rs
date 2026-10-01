//! `.seq` sequences: the command lists `Svc::CmdSequencer` runs, with conditionals, compiled
//! to a module `Svc::WasmSequencer` runs.
//!
//! ```text
//! ; Command lines are exactly what `fprime-seqgen` reads.
//! R00:00:00 CdhCore.cmdDisp.CMD_NO_OP
//! IF TLM Ref.typeDemo.ScalarF32Ch < 21.5 AND PRM Ref.typeDemo.CHOICE_PRM == RED
//!     R00:00:05 Ref.typeDemo.SEND_BOOL true
//! ELIF LAST_CMD != OK
//!     A2026-001T00:00:00 Ref.typeDemo.CHOICE BLUE CONTINUE
//! ELSE
//!     R00:00:01 CdhCore.cmdDisp.CMD_NO_OP_STRING "nominal"
//! ENDIF
//! ```
//!
//! Compiling one source file is a pipeline:
//!
//! * `lex` — characters into tokens, grouped by logical line
//! * `parse` — lines into statements, `IF` blocks nested
//! * `resolve` — names against the dictionary; each command encoded to its bytes, each
//!   condition typed
//! * `codegen` — the module, importing only the `fprime_v1` functions it uses
//!
//! The language is described in full in this crate's README.

mod codegen;
mod diag;
mod encode;
mod lex;
mod parse;
mod resolve;
mod time;

pub use codegen::MAX_CONTROL_FRAMES;
pub use diag::{Diagnostic, Level, Span};
pub use parse::MAX_NESTING;

use fprime_dictionary::Dictionary;

/// A compiled sequence.
#[derive(Debug)]
pub struct Compiled {
    /// The module: what `RUN` loads.
    pub wasm: Vec<u8>,
    /// Problems that did not stop compilation.
    pub warnings: Vec<Diagnostic>,
}

/// Compile one `.seq` source against a deployment's dictionary.
///
/// On failure, every diagnostic found — errors and warnings — in source order.
pub fn compile(source: &str, dictionary: &Dictionary) -> Result<Compiled, Vec<Diagnostic>> {
    let mut diagnostics = vec![];
    let lines = lex::lex(source, &mut diagnostics);
    let statements = parse::parse(&lines, &mut diagnostics);
    let program = resolve::resolve(&statements, dictionary, &mut diagnostics);

    let in_order = |mut diagnostics: Vec<Diagnostic>| {
        diagnostics.sort_by_key(|diagnostic| diagnostic.span);
        diagnostics
    };

    if diagnostics.iter().any(Diagnostic::is_error) {
        return Err(in_order(diagnostics));
    }
    match codegen::generate(&program) {
        Ok(wasm) => Ok(Compiled {
            wasm,
            warnings: in_order(diagnostics),
        }),
        Err(error) => {
            diagnostics.push(error);
            Err(in_order(diagnostics))
        }
    }
}
