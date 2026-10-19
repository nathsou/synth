#!/usr/bin/env python3
"""Functional check of native Artix-7 bitstreams against their source RTL.

Project X-Ray's bitread decodes each bitstream to FASM, and f4pga fasm2bels
rebuilds the placed and routed netlist from it: cells from the site
configuration, nets from the routing pips. Icarus simulates that netlist
against the design's RTL with random stimulus, comparing every output each
half clock period. The synth executable never runs these tools. fasm2bels
has no DSP48 model, so DSP designs are skipped.

Input is the output directory of tools/ci_artix7_physical.py (.v, .xdc and
.bit per design and part). Set up the tools with tools/prepare_fasm2bels.sh.
"""
import argparse
import csv
import json
import os
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
DATA = Path(os.environ.get("YOSYS_DATDIR", "/usr/share/yosys"))
VENDOR = ["RAMB18E1", "RAM32M", "RAM64M", "SRLC32E", "MUXF7", "MUXF8"]


def run(args, log):
    with log.open("w") as f:
        p = subprocess.run([str(a) for a in args], cwd=ROOT, stdout=f, stderr=subprocess.STDOUT,
                           timeout=3600)
    if p.returncode != 0:
        raise RuntimeError(f"{args[0]} exited {p.returncode}; see {log}")


def ports(xdc, pins):
    """Port bits by name with their pad sites, and the clocks."""
    sites, clocks = {}, []
    for line in xdc.read_text().splitlines():
        if m := re.search(r"PACKAGE_PIN (\S+) .*\[get_ports \{(.+)\}\]", line):
            sites[m[2]] = pins[m[1]]
        elif m := re.search(r"create_clock .*\[get_ports \{(.+)\}\]", line):
            clocks.append(m[1])
    return sites, clocks


def invert_memory_clocks(net, fasm):
    """fasm2bels applies a slice's CLKINV to its flip-flops only. The SLICEM
    clock inverter also drives the write clock of its shift registers and
    LUT RAM, so give those cells the inversion too."""
    inverted = set()
    for line in fasm.read_text().splitlines():
        if m := re.match(r"(\S+)\.SLICEM_X0\.CLKINV$", line.strip()):
            inverted.add(m[1])
    def patch(m):
        kind, params, tile, x = m[1], m[2], m[3], int(m[4])
        if tile not in inverted or x % 2:
            return m[0]
        pin = "IS_WCLK_INVERTED" if kind.startswith("RAM") else "IS_CLK_INVERTED"
        return m[0].replace(params, params.rstrip() + f",\n    .{pin}(1'b1)\n  ", 1)
    text = re.sub(r"\b(SRLC32E|SRL16E|RAM32M|RAM64M) #\(((?:.|\n)*?)\) ((?:CLBLM_[LR]_X\d+Y\d+))_SLICE_X(\d+)Y\d+_",
                  patch, net.read_text())
    net.write_text(text)


