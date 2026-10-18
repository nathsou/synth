#!/usr/bin/env python3
"""Compare seed-1 edited benchmark bitstreams with their corresponding RTL."""
import argparse
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--out', type=Path, required=True)
parser.add_argument('--designs', nargs='+')
parser.add_argument('--modes', nargs='+', choices=['fresh', 'incremental'], default=['incremental'])
parser.add_argument('--edits', nargs='+')
parser.add_argument('--cycles', type=int)
args = parser.parse_args()
root = args.out.resolve()
bitsim = Path(__file__).resolve().with_name('bitsim.py')
cases = [
    ('blinky_ram', 'top', 'vq100', 'go_board.pcf', 10000, [], []),
    ('picosoc', 'top', 'cb132', 'alchitry_cu.pcf', 200000, [], ['picorv32.v']),
    ('euler31', 'euler_top', 'cb132', 'alchitry_cu.pcf', 100000,
     ['--const', 'rst_n=1', '-D', 'SOLVER=euler31', '-D', 'ANSWER_WIDTH=17'],
     ['euler_top.v', 'report.v']),
    ('euler48', 'euler_top', 'cb132', 'alchitry_cu.pcf', 150000,
     ['--const', 'rst_n=1', '-D', 'SOLVER=euler48', '-D', 'ANSWER_WIDTH=34'],
     ['euler_top.v', 'report.v']),
]
failed = []
for design, top, package, pcf, cycles, flags, sources in cases:
    if args.designs and design not in args.designs:
        continue
    cycles = args.cycles or cycles
    directory = root / design
    if not directory.exists():
        continue
    for label in args.edits or (['small', 'large'] + (['multiply'] if design == 'picosoc' else [])):
        for mode in args.modes:
            asc = directory / f'1-{label}-{mode}.asc'
            if not asc.exists():
                raise SystemExit(f'Missing benchmark output: {asc}')
            print(f'VALIDATE {design}/{label}/{mode}', flush=True)
            cmd = ['python3', str(bitsim), '--asc', str(asc),
                   '--pcf', str(directory / pcf), '--package', package, '--top', top,
                   '--clock', 'clk', '--cycles', str(cycles), *flags,
                   str(directory / 'variants' / f'{label}.v'),
                   *[str(directory / source) for source in sources]]
            if subprocess.run(cmd, cwd=directory).returncode:
                failed.append(f'{design}/{label}/{mode}')

if failed:
    raise SystemExit("Failed bitstream comparisons: " + ", ".join(failed))
