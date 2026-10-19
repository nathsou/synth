#!/usr/bin/env python3
"""Routed Artix-7 comparison: synth's native flow against Yosys+nextpnr-xilinx.

Each Euler design is implemented three ways on one part with the same pin
constraints: synth from source to bitstream, Yosys synth_xilinx followed by
nextpnr-xilinx, and Yosys's netlist through synth's native place and route
(a matched backend). Resources are counted from each flow's FASM in the same
way: occupied slices, LUT sites and flip-flops, block RAMs, DSPs and routing
pips. Each flow's timing estimate is recorded: nextpnr's, and synth's from
the same routing delay model; neither is calibrated against vendor timing.
nextpnr-xilinx does not time block RAM or DSP cells, so synth's estimate
without those paths (`fabric_mhz`) is the one comparable with nextpnr's.
A flow that fails records its error. With --seeds N, each flow is placed
and routed with seeds 1 to N and the run with the median estimate is
reported, with every run's results: one run's Fmax varies by up to 10%.

Set up nextpnr-xilinx with tools/prepare_nextpnr_xilinx.sh.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
SYNTH = os.environ.get("SYNTH", str(ROOT / "target/release/synth"))
spec = importlib.util.spec_from_file_location("physical", ROOT / "tools/ci_artix7_physical.py")
physical = importlib.util.module_from_spec(spec)
spec.loader.exec_module(physical)


def run(cmd, log):
    start = time.monotonic()
    with log.open("w") as f:
        p = subprocess.run(list(map(str, cmd)), cwd=ROOT, stdout=f, stderr=subprocess.STDOUT,
                           timeout=7200)
    seconds = round(time.monotonic() - start, 3)
    if p.returncode != 0:
        lines = [l for l in log.read_text().splitlines() if "rror" in l]
        return seconds, (lines[-1] if lines else f"exit {p.returncode}")
    return seconds, None


def resources(fasm):
    """Resources a FASM file configures, counted alike for every flow."""
    slices, luts, ffs, brams, dsps, pips = set(), set(), set(), set(), set(), 0
    for line in fasm.read_text().splitlines():
        feature = line.split("=")[0].strip()
        if m := re.match(r"(\S+?)\.(SLICE[LM]_X\d)\.(\S+)", feature):
            site = (m[1], m[2])
            slices.add(site)
            if lut := re.match(r"([A-D])LUT\.(INIT|RAM|SRL)", m[3]):
                luts.add(site + (lut[1],))
            if ff := re.match(r"([A-D]5?)FFMUX\.", m[3]):
                ffs.add(site + (ff[1],))
        elif m := re.match(r"(BRAM_[LR]_X\d+Y\d+)\.RAMB18_(Y\d)\.IN_USE", feature):
            brams.add(m.groups())
        elif m := re.match(r"(DSP_[LR]_X\d+Y\d+)\.DSP48\.DSP_(\d)\.", feature):
            dsps.add(m.groups())
        elif re.match(r"INT_[LR]_X\d+Y\d+\.\S+\.\S+$", feature):
            pips += 1
    return {"slices": len(slices), "lut_sites": len(luts), "ffs": len(ffs),
            "bram18": len(brams), "dsp48": len(dsps), "int_pips": pips}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--db", type=Path, required=True, help="prjxray-db/artix7")
    p.add_argument("--nextpnr", type=Path, required=True, help="nextpnr-xilinx executable")
    p.add_argument("--chipdb", type=Path, required=True, help="directory of <part>.bin chip databases")
    p.add_argument("--part", default="xc7a100tcsg324-1")
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--design", action="append", help="Euler number; repeat to select a subset")
    p.add_argument("--seeds", type=int, default=1,
                   help="place and route each flow with seeds 1..N and report the median")
    a = p.parse_args()
    out = a.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    versions = {"yosys": subprocess.check_output(["yosys", "-V"], text=True).strip(),
                "synth": subprocess.check_output([SYNTH, "version"], text=True).strip(),
                "revision": subprocess.check_output(["git", "describe", "--always", "--dirty"],
                                                    cwd=ROOT, text=True).strip()}
    report = {"part": a.part, "versions": versions, "timing_certified": False, "designs": []}
    euler = ROOT / "tests/designs/euler"
    print("design | slices synth/nextpnr/matched | LUT sites | FFs | INT pips | estimated MHz | fabric MHz", flush=True)
    for line in (euler / "designs.txt").read_text().splitlines():
        if not line or line.startswith("#"):
            continue
        n, width, _, _, *extra = line.split()
        if a.design and n not in a.design:
            continue
        top = f"euler{n}"
        files = [euler / f"p0{n}.v", *[euler / f for f in extra]]
        prefix = out / top
        ports = ["clk", "rst", "start", "done"] + [f"answer[{i}]" for i in range(int(width))]
        xdc = prefix.with_suffix(".xdc")
        xdc.write_text(physical.constraints(a, a.part, ports, ["clk"]))
        # nextpnr's XDC reader takes create_clock targets without braces.
        npx_xdc = prefix.with_suffix(".nextpnr.xdc")
        npx_xdc.write_text(re.sub(r"\[get_ports \{(\w+)\}\]$", r"[get_ports \1]", xdc.read_text(), flags=re.M))
        row = {"design": top}
        flows = {
            "synth": [[SYNTH, "build", *files, "--top", top, "--family", "artix7", "--db", a.db,
                       "--device", a.part, "--xdc", xdc, "--fasm", f"{prefix}.synth.fasm",
                       "-o", f"{prefix}.synth.bit", "--timing-allow-fail"]],
            "nextpnr": [["yosys", "-Q", "-p", f'read_verilog {" ".join(map(str, files))}; '
                         f'synth_xilinx -family xc7 -flatten -top {top}; delete t:$scopeinfo; '
                         f'write_json "{prefix}.yosys.json"'],
                        [a.nextpnr, "--chipdb", a.chipdb / f"{a.part}.bin", "--xdc", npx_xdc,
                         "--json", f"{prefix}.yosys.json", "--fasm", f"{prefix}.nextpnr.fasm",
                         "--timing-allow-fail"]],
            "matched": [["yosys", "-Q", "-p", f'read_verilog {" ".join(map(str, files))}; '
                         f'synth_xilinx -family xc7 -flatten -top {top} -noiopad -noclkbuf; '
                         f'write_rtlil "{prefix}.yosys.il"'],
                        [SYNTH, "pnr", f"{prefix}.yosys.il", "--family", "artix7", "--db", a.db,
                         "--device", a.part, "--xdc", xdc, "--fasm", f"{prefix}.matched.fasm",
                         "-o", f"{prefix}.matched.bit", "--timing-allow-fail"]],
        }
        for flow, steps in flows.items():
            # Synthesis once, then place and route once per seed; the run
            # with the median timing estimate is reported, with every run's.
            result = {"seconds": 0.0}
            for i, cmd in enumerate(steps[:-1]):
                seconds, error = run(cmd, out / f"{top}.{flow}.{i}.log")
                result["seconds"] = round(result["seconds"] + seconds, 3)
                if error:
                    result["error"] = error
                    break
            else:
                runs = []
                for seed in range(1, a.seeds + 1):
                    log_path = out / f"{top}.{flow}.{len(steps) - 1}.seed{seed}.log"
                    seconds, error = run(steps[-1] + ["--seed", seed], log_path)
                    if error:
                        result["error"] = error
                        break
                    r = {"seed": seed, "seconds": round(result["seconds"] + seconds, 3)}
                    r.update(resources(Path(f"{prefix}.{flow}.fasm")))
                    log = log_path.read_text()
                    if flow == "nextpnr":
                        mhz = re.findall(r"Max frequency for clock .*?: ([\d.]+) MHz", log)
                    else:
                        # The same routing delay model, uncalibrated like nextpnr's.
                        mhz = re.findall(r"timing estimate: clock .*?\(([\d.]+) MHz\)", log)
                        # nextpnr-xilinx has no block RAM or DSP timing; this
                        # estimate leaves them out alike.
                        fabric = re.findall(
                            r"without block RAM and DSP paths: clock .*?\(([\d.]+) MHz\)", log)
                        r["fabric_mhz"] = float((fabric or mhz)[-1]) if fabric or mhz else None
                    r["fmax_mhz"] = float(mhz[-1]) if mhz else None
                    r.setdefault("fabric_mhz", r["fmax_mhz"])
                    runs.append(r)
                if runs and "error" not in result:
                    ranked = sorted(runs, key=lambda r: (r["fmax_mhz"] or 0.0, r["seed"]))
                    result = dict(ranked[(len(ranked) - 1) // 2])
                    result["runs"] = runs
            row[flow] = result
        report["designs"].append(row)
        (out / "results.json").write_text(json.dumps(report, indent=2) + "\n")
        cell = lambda key: "/".join(str(row[f].get(key, "-")) for f in flows)
        print(f'{top} | {cell("slices")} | {cell("lut_sites")} | {cell("ffs")} | {cell("int_pips")} | '
              f'{cell("fmax_mhz")} | {cell("fabric_mhz")}', flush=True)


if __name__ == "__main__":
    main()
