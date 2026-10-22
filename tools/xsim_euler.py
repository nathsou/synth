#!/usr/bin/env python3
"""Simulates synth's Artix-7 netlists of the Euler solvers in Vivado's xsim.

Each design is synthesized with `synth synth --family artix7 --verilog` (or
read from --netlist-dir as eulerNN.v), simulated with Xilinx's UNISIM
models in tests/designs/euler/tb.v, and must give the expected answer after
exactly the expected number of cycles. With --bitstream-dir (the output of
tools/vivado_artix7.py), the netlist is instead rebuilt from synth's routed
bitstream by f4pga fasm2bels (set up with tools/prepare_fasm2bels.sh), which
has no DSP48 model: DSP designs are skipped. A development check, not shipped.
"""
import argparse
import csv
import importlib.util
import os
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
SYNTH = os.environ.get("SYNTH", str(ROOT / "target/release/synth"))
BIN = Path(os.environ.get("VIVADO", "/opt/Xilinx/2025.2/Vivado/bin/vivado")).parent
spec = importlib.util.spec_from_file_location("bitsim", ROOT / "tools/ci_artix7_bitsim.py")
bitsim = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bitsim)
# Global set/reset only at time zero: the testbench counts cycles from the
# first clock edge, which Vivado's 100 ns glbl pulse would overlap.
GLBL = """`timescale 1ps/1ps
module glbl;
  reg GSR = 1; wire GTS = 0; wire PRLD = 0; wire GRESTORE = 0;
  initial #1 GSR = 0;
endmodule
"""


def from_bitstream(a, src, d, top, net):
    """The solver module rebuilt from its bitstream, wrapped in its ports."""
    if re.search(r"^DSP_[LR]_", (src / "synth.fasm").read_text(), re.M):
        return False
    a.connections.mkdir(parents=True, exist_ok=True)
    bels = d / "bels.v"
    bitsim.run([a.fasm2bels, "-W", "ignore", "-m", "fasm2bels",
                "--connection_database", a.connections / f"{a.part}.db", "--db_root", a.db,
                "--part", a.part, "--bit_file", src / "synth.bit", "--bitread", a.bitread,
                "--fasm_file", d / "decoded.fasm", "--verilog_file", bels,
                "--xdc_file", d / "bels.xdc", "--top", "bels",
                "--iostandard", "LVCMOS33", "--drive", "12"], d / "fasm2bels.log")
    bitsim.invert_memory_clocks(bels, d / "decoded.fasm")
    with (a.db / a.part / "package_pins.csv").open() as f:
        pins = {r["pin"]: (r["tile"], r["site"]) for r in csv.DictReader(f)}
    sites, _ = bitsim.ports(src / "pins.xdc", pins)
    header = bels.read_text().split(");", 1)[0]
    pads, widths = [], {}
    for port, (tile, site) in sites.items():
        m = re.search(rf"(input|output) ({tile}_{site}_[IO]PAD)\b", header)
        if not m:
            raise RuntimeError(f"{top}: no pad for {port} at {site}")
        pads.append(f".{m[2]}({port})")
        base, _, index = port.partition("[")
        widths[base] = max(widths.get(base, 0), int(index.rstrip("]")) + 1 if index else 0)
    decls = [f"{'output' if b in ('done', 'answer') else 'input'} "
             f"{f'[{w - 1}:0] ' if w else ''}{b}" for b, w in widths.items()]
    net.write_text(bels.read_text() + f"\nmodule {top}({', '.join(decls)});\n"
                   f"  bels netlist({', '.join(pads)});\nendmodule\n")
    return True


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--design", action="append", help="Euler number; repeat to select a subset")
    p.add_argument("--netlist-dir", type=Path, help="use eulerNN.v netlists from here")
    p.add_argument("--bitstream-dir", type=Path,
                   help="tools/vivado_artix7.py --out: rebuild eulerNN/synth.bit with fasm2bels")
    p.add_argument("--db", type=Path, help="prjxray-db/artix7 (with --bitstream-dir)")
    p.add_argument("--part", default="xc7a100tcsg324-1")
    p.add_argument("--bitread", type=Path, help="Project X-Ray bitread (with --bitstream-dir)")
    p.add_argument("--fasm2bels", type=Path, help="Python with fasm2bels installed")
    p.add_argument("--connections", type=Path, help="fasm2bels connection database directory")
    a = p.parse_args()
    out = a.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    (out / "glbl.v").write_text(GLBL)
    euler = ROOT / "tests/designs/euler"
    failed = []
    for line in (euler / "designs.txt").read_text().splitlines():
        if not line or line.startswith("#"):
            continue
        n, width, answer, cycles, *extra = line.split()
        if a.design and n not in a.design:
            continue
        top = f"euler{n}"
        d = out / top
        d.mkdir(exist_ok=True)
        net = (a.netlist_dir / f"{top}.v") if a.netlist_dir else d / f"{top}.v"
        if a.bitstream_dir:
            net = d / f"{top}.v"
            if not from_bitstream(a, a.bitstream_dir / top, d, top, net):
                print(f"SKIP {top}: fasm2bels has no DSP48 model", flush=True)
                continue
        elif not a.netlist_dir:
            files = [euler / f"p0{n}.v", *[euler / f for f in extra]]
            subprocess.run([SYNTH, "synth", *files, "--top", top, "--family", "artix7",
                            "--verilog", net], check=True, capture_output=True)
        defines = [f"SOLVER={top}", f"ANSWER_WIDTH={width}", f"EXPECT=64'd{answer}",
                   f"MAX_CYCLES={int(cycles) + 10}"]
        log = d / "xsim.log"
        with log.open("w") as f:
            for cmd in [[BIN / "xvlog", *sum((["-d", x] for x in defines), []), net,
                         euler / "tb.v", out / "glbl.v"],
                        [BIN / "xelab", "-L", "unisims_ver", "tb", "glbl", "-s", top, "-timescale", "1ns/1ps"],
                        [BIN / "xsim", top, "-R"]]:
                if subprocess.run(list(map(str, cmd)), cwd=d, stdout=f, stderr=subprocess.STDOUT).returncode:
                    break
        result = re.search(r"(PASS|FAIL) answer .*", log.read_text())
        ok = result and result[1] == "PASS" and re.search(rf"after {cycles} cycles", result[0])
        print(f"{'PASS' if ok else 'FAIL'} {top}: {result[0] if result else 'no result, see ' + str(log)}",
              flush=True)
        if not ok:
            failed.append(top)
    if failed:
        raise SystemExit(f"failed: {' '.join(failed)}")


if __name__ == "__main__":
    main()
