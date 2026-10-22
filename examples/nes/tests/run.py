#!/usr/bin/env python3
# SPDX-License-Identifier: CERN-OHL-W-2.0
"""Run public NES diagnostics against the compiled Weft hardware.

No ROMs are downloaded or distributed by this script. Every selected test must
report a passing result; missing ROMs, failures and timeouts are errors.
"""
import argparse
import concurrent.futures
import fnmatch
import json
from pathlib import Path
import subprocess
import sys
import time

GROUPS = [
    "instr_test-v5/rom_singles/*.nes",
    "instr_misc/rom_singles/*.nes",
    "instr_timing/rom_singles/*.nes",
    "ppu_vbl_nmi/rom_singles/*.nes",
    "apu_test/rom_singles/*.nes",
    "cpu_interrupts_v2/rom_singles/*.nes",
    "mmc3_test_2/rom_singles/[1-5]-*.nes",
    "cpu_reset/*.nes",
    "apu_reset/*.nes",
    "cpu_dummy_writes/*.nes",
    "ppu_open_bus/*.nes",
    "ppu_read_buffer/*.nes",
    "sprite_hit_tests_2005.10.05/*.nes",
    "sprite_overflow_tests/*.nes",
    "dmc_dma_during_read4/*.nes",
    "sprdma_and_dmc_dma/*.nes",
    "vbl_nmi_timing/*.nes",
    "branch_timing_tests/*.nes",
    "blargg_apu_2005.07.30/*.nes",
    "blargg_ppu_tests_2005.09.15b/*.nes",
    "cpu_exec_space/*.nes",
    "oam_read/*.nes",
    "oam_stress/*.nes",
    "mmc3_test/[1-5]-*.nes",
    "cpu_timing_test6/*.nes",
]

# Diagnostics that predate the $6000 protocol report results in other ways.
LEGACY_RESULT = {"sprite_hit_tests_2005.10.05", "sprite_overflow_tests", "vbl_nmi_timing", "branch_timing_tests"}
CONSOLE_CODE = {"blargg_apu_2005.07.30", "blargg_ppu_tests_2005.09.15b"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sim", required=True, type=Path)
    parser.add_argument("--roms", required=True, type=Path)
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--frames", type=int, default=1800)
    parser.add_argument("--timeout", type=int, default=600)
    parser.add_argument("--only", action="append", help="filter relative ROM paths with glob patterns")
    parser.add_argument("--report", type=Path, help="save all results and diagnostic text as JSON")
    args = parser.parse_args()
    if args.jobs < 1 or args.frames < 1 or args.timeout < 1:
        parser.error("jobs, frames and timeout must be positive")
    if not args.sim.is_file():
        parser.error(f"simulator not found: {args.sim}")
    roms = []
    for pattern in GROUPS:
        found = sorted(args.roms.glob(pattern))
        if not found:
            parser.error(f"missing diagnostic ROM group: {pattern}")
        roms.extend(found)
    if args.only:
        roms = [p for p in roms if any(fnmatch.fnmatch(p.relative_to(args.roms).as_posix(), f) for f in args.only)]
    if not roms:
        parser.error("no ROMs selected")

    def run(path):
        start = time.monotonic()
        mode = ["--test"]
        if path.parent.name in LEGACY_RESULT:
            mode = ["--legacy-result", "f8"]
        elif path.parent.name in CONSOLE_CODE:
            mode = ["--console-code"]
        elif path.parent.name == "cpu_timing_test6":
            # Holding B at power-up selects official and undocumented opcodes.
            mode = ["--input", str(Path(__file__).with_name("hold_b.txt")), "--console-pass", "PASSED",
                    "--console-fail", "FAIL OP", "--console-fail", "UNKNOWN ERROR", "--console-fail", "BASIC TIMING WRONG"]
        elif path.parent.name == "dmc_dma_during_read4":
            mode = ["--console-test"]
            if path.name == "dma_2007_read.nes":
                mode += ["--console-crc", "159A7A8F", "--console-crc", "5E3DF9C4"]
            elif path.name == "double_2007_read.nes":
                mode += ["--console-crc", "85CFD627", "--console-crc", "F018C287", "--console-crc", "440EF923", "--console-crc", "E52F41A5"]
        command = [str(args.sim.resolve()), str(path), *mode, "--frames", str(args.frames)]
        try:
            proc = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                  text=True, timeout=args.timeout, check=False)
            code, output = proc.returncode, proc.stdout
        except subprocess.TimeoutExpired as exc:
            code, output = 2, f"Process exceeded {args.timeout} seconds\n{exc.stdout or ''}"
        except OSError as exc:
            code, output = 1, str(exc)
        return {"rom": path.relative_to(args.roms).as_posix(), "code": code,
                "seconds": round(time.monotonic() - start, 3), "output": output}

    results = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as executor:
        for result in executor.map(run, roms):
            results.append(result)
            print(f"{'PASS' if result['code'] == 0 else 'FAIL'} {result['rom']} ({result['seconds']:.1f}s)", flush=True)
            if result["code"]:
                print(result["output"], flush=True)
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(results, indent=2) + "\n")
    failed = sum(r["code"] != 0 for r in results)
    print(f"{len(results) - failed}/{len(results)} passed")
    return int(failed != 0)


if __name__ == "__main__":
    sys.exit(main())
