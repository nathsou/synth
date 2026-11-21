# SPDX-License-Identifier: CERN-OHL-W-2.0
# Builds the benchmark's ROM and RAM images (rom.hex, ram.hex: one 32-bit
# little-endian word per line) with clang and lld, for rv32i.
CC = clang
OBJCOPY = llvm-objcopy
CFLAGS = --target=riscv32 -march=rv32i -mabi=ilp32 -O2 -ffreestanding -nostdlib \
	-fno-builtin-memset -Wall -Wextra
HEX = python3 -c "import sys; d=open(sys.argv[1],'rb').read(); d+=bytes(-len(d)%4); \
	print('\n'.join('%08x'%int.from_bytes(d[i:i+4],'little') for i in range(0,len(d),4)))"

all: rom.hex ram.hex

rom.hex: bench.elf
	$(OBJCOPY) -O binary -j .text $< rom.bin
	$(HEX) rom.bin > $@

ram.hex: bench.elf
	$(OBJCOPY) -O binary -j .data $< ram.bin
	$(HEX) ram.bin > $@

bench.elf: start.S bench.c link.ld
	$(CC) $(CFLAGS) -fuse-ld=lld -Wl,-T,link.ld -o $@ start.S bench.c

clean:
	rm -f bench.elf rom.bin ram.bin
