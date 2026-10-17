# synth

A unified, self-contained FPGA toolchain written in Rust. It takes Verilog-2005
or RTLIL (for example from Amaranth) to a programmable bitstream in one tool, with
no Yosys, nextpnr, Python or vendor tools at runtime.

Initial targets, in order:

| Family | Parts | Reference boards |
|---|---|---|
| Lattice iCE40 | LP / HX / UltraPlus | Alchitry Cu (HX8K), Nandland Go Board (HX1K) |
| Xilinx Artix-7 | XC7A35T – XC7A200T | Nexys A7 (XC7A100T), Alchitry Au (XC7A35T) |
| Lattice ECP5 | LFE5U / LFE5UM / LFE5UM5G | deferred |

The goal is to match or beat vendor tools (iCEcube2/Radiant, Diamond, Vivado) on
resource use and Fmax, with deterministic, reproducible results.

```
synth init --board alchitry-cu
synth build
synth prog
```

## Status

**Planning.** Nothing is implemented yet. See:

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): pipeline, IRs, algorithms,
  device database, verification strategy.
- [docs/ROADMAP.md](docs/ROADMAP.md): phases, exit criteria, first PRs, risks and
  open decisions.

Milestone 1: a pipelined RISC-V SoC built only with `synth`, running on the
Alchitry Cu.

## Licence

`synth` is licensed under the [EUPL-1.2](LICENSE). HDL kept in this repository
(example designs, test SoCs, soft IP) is licensed under CERN-OHL-W-2.0, as marked
in those files.

Bitstreams, netlists, reports and other output that `synth` produces from your
designs are yours. They are not covered by the licence of the toolchain.

## Acknowledgements

Device knowledge comes from the open bitstream documentation projects:
[Project IceStorm](https://github.com/YosysHQ/icestorm),
[Project Trellis](https://github.com/YosysHQ/prjtrellis) and
[Project X-Ray](https://github.com/f4pga/prjxray).
