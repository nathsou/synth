#!/usr/bin/env python3
"""Artix-7 QoR against Vivado, with Vivado's timer as the referee.

Each Euler design is implemented three ways on one part with the same pin
constraints and clock:

- vivado: Vivado from RTL with its default synthesis and implementation
  steps (synth_design, opt_design, place_design, phys_opt_design,
  route_design).
- netlist: synth's mapped netlist through the same Vivado steps (Vivado
  keeps instantiated primitives and adds the I/O and clock buffers), so
  the two compare synthesis alone, placed, routed and timed by Vivado.
- synth: synth's native flow from source to bitstream, counted from its
  FASM. With --fasm2bels, f4pga fasm2bels rebuilds the placed and routed
  netlist from the bitstream with the placement and routing as
  constraints, and Vivado times it: every net keeps synth's route except
  clock and constant nets, which Vivado routes (its clock tree ties unused
  RAM register clocks to VCC; constants carry no timing). fasm2bels has no
  DSP48 model, so DSP designs keep only synth's own estimate.

Fmax is from Vivado's post-route setup analysis: the smallest period at
which every reported path meets its requirement (a half-cycle path counts
twice). Slices and LUT sites are counted from cell placements in Vivado
and from FASM for synth, alike. Reports are kept in --out.
"""
import argparse
import csv
import importlib.util
import json
import os
from collections import Counter
from pathlib import Path
import re
import statistics
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
SYNTH = os.environ.get("SYNTH", str(ROOT / "target/release/synth"))
VIVADO = os.environ.get("VIVADO", "/opt/Xilinx/2025.2/Vivado/bin/vivado")
spec = importlib.util.spec_from_file_location("physical", ROOT / "tools/ci_artix7_physical.py")
physical = importlib.util.module_from_spec(spec)
spec.loader.exec_module(physical)
spec = importlib.util.spec_from_file_location("bitsim", ROOT / "tools/ci_artix7_bitsim.py")
bitsim = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bitsim)
spec = importlib.util.spec_from_file_location("pnr", ROOT / "tools/pnr_artix7.py")
pnr = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pnr)

# Placed cells, for counting alike with synth's FASM; then setup paths.
REPORT = r"""
set t [clock milliseconds]
opt_design
set t_opt [expr {[clock milliseconds] - $t}]
place_design
set t_place [expr {[clock milliseconds] - $t - $t_opt}]
phys_opt_design
route_design
puts "QOR seconds_impl [expr {([clock milliseconds] - $t) / 1000.0}]"
report_utilization -file {prefix}.util.rpt
report_timing_summary -max_paths 10 -file {prefix}.timing.rpt
report_route_status -file {prefix}.route.rpt
set f [open {prefix}.cells.txt w]
foreach c [get_cells -hier -filter {PRIMITIVE_LEVEL==LEAF || PRIMITIVE_LEVEL==INTERNAL}] {
  puts $f "[get_property REF_NAME $c] [get_property LOC $c] [get_property BEL $c]"
}
close $f
set f [open {prefix}.paths.txt w]
foreach p [get_timing_paths -setup -max_paths 200 -nworst 1 -unique_pins] {
  puts $f "[get_property SLACK $p] [get_property REQUIREMENT $p] [get_property STARTPOINT_CLOCK $p] [get_property ENDPOINT_CLOCK $p]"
}
close $f
puts "QOR unrouted [llength [get_nets -hier -filter {ROUTE_STATUS==UNROUTED || ROUTE_STATUS==CONFLICTS}]]"
"""


# Each routed connection's slow and fast corner delays (ps) by sink site pin.
DELAYS = r"""
set f [open {out} w]
foreach n [get_nets -hier -filter {TYPE==SIGNAL && ROUTE_STATUS==ROUTED}] {
  foreach d [get_net_delays -of_objects $n] {
    set sp [get_site_pins -quiet -of [get_pins [get_property TO_PIN $d]]]
    if {$sp eq ""} continue
    puts $f "$sp [get_property SLOW_MAX $d] [get_property FAST_MAX $d]"
  }
}
close $f
"""


