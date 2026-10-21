# 0008: A compiled cycle simulator, `synth-sim`

Status: Accepted

## Context

Every stage of the flow is checked by simulation, but synth cannot simulate
anything itself. The RTL, the synthesized netlists and the decoded bitstreams
are written out as Verilog and run in Icarus, under Python harnesses
(`tools/rtlsim.py`, `tools/bitsim.py`, `tools/ci_*.sh`). Weft users simulate
through `synth elab --verilog` (WEFT.md §17), and the NES in Weft runs in
Verilator because it needs tens of millions of cycles. The only native
simulator is a test helper in `synth-weft`, which interprets cells on
heap-allocated vectors and re-evaluates the whole design after every change.

ARCHITECTURE.md §4 planned two simulators: a 4-state event-driven one for RTL
and a 2-state compiled cycle simulator for netlists. The RTL IR has no delays
and no processes, only flat combinational cells, flip-flops, latches and
memories, so event-driven scheduling would only add cost.

Constraints: `std` only (0002), no `unsafe` outside ChipDB views and SIMD
kernels, deterministic results, and the code must build for WebAssembly.

## Decision

- One simulator, `synth-sim`, compiles a `synth_rtl::Module` into a flat
  program over a buffer of 64-bit words: each value of up to 64 bits is one
  word, wider values are consecutive words, and 4-state values carry a second
  plane with `LogicVec`'s encoding. 4-state or 2-state is chosen when the
  program is built. 2-state follows `opt::two_state`: unknown constants,
  initial values and out-of-range reads are 0.
- Scheduling is static. Combinational cells are ordered once; strongly
  connected components (combinational loops, latches) iterate to a fixpoint.
  Each operation records which sources (inputs, clock domains, memories)
  reach it, and an evaluation runs only the operations reached by what
  changed, in the precomputed order.
- Clocks are signals, not a separate mechanism: after settling, edges on
  clock bits fire their domains. Every fired domain samples before any
  commits, and settling repeats until no clock changes, which covers both
  edges of one clock, several clocks and derived clocks.
- The program has backends: an interpreter that is always available
  (tests, CI, the playground), and C source output (`synth sim --emit-c`) for
  long runs, compiled by the user's C compiler like a Verilator model. There
  is no JIT: it would need `unsafe`, two instruction sets and does not run in
  WebAssembly.
- Gate-level netlists are simulated by turning primitives back into RTL
  cells. Each architecture crate supplies its primitive models as RTL;
  `synth-sim` depends on no architecture.
- The semantics of every operation are those of `synth_rtl::eval`, checked by
  property tests on random 4-state values.
- Icarus and Verilator stay as test oracles. `eval` also drives constant
  folding, so a bug there would be shared by synthesis and the simulator and
  only an independent simulator can find it.

## Consequences

- Differential checks (RTL against netlists against bitstreams) can run in
  process, on many seeds, without Python or Icarus in the loop.
- Weft gains a simulator: `synth sim` runs a design with a stimulus, writes
  VCD, or emits a C model.
- The C backend makes the performance of long simulations depend on a C
  compiler, which synth itself never needs.
- External memories (a cartridge, a firmware ROM) are modelled as memories
  attached to ports, so they run inside the program in every backend.
