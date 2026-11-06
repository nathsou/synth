#!/usr/bin/env python3
"""iCE40 QoR against iCEcube2 (development only).

Each design goes through two flows with the same device, package, pin
constraints, sources and clock target (--freq MHz for every clock):

- synth: `synth build` (--timing-allow-fail), for each of --seeds; the
  median run by Fmax (the `worst` figure below) is reported.
- iCEcube2: Lattice LSE synthesis, then the iCEcube2 back end as its IDE
  runs it: edifparser, sbtplacer, packer, sbrouter (timing driven, with
  LUT pin permutation), netlister, sbtimer and bitmap. Clocks are
  constrained in an SDC: the clock ports, and the clocks LSE finds on
  internal nets (a divided clock), all at --freq.

Both bitstreams are then decoded and timed by `synth time`, the same
static timing analysis (IceStorm's worst-case models) for both; sbtimer's
own Fmax for the iCEcube2 bitstream is reported too, as a check on that
referee. Logic cells and block RAMs are counted from the decoded
bitstreams alike. Fmax is the clock of the design's clock port (the first
port clock); `worst` is the fastest that clock can run with every clock
meeting timing (the slowest clock, a divided clock counting at its
multiple: the Euler solvers' UART reporter runs at half the clock,
constrained at half the target in both flows).

iCEcube2's synthesis is LSE: Synplify Pro, its default synthesis tool,
needs a FlexNet license that checks the host ID of `eth0`. With
--synthesis synplify the script runs it instead (through synpwrap), for
when that license works.

Needs iCEcube2 (ICECUBE2, default ~/lscc/iCEcube2.2020.12) with a license
(LM_LICENSE_FILE, default the one iCEcube2's config.xml names). Results go
to --out as results.json and the tools' logs, one directory per design.
"""
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
SYNTH = os.environ.get("SYNTH", str(ROOT / "target/release/synth"))
# The timer of both flows' bitstreams (SYNTH may be an older build to compare).
TIMER = os.environ.get("SYNTH_TIME", str(ROOT / "target/release/synth"))
ICECUBE = Path(os.environ.get("ICECUBE2", Path.home() / "lscc/iCEcube2.2020.12"))

# synth device name -> iCEcube2 device, device file, timing library, sbtimer
# family and device names.
DEVICES = {
    "hx1k": ("iCE40HX1K", "ICE40P01.dev", "ice40HX1K.lib", "iCE40", "HX1K"),
    "hx8k": ("iCE40HX8K", "ICE40P08.dev", "ice40HX8K.lib", "iCE40", "HX8K"),
    "lp1k": ("iCE40LP1K", "ICE40P01.dev", "ice40LP1K.lib", "iCE40", "LP1K"),
    "lp8k": ("iCE40LP8K", "ICE40P08.dev", "ice40LP8K.lib", "iCE40", "LP8K"),
}

D = ROOT / "tests/designs"
SERV = D / "third_party/serv"
HAZARD3 = D / "third_party/hazard3/hdl"
EULER = D / "euler"
EX = ROOT / "examples"


def hazard3_files():
    return [HAZARD3 / l.split()[1] for l in (HAZARD3 / "hazard3.f").read_text().splitlines()
            if l.startswith("file ")]


def design(name, dir, top, dev, pkg, pcf, files, includes=(), defines=None, params=None,
           divided=None):
    """`divided`: (net, n), a clock made in the design by dividing the clock
    port's by n, constrained at the target divided by n."""
    return dict(name=name, dir=Path(dir), top=top, dev=dev, pkg=pkg, pcf=pcf,
                files=[Path(dir) / f for f in files], includes=[Path(i) for i in includes],
                defines=defines or {}, params=params or {}, divided=divided)