def vivado(tcl, log, cwd):
    start = time.monotonic()
    with log.open("w") as f:
        p = subprocess.run([VIVADO, "-mode", "batch", "-nojournal", "-nolog", "-source", tcl],
                           cwd=cwd, stdout=f, stderr=subprocess.STDOUT, timeout=7200)
    seconds = round(time.monotonic() - start, 3)
    text = log.read_text()
    if p.returncode != 0 or "QOR unrouted" not in text:
        errors = [l for l in text.splitlines() if l.startswith("ERROR")]
        return seconds, (errors[0] if errors else f"exit {p.returncode}")
    return seconds, None


def placed(prefix):
    """Cells as Vivado placed them, counted as pnr_artix7.resources counts FASM."""
    refs, slices, luts, ffs, brams, dsps = Counter(), set(), set(), set(), set(), set()
    for line in Path(f"{prefix}.cells.txt").read_text().splitlines():
        ref, loc, bel = (line.split() + ["", ""])[:3]
        bel = bel.split(".")[-1]
        refs[ref] += 1
        if loc.startswith("SLICE_"):
            slices.add(loc)
            if m := re.match(r"([A-D])[56]LUT$", bel):
                luts.add((loc, m[1]))
            if re.match(r"[A-D]5?FF$", bel):
                ffs.add((loc, bel))
        elif loc.startswith("RAMB18_"):
            brams.add(loc)
        elif loc.startswith("RAMB36_"):
            brams.update({loc + "a", loc + "b"})
        elif loc.startswith("DSP48_"):
            dsps.add(loc)
    lut = sum(refs[f"LUT{i}"] for i in range(1, 7))
    return {"slices": len(slices), "lut_sites": len(luts), "ffs": len(ffs),
            "bram18": len(brams), "dsp48": len(dsps), "lut": lut,
            "carry4": refs["CARRY4"], "muxf7": refs["MUXF7"], "muxf8": refs["MUXF8"],
            "lutram": sum(n for r, n in refs.items() if r.startswith("RAMD") or r.startswith("RAMS")),
            "srl": sum(n for r, n in refs.items() if r.startswith("SRL"))}


def fmax(prefix):
    """Smallest period at which each path meets its scaled requirement."""
    period = None
    for line in Path(f"{prefix}.paths.txt").read_text().splitlines():
        slack, requirement, start, end = (line.split() + ["", "", ""])[:4]
        if slack in ("", "inf") or not requirement or start != end:
            continue
        slack, requirement = float(slack), float(requirement)
        if requirement <= 0:
            continue
        # The path needs requirement - slack of its requirement's share
        # of the period.
        needed = (requirement - slack) * PERIOD / requirement
        period = max(period or 0.0, needed)
    return round(1000.0 / period, 1) if period else None


def implement(top, prefix, sources, xdc, part, threads):
    report = REPORT.replace("{prefix}", str(prefix))
    # VIVADO_SKIP=opt_design,phys_opt_design leaves those steps out, to
    # measure what they gain.
    for step in filter(None, os.environ.get("VIVADO_SKIP", "").split(",")):
        report = report.replace(f"\n{step}\n", "\n")
    reads = "".join(f"read_verilog {{{s}}}\n" for s in sources)
    tcl = (f"set_param general.maxThreads {threads}\n{reads}read_xdc {{{xdc}}}\n"
           f"set t [clock milliseconds]\nsynth_design -top {top} -part {part}\n"
           f"puts \"QOR seconds_synth [expr {{([clock milliseconds] - $t) / 1000.0}}]\"\n"
           f"report_utilization -file {{{prefix}.synth_util.rpt}}\n" + report)
    path = Path(f"{prefix}.tcl")
    path.write_text(tcl)
    seconds, error = vivado(path, Path(f"{prefix}.log"), prefix.parent)
    result = {"seconds": seconds}
    if error:
        result["error"] = error
        return result
    log = Path(f"{prefix}.log").read_text()
    for key in ["seconds_synth", "seconds_impl", "unrouted"]:
        result[key] = float(re.findall(rf"^QOR {key} (\S+)", log, re.M)[-1])
    result.update(placed(prefix))
    result["fmax_mhz"] = fmax(prefix)
    return result


