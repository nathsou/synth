MEMORY { RAM (rwx) : ORIGIN = 0, LENGTH = 4K }
SECTIONS {
  .text : { *(.text.start) *(.text*) *(.rodata*) *(.srodata*) } > RAM
  .data : { *(.data*) *(.sdata*) } > RAM
  .bss : { *(.bss*) *(.sbss*) *(COMMON) } > RAM
}
