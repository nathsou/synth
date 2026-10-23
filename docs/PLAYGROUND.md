# The synth playground

The playground runs the whole flow, from Weft or Verilog source to an iCE40 or
Artix-7 bitstream, in a web page, and shows every step. You can edit the design and
watch it change at each step: the word-level RTL after each pass, the gates,
the LUTs, the cells placed on the die, every routed wire, the timing and each
configuration bit.

## Running it

Natively, with the flow running in the `synth` process:

```sh
cargo build --release
target/release/synth playground            # http://127.0.0.1:8080/
target/release/synth playground --port 9000 --host 0.0.0.0
```

The Artix-7 boards need Project X-Ray's database (see `docs/ARTIX7.md`):

```sh
tools/fetch_xc7_db.sh /tmp/prjxray-db
target/release/synth playground --xc7-db /tmp/prjxray-db/artix7   # or SYNTH_XC7_DB
```

As a static site, with the flow compiled to WebAssembly and run in the browser
(nothing leaves the machine; any static file server works):

```sh
rustup target add wasm32-unknown-unknown
tools/build_playground.sh                  # writes target/playground/
python3 -m http.server -d target/playground
```

With `XC7_DB=/tmp/prjxray-db/artix7 tools/build_playground.sh`, the static
site also carries the database for the Artix-7 boards, one bundle of about
4 MB per part (`xc7db/<part>.tar.gz`), fetched on the first run for that part.
An Artix-7 run takes a few seconds to a few minutes in the browser and up to
about 1.5 GB of memory for the largest examples.

Every push to `main` publishes this static build to GitHub Pages
(`.github/workflows/pages.yml`; enable it once in the repository's settings,
Pages → Source: GitHub Actions).

The page uses the WebAssembly build when `synth_playground.wasm` is next to it,
and the `synth playground` server's API otherwise. It needs a browser with
WebGL2.

## What it shows

**Editor.** Files are tabs: Weft (`.weft`), Verilog (`.v`) and the pin
constraints (`.pcf` for iCE40, `.xdc` for Artix-7). As you type, the Weft sources are checked: errors and
lint warnings are underlined, hovering a name shows its type, value range,
width and clock domain, Ctrl+click (⌘+click) goes to its definition, uses of
the name under the cursor are highlighted, and `.` after an instance lists its
ports. Registers, ports, instances and domains are coloured by what they are,
not only by syntax. The format button (Shift+Alt+F) lays out a Weft file as
`synth fmt` does. The flow runs again shortly after you stop typing.

**Examples.** Blinky, a UART greeting and echo, breathing LEDs, Conway's Game
of Life over a UART, three RV32I cores running the same program (one
instruction per cycle, five cycles per instruction, and a five-stage pipeline
with forwarding), and ten Project Euler solvers (`tests/designs/euler`) that
send `<answer> <cycles>` over the UART and show the answer's low byte on the
LEDs. Each starts on a board's device, and its pins file is that board's.
Changing the device or package replaces it with the pins of a board that uses
it (Nandland Go Board, iCEstick, iCE40-HX8K Breakout Board, Alchitry Cu,
Alchitry Au, Digilent Nexys A7-100T), or with none, so that the placer chooses
them; it asks first if the file was edited. The pins file is `pins.pcf` on
iCE40 and `pins.xdc` on Artix-7; constraints for ports the design does not
have are ignored (and logged), so every board file lists all of its LEDs,
buttons and UART pins.

