#!/usr/bin/env python3
"""Merges tools/vivado_artix7.py runs into docs/ARTIX7_VIVADO.json and a table.

Each flow may come from its own run (the Vivado flows change rarely; synth's
is rerun with each change): --vivado, --netlist and --synth name results.json
files, or one file holds several flows. Prints the Markdown table for
docs/ARTIX7.md.
"""
import argparse
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--vivado", type=Path, required=True)
    p.add_argument("--netlist", type=Path, required=True)
    p.add_argument("--synth", type=Path, required=True)
    p.add_argument("--out", type=Path, default=ROOT / "docs/ARTIX7_VIVADO.json")
    a = p.parse_args()
    runs = {k: json.loads(getattr(a, k).read_text()) for k in ("vivado", "netlist", "synth")}
    report = {"part": runs["synth"]["part"], "period_ns": runs["synth"]["period_ns"],
              "versions": {k: r["versions"] for k, r in runs.items()}, "designs": []}
    rows = []
    for d in runs["synth"]["designs"]:
        top = d["design"]
        row = {"design": top}
        for k, r in runs.items():
            src = next((x for x in r["designs"] if x["design"] == top), {})
            flow = dict(src.get(k, {}))
            flow.pop("runs", None)
            row[k] = flow
        report["designs"].append(row)
        v, n, s = row["vivado"], row["netlist"], row["synth"]
        mhz = lambda f, key="fmax_mhz": f.get(key) if f.get(key) is not None else "-"
        rows.append(f'| {top.replace("euler", "Euler ")} | {v.get("slices", "-")} / {n.get("slices", "-")} / {s.get("slices", "-")} '
                    f'| {v.get("lut", "-")} / {n.get("lut", "-")} | {v.get("ffs", "-")} / {n.get("ffs", "-")} / {s.get("ffs", "-")} '
                    f'| {mhz(v)} / {mhz(n)} / {mhz(s, "vivado_mhz")} | {mhz(s, "estimate_mhz")} |')
    a.out.write_text(json.dumps(report, indent=2) + "\n")
    print("| Design | Slices: Vivado / netlist / synth | LUTs: Vivado / netlist | FFs | MHz (Vivado's timer): Vivado / netlist / synth | synth's estimate |")
    print("|---|---:|---:|---:|---:|---:|")
    print("\n".join(rows))


if __name__ == "__main__":
    main()
