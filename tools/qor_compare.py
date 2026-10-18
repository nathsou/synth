#!/usr/bin/env python3
"""Compare two synth policies on frozen board designs, using patched icetime.

Example (ICETIME and ICETIME_CHIPDB select the patched timer/database):
  python3 tools/qor_compare.py --synth target/release/synth --out work/qor

The baseline forces carry width 3; the candidate uses the automatic default.
Use --candidate-flags='--carry-min-width 8' for the fixed-width experiment.
Serial execution is the default so build wall times are comparable. Parallel
runs are suitable for QoR screening, not controlled runtime comparisons.
"""
import argparse
import concurrent.futures
import csv
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]


def designs():
    d = ROOT / "tests/designs"
    e = ROOT / "examples"
    serv = d / "third_party/serv"
    hazard = d / "third_party/hazard3/hdl"
    sf = [serv / "servant" / n for n in ["servant.v", "servant_mux.v", "servant_ram.v",
          "servant_timer.v", "servant_gpio.v"]]
    sf += sorted((serv / "servile").glob("*.v")) + sorted((serv / "rtl").glob("*.v"))
    hf = [hazard / line.split()[1] for line in (hazard / "hazard3.f").read_text().splitlines()
          if line.startswith("file")]
    def spec(cwd, top, args, pcf="alchitry_cu.pcf", device="hx8k", package="cb132"):
        return dict(cwd=cwd, top=top, args=list(map(str, args)), pcf=pcf,
                    device=device, package=package)
    result = {
        "picosoc": spec(d / "picosoc", "top", ["soc.v", "picorv32.v"]),
        "serv": spec(serv / "sw", "servant", sf + ["-P", 'memfile="hello_uart.hex"'], pcf=None),
        "cu_hazard3": spec(e / "alchitry_cu_hazard3", "top", ["top.v"] + hf + ["-I", hazard]),
        "go_serv": spec(e / "go_board_serv", "top", ["top.v"] + sf,
                        "go_board.pcf", "hx1k", "vq100"),
    }
    for line in (d / "euler/designs.txt").read_text().splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        n, width, _, _, *extra = line.split()
        if n == "10":  # Does not fit the target, for either policy.
            continue
        result["euler" + n] = spec(d / "euler", "euler_top",
            ["euler_top.v", "report.v", "p0" + n + ".v"] + extra +
            ["-D", "SOLVER=euler" + n, "-D", "ANSWER_WIDTH=" + width])
    return result


def invoke(command, cwd, env, log, timeout):
    start = time.monotonic()
    with log.open("w") as output:
        try:
            code = subprocess.run(command, cwd=cwd, env=env, stdout=output,
                                  stderr=subprocess.STDOUT, timeout=timeout).returncode
        except subprocess.TimeoutExpired:
            code = 124
    return code, time.monotonic() - start


def main():
    specs = designs()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--synth", type=Path, default=ROOT / "target/release/synth")
    parser.add_argument("--baseline-synth", type=Path)
    parser.add_argument("--baseline-flags", default="--carry-min-width 3")
    parser.add_argument("--candidate-flags", default="")
    parser.add_argument("--candidate-env", action="append", default=[], metavar="NAME=VALUE")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--designs", default=",".join(specs))
    parser.add_argument("--seeds", default="1,2,3")
    parser.add_argument("--workers", type=int, default=1)
    parser.add_argument("--timeout", type=int, default=600)
    args = parser.parse_args()
    extra_env = dict(pair.split("=", 1) for pair in args.candidate_env)
    args.out = args.out.resolve()
    args.out.mkdir(parents=True, exist_ok=True)
    binaries = {"baseline": (args.baseline_synth or args.synth).resolve(),
                "candidate": args.synth.resolve()}
    hashes = {v: hashlib.sha256(p.read_bytes()).hexdigest() for v, p in binaries.items()}
    flags = {"baseline": shlex.split(args.baseline_flags),
             "candidate": shlex.split(args.candidate_flags)}
    def run(job):
        name, seed, variant = job
        spec = specs[name]
        output = args.out / name / str(seed) / variant
        output.mkdir(parents=True, exist_ok=True)
        asc = output / "design.asc"
        env = {k: v for k, v in os.environ.items() if not k.startswith("QOR_")}
        env["ICETIME_FEEDOUT"] = "1"
        if variant == "candidate":
            env.update(extra_env)
        command = [str(binaries[variant]), "build", *spec["args"], "--top", spec["top"],
                   "--device", spec["device"], "--package", spec["package"], "--seed", str(seed),
                   "--freq", "200", "--timing-allow-fail", "--asc", str(asc),
                   "-o", str(output / "design.bin"), *flags[variant]]
        if spec["pcf"]:
            command += ["--pcf", spec["pcf"]]
        rc, seconds = invoke(command, spec["cwd"], env, output / "build.log", args.timeout)
        text = (output / "build.log").read_text()
        cells = re.findall(r"(\d+) ICESTORM_LC", text)
        result = dict(design=name, seed=seed, variant=variant, rc=rc, seconds=seconds,
                      cells=int(cells[-1]) if cells else None, mhz=None,
                      binary_sha256=hashes[variant], command=command,
                      candidate_env=extra_env if variant == "candidate" else {})
        if rc == 0:
            timer = [env.get("ICETIME", "icetime"), "-d", spec["device"], "-P", spec["package"]]
            if env.get("ICETIME_CHIPDB"):
                timer += ["-C", str(Path(env["ICETIME_CHIPDB"]) /
                                   ("chipdb-" + spec["device"][2:] + ".txt"))]
            timer += ["-t", str(asc)]
            trc, _ = invoke(timer, spec["cwd"], env, output / "icetime.log", args.timeout)
            match = re.search(r"Total path delay: ([\d.]+) ns \(([\d.]+) MHz\)",
                              (output / "icetime.log").read_text())
            result.update(timing_rc=trc, timer_command=timer,
                          asc_sha256=hashlib.sha256(asc.read_bytes()).hexdigest())
            if trc == 0 and match:
                result.update(period_ns=float(match[1]), mhz=float(match[2]))
        (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(name, seed, variant, "rc", rc, "MHz", result["mhz"], flush=True)
        return result
    # Alternate AB/BA order by seed for serial runtime comparisons.
    jobs = [(d, int(s), v) for d in args.designs.split(",") for s in args.seeds.split(",")
            for v in (["baseline", "candidate"] if int(s) % 2 else ["candidate", "baseline"])]
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as pool:
        rows = list(pool.map(run, jobs))
    (args.out / "results.json").write_text(json.dumps(rows, indent=2) + "\n")
    with (args.out / "results.csv").open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=["design", "seed", "variant", "rc", "cells",
            "mhz", "seconds", "binary_sha256", "asc_sha256"], extrasaction="ignore")
        writer.writeheader()
        writer.writerows(rows)
    return int(any(r["rc"] != 0 or r["mhz"] is None for r in rows))


if __name__ == "__main__":
    raise SystemExit(main())