def testbench(name, sites, clocks, directions, cycles):
    """Drives both models with the same random inputs; compares outputs."""
    bits = {}
    for port in sites:
        base, _, index = port.partition("[")
        bits.setdefault(base, {})[int(index.rstrip("]")) if index else None] = port
    ident = lambda p: re.sub(r"\W", "_", p)
    inputs = [p for p in sites if directions[p] == "input"]
    outputs = [p for p in sites if directions[p] == "output"]
    lines = ["`timescale 1ns/1ps", "module tb;"]
    lines += [f"  reg {ident(p)} = 0;" for p in inputs]
    lines += [f"  wire ref_{ident(p)}, got_{ident(p)};" for p in outputs]
    lines += [f"  reg seen_{ident(p)} = 0;" for p in outputs]
    conns = []
    for base, members in bits.items():
        prefix = "" if directions[next(iter(members.values()))] == "input" else "ref_"
        if None in members:
            conns.append(f".{base}({prefix}{ident(members[None])})")
        else:
            width = max(members) + 1
            conns.append(f".{base}({{" + ", ".join(
                f"{prefix}{ident(members[i])}" if i in members else "1'bz"
                for i in reversed(range(width))) + "})")
    lines.append(f"  top reference({', '.join(conns)});")
    pads = [f".{tile}_{site}_{'IPAD' if directions[p] == 'input' else 'OPAD'}"
            f"({'' if directions[p] == 'input' else 'got_'}{ident(p)})"
            for p, (tile, site) in sites.items()]
    lines.append(f"  bels netlist({', '.join(pads)});")
    data = [p for p in inputs if p not in clocks]
    lines += ["  integer i, seed = 1;", "  initial begin",
              # Configuration and GSR settle with the clocks stopped.
              "    #200;",
              f"    for (i = 0; i < {2 * cycles}; i = i + 1) begin"]
    lines += [f"      {ident(p)} = $random(seed);" for p in data]
    lines += ["      #2.5;"]
    lines += [f"      {ident(c)} = !{ident(c)};" for c in clocks]
    lines += ["      #2.5;"]
    # Registers without an initial value read x in the RTL until reset; the
    # bitstream starts them at 0. Compare the outputs the RTL defines.
    for p in outputs:
        lines.append(f"      if (ref_{ident(p)} !== 1'bx) seen_{ident(p)} = 1;")
        lines.append(f"      if (ref_{ident(p)} !== 1'bx && got_{ident(p)} !== ref_{ident(p)}) "
                     f"$fatal(1, \"{name}: {p} %b != %b after %0d half cycles\", "
                     f"got_{ident(p)}, ref_{ident(p)}, i);")
    lines += ["    end"]
    # Every output must have been compared at some point.
    for p in outputs:
        lines.append(f"    if (!seen_{ident(p)}) $fatal(1, \"{name}: {p} never defined in the RTL\");")
    lines += [f"    $display(\"PASS bitstream {name}\");", "    $finish;", "  end",
              "endmodule"]
    return "\n".join(lines) + "\n"


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--physical", type=Path, required=True, help="ci_artix7_physical.py --out")
    p.add_argument("--db", type=Path, required=True, help="prjxray-db/artix7")
    p.add_argument("--bitread", type=Path, required=True)
    p.add_argument("--fasm2bels", type=Path, required=True, help="Python with fasm2bels installed")
    p.add_argument("--connections", type=Path, required=True, help="connection database directory")
    p.add_argument("--unisim", type=Path, default=os.environ.get("XILINX_UNISIM"),
                   help="vendor models from tools/fetch_xc7_models.sh")
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--design", action="append")
    p.add_argument("--cycles", type=int, default=2000)
    a = p.parse_args()
    a.out.mkdir(parents=True, exist_ok=True)
    a.connections.mkdir(parents=True, exist_ok=True)
    # Yosys's cell models, with the vendor models where they exist.
    text = (DATA / "xilinx/cells_sim.v").read_text()
    for m in VENDOR:
        text = re.sub(rf"module {m}\s*\(.*?endmodule", "", text, flags=re.S)
    models = a.out / "cells.v"
    models.write_text(text + "".join((a.unisim / f"{m}.v").read_text() + "\n" for m in VENDOR))
    glbl = a.out / "glbl.v"
    glbl.write_text("`timescale 1ps/1ps\nmodule glbl; reg GSR = 0; initial begin #1 GSR=1; #100 GSR=0; end endmodule\n")
    report = {"checks": []}
    for part_dir in sorted(d for d in a.physical.iterdir() if d.is_dir()):
        part = part_dir.name
        with (a.db / part / "package_pins.csv").open() as f:
            pins = {r["pin"]: (r["tile"], r["site"]) for r in csv.DictReader(f)}
        for bit in sorted(part_dir.glob("*.bit")):
            name = bit.stem
            if "." in name or (a.design and name not in a.design):
                continue
            source = bit.with_suffix(".v")
            if re.search(r"^DSP_[LR]_", bit.with_suffix(".fasm").read_text(), re.M):
                print(f"SKIP {part} {name}: fasm2bels has no DSP48 model", flush=True)
                continue
            prefix = a.out / part / name
            prefix.parent.mkdir(exist_ok=True)
            net = prefix.with_suffix(".bels.v")
            run([a.fasm2bels, "-W", "ignore", "-m", "fasm2bels",
                 "--connection_database", a.connections / f"{part}.db", "--db_root", a.db,
                 "--part", part, "--bit_file", bit, "--bitread", a.bitread,
                 "--fasm_file", prefix.with_suffix(".decoded.fasm"), "--verilog_file", net,
                 "--xdc_file", prefix.with_suffix(".bels.xdc"), "--top", "bels",
                 "--iostandard", "LVCMOS33", "--drive", "12"], prefix.with_suffix(".fasm2bels.log"))
            invert_memory_clocks(net, prefix.with_suffix(".decoded.fasm"))
            sites, clocks = ports(bit.with_suffix(".xdc"), pins)
            header = net.read_text().split(");", 1)[0]
            directions = {}
            for port, (tile, site) in sites.items():
                m = re.search(rf"(input|output) {tile}_{site}_[IO]PAD\b", header)
                if not m:
                    raise RuntimeError(f"{name}: no pad for {port} at {site}")
                directions[port] = m[1]
            tb = prefix.with_suffix(".tb.v")
            tb.write_text(testbench(name, sites, clocks, directions, a.cycles))
            vvp = prefix.with_suffix(".vvp")
            run(["iverilog", "-g2012", "-s", "tb", "-s", "glbl", "-o", vvp, tb, source, net, models,
                 glbl], prefix.with_suffix(".compile.log"))
            log = prefix.with_suffix(".sim.log")
            run(["vvp", vvp], log)
            if f"PASS bitstream {name}" not in log.read_text():
                raise RuntimeError(f"{name}: simulation failed; see {log}")
            report["checks"].append(dict(part=part, design=name, cycles=a.cycles))
            print(f"PASS {part} {name}: {a.cycles} cycles", flush=True)
    (a.out / "results.json").write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
