#!/usr/bin/env python3
"""Native Artix-7 flow vs independent X-Ray frame/packet oracles.

Python, FASM and X-Ray are TEST dependencies only. The synth executable never
invokes them. This checks configuration encoding, not hardware functionality
or timing closure. See docs/ARTIX7.md for the remaining acceptance criteria.
"""
import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import random
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
PARTS = ["xc7a35tftg256-1", "xc7a100tcsg324-1"]
DB_REV = "0a0addedd73e7e4139d52a6d8db4258763e0f1f3"
XRAY_REV = "c9f02d8576042325425824647ab5555b1bc77833"


def run(args, log, expected=0):
    with log.open("w") as f:
        p = subprocess.run([str(a) for a in args], cwd=ROOT, stdout=f,
                           stderr=subprocess.STDOUT, timeout=300)
    if p.returncode != expected:
        raise RuntimeError(f"{args[0]} exited {p.returncode}, expected {expected}; see {log}")


def frames(path):
    result = {}
    for line in path.read_text().splitlines():
        a, data = line.split()
        words = [int(w, 16) for w in data.split(",")]
        assert len(words) == 101
        words[50] &= ~0x1fff  # ECC is checked separately in packet comparison.
        result[int(a, 16)] = words
    return result


def oracle(a, part, fasm, bit, prefix):
    from prjxray.db import Database
    from prjxray.fasm_assembler import FasmAssembler
    asm = FasmAssembler(Database(str(a.db), part))
    asm.parse_fasm_filename(str(fasm))
    ref = asm.get_frames()
    native = prefix.with_suffix(".native.frm")
    run([a.synth, "unpack-xc7", bit, "--db", a.db, "--device", part, "-o", native],
        prefix.with_suffix(".decode.log"))
    actual = frames(native)
    assert set(ref) <= set(actual), "reference contains unexpected frames"
    for addr, data in actual.items():
        assert data == ref.get(addr, [0] * 101), f"frame mismatch at {addr:08x}"
    # Independently encode all native frame addresses, including blank frames.
    frm = prefix.with_suffix(".reference.frm")
    frm.write_text("".join(f"0x{k:08x} " + ",".join(f"0x{w:08x}" for w in v) + "\n"
                           for k, v in actual.items()))
    refbit = prefix.with_suffix(".reference.bit")
    run([a.xray_tools / "xc7frames2bit", "--part_file", a.db / part / "part.yaml",
         "--part_name", part, "--frm_file", frm, "--output_file", refbit],
        prefix.with_suffix(".encode.log"))
    # Compare C++ decoder output for native CRC-enabled and reference streams,
    # including frame ECC. This is independent of synth's own packet decoder.
    bits = []
    for tag, source in [("native", bit), ("reference", refbit)]:
        output = prefix.with_suffix(f".{tag}.bits")
        run([a.xray_tools / "bitread", "--part_file", a.db / part / "part.yaml",
             "-C", "-y", "-o", output, source], prefix.with_suffix(f".{tag}.bitread.log"))
        bits.append(output.read_bytes())
    assert bits[0] == bits[1], "independent packet/ECC readback differs"
    return len(actual)


