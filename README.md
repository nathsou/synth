# synth

A unified, self-contained FPGA toolchain written in Rust. It takes Verilog to a
programmable bitstream in one tool, with no Yosys, nextpnr, Python or vendor tools
at runtime.

Initial targets:

| Family | Parts | Reference board |
|---|---|---|
| Lattice iCE40 | LP / HX / UltraPlus | iCEBreaker (UP5K) |
| Lattice ECP5 | LFE5U / LFE5UM / LFE5UM5G | ULX3S |
| Xilinx Artix-7 | XC7A35T – XC7A200T | Arty A7 |

The goal is to match or beat vendor tools (iCEcube2/Radiant, Diamond, Vivado) on
resource use and Fmax, with deterministic, reproducible results.

```
synth init --board icebreaker
synth build
synth prog
```

## Status

**Planning.** Nothing is implemented yet. See:

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): pipeline, IRs, algorithms,
  device database, verification strategy.
- [docs/ROADMAP.md](docs/ROADMAP.md): phases, exit criteria, first PRs, risks and
  open decisions.

Milestone 1: a pipelined RISC-V SoC built only with `synth`, running on an
iCEBreaker.

## Acknowledgements

Device knowledge comes from the open bitstream documentation projects:
[Project IceStorm](https://github.com/YosysHQ/icestorm),
[Project Trellis](https://github.com/YosysHQ/prjtrellis) and
[Project X-Ray](https://github.com/f4pga/prjxray).
