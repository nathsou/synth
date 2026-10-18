# 0002: Dependency policy

Status: Accepted

## Context

The tool must be standalone: no Yosys, ABC, nextpnr, Python, Tcl or vendor tools
at runtime, and as few third-party crates as possible.

## Decision

- Shipped crates depend only on the Rust standard library.
- Dev-only tooling (database importers, benchmark harness) should also avoid
  crates; an exception needs its own decision record.
- The open bitstream documentation databases (Project IceStorm, Project X-Ray)
  are imported offline into our own ChipDB format. Re-deriving them would need
  the vendor tools anyway.
- Yosys, nextpnr, IceStorm, Icarus Verilog, Verilator and vendor tools may be
  used as test oracles in development and CI only. Yosys output (as RTLIL) may be
  used to bring up the backend before our frontend is ready.

## Consequences

We write our own SAT solver, FFT, hashing, parsers and diagnostics. These are
small, and owning them keeps determinism and performance under our control.