def designs():
    sv = SERV / "servant"
    serv_core = sorted((SERV / "servile").glob("*.v")) + sorted((SERV / "rtl").glob("*.v"))
    out = [
        design("picosoc", D / "picosoc", "top", "hx8k", "cb132", "alchitry_cu.pcf",
               ["soc.v", "picorv32.v"]),
        design("serv", SERV / "sw", "servant", "hx8k", "cb132", None,
               [sv / f for f in ["servant.v", "servant_mux.v", "servant_ram.v",
                                 "servant_timer.v", "servant_gpio.v"]] + serv_core,
               params={"memfile": '"hello_uart.hex"'}),
        design("cu_hazard3", EX / "alchitry_cu_hazard3", "top", "hx8k", "cb132",
               "alchitry_cu.pcf", ["top.v"] + hazard3_files(), includes=[HAZARD3]),
        design("go_serv", EX / "go_board_serv", "top", "hx1k", "vq100", "go_board.pcf",
               ["top.v"] + [sv / f for f in ["servant.v", "servant_mux.v", "servant_ram.v",
                                             "servant_timer.v", "servant_gpio.v"]]
               + serv_core),
    ]
    for line in (EULER / "designs.txt").read_text().splitlines():
        f = line.split()
        if not f or f[0].startswith("#"):
            continue
        n, width = f[0], f[1]
        src = ["euler_top.v", "report.v", f"p0{n}.v"] + f[4:5]
        # The UART reporter runs on the clock divided by two (`slow`).
        out.append(design(f"euler{n}", EULER, "euler_top", "hx8k", "cb132", "alchitry_cu.pcf",
                          src, defines={"SOLVER": f"euler{n}", "ANSWER_WIDTH": width},
                          divided=("slow", 2)))
    return out


TIMEOUT = 1800


def run(cmd, log, cwd=None, env=None, timeout=None):
    timeout = timeout or TIMEOUT
    t = time.time()
    with open(log, "w") as f:
        try:
            rc = subprocess.run([str(c) for c in cmd], stdout=f, stderr=subprocess.STDOUT,
                                cwd=cwd, env=env, timeout=timeout).returncode
        except subprocess.TimeoutExpired:
            rc = -1
    return rc, time.time() - t


def ports_of_pcf(path):
    return {l.split()[1]: l.split()[2] for l in Path(path).read_text().splitlines()
            if l.startswith("set_io")}


def clock_ports(d):
    """Clock ports of a design: the inputs named like a clock."""
    names = {"clk", "wb_clk", "i_clk", "clock"}
    return [p for p in names if re.search(rf"\binput\b[^;]*\b{p}\b", top_source(d))]


def top_source(d):
    for f in d["files"]:
        text = f.read_text(errors="replace")
        if re.search(rf"\bmodule\s+{d['top']}\b", text):
            return text
    return ""


def parse_time(text):
    """`synth time` or `synth build` output: cells and clocks."""
    cells = {}
    m = re.search(r"^cells: (.*)$", text, re.M)
    if m:
        for part in m.group(1).split(", "):
            n, k = part.split(" ", 1)
            cells[k] = int(n)
    m = re.search(r"packed in [0-9.]+s: (.*)$", text, re.M)
    if m and not cells:
        for part in m.group(1).split(", "):
            n, k = part.split(" ", 1)
            cells[k] = int(n)
    clocks = {}
    for name, mhz in re.findall(r"clock (\S+): [0-9.]+ ns, ([0-9.]+) MHz", text):
        clocks[name] = float(mhz)
    return cells, clocks


def icecube_env():
    lic = os.environ.get("LM_LICENSE_FILE")
    if not lic:
        cfg = (ICECUBE / "config.xml").read_text()
        m = re.search(r"<LM_LICENSE_FILE[^>]*>([^<]*)<", cfg)
        lic = m.group(1) if m else ""
    env = dict(os.environ)
    env.update({
        "FOUNDRY": str(ICECUBE / "LSE"),
        "SBT_DIR": str(ICECUBE / "sbt_backend"),
        "SYNPLIFY_PATH": str(ICECUBE / "synpbase"),
        "LM_LICENSE_FILE": lic,
        "TCL_LIBRARY": str(ICECUBE / "sbt_backend/bin/linux/lib/tcl8.4"),
        "LD_LIBRARY_PATH": ":".join(str(ICECUBE / p) for p in [
            "sbt_backend/bin/linux/opt", "sbt_backend/bin/linux/opt/synpwrap",
            "sbt_backend/lib/linux/opt", "LSE/bin/lin64"]),
    })
    return env


