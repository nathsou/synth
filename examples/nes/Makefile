# SPDX-License-Identifier: CERN-OHL-W-2.0
SYNTH ?= ../../target/release/synth
VERILATOR ?= verilator
BUILD ?= ../../work/nes
JOBS ?= 4
TEST_ROMS ?= ../../work/nes-test-roms
VECTORS ?= ../../work/nes-vectors.bin
BUILD := $(abspath $(BUILD))
SOURCES := nes.weft cpu.weft decode.weft ppu.weft apu.weft mapper.weft

.PHONY: all sim cpu-test vectors-test test fpga clean
all: sim
sim: $(BUILD)/obj/Vnes

$(BUILD)/nes.v: $(SOURCES) $(SYNTH)
	mkdir -p $(BUILD)
	$(SYNTH) elab $(SOURCES) --top nes --verilog $@

$(BUILD)/obj/Vnes: $(BUILD)/nes.v sim/main.cpp
	$(VERILATOR) --cc --exe --build -j $(JOBS) -CFLAGS "-std=c++17" -Wno-fatal --top-module nes \
	    --Mdir $(BUILD)/obj $< $(abspath sim/main.cpp)

$(BUILD)/cpu.v: cpu.weft decode.weft $(SYNTH)
	mkdir -p $(BUILD)
	$(SYNTH) elab cpu.weft decode.weft --top nes_cpu_test --verilog $@

$(BUILD)/cpu/Vnes_cpu_test: $(BUILD)/cpu.v tests/cpu.cpp
	$(VERILATOR) --cc --exe --build -j $(JOBS) -CFLAGS "-std=c++17" -Wno-fatal --top-module nes_cpu_test \
	    --Mdir $(BUILD)/cpu $< $(abspath tests/cpu.cpp)

cpu-test: $(BUILD)/cpu/Vnes_cpu_test
	$< $(TEST_ROMS)/other/nestest.nes $(TEST_ROMS)/other/nestest.log

$(BUILD)/vectors/Vnes_cpu_test: $(BUILD)/cpu.v tests/vectors.cpp
	$(VERILATOR) --cc --exe --build -j $(JOBS) -CFLAGS "-std=c++17" -Wno-fatal --top-module nes_cpu_test \
	    --Mdir $(BUILD)/vectors $< $(abspath tests/vectors.cpp)

vectors-test: $(BUILD)/vectors/Vnes_cpu_test
	$< $(VECTORS)

test: sim cpu-test
	python3 tests/run.py --sim $(BUILD)/obj/Vnes --roms $(TEST_ROMS)

# Place and route the pin-level wrapper on an HX8K against the dot clock.
fpga: $(SOURCES) board.weft $(SYNTH)
	mkdir -p $(BUILD)
	$(SYNTH) build $(SOURCES) board.weft --top nes_board --device hx8k --package ct256 \
	    --freq 5.369318 -o $(BUILD)/nes_board.bin

clean:
	rm -rf $(BUILD)/obj $(BUILD)/cpu $(BUILD)/vectors $(BUILD)/nes.v $(BUILD)/cpu.v $(BUILD)/nes_board.bin
