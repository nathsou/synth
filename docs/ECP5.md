# Native ECP5 implementation status

synth has a native Rust path from Verilog (or Weft) through synthesis,
packing, placement, routing, static timing analysis and a bitstream for the
Lattice ECP5 (LFE5U, LFE5UM, LFE5UM5G; 12K to 85K), on Project Trellis'
device database. It is **experimental**: its bitstreams are written byte for
byte as Trellis' `ecppack` writes them, and every design below is checked by
simulating the netlist decoded from its bitstream against the source RTL,
but none has run on hardware yet (there is no ECP5 board in the project). The
flow has been exercised mostly on the LFE5U-25F in the CABGA381 package.

```sh
# Debian/Ubuntu: apt install fpga-trellis-database (or --db <prjtrellis-db>)
target/release/synth build design.v --top top --family ecp5 \
  --device LFE5U-25F --package CABGA381 --lpf pins.lpf -o design.bit
```

On the [QoR comparison](#qor) below, against Yosys + nextpnr-ecp5 on the same
sources and device, synth uses fewer LUTs on every design and reaches a
higher Fmax on all but one, by its own timing analysis (which agrees with
nextpnr's on nextpnr's own routed designs, see [Validation](#validation)).

## The database

The database is read at run time, as the Artix-7 flow reads prjxray-db: from
`--db`, else `SYNTH_TRELLIS_DB`, else the packaged
`/usr/share/trellis/database` (Debian/Ubuntu's `fpga-trellis-database`). Only
its JSON files are used (devices, tile grids, package pins and banks, global
clock geometry, each tile type's configuration bits and arcs, and the
timing of each speed grade); none of Trellis' code runs.

The routing graph follows Trellis' wire naming and per-tile arcs, with the
clock network's wire links (quadrant globals to spines, spines to taps, taps
to branches) and the LUT input permutations added. On the LFE5U-25F it has
exactly pytrellis' 8,199,469 arcs, and it builds in 1.0 s (4.0 s for the
85K).

## Synthesis

`synth synth --family ecp5` runs the iCE40 pipeline (a LUT4 architecture with
a carry chain) on ECP5 primitives:

- **Logic**: `LUT4`s and `TRELLIS_FF`s. An ECP5 flip-flop's set/reset
  overrides its enable and sets both its reset and its power-up value
  (`REGSET`): a reset meant to act only when enabled is gated with the
  enable, and a synchronous reset to a value other than the power-up value
  moves into the data path.
- **Carry chains**: `CCU2C`s, two bits each, operand inversions and
  constants folded into the tables, as nextpnr builds chains (a first half
  generating the carry in, a last half passing a carry out to the logic).
- **Block RAM**: memories of 256 bits or more become `DP16KD`s (1024 × 18):
  in the 18- or 9-bit mode, whichever takes fewer blocks, then fewer banks;
  lanes of up to 9 bits sharing a write enable, byte enables in the 18-bit
  mode, banks muxed by a registered selector.
- **Multipliers**: products whose operands are worth it (both of 4 bits or
  more, together 18 or more, neither a constant of one or two set bits)
  become unregistered `MULT18X18D`s, wider operands split in 18-bit chunks
  (only the high chunk of a signed operand signed), the partial products
  summed in logic.
- **PLLs**: `synth_pll<IN_HZ, OUT_HZ>` becomes an `EHXPLLL` in internal
  feedback (`INT_OP`) with the dividers Trellis' `ecppll` finds (reference
  8–400 MHz, phase detector 3.125–400 MHz, VCO 400–800 MHz, within 0.2% of
  the frequency asked for), and its output phase and loop filter settings:
  the PLL's tiles are configured bit for bit as nextpnr configures `ecppll`'s
  PLL. Equal frequencies need no PLL.

Wide functions (`PFUMX`, `L6MUX21`), distributed RAM (`TRELLIS_DPR16X4`)
and I/O registers are not used: an experiment with wide functions shortened
logic depth by 0–10% on the Euler designs, and is deferred.

## Place and route

- **Packing**: a logic tile has four slices of two logic cells (a LUT and a
  flip-flop). A flip-flop takes its data from its own cell's LUT or from the
  slice's `M` input; a slice's flip-flops share a clock enable, a tile's a
  clock and a set/reset (the tile's second clock and set/reset are not
  used, as with nextpnr). A `CCU2C` fills a slice and a chain consecutive
  slices of a row. Constants fold into LUT tables, or come from the routing
  tiles' ties (`CIB.<wire>MUX`) for block RAM, multiplier and PLL inputs;
  other constant readers get a constant LUT.
- **Placement**: simulated annealing of logic cells, whole carry chains and
  hard blocks (block RAMs, multipliers, PLLs on sites of their kind), with
  VPR's cost (bounding boxes weighted for net size, incrementally updated),
  timing-driven as VPR (each connection's estimated delay weighted by its
  criticality to the eighth power, criticalities following the placement).
  `--tries` (default 4) places and routes that many seeds on threads of
  their own and keeps the best timed.
- **Clocks**: each clock (most clock pins first, up to 16) enters the global
  network through the clock buffer (`DCCA`) nearest its source by delay,
  then reaches its pins over one global's quadrant wires, spine, taps and
  branches, as nextpnr's global router does.
- **Routing**: negotiated congestion (`synth-route`), critical connections
  first, with rounds of rerouting the critical ones once routing is legal.
  A LUT's inputs are routed to its logical pins, any physical input
  reaching each; its table is permuted to the inputs the routes take.
- **Configuration**: as nextpnr-ecp5's `bitstream.cc` writes it, from the
  device's empty-design base: switches, slices, I/O (types, drive, pull,
  bank voltages), block RAM, multiplier and PLL settings and the ties.
  Bidirectional pads are not supported yet.
