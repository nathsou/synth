# Board examples

RISC-V systems built from Verilog to bitstream by `synth` alone (no Yosys or
nextpnr involved). The prebuilt firmware images are committed, so building
the bitstreams needs only `synth`; rebuilding the firmware needs a RISC-V
GCC (`riscv64-unknown-elf-gcc`).

Build `synth` first, from the repository root:

```sh
cargo build --release
export PATH=$PWD/target/release:$PATH
```

Designs written in Weft, synth's own HDL, are in [`weft/`](weft/).

Each example's bitstream is also checked in CI by simulating the decoded
bitstream against the RTL (`tools/ci_bitsim.sh`).

## Alchitry Cu: Hazard3 (5-stage pipelined RV32I)

[Hazard3](https://github.com/Wren6991/Hazard3) with 8 KiB of block RAM, a
UART and the LEDs. The core runs at 12.5 MHz (the 100 MHz board clock
divided by 8). The firmware prints a line with a counter over the USB
serial port (115200 8N1) about twice a second and shows the counter on the
LEDs. The reset button restarts it.

```sh
cd examples/alchitry_cu_hazard3
H=../../tests/designs/third_party/hazard3/hdl
synth build top.v $(sed -n "s|^file |$H/|p" $H/hazard3.f) -I $H --top top \
  --device hx8k --package cb132 --pcf alchitry_cu.pcf -o hazard3.bin
```

About 2400 of 7680 logic cells and 20 of 32 block RAMs.

## Nandland Go Board: SERV (bit-serial RV32I)

[SERV](https://github.com/olofk/serv) in its `servant` SoC with 4 KiB of
RAM, at the board's 25 MHz. The firmware bit-bangs a line over the USB
serial port once a second, at 19200 8N1. LED1 lights while characters are
sent and LED4 blinks.

```sh
cd examples/go_board_serv
S=../../tests/designs/third_party/serv
synth build top.v $S/servant/{servant,servant_mux,servant_ram,servant_timer,servant_gpio}.v \
  $S/servile/*.v $S/rtl/*.v --top top \
  --device hx1k --package vq100 --pcf go_board.pcf -o serv.bin
```

About 590 of 1280 logic cells and 9 of 16 block RAMs.

## Programming

Both boards program the SPI flash through their FTDI chip. With
[openFPGALoader](https://github.com/trabucayre/openFPGALoader):

```sh
openFPGALoader -b ice40_generic -f hazard3.bin   # or serv.bin
```

If your openFPGALoader build lists a dedicated entry for the board
(`openFPGALoader --list-boards`), `-b` with that name works too; IceStorm's
`iceprog serv.bin` also works for both boards.

Then open the serial port (the second FTDI interface; on Linux usually
`/dev/ttyUSB1`, on macOS `/dev/tty.usbserial-*1`):

```sh
screen /dev/ttyUSB1 115200    # Alchitry Cu
screen /dev/ttyUSB1 19200     # Go Board
```

## Rebuilding the firmware

- Alchitry Cu: `make -C examples/alchitry_cu_hazard3/fw` writes `fw.hex`.
- Go Board:

  ```sh
  cd examples/go_board_serv
  riscv64-unknown-elf-gcc -nostartfiles -nostdlib -march=rv32i -mabi=ilp32 \
    -T link.ld -o hello.elf hello.S
  riscv64-unknown-elf-objcopy -O binary hello.elf hello.bin
  python3 makehex.py hello.bin > hello.hex
  ```

  The UART bit time is set by `BIT_NOPS` in `hello.S` (SERV takes about 36
  clock cycles per extra instruction in the bit loop); 30 gives 1287 cycles
  per bit, 19425 baud at 25 MHz.
