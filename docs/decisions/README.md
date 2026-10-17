# Architecture decision records

Each file records one decision: the context, what was decided, and its
consequences. Records are append-only. To change a decision, add a new record
that supersedes the old one and update the old record's status line.

New records copy [`0000-template.md`](0000-template.md) and take the next number.

| # | Decision | Status |
|---|---|---|
| [0001](0001-rust.md) | Implementation language: Rust | Accepted |
| [0002](0002-dependencies.md) | Dependency policy: `std` only in shipped code | Accepted |
| [0003](0003-inputs.md) | Inputs: Verilog-2005, RTLIL and SRTL | Accepted |
| [0004](0004-licence.md) | Licence: EUPL-1.2 (code), CERN-OHL-W-2.0 (HDL) | Accepted |
| [0005](0005-targets.md) | Targets and order: iCE40, then Artix-7; ECP5 deferred | Accepted |
