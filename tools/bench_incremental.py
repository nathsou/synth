#!/usr/bin/env python3
"""Paired fresh/incremental benchmarks; retain logs, RTL, ASC and JSON in --out."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--out', type=Path, required=True)
p.add_argument('--seeds', nargs='+', type=int, default=[1, 2, 3])
p.add_argument('--designs', nargs='+', choices=['blinky_ram', 'picosoc', 'euler31', 'euler48'],
               default=['blinky_ram', 'picosoc', 'euler31', 'euler48'])
p.add_argument('--timeout', type=int, default=180)
a = p.parse_args()
out = a.out.resolve()
if out.exists() and any(out.iterdir()):
    raise SystemExit('Output directory must be empty; previous results are retained')
out.mkdir(parents=True, exist_ok=True)
binary = ROOT / 'target/release/synth'
results = []
meta = {'commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
        'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(), 'seeds': a.seeds}
(out / 'metadata.json').write_text(json.dumps(meta, indent=2))

for design in a.designs:
    euler = design.startswith('euler')
    src = ROOT / 'tests/designs' / ('euler' if euler else design)
    work = out / design
    if work.exists():
        raise SystemExit(f'{work} exists; choose a new output directory')
    shutil.copytree(src, work)
    if euler:
        n = int(design[5:])
        rtl = work / f'p{n:03d}.v'
        files = ['euler_top.v', 'report.v', rtl.name]
        flags = ['--top', 'euler_top', '-D', f'SOLVER=euler{n}', '-D', f'ANSWER_WIDTH={17 if n == 31 else 34}']
        device, package, pcf, freq = 'hx8k', 'cb132', 'alchitry_cu.pcf', (60 if n == 31 else 30)
        edits = [('small', "8'd200", "8'd199"), ('large', 'STAGES = 8', 'STAGES = 4')] if n == 31 else [('small', "10'd1000", "10'd999"), ('large', 'LANES = 6', 'LANES = 3')]
    elif design == 'picosoc':
        rtl = work / 'soc.v'
        files, flags = ['soc.v', 'picorv32.v'], ['--top', 'top']
        device, package, pcf, freq = 'hx8k', 'cb132', 'alchitry_cu.pcf', 50
        edits = [('small', 'tx_div == 7', 'tx_div == 6'), ('large', '.BARREL_SHIFTER(0)', '.BARREL_SHIFTER(1)'), ('multiply', '.ENABLE_MUL(0)', '.ENABLE_MUL(1)')]
    else:
        rtl = work / 'blinky_ram.v'
        files, flags = [rtl.name], ['--top', 'top']
        device, package, pcf, freq = 'hx1k', 'vq100', 'go_board.pcf', 50
        edits = [('small', 'i * 7 + 3', 'i * 7 + 5'), ('large', 'assign led = q', 'assign led = (q + cnt[7:0])')]
    original = rtl.read_text()
    variants = [('unchanged', original)]
    for label, old, new in edits:
        assert original.count(old) == 1, (design, old)
        variants.append((label, original.replace(old, new)))
    (work / 'variants').mkdir()
    for label, source in variants:
        (work / 'variants' / f'{label}.v').write_text(source)
    for seed in a.seeds:
        base = work / f'baseline-{seed}.chk'
        def run(label, mode, checkpoint=None):
            prefix = work / f'{seed}-{label}-{mode}'
            cmd = [str(binary), 'build', *files, *flags, '--device', device, '--package', package, '--pcf', pcf, '--freq', str(freq), '--seed', str(seed), '--asc', str(prefix.with_suffix('.asc'))]
            if checkpoint:
                cmd += ['--incremental', str(checkpoint)]
            start = time.perf_counter()
            try:
                proc = subprocess.run(cmd, cwd=work, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=a.timeout)
                log, code = proc.stdout.decode(errors='replace'), proc.returncode
            except subprocess.TimeoutExpired as exc:
                log, code = (exc.stdout or b'').decode(errors='replace'), 'timeout'
            elapsed = time.perf_counter() - start
            prefix.with_suffix('.log').write_text(log)
            clocks = re.findall(r'clock (\S+): [\d.]+ ns, ([\d.]+) MHz', log)
            packed = re.findall(r'packed in .*?: (.*)', log)
            row = dict(design=design, seed=seed, edit=label, mode=mode, seconds=elapsed, exit=code, target=freq,
                       clocks=dict((k, float(v)) for k, v in clocks), resources=packed[-1] if packed else None,
                       cells=re.findall(r'incremental placement: reused (\d+)/(\d+)', log),
                       routes=re.findall(r'incremental routing: reused (\d+)/(\d+)', log),
                       fallback='using fresh PnR' in log, command=cmd)
            results.append(row)
            (out / 'results.json').write_text(json.dumps(results, indent=2))
            print(f'{design} seed={seed} {label}/{mode}: {elapsed:.3f}s exit={code} clocks={row["clocks"]} fallback={row["fallback"]}', flush=True)
        rtl.write_text(original)
        run('baseline', 'checkpoint', base)
        if not base.exists():
            print('No valid baseline checkpoint; skipping paired runs', flush=True)
            continue
        for label, source in variants:
            rtl.write_text(source)
            checkpoint = work / f'{seed}-{label}.chk'
            shutil.copyfile(base, checkpoint)
            # Alternate order across seeds to reduce systematic warm-cache bias.
            order = ['fresh', 'incremental'] if seed % 2 else ['incremental', 'fresh']
            for mode in order:
                run(label, mode, checkpoint if mode == 'incremental' else None)
    rtl.write_text(original)

if any(row['exit'] != 0 for row in results):
    raise SystemExit('Some builds failed or timed out; inspect results.json and logs')