def timed(a, prefix, xdc):
    """Vivado's timing of the native bitstream, rebuilt by fasm2bels."""
    if re.search(r"^DSP_[LR]_", Path(f"{prefix}.fasm").read_text(), re.M):
        return {"vivado_error": "fasm2bels has no DSP48 model"}
    a.connections.mkdir(parents=True, exist_ok=True)
    bels = Path(f"{prefix}.bels.v")
    try:
        bitsim.run([a.fasm2bels, "-W", "ignore", "-m", "fasm2bels",
                    "--connection_database", a.connections / f"{a.part}.db", "--db_root", a.db,
                    "--part", a.part, "--bit_file", f"{prefix}.bit", "--bitread", a.bitread,
                    "--fasm_file", f"{prefix}.decoded.fasm", "--verilog_file", bels,
                    "--xdc_file", f"{prefix}.bels.xdc", "--top", "bels",
                    "--iostandard", "LVCMOS33", "--drive", "12"], Path(f"{prefix}.fasm2bels.log"))
    except RuntimeError as e:
        return {"vivado_error": str(e)}
    skip = set()
    for attempt in range(4):
        result = timed_attempt(a, prefix, xdc, skip)
        log = Path(f"{prefix}.timing.log").read_text()
        # Vivado ties some SLICEM shift register pins to VCC where the
        # rebuilt netlist routes them: route those nets freely and retry.
        conflicts = set(re.findall(r"Fixed routing constraint on net (\S+) conflicts", log))
        conflicts |= set(re.findall(r"Net, (\S+), is internally routed", log))
        if "vivado_error" not in result or not conflicts or attempt == 3:
            return result
        skip |= conflicts
    return result


# Vivado refuses a directed route on a net that is also routed inside a
# site (a LUT feeding its lane's flip-flop and the interconnect).
INTERNAL = r"""
proc routed_internally {net} {
  set dp [get_pins -quiet -leaf -of $net -filter {DIRECTION==OUT}]
  if {[llength $dp] != 1} { return 0 }
  set drv [get_cells -of $dp]
  set ds [get_sites -quiet -of $drv]
  set dbel [lindex [split [get_property BEL $drv] .] end]
  set lane ""
  if {[string match "?6LUT" $dbel] || [string match "?5LUT" $dbel]} { set lane [string index $dbel 0] }
  if {$dbel eq "CARRY4" && [regexp {\[(\d)\]} [get_property REF_PIN_NAME $dp] -> k]} { set lane [string index "ABCD" $k] }
  if {$lane eq ""} { return 0 }
  foreach p [get_pins -quiet -leaf -of $net -filter {DIRECTION==IN}] {
    set c [get_cells -of $p]
    if {[get_sites -quiet -of $c] ne $ds} { continue }
    set bel [lindex [split [get_property BEL $c] .] end]
    if {[string match "*FF" $bel] && [string index $bel 0] eq $lane} { return 1 }
  }
  return 0
}
"""