def lse_project(d, w, args, dev):
    opts = [
        f"-a SBTiCE40", f"-d {dev}", f"-t {d['pkg'].upper()}",
        f"-optimization_goal {args.goal}", "-twr_paths 3", "-bram_utilization 100.00",
        "-ramstyle Auto", "-romstyle Auto", "-use_carry_chain 1", "-carry_chain_length 0",
        "-resource_sharing 1", "-propagate_constants 1", "-remove_duplicate_regs 1",
        "-max_fanout 10000", "-fsm_encoding_style Auto", "-use_io_insertion 1",
        "-use_io_reg auto", "-resolve_mixed_drivers 0", "-RWCheckOnRam 0",
        "-fix_gated_clocks 1", "-loop_limit 1950", f'-sdc "{w}/input.sdc"',
    ]
    for k, v in d["params"].items():
        # A string in braces (a concatenation of it): LSE drops the quotes.
        opts.append(f"-hdl_param {k} {{{v}}}" if v.startswith('"') else f"-hdl_param {k} {v}")
    opts.append(f'-ver "{w}/defines.v"')
    opts += [f'-ver "{f}"' for f in d["files"]]
    opts += [f'-p "{i}"' for i in d["includes"]] + [f'-p "{w}"']
    opts += [f"-top {d['top']}", f"-output_edif {w}/impl/impl.edf", f'-logfile "{w}/lse.log"']
    return "\n".join(opts) + "\n"


def synplify_project(d, w, dev):
    lines = [f"add_file -verilog -lib work {f}" for f in [w / "defines.v"] + d["files"]]
    lines += [f"add_file -constraint -lib work {w}/input.sdc", "impl -add impl -type fpga",
              "set_option -vlog_std v2001", f"set_option -top_module {d['top']}"]
    for i in d["includes"]:
        lines.append(f"set_option -include_path {{{i}}}")
    for k, v in d["params"].items():
        lines.append(f"set_option -hdl_param -set {k}={v}")
    lines += [
        "set_option -technology SBTiCE40", f"set_option -part {dev}",
        f"set_option -package {d['pkg'].upper()}", "set_option -speed_grade",
        'set_option -part_companion ""', "set_option -frequency auto",
        "set_option -write_verilog 0", "set_option -write_vhdl 0", "set_option -maxfan 10000",
        "set_option -disable_io_insertion 0", "set_option -pipe 1", "set_option -retiming 0",
        "set_option -update_models_cp 0", "set_option -fixgatedclocks 2",
        "set_option -fixgeneratedclocks 0", "set_option -popfeed 0", "set_option -constprop 0",
        "set_option -createhierarchy 0", "set_option -symbolic_fsm_compiler 1",
        "set_option -compiler_compatible 0", "set_option -resource_sharing 1",
        "set_option -write_apr_constraint 1", "project -result_format edif",
        f"project -result_file {w}/impl/impl.edf", "impl -active impl",
        "project -run synthesis -clean",
    ]
    return "\n".join(lines) + "\n"


