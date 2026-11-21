/* SPDX-License-Identifier: CERN-OHL-W-2.0 */
/* The instructions in the ROM, everything else in the RAM. */
MEMORY {
    ROM (rx) : ORIGIN = 0x00000000, LENGTH = 8K
    RAM (rw) : ORIGIN = 0x00010000, LENGTH = 4K
}
SECTIONS {
    .text : { *(.text.start) *(.text*) } > ROM
    .data : { *(.rodata*) *(.srodata*) *(.data*) *(.sdata*) } > RAM
    .bss (NOLOAD) : {
        . = ALIGN(4);
        __bss_start = .;
        *(.bss*) *(.sbss*) *(COMMON)
        . = ALIGN(4);
        __bss_end = .;
    } > RAM
    __stack_top = ORIGIN(RAM) + LENGTH(RAM);
}
