# rv_fast: an RV32I core for speed on the iCE40

[`examples/weft/rv_fast.weft`](../examples/weft/rv_fast.weft) is an RV32I
core written in Weft for the iCE40 HX8K, laid out stage by stage for its
LUT4s, carry chains and block RAMs. It runs at about 98 MHz on the Alchitry
Cu and retires 0.94 instructions per cycle on the benchmark of
[`examples/weft/bench`](../examples/weft/bench): 92 million instructions a
second, 2.4 times the throughput of the five-stage
[`rv_pipe`](../examples/weft/rv_pipe.weft) and 5.2 times that of PicoRV32
with its barrel shifter, in as many logic cells.

## Results

The benchmark (`bench.c`: CRC-32, a quicksort, a sieve, a linked list,
word hashing, recursive calls, a matrix product in software multiplication,
bit twiddling; built for rv32i by clang `-O2`) on the Alchitry Cu (HX8K
CB132), each core with the memories the benchmark needs:

| Core | Cycles | Logic cells | Block RAMs | Fmax | Time | |
|---|---:|---:|---:|---:|---:|---:|
| rv_fast | 499,719 | 2014 | 30 | 97.6 MHz | 5.12 ms | 1 |
| rv_pipe | 602,956 | 1842 | 28 | 49.1 MHz | 12.28 ms | 2.4× |
| PicoRV32, barrel shifter | 2,128,539 | 1971 | 28 | 80.1 MHz | 26.57 ms | 5.2× |
| PicoRV32 | 4,796,478 | 1692 | 28 | 77.2 MHz | 62.13 ms | 12.1× |

The cycles are those between the benchmark's marks (the LEDs at 1, then
2), counted in simulation, with the checksums it prints checked against
those of the same program built for the host. rv_fast and rv_pipe run
the same image, 469,461 instructions: rv_fast takes 1.064 cycles per
instruction, rv_pipe 1.284. Fmax is synth's, `--freq 200` (above what any
of them reaches), the median of seeds 1, 2 and 3 (each the best of synth's
four placements).

- **rv_fast**: [`bench/bench.weft`](../examples/weft/bench/bench.weft),
  an 8 KiB ROM and a 4 KiB RAM; 30 of the 32 block RAMs, with the register
  file (4), the branch target buffer (1) and the history table (1).
- **rv_pipe**: the same memories, made for the comparison by
  `tools/bench_riscv.sh`.
- **PicoRV32**: the PicoSoC of
  [`tests/designs/picosoc`](../tests/designs/picosoc) with 12 KiB of
  memory for code and data (its interface takes two or more cycles per
  access), with and without `BARREL_SHIFTER`, made the same way.

`tools/bench_riscv.sh` builds and runs all four, as in this table
(`SEEDS="1 2 3"`). On its own system with `rv_program.weft`
(`examples/weft/rv_fast.weft`, a 512-byte ROM and a 1 KiB RAM), rv_fast
takes 1749 logic cells (1433 LUTs, 608 flip-flops, 77 carries) and 10 block
RAMs, and runs at 98-100 MHz. Both systems run at 90 MHz on the Cu from its
PLL (`MAX_HZ`), which every placement seed tried meets with 5% to spare.

## How it is laid out

On the HX8K, worst case, a flip-flop's output comes 0.54 ns after the
clock, a block RAM's read data 2.15 ns; a LUT takes 0.32-0.45 ns, and the
shortest route between two logic cells 0.59 ns (a typical one 1 ns); a
carry chain 0.126 ns a bit, and 0.2 ns more into each tile of eight bits.
A 32-bit addition from flip-flops to flip-flops is about 6.5 ns, so a
stage of 10 ns has room for a carry chain and one or two LUTs, or for five
or six LUTs. rv_fast has six stages:

