#!/usr/bin/env python3
"""Bitstream-level simulation check (development tool, not shipped).

Decompiles a placed-and-routed iCE40 design (.asc) with IceStorm's
icebox_vlog, then simulates it against the original RTL with Icarus Verilog
under the same random stimulus, comparing every output after every clock
edge. Requires icebox_vlog, iverilog/vvp and Yosys' iCE40 simulation models.

    tools/bitsim.py --asc out.asc --pcf pins.pcf --package cb132 \
        --top top --clock clk --cycles 20000 rtl.v [more.v ...]
"""
import argparse
import os
import random
import re
import subprocess
import sys
import tempfile

CELLS_SIM = "/usr/share/yosys/ice40/cells_sim.v"


def chip_ports(chip_v):
    m = re.search(r"module chip \((.*?)\);", chip_v, re.S)
    ports = []
    for p in m.group(1).split(","):
        p = p.strip()
        d, name = p.split(None, 1)
        if d == "inout":
            raise SystemExit("inout ports not supported by this harness")
        ports.append((d, name.strip()))
    return ports


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--asc", required=True)
    ap.add_argument("--pcf", required=True)
    ap.add_argument("--package", required=True)
    ap.add_argument("--top", required=True)
    ap.add_argument("--clock", required=True)
    ap.add_argument("--cycles", type=int, default=10000)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--hold", type=int, default=50, help="max cycles an input value is held")
    ap.add_argument("rtl", nargs="+")
    a = ap.parse_args()

    tmp = tempfile.mkdtemp(prefix="bitsim")
    chip_v = subprocess.run(["icebox_vlog", "-d", a.package, "-p", a.pcf, "-n", "chip", a.asc],
                            capture_output=True, text=True, check=True).stdout
    ports = chip_ports(chip_v)
    # icebox_vlog redeclares ANSI ports as wires, which Icarus rejects.
    for _, n in ports:
        chip_v = chip_v.replace(f"\nwire {n};\n", "\n").replace(f"\nwire {n} ;\n", "\n")
    with open(os.path.join(tmp, "chip.v"), "w") as f:
        f.write(chip_v)
    # Group scalar chip ports back into RTL vectors: "\led[3] " -> led, 3.
    def split(name):
        m = re.match(r"\\(\w+)\[(\d+)\]", name)
        return (m.group(1), int(m.group(2))) if m else (name, None)
    vectors = {}
    for d, n in ports:
        base, idx = split(n)
        vectors.setdefault(base, (d, set()))[1].add(idx)
    rnd = random.Random(a.seed)

    tb = ["`timescale 1ns/1ps", "module tb;"]
    decl = []
    for base, (d, idxs) in vectors.items():
        width = (max(i for i in idxs if i is not None) + 1) if None not in idxs else 1
        rng = f"[{width-1}:0] " if None not in idxs else ""
        if d == "input":
            tb.append(f"  reg {rng}{base} = 0;")
        else:
            tb.append(f"  wire {rng}rtl_{base};")
            tb.append(f"  wire {rng}chip_{base};")
        decl.append((base, d, idxs, width))
    conns_rtl = ", ".join(f".{b}({b if d == 'input' else 'rtl_' + b})" for b, d, _, _ in decl)
    tb.append(f"  {a.top} dut_rtl ({conns_rtl});")
    conns_chip = []
    for b, d, idxs, w in decl:
        sig = b if d == "input" else "chip_" + b
        if None in idxs:
            conns_chip.append(f".{b}({sig})")
        else:
            conns_chip += [f".\\{b}[{i}] ({sig}[{i}])" for i in sorted(idxs)]
    tb.append(f"  chip dut_chip ({', '.join(conns_chip)});")
    tb.append("  integer cyc, errors;")
    tb.append("  initial begin")
    tb.append("    errors = 0;")
    tb.append(f"    for (cyc = 0; cyc < {a.cycles}; cyc = cyc + 1) begin")
    tb.append(f"      #5 {a.clock} = 1;")
    tb.append(f"      #5 {a.clock} = 0;")
    outs = [b for b, d, _, _ in decl if d == "output"]
    for b in outs:
        tb.append(f"      if (rtl_{b} !== chip_{b}) begin errors = errors + 1; "
                  f"if (errors < 10) $display(\"cycle %0d: {b} rtl=%b chip=%b\", cyc, rtl_{b}, chip_{b}); end")
    ins = [b for b, d, _, _ in decl if d == "input" and b != a.clock]
    tb.append("      // random stimulus with random hold times")
    for b in ins:
        tb.append(f"      if (($random & 63) < {max(1, 64 // a.hold)}) {b} = $random;")
    tb.append("    end")
    tb.append("    if (errors == 0) $display(\"PASS %0d cycles\", cyc); else $display(\"FAIL %0d mismatches\", errors);")
    tb.append("    $finish;")
    tb.append("  end")
    tb.append("endmodule")
    tb_path = os.path.join(tmp, "tb.v")
    with open(tb_path, "w") as f:
        f.write("\n".join(tb) + "\n")
    out = os.path.join(tmp, "sim")
    subprocess.run(["iverilog", "-g2012", "-o", out, "-DNO_ICE40_DEFAULT_ASSIGNMENTS", tb_path,
                    os.path.join(tmp, "chip.v"), CELLS_SIM] + a.rtl, check=True)
    r = subprocess.run(["vvp", "-n", out], capture_output=True, text=True)
    print(r.stdout.strip())
    sys.exit(0 if "PASS" in r.stdout else 1)


if __name__ == "__main__":
    main()