def timed_attempt(a, prefix, xdc, skip):
    bels = Path(f"{prefix}.bels.v")
    guard = 'if {[llength $net] && [get_property TYPE $net] ne "GLOBAL_CLOCK" && ![routed_internally $net]'
    # Vivado names such nets after their driver pin, as the constraints
    # select them.
    guard += "".join(f' && [get_property NAME $net] ne "{n}" && ![string match "*{n}" $pin]'
                     for n in sorted(skip))
    constraints = re.sub(r"^set_property FIXED_ROUTE \$route \$net$",
                         guard + '} { set_property FIXED_ROUTE $route $net }',
                         Path(f"{prefix}.bels.xdc").read_text(), flags=re.M)
    Path(f"{prefix}.routes.xdc").write_text(INTERNAL + constraints)
    with (a.db / a.part / "package_pins.csv").open() as f:
        pins = {r["pin"]: (r["tile"], r["site"]) for r in csv.DictReader(f)}
    sites, clocks = bitsim.ports(xdc, pins)
    clock = "_".join(sites[clocks[0]]) + "_IPAD"
    tcl = Path(f"{prefix}.timing.tcl")
    tcl.write_text(f"read_verilog {{{bels}}}\nlink_design -top bels -part {a.part}\nsource {{{prefix}.routes.xdc}}\n"
                   f"create_clock -period {PERIOD} [get_ports {clock}]\n"
                   "place_design\nroute_design\n"
                   f"report_timing_summary -max_paths 10 -file {{{prefix}.timing.rpt}}\n"
                   f"report_route_status -file {{{prefix}.route.rpt}}\n"
                   f"set f [open {{{prefix}.paths.txt}} w]\n"
                   "foreach p [get_timing_paths -setup -max_paths 200 -nworst 1 -unique_pins] {\n"
                   "  puts $f \"[get_property SLACK $p] [get_property REQUIREMENT $p] "
                   "[get_property STARTPOINT_CLOCK $p] [get_property ENDPOINT_CLOCK $p]\"\n}\nclose $f\n"
                   "puts \"QOR unrouted [llength [get_nets -hier -filter {ROUTE_STATUS==UNROUTED || "
                   "ROUTE_STATUS==CONFLICTS}]]\"\n"
                   "puts \"QOR unfixed [llength [get_nets -hier -filter {ROUTE_STATUS==ROUTED && "
                   "!IS_ROUTE_FIXED && TYPE==SIGNAL}]]\"\n" + (DELAYS.replace("{out}", f"{prefix}.vivado_delays.txt")
                                                             if a.delays else ""))
    seconds, error = vivado(tcl, Path(f"{prefix}.timing.log"), prefix.parent)
    if error:
        return {"vivado_error": error}
    log = Path(f"{prefix}.timing.log").read_text()
    hold = re.search(r"^Hold\s*:\s*(\d+)\s+Failing Endpoints?\s*,\s*Worst Slack\s+(-?[0-9.]+)ns",
                     Path(f"{prefix}.timing.rpt").read_text(), re.M)
    return {"vivado_mhz": fmax(prefix),
            "vivado_unrouted": int(re.findall(r"^QOR unrouted (\d+)", log, re.M)[-1]),
            "vivado_unfixed": int(re.findall(r"^QOR unfixed (\d+)", log, re.M)[-1]),
            "vivado_hold_failing": int(hold[1]) if hold else None,
            "vivado_hold_wns": float(hold[2]) if hold else None}