```
IF   the PC addresses the ROM; the BTB and BHT entries of the PC choose the next PC
ID   decode; the register file addressed; operand sources chosen; returns, unpredicted jumps
RF   the operands read and forwarded into EX's operand registers
EX   the ALU and the branch check; loads and stores address the RAM
MEM  loads aligned; stores write the RAM; devices; mispredictions redirect the fetch
WB   the register file written (two copies, one per read port)
```

**Forwarding a stage ahead.** In ID, each source register is compared
with the destinations of the four instructions ahead, and the result is
registered as a one-hot choice among: the sum, the shift, the SLT or the
other result of the instruction in EX; the value of the one in MEM, WB, or
of the one that wrote the register file in the previous cycle; the
register file; the immediate or the PC. RF's multiplexer is then a flat
AND-OR of registered selects, and the results of the instruction in EX
enter its last LUTs: an instruction can use the sum of the one just before
it with one LUT between the carry chain and its operand register. The
second operand is inverted there, with the carry in set, for subtractions,
comparisons and branches, so EX has one adder, `a + b + cin`, on one
carry chain.

**Results a cycle late.** Only loads give their result in MEM. An
instruction that uses it right after is fetched again rather than held in
ID (it waits two cycles instead of one): holding it would make the
decision, late in ID, reach the read enables of the ROM's sixteen block
RAMs. Shifts are two shifters of five LUT levels each, and an SLT's bit
comes a LUT after the adder's carry out, so both are forwarded too; at
first they were late like loads, and their stalls cost 15% of the cycles.

**Prediction in IF.** The branch target buffer (256 entries: a valid
bit, the rest of the PC and the target, in one block RAM) and the branch
history table (a two-bit counter for every instruction, 3 for JALs, in
another) are read with the *next* PC, so their entries for the PC being
fetched are there in the same cycle, and a predicted taken branch costs no
cycle (from the RAMs' data through the tag check and the multiplexer of
the next PC back to their address). A JAL or a
branch the table predicts taken whose target IF does not know yet (the
first time) is taken from ID, a cycle late; so are returns, from a return
address stack of four entries that calls push in ID. Branches are checked
in EX: the comparison is chosen in RF (equality, signed or not, whether
it holding means a misprediction), so the carry out goes through one LUT
into the register that redirects the fetch from MEM, four cycles after the
branch was fetched.

**Memory.** Loads and stores have an adder of their own, for the 12 bits
of address a 4 KiB RAM and the device decoding need: an address is a
device's when its bit 28 is set, and with a 12-bit offset the carry into
bit 28 needs only that adder's carry out and whether the base's bits 27:12
are all ones or zero. The RAM is read in EX and written from MEM, with
everything registered, so no late signal reaches its write enables; a load
right after a store to the same word takes the bytes the store writes,
folded into the one-hot selects of the load's byte lanes, which arrive
before the RAM's data.

**The PC** counts words and has only the bits the ROM needs.

## Where the cycles go

On the benchmark, 30,258 of the 499,719 cycles are not spent retiring an
instruction:

| | Cycles |
|---|---:|
| Mispredicted branches (5,718 of 70,312, 8.1%), 4 each | 22,872 |
| Jumps from ID (returns, targets not yet in the BTB), 1 each | 4,032 |
| Instructions fetched again after a load (1,751), 2 each | 3,502 |

The return address stack predicts 2,764 of the 2,794 returns. The
mispredicted branches are mostly the benchmark's data-dependent ones
(comparisons of random numbers, character classes).

The design went through these steps, each measured on the benchmark (CPI)
and on the Cu (Fmax of the system with `rv_program.weft`, one build with
the default seed, the best of four placements):

