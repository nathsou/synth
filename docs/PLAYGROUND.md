# The synth playground

The playground runs the whole flow, from Weft or Verilog source to an iCE40
bitstream, in a web page, and shows every step. You can edit the design and
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

As a static site, with the flow compiled to WebAssembly and run in the browser
(nothing leaves the machine; any static file server works):

```sh
rustup target add wasm32-unknown-unknown
tools/build_playground.sh                  # writes target/playground/
python3 -m http.server -d target/playground
```

Every push to `main` publishes this static build to GitHub Pages
(`.github/workflows/pages.yml`; enable it once in the repository's settings,
Pages → Source: GitHub Actions).

The page uses the WebAssembly build when `synth_playground.wasm` is next to it,
and the `synth playground` server's API otherwise. It needs a browser with
WebGL2.

## What it shows

**Editor.** Files are tabs: Weft (`.weft`), Verilog (`.v`) and the pin
constraints (`.pcf`). As you type, the Weft sources are checked: errors and
lint warnings are underlined, hovering a name shows its type, value range,
width and clock domain, Ctrl+click (⌘+click) goes to its definition, uses of
the name under the cursor are highlighted, and `.` after an instance lists its
ports. Registers, ports, instances and domains are coloured by what they are,
not only by syntax. The flow runs again shortly after you stop typing.

**Pipeline.** Each step of the flow with its statistics and how they changed
from the previous step: cells by kind after elaboration, after RTL
optimisation and after memory mapping; AND gates, flip-flops and carry chains
after lowering and after gate optimisation; iCE40 primitives after LUT
mapping; logic cells after packing; then placement, routing, timing and the
bitstream. Click a step to inspect it.

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
read back from the bits rather than from the router. Download the `.bin`.

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
  `examples` and `devices`.
- `run` elaborates (Weft or Verilog), then synthesises through
  `arch_ice40::synth::synthesize_observed`, which reports the design after each
  pass, then implements it through `arch_ice40::flow::implement_detailed`,
  which returns the placement, routing and timing next to the bitstream.
  `graph.rs` turns the RTL, the gate network and netlists into graphs;
  `chip.rs` describes the die, the placed cells and routed nets, and decodes
  the configuration bits.
- Weft elaboration names every cell after its instance path, so the schematic
  can rebuild the hierarchy of the flattened design.
- On WebAssembly (`wasm.rs`), the page writes a request into memory from
  `pg_alloc`, calls `pg_call(method, len, body, len)` and reads `pg_result_len`
  bytes at the returned pointer. After a panic, `pg_panic` gives the message.
  The module imports one function, `env.synth_now_ms`, for timing.
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
`cargo test -p synth-playground` (every example through the flow, the asset
list) and `node --test crates/synth-playground/web/tests/web.test.mjs` (layout
and highlighting, with Node's built-in runner).

Limits: iCE40 only, like the rest of the backend. Gate-level graphs above
40,000 AND nodes are left out, and large schematics start collapsed.