def native(files, top, prefix, xdc, a):
    """synth from source to bitstream, the run with the median estimate over seeds."""
    runs = []
    for seed in range(a.first_seed, a.first_seed + a.seeds):
        log = Path(f"{prefix}.seed{seed}.log")
        env = {"SYNTH_XC7_DELAYS": f"{prefix}.delays.txt"} if a.delays else {}
        os.environ.update(env)
        seconds, error = pnr.run([SYNTH, "build", *files, "--top", top, "--family", "artix7",
                                  "--db", a.db, "--device", a.part, "--xdc", xdc,
                                  "--fasm", f"{prefix}.fasm", "-o", f"{prefix}.bit",
                                  "--timing-allow-fail", "--seed", seed], log)
        if error:
            return {"seconds": seconds, "error": error}
        r = {"seed": seed, "seconds": seconds, **pnr.resources(Path(f"{prefix}.fasm"))}
        mhz = re.findall(r"timing estimate: clock .*?\(([\d.]+) MHz\)", log.read_text())
        r["estimate_mhz"] = float(mhz[-1]) if mhz else None
        if a.fasm2bels:
            r.update(timed(a, prefix, xdc))
        runs.append(r)
    # Vivado's timing when there is one, else synth's estimate.
    ranked = sorted(runs, key=lambda r: (r.get("vivado_mhz") or r["estimate_mhz"] or 0.0, r["seed"]))
    return {**ranked[(len(ranked) - 1) // 2], "runs": runs}


def main():
    global PERIOD
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--db", type=Path, required=True, help="prjxray-db/artix7")
    p.add_argument("--part", default="xc7a100tcsg324-1")
    p.add_argument("--period", type=float, default=10.0, help="clock period in ns")
    p.add_argument("--targets", type=Path,
                   help="results.json of a previous run: constrain each design 20%% beyond the "
                        "best Vivado-timed Fmax there, so every flow works for timing")
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--design", action="append", help="Euler number; repeat to select a subset")
    p.add_argument("--flow", action="append", choices=["vivado", "netlist", "synth"],
                   help="run only these flows; repeat to select several")
    p.add_argument("--seeds", type=int, default=1, help="native runs with N seeds; the median is reported")
    p.add_argument("--first-seed", type=int, default=1, help="the first native seed")
    p.add_argument("--threads", type=int, default=8, help="Vivado general.maxThreads")
    p.add_argument("--fasm2bels", type=Path, help="Python with fasm2bels: time native results in Vivado")
    p.add_argument("--bitread", type=Path, help="Project X-Ray bitread (with --fasm2bels)")
    p.add_argument("--connections", type=Path, help="fasm2bels connection database directory")
    p.add_argument("--delays", action="store_true",
                   help="write synth's and Vivado's delay of each routed connection (with --fasm2bels)")
    a = p.parse_args()
    PERIOD = a.period
    flows = a.flow or ["vivado", "netlist", "synth"]
    out = a.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    version = subprocess.run([VIVADO, "-version"], capture_output=True, text=True).stdout.splitlines()[0]
    versions = {"vivado": version,
                "synth": subprocess.check_output([SYNTH, "version"], text=True).strip(),
                "revision": subprocess.check_output(["git", "describe", "--always", "--dirty"],
                                                    cwd=ROOT, text=True).strip()}
    report = {"part": a.part, "period_ns": "targets" if a.targets else a.period, "versions": versions,
              "designs": []}
    previous = out / "results.json"
    if previous.exists():
        old = json.loads(previous.read_text())
        if (old["part"], old["period_ns"]) == (a.part, report["period_ns"]):
            report["designs"] = old["designs"]
    print(json.dumps(versions), flush=True)
    print("design | slices vivado/netlist/synth | LUTs | FFs | CARRY4 | Vivado MHz vivado/netlist/synth | "
          "synth estimate MHz",
          flush=True)
    euler = ROOT / "tests/designs/euler"
    for line in (euler / "designs.txt").read_text().splitlines():
        if not line or line.startswith("#"):
            continue
        n, width, _, _, *extra = line.split()
        if a.design and n not in a.design:
            continue
        top = f"euler{n}"
        files = [euler / f"p0{n}.v", *[euler / f for f in extra]]
        d = out / top
        d.mkdir(exist_ok=True)
        ports = ["clk", "rst", "start", "done"] + [f"answer[{i}]" for i in range(int(width))]
        xdc = d / "pins.xdc"
        text = physical.constraints(a, a.part, ports, ["clk"])
        PERIOD = a.period
        if a.targets:
            prior = next(r for r in json.loads(a.targets.read_text())["designs"] if r["design"] == top)
            best = max(r.get(k) or 0.0 for r in (prior.get(f, {}) for f in ("vivado", "netlist", "synth"))
                       for k in ("fmax_mhz", "vivado_mhz"))
            PERIOD = round(1000.0 / (1.2 * best), 2)
        xdc.write_text(text.replace("create_clock -period 10 ", f"create_clock -period {PERIOD} "))
        row = next((r for r in report["designs"] if r["design"] == top), None)
        if row is None:
            row = {"design": top}
            report["designs"].append(row)
        row["period_ns"] = PERIOD
        if "vivado" in flows:
            row["vivado"] = implement(top, d / "vivado", files, xdc, a.part, a.threads)
        if "netlist" in flows:
            net = d / "netlist.v"
            seconds, error = pnr.run([SYNTH, "synth", *files, "--top", top, "--family", "artix7",
                                      "--verilog", net], d / "netlist.synth.log")
            if error:
                row["netlist"] = {"error": error}
            else:
                row["netlist"] = implement(top, d / "netlist", [net], xdc, a.part, a.threads)
                # Vivado only reads the netlist in its synth_design step.
                row["netlist"]["seconds_import"] = row["netlist"]["seconds_synth"]
                row["netlist"]["seconds_synth"] = seconds
        if "synth" in flows:
            row["synth"] = native(files, top, d / "synth", xdc, a)
        report["designs"].sort(key=lambda r: int(r["design"][5:]))
        previous.write_text(json.dumps(report, indent=2) + "\n")
        cell = lambda key, fs=("vivado", "netlist", "synth"): "/".join(
            str(row.get(f, {}).get(key, "-")) for f in fs)
        print(f'{top} | {cell("slices")} | {cell("lut", ("vivado", "netlist"))} | {cell("ffs")} | '
              f'{cell("carry4", ("vivado", "netlist"))} | {cell("fmax_mhz", ("vivado", "netlist"))}/'
              f'{row.get("synth", {}).get("vivado_mhz", "-")} | '
              f'{row.get("synth", {}).get("estimate_mhz", "-")}', flush=True)


if __name__ == "__main__":
    main()