def constraints(a, part, ports, clocks=(), single_io=False):
    with (a.db / part / "package_pins.csv").open() as f:
        pins = list(csv.DictReader(f))
    if single_io:
        pins = [p for p in pins if "_SING_" in p["tile"]]
    chosen = {}
    for clock in clocks:
        pin = next(p for p in pins if "MRCC" in p["pin_function"] and p["pin"] not in chosen.values())
        chosen[clock] = pin["pin"]
    for port in ports:
        if port not in chosen:
            chosen[port] = next(p["pin"] for p in pins if p["pin"] not in chosen.values())
    return "".join(f"set_property -dict {{ PACKAGE_PIN {pin} IOSTANDARD LVCMOS33 }} [get_ports {{{p}}}]\n"
                   for p, pin in chosen.items()) + "".join(
                       f"create_clock -period 10 [get_ports {{{p}}}]\n" for p in clocks)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--db", type=Path, required=True, help="prjxray-db/artix7")
    p.add_argument("--xray", type=Path, required=True, help="Project X-Ray checkout")
    p.add_argument("--xray-tools", type=Path, required=True, help="C++ build/tools directory")
    p.add_argument("--synth", type=Path, default=Path(os.environ.get("SYNTH", ROOT / "target/release/synth")))
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--part", choices=PARTS, action="append")
    p.add_argument("--euler", action="store_true", help="also implement the Euler 31 solver")
    p.add_argument("--full-luts", action="store_true", help="randomize every LUT in the fabric")
    a = p.parse_args()
    for key in ["db", "xray", "xray_tools", "synth", "out"]:
        setattr(a, key, getattr(a, key).resolve())
    a.out.mkdir(parents=True, exist_ok=True)
    sys.path.insert(0, str(a.xray))
    versions = {}
    for name, path, want in [("database", a.db.parent, DB_REV), ("xray", a.xray, XRAY_REV)]:
        rev = subprocess.check_output(["git", "-C", str(path), "rev-parse", "HEAD"], text=True).strip()
        if rev != want:
            raise RuntimeError(f"{name} revision {rev}, expected {want}")
        dirty = subprocess.check_output(["git", "-C", str(path), "status", "--porcelain", "--untracked-files=no"], text=True)
        if dirty.strip():
            raise RuntimeError(f"{name} checkout has tracked modifications")
        versions[name] = rev
    for name in ["xc7frames2bit", "bitread"]:
        versions[name + "_sha256"] = hashlib.sha256((a.xray_tools / name).read_bytes()).hexdigest()
    versions["synth_sha256"] = hashlib.sha256(a.synth.read_bytes()).hexdigest()
    report = {"versions": versions, "checks": [], "hardware_validated": False,
              "timing_validated": False}
    cases = [
        ("invert", "module top(input a,output y); assign y=~a; endmodule", ["a", "y"], []),
        ("single_io", "module top(input a,b,output y,z); assign y=~a; assign z=~b; endmodule", ["a","y","b","z"], []),
        ("counter", "module top(input clk,output y); reg [16:0] q=17'h12345; always @(posedge clk) q<=q+1; assign y=q[16]; endmodule", ["clk", "y"], ["clk"]),
        ("controls", "module top(input clk,rst,ce,d,output reg [3:0] q=4'h6); always @(posedge clk) if(rst) q[0]<=0; else if(ce) q[0]<=d; always @(negedge clk) if(rst) q[1]<=1; else if(ce) q[1]<=d; always @(posedge clk or posedge rst) if(rst) q[2]<=0; else q[2]<=d; always @(negedge clk or posedge rst) if(rst) q[3]<=1; else q[3]<=d; endmodule", ["clk","rst","ce","d"]+[f"q[{i}]" for i in range(4)], ["clk"]),
        ("ram", "module top(input clk,we,input [8:0] addr,input [7:0] din,output reg [7:0] q=8'h93); reg [7:0] mem[0:511]; integer i; initial for(i=0;i<512;i=i+1) mem[i]=i^8'hA5; always @(posedge clk) begin if(we) mem[addr]<=din; q<=mem[addr]; end endmodule", ["clk","we"]+[f"{p}[{i}]" for p,n in [("addr",9),("din",8),("q",8)] for i in range(n)], ["clk"]),
    ]
    if a.euler:
        source = (ROOT / "tests/designs/euler/p031.v").read_text().replace("module euler31", "module top")
        cases.append(("euler31",source,["clk","rst","start","done"]+[f"answer[{i}]" for i in range(17)],["clk"]))
    for part in a.part or PARTS:
        out = a.out / part
        out.mkdir(exist_ok=True)
        for name, source, ports, clocks in cases:
            start = time.monotonic()
            prefix = out / name
            v, xdc, fasm, bit = [prefix.with_suffix(s) for s in [".v", ".xdc", ".fasm", ".bit"]]
            v.write_text(source + "\n")
            xdc.write_text(constraints(a, part, ports, clocks, name == "single_io"))
            cmd = [a.synth,"build",v,"--family","artix7","--top","top","--db",a.db,
                   "--device",part,"--xdc",xdc,"--fasm",fasm,"-o",bit]
            run(cmd + ["--timing-allow-fail"], prefix.with_suffix(".log"))
            count = oracle(a, part, fasm, bit, prefix)
            if name == "invert":
                invalid = prefix.with_suffix(".invalid.xdc")
                invalid.write_text(xdc.read_text().replace("PACKAGE_PIN ", "PACKAGE_PIN INVALID_", 1))
                bad = cmd.copy()
                bad[bad.index("--xdc") + 1] = invalid
                run(bad + ["--timing-allow-fail"], prefix.with_suffix(".invalid-pin.log"), expected=1)
                assert "not a user IO" in prefix.with_suffix(".invalid-pin.log").read_text()
                corrupt = prefix.with_suffix(".corrupt.bit")
                data = bytearray(bit.read_bytes())
                pos = data.index(bytes.fromhex("30018001"))
                data[pos+7] ^= 1
                corrupt.write_bytes(data)
                run([a.synth,"unpack-xc7",corrupt,"--db",a.db,"--device",part,"-o",prefix.with_suffix(".corrupt.frm")],prefix.with_suffix(".corrupt.log"),expected=1)
                assert "IDCODE" in prefix.with_suffix(".corrupt.log").read_text()
            if name == "counter":
                original = bit.read_bytes()
                run(cmd + ["--timing-allow-fail"], prefix.with_suffix(".repeat.log"))
                assert bit.read_bytes() == original, "same-seed build is not deterministic"
                run(cmd, prefix.with_suffix(".timing-gate.log"), expected=1)
                assert "timing acceptance failed" in prefix.with_suffix(".timing-gate.log").read_text()
            report["checks"].append(dict(part=part,design=name,frames=count,
                sha256=hashlib.sha256(bit.read_bytes()).hexdigest(),
                source_sha256=hashlib.sha256(v.read_bytes()).hexdigest(),
                constraints_sha256=hashlib.sha256(xdc.read_bytes()).hexdigest(),seconds=time.monotonic()-start))
            print(f"PASS {part} {name}: {count} frames", flush=True)
        if a.full_luts:
            grid = json.loads((a.db / ("xc7a50t" if "35t" in part else "xc7a100t") / "tilegrid.json").read_text())
            rand = random.Random(7)
            prefix = out / "all-luts"
            fasm, bit = prefix.with_suffix(".fasm"), prefix.with_suffix(".bit")
            with fasm.open("w") as f:
                for tile,t in sorted(grid.items()):
                    if not t["type"].startswith("CLB"): continue
                    for x in range(2):
                        kind = "SLICEM" if "CLBLM" in t["type"] and x == 0 else "SLICEL"
                        for lane in "ABCD":
                            f.write(f"{tile}.{kind}_X{x}.{lane}LUT.INIT[63:0] = 64'h{rand.getrandbits(64):016x}\n")
            run([a.synth,"pack-fasm",fasm,"--db",a.db,"--device",part,"-o",bit],prefix.with_suffix(".log"))
            count = oracle(a,part,fasm,bit,prefix)
            report["checks"].append(dict(part=part,design="all-luts",frames=count))
            print(f"PASS {part} all LUTs", flush=True)
    (a.out / "results.json").write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