def run_icecube(d, w, args):
    dev, devfile, lib, family, sbdev = DEVICES[d["dev"]]
    bins = ICECUBE / "sbt_backend/bin/linux/opt"
    devdir = ICECUBE / "sbt_backend/devices"
    env = icecube_env()
    top, pkg = d["top"], d["pkg"].upper()
    period = 1000.0 / args.freq
    r = {"steps": {}}
    for sub in ["impl", "outputs/placer", "outputs/packer", "outputs/router",
                "outputs/netlist", "outputs/bitmap", "outputs/timer", "netlist/Log/bitmap"]:
        (w / sub).mkdir(parents=True, exist_ok=True)
    (w / "defines.v").write_text("".join(f"`define {k} {v}\n" for k, v in d["defines"].items()))
    for hexfile in d["dir"].glob("*.hex"):
        shutil.copy(hexfile, w / hexfile.name)
    pcf = w / "input.pcf"
    pcf.write_text((d["dir"] / d["pcf"]).read_text() if d["pcf"] else "")
    sdc = "".join(f"create_clock -period {period:.3f} -name {{{p}}} [get_ports {{{p}}}]\n"
                  for p in clock_ports(d))
    (w / "input.sdc").write_text(sdc)

    def step(name, cmd, cwd=w):
        rc, t = run(cmd, w / f"{name}.log", cwd=cwd, env=env)
        r["steps"][name] = round(t, 1)
        if rc != 0:
            r["failed"] = name if rc > 0 else f"{name} (timeout)"
        return rc == 0

    if args.synthesis == "lse":
        (w / "lse.prj").write_text(lse_project(d, w, args, dev))
        ok = step("lse", [ICECUBE / "LSE/bin/lin64/synthesis", "-f", w / "lse.prj"])
    else:
        (w / "syn.prj").write_text(synplify_project(d, w, dev))
        ok = step("synplify", [bins / "synpwrap/synpwrap", "-prj", w / "syn.prj", "-log",
                               w / "syn.srr"])
    if not ok or not (w / "impl/impl.edf").exists():
        r.setdefault("failed", "synthesis")
        return r
    # LSE writes the constraints the back end reads (impl.scf), with the
    # clocks it found on internal nets (a divided clock) at a default
    # period: every clock gets the same target.
    scf = w / "impl/impl.scf"
    if scf.exists():
        def clock(m):
            p = period
            if d["divided"] and re.search(rf"get_nets\s+{{?\s*{d['divided'][0]}\b", m.group(0)):
                p *= d["divided"][1]
            return re.sub(r"-period\s+[0-9.]+\s+-waveform\s*\{[^}]*\}",
                          f"-period {p:.3f} -waveform {{0.000 {p / 2:.3f}}}", m.group(0))
        text = re.sub(r"create_clock\s+-period[^\n]*", clock, scf.read_text())
        scf.write_text(text)
        (w / "input.sdc").write_text(text)
    db = w / f"netlist/oadb-{top}"
    steps = [
        ("edifparser", [bins / "edifparser", devdir / devfile, w / "impl/impl.edf", w / "netlist",
                        f"-p{pkg}", f"-y{pcf}", f"-s{w}/input.sdc", "-c", "--devicename", dev]),
        ("placer", [bins / "sbtplacer", "--des-lib", db, "--outdir", w / "outputs/placer",
                    "--device-file", devdir / devfile, "--package", pkg, "--deviceMarketName",
                    dev, "--sdc-file", w / "Temp/sbt_temp.sdc", "--lib-file", devdir / lib,
                    "--effort_level", args.effort, "--out-sdc-file",
                    w / "outputs/placer/top_pl.sdc"] + (["--lut_pin_permutation"]
                                                         if args.lut_permutation else [])),
        ("packer", [bins / "packer", devdir / devfile, db, "--package", pkg, "--outdir",
                    w / "outputs/packer", "--translator",
                    ICECUBE / "sbt_backend/bin/sdc_translator.tcl", "--src_sdc_file",
                    w / "outputs/placer/top_pl.sdc", "--dst_sdc_file",
                    w / "outputs/packer/top_pk.sdc", "--devicename", dev]),
        ("router", [bins / "sbrouter", devdir / devfile, db, devdir / lib,
                    w / "outputs/packer/top_pk.sdc", "--outdir", w / "outputs/router",
                    "--sdf_file", w / "outputs/netlist/top_sbt.sdf", "--pin_permutation"]),
        ("netlister", [bins / "netlister", "--verilog", w / "outputs/netlist/top_sbt.v", "--vhdl",
                       w / "outputs/netlist/top_sbt.vhd", "--lib", db, "--view", "rt",
                       "--device", devdir / devfile, "--splitio", "--in-sdc-file",
                       w / "outputs/packer/top_pk.sdc", "--out-sdc-file",
                       w / "outputs/netlist/top_sbt.sdc"]),
    ]
    for name, cmd in steps:
        if not step(name, cmd):
            return r
    # sbtimer reads the device (for its derating) from a project file.
    for link in ["sbt", "x_Implmnt"]:
        if not (w / link).exists():
            os.symlink(".", w / link)
    (w / "x_sbt.project").write_text(
        f"[Project]\nImplementations=x_Implmnt\n\n[x_Implmnt]\nDeviceFamily={family}\n"
        f"Device={sbdev}\nDevicePackage={pkg}\nDevicevoltage=1.14\n"
        "DevicevoltagePerformance=+/-5%(datasheet default)\nDeviceTemperature=85\n"
        "TimingAnalysisBasedOn=Worst\nOperationRange=Commercial\n"
        "IOBankVoltages=topBank,2.5 bottomBank,2.5 leftBank,2.5 rightBank,2.5\n")
    step("sbtimer", [bins / "sbtimer", "--des-lib", w / f"x_Implmnt/sbt/netlist/oadb-{top}",
                     "--lib-file", devdir / lib, "--sdc-file", w / "outputs/netlist/top_sbt.sdc",
                     "--sdf-file", w / "outputs/netlist/top_sbt.sdf", "--report-file",
                     w / "outputs/timer/timing.rpt", "--device-file", devdir / devfile,
                     "--timing-summary"])
    if not step("bitmap", [bins / "bitmap", devdir / devfile, "--design", db, "--device_name",
                           dev, "--package", pkg, "--outdir", w / "outputs/bitmap", "--low_power",
                           "on", "--init_ram", "on", "--init_ram_bank", "1111", "--frequency",
                           "low", "--warm_boot", "on"]):
        return r
    r.pop("failed", None)
    r["seconds"] = round(sum(r["steps"].values()), 1)
    rpt = w / "outputs/timer/timing.rpt"
    if rpt.exists():
        text = rpt.read_text(errors="replace")
        r["sbtimer"] = {n: float(f) for n, f in
                        re.findall(r"^Clock: (\S+)\s*\| Frequency: ([0-9.]+) MHz", text, re.M)}
    bitstream = w / f"outputs/bitmap/{top}_bitmap.bin"
    r["bitstream"] = str(bitstream)
    # Pins as iCEcube2 placed them, for designs without constraints (from
    # the packer's table: `INPUT  wb_clk  location (0, 17, 0)  G1`).
    r["pcf"] = str(pcf)
    table = w / f"outputs/packer/{top}_io_pcf.log"
    if not d["pcf"] and table.exists():
        pins = re.findall(r"^(?:INPUT|OUTPUT|INOUT)\s+(\S+)\s+location\s*\([^)]*\)\s+(\S+)",
                          table.read_text(errors="replace"), re.M)
        placed = w / "placed.pcf"
        placed.write_text("".join(f"set_io {p} {pin}\n" for p, pin in pins))
        r["pcf"] = str(placed)
    return r


