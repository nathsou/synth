#!/usr/bin/env python3
"""Independent Artix-7 primitive validation. Requires Yosys and Icarus.

XILINX_UNISIM must contain RAMB18E1.v from tools/fetch_xc7_models.sh.
SYNTH and YOSYS_DATDIR override the synth binary and Yosys model directory.
All artifacts are retained in the required --out directory.
"""
import argparse
import os
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
SYNTH = os.environ.get("SYNTH", str(ROOT / "target/release/synth"))
DATA = Path(os.environ.get("YOSYS_DATDIR", "/usr/share/yosys"))
SRC = ROOT / "tests/designs/artix7"


def run(args, log, cwd=ROOT):
    with log.open("w") as f:
        subprocess.run(list(map(str, args)), cwd=cwd, stdout=f, stderr=subprocess.STDOUT,
                       check=True, timeout=1800)


def synth(source, top, out, params=()):
    net = out.with_suffix(".v")
    args = [SYNTH, "synth", source, "--family", "artix7", "--top", top,
            "--rename", top + "_net", "--verilog", net]
    for p in params:
        args += ["-P", p]
    run(args, out.with_suffix(".synth.log"))
    return net


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--section", choices=["all", "primitives", "euler"], default="all")
    parser.add_argument("--long", action="store_true", help="also simulate the large Euler 10 design")
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    cells = DATA / "xilinx/cells_sim.v"
    unisim = Path(os.environ["XILINX_UNISIM"]) / "RAMB18E1.v"
    # Replace Yosys's black-box RAM declaration with the independent vendor model.
    models = out / "cells.v"
    models.write_text(re.sub(r"module RAMB18E1\s*\(.*?endmodule", "", cells.read_text(), flags=re.S))
    glbl = out / "glbl.v"
    glbl.write_text("`timescale 1ps/1ps\nmodule glbl; reg GSR = 0; initial begin #1 GSR=1; #100 GSR=0; end endmodule\n")

    if args.section in ["all", "primitives"]:
        # Resolve parameterized cell models BEFORE flattening the equivalence miter.
        for width in [1, 2, 3, 4, 5, 8, 9, 17, 33]:
            name = f"arithmetic-{width}"
            net = synth(SRC / "arithmetic.v", "arithmetic", out / name, [f"W={width}"])
            script = (f'read_verilog "{SRC / "arithmetic.v"}"; chparam -set W {width} arithmetic; '
                      f'read_verilog "{net}" "{cells}"; hierarchy -check; proc; '
                      'miter -equiv -flatten arithmetic arithmetic_net miter; '
                      'hierarchy -top miter; flatten; opt; sat -verify -prove trigger 0')
            run(["yosys", "-Q", "-T", "-p", script], out / f"{name}.formal.log")
            print(f"PASS formal {name}", flush=True)
        net = synth(SRC / "lut6.v", "lut6", out / "lut6")
        run(["yosys", "-Q", "-T", "-p",
             f'read_verilog "{SRC / "lut6.v"}" "{net}" "{cells}"; hierarchy -check; proc; '
             'miter -equiv -flatten lut6 lut6_net miter; hierarchy -top miter; flatten; opt; '
             'sat -verify -prove trigger 0'], out / "lut6.formal.log")
        print("PASS formal lut6", flush=True)

        net = synth(SRC / "controls.v", "controls", out / "controls")
        run(["iverilog", "-g2012", "-s", "tb", "-o", out / "controls.vvp",
             SRC / "controls_tb.v", SRC / "controls.v", net, cells], out / "controls.compile.log")
        run(["vvp", out / "controls.vvp"], out / "controls.sim.log")
        assert "PASS controls" in (out / "controls.sim.log").read_text()
        print("PASS controls", flush=True)

        # Independent DSP48E1 model: signed extrema, truncation and partial
        # products crossing both the 25-bit A and 18-bit B multiplier widths.
        for a,b,y in [(9,9,18),(24,17,48),(25,18,48),(25,25,64),(33,19,64),(64,64,64),(32,32,17)]:
            for signed in [0,1]:
                name=f"multiply-{a}-{b}-{y}-{signed}"
                net=synth(SRC / "multiplier.v", "multiplier", out / name,
                          [f"A={a}",f"B={b}",f"Y={y}",f"SIGNED={signed}"])
                assert "DSP48E1" in net.read_text(), name
                tb=out / f"{name}.tb.v"
                tb.write_text(f'''module tb;
                  reg [{a-1}:0] a=0; reg [{b-1}:0] b=0;
                  wire [{y-1}:0] refy,got;
                  multiplier #(.A({a}),.B({b}),.Y({y}),.SIGNED({signed})) refdut(a,b,refy);
                  multiplier_net dut(a,b,got);
                  integer i;
                  initial begin
                    for(i=0;i<2000;i=i+1) begin
                      case(i)
                        0: begin a=0; b=0; end
                        1: begin a=-1; b=-1; end
                        2: begin a=1; b=-1; end
                        3: begin a=1<<({a}-1); b=1<<({b}-1); end
                        default: begin a={{$random,$random}}; b={{$random,$random}}; end
                      endcase
                      #10; if(got!==refy) $fatal(1,"multiply case %d: %h * %h: %h != %h",i,a,b,got,refy);
                    end
                    $display("PASS multiplier"); $finish;
                  end
                endmodule''')
                run(["iverilog","-g2012","-s","tb","-o",out/f"{name}.vvp",tb,SRC/"multiplier.v",net,cells],out/f"{name}.compile.log")
                run(["vvp",out/f"{name}.vvp"],out/f"{name}.sim.log")
                assert "PASS multiplier" in (out/f"{name}.sim.log").read_text()
                print(f"PASS {name}",flush=True)

        # All BRAM modes, parity, replication, bank selection, offsets, masked
        # writes, falling edges, read enables, initialization and read/write collision.
        for w, d, offset, neg, mask in [(1, 1024, 0, 0, 0), (2, 512, 3, 1, 0),
                (4, 256, 0, 0, 0), (9, 128, 3, 1, 0), (18, 128, 0, 0, 0),
                (18, 1100, 5, 0, 0), (35, 128, 0, 1, 0), (4, 256, 0, 0, 1)]:
            name = f"memory-{w}-{d}-{offset}-{neg}-{mask}"
            params = [f"W={w}", f"D={d}", f"OFFSET={offset}", f"NEG={neg}", f"MASK={mask}"]
            net = synth(SRC / "memory.v", "memory", out / name, params)
            assert "RAMB18E1" in net.read_text(), name
            tb = out / f"{name}.tb.v"
            tb.write_text(f'''`timescale 1ns/1ps
    module tb;
      reg clk={neg}, re=1;
      reg [{w-1}:0] we=0, d=0;
      reg [15:0] ra={offset}, wa={offset};
      wire [{w-1}:0] refq, refq2, got, got2;
      memory #(.W({w}),.D({d}),.OFFSET({offset}),.NEG({neg}),.MASK({mask})) refdut(clk,re,we,ra,wa,d,refq,refq2);
      memory_net dut(clk,re,we,ra,wa,d,got,got2);
      integer i;
      reg q2valid=1;
      initial begin
        #1;
        if (got !== refq || got2 !== refq2) $fatal(1,"read-register initialization");
        for (i=0; i<3000; i=i+1) begin
          if (re) q2valid=(wa >= {offset} && wa < {offset+d});
          #5 clk=!clk;
          #1;
          if (got !== refq || (q2valid && got2 !== refq2)) $fatal(1,"cycle %d: %h %h != %h %h",i,got,got2,refq,refq2);
          #5 clk=!clk;
          ra={offset}+(i*41)%{d}; wa=(i%4 == 0) ? ra : {offset}+(i*17)%{d};
          if (i%11 == 0) wa=65535;
          re=(i%7 != 0); we=(i%3 == 0) ? (i*13+1) : 0;
          d=(i*937)^(i<<13);
        end
        $display("PASS memory"); $finish;
      end
    endmodule
    ''')
            run(["iverilog", "-g2012", "-s", "tb", "-s", "glbl", "-o", out / f"{name}.vvp",
                 tb, SRC / "memory.v", net, models, unisim, glbl], out / f"{name}.compile.log")
            run(["vvp", out / f"{name}.vvp"], out / f"{name}.sim.log")
            result = (out / f"{name}.sim.log").read_text()
            assert "PASS memory" in result and "Error" not in result, name
            print(f"PASS {name}", flush=True)

        net = synth(SRC / "address_memory.v", "address_memory", out / "address_memory")
        assert "RAMB18E1" in net.read_text()
        run(["iverilog", "-g2012", "-s", "tb", "-s", "glbl", "-o", out / "address_memory.vvp",
             SRC / "address_memory_tb.v", SRC / "address_memory.v", net, models, unisim, glbl],
            out / "address_memory.compile.log")
        run(["vvp", out / "address_memory.vvp"], out / "address_memory.sim.log")
        assert "PASS address memory" in (out / "address_memory.sim.log").read_text()
        print("PASS address memory initialization and transparency", flush=True)

    if args.section in ["all", "euler"]:
        # Real designs: independent known answer AND exact cycle count.
        euler = ROOT / "tests/designs/euler"
        for line in (euler / "designs.txt").read_text().splitlines():
            if not line or line.startswith("#"):
                continue
            n, width, answer, cycles, *extra = line.split()
            if int(cycles) > 200000 or (n == "10" and not args.long):
                continue
            name = f"euler{n}"
            net = out / f"{name}.v"
            run([SYNTH, "synth", euler / f"p0{n}.v", *[euler / p for p in extra],
                 "--family", "artix7", "--top", name, "--verilog", net], out / f"{name}.synth.log")
            run(["iverilog", "-g2012", "-s", "tb", "-s", "glbl", f"-DSOLVER={name}",
                 f"-DANSWER_WIDTH={width}", f"-DEXPECT=64'd{answer}", f"-DMAX_CYCLES={int(cycles)+10}",
                 "-o", out / f"{name}.vvp", euler / "tb.v", net, models, unisim, glbl],
                out / f"{name}.compile.log")
            run(["vvp", out / f"{name}.vvp"], out / f"{name}.sim.log")
            assert f"PASS answer {answer} after {cycles} cycles" in (out / f"{name}.sim.log").read_text(), name
            print(f"PASS {name} answer and cycle count", flush=True)

    if args.section in ["all", "primitives"]:
        # Determinism, target defaults and invalid target diagnostics.
        repeat = out / "repeat.v"
        run([SYNTH, "synth", SRC / "controls.v", "--family", "xc7", "--top", "controls",
             "--rename", "controls_net", "--verilog", repeat], out / "repeat.log")
        assert repeat.read_bytes() == (out / "controls.v").read_bytes()
        run([SYNTH, "synth", SRC / "controls.v", "--top", "controls", "--verilog", out / "ice40.v"], out / "ice40.log")
        assert "SB_DFF" in (out / "ice40.v").read_text()
        bad = subprocess.run([SYNTH, "synth", SRC / "controls.v", "--family", "typo"], capture_output=True)
        assert bad.returncode != 0 and b"unknown synthesis family" in bad.stderr
        print("PASS CLI and determinism", flush=True)


if __name__ == "__main__":
    main()
