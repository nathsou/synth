# Third-party test designs

Unmodified copies of open-source RISC-V cores, used only as test inputs
(differential simulation of the frontend, and later end-to-end hardware
tests). Each directory keeps its upstream licence file.

| Directory | Upstream | Commit | Licence |
|-----------|----------|--------|---------|
| `serv/` | https://github.com/olofk/serv | `f200eb2ed7b69ac1c6b8eddd47654522aeee5ce8` | ISC |
| `darkriscv/` | https://github.com/darklife/darkriscv | `955511961f139c54d3385063cd9bf40f4dddadd8` | BSD-3-Clause |
| `hazard3/` | https://github.com/Wren6991/Hazard3 | `8af992930f71a69b0e06c38734c1094f41a05ca0` | Apache-2.0 |

Only the files needed to build each SoC/core are included: SERV's `servant`
SoC with the `hello_uart` program, DarkRISCV's `darksocv` with its default
firmware (`darkspi.v` dropped: it needs a module not in the tree), and the
Hazard3 core as listed in `hdl/hazard3.f`.

`../picosoc/picorv32.v` is PicoRV32 (ISC, https://github.com/YosysHQ/picorv32);
the rest of `../picosoc/` is a minimal SoC and firmware written for this
project.