def time_bitstream(d, bitstream, pcf, freq, log):
    cmd = [TIMER, "time", bitstream, "--device", d["dev"], "--package", d["pkg"],
           "--freq", str(freq)]
    if pcf and Path(pcf).read_text().strip():
        cmd += ["--pcf", pcf]
    rc, _ = run(cmd, log)
    return parse_time(Path(log).read_text()) if rc == 0 else ({}, {})


def run_synth(d, w, args, seed=1):
    w = w / f"seed{seed}"
    w.mkdir(parents=True, exist_ok=True)
    cmd = [SYNTH, "build", *d["files"], "--top", d["top"], "--device", d["dev"], "--package",
           d["pkg"], "--seed", str(seed), "--freq", str(args.freq), "--timing-allow-fail",
           "-o", w / "synth.bin"]
    for i in d["includes"]:
        cmd += ["-I", i]
    for k, v in d["defines"].items():
        cmd += ["-D", f"{k}={v}"]
    for k, v in d["params"].items():
        cmd += ["-P", f"{k}={v}"]
    if d["pcf"] or d["divided"]:
        pcf = w / "synth.pcf"
        text = (d["dir"] / d["pcf"]).read_text() if d["pcf"] else ""
        if d["divided"]:
            net, div = d["divided"]
            text += f"\nset_frequency {net} {args.freq / div:g}\n"
        pcf.write_text(text)
        cmd += ["--pcf", pcf]
    rc, t = run(cmd, w / "synth.log", cwd=d["dir"])
    r = {"seconds": round(t, 1)}
    if rc != 0 or not (w / "synth.bin").exists():
        r["failed"] = "build"
        return r
    _, r["build_clocks"] = parse_time((w / "synth.log").read_text())
    r["bitstream"] = str(w / "synth.bin")
    r["pcf"] = str(d["dir"] / d["pcf"]) if d["pcf"] else None
    return r


