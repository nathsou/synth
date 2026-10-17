#!/usr/bin/env python3
"""Differential RTL simulation (development tool, not shipped).

Simulates the original Verilog of a design against the netlist that
`synth elab --verilog <out.v> --rename <top>_netlist` wrote for it, under the
same random stimulus, and compares every output after every clock edge
(x bits in the reference are don't-cares).

    tools/rtlsim.py --top top --netlist top_netlist.v --clock clk \
        --cycles 10000 rtl.v [more.v ...]
"""
import argparse
import os
import re
import subprocess
import sys
import tempfile


def ports(netlist, module):
    m = re.search(r"module %s\((.*?)\);(.*?)endmodule" % re.escape(module), netlist, re.S)
    names = [p.strip() for p in m.group(1).split(",") if p.strip()]
    decls = {}
    for d, rng, name in re.findall(r"^\s*(input|output|inout) wire (\[\d+:0\] )?(\S+)\s*;", m.group(2), re.M):
        width = int(re.match(r"\[(\d+)", rng).group(1)) + 1 if rng else 1
        decls[name] = (d, width)
    return [(n, decls[n][0], decls[n][1]) for n in names]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--top", required=True)
    ap.add_argument("--netlist", required=True)
    ap.add_argument("--clock", action="append", default=[])
    ap.add_argument("--reset", help="input held active (1) for the first cycles")
    ap.add_argument("--reset-low", help="input held active (0) for the first cycles")
    ap.add_argument("--cycles", type=int, default=10000)
    ap.add_argument("--hold", type=int, default=20)
    ap.add_argument("-I", dest="incdirs", action="append", default=[])
    ap.add_argument("-D", dest="defines", action="append", default=[])
    ap.add_argument("rtl", nargs="+")
    a = ap.parse_args()
    net = open(a.netlist).read()
    nmod = a.top + "_netlist"
    ps = ports(net, nmod)
    tb = ["`timescale 1ns/1ps", "module tb;"]
    for n, d, w in ps:
        if d == "input":
            tb.append(f"  reg [{w-1}:0] {n} = 0;")
        else:
            tb.append(f"  wire [{w-1}:0] ref_{n}, net_{n};")
    conn = lambda pre: ", ".join(f".{n}({n if d == 'input' else pre + n})" for n, d, _ in ps)
    tb.append(f"  {a.top} u_ref ({conn('ref_')});")
    tb.append(f"  {nmod} u_net ({conn('net_')});")
    tb.append("  integer cyc, errors, k;")
    tb.append("  initial begin")
    tb.append("    errors = 0;")
    if a.reset:
        tb.append(f"    {a.reset} = 1;")
    if a.reset_low:
        tb.append(f"    {a.reset_low} = 0;")
    tb.append(f"    for (cyc = 0; cyc < {a.cycles}; cyc = cyc + 1) begin")
    for c in a.clock:
        tb.append(f"      #5 {c} = 1;")
    tb.append("      #1;")
    for n, d, w in ps:
        if d != "input":
            tb.append(f"      for (k = 0; k < {w}; k = k + 1) if (ref_{n}[k] !== 1'bx && ref_{n}[k] !== net_{n}[k]) begin")
            tb.append(f"        errors = errors + 1; if (errors < 10) $display(\"cycle %0d: {n} ref=%b net=%b\", cyc, ref_{n}, net_{n}); end")
    for c in a.clock:
        tb.append(f"      #4 {c} = 0;")
    for n, d, w in ps:
        if d == "input" and n not in a.clock and n not in (a.reset, a.reset_low):
            tb.append(f"      if (($random & 1023) < {max(1, 1024 // a.hold)}) {n} = $random;")
    if a.reset:
        tb.append(f"      if (cyc == 4) {a.reset} = 0;")
    if a.reset_low:
        tb.append(f"      if (cyc == 4) {a.reset_low} = 1;")
    tb.append("    end")
    tb.append("    if (errors == 0) $display(\"PASS %0d cycles\", cyc); else $display(\"FAIL %0d mismatches\", errors);")
    tb.append("    $finish;")
    tb.append("  end")
    tb.append("endmodule")
    tmp = tempfile.mkdtemp(prefix="rtlsim")
    tbp = os.path.join(tmp, "tb.v")
    open(tbp, "w").write("\n".join(tb) + "\n")
    out = os.path.join(tmp, "sim")
    cmd = ["iverilog", "-g2012", "-o", out, tbp, a.netlist] + [f"-I{i}" for i in a.incdirs] + [f"-D{d}" for d in a.defines] + a.rtl
    subprocess.run(cmd, check=True)
    r = subprocess.run(["vvp", "-n", out], capture_output=True, text=True)
    print(r.stdout.strip()[-3000:])
    sys.exit(0 if "PASS" in r.stdout else 1)


if __name__ == "__main__":
    main()
