# 0003: Input formats

Status: Accepted (extended by [0006](0006-native-hdl.md))

## Context

Milestone 1 needs Verilog. Designs written in Amaranth, or in SystemVerilog and
VHDL processed by Yosys plugins, can be reached through a lower-level format.

## Decision

- Verilog-2005 (synthesizable subset) is the HDL frontend.
- RTLIL is read directly. It serves Amaranth designs and the backend bring-up on
  netlists mapped by Yosys.
- SRTL, the text form of our own RTL IR, is read and written. It is used for
  tests and as a stable target for generators.
- SystemVerilog, FIRRTL and VHDL frontends are deferred. See Architecture §6.1
  for why FIRRTL is not a low-level format.

## Consequences

One process-lowering engine serves both Verilog `always` blocks and RTLIL
processes. The RTLIL reader pins tested Yosys and Amaranth versions because RTLIL
has no formal specification.
