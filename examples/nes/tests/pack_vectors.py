#!/usr/bin/env python3
# SPDX-License-Identifier: CERN-OHL-W-2.0
"""Convert local SingleStepTests nes6502/v1 JSON files to a compact bus test.

Obtain vectors from https://github.com/SingleStepTests/65x02 at revision
2f6980a2d95757486c7bee24355c360e40e2a224. No vectors are distributed here.
KIL and electrically unstable operations are excluded from stable CPU checks.
"""
import argparse
import json
from pathlib import Path
import struct

UNSTABLE = {0x8B, 0x93, 0x9B, 0x9C, 0x9E, 0x9F, 0xAB}
KIL = {0x02, 0x12, 0x22, 0x32, 0x42, 0x52, 0x62, 0x72, 0x92, 0xB2, 0xD2, 0xF2}


def registers(state):
    return struct.pack("<HBBBBB", state["pc"], state["a"], state["x"], state["y"], state["p"], state["s"])


def ram(state):
    entries = state["ram"]
    return struct.pack("<H", len(entries)) + b"".join(struct.pack("<HB", addr, data) for addr, data in entries)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--per-op", type=int, default=64)
    args = parser.parse_args()
    if args.per_op <= 0:
        parser.error("per-op must be positive")
    records = []
    opcodes = 0
    for opcode in range(256):
        if opcode in UNSTABLE or opcode in KIL:
            continue
        path = args.directory / f"{opcode:02x}.json"
        if not path.is_file():
            parser.error(f"missing opcode file: {path}")
        vectors = json.loads(path.read_text())[:args.per_op]
        if not vectors:
            parser.error(f"empty opcode file: {path}")
        opcodes += 1
        for index, vector in enumerate(vectors):
            cycles = vector["cycles"]
            record = struct.pack("<BI", opcode, index) + registers(vector["initial"]) + ram(vector["initial"])
            record += struct.pack("<B", len(cycles))
            record += b"".join(struct.pack("<HBB", a, d, int(kind == "write")) for a, d, kind in cycles)
            record += registers(vector["final"]) + ram(vector["final"])
            records.append(record)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_bytes(b"NST1" + struct.pack("<I", len(records)) + b"".join(records))
    print(f"Packed {len(records)} vectors across {opcodes} stable opcodes")


if __name__ == "__main__":
    main()