def summarise(d, flow, r):
    """Cells and Fmax of a flow's bitstream, timed by `synth time`."""
    if "bitstream" not in r:
        return
    cells, clocks = time_bitstream(d, r["bitstream"], r.get("pcf"), r["freq"],
                                   Path(r["bitstream"]).parent / f"{flow}.time.log")
    if not clocks and "build_clocks" in r:
        clocks = r["build_clocks"]  # no pin constraints to find the clock pads
    r["lc"] = cells.get("ICESTORM_LC")
    r["ram"] = cells.get("ICESTORM_RAM", 0)
    r["clocks"] = clocks
    ports = [c for c in clock_ports(d) if c in clocks]
    r["fmax"] = clocks[ports[0]] if ports else (min(clocks.values()) if clocks else None)
    # The fastest the clock port can run with every clock meeting timing:
    # a divided clock may be that many times slower (decoded bitstreams
    # name no nets: there, every clock but the port's is the divided one).
    div = d["divided"][1] if d["divided"] else 1
    r["worst"] = min((f if n in ports else f * div for n, f in clocks.items()), default=None)


def one(d, args):
    w = Path(args.out) / d["name"]
    if w.exists() and not args.icecube2_from:
        shutil.rmtree(w)
    w.mkdir(parents=True, exist_ok=True)
    res = {"design": d["name"], "device": d["dev"], "package": d["pkg"]}
    if "synth" in args.flows:
        # Each seed; the median by Fmax (iCEcube2 has no seed).
        runs = []
        for seed in args.seeds:
            s = run_synth(d, w, args, seed)
            s["freq"], s["seed"] = args.freq, seed
            summarise(d, "synth", s)
            runs.append(s)
        ok = sorted((r for r in runs if r.get("worst")), key=lambda r: r["worst"])
        s = dict(ok[(len(ok) - 1) // 2] if ok else runs[0])
        s["seeds"] = {r["seed"]: [r.get("fmax"), r.get("worst")] for r in runs}
        s["seconds"] = round(sorted(r["seconds"] for r in runs)[(len(runs) - 1) // 2], 1)
        res["synth"] = s
    if args.icecube2_from:
        # iCEcube2's results do not depend on synth's: reuse earlier runs.
        for run_dir in args.icecube2_from:
            prior = json.loads((Path(run_dir) / "results.json").read_text())["results"]
            for r in prior:
                if r["design"] == d["name"] and "icecube2" in r:
                    res["icecube2"] = r["icecube2"]
    elif "icecube2" in args.flows:
        ic = run_icecube(d, w / "icecube2", args)
        ic["freq"] = args.freq
        summarise(d, "icecube2", ic)
        res["icecube2"] = ic
    return res


def fmt(v, f="{:.1f}"):
    return "-" if v is None else f.format(v)


def table(results, synthesis="lse"):
    name = {"lse": "LSE", "synplify": "Synplify Pro"}[synthesis]
    print(f"{'':14} {'synth':>27}   {'iCEcube2 (' + name + ')':>36}")
    print(f"{'design':14} {'LC':>6} {'RAM':>3} {'Fmax':>6} {'worst':>6} {'s':>4}   "
          f"{'LC':>6} {'RAM':>3} {'Fmax':>6} {'worst':>6} {'sbtime':>6} {'s':>5}")
    for r in results:
        s, ic = r.get("synth", {}), r.get("icecube2", {})
        def cols(x, sb=False):
            if "lc" not in x:
                return f"{'FAIL ' + x.get('failed', ''):>27}" if not sb else \
                    f"{'FAIL ' + x.get('failed', ''):>36}"
            c = f"{fmt(x['lc'], '{}'):>6} {fmt(x['ram'], '{}'):>3} {fmt(x['fmax']):>6} " \
                f"{fmt(x['worst']):>6}"
            if sb:
                sbt = x.get("sbtimer", {})
                ports = [p for p in sbt if "|" not in p]
                c += f" {fmt(sbt[ports[0]] if ports else None):>6} {fmt(x.get('seconds')):>5}"
            else:
                c += f" {fmt(x.get('seconds'), '{:.0f}'):>4}"
            return c
        print(f"{r['design']:14} {cols(s)}   {cols(ic, True)}")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--designs", nargs="*", help="design names (default: all)")
    ap.add_argument("--freq", type=float, default=200.0, help="clock target, MHz")
    ap.add_argument("--flows", nargs="*", default=["synth", "icecube2"])
    ap.add_argument("--seeds", nargs="*", type=int, default=[1, 2, 3],
                    help="synth placement seeds (the table shows the median by worst Fmax)")
    ap.add_argument("--synthesis", choices=["lse", "synplify"], default="lse")
    ap.add_argument("--goal", choices=["Timing", "Area"], default="Timing",
                    help="LSE optimization goal")
    ap.add_argument("--effort", choices=["std", "mid", "high"], default="std",
                    help="iCEcube2 placer effort")
    ap.add_argument("--lut-permutation", action="store_true",
                    help="iCEcube2 placer LUT pin permutation (for packing)")
    ap.add_argument("-j", "--jobs", type=int, default=4)
    ap.add_argument("--timeout", type=int, default=1800, help="seconds per tool run")
    ap.add_argument("--out", default=str(ROOT / "target/icecube2"))
    ap.add_argument("--table", action="store_true", help="only print the table of --out")
    ap.add_argument("--icecube2-from", nargs="*", default=[], metavar="DIR",
                    help="reuse the iCEcube2 results of earlier runs (later ones win)")
    args = ap.parse_args()
    global TIMEOUT
    TIMEOUT = args.timeout
    out = Path(args.out)
    if args.table:
        data = json.loads((out / "results.json").read_text())
        table(data["results"], data["meta"].get("synthesis", "lse"))
        return
    out.mkdir(parents=True, exist_ok=True)
    ds = [d for d in designs() if not args.designs or d["name"] in args.designs]
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as ex:
        results = list(ex.map(lambda d: one(d, args), ds))
    meta = {"freq": args.freq, "seeds": args.seeds, "synthesis": args.synthesis, "goal": args.goal,
            "effort": args.effort, "lut_permutation": args.lut_permutation,
            "synth": subprocess.run(["git", "-C", ROOT, "describe", "--always", "--dirty"],
                                    capture_output=True, text=True).stdout.strip()}
    (out / "results.json").write_text(json.dumps({"meta": meta, "results": results}, indent=1,
                                                 default=str))
    table(results, args.synthesis)


if __name__ == "__main__":
    main()
