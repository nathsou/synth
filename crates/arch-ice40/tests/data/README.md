# Test data

Reference files produced by the open-source flow, used only to check that our
bitstream code is bit-exact with Project IceStorm. They contain no third-party
source code.

| File | Produced by |
|---|---|
| `hx1k_blinky_ram.asc` | Yosys 0.33 `synth_ice40` + nextpnr-ice40 0.6, `--hx1k --package vq100` (Go Board pins) |
| `hx8k_blinky_ram.asc` | Same, `--hx8k --package cb132` (Alchitry Cu pins) |
| `*.bin` | `icepack` (IceStorm 2023-02-18) from the matching `.asc` |

The design is a counter driving LEDs through an initialized, writable 512x8
block RAM, so the files exercise logic, IO, global and RAM tiles plus BRAM
initialization data.