| Step | CPI | Fmax |
|---|---:|---:|
| First version: BHT in ID, shifts and SLT late | 1.417 | 78.6 MHz |
| One carry chain for `a + b + cin` (synth) | | 90.4 MHz |
| Branch check through one LUT; BRAM data late in mapping (synth) | | 89.2 MHz |
| Early device bit; forwarding split by result; two shifters | | 101.5 MHz |
| Shifts forwarded | 1.213 | 101.4 MHz |
| BTB read with the next PC | 1.100 | 96.5 MHz |
| BTB for targets, BHT for directions | 1.084 | 98.5 MHz |
| Address adder; store data through the second operand | 1.084 | 102.9 MHz |
| Replay instead of stall; SLT forwarded | 1.082 | 100.5 MHz |
| Stores from MEM; return address stack | 1.064 | 97.6 MHz |
| Late control nets off global buffers (synth) | 1.064 | 99.0 MHz |

## What it changed in synth

- **Carry-in after width reduction.** `(a + b) + c`, with `c` one bit, is
  one carry chain with carry-in `c`; width reduction left the inner sum
  narrower, zero-extended, and the two additions became two chains in
  series. They merge when the inner one cannot overflow.
- **Block RAM timing in LUT mapping.** The mapper takes a block RAM's read
  data as arriving 1.4 LUT levels after a flip-flop's output, and the
  routes into its inputs as 0.6 levels longer (`Timing::output_delay`).
- **Global networks for control nets.** An enable or reset decided by
  four or more levels of logic (three after a block RAM) takes a global
  network only for more than 128 users rather than 32: through the global
  buffer, at its fixed place on the die, it came 2.5-4 ns later than over
  the fabric. The estimate counts LUTs back to registers when packing.
  rv_pipe's critical path ended this way too.
- **`SYNTH_TIMING_ENDPOINTS=n`** prints the latest path into each of the n
  worst endpoints, where the report gives the worst path only.

On the iCE40 benchmark of `tools/qor.sh`, with rv_pipe and rv_fast's two
systems, against the revision before (`a87a7e9`): logic cells and Fmax,
the median of seeds 1-3 (each the best of four placements), `--freq 200`.

| Design | Cells before | after | Fmax before | after |
|---|---:|---:|---:|---:|
| PicoRV32 SoC | 1545 | 1544 | 84.9 MHz | 82.1 MHz (-3.3%) |
| Hazard3 SoC (Cu) | 2204 | 2227 | 49.5 MHz | 50.1 MHz (+1.2%) |
| SERV, Go Board | 581 | 579 | 115.1 MHz | 119.7 MHz (+4.0%) |
| rv_pipe | 1807 | 1835 | 49.4 MHz | 50.3 MHz (+1.8%) |
| rv_fast | 1755 | 1749 | 87.9 MHz | 100.2 MHz (+14.0%) |
| rv_fast, benchmark system | 2031 | 2014 | 87.1 MHz | 97.6 MHz (+12.1%) |
| Euler 10 | 7100 | 7100 | 70.2 MHz | 69.4 MHz (-1.1%) |
| Euler 14 | 6015 | 6015 | 73.3 MHz | 75.2 MHz (+2.6%) |
| Euler 25 | 1324 | 1324 | 75.7 MHz | 75.7 MHz |
| Euler 31 | 989 | 989 | 157.2 MHz | 157.2 MHz |
| Euler 43 | 5959 | 5959 | 55.3 MHz | 56.4 MHz (+2.0%) |
| Euler 48 | 6503 | 6503 | 76.6 MHz | 77.0 MHz (+0.5%) |
| Euler 57 | 2629 | 2629 | 66.0 MHz | 67.7 MHz (+2.6%) |
| Euler 58 | 7381 | 7381 | 69.4 MHz | 70.3 MHz (+1.3%) |
| Euler 92 | 3841 | 3841 | 93.1 MHz | 92.6 MHz (-0.5%) |
| Euler 97 | 1467 | 1467 | 91.6 MHz | 91.6 MHz |

Without rv_fast's systems, Fmax rises 0.8% (geometric mean); the
PicoRV32 SoC's median is 3% lower, within its spread over seeds (79-88
MHz before, 81-86 after). A calibration of the carry chains in the
mapper's delays (a tenth of the tile crossings per bit) was measured too,
and left out: it changed nothing consistently.
