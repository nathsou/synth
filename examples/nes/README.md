# NES in Weft

An NTSC NES digital hardware implementation, compiled from Weft to Verilog and
simulated with Verilator. This is a draft under active accuracy testing; a game
booting is not evidence of full hardware accuracy.

The hardware includes the 2A03 CPU and all 256 opcode decodes, RP2C02 background
and sprite pipelines, CPU/PPU memories, scrolling and palette registers, two
controllers, OAM DMA, DMC DMA and five APU channels. Cartridge address translation
supports NROM, MMC1B, UxROM, CNROM, MMC3B and AxROM. The C++ runner supplies only
cartridge storage, input scripts, RGB conversion and audio mixing/output.

## Build and run

From the repository root, install Rust, a C++ compiler, make and Verilator, then:

```sh
cargo build --release -p synth
make -C examples/nes
work/nes/obj/Vnes /path/to/game.nes --frames 600 --ppm work/frame.ppm --wav work/audio.wav
```

The runner has no display or audio device dependency. PPM output is 256×240 RGB;
WAV output is 48 kHz mono PCM. `--input path` reads lines containing a decimal
frame number and hexadecimal controller bytes. Bits 0–7 are A, B, Select,
Start, Up, Down, Left and Right. Values persist until the next event. For example:

```text
120 08 00
122 00 00
300 80 00
```

This presses Start on controller 1 for two frames, then holds Right from frame
300 onward. `--save path` loads and writes an explicit 8 KiB battery save for a
battery-backed cartridge. `--trace path` writes every CPU bus cycle to CSV.
Invalid/truncated headers, unsupported mappers/regions, NES 2.0 and unsupported
memory geometries fail explicitly. ROMs are never modified or distributed here.

Override `SYNTH`, `VERILATOR`, `BUILD`, `JOBS` and `TEST_ROMS` on the make command
line when using other tool locations. Generated files live under `work/`.

## Hardware boundary

`nes` takes a PPU dot clock (5.369318 MHz) and synchronous active-high reset. Its
CPU and APU advance once every three dot clocks. All boundary inputs must be
synchronous to `clk`; synchronize physical controller and reset inputs outside
the core. The cartridge configuration must stay constant until reset.

`prg_address` selects bytes in an external PRG ROM. `chr_address` selects bytes
in external CHR ROM/RAM; accept `chr_write` only for CHR RAM. Cartridge RAM uses
`ram_address`, `ram_write` and `ram_write_data`. Reads are combinational; writes
are accepted on the next rising edge. Masks are the number of 8 KiB PRG or 1 KiB
CHR banks minus one. The runner validates power-of-two sizes. Internal memory
currently uses combinational reads; FPGA area/timing and a board port have not
been validated.

`pixel_valid`, `pixel_x`, `pixel_y`, `pixel` and `emphasis` describe each visible
dot. `frame` pulses at vblank entry. Audio outputs are the raw channel DAC
values; `audio_valid` marks a CPU cycle. The runner applies the nonlinear NES
mixer and an approximate analog filter. Composite decoding, analog color and
analog decay are outside the digital hardware boundary.

## Verification

Use the public diagnostic ROM collection at
[christopherpow/nes-test-roms](https://github.com/christopherpow/nes-test-roms),
revision `95d8f621ae55cee0d09b91519a8989ae0e64753b`, in `work/nes-test-roms`:

```sh
git clone https://github.com/christopherpow/nes-test-roms.git work/nes-test-roms
git -C work/nes-test-roms checkout 95d8f621ae55cee0d09b91519a8989ae0e64753b
make -C examples/nes test
work/nes/obj/Vnes work/nes-test-roms/ppu_vbl_nmi/rom_singles/01-vbl_basics.nes --test
```

The CPU trace test requires all 8,991 `nestest` instructions to match PC, A, X,
Y, P, SP and cycle count. Diagnostic ROM mode recognizes the `$6000` test
protocol and fails on a nonzero result, a KIL opcode or a timeout. Frame output
alone is a smoke check, not a diagnostic pass. Timing/compatibility results and
remaining failures will be recorded after the full validation run.

## References

The implementation follows NESdev's documentation of the
[CPU](https://www.nesdev.org/wiki/CPU),
[instruction set](https://www.nesdev.org/wiki/Instruction_reference),
[PPU rendering](https://www.nesdev.org/wiki/PPU_rendering),
[registers](https://www.nesdev.org/wiki/PPU_registers),
[sprite evaluation](https://www.nesdev.org/wiki/PPU_sprite_evaluation),
[APU](https://www.nesdev.org/wiki/APU),
[DMA](https://www.nesdev.org/wiki/DMA),
[MMC1](https://www.nesdev.org/wiki/MMC1) and
[MMC3](https://www.nesdev.org/wiki/MMC3).

NTSC home NES only. PAL/Dendy, VS/PlayChoice, FDS, expansion audio, peripheral
controllers and cartridge board variants beyond those described above are not
implemented. Unstable undocumented opcodes and analog effects require particular
care when judging hardware accuracy.
