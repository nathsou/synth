# Builds fw.hex (one 32-bit little-endian word per line, for $readmemh).
PREFIX ?= riscv64-unknown-elf-
CFLAGS = -march=rv32i -mabi=ilp32 -Os -ffreestanding -nostdlib -Wall

../fw.hex: fw.bin
	python3 -c "import sys; d=open('fw.bin','rb').read(); d+=bytes(-len(d)%4); \
	print('\n'.join('%08x'%int.from_bytes(d[i:i+4],'little') for i in range(0,len(d),4)))" > $@

fw.bin: fw.elf
	$(PREFIX)objcopy -O binary $< $@

fw.elf: start.S main.c link.ld
	$(PREFIX)gcc $(CFLAGS) -T link.ld -o $@ start.S main.c

clean:
	rm -f fw.elf fw.bin