Every board has a clock frequency (25 MHz on the Go, 12 MHz on the iCEstick
and the HX8K Breakout, 100 MHz on the Cu, Au and Nexys A7), passed to the
design as its top's `CLK_HZ` parameter when it declares one: the examples
derive their baud-rate dividers and blink rates from it, so the same source
works on every board. It is also the timing target. The RISC-V cores and the
Euler solvers state, as `MAX_HZ`, the fastest clock they meet timing at on
every board (the Cu's iCE40 is the slowest), and run at the slower of the two
from the board's PLL (`synth_pll`, see `docs/WEFT.md` §8.7), held in reset
until it locks; the PLL's output frequency is then the timing target. Only
the small examples and Euler 31 fit the Go's HX1K, which has no PLL in its
package.

**Pipeline.** Each step of the flow with its statistics and how they changed
from the previous step: cells by kind after elaboration, after RTL
optimisation and after memory mapping; AND gates, flip-flops and carry chains
after lowering and after gate optimisation; iCE40 primitives after LUT
mapping; logic cells after packing; then placement, routing, timing and the
bitstream. On Artix-7, synthesis maps to 7-series primitives (LUT6, CARRY4,
FDRE, RAMB, DSP48E1, …) and the physical flow is one step, with the slices,
block RAMs, DSPs and IOs used; the Chip and Bitstream views are iCE40 only. Click a step to inspect it.

**Schematic.** The design at any of those steps as a graph. Instances start
collapsed into blocks with their boundary signals as ports (the module-level
view); double-click a block, or use **Cells**, to open it down to the
operators, gates, LUTs and flip-flops. The gate level shows the and-inverter
graph with its inversions as bubbles. Hover for properties, click to inspect a
node (with a truth table for LUTs) and follow its connections, search for a
cell or signal by name.

**Chip.** The die as it is: logic tiles with their eight logic cells, RAM
tiles, the IO ring with package pin names. Each used logic cell shows its LUT,
flip-flop and carry; zoom in to read the LUT contents. Every routed net is
drawn over the wires it uses (span-4, span-12, local tracks, cell inputs) and
the switches between them. Colour wires by net, by timing criticality or by
wire type; show the critical path of a clock; click a cell for its
configuration and pins, a net for its route, a tile for its bits.

**Timing.** For each clock: maximum frequency, the target if set, and the
critical path step by step with the delay of each step.

**Bitstream.** The configuration memory drawn bit for bit, tile by tile, in
IceStorm's `B<row>[<col>]` layout, coloured by purpose. Click a tile to decode
it: its logic cells (LUT contents, carry, flip-flop) and every routing switch,
read back from the bits rather than from the router. Download the `.bin`
(iCE40) or the `.bit` (Artix-7, for `openFPGALoader`).

**Simulation.** The design running on a virtual board, simulated cycle by
cycle with synth-sim (in the browser's own worker, or by the server): its
clock driven in real time (or as fast as the simulator goes), its outputs as
LEDs lit by their duty cycle (so a PWM dims them), its 1-bit inputs as
buttons to hold, and its serial port as a terminal: `tx` decoded at 115200
baud, and what you type sent on `rx`. The terminal understands enough VT100
for the examples (cursor home, clearing, line endings) and counts the frames
of a design that redraws in place; **Pixels** draws its text as cells, every
character other than a space or a dot lit, so the Game of Life shows as
frames of a grid. The design is elaborated with its `CLK_HZ` at 921.6 kHz,
eight clock cycles per bit at 115200 baud, a clock the simulator keeps up
with in real time for most examples (the RISC-V cores run at a fraction of
it): the examples derive their rates from `CLK_HZ`, so they behave as on the
board. `synth_pll` is simulated as a wire, locked from the start. The view
starts when opened and restarts when the design changes.

**Progress.** While the flow runs, the status bar shows a bar with a segment
per step (elaborating, synthesising, loading the device, packing, placing,
routing, timing, the bitstream), the step and what it is doing (the mapping
candidate, the annealing round and its acceptance rate, the routing
iteration and how many wires are still congested) and the time so far; a
line under the top bar shows the whole run. The placer and the router say
how far through they are (from how far annealing has cooled, and how far
congestion has fallen); a step that cannot tell is striped. Each segment's
width is its step's share of a run, learned from the runs in the browser.

**Problems and Log.** Diagnostics (click to jump to the source) and the log of
the run with the time each part took.

## Configuring it

The gear opens the settings, kept in the browser:

- appearance: dark or light theme, accent colour, code font size, whether the
  editor is on the left, on the right or hidden (the splitters between the
  panes can be dragged);
- flow: running after edits, checking as you type, placement seed, target
  frequency, timing weight and effort;
- schematic: the hierarchy depth that starts open, curved, orthogonal or
  straight wires, port and signal names, the node limit above which instances
  collapse, and the colours of each kind of node;
- chip: wire colouring, layers (routing, global networks, pins), dimming unused
  tiles, wire opacity, and the colours of tiles, cells and wire types;
- the keyboard shortcuts.

The share button copies a link that contains the design (compressed in the URL
fragment, so it is not sent to any server).

## How it works

`crates/synth-playground` holds the flow-to-data layer and the web app
(`web/`, plain JavaScript modules, no build step and no libraries).

- `handle(method, body)` answers the app's requests, JSON in and out: `run`
  (the whole flow), `analyze` (diagnostics and the name index, for the editor),
  `format`, `examples` and `devices` (with the boards and pins of each
  package).
- `run` elaborates (Weft or Verilog), then synthesises through
  `arch_ice40::synth::synthesize_observed`, which reports the design after each
  pass, then implements it through `arch_ice40::flow::implement_detailed`,
  which returns the placement, routing and timing next to the bitstream.
  Artix-7 parts go through `arch_xc7::synth::synthesize` and
  `arch_xc7::flow::implement` instead.
  `graph.rs` turns the RTL, the gate network and netlists into graphs;
  `chip.rs` describes the die, the placed cells and routed nets, and decodes
  the configuration bits.
- Weft elaboration names every cell after its instance path, so the schematic
  can rebuild the hierarchy of the flattened design.
- On WebAssembly (`wasm.rs`), the page writes a request into memory from
  `pg_alloc`, calls `pg_call(method, len, body, len)` and reads `pg_result_len`
  bytes at the returned pointer. After a panic, `pg_panic` gives the message.
  The module imports `env.synth_now_ms`, for timing, and
  `env.synth_fs_read` and `env.synth_fs_take` (`synth_core::fs`), through
  which the Artix-7 flow reads the database files from the worker, which
  unpacks the part's bundle before the run. The flow runs on one thread there.
- The flow reports its progress through `synth_core::progress`: the step it
  is in and, where it can tell, the fraction done, sent to a sink at most
  every 100 ms. `handle_observed` turns the reports into JSON; the server
  streams them (`/api/run?progress`: a line of JSON per report, then the
  result) and the WebAssembly module calls the import `env.synth_progress`,
  which the worker posts to the page. `web/progress.js` lays out the bar.
- `sim.rs` runs the simulation: `sim_start` elaborates the design and
  compiles it with synth-sim, `sim_step` sets the inputs, runs up to a number
  of cycles or milliseconds and returns the bytes the UART sent and each
  output's duty cycle, `sim_stop` ends it. `web/simview.js` paces the
  requests in real time and draws the board; `web/terminal.js` is the
  terminal.
- `server.rs` is the `synth playground` server: a few lines of HTTP over
  `std::net`, serving the embedded app and `/api/<method>`.
- The schematic, chip and bitstream views draw with WebGL2 (`gl.js`: instanced
  rounded rectangles and anti-aliased lines, a pan and zoom camera) with a
  Canvas2D layer for text. `layout.js` lays out graphs in layers: cycle
  breaking, longest-path layering, barycentre ordering that keeps instances
  together, and vertical placement by isotonic regression.
- Two workers run the WebAssembly build, one for runs and one for editor
  checks, so typing stays responsive during a run; only the latest request of
  each kind is kept.

## Working on it

`SYNTH_PLAYGROUND_DIR=crates/synth-playground/web synth playground` serves the
app from disk, so edits show on reload without rebuilding. Tests:
`cargo test -p synth-playground` (every example through the flow and on the
Go and Cu, the asset list; with `SYNTH_XC7_DB` set,
`cargo test --release -p synth-playground -- --ignored` builds every example
for the Au and the Nexys A7 too) and `node --test crates/synth-playground/web/tests/web.test.mjs` (layout
and highlighting, with Node's built-in runner).

Limits: Artix-7 shows the flow up to the mapped netlist, then the results of
place and route, without the chip and bitstream views. Gate-level graphs above
40,000 AND nodes are left out, and large schematics start collapsed.