- **Bitstream**: Trellis' frames and command stream (CRC-16, optional
  compression, block RAM initialisation), byte for byte as `ecppack` writes
  them; `synth pack-ecp5` and `synth unpack-ecp5` stand in for `ecppack`
  and `ecpunpack`.

Constraints come from the LPF, in the subset nextpnr-ecp5 reads: `LOCATE`,
`IOBUF` (I/O type, drive, pull mode, slew rate, …), `FREQUENCY PORT|NET` and
`SYSCONFIG`. Ports without a `LOCATE` go on free pins, with a warning.

## Timing

The static timing analysis uses Trellis' cell arcs and switch delays per
speed grade, as nextpnr-ecp5 times a design: register-to-register paths per
clock against its target (`FREQUENCY` in the LPF, `--freq` for the others,
a PLL's output at its frequency), each routed switch's delay plus an adder
per branch its net takes from the wire. Carries and a flip-flop's data from
its own cell's LUT are dedicated connections. Clocks are ideal (no skew),
and I/O paths are not timed. Timing is reported, not enforced.

`synth unpack-ecp5 <bit> --timing` times any ECP5 bitstream, nextpnr's
included, by decoding it.

## Validation

- **Bitstreams**: `cargo test -p arch-ecp5 --test trellis` checks that
  bitstreams are byte for byte those of `ecppack` (compressed or not) and
  read back as `ecpunpack` reads them, on nextpnr-ecp5 designs across five
  devices, and that the routing graph has pytrellis' arcs.
- **Decoding**: a configuration decodes back to the primitives it
  configures (LUT4, CCU2C, TRELLIS_FF, DP16KD in every port width and the
  pseudo dual port mode, unregistered MULT18X18D, EHXPLLL), which simulate
  with models of the Lattice library. The decoder is checked on Yosys +
  nextpnr bitstreams (logic, carries, block RAMs of every width, ROMs,
  multipliers, PLLs, PicoRV32 SoC).
- **Simulation**: `tools/ci_ecp5.sh` (in CI) checks synthesised netlists
  (PicoRV32 SoC, SERV, DarkRISCV, Hazard3) and bitstreams (those but the bare
  Hazard3 core, which has more ports than the package has pins, plus a block
  RAM blinky, the Alchitry Cu Hazard3 SoC, and a solver behind a PLL with its
  UART reporter) against their RTL under random inputs, and the Project
  Euler solvers' answers and cycle counts from their bitstreams.
- **Timing**: on nextpnr's own routed designs, synth's analysis agrees with
  nextpnr's Fmax:

| Design | synth | nextpnr |
|---|---:|---:|
| PicoRV32 SoC | 89.8 MHz | 90.4 MHz |
| Euler 25 | 63.3 MHz | 66.5 MHz |
| Euler 43 | 51.7 MHz | 52.9 MHz |
| Euler 48 | 69.7 MHz | 74.2 MHz |
| Euler 57 | 61.9 MHz | 63.1 MHz |
| Euler 58 | 60.9 MHz | 61.4 MHz |
| Euler 97 (multipliers) | 57.6 MHz | 57.9 MHz |

  within 0.5–6.1%, synth's on the pessimistic side.

## QoR

Both flows run on the same sources for the LFE5U-25F (CABGA381, speed 6),
with their defaults: `synth build --family ecp5` (four placement seeds on
four cores, the best timed kept) and Yosys's `synth_ecp5` followed by
nextpnr-ecp5 (seed 1). Each Fmax is its own tool's analysis (the two agree
on the same routed design, see above). LUTs count the LUT4 sites used (a
`CCU2C` takes two); times are wall clock, Yosys included.

