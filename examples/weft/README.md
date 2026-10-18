# Weft examples

Designs in [Weft](../../docs/WEFT.md), synth's own HDL, for the Nandland Go
Board (iCE40 HX1K, 25 MHz). Build `synth` first (`cargo build --release` at
the repository root) and run the commands from this directory.

| Design | What it shows |
|---|---|
| [`blinky.weft`](blinky.weft) | A counter on the LEDs: one clock port, so no domain annotations |
| [`hello.weft`](hello.weft) + [`uart.weft`](uart.weft) | Prints "Hello, Weft!" once a second: enums, a ROM constant, `index<N>` counters, driving instance inputs from the parent |
| [`echo.weft`](echo.weft) + [`uart.weft`](uart.weft) | Echoes the USB UART through a 64-byte FIFO in block RAM: an asynchronous input, a button reset through `reset_sync`, generic modules |
| [`blinky_ram.weft`](blinky_ram.weft) | A port of [`tests/designs/blinky_ram`](../../tests/designs/blinky_ram): memory initialized by a function, registered reads |
| [`serv.weft`](serv.weft) | A port of [`go_board_serv`](../go_board_serv): the SERV RISC-V core, in Verilog, instantiated through an `extern module` |

```sh
synth build blinky.weft --device hx1k --package vq100 --pcf go_board.pcf -o blinky.bin
synth build hello.weft uart.weft --device hx1k --package vq100 --pcf go_board.pcf -o hello.bin
synth build echo.weft uart.weft --device hx1k --package vq100 --pcf go_board.pcf -o echo.bin
synth build blinky_ram.weft --device hx1k --package vq100 --pcf ../../tests/designs/blinky_ram/go_board.pcf -o blinky_ram.bin
```

For `serv.weft`, see the command at the top of the file.

`cargo test -p synth-weft --test examples` simulates these designs: it decodes
the UART output of `hello` and `echo`, and runs `blinky_ram` and `serv`
cycle by cycle against their Verilog originals.

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
