# 0005: Target families and order

Status: Accepted

## Context

Exit criteria require running designs on real hardware. The available boards are
the Nandland Go Board (iCE40HX1K), Alchitry Cu (iCE40HX8K), Alchitry Au
(XC7A35T) and Nexys A7 (XC7A100T). There is no ECP5 board.

## Decision

- iCE40 first (Milestone 1 on the Alchitry Cu), then Artix-7.
- ECP5 is out of scope for now. The architecture keeps room for it.
- Core data structures (ChipDB, `Arch` trait) are validated against Artix-7's
  scale early, so iCE40 assumptions do not leak into them.

## Consequences

The HX8K has no DSP blocks and 16 KB of block RAM, which shapes the Milestone 1
RISC-V target (RV32I, or RV32IM with an iterative multiplier).
