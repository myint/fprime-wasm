# fprime-wasm

Create and inspect [F Prime](https://github.com/nasa/fprime) Wasm sequence
projects, and compile `.seq` command sequences with conditionals.

```shell
cargo binstall fprime-wasm   # prebuilt binary. `cargo install fprime-wasm` builds from source
cargo binstall wasm-opt
```

## `init`

To initialize a fprime-wasm project:

```shell
mkdir ref-sequences && cd ref-sequences
fprime-wasm init
```

Scaffolds a sequence crate with all the supporting boilerplate.

[VS Code](https://code.visualstudio.com) is the recommended editor for
a generated sequences project.

## `add`

To add a new sequence:

```shell
fprime-wasm add safing
```

Creates `src/bin/safing.rs` and `tests/safing.rs`.

## `build`

```shell
fprime-wasm build
```

Compiles the sequences to `target/wasm32v1-none/release/*.wasm`. The only place
the Wasm target and the `wasm` feature are named, so a sequence author types
neither.

Both are needed together, and this is why `[build] target` is *not* in the
scaffolded `.cargo/config.toml`: it would apply to `cargo test` too, and a
`wasm32v1-none` test binary cannot be built at all because libtest needs `std`.
Leaving the host as the default target is what lets a sequence project have
ordinary host tests. `--debug` builds the unoptimised modules.

## `test`

```shell
fprime-wasm test [FILTER]
```

Builds the sequences, then runs the tests in `tests/`. Each test runs a compiled
sequence on the `spacewasm` interpreter and checks the conversation it has with
the spacecraft — the commands it sends, the responses it gets, the telemetry it
reads. `--no-build` uses the modules already on disk; `--debug` tests the debug
build.

The second half is a plain `cargo test`, so `cargo test` and an editor's test
runner work directly too.

## `verify`

```shell
fprime-wasm verify
```

Builds the sequences, then loads each module into a
[`spacewasm`](https://github.com/nasa/spacewasm) interpreter and sizes it
against `sequencer.toml` limits. `--no-build` uses the modules already on disk.

```
Limits (sequencer.toml): memory 8192 B, heap 8 pages, code 256 pages, operand stack 1024 words, page 8192 B

  Module                   Bytes  Memory  Heap  Code  Fits
  -----------------------  -----  ------  ----  ----  ----
  cmd_no_args                122     516     2     1  ok
  example                    641     941     2     2  ok
  mixed_max                 1956    1004     2     6  ok
  tlm_scalar_x8              973     788     2     3  ok

All 4 modules fit.
```

Nothing is executed: `verify` decodes the module, links it against the
`fprime_v1` host interface, instantiates it and stops before the first
instruction — everything the on-board interpreter checks at load time. That is
what makes it a cheap CI gate: exit status is non-zero if a module will not
load or overruns a budget, and no sequence has to be driven anywhere to find
out.

Two settings can only be sized by running a sequence, so they are not here:
`stackSize` (the operand stack's depth) and the guest stack. `fprime-wasm test`
measures both, along with what the sequence actually does.

`--verbose` expands each module: every budget's utilisation, and the guest and
interpreter figures.

### JSON

```shell
fprime-wasm verify --json | jq '.modules | max_by(.guest.declared_bytes) | .path'
```

`--json` reports the same run as one document on stdout, diagnostics on
stderr: `{schema, limits, modules, errors, summary}`, one entry per module
under `modules` with the same figures the tables show. `limits` echoes
`sequencer.toml` whole, `stackSize` included — it is the configuration, not a
measurement.

## `seq`

```shell
fprime-wasm seq safing.seq --dictionary RefTopologyDictionary.json
```

Compiles a `.seq` file, the command sequence format `fprime-seqgen` reads for
`Svc::CmdSequencer`, into `safing.wasm` for `Svc::WasmSequencer`. The language is
`.seq` with `IF`/`ELIF`/`ELSE` on telemetry, parameters and command responses.
Compiling one needs no Rust toolchain and no sequence project. Inside a project,
`--dictionary` defaults to the project's own dictionary. `--output` names the module
when compiling a single sequence. Otherwise each module is written next to its source.

```text
; Power down if the battery is low, otherwise report in.
R00:00:00 CdhCore.cmdDisp.CMD_NO_OP
IF TLM power.BatteryVoltage < 21.5 AND PRM power.MODE != SAFE
    R00:00:05 power.PWR_OFF
    R00:00:01 power.PWR_STATUS CONTINUE
    IF LAST_CMD != OK
        R00:00:00 cmdDisp.CMD_NO_OP_STRING "status failed"
    ENDIF
ELIF TLM power.Status.state == OFF
    R00:00:00 cmdDisp.CMD_NO_OP_STRING "already off"
ELSE
    A2026-001T12:00:00 cmdDisp.CMD_NO_OP_STRING "nominal"
ENDIF
```

Errors are reported together, as `file:line:column: error: ...`, and nothing is written
for a sequence that has any. Run `fprime-wasm verify safing.wasm` to size the module
against `sequencer.toml`, then upload it and `RUN` it like any other module.

### Commands

A command line is exactly what `fprime-seqgen` reads, so an existing `.seq` file
compiles unchanged.

```text
R01:00:01.050 cmdDisp.CMD_NO_OP_STRING "Awesome string!" ; and a comment
```

* **Time tag.** `RHH:MM:SS[.ffffff]` waits that long after the previous command
  completes (or after the sequence starts). `AYYYY-DDDTHH:MM:SS[.ffffff]` waits until
  that UTC time. `R00:00:00` dispatches at once. Any other wait is an `rsleep` or
  `asleep`, which the sequencer wakes from on its `checkTimers` tick.
* **Mnemonic.** The command's full dictionary name (`CdhCore.cmdDisp.CMD_NO_OP`) or any
  trailing part of it that is unique (`cmdDisp.CMD_NO_OP`, `CMD_NO_OP`). Telemetry
  channels and parameters are looked up the same way.
* **Arguments**, optionally separated by commas: numbers (`42`, `-7`, `0x1F`, `1_000`,
  `2.5e-1`), strings (`"..."` or `'...'`, taken verbatim as `fprime-seqgen` does),
  `true`/`false`, enum constants (`RED` or `Ref.Choice.RED`), arrays (`[1, 2, 3]`) and
  structs (`{first: RED, second: BLUE}`). Each is checked against the dictionary and
  serialised when the sequence is compiled. A value out of range, a string longer than
  its argument, or a missing struct member is an error, not a truncation.
* **Failure.** A command that responds other than `OK` ends the sequence: it exits with
  the command's line number as its code, which `Svc::WasmSequencer` reports and counts as
  a failure. Write `CONTINUE` after the arguments to carry on regardless. `CONTINUE`
  after a full argument list is always this modifier, even when the last argument is an
  enum that has a `CONTINUE` constant.

### Conditions

```text
IF <condition>
ELIF <condition>
ELSE
ENDIF
```

Blocks nest, up to 32 deep. Keywords are upper case. A condition compares operands:

| Operand | Is |
|---|---|
| `TLM <channel>` | A telemetry channel's current value |
| `PRM <parameter>` | A parameter's current value |
| `LAST_CMD` | The last command's `Fw::CmdResponse`: `OK`, `EXECUTION_ERROR`, ... `OK` before any command |
| `21.5`, `0x10`, `true`, `RED` | A constant |

A member or element is reached the way it is written: `TLM health.Status.history[2]`.
Members after a string are out of reach, since where they land depends on the string.

Comparisons are `==`, `!=`, `<`, `<=`, `>` and `>=`. Combine them with `AND`, `OR`, `NOT`
and parentheses. `NOT` binds tightest and `OR` loosest. `AND` and `OR` stop at the side
that decides, so a channel on the other side is not read. A bool operand is a condition
on its own: `IF TLM sys.Enabled`.

| Operand type | Compares with | Operators |
|---|---|---|
| Integer | An integer constant it can hold, or any other number | all |
| Float | A number, or any other number | all |
| Bool | `true`, `false`, or another bool | `==`, `!=` |
| Enum | One of its constants, or the same enum | `==`, `!=` |

Two integers are compared in a type that holds both exactly: `TLM a.U32 > TLM b.I32`
compares as 64-bit signed integers, not as raw bits. The one pair that has no such type,
`U64` against a signed integer, is an error. An integer against a float is compared as an
`F64`, which is exact up to 32 bits; a `U64` or `I64` beyond 2^53 is rounded to the
nearest `F64` first. A float constant against an `F32` channel
is taken as an `F32`, so `TLM x.F32 == 0.1` holds when the channel is `0.1`. Strings,
and whole structs and arrays, cannot be compared.

A channel or parameter that does not read as valid ends the sequence, with the line of
the `IF` or `ELIF` reading it as the exit code: telemetry must be `VALID`, a parameter
`VALID` or `DEFAULT`.

A condition is evaluated as soon as the line before it is done, before the time tag of
the first command inside it. In

```text
R00:00:00 cmdDisp.CMD_NO_OP
IF TLM power.BatteryVoltage < 21.5
    R00:10:00 power.PWR_OFF
ENDIF
```

the voltage is read right after `CMD_NO_OP` completes, and `PWR_OFF` follows ten minutes
later whatever the voltage is by then. To decide on a fresher reading, wait first with a
command of its own (`R00:10:00 cmdDisp.CMD_NO_OP`) and put the `IF` after it.

### What it compiles to

A `.seq` module imports only the `fprime_v1` functions it uses and exports `main`. Its
memory is sized to the byte. It holds each distinct command, already serialised, so a
repeated command costs only the call to send it, plus room for one value read. The
on-board interpreter validates a function against at most 64 nested blocks. Each `IF`,
and each `AND`/`OR` nested inside another, takes one, and each `ELIF` chain takes two
however long it is. A sequence that needs more is an error at compile time, not a module
that fails to load.

Unlike `Svc::CmdSequencer`, there is no time base check, since a module has no header
to carry one. Absolute times are compared against whatever the sequencer's clock
reports.

## `sequencer.toml`

`init` writes one at the crate root, holding the limits `verify` sizes against
and `test` runs under. Edit it to match the deployment that will fly the
sequences:

```toml
# Per WasmSequencer component instance (Svc::WasmSequencer::Config)
[config]
heap_pages = 8            # heapPages
guest_memory = 8192       # guestMemorySize, bytes
stack_size = 1024         # stackSize, 32-bit words
max_code_pages = 256      # maxCodePages
max_guest_modules = 8     # maxGuestModules

# Build-time configuration (set across deployment/project)
[constants]
page_size = 8192          # WASM_SEQ_SPACEWASM_PAGE_SIZE
event_message_max = 128   # Wasm.GUEST_EVENT_MESSAGE_SIZE
serial_ports = 5          # Wasm.MAX_SERIAL_IN_PORTS / MAX_SERIAL_OUT_PORTS

# `fprime-wasm test` settings: only a run uses these, since `verify` does not execute
[test]
max_instructions = 10000000
stack_sample = 1
```

`--limits <path>` measures against a different file, for trying a
configuration out without editing the tracked one. A deployment with more than
one `WasmSequencer` instance wants a file per instance; a test names the one it
is about with `#[fprime_test(sequence = "safing", limits = "sequencer-payload.toml")]`.

## License

Apache-2.0
