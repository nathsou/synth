# IR text

The playground shows each step of synthesis as text in one of three small
languages, one per level of the flow:

| Language | Extension | Level | Steps |
|---|---|---|---|
| RTL | `.rtl` | Word-level cells (`synth_rtl::Module`) | Elaborate, Optimise, Memories |
| Gates | `.gates` | And-inverter graph with flip-flops, carry chains and instances (`synth_logic::LogicNet`) | Gates, Gate opt. |
| Netlist | `.netlist` | Primitives and nets (`synth_netlist::Netlist`) | LUTs, Pack, and the Artix-7 mapped netlist |

Each writes its IR as it is, cell by cell, under the IR's own names. The
playground writes these languages (`crates/synth-playground/src/text.rs`) and
highlights them (`web/highlight.js`); nothing reads them yet.

## Lexical layer (all three)

- **Comments**: `//` to the end of the line.
- **Names**: an identifier `[A-Za-z_$][A-Za-z0-9_$.]*` (IR names are
  hierarchical, `uart.$n20`), or any other name quoted: `"count[0]$lc"`, with
  `\"` and `\\` as escapes. A name spelled like a statement keyword is quoted.
- **Constants** are sized: `W'dV` (decimal), `W'hH` (hexadecimal), `W'bB`
  (binary, with `x` and `z`), where `W` is the width in bits. Unsized decimal
  integers are widths, depths, addresses, offsets and integer parameters.
- **Signals**: a name; a bit `x[3]` or slice `x[7:4]` of a multi-bit name; a
  constant; or a concatenation `{a, b[3:0], 2'd1}`, most significant part
  first. In argument positions, `~x` is a control or bit that is active low or
  complemented.
- **Statements** are lines. Each begins with a keyword (`input`, `wire`,
  `connect`, …), or defines a signal with `=`.
- **Cell labels**: `@name`, at the end of a statement, names the IR cell it
  writes (where the defined signal does not already).
- **Pins** of instances and cells: `P: x` for an input, `P -> x` for an
  output, `P <-> x` for an inout.

Every text is one `module name { … }`, ports first:

```
input clk: 1
output led: 4
inout sda: 1
```

## RTL (`.rtl`)

```
module top {
    input clk: 1
    output led: 4

    wire count: 26
    wire $n3: 26

    connect led = count[25:22]

    $n3 = add count, 26'd1 @$binary0
    count = dff $n3, clk: posedge clk, init: 26'd0 @count
}
```

| Statement | Meaning |
|---|---|
| `wire x: W [signed]` | A wire of `W` bits (only the wires the text uses are declared) |
| `connect a = b` | `a` is driven by `b` |
| `y = OP a [, b] @cell` | A unary or binary operation |
| `y = mux s ? b : a @cell` | `b` when `s` is 1, else `a` |
| `y = pmux s, default: d, cases: [c0, c1, …] @cell` | `ci` when bit `i` of `s` is set (`s` is one-hot), `d` when none is |
| `y = shiftx a, b [, offset: k] @cell` | `y` bits of `a` from bit `b + k` (`x` out of range) |
| `q = dff d, clk: posedge c [, en: e] [, srst: r -> V [, en_over_srst]] [, arst: r -> V] [, init: V] @cell` | A flip-flop vector. Priority: async reset, sync reset, enable; with `en_over_srst`, the sync reset acts only when enabled. `init` is the power-on value (left out when undefined) |
| `q = latch d, en: e @cell` | A level-sensitive latch |
| `inst name: module {P: v, …} (pins)` | An instance of a module or primitive |
| `memory m: D x W [offset k] { … }` | A memory of `D` words of `W` bits (addresses from `k`) |
| `init a: w0, w1, …` | (in a memory) Words from address `a` |
| `write addr, data, en: e, clk: posedge c` | (in a memory) A write port; `en` is one bit, or one bit per data bit |
| `data = read addr [, en: e], clk: posedge c` | (in a memory) A clocked read port |
| `data = read addr` | (in a memory) An asynchronous read port |

Operations, with `.s` for signed operands (`lt.s`, `shiftx.s` for a signed
shift amount): `not`, `neg`, `reduce_and`, `reduce_or`, `reduce_xor`,
`reduce_xnor`, `logic_not`; `and`, `or`, `xor`, `xnor`, `add`, `sub`, `mul`,
`div`, `mod`, `shl`, `shr`, `sshr` (arithmetic), `eq`, `ne`, `eqx`, `nex`
(comparing `x` and `z` bits too), `lt`, `le`, `gt`, `ge`, `logic_and`,
`logic_or`. Operands are extended to the result's width as `synth_rtl`
defines.

Controls (`en`, `srst`, `arst`, a latch's `en`) are active high, or active
low as `~x`. Clocks are `posedge c` or `negedge c`.

## Gates (`.gates`)

```
module top {
    input clk: 1
    output led: 4

    // 0 AND gates, 26 flip-flops, 1 carry chains, 0 instances (abridged)

    connect led[0] = count$ff22

    count$ff0 = dff ~count$ff0, clk: posedge clk, init: 1'd0
    count$ff1 = dff carry0.s[0], clk: posedge clk, init: 1'd0

    carry0 = carry {count$ff25, count$ff24, count$ff23}, 3'd0, cin: count$ff0
}
```

Every value is one bit; `~x` is its complement, `1'd0` and `1'd1` the
constants.

| Statement | Meaning |
|---|---|
| `$gN = and a, b` | AND gate `N` (node `N` of the graph), for each gate that feeds something, in topological order |
| `connect port[i] = x` | An output port bit |
| `name$ffN = dff d, clk: posedge c [, en: …] [, srst: r -> V [, en_over_srst]] [, arst: r -> V] [, init: V]` | Flip-flop `N` of the network, from the RTL cell `name` (as in `.rtl`, one bit) |
| `carryN = carry a, b, cin: c` | An adder on a carry chain: `carryN.s[i]` is bit `i` of `a + b + c`, `carryN.cout` the carry out |
| `inst name: module {P: v, …} (pins)` | An instance; its output bits are `name.P[i]` (`name.P` for one bit) |

Values the graph takes in are named after what makes them: port bits
(`btn[1]`), flip-flops (`count$ff3`), carry chains (`carry0.s[4]`) and instance
outputs (`pll.clk_out`).

## Netlist (`.netlist`)

```
module top {
    input clk: 1
    output led: 4

    // 76 cells

    connect $s0 = clk
    connect led[0] = $s23

    cell "count[0]": SB_DFF (C: $s0, D: $inv2, Q -> $s1)
    cell $not2: SB_LUT4 {LUT_INIT: 16'h3333} (I1: $s1, O -> $inv2)
}
```

| Statement | Meaning |
|---|---|
| `connect net = port[i]` | An input port bit drives a net |
| `connect port[i] = net` | A net drives an output port bit |
| `cell name: TYPE {P: v, …} (pins)` | A primitive, its pins on nets |

Nets are single bits named as in the netlist; the constant nets are `1'd0`,
`1'd1` and `1'bx`. A bus pin (`RDATA[0]` … `RDATA[15]`) is written once, as a
concatenation of nets, most significant first, with `_` for an unconnected
bit. Bit-vector parameters are hexadecimal (`16'h3333`; above 64 bits,
without leading zeros, `256'h0`), or binary when their width is not a
multiple of four (`2'b01`).

## Limits

A text above 1 MiB is cut at a line, with a comment saying so, and closed.
