# Weft examples

Designs in [Weft](../../docs/WEFT.md), synth's own HDL, for the Nandland Go
Board (iCE40 HX1K, 25 MHz) and, for the RISC-V cores, the Lattice
iCE40-HX8K Breakout Board (12 MHz). Build `synth` first (`cargo build
--release` at the repository root) and run the commands from this directory.
All of them are also in the [playground](../../docs/PLAYGROUND.md).

| Design | What it shows |
|---|---|
| [`blinky.weft`](blinky.weft) | A counter on the LEDs: one clock port, so no domain annotations |
| [`hello.weft`](hello.weft) + [`uart.weft`](uart.weft) | Prints "Hello, Weft!" once a second: enums, a ROM constant, `index<N>` counters, driving instance inputs from the parent |
| [`echo.weft`](echo.weft) + [`uart.weft`](uart.weft) | Echoes the USB UART through a 64-byte FIFO in block RAM: an asynchronous input, a button reset through `reset_sync`, generic modules |
| [`blinky_ram.weft`](blinky_ram.weft) | A port of [`tests/designs/blinky_ram`](../../tests/designs/blinky_ram): memory initialized by a function, registered reads |
| [`serv.weft`](serv.weft) | A port of [`go_board_serv`](../go_board_serv): the SERV RISC-V core, in Verilog, instantiated through an `extern module` |
| [`breathe.weft`](breathe.weft) | Four LEDs breathing out of phase: PWM through a gamma table computed at compile time, a `for` over the LEDs |
| [`life.weft`](life.weft) + [`uart.weft`](uart.weft) | Conway's Game of Life on an 8×8 torus, drawn in a serial terminal: arrays of arrays, a struct-returning full adder, a whole generation computed by one function |
| [`rv_single.weft`](rv_single.weft) | An RV32I core that runs one instruction per cycle; the ROM and register file in block RAM, read for the next instruction |
| [`rv_multi.weft`](rv_multi.weft) | An RV32I core that takes five cycles per instruction, one per step: a `match` on an enum state (it just fits the Go Board's HX1K) |
| [`rv_pipe.weft`](rv_pipe.weft) | An RV32I core in five pipeline stages, with forwarding, a load-use stall and branches resolved in EX |

The three cores share [`rv32i.weft`](rv32i.weft) (decoder, ALU, load and
store lanes, the memory-mapped LEDs and UART, and an assembler written as
functions) and run [`rv_program.weft`](rv_program.weft), a program assembled
at compile time. It prints a few results, the Fibonacci numbers up to 610 and
their sum over the UART, one hexadecimal number per line, and shows each
Fibonacci number on the LEDs.

```sh
synth build blinky.weft --device hx1k --package vq100 --pcf go_board.pcf -o blinky.bin
synth build hello.weft uart.weft --device hx1k --package vq100 --pcf go_board.pcf -o hello.bin
synth build echo.weft uart.weft --device hx1k --package vq100 --pcf go_board.pcf -o echo.bin
synth build blinky_ram.weft --device hx1k --package vq100 --pcf ../../tests/designs/blinky_ram/go_board.pcf -o blinky_ram.bin
synth build breathe.weft --device hx1k --package vq100 --pcf go_board.pcf -o breathe.bin
synth build life.weft uart.weft --device hx1k --package vq100 --pcf go_board.pcf -o life.bin
synth build rv_pipe.weft rv32i.weft rv_program.weft uart.weft --device hx8k --package ct256 --pcf hx8k_breakout.pcf -o rv_pipe.bin
```

`rv_single.weft` and `rv_multi.weft` build like `rv_pipe.weft`. On the Go
Board, `rv_multi.weft` needs `-P CLKS_PER_BIT=217 --top top` (for 25 MHz).

For `serv.weft`, see the command at the top of the file.

`cargo test -p synth-weft --test examples` simulates these designs: it decodes
the UART output of `hello`, `echo`, `life` and the RISC-V cores, measures the
brightness of `breathe`, and runs `blinky_ram` and `serv` cycle by cycle
against their Verilog originals.

To see the diagnostics, break something. For example, use the button in
`blinky_ram.weft` without `@clk`:

```text
error: an asynchronous value reaches register `cnt` (in domain `clk`)
  --> blinky_ram.weft:15:23
   |
15 |     cnt := cnt +% (if btn { 2 } else { 1 });
   |                       ^^^ `btn` (an asynchronous top-level input)
  = help: synchronize a single bit first: `sync(btn)`
  = help: or, if the board drives it synchronously to the clock, declare it `in btn: ... @clk`
note: sampling an asynchronous signal directly can make flip-flops metastable
note: it also reaches memory `ram` (in domain `clk`)
```
