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

From the repository root, install Rust, a C++17 compiler, make and Verilator, then:

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
`--ppm-every N` additionally writes `frame-N.ppm`, `frame-2N.ppm`, … next to the
`--ppm` path, which is useful for checking motion and scrolling.
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
OAM decay are outside the digital hardware boundary. The PPU I/O bus uses a
deterministic 600 ms charge-decay model; physical decay varies with temperature
and chip revision.

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
protocol and fails on a nonzero result, a KIL opcode or a timeout. Reset requests
(`$81`) wait at least 100 ms, then apply reset while preserving CPU/cartridge
RAM. The runner does not modify the test's result memory.

Older sprite, VBL/NMI and branch timing tests use `--legacy-result f8`: the
runner waits for the terminal self-jump before judging the result byte. The
2005 APU and PPU tests use `--console-code`: at the terminal self-jump the
runner reads the `$hh` code they print, where `$01` means every test passed.
ASCII-console tests use `--console-test` with the default `Passed`/`Failed`
text, `--console-pass`/`--console-fail` strings, or explicit `--console-crc`
values from the test's documented output. These modes are selected explicitly
by `tests/run.py`; an ordinary frame or timeout is never counted as a pass.

`tests/run.py` covers 121 ROMs: `instr_test-v5`, `instr_misc`, `instr_timing`,
`cpu_timing_test6` (official and undocumented opcodes), `branch_timing_tests`,
`cpu_interrupts_v2`, `cpu_reset`, `cpu_dummy_writes`, `cpu_exec_space`,
`ppu_vbl_nmi`, `vbl_nmi_timing`, `ppu_open_bus`, `ppu_read_buffer`, the 2005
PPU tests, `oam_read`, `oam_stress`, the sprite hit and overflow tests,
`apu_test`, `apu_reset`, the 2005 APU tests, `dmc_dma_during_read4`,
`sprdma_and_dmc_dma`, and `mmc3_test`/`mmc3_test_2` tests 1–5. The MMC6 and
MMC3A variants of test 6 are expected to fail because the core implements
MMC3B. Palette RAM starts with the contents blargg recorded on his console
(`power_up_palette`); real power-up contents differ between chips.

For independent per-cycle CPU checks, obtain the `nes6502/v1` JSON vectors from
[SingleStepTests/65x02](https://github.com/SingleStepTests/65x02) at revision
`2f6980a2d95757486c7bee24355c360e40e2a224`, then run:

```sh
python3 examples/nes/tests/pack_vectors.py /path/to/nes6502/v1 work/nes-vectors.bin --per-op 128
make -C examples/nes vectors-test
```

This checks 30,336 instructions across 237 stable opcodes, including every bus
address, read/write direction, data byte and final state. State initialization
runs real CPU instructions; the harness does not replace CPU state or execute
the instruction in software. The nonphysical B flag is normalized for register
comparisons; stack writes are checked byte for byte. KIL and electrically
unstable opcodes are excluded from these stable-vector checks.

The dedicated NES CI workflow runs the pinned public diagnostics when a PR is
ready for review or when manually dispatched, following the repository's policy
for expensive simulations. Uploaded commercial ROMs are not included in CI.

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