| Design | synth LUTs | FFs | MULT18 | Fmax | time | Y+N LUTs | FFs | MULT18 | Fmax | time |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| PicoRV32 SoC | 1324 | 556 | 0 | 112.1 MHz | 8 s | 2093 | 517 | 0 | 86.6 MHz | 10 s |
| Euler 10 | 6570 | 1859 | 0 | 38.3 MHz | 24 s | 137,099 | 1643 | 0 | does not fit | 441 s |
| Euler 14 | 5214 | 2214 | 0 | 81.7 MHz | 16 s | 7401 | 2233 | 0 | 76.2 MHz | 58 s |
| Euler 25 | 978 | 187 | 0 | 73.9 MHz | 6 s | 1678 | 187 | 0 | 64.6 MHz | 7 s |
| Euler 31 | 315 | 402 | 0 | 144.7 MHz | 4 s | 744 | 668 | 0 | 118.3 MHz | 4 s |
| Euler 43 | 5738 | 1497 | 0 | 52.8 MHz | 18 s | 6641 | 1517 | 0 | 54.9 MHz | 124 s |
| Euler 48 | 5267 | 2412 | 13 | 94.8 MHz | 13 s | 6429 | 2438 | 6 | 80.6 MHz | 45 s |
| Euler 57 | 1808 | 857 | 0 | 68.5 MHz | 8 s | 2961 | 1114 | 0 | 67.3 MHz | 15 s |
| Euler 58 | 5831 | 2158 | 0 | 79.8 MHz | 21 s | 7853 | 2030 | 5 | 59.4 MHz | 50 s |
| Euler 92 | 1183 | 2806 | 0 | 81.4 MHz | 10 s | 1662 | 2867 | 1 | 68.8 MHz | 15 s |
| Euler 97 | 968 | 445 | 3 | 61.6 MHz | 5 s | 989 | 445 | 2 | 58.3 MHz | 5 s |

synth uses fewer LUTs on every design (2–58% fewer; Yosys maps Euler 10 to
137,099, five times the device) and reaches a higher Fmax on all but Euler 43
(4% lower), by 2% (Euler 57) to 34% (Euler 58). PicoRV32 SoC and Euler 58
take 6–8% more flip-flops. The whole flow is faster than Yosys + nextpnr on
nine designs (up to 18 times, on Euler 10) and within 0.5 s of it on the
other two, small ones: about 4 s of every build goes to loading the device
(1 s) and building the router's lookahead (searching the whole device from
sample sinks; once per build, for the first seed's placement, and shared by
the seeds).

## Remaining work

- Hardware validation on an ECP5 board.
- Wide functions (`PFUMX`, `L6MUX21`), distributed RAM, registered and
  cascaded DSPs, I/O registers and DDR, bidirectional pads.
- The PLL's secondary outputs, and placing a PLL by the dedicated clock pin
  feeding it (the annealer places it today, by wirelength).
- Run time on small designs: the router's lookahead could be built once
  per device rather than per build.
