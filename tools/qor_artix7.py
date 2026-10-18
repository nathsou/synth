#!/usr/bin/env python3
"""Artix-7 synthesis QoR on the existing Euler corpus; no PnR/Fmax claims.

Both flows use the same sources/top, flatten hierarchy, and omit I/O and clock
buffers. Yosys otherwise uses its default synth_xilinx -family xc7 flow,
including DSP, LUTRAM, SRL and wide mux inference. Preserve all reports and
netlists in --out. Nonzero tool exits fail the run; missing tools are errors.
"""
import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
SYNTH = os.environ.get("SYNTH", str(ROOT / "target/release/synth"))


def run(cmd, log):
    start = time.monotonic()
    with log.open("w") as f:
        subprocess.run(list(map(str, cmd)), cwd=ROOT, stdout=f, stderr=subprocess.STDOUT,
                       timeout=1800, check=True)
    return round(time.monotonic() - start, 3)


def counts(path, top):
    cells = json.loads(path.read_text())["modules"][top]["cells"]
    c = Counter(cell["type"] for cell in cells.values())
    c.pop("$scopeinfo", None)
    # Include standalone INV cells (which occupy LUTs) on the same basis
    # as synth's LUT1 inverters. Keep all other resource types separately.
    return {"lut": sum(c.get(f"LUT{i}", 0) for i in range(1, 7)) + c.get("INV", 0),
            "ff": sum(n for k, n in c.items() if k.startswith("FD")),
            "carry4": c.get("CARRY4", 0),
            "bram18": c.get("RAMB18E1", 0) + 2 * c.get("RAMB36E1", 0),
            "dsp48": c.get("DSP48E1", 0), "cells": dict(sorted(c.items()))}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--design", action="append", help="Euler number; repeat to select a subset")
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    versions = {"yosys": subprocess.check_output(["yosys", "-V"], text=True).strip(),
                "synth": subprocess.check_output([SYNTH, "version"], text=True).strip(),
                "revision": subprocess.check_output(["git", "describe", "--always", "--dirty"], cwd=ROOT, text=True).strip()}
    report = {"versions": versions, "designs": []}
    print(json.dumps(versions), flush=True)
    print("design | LUT synth/yosys | FF | CARRY4 | BRAM18 | DSP48 | seconds", flush=True)
    source_dir = ROOT / "tests/designs/euler"
    for line in (source_dir / "designs.txt").read_text().splitlines():
        if not line or line.startswith("#"):
            continue
        n, _, _, _, *extra = line.split()
        if args.design and n not in args.design:
            continue
        top = f"euler{n}"
        files = [source_dir / f"p0{n}.v", *[source_dir / f for f in extra]]
        hashes = {str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest() for p in files}
        row = {"design": top, "sources": hashes}
        net = out / f"{top}.synth.v"
        sj, yj = out / f"{top}.synth.json", out / f"{top}.yosys.json"
        row["synth_seconds"] = run([SYNTH, "synth", *files, "--top", top, "--family", "artix7", "--verilog", net], out / f"{top}.synth.log")
        run(["yosys", "-Q", "-T", "-p",
             f'read_verilog -lib +/xilinx/cells_sim.v; read_verilog "{net}"; hierarchy -check -top {top}; check -assert; blackbox =A:whitebox; write_json "{sj}"'],
            out / f"{top}.validate.log")
        src = " ".join(f'"{p}"' for p in files)
        row["yosys_seconds"] = run(["yosys", "-Q", "-T", "-p",
             f'read_verilog {src}; synth_xilinx -family xc7 -flatten -top {top} -noiopad -noclkbuf; write_json "{yj}"'], out / f"{top}.yosys.log")
        row["synth"], row["yosys"] = counts(sj, top), counts(yj, top)
        report["designs"].append(row)
        (out / "results.json").write_text(json.dumps(report, indent=2) + "\n")
        fields = " | ".join(f'{row["synth"][k]}/{row["yosys"][k]}' for k in ["lut", "ff", "carry4", "bram18", "dsp48"])
        print(f'{top} | {fields} | {row["synth_seconds"]}/{row["yosys_seconds"]}', flush=True)
    if not report["designs"]:
        parser.error("no matching designs")


if __name__ == "__main__":
    main()
