# 0006: A native HDL that targets the RTL IR

Status: Accepted

## Context

Verilog is the main way into synth (0003), and Verilog makes simple mistakes
easy: silent truncation and extension, signed and unsigned mixing, blocking
versus non-blocking assignments, inferred latches, implicit nets, unsynchronized
inputs, forgotten resets and fragile memory inference. Most of these are only
caught in simulation or on hardware.

Options considered:

- **Strict Verilog lints.** Cheap and useful to every current user, but they
  cannot express clock domains, enums or memory read semantics. Worth doing
  anyway.
- **Amaranth through RTLIL.** Already supported. It avoids several bug classes,
  but needs Python, still truncates silently on assignment, and does not check
  clock-domain crossings.
- **FIRRTL.** An IR emitted by Chisel, not a language people write. A reader is a
  second frontend (width and reset inference, `when` expansion, aggregate
  lowering), its spec moves with Chisel, and Chisel already reaches synth through
  `firtool` and Verilog. As a target for a new language, it would lose clock
  domains and memory read behaviour, which the RTL IR represents directly.
- **A native HDL compiled straight to the RTL IR.** The IR already has explicit
  flip-flop enables and resets, power-on values, memories with per-port
  read-during-write behaviour, and the convention that the frontend sizes
  operands.

## Decision

- Add a native HDL, **Weft** (working name), specified in
  [`docs/WEFT.md`](../WEFT.md). Its central choices:
  - exact arithmetic with explicit wrapping, and value ranges as integer types
    (including `index<N>`), with indices checked at compile time;
  - registers declared with a reset or power-on value; resets and their style
    are a property of the clock domain;
  - clock domains as part of the type system, implicit when a module has only
    one, with top-level inputs asynchronous until synchronized;
  - one assignment model (`=` for combinational signals, `:=` for next state),
    complete-assignment checking, exhaustive `match`;
  - `mem` as a declared storage kind whose read latency is visible in the source;
  - extern declarations for Verilog modules and device primitives.
- It is compiled by a new crate, `synth-weft` (lexer, parser, checker,
  elaboration to `synth_rtl::Module`), under the dependency policy of 0002.
- It extends the inputs of 0003. Verilog, RTLIL and SRTL remain.
- The scope of version 0.1 is the spec as written. The features listed in its
  §19 are left out until there is demand.

## Consequences

- There is no external oracle for the language. It is tested by:
  - porting `tests/designs` and the examples to Weft and comparing them with
    their Verilog originals, by differential simulation of the elaborated RTL
    (written back as Verilog, run in Icarus) and by equivalence checking;
  - golden diagnostics for each error class in the spec;
  - unit tests of range arithmetic, exhaustiveness and tag checking.
- Until synth has a simulator, Weft users simulate through
  `synth elab --verilog`. The Verilog writer must keep source names readable for
  waveforms.
- Range arithmetic needs arbitrary-precision integers. A small implementation
  goes in `synth-core`.
- Extern Verilog modules are elaborated by `synth-elab` with parameter overrides
  and inlined. String parameters already work there.
- Each `arch-*` crate exports extern declarations for its primitives.
- The language is a long-term commitment: spec, diagnostics, editor support and
  standard library. To keep it small, new features need a use case that the
  current language cannot express well.
- Suggested order: (1) one domain, integers, `bool`, enums, `reg`, `match`,
  instances and functions, enough to port `blinky_ram`; (2) multiple domains,
  `sync`, extern modules, enough for `go_board_serv`; (3) memories, generics,
  structs and arrays.
