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

### Coming from `Svc::CmdSequencer`

If you already write `.seq` files, there is little new to learn. Every command line you
write today means the same thing here. The additions let a sequence act on what the
spacecraft reports:

| You write | New? | Meaning |
|---|---|---|
| `R00:00:05 power.PWR_OFF` | Same as today | Wait, then dispatch a command. Stop the sequence if it fails |
| `; comment`, `\` at line end | Same as today | Comment, continue the line onto the next |
| `R00:00:05 power.PWR_OFF CONTINUE` | New | As above, but carry on even if the command fails |
| `IF <condition>` ... `ENDIF` | New | Run the lines between only if the condition holds |
| `ELIF <condition>`, `ELSE` | New | Alternatives inside an `IF` block. The first arm that holds runs, at most one |
| `TLM`, `PRM`, `LAST_CMD` | New | What a condition reads: telemetry, a parameter, the last command's response |

`IF`, `ELIF`, `ELSE` and `ENDIF` lines have no time tag, and indentation is optional.
What is different is where the sequence runs, not how you write it:

* The output is a Wasm module for `Svc::WasmSequencer`, not a binary sequence file for
  `Svc::CmdSequencer`. Use the `fprime-wasm seq` compiler in place of `fprime-seqgen`.
* A sequence that fails exits with the line number of what failed as its exit code. A
  sequence that reaches its end succeeds.
* There is no time base check, since a module has no header to carry one (see
  [What it compiles to](#what-it-compiles-to)).

Moving an existing sequence over is one command: `fprime-wasm seq existing.seq`. Then add
conditions where the old sequence had to assume. For example, a sequence that only turns a
heater on when it is cold:

```text
; Before: always turns the heater on.
R00:00:00 thermal.HEATER_ON

; After: only when it is cold, and say so either way.
IF TLM thermal.PanelTemp < -10.0
    R00:00:00 thermal.HEATER_ON
ELSE
    R00:00:00 cmdDisp.CMD_NO_OP_STRING "warm enough, heater left off"
ENDIF
```

### Commands

A command line is exactly what `fprime-seqgen` reads, so an existing `.seq` file
compiles unchanged.

```text
R01:00:01.050 cmdDisp.CMD_NO_OP_STRING "Awesome string!" ; and a comment
```

* **Time tag.** `RHH:MM:SS[.ffffff]` waits that long after the previous command
  completes (or after the sequence starts). `AYYYY-DDDTHH:MM:SS[.ffffff]` waits until
  that UTC time. `R00:00:00` dispatches at once. Any other wait is an `rsleep` or
  `asleep`, which the sequencer wakes from on its `checkTimers` tick. Hours run `00` to
  `23` in both, so the longest single relative wait is `R23:59:59.999999`.
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

Blocks nest, up to 32 deep. Keywords (`IF`, `ELIF`, `ELSE`, `ENDIF`, `AND`, `OR`, `NOT`,
`TLM`, `PRM`, `LAST_CMD`, `CONTINUE`) are upper case. Any part may be left out except `IF` and `ENDIF`,
and a block may be empty. A condition compares operands:

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

At least one side of a comparison is `TLM`, `PRM` or `LAST_CMD`: two constants are an
error. What the other side may be depends on the type the dictionary gives that side:

| Operand type | Compares with | Operators |
|---|---|---|
| Integer | An integer constant it can hold (`5`, not `5.0`), or an integer or float `TLM`/`PRM` | all |
| Float | An integer or float constant, or an integer or float `TLM`/`PRM` | all |
| Bool | `true`, `false`, or another bool | `==`, `!=` |
| Enum | One of its constants (`RED` or `Ref.Choice.RED`), or the same enum | `==`, `!=` |

Two integers are compared in a type that holds both exactly: `TLM a.U32 > TLM b.I32`
compares as 64-bit signed integers, not as raw bits. The one pair that has no such type,
`U64` against a signed integer, is an error. An integer `TLM`/`PRM` against a float one is
compared as an `F64`, which is exact up to 32 bits; a `U64` or `I64` beyond 2^53 is rounded
to the nearest `F64` first. Any constant against an `F32` channel, integer or float, is
taken as an `F32`, so `TLM x.F32 == 0.1` holds when the channel is `0.1`. Strings, and
whole structs and arrays, cannot be compared.

A channel or parameter that does not read as valid ends the sequence, with the line of
the `IF` or `ELIF` reading it as the exit code: telemetry must be `VALID`, a parameter
`VALID` or `DEFAULT`.

#### When a condition is read

A condition is evaluated the moment the sequence reaches it: as soon as the line before it
is done, or at the start of the sequence. It reads the latest value of each channel at that
instant. That is before the time tag of the first command inside it. In

```text
R00:00:00 cmdDisp.CMD_NO_OP
IF TLM power.BatteryVoltage < 21.5
    R00:10:00 power.PWR_OFF
ENDIF
```

the voltage is read right after `CMD_NO_OP` completes, and `PWR_OFF` follows ten minutes
later whatever the voltage is by then. An `IF` has no time tag of its own. To decide on a
fresher reading, or to give telemetry time to respond to a command, wait first with a
command of its own, for example a no-op, and put the `IF` after it:

```text
R00:00:00 thermal.HEATER_ON
R00:00:30 cmdDisp.CMD_NO_OP              ; wait 30 s for the temperature to respond
IF TLM thermal.PanelTemp < -10.0
    R00:00:00 cmdDisp.CMD_NO_OP_STRING "heater not working"
ENDIF
```

A relative time tag counts from the last command that actually ran. The first command
after an `IF` block, or inside the arm that is taken, waits from whichever command ran
last, which depends on the branch the sequence took.

#### `LAST_CMD` and `CONTINUE`

`LAST_CMD` is only useful after a command marked `CONTINUE`. Without `CONTINUE`, a failed
command ends the sequence before the next line can look at it, so `LAST_CMD` would always
be `OK`. The compiler warns when a sequence reads `LAST_CMD` but has no `CONTINUE`.
Pair them like this:

```text
R00:00:00 power.PWR_STATUS CONTINUE      ; carry on even if this fails
IF LAST_CMD == EXECUTION_ERROR
    R00:00:00 power.PWR_RESET
ELIF LAST_CMD != OK
    R00:00:00 cmdDisp.CMD_NO_OP_STRING "status rejected"
ENDIF
```

`LAST_CMD` is the response of the most recent command, whichever it was. It is not the
response of the most recent `CONTINUE` command. Check it right after the command you care
about.

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
