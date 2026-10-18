#!/usr/bin/env python3
"""Decides which CI work a change needs (development/CI only).

Reads the changed paths (one per line on stdin, or `--all`) and prints
GitHub Actions outputs:

  packages    cargo package flags for the crates to check and test: the
              changed crates and every crate that depends on them, or
              `--workspace`; empty when no Rust is affected
  frontend    the frontend simulations (elaborated and synthesised netlists
              against the RTL, Project Euler): Verilog frontend, RTL,
              synthesis, the CLI
  pnr         the bitstream simulations (Yosys or synth synthesis, synth
              place and route): the frontend crates too, plus RTLIL, routing
              and the iCE40 backend
  artix7      the Artix-7 primitive and physical encoding suites
  playground  the WebAssembly build and the web app's checks

Usage: git diff --name-only BASE...HEAD | tools/ci_changes.py
"""

import json
import subprocess
import sys

# Changes here may affect anything.
GLOBAL = ('Cargo.toml', 'Cargo.lock', '.github/workflows/ci.yml', 'tools/ci_changes.py')

# Crates and files each simulation suite exercises.
FRONTEND_CRATES = {'synth-core', 'synth-verilog', 'synth-elab', 'synth-rtl', 'synth-logic', 'synth-netlist', 'synth'}
FRONTEND_FILES = ('crates/arch-ice40/src/synth.rs', 'crates/arch-ice40/src/cells.rs', 'crates/arch-ice40/src/lib.rs',
                  'tools/ci_rtlsim.sh', 'tools/rtlsim.py', 'tools/ci_euler.sh', 'tests/designs/')
PNR_CRATES = FRONTEND_CRATES | {'synth-rtlil', 'synth-route', 'arch-ice40'}
PNR_FILES = ('tools/ci_bitsim.sh', 'tools/ci_incremental.sh', 'tools/bitsim.py', 'tests/designs/')

ARTIX7_CRATES = FRONTEND_CRATES | {'synth-rtlil', 'synth-route', 'arch-xc7'}
ARTIX7_FILES = ('tools/ci_artix7', 'tools/fetch_xc7', 'tools/prepare_xc7', 'tools/xc7-test-requirements.txt')

PLAYGROUND_FILES = ('editors/', 'tools/build_playground.sh')


def workspace():
    """Crate name → (directory, workspace crates it depends on)."""
    meta = json.loads(subprocess.check_output(['cargo', 'metadata', '--format-version', '1', '--no-deps']))
    root = meta['workspace_root'].rstrip('/') + '/'
    names = {p['name'] for p in meta['packages']}
    return {
        p['name']: (p['manifest_path'][len(root):].rsplit('/', 1)[0] + '/',
                    {d['name'] for d in p['dependencies'] if d['name'] in names})
        for p in meta['packages']
    }


def decide(paths, crates):
    everything = '--all' in paths or any(p in GLOBAL for p in paths)
    changed = set()
    for p in paths:
        for name, (d, _) in crates.items():
            if p.startswith(d):
                changed.add(name)
        # Tests that read these trees.
        if p.startswith('examples/'):
            changed |= {'synth-weft', 'synth-playground'}
        if p.startswith('tests/'):
            everything = True
    # Every crate that depends on a changed one, directly or not.
    affected = set(changed)
    grew = True
    while grew:
        grew = False
        for name, (_, deps) in crates.items():
            if name not in affected and deps & affected:
                affected.add(name)
                grew = True
    if everything:
        affected = set(crates)
    packages = '--workspace' if affected == set(crates) else ' '.join(f'-p {c}' for c in sorted(affected))
    frontend = everything or bool(changed & FRONTEND_CRATES) or any(p.startswith(FRONTEND_FILES) for p in paths)
    pnr = everything or bool(changed & PNR_CRATES) or any(p.startswith(PNR_FILES) for p in paths)
    playground = everything or 'synth-playground' in affected or any(p.startswith(PLAYGROUND_FILES) for p in paths)
    artix7 = everything or bool(changed & ARTIX7_CRATES) or any(p.startswith(ARTIX7_FILES) for p in paths)
    return {'artix7': artix7, 'packages': packages, 'frontend': frontend, 'pnr': pnr, 'playground': playground}


def main():
    paths = sys.argv[1:] if '--all' in sys.argv else [l.strip() for l in sys.stdin if l.strip()]
    out = decide(paths, workspace())
    for k, v in out.items():
        print(f'{k}={str(v).lower() if isinstance(v, bool) else v}')


if __name__ == '__main__':
    main()
