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


def named_wires(netlist, module):
    """User-named internal wires of the netlist module as (name, width)."""
    m = re.search(r"module %s\((.*?)\);(.*?)endmodule" % re.escape(module), netlist, re.S)
    out = []
    for rng, name in re.findall(r"^\s*wire (\[\d+:0\] )?(\\\S+|[A-Za-z_]\w*)\s*;", m.group(2), re.M):
        bare = name[1:] if name.startswith("\\") else name
        if bare.startswith("$") or not re.fullmatch(r"[A-Za-z_][\w$]*(\.[A-Za-z_][\w$]*)*", bare):
            continue
        width = int(re.match(r"\[(\d+)", rng).group(1)) + 1 if rng else 1
        out.append((name, width))
    return out


def probe_decl(probes):
    return "\n".join(f"  reg p{i}_bad = 0; reg [{w-1}:0] p{i}_r, p{i}_n; /*p{i}*/" for i, (_, w) in enumerate(probes))


def probe_check(probes):
    # Report each probe's first divergence (reference known, netlist different).
    lines = []
    for i, (name, w) in enumerate(probes):
        bare = name[1:] if name.startswith("\\") else name
        ref = "u_ref." + bare
        netp = "u_net." + name + (" " if name.startswith("\\") else "")
        # Copy into zero-based registers: the reference may declare [msb:lsb].
        lines.append(
            f"      p{i}_r = {ref}; p{i}_n = {netp}; /*p{i}*/\n"
            # (An all-z netlist value is a wire whose logic was removed as dead.)
            f"      if (p{i}_n !== {w}'bz) for (k = 0; k < {w}; k = k + 1) if (!p{i}_bad && p{i}_r[k] !== 1'bx && p{i}_r[k] !== p{i}_n[k]) begin"
            f" p{i}_bad = 1; $display(\"probe cycle %0d: {bare} ref=%b net=%b\", cyc, p{i}_r, p{i}_n); end")
    return "\n".join(lines)


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
    ap.add_argument("-P", dest="params", action="append", default=[],
                    help="NAME=VALUE parameter override for the reference top")
    ap.add_argument("--lib", action="append", default=[],
                    help="extra Verilog for the netlist side (primitive models)")
    ap.add_argument("--probe", action="store_true",
                    help="also compare every named internal wire and report the first divergence")
    ap.add_argument("--watch", default="",
                    help="comma-separated hierarchical names to print (reference / netlist) each cycle")
    ap.add_argument("--window", default="0:20", help="FIRST:LAST cycles for --watch")
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
    for p in a.params:
        k, v = p.split("=", 1)
        tb.append(f"  defparam u_ref.{k} = {v};")
    tb.append(f"  {nmod} u_net ({conn('net_')});")
    tb.append("  integer cyc, errors, k, perr;")
    probes = named_wires(net, nmod) if a.probe else []
    tb.append("PROBES_DECL")
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
    tb.append("PROBES_CHECK")
    if a.watch:
        lo, hi = a.window.split(":")
        names = [n for n in a.watch.split(",") if n]
        fmt = " ".join(f"{n}=%b/%b" for n in names)
        args = ", ".join(f"u_ref.{n}, u_net.\\{n} " for n in names)
        tb.append(f"      if (cyc >= {lo} && cyc <= {hi}) $display(\"watch %0d: {fmt}\", cyc, {args});")
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
    # The design may stop the simulation itself ($finish in simulation-only code).
    tb.append(f"  final if (cyc < {a.cycles}) begin")
    tb.append("    if (errors == 0) $display(\"PASS %0d cycles (design called $finish)\", cyc);")
    tb.append("    else $display(\"FAIL %0d mismatches\", errors);")
    tb.append("  end")
    tb.append("endmodule")
    tmp = tempfile.mkdtemp(prefix="rtlsim")
    tbp = os.path.join(tmp, "tb.v")
    out = os.path.join(tmp, "sim")
    cmd = ["iverilog", "-g2012", "-o", out, tbp, a.netlist] + a.lib + [f"-I{i}" for i in a.incdirs] + [f"-D{d}" for d in a.defines] + a.rtl
    # Probes whose path does not exist in the reference are dropped and the
    # testbench is recompiled.
    while True:
        open(tbp, "w").write("\n".join(tb).replace("PROBES_DECL", probe_decl(probes))
                             .replace("PROBES_CHECK", probe_check(probes)) + "\n")
        r = subprocess.run(cmd, capture_output=True, text=True)
        if r.returncode == 0 or not probes:
            break
        bad = {int(m) for m in re.findall(r"tb\.v:(\d+): error", r.stderr)}
        lines = open(tbp).read().split("\n")
        drop = {i for i, (name, _) in enumerate(probes) if any(f"/*p{i}*/" in lines[l - 1] for l in bad)}
        if not drop:
            break
        probes = [p for i, p in enumerate(probes) if i not in drop]
    if r.returncode != 0:
        sys.stderr.write(r.stderr)
        sys.exit(2)
    r = subprocess.run(["vvp", "-n", out], capture_output=True, text=True)
    print(r.stdout.strip() if a.probe or a.watch else r.stdout.strip()[-3000:])
    sys.exit(0 if "PASS" in r.stdout else 1)


if __name__ == "__main__":
    main()
